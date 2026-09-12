//! HTTP front end: the OpenAI-compatible endpoint and the retry loop.
//!
//! FILE OWNER: teammate proxy-dev. Replace stub bodies; keep public signatures.
//!
//! Hard rules:
//!   - An account switch may only happen BEFORE the first byte is written to
//!     the client. Connect upstream, inspect the status, and only then start
//!     relaying.
//!   - Never buffer a whole response body. Relay bytes as they arrive and tee
//!     them into the SSE meter on the side.
//!   - When the client disconnects, abort the upstream request so billing stops.
//!   - Bound retries. Streaming retries can double-bill, so a request that has
//!     already delivered bytes must never be retried.
//!
//! The handler therefore works in two phases:
//!   1. Plan and probe: ask the pool for an account, send the upstream request,
//!      classify a non-2xx response and retry or fail over. Nothing reaches the
//!      client in this phase, so an account switch is still safe.
//!   2. Relay: wrap the upstream byte stream in RelayStream, which forwards
//!      every chunk unchanged, feeds a StreamMeter on the side, stamps TTFT,
//!      and records the request when the stream ends or the client goes away.

use crate::accounts::{AttemptPlan, PlanError};
use crate::config::{Config, ResolvedRoute};
use crate::domain::{
    Account, ApiKey, AttemptRecord, GateClass, RequestRecord, RequestStatus, TokenUsage, now,
};
use crate::error::{ErrorSignals, UpstreamErrorClass, parse_error_body, parse_retry_after};
use crate::router::Gateway;
use crate::sse::{MeterMode, StreamMeter, inject_include_usage};
use anyhow::{Context, Result};
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post};
use futures_util::Stream;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant};
use time::OffsetDateTime;

/// Longest backoff the retry loop will sleep. A quota window is handled by
/// rotating accounts, not by stalling the client for an hour.
const MAX_BACKOFF: Duration = Duration::from_secs(2);

/// Gives an attempt's reservation back if the request never reaches settle.
///
/// select() commits the estimate the moment it picks an account, and only
/// settle() returns it. Everything between that choice and the response -- the
/// pre-switch probe, the wait for headers -- can be interrupted by the client
/// hanging up, which drops this future and runs no cleanup of ours. The
/// commitment then stayed on the books for the life of the process, and the
/// in-flight term in the hard gate kept shrinking every account's headroom
/// until nothing was eligible and only a restart could clear it.
struct AttemptLease<'a> {
    backend: &'a dyn ProxyBackend,
    plan: AttemptPlan,
    armed: bool,
}

impl<'a> AttemptLease<'a> {
    fn new(backend: &'a dyn ProxyBackend, plan: &AttemptPlan) -> Self {
        Self {
            backend,
            plan: plan.clone(),
            armed: true,
        }
    }

    /// The attempt reached a path that settles it itself.
    fn settled(&mut self) {
        self.armed = false;
    }
}

impl Drop for AttemptLease<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.backend.release(&self.plan);
        }
    }
}

/// How long to wait before the next attempt.
///
/// A Retry-After the upstream gave us wins. Otherwise a retryable failure still
/// waits: retrying instantly turns one transient blip into a thundering herd
/// against the next account, and for a transport failure there is nothing to
/// gain by being the fastest to ask again.
fn backoff_for(
    class: UpstreamErrorClass,
    attempt_no: u32,
    retry_after: Option<Duration>,
) -> Duration {
    if let Some(delay) = retry_after {
        return delay.min(MAX_BACKOFF);
    }
    if !(class.retry_other_account() || class.retry_same_account()) {
        return Duration::ZERO;
    }
    let shift = attempt_no.saturating_sub(1).min(4);
    Duration::from_millis(250u64 << shift).min(MAX_BACKOFF)
}

/// The slice of gateway behaviour the HTTP layer needs.
///
/// This seam exists so the router can be driven end-to-end in tests without a
/// live database or a probed pool. GatewayBackend is the production wiring;
/// every method mirrors a frozen Gateway/pool signature.
pub trait ProxyBackend: Send + Sync + 'static {
    fn config(&self) -> &Config;

    fn resolve(&self, client_model: &str) -> Result<ResolvedRoute>;

    fn plan(
        &self,
        client_model: &str,
        session_key: Option<&str>,
        est_cost: Decimal,
        request_id: &str,
        attempt_no: u32,
        exclude: &[String],
    ) -> std::result::Result<(ResolvedRoute, AttemptPlan), PlanError>;

    /// Pick an account for a request that has no model of its own, such as the
    /// model-list passthrough.
    fn pick_account(
        &self,
        provider: &str,
        est_cost: Decimal,
        request_id: &str,
        attempt_no: u32,
    ) -> std::result::Result<AttemptPlan, PlanError>;

    /// Look up an account by canonical provider/id identity.
    fn account(&self, qualified_id: &str) -> Option<Account>;

    /// Give back an attempt's reservation without recording it.
    ///
    /// Called by AttemptLease when a request never reaches settle. A no-op by
    /// default so a test backend that commits nothing needs nothing.
    fn release(&self, _plan: &AttemptPlan) {}

    /// Refresh an account's quota reading immediately before switching to it.
    ///
    /// A boxed future rather than an async-trait method so the default can be a
    /// no-op and test backends need no implementation. The real gateway honours
    /// policy.probe_before_switch; without this call that setting was parsed and
    /// then never read, so a switch could land on an account that had already
    /// filled up since the last poll.
    fn ensure_fresh<'a>(
        &'a self,
        _provider: &'a str,
        _account_id: &'a str,
    ) -> Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async {})
    }

    fn retry_after_for(&self, provider: &str) -> Option<Duration>;

    fn http(&self) -> reqwest::Client;

    /// Price a usage sample through the learned price table. None means the
    /// model is unknown and the caller must fall back conservatively.
    /// Which price tier a request starting now falls into. 0 is the default.
    fn tier_at(&self, provider: &str, model: &str) -> u32;

    fn estimate_cost(
        &self,
        provider: &str,
        model: &str,
        tier: u32,
        usage: &TokenUsage,
        multiplier: Decimal,
    ) -> Option<Decimal>;

    fn record_success(
        &self,
        plan: &AttemptPlan,
        usage: &TokenUsage,
        cost: Decimal,
        attempt: AttemptRecord,
        record: RequestRecord,
    );

    fn record_failure(
        &self,
        plan: &AttemptPlan,
        class: UpstreamErrorClass,
        retry_after: Option<Duration>,
        window_reset: Option<OffsetDateTime>,
        attempt: AttemptRecord,
    );

    /// Persist a request-level row once every attempt has failed.
    ///
    /// Attempts are already stored individually; this exists so a total failure
    /// still shows up in history and usage reports instead of leaving orphaned
    /// attempt rows behind.
    fn record_failed_request(&self, _record: RequestRecord) {}
}

/// Production wiring: a thin adapter over the frozen Gateway API.
pub struct GatewayBackend {
    gateway: Arc<Gateway>,
}

impl GatewayBackend {
    pub fn new(gateway: Arc<Gateway>) -> Self {
        GatewayBackend { gateway }
    }
}

impl ProxyBackend for GatewayBackend {
    fn config(&self) -> &Config {
        &self.gateway.config
    }

    fn resolve(&self, client_model: &str) -> Result<ResolvedRoute> {
        self.gateway.resolve(client_model)
    }

    fn plan(
        &self,
        client_model: &str,
        session_key: Option<&str>,
        est_cost: Decimal,
        request_id: &str,
        attempt_no: u32,
        exclude: &[String],
    ) -> std::result::Result<(ResolvedRoute, AttemptPlan), PlanError> {
        self.gateway.plan(
            client_model,
            session_key,
            est_cost,
            request_id,
            attempt_no,
            exclude,
        )
    }

    fn pick_account(
        &self,
        provider: &str,
        est_cost: Decimal,
        request_id: &str,
        attempt_no: u32,
    ) -> std::result::Result<AttemptPlan, PlanError> {
        let plan =
            self.gateway
                .pool
                .select(provider, "", None, est_cost, request_id, attempt_no, &[])?;
        // A model-list passthrough spends no tokens, so it gives the reservation
        // straight back. Leaving it committed burned one concurrency slot per
        // call against the account's own concurrency_limit, and after N calls
        // that account could never be chosen again.
        self.gateway.pool.settle(&plan, Decimal::ZERO, false);
        Ok(plan)
    }

    fn account(&self, qualified_id: &str) -> Option<Account> {
        self.gateway.pool.account(qualified_id)
    }

