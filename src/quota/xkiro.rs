//! xKiro quota adapter.
//!
//! FILE OWNER: quota-dev.
//!
//! Endpoint: GET {pinned probe host}/v1/usage with Authorization: Bearer <key>.
//!
//! Recorded payload shape (docs.xkiro.com/api/usage):
//! {
//!   "plan": "pro",
//!   "windows": [
//!     { "kind": "short", "window_sec": 18000, "spent_usd": "0.000356",
//!       "cap_usd": "2.000000", "remaining_usd": "1.999644", "resets_in_sec": 3612 },
//!     { "kind": "long",  "window_sec": 604800, "spent_usd": "1.250000",
//!       "cap_usd": "10.000000", "remaining_usd": "8.750000", "resets_in_sec": 402113 }
//!   ],
//!   "free_tokens": { "used_today": 1200, "limit_per_day": 10000, "remaining": 8800 },
//!   "wallet": { "balance_usd": "5.000000", "held_usd": "0.250000" }
//! }
//!
//! Mapping:
//!   windows[]        -> FiveHour / Weekly, chosen by the window_sec VALUE
//!   wallet           -> Wallet        (Metered: real money)
//!   free_tokens      -> FreeTokens    (Covered daily token allowance)

use super::{get_body, json_money, json_u64, ProbeError, ProbeOutcome, QuotaProbe};
use crate::accounts::gate;
use crate::domain::{Account, Gate, GateKind};
use async_trait::async_trait;
use rust_decimal::Decimal;
use serde_json::{Map, Value};

/// Path on the pinned probe host.
pub(crate) const USAGE_PATH: &str = "/v1/usage";

/// A five-hour window is 5 * 3600 seconds; a weekly window is 7 * 86400.
///
/// These two numbers are the ONLY thing that decides which window a reading
/// belongs to. The provider's own label field is ignored on purpose: it is
/// cosmetic, providers relabel it, and mapping it wrongly would apply the
/// weekly cap to the five-hour window (or the reverse), which is exactly how a
/// gateway ends up silently spending metered credit.
const FIVE_HOUR_SECS: u64 = 18_000;
const WEEKLY_SECS: u64 = 604_800;

/// xKiro probe adapter.
pub struct XkiroProbe {
    /// Pinned probe host, from configuration.
    pub base_url: String,
}

impl XkiroProbe {
    pub fn new(base_url: String) -> Self {
        XkiroProbe { base_url }
    }
}

/// Map a recorded /v1/usage body to gates.
///
/// Kept separate from the HTTP call so the mapping can be driven directly by
/// recorded fixtures.
pub(crate) fn outcome_from_body(body: &str) -> Result<ProbeOutcome, ProbeError> {
    let value: Value = serde_json::from_str(body)
        .map_err(|_| ProbeError::new("xkiro /v1/usage: response is not valid JSON"))?;
    let object = value.as_object().ok_or_else(|| {
        ProbeError::new("xkiro /v1/usage: response is not a JSON object")
    })?;

    let mut gates = Vec::new();

    if let Some(windows) = object.get("windows").and_then(Value::as_array) {
        for window in windows {
            if let Some(g) = window_gate(window) {
                gates.push(g);
            }
        }
    }

    if let Some(free) = object.get("free_tokens").and_then(Value::as_object)
        && let Some(g) = free_tokens_gate(free) {
            gates.push(g);
        }

    // An explicit null wallet is a real reading ("no wallet on this plan"), so
    // it must not become a zero-balance gate. A missing amount inside a present
    // wallet object is likewise not treated as zero.
    if let Some(wallet) = object.get("wallet").and_then(Value::as_object)
        && let Some(g) = wallet_gate(wallet) {
            gates.push(g);
        }

    if gates.is_empty() {
        // Never report an empty success: the caller would carry nothing forward
        // and the CLI would show a healthy account with no quota at all.
        return Err(ProbeError::new(
            "xkiro /v1/usage: response contained no usable quota reading",
        ));
    }

    Ok(ProbeOutcome {
        gates,
        plan: object.get("plan").and_then(|p| p.as_str()).map(str::to_string),
        // xKiro does not report a currency; amounts are USD by contract.
        currency: None,
    })
}

