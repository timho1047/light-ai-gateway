//! Quota probing: turn each provider's account state into Gates.
//!
//! FILE OWNER: teammate quota-dev. Replace the stub bodies; keep the public
//! signatures stable so router.rs and the poller keep compiling.
//!
//! Adapters implemented here:
//!   - Xkiro        GET {probe_base}/v1/usage
//!   - CommandCode  GET api.commandcode.ai/alpha/billing/credits (+ subscriptions, usage/summary, whoami)
//!   - Deepseek     GET {probe_base}/user/balance
//!
//! Rules every adapter must follow:
//!   - Treat upstream JSON as UNTRUSTED: parse defensively, tolerate missing
//!     and unknown fields, and never panic on an unexpected shape.
//!   - Arithmetic on provider-supplied amounts goes through the saturating
//!     helpers in this module. rust_decimal's `+` and `-` panic on overflow, and
//!     an overflow is only ever one hostile or broken payload away: a window
//!     with cap_usd "-7e28" and remaining_usd "7e28" is enough to take down the
//!     task that owns the probe.
//!   - Money arrives as fixed-point STRINGS from xKiro. Parse with
//!     crate::domain::parse_money, never through f64.
//!   - A failed probe returns Err. The caller carries the previous reading
//!     forward and flags it stale; an adapter must never invent zeros.
//!   - The probe host is pinned per provider and must not be derived from a
//!     user-supplied request URL.

use crate::config::QuotaAdapter;
use crate::domain::{Account, ApiKey, Gate};
use async_trait::async_trait;
use rust_decimal::Decimal;
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;

pub mod commandcode;
pub mod deepseek;
pub mod xkiro;

/// A successful probe.
#[derive(Debug, Clone, Default)]
pub struct ProbeOutcome {
    pub gates: Vec<Gate>,
    /// Plan name, when the provider reports one.
    pub plan: Option<String>,
    /// Currency observed, when the provider reports one.
    pub currency: Option<String>,
}

/// A failed probe. The message must never contain credentials.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct ProbeError {
    pub message: String,
}

impl ProbeError {
    pub fn new(message: impl Into<String>) -> Self {
        ProbeError {
            message: message.into(),
        }
    }
}

/// One provider's quota adapter.
#[async_trait]
pub trait QuotaProbe: Send + Sync {
    /// Stable adapter name, used in logs and the doctor command.
    fn name(&self) -> &'static str;

    /// Read this account's current gates.
    async fn probe(
        &self,
        account: &Account,
        client: &reqwest::Client,
    ) -> Result<ProbeOutcome, ProbeError>;
}

/// Build the adapter for a provider, or None when it has no probe endpoint.
///
/// probe_base_url is pinned from configuration, so credentials never follow a
/// client-supplied URL to another host.
pub fn probe_for(adapter: QuotaAdapter, probe_base_url: &str) -> Option<Arc<dyn QuotaProbe>> {
    let base = probe_base_url.trim_end_matches('/').to_string();
    match adapter {
        QuotaAdapter::Xkiro => Some(Arc::new(xkiro::XkiroProbe::new(base))),
        QuotaAdapter::CommandCode => Some(Arc::new(commandcode::CommandCodeProbe::new(base))),
        QuotaAdapter::Deepseek => Some(Arc::new(deepseek::DeepseekProbe::new(base))),
        QuotaAdapter::None => None,
    }
}

// ---------------------------------------------------------------------------
// Shared HTTP + JSON helpers.
//
// All three adapters read a pinned host with a bearer token, cap the response,
// refuse redirects and must never write a credential into an error message, so
// that part lives here rather than being repeated (and slowly diverging) three
// times.
// ---------------------------------------------------------------------------

/// Hard ceiling on any probe request. A hung provider must not stall the poller.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/// Hard cap on a probe response body. Quota payloads are a few kilobytes;
/// anything larger is a mistake or an attempt to exhaust memory.
pub(crate) const MAX_PROBE_BODY_BYTES: usize = 512 * 1024;

/// Build a ProbeError whose message can never contain the account's key.
///
/// Every message that could contain upstream-controlled text passes through
/// here, so no adapter can leak a credential through a log line.
pub(crate) fn probe_error(key: &ApiKey, message: impl AsRef<str>) -> ProbeError {
    let secret = key.expose();
    let message = message.as_ref();
    let redacted = if secret.is_empty() {
        message.to_string()
    } else {
        message.replace(secret, "***")
    };
    ProbeError::new(redacted)
}