    fn ensure_fresh<'a>(
        &'a self,
        provider: &'a str,
        account_id: &'a str,
    ) -> Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            self.gateway.ensure_fresh(provider, account_id).await;
        })
    }

    fn release(&self, plan: &AttemptPlan) {
        self.gateway.pool.settle(plan, Decimal::ZERO, false);
    }

    fn retry_after_for(&self, provider: &str) -> Option<Duration> {
        self.gateway.pool.retry_after_for(provider)
    }

    fn http(&self) -> reqwest::Client {
        self.gateway.http.clone()
    }

    fn tier_at(&self, provider: &str, model: &str) -> u32 {
        self.gateway.tier_at(provider, model, crate::domain::now())
    }

    fn estimate_cost(
        &self,
        provider: &str,
        model: &str,
        tier: u32,
        usage: &TokenUsage,
        multiplier: Decimal,
    ) -> Option<Decimal> {
        self.gateway
            .prices
            .lock()
            .ok()
            .and_then(|prices| prices.estimate(provider, model, tier, usage, multiplier))
    }

    fn record_success(
        &self,
        plan: &AttemptPlan,
        usage: &TokenUsage,
        cost: Decimal,
        attempt: AttemptRecord,
        record: RequestRecord,
    ) {
        self.gateway
            .record_success(plan, usage, cost, attempt, record);
    }

    fn record_failure(
        &self,
        plan: &AttemptPlan,
        class: UpstreamErrorClass,
        retry_after: Option<Duration>,
        window_reset: Option<OffsetDateTime>,
        attempt: AttemptRecord,
    ) {
        self.gateway
            .record_failure(plan, class, retry_after, window_reset, attempt);
    }

    fn record_failed_request(&self, record: RequestRecord) {
        self.gateway.record_failed_request(record);
    }
}

/// Run the gateway until shutdown.
pub async fn serve(config: Config) -> Result<()> {
    let listen = config.server.listen.clone();
    let addr: std::net::SocketAddr = listen
        .parse()
        .with_context(|| format!("invalid server.listen address {listen}"))?;
    let gateway = Gateway::new(config).await?;
    gateway.spawn_poller();
    let app = build_router(gateway);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!(%addr, "light-ai-gateway listening");
    axum::serve(listener, app).await.context("serving HTTP")?;
    Ok(())
}

/// Build the axum router. Exposed for integration tests.
pub fn build_router(gateway: Arc<Gateway>) -> axum::Router {
    build_router_with(Arc::new(GatewayBackend::new(gateway)))
}

/// Build the router against any backend. Exposed for integration tests.
pub fn build_router_with(backend: Arc<dyn ProxyBackend>) -> axum::Router {
    let max_body_bytes = backend.config().policy.max_body_bytes;
    let auth = match &backend.config().server.local_key {
        None => {
            // Serving without a local key means anything that can reach the port
            // can spend this account's quota. The configuration template calls
            // the key "strongly recommended", and then nothing said a word when
            // it was missing -- the operator's only evidence was a template they
            // had already closed.
            tracing::warn!(
                listen = %backend.config().server.listen,
                "server.local_key is not configured: anything able to reach this address \
                 can spend the pooled quota. Set one with 'lgw keys add local gateway', \
                 then local_key = {{ keychain = true }}"
            );
            LocalAuth::Open
        }
        Some(secret) => match secret.resolve(
            &crate::secrets::CredentialStore::from_env(),
            crate::secrets::LOCAL_KEY_PROVIDER,
            crate::secrets::LOCAL_KEY_ACCOUNT,
        ) {
            Ok(key) => LocalAuth::Required(key),
            Err(e) => {
                // Fail closed: without our key on disk this proxy would be an
                // open door to paid subscription credentials.
                tracing::error!(error = %e, "local_key could not be resolved; refusing every request");
                LocalAuth::Broken
            }
        },
    };
    let state = Arc::new(AppState { backend, auth });
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/models", get(list_models))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_local_key,
        ))
        .layer(DefaultBodyLimit::max(max_body_bytes))
        .with_state(state)
}

struct AppState {
    backend: Arc<dyn ProxyBackend>,
    auth: LocalAuth,
}

/// Local bearer authentication state.
enum LocalAuth {
    /// No local_key configured.
    Open,
    Required(ApiKey),
    /// local_key is configured but unreadable: reject everything.
    Broken,
}