fn window_gate(window: &Value) -> Option<Gate> {
    let window_sec = json_u64(window.get("window_sec")?)?;
    let kind = match window_sec {
        FIVE_HOUR_SECS => GateKind::FiveHour,
        WEEKLY_SECS => GateKind::Weekly,
        // An unrecognised window is not guessed at: there is no gate kind for
        // "some other window", and inventing one would corrupt the router.
        _ => return None,
    };

    let spent = window.get("spent_usd").and_then(json_money);
    let cap = window.get("cap_usd").and_then(json_money);
    let remaining = window.get("remaining_usd").and_then(json_money);

    let cap = cap.or_else(|| Some(spent? + remaining?))?;
    let used = spent.or_else(|| remaining.map(|r| (cap - r).max(Decimal::ZERO)))?;

    let resets_at = window
        .get("resets_in_sec")
        .and_then(json_u64)
        .map(|secs| crate::domain::now() + time::Duration::seconds(secs as i64));

    Some(gate(kind, cap, used, resets_at))
}

fn free_tokens_gate(free: &Map<String, Value>) -> Option<Gate> {
    let used_today = free.get("used_today").and_then(json_u64);
    let limit = free.get("limit_per_day").and_then(json_u64);
    let remaining = free.get("remaining").and_then(json_u64);

    let cap = limit.or_else(|| used_today?.checked_add(remaining?))?;
    let used = used_today.or_else(|| remaining.map(|r| cap.saturating_sub(r)))?;

    // The allowance is a rolling 24h window the provider does not timestamp.
    Some(gate(
        GateKind::FreeTokens,
        Decimal::from(cap),
        Decimal::from(used),
        None,
    ))
}

fn wallet_gate(wallet: &Map<String, Value>) -> Option<Gate> {
    // A wallet is a bearer balance, not a budget: the cap is what is left and
    // nothing has been drawn from it yet. held_usd is money already in flight
    // and is deliberately NOT subtracted -- it is not spent, and the hard gate
    // adds its own in-flight margin.
    let balance = wallet.get("balance_usd").and_then(json_money)?;
    Some(gate(GateKind::Wallet, balance, Decimal::ZERO, None))
}