/// GET a provider endpoint with the account's bearer token and return the body.
///
/// The URL is built by the adapter from the pinned probe host; the caller never
/// supplies any part of it.
pub(crate) async fn get_body(
    client: &reqwest::Client,
    url: &str,
    key: &ApiKey,
    label: &str,
) -> Result<String, ProbeError> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|_| probe_error(key, format!("{label}: invalid probe URL")))?;

    let response = client
        .get(parsed.clone())
        .timeout(PROBE_TIMEOUT)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", key.expose()),
        )
        .send()
        .await
        .map_err(|e| probe_error(key, format!("{label}: request failed: {e}")))?;

    // Defence in depth: the shared client pins Policy::none(), but if a caller
    // ever hands us a redirect-following client the credential would have been
    // sent to another host and the payload cannot be trusted. Never echo the
    // URL here -- only the endpoint label.
    let same_host = response.url().host_str() == parsed.host_str()
        && response.url().port_or_known_default() == parsed.port_or_known_default();
    if !same_host {
        return Err(probe_error(
            key,
            format!("{label}: probe left the pinned host, response discarded"),
        ));
    }

    let status = response.status().as_u16();
    if (300..400).contains(&status) {
        return Err(probe_error(
            key,
            format!("{label}: HTTP {status} redirect not followed"),
        ));
    }
    if !(200..300).contains(&status) {
        // Name the URL that actually failed. Without it a 404 from a probe base
        // that accidentally kept its /v1 segment reads as "HTTP 404" with no clue
        // which path was wrong, which is exactly how a real misconfiguration was
        // mistaken for a bad credential. The URL is safe to print: the key
        // travels in a header, and only scheme/host/path are included.
        //
        // The body contributes a machine-readable code only, truncated: the human
        // message is reworded freely by providers and may echo the credential.
        let mut detail = format!("{label}: HTTP {status} at {}", safe_url(&parsed));
        if let Ok(body) = read_capped(response, key, label).await {
            let (ty, code) = crate::error::parse_error_body(&body);
            if let Some(code) = code.or(ty) {
                let short: String = code.chars().take(64).collect();
                detail.push_str(&format!(" ({short})"));
            }
        }
        return Err(probe_error(key, detail));
    }

    read_capped(response, key, label).await
}

/// A URL fit for an error message: scheme, host and path only.
///
/// The query string and any userinfo are deliberately dropped. They are the only
/// parts that can carry a credential, and a diagnostic that leaks one is worse
/// than a diagnostic that is vague.
pub(crate) fn safe_url(url: &reqwest::Url) -> String {
    let mut out = format!("{}://{}", url.scheme(), url.host_str().unwrap_or("?"));
    if let Some(port) = url.port() {
        out.push_str(&format!(":{port}"));
    }
    out.push_str(url.path());
    out
}

/// GET a pinned endpoint and parse the body as JSON.
pub(crate) async fn get_json(
    client: &reqwest::Client,
    url: &str,
    key: &ApiKey,
    label: &str,
) -> Result<serde_json::Value, ProbeError> {
    let body = get_body(client, url, key, label).await?;
    serde_json::from_str(&body)
        .map_err(|_| probe_error(key, format!("{label}: response is not valid JSON")))
}

/// Read a response body with a hard size cap.
async fn read_capped(
    mut response: reqwest::Response,
    key: &ApiKey,
    label: &str,
) -> Result<String, ProbeError> {
    let mut body: Vec<u8> = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if body.len() + chunk.len() > MAX_PROBE_BODY_BYTES {
                    return Err(probe_error(
                        key,
                        format!(
                            "{label}: response exceeds the {} KiB probe limit",
                            MAX_PROBE_BODY_BYTES / 1024
                        ),
                    ));
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(e) => {
                return Err(probe_error(
                    key,
                    format!("{label}: reading response failed: {e}"),
                ));
            }
        }
    }
    String::from_utf8(body)
        .map_err(|_| probe_error(key, format!("{label}: response is not valid UTF-8")))
}