impl LocalAuth {
    fn authorizes(&self, headers: &HeaderMap) -> bool {
        let required = match self {
            LocalAuth::Open => return true,
            LocalAuth::Broken => return false,
            LocalAuth::Required(key) => key,
        };
        let Some(value) = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
        else {
            return false;
        };
        let Some((scheme, token)) = value.split_at_checked(7) else {
            return false;
        };
        if !scheme.eq_ignore_ascii_case("bearer ") {
            return false;
        }
        constant_time_eq(token.trim().as_bytes(), required.expose().as_bytes())
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

async fn require_local_key(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    if state.auth.authorizes(request.headers()) {
        next.run(request).await
    } else {
        error_response(
            StatusCode::UNAUTHORIZED,
            "invalid_api_key",
            "missing or invalid Authorization bearer token",
        )
    }
}

async fn chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Ok(mut value) = serde_json::from_slice::<Value>(&body) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "request body is not valid JSON",
        );
    };
    if !value.is_object() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "request body must be a JSON object",
        );
    }
    let Some(client_model) = value
        .get("model")
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty())
        .map(str::to_string)
    else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "request must carry a non-empty \"model\"",
        );
    };
    let stream = value
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let session_key = session_key(&headers, &value);
    let request_id = uuid::Uuid::new_v4().to_string();
    // Captured outside the retry loop so a total failure can still be recorded.
    let request_started = now();

    let route = match state.backend.resolve(&client_model) {
        Ok(route) => route,
        Err(e) => {
            return error_response(
                StatusCode::NOT_FOUND,
                "invalid_request_error",
                &e.to_string(),
            );
        }
    };

    let estimated_prompt_tokens = crate::meter::estimate_prompt_tokens(&value);
    let est_cost = estimate_request_cost(
        state.backend.as_ref(),
        &route.provider,
        &route.model,
        &value,
    );

    // Without this flag the upstream never emits a usage frame and the local
    // ledger records zero forever, which silently disables cost control.
    let mut upstream_body = body.clone();
    if stream
        && inject_include_usage(&mut value)
        && let Ok(bytes) = serde_json::to_vec(&value)
    {
        upstream_body = Bytes::from(bytes);
    }

    let max_attempts = state.backend.config().policy.max_retries.max(1);
    // How long we wait for the upstream to START answering.
    //
    // This used to be connect_timeout_secs, which is a TCP-connect budget. A
    // non-streaming request only receives its headers after the upstream has
    // generated the entire completion, so any legitimate non-streaming request
    // slower than ten seconds was cancelled, marked unbilled, and retried --
    // possibly paying for the same completion twice.
    let header_timeout =
        Duration::from_secs(state.backend.config().policy.ttft_timeout_secs.max(1));

    let mut attempt_no: u32 = 1;
    let mut last_failure: Option<Failure> = None;
    let mut last_plan: Option<AttemptPlan> = None;
    // Accounts this request has already attempted. Passed to the pool so the
    // next attempt reaches a DIFFERENT account instead of spending the whole
    // retry budget on the one that just failed.
    let mut tried: Vec<String> = Vec::new();
    let mut previous_account: Option<String> = None;

    while attempt_no <= max_attempts {
        let plan = match state.backend.plan(
            &client_model,
            session_key.as_deref(),
            est_cost,
            &request_id,
            attempt_no,
            &tried,
        ) {
            Ok((_route, plan)) => plan,
            Err(err) => return plan_error_response(&err, state.backend.as_ref()),
        };
        // Switching accounts is worth one extra round trip: it happens only
        // after the previous account has already failed, so the client is
        // waiting anyway, and it stops a switch from landing on an account whose
        // window filled up since the last poll.
        let qualified = plan.qualified_account();
        if let Some(previous) = previous_account.as_deref()
            && previous != qualified
        {
            state
                .backend
                .ensure_fresh(&plan.provider, &plan.account_id)
                .await;
        }
        previous_account = Some(qualified.clone());
        // Recorded before the attempt, not after: whatever the outcome, this
        // account has now been used by this request.
        tried.push(qualified);
        last_plan = Some(plan.clone());
        // Any exit from this iteration that does not settle -- an early return,
        // or the client hanging up while we wait -- gives the reservation back
        // through this lease.
        let mut lease = AttemptLease::new(state.backend.as_ref(), &plan);

        let Some(account) = state.backend.account(&plan.qualified_account()) else {
            tracing::error!(account = %plan.account_id, "the planned account vanished from the pool");
            // Release the reservation that select() just committed. Returning
            // without recording the attempt left an estimate committed against
            // an account that nothing would ever settle, and the in-flight term
            // in the hard gate would keep counting it for the life of the
            // process -- shrinking the headroom every later request sees.
            lease.settled();
            record_attempt_failure(
                state.backend.as_ref(),
                &plan,
                UpstreamErrorClass::Other,
                None,
                Some(StatusCode::INTERNAL_SERVER_ERROR.as_u16()),
                None,
                now(),
                attempt_no,
                &request_id,
            );
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "the planned account is no longer in the pool",
            );
        };

        let url = format!(
            "{}/chat/completions",
            account.base_url.trim_end_matches('/')
        );
        let started_at = now();
        let started = Instant::now();
        let request = state
            .backend
            .http()
            .post(&url)
            .bearer_auth(account.api_key.expose())
            .header(header::CONTENT_TYPE, "application/json")
            .header(
                header::ACCEPT,
                if stream {
                    "text/event-stream"
                } else {
                    "application/json"
                },
            )
            .body(upstream_body.clone());

        // Phase 1: nothing has reached the client yet, so every failure below
        // is still retryable.
        let response = match tokio::time::timeout(header_timeout, request.send()).await {
            Ok(Ok(response)) => response,
            Ok(Err(e)) => {
                let timed_out = e.is_timeout();
                let class = ErrorSignals {
                    timed_out,
                    transport_failure: !timed_out,
                    ..Default::default()
                }
                .classify();
                lease.settled();
                record_attempt_failure(
                    state.backend.as_ref(),
                    &plan,
                    class,
                    None,
                    None,
                    None,
                    started_at,
                    attempt_no,
                    &request_id,
                );
                last_failure = Some(Failure {
                    status: status_for_class(class),
                    kind: class,
                    message: format!("could not reach the upstream: {e}"),
                    retry_after: None,
                });
                let retryable = should_retry(class, attempt_no, max_attempts);
                let delay = backoff_for(class, attempt_no, None);
                attempt_no += 1;
                if !retryable {
                    break;
                }
                // A transport failure or a timeout backs off as well: retrying
                // instantly turns one blip into a thundering herd, and there is
                // nothing to gain by being the fastest to ask a dead host again.
                tokio::time::sleep(delay).await;
                continue;
            }
            Err(_) => {
                let class = UpstreamErrorClass::Timeout;
                lease.settled();
                record_attempt_failure(
                    state.backend.as_ref(),
                    &plan,
                    class,
                    None,
                    None,
                    None,
                    started_at,
                    attempt_no,
                    &request_id,
                );
                last_failure = Some(Failure {
                    status: status_for_class(class),
                    kind: class,
                    message: format!(
                        "the upstream did not respond within {}s",
                        header_timeout.as_secs()
                    ),
                    retry_after: None,
                });
                let retryable = should_retry(class, attempt_no, max_attempts);
                let delay = backoff_for(class, attempt_no, None);
                attempt_no += 1;
                if !retryable {
                    break;
                }
                // A transport failure or a timeout backs off as well: retrying
                // instantly turns one blip into a thundering herd, and there is
                // nothing to gain by being the fastest to ask a dead host again.
                tokio::time::sleep(delay).await;
                continue;
            }
        };

        let status = response.status();
        if !status.is_success() {
            let response_headers = response.headers().clone();
            let text = read_bounded(response, 64 * 1024).await;
            let (error_type, error_code) = parse_error_body(&text);
            let retry_after = response_headers
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|raw| parse_retry_after(raw, now()));

            // xKiro reuses one 429 code for both pacing and exhaustion. Our own
            // ledger is the only independent evidence, and error.rs says a
            // probe-confirmed empty window outranks the header.
            // Canonical provider/id: account() looks up a qualified identity,
            // and passing the bare id meant this never matched anything, so the
            // probe-confirmed "the window is spent" branch was unreachable.
            let window_exhausted_hint = state
                .backend
                .account(&plan.qualified_account())
                .map(|acct| {
                    acct.covered_gates()
                        .any(|g| g.cap > Decimal::ZERO && g.remaining() <= Decimal::ZERO)
                })
                .unwrap_or(false);

            let class = ErrorSignals {
                status: Some(status.as_u16()),
                retry_after,
                error_type,
                error_code: error_code.clone(),
                window_exhausted_hint,
                transport_failure: false,
                timed_out: false,
            }
            .classify();

            lease.settled();
            record_attempt_failure(
                state.backend.as_ref(),
                &plan,
                class,
                retry_after,
                Some(status.as_u16()),
                error_code,
                started_at,
                attempt_no,
                &request_id,
            );

            let detail = upstream_error_message(&text)
                .unwrap_or_else(|| format!("upstream returned HTTP {}", status.as_u16()));
            // Other maps back to whatever the upstream said; the rest are
            // remapped so the client sees a class, not a provider quirk.
            let client_status = if class == UpstreamErrorClass::Other {
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY)
            } else {
                status_for_class(class)
            };
            let client_retry =
                retry_after.or_else(|| state.backend.retry_after_for(&plan.provider));
            last_failure = Some(Failure {
                status: client_status,
                kind: class,
                message: format!(
                    "upstream {} ({}) - {}",
                    status.as_u16(),
                    class.as_str(),
                    detail
                ),
                retry_after: client_retry,
            });

            let retryable = should_retry(class, attempt_no, max_attempts);
            let delay = backoff_for(class, attempt_no, retry_after);
            attempt_no += 1;
            if !retryable {
                break;
            }
            tokio::time::sleep(delay).await;
            continue;
        }

        // Phase 2: 2xx means bytes are about to be delivered. From here the
        // request is not retryable, so the relay owns the record.
        let coverage = plan.coverage;
        let response_headers = response.headers().clone();
        let content_type = response_headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let provider_request_id = provider_request_id(&response_headers);
        let mode = if stream || content_type.contains("text/event-stream") {
            MeterMode::Sse
        } else {
            MeterMode::Json
        };
        // Judged from the response, not from the client's "stream": true. A
        // client that asks for SSE and gets a plain JSON 200 back was being
        // marked as a truncated stream for missing a terminator that a JSON
        // body never had.
        let is_sse = content_type.contains("text/event-stream");
        lease.settled();
        let finalizer = Finalizer {
            backend: state.backend.clone(),
            plan,
            request_id: request_id.clone(),
            client_model: client_model.clone(),
            provider_request_id,
            currency: account.currency.clone(),
            credit_multiplier: account.credit_multiplier,
            coverage,
            attempt_no,
            started_at,
            estimated_prompt_tokens,
            status_code: status.as_u16(),
        };
        let meter = match mode {
            MeterMode::Sse => StreamMeter::sse(),
            MeterMode::Json => StreamMeter::json(),
        };
        let idle_timeout =
            Duration::from_secs(state.backend.config().policy.idle_timeout_secs.max(1));
        let relay = RelayStream {
            inner: Box::pin(response.bytes_stream()),
            meter,
            started,
            first_content_at: None,
            bytes_sent: 0,
            finalizer: Some(finalizer),
            is_sse,
            idle_timeout,
            idle: Box::pin(tokio::time::sleep(idle_timeout)),
        };
        let mut builder = Response::builder().status(status);
        if !content_type.is_empty() {
            builder = builder.header(header::CONTENT_TYPE, content_type);
        }
        return builder.body(Body::from_stream(relay)).unwrap_or_else(|_| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "failed to build the upstream response",
            )
        });
    }

    match last_failure {
        Some(failure) => {
            // Every attempt failed. Record the request itself so failures show
            // up in history instead of existing only as orphaned attempt rows.
            if let Some(plan) = last_plan.as_ref()
                && let Some(account) = state.backend.account(&plan.qualified_account())
            {
                let total_ms = (now() - request_started).whole_milliseconds().max(0) as u64;
                state.backend.record_failed_request(RequestRecord {
                    request_id: request_id.clone(),
                    ts: request_started,
                    session_key: session_key.clone(),
                    client_model: client_model.clone(),
                    provider: plan.provider.clone(),
                    account_id: plan.account_id.clone(),
                    model: plan.model.clone(),
                    usage: TokenUsage::default(),
                    ttft_ms: None,
                    total_ms,
                    tps: None,
                    cost: Decimal::ZERO,
                    currency: account.currency.clone(),
                    credit_multiplier: account.credit_multiplier,
                    coverage: plan.coverage,
                    status: RequestStatus::Error,
                    // attempt_no is already one past the last attempt: every
                    // failure path increments it before deciding whether to
                    // retry. Reporting it directly counted the first attempt
                    // as a retry.
                    retries: attempt_no.saturating_sub(2),
                    provider_request_id: None,
                });
            }
            error_response_retry(
                failure.status,
                failure.kind.as_str(),
                &failure.message,
                failure.retry_after,
            )
        }
        None => error_response(
            StatusCode::BAD_GATEWAY,
            "upstream_error",
            "no upstream attempt was made",
        ),
    }
}

#[derive(serde::Deserialize)]
struct ModelsQuery {
    /// Only list the routes served by this provider.
    #[serde(default)]
    provider: Option<String>,
}

/// Cap on a provider's model catalog.
///
/// It is a list of names, not a payload: a provider answering with more than this
/// is not answering the question we asked, and reading it unbounded lets one slow
/// upstream hold the endpoint open.
const MAX_CATALOG_BYTES: usize = 1024 * 1024;

/// One provider's own model catalog, keyed by the id that provider publishes.
pub type Catalog = BTreeMap<String, Value>;