#[async_trait]
impl QuotaProbe for XkiroProbe {
    fn name(&self) -> &'static str {
        "xkiro"
    }

    async fn probe(
        &self,
        account: &Account,
        client: &reqwest::Client,
    ) -> Result<ProbeOutcome, ProbeError> {
        let url = format!(
            "{}{}",
            self.base_url.trim_end_matches('/'),
            USAGE_PATH
        );
        let body = get_body(client, &url, &account.api_key, "xkiro /v1/usage").await?;
        outcome_from_body(&body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{parse_money, GateClass, GateKind};
    use crate::quota::testutil::{strict_client, test_account, Reply, TestServer};

    /// Recorded /v1/usage response: both windows, free tokens and a wallet.
    const USAGE_BODY: &str = r#"{
      "plan": "pro",
      "windows": [
        {
          "kind": "short",
          "window_sec": 18000,
          "spent_usd": "0.000356",
          "cap_usd": "2.000000",
          "remaining_usd": "1.999644",
          "resets_in_sec": 3612
        },
        {
          "kind": "long",
          "window_sec": 604800,
          "spent_usd": "1.250000",
          "cap_usd": "10.000000",
          "remaining_usd": "8.750000",
          "resets_in_sec": 402113
        }
      ],
      "free_tokens": {
        "used_today": 1200,
        "limit_per_day": 10000,
        "remaining": 8800
      },
      "wallet": { "balance_usd": "5.000000", "held_usd": "0.250000" }
    }"#;

    fn gate_of(outcome: &ProbeOutcome, kind: GateKind) -> &Gate {
        outcome
            .gates
            .iter()
            .find(|g| g.kind == kind)
            .unwrap_or_else(|| panic!("expected a {kind:?} gate"))
    }

    #[test]
    fn xkiro_maps_windows_wallet_and_free_tokens() {
        let outcome = outcome_from_body(USAGE_BODY).expect("recorded payload must parse");
        assert_eq!(outcome.plan.as_deref(), Some("pro"));

        let five = gate_of(&outcome, GateKind::FiveHour);
        assert_eq!(five.cap, parse_money("2.000000").unwrap());
        assert_eq!(five.used, parse_money("0.000356").unwrap());
        assert_eq!(five.class(), GateClass::Covered);
        let reset = five.resets_at.expect("resets_in_sec becomes resets_at");
        assert!(reset > crate::domain::now(), "reset must be in the future");
        assert!(reset <= crate::domain::now() + time::Duration::seconds(3612));

        let weekly = gate_of(&outcome, GateKind::Weekly);
        assert_eq!(weekly.cap, parse_money("10.000000").unwrap());
        assert_eq!(weekly.used, parse_money("1.250000").unwrap());

        let free = gate_of(&outcome, GateKind::FreeTokens);
        assert_eq!(free.cap, Decimal::from(10_000u64));
        assert_eq!(free.used, Decimal::from(1_200u64));
        assert_eq!(free.class(), GateClass::Covered);

        // The wallet is real money: cap is the balance, nothing spent yet.
        let wallet = gate_of(&outcome, GateKind::Wallet);
        assert_eq!(wallet.cap, parse_money("5.000000").unwrap());
        assert_eq!(wallet.used, Decimal::ZERO);
        assert_eq!(wallet.class(), GateClass::Metered);
        assert_eq!(wallet.remaining(), parse_money("5.000000").unwrap());
    }

    /// The provider's label field is cosmetic; window_sec alone decides.
    #[test]
    fn xkiro_window_kind_follows_window_sec_not_kind() {
        let body = r#"{
          "windows": [
            { "kind": "short", "window_sec": 604800, "spent_usd": "1", "cap_usd": "10" },
            { "kind": "long",  "window_sec": 18000,  "spent_usd": "2", "cap_usd": "20" },
            { "kind": "medium","window_sec": 86400,  "spent_usd": "3", "cap_usd": "30" }
          ]
        }"#;
        let outcome = outcome_from_body(body).expect("payload must parse");

        let kinds: Vec<GateKind> = outcome.gates.iter().map(|g| g.kind).collect();
        assert_eq!(
            kinds,
            vec![GateKind::Weekly, GateKind::FiveHour],
            "604800 is weekly and 18000 is five-hour whatever kind says;              an unrecognised window is dropped rather than guessed"
        );
        assert_eq!(
            gate_of(&outcome, GateKind::Weekly).cap,
            parse_money("10").unwrap()
        );
        assert_eq!(
            gate_of(&outcome, GateKind::FiveHour).cap,
            parse_money("20").unwrap()
        );
        assert!(gate_of(&outcome, GateKind::Weekly).resets_at.is_none());
    }

    #[test]
    fn xkiro_tolerates_missing_optional_fields() {
        let body = r#"{
          "windows": [
            { "window_sec": 18000, "spent_usd": "0.5", "cap_usd": "2.0" }
          ],
          "wallet": null
        }"#;
        let outcome = outcome_from_body(body).expect("payload must parse");

        assert_eq!(outcome.gates.len(), 1, "absent sections produce no gates");
        assert_eq!(outcome.plan, None);
        assert_eq!(gate_of(&outcome, GateKind::FiveHour).used, parse_money("0.5").unwrap());
    }

    #[test]
    fn xkiro_derives_a_missing_cap_from_spent_plus_remaining() {
        let body = r#"{
          "windows": [
            { "window_sec": 18000, "spent_usd": "0.5", "remaining_usd": "1.5" }
          ]
        }"#;
        let outcome = outcome_from_body(body).expect("payload must parse");
        let five = gate_of(&outcome, GateKind::FiveHour);
        assert_eq!(five.cap, parse_money("2.0").unwrap());
        assert_eq!(five.used, parse_money("0.5").unwrap());
    }

    #[test]
    fn xkiro_missing_amounts_are_not_coerced_to_zero() {
        // The window has neither a cap nor a remaining amount, and the free
        // token block has neither a limit nor a remaining count: nothing here
        // can be turned into a trustworthy gate, so the probe must fail rather
        // than report a healthy empty reading.
        let body = r#"{
          "windows": [ { "window_sec": 18000, "spent_usd": "0.5" } ],
          "free_tokens": { "used_today": 3 }
        }"#;
        let err = outcome_from_body(body).expect_err("no usable reading must fail");
        assert!(err.message.contains("no usable"), "got {:?}", err.message);
    }

    #[test]
    fn xkiro_empty_response_is_an_error_not_an_empty_success() {
        assert!(outcome_from_body("{}").is_err());
        assert!(outcome_from_body(r#"{"wallet": null}"#).is_err());
    }

    #[test]
    fn xkiro_rejects_a_malformed_body() {
        let err = outcome_from_body("<html>502 Bad Gateway</html>")
            .expect_err("non-JSON must fail");
        assert!(err.message.contains("not valid JSON"), "got {:?}", err.message);

        let err = outcome_from_body("[1, 2, 3]").expect_err("non-object must fail");
        assert!(err.message.contains("not a JSON object"), "got {:?}", err.message);
    }

    #[tokio::test]
    async fn xkiro_probe_reads_the_pinned_path_with_the_bearer_key() {
        let server = TestServer::start(vec![Reply::json(USAGE_BODY)]).await;
        let client = strict_client();
        let account = test_account("xkiro", "sk-xkiro-test-key");

        let probe = XkiroProbe::new(server.base_url.clone());
        let outcome = probe.probe(&account, &client).await.expect("probe must succeed");

        assert_eq!(outcome.gates.len(), 4);
        assert_eq!(server.call_count(), 1);
        let request = server.requests()[0].to_ascii_lowercase();
        assert!(request.starts_with("get /v1/usage http/1.1"), "got {:?}", request);
        assert!(
            request.contains("authorization: bearer sk-xkiro-test-key"),
            "the bearer key must be sent: {request:?}"
        );
    }

    #[tokio::test]
    async fn xkiro_probe_fails_on_a_non_200_status() {
        let server = TestServer::start(vec![Reply::status(
            503,
            r#"{"error":{"code":"service_unavailable","message":"try later"}}"#,
        )])
        .await;
        let client = strict_client();
        let account = test_account("xkiro", "sk-xkiro-test-key");

        let probe = XkiroProbe::new(server.base_url.clone());
        let err = probe
            .probe(&account, &client)
            .await
            .expect_err("non-200 must fail");
        assert!(err.message.contains("503"), "got {:?}", err.message);
        assert!(
            err.message.contains("service_unavailable"),
            "the machine-readable code should be surfaced: {:?}",
            err.message
        );
    }

    #[tokio::test]
    async fn xkiro_probe_fails_on_a_malformed_body() {
        let server = TestServer::start(vec![Reply::json("not json at all")]).await;
        let client = strict_client();
        let account = test_account("xkiro", "sk-xkiro-test-key");

        let probe = XkiroProbe::new(server.base_url.clone());
        let err = probe
            .probe(&account, &client)
            .await
            .expect_err("malformed body must fail");
        assert!(err.message.contains("not valid JSON"), "got {:?}", err.message);
    }
}