// ---------------------------------------------------------------------------
// Untrusted-JSON accessors.
//
// Providers are loose about types: an amount may arrive as a fixed-point
// string, a JSON number or (after an API change) not at all. A field that is
// absent must stay absent -- never zero.
// ---------------------------------------------------------------------------

/// Money from a JSON scalar: fixed-point string first, JSON number otherwise.
/// Never routed through f64.
///
/// A bare JSON number is re-serialised to its own text form and then parsed as
/// a Decimal. Going through `as_f64` + `Decimal::from_f64` instead would keep
/// the whole binary double: the recorded balance 0.1 would come back as
/// 0.1000000000000000055511151231257827, and an integer such as
/// 12345678901234567890 would silently become 12345678901234567168. The text
/// form is the shortest representation that round-trips the f64 serde_json
/// already parsed into, so it is the best this adapter can do without the
/// arbitrary_precision feature; `json_money_keeps_a_bare_json_number_exact`
/// pins that down.
pub(crate) fn json_money(value: &serde_json::Value) -> Option<Decimal> {
    match value {
        serde_json::Value::String(s) => crate::domain::parse_money(s),
        serde_json::Value::Number(n) => crate::domain::parse_money(&n.to_string()),
        _ => None,
    }
}

/// Addition that saturates instead of panicking.
///
/// meter.rs keeps the same pair (sat_add/sat_mul) for the pricing path, but
/// keeps them private, and a shared home would make the quota adapters depend
/// on the pricing module. Saturation points at Decimal::MAX, which is the
/// conservative direction here: an amount is never made to look smaller than it
/// is.
pub(crate) fn sat_add(a: Decimal, b: Decimal) -> Decimal {
    a.checked_add(b).unwrap_or(Decimal::MAX)
}

/// Subtraction that saturates instead of panicking (Decimal::MIN side).
pub(crate) fn sat_sub(a: Decimal, b: Decimal) -> Decimal {
    a.checked_sub(b).unwrap_or(Decimal::MIN)
}

/// A non-negative integer from a JSON scalar (counts, seconds, token limits).
pub(crate) fn json_u64(value: &serde_json::Value) -> Option<u64> {
    match value {
        serde_json::Value::Number(n) => n.as_u64().or_else(|| {
            n.as_f64()
                .filter(|f| f.is_finite() && *f >= 0.0)
                .map(|f| f as u64)
        }),
        serde_json::Value::String(s) => s.trim().parse::<u64>().ok(),
        _ => None,
    }
}

/// Tolerant boolean: providers send true/false, 1/0 or "true".
pub(crate) fn json_truthy(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        serde_json::Value::String(s) => {
            matches!(s.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes")
        }
        _ => false,
    }
}

/// The raw scalar as a string, for query parameters that must be forwarded
/// verbatim (an opaque period start, an org id).
pub(crate) fn json_scalar_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// A timestamp from RFC 3339 text or an epoch number (seconds or milliseconds).
pub(crate) fn json_timestamp(value: &serde_json::Value) -> Option<OffsetDateTime> {
    match value {
        serde_json::Value::String(s) => parse_timestamp_str(s),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                return epoch(i);
            }
            crate::domain::parse_money(&n.to_string())
                .and_then(|d| {
                    use rust_decimal::prelude::ToPrimitive;
                    d.to_i64()
                })
                .and_then(epoch)
        }
        _ => None,
    }
}

fn parse_timestamp_str(raw: &str) -> Option<OffsetDateTime> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(t) = OffsetDateTime::parse(trimmed, &time::format_description::well_known::Rfc3339) {
        return Some(t);
    }
    trimmed.parse::<i64>().ok().and_then(epoch)
}

/// Epoch seconds, or milliseconds when the value is obviously too large to be
/// seconds (anything past the year 5138).
fn epoch(value: i64) -> Option<OffsetDateTime> {
    let seconds = if value >= 100_000_000_000 {
        value / 1000
    } else {
        value
    };
    OffsetDateTime::from_unix_timestamp(seconds).ok()
}

#[cfg(test)]
pub(crate) mod testutil {
    //! A tiny HTTP server for adapter tests.
    //!
    //! Hand-rolled on purpose: the tests must be able to assert on the raw
    //! request line and headers (method, pinned path, query, bearer header) and
    //! must be able to answer with a redirect or an oversized body, none of
    //! which needs another dependency.