/// The model catalog: exactly the routes this gateway is configured to serve.
///
/// The SET is ours and the FIELDS are the provider's. A client's contract with
/// this gateway is `[routes]` -- `Config::resolve` refuses anything else with "no
/// route for model", so answering with the provider's whole catalog would
/// advertise a hundred models that all fail, and the one thing a model list is
/// for is picking one that works. Each listed model therefore comes from the
/// route table, while everything else about it (display name, context length,
/// pricing, capabilities -- whatever the provider publishes) is passed through
/// from that provider's own `/v1/models` response. Nothing here is a second copy
/// that can drift from what the provider charges.
///
/// One upstream call per provider, not per model. A provider whose catalog cannot
/// be read does not fail the request either: its configured routes are listed
/// with the fields we do have, the reason is logged, and a client still gets the
/// list it needs.
async fn list_models(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ModelsQuery>,
) -> Response {
    let backend = state.backend.as_ref();

    if let Some(named) = &query.provider
        && !backend.config().providers.contains_key(named)
    {
        return error_response(
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            &format!("unknown provider {named}"),
        );
    }

    // routes is model -> provider, and it iterates in model order, which is the
    // order a reader scans a model list in.
    let routes: Vec<(String, String)> = backend
        .config()
        .routes
        .iter()
        .filter(|(_, provider)| {
            query
                .provider
                .as_ref()
                .is_none_or(|named| named == *provider)
        })
        .map(|(model, provider)| (model.clone(), provider.clone()))
        .collect();

    let mut wanted: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (model, provider) in &routes {
        wanted.entry(provider.as_str()).or_default().push(model);
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let mut catalogs: BTreeMap<&str, Catalog> = BTreeMap::new();
    for provider in wanted.keys() {
        match catalog_for_provider(backend, provider, &request_id).await {
            Ok(catalog) => {
                let unmatched: Vec<&str> = wanted[provider]
                    .iter()
                    .copied()
                    .filter(|model| lookup(&catalog, model).is_none())
                    .collect();
                for model in unmatched {
                    // A model list is read for its fields, so a route the provider
                    // does not publish is a route with no fields -- and the first
                    // question is always "what does the provider call it?". The ids
                    // answer that. Debug level, names only.
                    tracing::debug!(
                        provider,
                        model,
                        published = catalog.len(),
                        closest = %closest_hint(&catalog, model),
                        "this configured route is not in the provider's catalog; it is \
                         listed without the provider's fields"
                    );
                }
                catalogs.insert(provider, catalog);
            }
            Err(reason) => tracing::warn!(
                provider,
                reason = %reason,
                "the model catalog could not be read; listing this provider's configured \
                 routes without the provider's own fields"
            ),
        }
    }

    let mut data: Vec<Value> = Vec::with_capacity(routes.len());
    for (model, provider) in &routes {
        let published = catalogs
            .get(provider.as_str())
            .and_then(|catalog| lookup(catalog, model));
        data.push(model_entry(model, provider, published));
    }

    json_response(json!({ "object": "list", "data": data }))
}

/// One model: the provider's entry, with the two fields that are ours.
///
/// `id` and `object` are always set here -- the client asked for this route's
/// name, and an entry without `object` is not a model object. `owned_by` names
/// the provider only when the provider's own entry does not: when it is there it
/// is the provider's answer, not ours, and it is never overwritten.
fn model_entry(model: &str, provider: &str, published: Option<&Value>) -> Value {
    let mut entry = match published {
        Some(Value::Object(fields)) => Value::Object(fields.clone()),
        _ => json!({}),
    };
    entry["id"] = json!(model);
    entry["object"] = json!("model");
    if entry.get("owned_by").and_then(Value::as_str).is_none() {
        entry["owned_by"] = json!(provider);
    }
    entry
}

/// The published ids closest to a model that is missing from a catalog, as one
/// line for a log. "-" when nothing is close, so the line still reads.
fn closest_hint(catalog: &Catalog, model: &str) -> String {
    let ids = crate::closest_names(catalog.keys().map(String::as_str), model, 8);
    if ids.is_empty() {
        "-".to_string()
    } else {
        ids.join(", ")
    }
}

/// The provider's entry for a model: exact match first, then case-insensitive.
///
/// A provider that spells its own id differently from the route is not a reason
/// to drop the fields we asked for, and a case-insensitive match cannot pick the
/// wrong model -- only the same name spelled differently.
fn lookup<'a>(catalog: &'a Catalog, model: &str) -> Option<&'a Value> {
    catalog.get(model).or_else(|| {
        catalog
            .iter()
            .find(|(id, _)| id.eq_ignore_ascii_case(model))
            .map(|(_, entry)| entry)
    })
}

/// Index a `/models` response by model id.
///
/// Both envelopes in the wild are accepted -- OpenAI's
/// `{"object":"list","data":[]}` and a bare array -- because a provider that
/// answers the same question in the other shape is not worth failing a model list
/// over. Entries without a usable id are dropped rather than listed as blank.
fn catalog_from(value: Value) -> Catalog {
    let entries = match value {
        Value::Array(entries) => entries,
        other => other
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
    };
    entries
        .into_iter()
        .filter_map(|entry| {
            let id = entry.get("id").and_then(Value::as_str)?.trim().to_string();
            (!id.is_empty()).then_some((id, entry))
        })
        .collect()
}

/// Read one provider's catalog with an eligible account's key, through the pool.
async fn catalog_for_provider(
    backend: &dyn ProxyBackend,
    provider: &str,
    request_id: &str,
) -> std::result::Result<Catalog, String> {
    // A model list spends no tokens, so it is planned at zero cost, and
    // pick_account hands the reservation straight back: polling the list cannot
    // fill an account's concurrency limit. (An account with an rpm_limit does
    // count the call, which is the one place a polling client is visible to the
    // pool -- hence one call per provider, not one per model.)
    let plan = backend
        .pick_account(provider, Decimal::ZERO, request_id, 1)
        .map_err(|e| e.to_string())?;
    let Some(account) = backend.account(&plan.qualified_account()) else {
        return Err("the planned account is no longer in the pool".to_string());
    };
    fetch_catalog(&backend.http(), &account.base_url, &account.api_key).await
}

/// Read and parse one provider's model catalog.
///
/// Shared by `/v1/models` and `lgw doctor`: the two have to agree about what a
/// provider publishes, and a second copy of this is a second answer. Errors are
/// strings because every caller reports them rather than acting on them.
pub async fn fetch_catalog(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &ApiKey,
) -> std::result::Result<Catalog, String> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let response = client
        .get(&url)
        .bearer_auth(api_key.expose())
        .header(header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|e| format!("{url} is unreachable: {e}"))?;

    let status = response.status();
    if !status.is_success() {
        let text = read_bounded(response, 8 * 1024).await;
        return Err(upstream_error_message(&text)
            .unwrap_or_else(|| format!("{url} returned HTTP {}", status.as_u16())));
    }

    let text = read_bounded(response, MAX_CATALOG_BYTES).await;
    let value: Value =
        serde_json::from_str(&text).map_err(|e| format!("{url} did not answer with JSON: {e}"))?;
    Ok(catalog_from(value))
}

/// Whether the provider publishes this model id (exact, then case-insensitive).
pub fn publishes(catalog: &Catalog, model: &str) -> bool {
    lookup(catalog, model).is_some()
}

/// One upstream failure that is worth reporting to the client if no retry
/// succeeds.
struct Failure {
    status: StatusCode,
    kind: UpstreamErrorClass,
    message: String,
    retry_after: Option<Duration>,
}

fn should_retry(class: UpstreamErrorClass, attempt_no: u32, max_attempts: u32) -> bool {
    attempt_no < max_attempts && (class.retry_other_account() || class.retry_same_account())
}

fn status_for_class(class: UpstreamErrorClass) -> StatusCode {
    match class {
        UpstreamErrorClass::InsufficientCredit => StatusCode::PAYMENT_REQUIRED,
        UpstreamErrorClass::Auth => StatusCode::UNAUTHORIZED,
        UpstreamErrorClass::QuotaExhausted | UpstreamErrorClass::RateLimitBurst => {
            StatusCode::TOO_MANY_REQUESTS
        }
        UpstreamErrorClass::Overloaded | UpstreamErrorClass::Transport => StatusCode::BAD_GATEWAY,
        UpstreamErrorClass::Timeout => StatusCode::GATEWAY_TIMEOUT,
        UpstreamErrorClass::BadRequest => StatusCode::BAD_REQUEST,
        UpstreamErrorClass::Other => StatusCode::BAD_GATEWAY,
    }
}