    use crate::domain::{Account, AccountState, ApiKey};
    use rust_decimal::Decimal;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// One canned response, served to the nth request the server receives.
    #[derive(Clone, Debug)]
    pub struct Reply {
        pub status: u16,
        pub body: String,
        pub content_type: &'static str,
        pub headers: Vec<(String, String)>,
    }

    impl Reply {
        pub fn json(body: impl Into<String>) -> Reply {
            Reply {
                status: 200,
                body: body.into(),
                content_type: "application/json",
                headers: Vec::new(),
            }
        }

        pub fn status(status: u16, body: impl Into<String>) -> Reply {
            Reply {
                status,
                body: body.into(),
                content_type: "application/json",
                headers: Vec::new(),
            }
        }

        pub fn redirect(location: impl Into<String>) -> Reply {
            Reply {
                status: 302,
                body: String::new(),
                content_type: "text/plain",
                headers: vec![("Location".to_string(), location.into())],
            }
        }
    }

    /// A server on an ephemeral loopback port. Dropping it stops the listener.
    pub struct TestServer {
        pub base_url: String,
        requests: Arc<Mutex<Vec<String>>>,
        handle: tokio::task::JoinHandle<()>,
    }

    impl TestServer {
        pub async fn start(replies: Vec<Reply>) -> TestServer {
            let listener = TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("bind test server");
            let addr = listener.local_addr().expect("test server address");
            let requests = Arc::new(Mutex::new(Vec::new()));
            let collected = Arc::clone(&requests);

            let handle = tokio::spawn(async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        break;
                    };
                    let replies = replies.clone();
                    let collected = Arc::clone(&collected);
                    tokio::spawn(async move {
                        let head = read_request_head(&mut socket).await;
                        let index = {
                            let mut log = collected.lock().expect("test request log");
                            log.push(head);
                            log.len() - 1
                        };
                        let reply = replies.get(index).cloned().unwrap_or_else(|| {
                            Reply::status(500, "{\"error\":\"unexpected request\"}")
                        });
                        let mut response = format!(
                            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
                            reply.status,
                            reason(reply.status),
                            reply.content_type,
                            reply.body.len()
                        );
                        for (name, value) in &reply.headers {
                            response.push_str(&format!("{name}: {value}\r\n"));
                        }
                        response.push_str("\r\n");
                        response.push_str(&reply.body);

                        let _ = socket.write_all(response.as_bytes()).await;
                        let _ = socket.flush().await;
                        let _ = socket.shutdown().await;
                    });
                }
            });

            TestServer {
                base_url: format!("http://{addr}"),
                requests,
                handle,
            }
        }

        /// Raw request heads, in arrival order.
        pub fn requests(&self) -> Vec<String> {
            self.requests.lock().expect("test request log").clone()
        }

        pub fn call_count(&self) -> usize {
            self.requests.lock().expect("test request log").len()
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    /// Serialises the tests that capture log output.
    ///
    /// They all share one process-wide subscriber and one buffer, so they must
    /// not run at the same time as each other.
    static CAPTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Run a future with WARN-and-above events captured as log lines.
    ///
    /// Some of an adapter's degraded paths are only visible in the log -- a
    /// monthly gate that silently disappears, a window dropped because its
    /// window_sec is unknown -- so for those the log line IS the behaviour under
    /// test.
    ///
    /// The subscriber is GLOBAL, not thread-local. Installing one with
    /// set_default looked right because the tests drive a current-thread runtime,
    /// but under a full parallel run the warning intermittently landed in a
    /// buffer nobody was reading and the test failed roughly one run in ten.
    /// One subscriber plus a lock is deterministic.
    // Holding the lock across the await IS the mechanism: the whole point is
    // that two of these tests must not interleave their log capture.
    #[allow(clippy::await_holding_lock)]
    pub async fn capture_warnings<Fut: std::future::Future>(
        fut: Fut,
    ) -> (Vec<String>, Fut::Output) {
        use std::io::Write;

        /// Appends into a buffer the test can still read after the subscriber
        /// is dropped.
        struct SharedWriter(Arc<Mutex<Vec<u8>>>);
        impl Write for SharedWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().expect("log buffer").extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        /// Process-wide, because the events are.
        static BUFFER: std::sync::OnceLock<Arc<Mutex<Vec<u8>>>> = std::sync::OnceLock::new();

        let buffer = BUFFER
            .get_or_init(|| {
                let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
                let writer = SharedWriter(Arc::clone(&buffer));
                let subscriber = tracing_subscriber::fmt()
                    .with_writer(move || SharedWriter(Arc::clone(&writer.0)))
                    .with_max_level(tracing::Level::WARN)
                    .with_ansi(false)
                    .without_time()
                    .finish();
                // Only the first caller installs it; every later one writes into
                // the same buffer, which is what makes the capture independent of
                // whichever thread happens to emit the event.
                let _ = tracing::subscriber::set_global_default(subscriber);
                buffer
            })
            .clone();

        let _serialised = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        buffer.lock().expect("log buffer").clear();

        let output = fut.await;

        let text =
            String::from_utf8(buffer.lock().expect("log buffer").clone()).unwrap_or_default();
        (text.lines().map(str::to_string).collect(), output)
    }

    /// A client that behaves like the gateway's shared client: redirects are
    /// never followed, so credentials cannot leave the pinned host.
    pub fn strict_client() -> reqwest::Client {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(8))
            .build()
            .expect("build test client")
    }

    pub fn test_account(provider: &str, key: &str) -> Account {
        Account {
            id: "acc-1".to_string(),
            provider: provider.to_string(),
            label: "acc-1".to_string(),
            base_url: "https://api.example.invalid/v1".to_string(),
            api_key: ApiKey::new(key),
            currency: "USD".to_string(),
            gates: Vec::new(),
            state: AccountState::Unknown,
            available_at: None,
            window_started_at: None,
            credit_multiplier: Decimal::ONE,
            rpm_limit: None,
            concurrency_limit: None,
            last_probe_at: None,
            last_error: None,
        }
    }

    async fn read_request_head(socket: &mut tokio::net::TcpStream) -> String {
        let mut head = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            match socket.read(&mut chunk).await {
                Ok(0) => break,
                Ok(n) => {
                    head.extend_from_slice(&chunk[..n]);
                    if head.windows(4).any(|w| w == b"\r\n\r\n") || head.len() > 64 * 1024 {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&head).to_string()
    }

    fn reason(status: u16) -> &'static str {
        match status {
            200 => "OK",
            301 => "Moved Permanently",
            302 => "Found",
            400 => "Bad Request",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            429 => "Too Many Requests",
            500 => "Internal Server Error",
            502 => "Bad Gateway",
            503 => "Service Unavailable",
            _ => "Status",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::{Reply, TestServer, test_account};
    use super::*;
    use crate::config::QuotaAdapter;

    fn adapters(base: &str) -> Vec<(&'static str, Arc<dyn QuotaProbe>)> {
        vec![
            (
                "xkiro",
                probe_for(QuotaAdapter::Xkiro, base).expect("xkiro adapter"),
            ),
            (
                "command-code",
                probe_for(QuotaAdapter::CommandCode, base).expect("command code adapter"),
            ),
            (
                "deepseek",
                probe_for(QuotaAdapter::Deepseek, base).expect("deepseek adapter"),
            ),
        ]
    }

    /// A JSON number must reach Decimal through its own text form. `as_f64`
    /// would keep the whole binary double (0.1 would become
    /// 0.1000000000000000055511151231257827) and would blunt a 20-digit integer
    /// balance to 12345678901234567168.
    #[test]
    fn json_money_keeps_a_bare_json_number_exact() {
        let parsed: serde_json::Value =
            serde_json::from_str(r#"{"tiny": 0.1, "big": 12345678901234567890, "amount": 12.5}"#)
                .expect("valid JSON fixture");

        assert_eq!(
            json_money(&parsed["tiny"]),
            crate::domain::parse_money("0.1")
        );
        assert_eq!(
            json_money(&parsed["big"]),
            crate::domain::parse_money("12345678901234567890")
        );
        assert_eq!(
            json_money(&parsed["amount"]),
            crate::domain::parse_money("12.5")
        );
        assert_eq!(json_money(&serde_json::json!(null)), None);
        assert_eq!(json_money(&serde_json::json!(true)), None);
    }

    /// Provider amounts are untrusted, and rust_decimal's own operators panic on
    /// overflow. Every adapter amount therefore goes through these two helpers,
    /// which saturate at the far end of the range instead.
    #[test]
    fn saturating_helpers_stop_an_overflowing_amount_from_panicking() {
        let huge = crate::domain::parse_money("7e28").expect("a huge amount must parse");
        let negative = crate::domain::parse_money("-7e28").expect("a huge amount must parse");

        assert_eq!(sat_add(huge, huge), Decimal::MAX);
        assert_eq!(sat_sub(negative, huge), Decimal::MIN);
        assert_eq!(sat_add(Decimal::ONE, Decimal::ONE), Decimal::from(2));
    }

    #[test]
    fn probe_for_builds_an_adapter_per_provider_and_none_for_none() {
        for (adapter, name) in [
            (QuotaAdapter::Xkiro, "xkiro"),
            (QuotaAdapter::CommandCode, "command-code"),
            (QuotaAdapter::Deepseek, "deepseek"),
        ] {
            let probe =
                probe_for(adapter, "https://api.example.invalid/").expect("adapter must exist");
            assert_eq!(probe.name(), name);
        }
        assert!(probe_for(QuotaAdapter::None, "https://api.example.invalid").is_none());
    }

    /// The pinned client never follows a redirect; a probe that receives a 3xx
    /// must fail instead of reading whatever answered.
    #[tokio::test]
    async fn no_adapter_follows_a_redirect_off_the_pinned_host() {
        let target = TestServer::start(vec![Reply::json(
            r#"{"plan":"pro","windows":[{"window_sec":18000,"spent_usd":"0","cap_usd":"10"}]}"#,
        )])
        .await;
        let source = TestServer::start(vec![
            Reply::redirect(format!("{}/v1/usage", target.base_url)),
            Reply::redirect(format!("{}/v1/usage", target.base_url)),
            Reply::redirect(format!("{}/v1/usage", target.base_url)),
            Reply::redirect(format!("{}/v1/usage", target.base_url)),
        ])
        .await;

        let client = testutil::strict_client();
        let account = test_account("xkiro", "sk-quota-redirect");

        for (name, probe) in adapters(&source.base_url) {
            let err = probe
                .probe(&account, &client)
                .await
                .expect_err("a redirect must fail the probe");
            assert!(
                err.message.contains("redirect"),
                "{name}: unexpected message {:?}",
                err.message
            );
        }

        assert_eq!(
            target.call_count(),
            0,
            "a credential must never be sent to the redirect target"
        );
    }

    /// A provider that reflects the key in an error body must not turn a probe
    /// failure into a credential leak.
    #[tokio::test]
    async fn no_adapter_error_message_contains_the_api_key() {
        const KEY: &str = "sk-quota-secret-abc123";
        let echoing = format!(
            r#"{{"error":{{"type":"authentication_error","code":"invalid_api_key {KEY}","message":"bad key {KEY}"}}}}"#
        );
        let server = TestServer::start(vec![
            Reply::status(401, echoing.clone()),
            Reply::status(401, echoing.clone()),
            Reply::status(401, echoing.clone()),
            Reply::status(401, echoing.clone()),
        ])
        .await;

        let client = testutil::strict_client();
        let account = test_account("xkiro", KEY);

        for (name, probe) in adapters(&server.base_url) {
            let err = probe
                .probe(&account, &client)
                .await
                .expect_err("401 must fail the probe");
            assert!(
                !err.message.contains(KEY),
                "{name}: error leaked the key: {:?}",
                err.message
            );
            assert!(
                err.message.contains("401"),
                "{name}: error should carry the status: {:?}",
                err.message
            );
        }
    }

    /// A probe that answers with more than the cap must fail rather than buffer
    /// an unbounded body.
    #[tokio::test]
    async fn a_probe_response_larger_than_the_cap_is_rejected() {
        let huge = format!(r#"{{"plan":"{}"#, "x".repeat(600 * 1024));
        let server = TestServer::start(vec![Reply::json(huge)]).await;
        let client = testutil::strict_client();
        let account = test_account("xkiro", "sk-quota-huge");

        let probe = probe_for(QuotaAdapter::Xkiro, &server.base_url).expect("adapter");
        let err = probe
            .probe(&account, &client)
            .await
            .expect_err("oversized response must fail");
        assert!(
            err.message.contains("512 KiB"),
            "unexpected message {:?}",
            err.message
        );
    }
}