fn plan_error_response(err: &PlanError, backend: &dyn ProxyBackend) -> Response {
    match err {
        PlanError::NoEligibleAccount {
            provider,
            retry_after,
            ..
        } => {
            // No fall-through to another provider and no silent metered spend:
            // the client is told to come back when the earliest window resets.
            let wait = retry_after.or_else(|| backend.retry_after_for(provider));
            error_response_retry(
                StatusCode::TOO_MANY_REQUESTS,
                "no_eligible_account",
                &err.to_string(),
                wait,
            )
        }
        PlanError::UnknownProvider(provider) => error_response(
            StatusCode::NOT_FOUND,
            "unknown_provider",
            &format!("unknown provider {provider}"),
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn record_attempt_failure(
    backend: &dyn ProxyBackend,
    plan: &AttemptPlan,
    class: UpstreamErrorClass,
    retry_after: Option<Duration>,
    status_code: Option<u16>,
    error_code: Option<String>,
    started_at: OffsetDateTime,
    attempt_no: u32,
    request_id: &str,
) {
    let attempt = AttemptRecord {
        request_id: request_id.to_string(),
        attempt_no,
        account_id: plan.account_id.clone(),
        provider: plan.provider.clone(),
        started_at,
        // The attempt failed before any content was produced, so it must not
        // contribute to TTFT or TPS statistics.
        ttft_ms: None,
        status_code,
        error_class: Some(class),
        error_code,
        retry_after_ms: retry_after.map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64),
        billed: false,
        bytes_sent: 0,
    };
    backend.record_failure(plan, class, retry_after, None, attempt);
}

fn estimate_request_cost(
    backend: &dyn ProxyBackend,
    provider: &str,
    model: &str,
    body: &Value,
) -> Decimal {
    let prompt = crate::meter::estimate_prompt_tokens(body);
    let max_tokens = body
        .get("max_tokens")
        .and_then(Value::as_u64)
        .or_else(|| body.get("max_completion_tokens").and_then(Value::as_u64));
    let completion = crate::meter::estimate_completion_tokens(max_tokens, prompt);
    let probe = TokenUsage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        ..Default::default()
    };
    let tier = backend.tier_at(provider, model);
    backend
        .estimate_cost(provider, model, tier, &probe, Decimal::ONE)
        // One shared definition of "unknown model" across the whole request
        // path: two different fallback constants would mean the gate and the
        // finalizer disagreed about what a request costs.
        .unwrap_or_else(|| crate::meter::conservative_estimate(&probe, Decimal::ONE))
}

fn session_key(headers: &HeaderMap, body: &Value) -> Option<String> {
    for name in ["x-session-id", "x-session-key", "x-conversation-id"] {
        if let Some(value) = headers.get(name).and_then(|v| v.to_str().ok()) {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    body.get("user")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn provider_request_id(headers: &HeaderMap) -> Option<String> {
    for name in [
        "x-request-id",
        "request-id",
        "openai-request-id",
        "x-amzn-requestid",
    ] {
        if let Some(value) = headers.get(name).and_then(|v| v.to_str().ok()) {
            return Some(value.to_string());
        }
    }
    None
}

/// Read at most the given number of bytes of an upstream error body.
async fn read_bounded(mut response: reqwest::Response, limit: usize) -> String {
    let mut buf: Vec<u8> = Vec::new();
    while buf.len() < limit {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let take = chunk.len().min(limit - buf.len());
                buf.extend_from_slice(&chunk[..take]);
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Pull the human-readable message out of an OpenAI/Anthropic error envelope.
fn upstream_error_message(body: &str) -> Option<String> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }
    let value: Value = serde_json::from_str(trimmed).ok()?;
    let obj = value.get("error").unwrap_or(&value);
    let message = obj
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| value.get("message").and_then(Value::as_str))?;
    let message = message.trim();
    if message.is_empty() {
        None
    } else {
        Some(message.to_string())
    }
}

fn sanitize_message(message: &str) -> String {
    let cleaned: String = message
        .chars()
        .filter(|c| !c.is_control())
        .take(300)
        .collect();
    if cleaned.trim().is_empty() {
        "upstream request failed".to_string()
    } else {
        cleaned
    }
}

/// A JSON body with the status the OpenAI SDKs expect from a success.
fn json_response(payload: Value) -> Response {
    let mut response = Response::new(Body::from(payload.to_string()));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    response
}

fn error_response(status: StatusCode, kind: &str, message: &str) -> Response {
    error_response_retry(status, kind, message, None)
}

fn error_response_retry(
    status: StatusCode,
    kind: &str,
    message: &str,
    retry_after: Option<Duration>,
) -> Response {
    let payload = json!({
        "error": {
            "message": sanitize_message(message),
            "type": kind,
            "code": kind,
        }
    });
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(delay) = retry_after {
        builder = builder.header(header::RETRY_AFTER, delay.as_secs().to_string());
    }
    builder
        .body(Body::from(payload.to_string()))
        .unwrap_or_else(|_| Response::new(Body::from("{}")))
}

/// Everything needed to close out a delivered request once its stream ends.
///
/// The record is written even when the client disconnects half-way: the
/// upstream has already produced (and billed) whatever was streamed, so
/// dropping the partial usage on the floor would understate the ledger.
struct Finalizer {
    backend: Arc<dyn ProxyBackend>,
    plan: AttemptPlan,
    request_id: String,
    client_model: String,
    provider_request_id: Option<String>,
    currency: String,
    credit_multiplier: Decimal,
    coverage: GateClass,
    attempt_no: u32,
    started_at: OffsetDateTime,
    estimated_prompt_tokens: u64,
    status_code: u16,
}

/// Forwards upstream bytes unchanged while metering them on the side.
struct RelayStream {
    inner: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>,
    meter: StreamMeter,
    started: Instant,
    first_content_at: Option<Instant>,
    bytes_sent: u64,
    finalizer: Option<Finalizer>,
    /// Whether this body is an SSE stream, whose terminator is meaningful.
    is_sse: bool,
    /// A silence longer than this ends the stream: policy.idle_timeout_secs.
    idle_timeout: Duration,
    /// Fires once the upstream has been silent for idle_timeout.
    idle: Pin<Box<tokio::time::Sleep>>,
}

impl RelayStream {
    fn complete(&mut self, status: RequestStatus) {
        let Some(f) = self.finalizer.take() else {
            return;
        };
        let mut result = self.meter.finish();

        // A stream that carried an error frame is a failure even though the
        // HTTP status was 200 and the bytes were relayed intact. Recording it
        // as a success inflates request counts and hides a failing provider.
        let status = if result.error.is_some() {
            RequestStatus::Error
        } else if matches!(status, RequestStatus::Ok) && self.is_sse && !result.saw_done() {
            // An SSE body that ended without its terminator was cut short --
            // by the idle timeout below, or by an upstream that closed early.
            // Calling that a success hides a broken response and inflates the
            // request count.
            tracing::warn!(
                request_id = %f.request_id,
                bytes = self.bytes_sent,
                "the SSE stream ended without its [DONE] terminator"
            );
            RequestStatus::Error
        } else {
            status
        };

        if !result.usage.measured {
            // The upstream never sent a usage frame. Estimate conservatively
            // from the request and the content we saw, and keep measured=false
            // so the estimate never contaminates real statistics.
            result.usage.prompt_tokens = result.usage.prompt_tokens.max(f.estimated_prompt_tokens);
            let completion_hint = self.meter.content_chars() / 4;
            result.usage.completion_tokens = result.usage.completion_tokens.max(completion_hint);
            result.usage.measured = false;
        }

        let cost = f
            .backend
            .estimate_cost(
                &f.plan.provider,
                &f.plan.model,
                f.plan.tier,
                &result.usage,
                f.credit_multiplier,
            )
            .unwrap_or_else(|| {
                crate::meter::conservative_estimate(&result.usage, f.credit_multiplier)
            });

        let total_ms = self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        let ttft_ms = self.first_content_at.map(|t| {
            t.saturating_duration_since(self.started)
                .as_millis()
                .min(u128::from(u64::MAX)) as u64
        });
        let tps = self.first_content_at.and_then(|first| {
            crate::meter::compute_tps(result.usage.completion_tokens, first, Instant::now())
        });
        let billed = self.bytes_sent > 0 || result.usage.measured;

        let attempt = AttemptRecord {
            request_id: f.request_id.clone(),
            attempt_no: f.attempt_no,
            account_id: f.plan.account_id.clone(),
            provider: f.plan.provider.clone(),
            started_at: f.started_at,
            ttft_ms,
            status_code: Some(f.status_code),
            error_class: None,
            error_code: None,
            retry_after_ms: None,
            billed,
            bytes_sent: self.bytes_sent,
        };
        let record = RequestRecord {
            request_id: f.request_id.clone(),
            ts: f.started_at,
            session_key: f.plan.session_key.clone(),
            client_model: f.client_model.clone(),
            provider: f.plan.provider.clone(),
            account_id: f.plan.account_id.clone(),
            model: f.plan.model.clone(),
            usage: result.usage.clone(),
            ttft_ms,
            total_ms,
            tps,
            cost,
            currency: f.currency.clone(),
            credit_multiplier: f.credit_multiplier,
            coverage: f.coverage,
            status,
            retries: f.attempt_no.saturating_sub(1),
            provider_request_id: f.provider_request_id.clone(),
        };
        f.backend
            .record_success(&f.plan, &result.usage, cost, attempt, record);
    }
}

impl Stream for RelayStream {
    type Item = reqwest::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Pending => {
                // A stalled upstream ends the stream. policy.idle_timeout_secs
                // used to be parsed and never read, so a provider that simply
                // stopped sending held the client, the connection and the
                // account's concurrency slot for as long as it liked. Ending the
                // body is the honest signal for SSE: the client sees no
                // terminator and the ledger records an error.
                if std::future::Future::poll(this.idle.as_mut(), cx).is_ready() {
                    tracing::warn!(
                        bytes = this.bytes_sent,
                        idle_secs = this.idle_timeout.as_secs(),
                        "upstream went silent; ending the relayed stream"
                    );
                    this.complete(RequestStatus::Error);
                    return Poll::Ready(None);
                }
                Poll::Pending
            }
            Poll::Ready(Some(Ok(chunk))) => {
                let first = this.meter.push(&chunk);
                if first && this.first_content_at.is_none() {
                    this.first_content_at = Some(Instant::now());
                }
                this.bytes_sent = this.bytes_sent.saturating_add(chunk.len() as u64);
                this.idle
                    .as_mut()
                    .reset(tokio::time::Instant::now() + this.idle_timeout);
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(e))) => {
                // The upstream died mid-stream: the response is already partly
                // delivered, so it cannot be retried, but the partial spend
                // still has to be recorded.
                this.complete(RequestStatus::Error);
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                this.complete(RequestStatus::Ok);
                Poll::Ready(None)
            }
        }
    }
}

impl Drop for RelayStream {
    fn drop(&mut self) {
        if self.finalizer.is_some() {
            // The client went away. Dropping the upstream body aborts the
            // upstream request so it stops billing, and we record what was
            // already delivered.
            self.complete(RequestStatus::Aborted);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::{Pool, gate};
    use crate::config::{Config, SecretRef};
    use crate::domain::GateKind;
    use serde_json::json;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ------------------------------------------------------------ mock upstream

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum MockMode {
        /// First call: HTTP 429 with a quota-shaped body. Later calls: a
        /// normal OpenAI SSE stream including the empty-choices usage frame.
        Rotate429ThenStream,
        /// Always a non-streaming JSON completion.
        JsonOk,
    }

    /// What a provider answers `GET /models` with. The default is the one model
    /// the test config routes to; tests about the catalog set their own.
    const MOCK_CATALOG: &str = r#"{"object":"list","data":[{"id":"mock/model","object":"model"}]}"#;

    struct MockUpstream {
        mode: MockMode,
        calls: AtomicUsize,
        bearers: Mutex<Vec<String>>,
        bodies: Mutex<Vec<Value>>,
        catalog: String,
    }

    async fn mock_models(State(m): State<Arc<MockUpstream>>, headers: HeaderMap) -> Response {
        let bearer = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        m.bearers.lock().unwrap().push(bearer);
        let mut response = Response::new(Body::from(m.catalog.clone()));
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
        response
    }

    async fn mock_chat(
        State(m): State<Arc<MockUpstream>>,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        let bearer = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        m.bearers.lock().unwrap().push(bearer);
        m.bodies
            .lock()
            .unwrap()
            .push(serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null));
        let call = m.calls.fetch_add(1, Ordering::SeqCst);

        if m.mode == MockMode::Rotate429ThenStream && call == 0 {
            let mut response = Response::new(Body::from(
                r#"{"error":{"message":"window exhausted","type":"insufficient_quota","code":"usage_exceeded"}}"#,
            ));
            *response.status_mut() = StatusCode::TOO_MANY_REQUESTS;
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, "3600".parse().unwrap());
            return response;
        }

        if m.mode == MockMode::JsonOk {
            let body = r#"{"id":"c1","object":"chat.completion","model":"mock/model","choices":[{"index":0,"message":{"role":"assistant","content":"Hello"},"finish_reason":"stop"}],"usage":{"prompt_tokens":12,"completion_tokens":3,"total_tokens":15}}"#;
            let mut response = Response::new(Body::from(body));
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
            return response;
        }

        let mut sse = String::new();
        for chunk in [
            json!({"id":"c1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}),
            json!({"id":"c1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}),
            json!({"id":"c1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
            json!({"id":"c1","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":12,"completion_tokens":3,"total_tokens":15}}),
        ] {
            sse.push_str("data: ");
            sse.push_str(&chunk.to_string());
            sse.push_str("\n\n");
        }
        sse.push_str("data: [DONE]\n\n");
        let mut response = Response::new(Body::from(sse));
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, "text/event-stream".parse().unwrap());
        response
    }

    async fn spawn_mock(mode: MockMode) -> (String, Arc<MockUpstream>) {
        spawn_mock_with_catalog(mode, MOCK_CATALOG).await
    }

    /// A mock provider whose `/models` answers with the given catalog.
    async fn spawn_mock_with_catalog(mode: MockMode, catalog: &str) -> (String, Arc<MockUpstream>) {
        let state = Arc::new(MockUpstream {
            mode,
            calls: AtomicUsize::new(0),
            bearers: Mutex::new(Vec::new()),
            bodies: Mutex::new(Vec::new()),
            catalog: catalog.to_string(),
        });
        let app = Router::new()
            .route("/chat/completions", post(mock_chat))
            .route("/models", get(mock_models))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), state)
    }

    // ------------------------------------------------------------ fake backend

    struct RecordedSuccess {
        plan: AttemptPlan,
        usage: TokenUsage,
        cost: Decimal,
        attempt: AttemptRecord,
        record: RequestRecord,
    }

    struct RecordedFailure {
        account_id: String,
        class: UpstreamErrorClass,
        attempt: AttemptRecord,
    }

    struct FakeBackend {
        config: Config,
        pool: Pool,
        http: reqwest::Client,
        successes: Mutex<Vec<RecordedSuccess>>,
        failures: Mutex<Vec<RecordedFailure>>,
    }

    impl FakeBackend {
        fn new(config: Config, pool: Pool) -> Self {
            FakeBackend {
                config,
                pool,
                http: reqwest::Client::new(),
                successes: Mutex::new(Vec::new()),
                failures: Mutex::new(Vec::new()),
            }
        }
    }

    impl ProxyBackend for FakeBackend {
        fn config(&self) -> &Config {
            &self.config
        }

        fn resolve(&self, client_model: &str) -> Result<ResolvedRoute> {
            self.config.resolve(client_model)
        }

        fn plan(
            &self,
            client_model: &str,
            session_key: Option<&str>,
            est_cost: Decimal,
            request_id: &str,
            attempt_no: u32,
            exclude: &[String],
        ) -> std::result::Result<(ResolvedRoute, AttemptPlan), PlanError> {
            let route = self
                .config
                .resolve(client_model)
                .map_err(|e| PlanError::UnknownProvider(e.to_string()))?;
            let plan = self.pool.select(
                &route.provider,
                &route.model,
                session_key,
                est_cost,
                request_id,
                attempt_no,
                exclude,
            )?;
            Ok((route, plan))
        }

        fn release(&self, plan: &AttemptPlan) {
            // The test backend commits reservations through the same pool, so it
            // has to give them back the same way.
            self.pool.settle(plan, Decimal::ZERO, false);
        }

        fn pick_account(
            &self,
            provider: &str,
            est_cost: Decimal,
            request_id: &str,
            attempt_no: u32,
        ) -> std::result::Result<AttemptPlan, PlanError> {
            let plan =
                self.pool
                    .select(provider, "", None, est_cost, request_id, attempt_no, &[])?;
            // Released immediately: see the production impl above.
            self.pool.settle(&plan, Decimal::ZERO, false);
            Ok(plan)
        }

        fn account(&self, account_id: &str) -> Option<Account> {
            self.pool.account(account_id)
        }

        fn retry_after_for(&self, provider: &str) -> Option<Duration> {
            self.pool.retry_after_for(provider)
        }

        fn http(&self) -> reqwest::Client {
            self.http.clone()
        }

        fn tier_at(&self, _provider: &str, _model: &str) -> u32 {
            0
        }

        fn estimate_cost(
            &self,
            _provider: &str,
            _model: &str,
            _tier: u32,
            _usage: &TokenUsage,
            _multiplier: Decimal,
        ) -> Option<Decimal> {
            None
        }

        fn record_success(
            &self,
            plan: &AttemptPlan,
            usage: &TokenUsage,
            cost: Decimal,
            attempt: AttemptRecord,
            record: RequestRecord,
        ) {
            self.pool.settle(plan, cost, true);
            self.successes.lock().unwrap().push(RecordedSuccess {
                plan: plan.clone(),
                usage: usage.clone(),
                cost,
                attempt,
                record,
            });
        }

        fn record_failure(
            &self,
            plan: &AttemptPlan,
            class: UpstreamErrorClass,
            retry_after: Option<Duration>,
            window_reset: Option<OffsetDateTime>,
            attempt: AttemptRecord,
        ) {
            self.pool.settle(plan, Decimal::ZERO, false);
            self.pool
                .note_error(&plan.qualified_account(), class, retry_after, window_reset);
            self.failures.lock().unwrap().push(RecordedFailure {
                account_id: plan.account_id.clone(),
                class,
                attempt,
            });
        }
    }

    // ------------------------------------------------------------ helpers

    fn test_config(base_url: &str) -> Config {
        let toml = format!(
            r#"
[server]
listen = "127.0.0.1:0"

[policy]
max_retries = 3

[providers.mock]
adapter = "none"
base_url = "{base_url}"

[providers.mock.accounts.a1]
key = "sk-a1"

[providers.mock.accounts.a2]
key = "sk-a2"

[routes]
"mock/model" = "mock"
"#
        );
        Config::from_toml_str(&toml).unwrap()
    }

    /// Give an account a probed window. Takes the canonical provider/id, since
    /// a bare id is ambiguous across providers.
    fn activate(pool: &Pool, qualified_id: &str, used: i64) {
        let gates = vec![gate(
            GateKind::FiveHour,
            Decimal::from(100),
            Decimal::from(used),
            None,
        )];
        pool.apply_probe(qualified_id, gates, now());
    }

    fn fake_backend(base_url: &str) -> Arc<FakeBackend> {
        let config = test_config(base_url);
        let accounts = config
            .build_accounts(&crate::secrets::CredentialStore::memory())
            .unwrap();
        let pool = Pool::new(accounts, &config.policy);
        activate(&pool, "mock/a1", 1);
        activate(&pool, "mock/a2", 0);
        Arc::new(FakeBackend::new(config, pool))
    }

    /// Two providers with one route each: what a model list has to fan out over.
    fn fake_backend_two(base_a: &str, base_b: &str) -> Arc<FakeBackend> {
        let toml = format!(
            r#"
[server]
listen = "127.0.0.1:0"

[providers.mock]
adapter = "none"
base_url = "{base_a}"

[providers.mock.accounts.a1]
key = "sk-a1"

[providers.other]
adapter = "none"
base_url = "{base_b}"

[providers.other.accounts.b1]
key = "sk-b1"

[routes]
"mock/model" = "mock"
"other/model" = "other"
"#
        );
        let config = Config::from_toml_str(&toml).unwrap();
        let accounts = config
            .build_accounts(&crate::secrets::CredentialStore::memory())
            .unwrap();
        let pool = Pool::new(accounts, &config.policy);
        activate(&pool, "mock/a1", 0);
        activate(&pool, "other/b1", 0);
        Arc::new(FakeBackend::new(config, pool))
    }

    async fn spawn_router(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    async fn wait_for_successes(fake: &Arc<FakeBackend>, want: usize) {
        for _ in 0..400 {
            if fake.successes.lock().unwrap().len() >= want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("timed out waiting for {want} recorded successes");
    }

    fn chat_body(stream: bool) -> Value {
        json!({
            "model": "mock/model",
            "stream": stream,
            "messages": [{"role": "user", "content": "hi"}],
        })
    }

    // ------------------------------------------------------------ tests

    #[tokio::test]
    async fn stream_is_relayed_usage_recorded_and_a_429_rotates_accounts() {
        let (upstream_url, mock) = spawn_mock(MockMode::Rotate429ThenStream).await;
        let fake = fake_backend(&upstream_url);
        let addr = spawn_router(build_router_with(fake.clone())).await;

        let response = reqwest::Client::new()
            .post(format!("{addr}/v1/chat/completions"))
            .json(&chat_body(true))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "the retry must be transparent");
        let text = response.text().await.unwrap();
        assert!(text.contains("Hello"), "the relayed body was: {text}");
        assert!(text.contains("[DONE]"), "the relayed body was: {text}");

        wait_for_successes(&fake, 1).await;
        let successes = fake.successes.lock().unwrap();
        assert_eq!(successes.len(), 1);
        let success = &successes[0];
        assert_eq!(
            success.plan.account_id, "a2",
            "the second account must serve after the first is parked"
        );
        assert_eq!(success.usage.prompt_tokens, 12);
        assert_eq!(success.usage.completion_tokens, 3);
        assert!(
            success.usage.measured,
            "usage must come from the usage frame"
        );
        assert!(success.cost > Decimal::ZERO);
        assert!(success.attempt.billed);
        assert!(
            success.attempt.ttft_ms.is_some(),
            "a delivered stream must record TTFT"
        );
        assert_eq!(success.record.status, RequestStatus::Ok);
        assert_eq!(success.record.retries, 1);
        assert_eq!(success.record.coverage, GateClass::Covered);
        drop(successes);

        let failures = fake.failures.lock().unwrap();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].account_id, "a1");
        assert_eq!(failures[0].class, UpstreamErrorClass::QuotaExhausted);
        assert!(
            failures[0].attempt.ttft_ms.is_none(),
            "an attempt that failed before content has no TTFT"
        );
        assert!(!failures[0].attempt.billed);
        drop(failures);

        assert_eq!(
            *mock.bearers.lock().unwrap(),
            vec!["Bearer sk-a1".to_string(), "Bearer sk-a2".to_string()],
            "each attempt must carry its own account key"
        );
        let bodies = mock.bodies.lock().unwrap();
        assert_eq!(bodies.len(), 2, "both attempts reached the upstream");
        for body in bodies.iter() {
            assert_eq!(
                body["stream_options"]["include_usage"],
                json!(true),
                "include_usage must be injected or the ledger records zero forever"
            );
        }
    }

    #[tokio::test]
    async fn models_passthrough_uses_an_eligible_account() {
        let (upstream_url, mock) = spawn_mock(MockMode::JsonOk).await;
        let fake = fake_backend(&upstream_url);
        let addr = spawn_router(build_router_with(fake.clone())).await;

        let response = reqwest::Client::new()
            .get(format!("{addr}/v1/models"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let text = response.text().await.unwrap();
        assert!(text.contains("mock/model"), "the model list was: {text}");
        assert_eq!(
            *mock.bearers.lock().unwrap(),
            vec!["Bearer sk-a1".to_string()],
            "the passthrough must authenticate as an eligible account"
        );
        // Picking an account commits a reservation; a model list spends no
        // tokens, so it must give it straight back. While it did not, every call
        // burned one concurrency slot against the account's own
        // concurrency_limit -- and, with that limit set, the account became
        // permanently unselectable after N listings.
        assert_eq!(
            fake.pool.inflight_count("mock/a1"),
            0,
            "a passthrough must not hold a reservation"
        );
    }

    /// The list is the configured routes, and the fields are the provider's own.
    #[tokio::test]
    async fn the_model_list_is_the_routes_described_by_the_provider() {
        let catalog = r#"{"object":"list","data":[
            {"id":"mock/not-routed","object":"model","display_name":"Not Routed"},
            {"id":"mock/model","object":"model","owned_by":"z-ai","display_name":"Mock Model",
             "context_length":200000,
             "pricing":{"currency":"USD","unit":"per_1m_tokens","input":0.44,"output":1.86}}
        ]}"#;
        let (upstream_url, mock) = spawn_mock_with_catalog(MockMode::JsonOk, catalog).await;
        let addr = spawn_router(build_router_with(fake_backend(&upstream_url))).await;

        let response = reqwest::Client::new()
            .get(format!("{addr}/v1/models"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body: Value = response.json().await.unwrap();

        assert_eq!(body["object"], json!("list"));
        let data = body["data"].as_array().unwrap();
        assert_eq!(
            data.len(),
            1,
            "the provider's catalog is not advertised: {body}"
        );
        assert_eq!(data[0]["id"], json!("mock/model"));
        assert_eq!(data[0]["object"], json!("model"));
        // Whatever the provider published is passed through, field for field.
        assert_eq!(data[0]["display_name"], json!("Mock Model"));
        assert_eq!(data[0]["context_length"], json!(200000));
        assert_eq!(data[0]["pricing"]["unit"], json!("per_1m_tokens"));
        assert_eq!(data[0]["pricing"]["input"], json!(0.44));
        // And its own answer is not overwritten by ours.
        assert_eq!(data[0]["owned_by"], json!("z-ai"));
        assert_eq!(
            *mock.bearers.lock().unwrap(),
            vec!["Bearer sk-a1".to_string()]
        );
    }

    /// A provider that does not list a configured route does not remove it.
    ///
    /// The route is what this gateway will actually serve, so hiding it because
    /// the provider's catalog is stale -- or spells the name differently -- would
    /// make the list lie about what works. Fields we cannot source stay absent.
    #[tokio::test]
    async fn a_route_the_provider_does_not_list_is_still_listed() {
        let (upstream_url, _mock) = spawn_mock_with_catalog(
            MockMode::JsonOk,
            r#"{"object":"list","data":[{"id":"mock/other","object":"model"}]}"#,
        )
        .await;
        let addr = spawn_router(build_router_with(fake_backend(&upstream_url))).await;

        let body: Value = reqwest::Client::new()
            .get(format!("{addr}/v1/models"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let data = body["data"].as_array().unwrap();
        assert_eq!(data.len(), 1, "{body}");
        assert_eq!(data[0]["id"], json!("mock/model"));
        assert_eq!(data[0]["object"], json!("model"));
        assert_eq!(data[0]["owned_by"], json!("mock"), "{body}");
        assert!(
            data[0].get("display_name").is_none(),
            "no field is invented for the provider: {body}"
        );
    }

    /// An unreachable provider leaves the route table intact.
    ///
    /// A model list is how a client decides what to send; failing it because one
    /// provider's status endpoint is down would take the whole gateway out of the
    /// picker even though every other provider is fine.
    #[tokio::test]
    async fn an_unreadable_catalog_still_lists_the_routes() {
        // Port 1 is not listening, so the upstream call fails.
        let addr = spawn_router(build_router_with(fake_backend("http://127.0.0.1:1"))).await;

        let response = reqwest::Client::new()
            .get(format!("{addr}/v1/models"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            200,
            "a broken provider is not a broken model list"
        );
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["data"][0]["id"], json!("mock/model"));
        assert_eq!(body["data"][0]["owned_by"], json!("mock"));
    }

    /// Every configured route, from every provider, one upstream call each.
    #[tokio::test]
    async fn the_model_list_fans_out_over_providers_once_each() {
        let (a_url, a) = spawn_mock_with_catalog(
            MockMode::JsonOk,
            r#"{"object":"list","data":[{"id":"mock/model","object":"model","display_name":"A"}]}"#,
        )
        .await;
        let (b_url, b) = spawn_mock_with_catalog(
            MockMode::JsonOk,
            r#"{"object":"list","data":[{"id":"other/model","object":"model","display_name":"B"}]}"#,
        )
        .await;
        let addr = spawn_router(build_router_with(fake_backend_two(&a_url, &b_url))).await;

        let body: Value = reqwest::Client::new()
            .get(format!("{addr}/v1/models"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let data = body["data"].as_array().unwrap();
        assert_eq!(data.len(), 2, "{body}");
        assert_eq!(data[0]["id"], json!("mock/model"));
        assert_eq!(data[1]["id"], json!("other/model"));
        assert_eq!(data[0]["display_name"], json!("A"));
        assert_eq!(data[1]["display_name"], json!("B"));
        assert_eq!(
            *a.bearers.lock().unwrap(),
            vec!["Bearer sk-a1".to_string()],
            "each provider is asked once, with its own key"
        );
        assert_eq!(*b.bearers.lock().unwrap(), vec!["Bearer sk-b1".to_string()]);

        // ?provider= narrows the answer to that provider's routes, without asking
        // the other one again.
        let filtered: Value = reqwest::Client::new()
            .get(format!("{addr}/v1/models?provider=other"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let data = filtered["data"].as_array().unwrap();
        assert_eq!(data.len(), 1, "{filtered}");
        assert_eq!(data[0]["id"], json!("other/model"));
        assert_eq!(
            a.bearers.lock().unwrap().len(),
            1,
            "the filtered list does not re-ask the other provider"
        );

        let unknown = reqwest::Client::new()
            .get(format!("{addr}/v1/models?provider=nope"))
            .send()
            .await
            .unwrap();
        assert_eq!(unknown.status(), 404);
    }

    #[tokio::test]
    async fn non_streaming_response_usage_is_recorded() {
        let (upstream_url, _mock) = spawn_mock(MockMode::JsonOk).await;
        let fake = fake_backend(&upstream_url);
        let addr = spawn_router(build_router_with(fake.clone())).await;

        let response = reqwest::Client::new()
            .post(format!("{addr}/v1/chat/completions"))
            .json(&chat_body(false))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let text = response.text().await.unwrap();
        assert!(text.contains("Hello"));

        wait_for_successes(&fake, 1).await;
        let successes = fake.successes.lock().unwrap();
        assert_eq!(successes[0].usage.prompt_tokens, 12);
        assert_eq!(successes[0].usage.completion_tokens, 3);
        assert!(successes[0].usage.measured);
        assert_eq!(successes[0].plan.account_id, "a1");
    }

    #[tokio::test]
    async fn no_eligible_account_is_429_with_retry_after_and_no_upstream_call() {
        let fake = fake_backend("http://127.0.0.1:1");
        let longest = now() + time::Duration::seconds(300);
        let earliest = now() + time::Duration::seconds(120);
        fake.pool.note_error(
            "mock/a1",
            UpstreamErrorClass::QuotaExhausted,
            None,
            Some(earliest),
        );
        fake.pool.note_error(
            "mock/a2",
            UpstreamErrorClass::QuotaExhausted,
            None,
            Some(longest),
        );
        let addr = spawn_router(build_router_with(fake.clone())).await;

        let response = reqwest::Client::new()
            .post(format!("{addr}/v1/chat/completions"))
            .json(&chat_body(true))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 429);
        let raw = response
            .headers()
            .get(header::RETRY_AFTER)
            .expect("a 429 must carry Retry-After")
            .to_str()
            .unwrap()
            .to_string();
        let secs: u64 = raw.parse().expect("Retry-After must be delta-seconds");
        assert!(
            (1..=120).contains(&secs),
            "the earliest reset wins, got {secs}"
        );

        // The body has to explain the refusal, not just name it. An operator
        // reading "no eligible account" cannot tell a spent allowance from a
        // request priced above it, and the two need different responses.
        let body = response.text().await.unwrap();
        assert!(
            body.contains("parked after a quota error"),
            "the refusal must name the reason: {body}"
        );
        assert!(
            body.contains("mock/a"),
            "the refusal must name the account that came closest: {body}"
        );

        assert!(fake.successes.lock().unwrap().is_empty());
        assert!(fake.failures.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn unknown_model_is_rejected_before_any_upstream_call() {
        let (upstream_url, mock) = spawn_mock(MockMode::JsonOk).await;
        let fake = fake_backend(&upstream_url);
        let addr = spawn_router(build_router_with(fake.clone())).await;

        let response = reqwest::Client::new()
            .post(format!("{addr}/v1/chat/completions"))
            .json(&json!({"model": "nope/model", "messages": []}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 404);
        assert_eq!(
            mock.calls.load(Ordering::SeqCst),
            0,
            "an unroutable model must never reach a provider"
        );
    }

    #[tokio::test]
    async fn local_bearer_auth_is_enforced_when_configured() {
        let (upstream_url, _mock) = spawn_mock(MockMode::JsonOk).await;
        let mut config = test_config(&upstream_url);
        config.server.local_key = Some(SecretRef::Literal("sk-local".into()));
        let accounts = config
            .build_accounts(&crate::secrets::CredentialStore::memory())
            .unwrap();
        let pool = Pool::new(accounts, &config.policy);
        activate(&pool, "mock/a1", 1);
        activate(&pool, "mock/a2", 0);
        let fake = Arc::new(FakeBackend::new(config, pool));
        let addr = spawn_router(build_router_with(fake)).await;
        let client = reqwest::Client::new();
        let url = format!("{addr}/v1/chat/completions");

        let missing = client
            .post(&url)
            .json(&chat_body(false))
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), 401);

        let wrong = client
            .post(&url)
            .bearer_auth("sk-nope")
            .json(&chat_body(false))
            .send()
            .await
            .unwrap();
        assert_eq!(wrong.status(), 401);

        let accepted = client
            .post(&url)
            .bearer_auth("sk-local")
            .json(&chat_body(false))
            .send()
            .await
            .unwrap();
        assert_eq!(accepted.status(), 200);
    }

    #[tokio::test]
    async fn request_body_over_the_configured_limit_is_rejected() {
        let (upstream_url, _mock) = spawn_mock(MockMode::JsonOk).await;
        let mut config = test_config(&upstream_url);
        config.policy.max_body_bytes = 64;
        let accounts = config
            .build_accounts(&crate::secrets::CredentialStore::memory())
            .unwrap();
        let pool = Pool::new(accounts, &config.policy);
        activate(&pool, "a1", 1);
        let fake = Arc::new(FakeBackend::new(config, pool));
        let addr = spawn_router(build_router_with(fake)).await;

        let big = json!({
            "model": "mock/model",
            "messages": [{"role": "user", "content": "x".repeat(4096)}],
        });
        let response = reqwest::Client::new()
            .post(format!("{addr}/v1/chat/completions"))
            .json(&big)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 413);
    }

    #[tokio::test]
    async fn dropping_the_relay_records_a_partial_aborted_request() {
        let fake = fake_backend("http://127.0.0.1:1");
        let plan = fake
            .pool
            .select("mock", "mock/model", None, Decimal::ZERO, "req-1", 1, &[])
            .unwrap();
        let payload: &'static [u8] =
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n";
        let finalizer = Finalizer {
            backend: fake.clone(),
            plan,
            request_id: "req-1".to_string(),
            client_model: "mock/model".to_string(),
            provider_request_id: None,
            currency: "USD".to_string(),
            credit_multiplier: Decimal::ONE,
            coverage: GateClass::Covered,
            attempt_no: 1,
            started_at: now(),
            estimated_prompt_tokens: 10,
            status_code: 200,
        };
        let chunks: Vec<reqwest::Result<Bytes>> = vec![Ok(Bytes::from_static(payload))];
        let mut relay = RelayStream {
            inner: Box::pin(futures_util::stream::iter(chunks)),
            meter: StreamMeter::sse(),
            started: Instant::now(),
            first_content_at: None,
            bytes_sent: 0,
            finalizer: Some(finalizer),
            is_sse: true,
            idle_timeout: Duration::from_secs(120),
            idle: Box::pin(tokio::time::sleep(Duration::from_secs(120))),
        };

        use futures_util::StreamExt;
        assert!(relay.next().await.is_some());
        // The client disconnects here: dropping the stream aborts the upstream
        // response and records the partial spend.
        drop(relay);

        let successes = fake.successes.lock().unwrap();
        assert_eq!(successes.len(), 1);
        assert_eq!(successes[0].record.status, RequestStatus::Aborted);
        assert_eq!(successes[0].attempt.bytes_sent, payload.len() as u64);
        assert!(successes[0].attempt.billed);
        assert!(successes[0].attempt.ttft_ms.is_some());
    }
}
