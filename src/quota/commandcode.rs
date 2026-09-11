//! Command Code quota adapter.
//!
//! FILE OWNER: quota-dev.
//!
//! Command Code exposes a small set of undocumented /alpha endpoints. The host
//! is pinned from configuration (probe_base_url) because the credential must
//! never follow a URL that arrived with a client request.
//!
//! Probe order:
//!   b. GET /alpha/whoami                  -> org.id, used as ?orgId= below
//!   a. GET /alpha/billing/credits         -> the ONLY required call
//!   c. GET /alpha/billing/subscriptions   -> current billing period
//!   d. GET /alpha/usage/summary?since=... -> spend inside that period
//!
//! whoami is probed before credits because the credits call is org-scoped: a
//! team/business account needs ?orgId= to get its own numbers. c is required
//! before d, and both feed the monthly gate only, so they degrade gracefully.
//!
//! Exhaustion semantics (the reason this adapter exists at all): when a window
//! is used up, Command Code does NOT return an error. It silently spends
//! purchasedCredits instead, so windowLimits.limited/.exceeded are the only
//! signal that the covered pool is gone. When they are set the window gates are
//! reported as spent even though the provider still reports used < cap. That is
//! deliberately conservative: under-reporting headroom costs throughput, while
//! over-reporting it spends real money without any error to notice.

use super::{
    get_body, get_json, json_money, json_scalar_string, json_timestamp, json_truthy, ProbeError,
    ProbeOutcome, QuotaProbe,
};
use crate::accounts::gate;
use crate::domain::{Account, Gate, GateKind};
use async_trait::async_trait;
use rust_decimal::Decimal;
use serde_json::{Map, Value};
use time::OffsetDateTime;

/// Paths on the pinned probe host.
pub(crate) const CREDITS_PATH: &str = "/alpha/billing/credits";
pub(crate) const WHOAMI_PATH: &str = "/alpha/whoami";
pub(crate) const SUBSCRIPTIONS_PATH: &str = "/alpha/billing/subscriptions";
pub(crate) const SUMMARY_PATH: &str = "/alpha/usage/summary";

/// Command Code probe adapter.
pub struct CommandCodeProbe {
    /// Pinned probe host, from configuration.
    pub base_url: String,
}

impl CommandCodeProbe {
    pub fn new(base_url: String) -> Self {
        CommandCodeProbe { base_url }
    }
}

/// The current billing period, as far as the subscriptions call revealed it.
#[derive(Debug, Clone)]
pub(crate) struct Period {
    /// Forwarded verbatim as the since= parameter of usage/summary.
    pub start: String,
    pub end: Option<OffsetDateTime>,
}

/// Map a recorded /alpha/billing/credits body to gates.
///
/// period_spend is the spend attributed to the current period (None when the
/// period could not be established). Without it there is no honest way to
/// compare spend against a monthly allowance, so the monthly gate is omitted.
pub(crate) fn outcome_from_credits_body(
    body: &str,
    period_spend: Option<Decimal>,
    period_end: Option<OffsetDateTime>,
) -> Result<ProbeOutcome, ProbeError> {
    let value: Value = serde_json::from_str(body).map_err(|_| {
        ProbeError::new("command-code /alpha/billing/credits: response is not valid JSON")
    })?;
    let object = value.as_object().ok_or_else(|| {
        ProbeError::new("command-code /alpha/billing/credits: response is not a JSON object")
    })?;

    let credits = object.get("credits").and_then(Value::as_object);
    let limits = object.get("windowLimits").and_then(Value::as_object);
    let mut gates = Vec::new();

    // A top-level flag means the account is currently enforced, and the
    // provider does not say which window is binding. Treat every window as
    // spent: this is the only reading that reliably keeps the router off an
    // account that would silently bill purchased credits.
    let account_enforced = limits
        .map(|l| json_truthy(l.get("limited").unwrap_or(&Value::Null))
            || json_truthy(l.get("exceeded").unwrap_or(&Value::Null)))
        .unwrap_or(false);

    if let Some(limits) = limits {
        for (field, kind) in [
            ("fiveHour", GateKind::FiveHour),
            ("weekly", GateKind::Weekly),
        ] {
            let Some(window) = limits.get(field).and_then(Value::as_object) else {
                continue;
            };
            let window_enforced = account_enforced
                || json_truthy(window.get("limited").unwrap_or(&Value::Null))
                || json_truthy(window.get("exceeded").unwrap_or(&Value::Null));
            if let Some(g) = window_gate(kind, window, window_enforced) {
                gates.push(g);
            }
        }
    }

    // The monthly allowance is Covered and can only be judged against spend
    // scoped to the CURRENT period: the unscoped summary is lifetime spend, and
    // comparing it with this cycle's remaining credits produces a nonsense
    // percentage. No period -> no monthly gate (the windows are still real).
    if let (Some(credits), Some(spend)) = (credits, period_spend) {
        if let Some(monthly) = credits.get("monthlyCredits").and_then(json_money) {
            gates.push(gate(GateKind::Monthly, monthly, spend, period_end));
        }
    }

    // Purchased credits never expire and are real money (Metered). They are a
    // bearer balance: the cap is what is left, nothing has been drawn from it
    // in this reading.
    if let Some(purchased) = credits
        .and_then(|c| c.get("purchasedCredits"))
        .and_then(json_money)
    {
        gates.push(gate(
            GateKind::PurchasedCredits,
            purchased,
            Decimal::ZERO,
            None,
        ));
    }

    // credits.freeCredits is deliberately NOT mapped. There is no usage figure
    // for it, so a gate would be a permanently undrawn covered gate that only
    // dilutes peak_covered_utilization and hides real exhaustion. It is not a
    // daily token allowance either, so FreeTokens would be the wrong unit.

    if gates.is_empty() {
        return Err(ProbeError::new(
            "command-code /alpha/billing/credits: response contained no usable quota reading",
        ));
    }

    Ok(ProbeOutcome {
        gates,
        plan: None,
        // Command Code credits are USD by contract and the payload carries no
        // currency field.
        currency: None,
    })
}

fn window_gate(kind: GateKind, window: &Map<String, Value>, enforced: bool) -> Option<Gate> {
    let cap = window.get("cap").and_then(json_money)?;
    let used = window.get("used").and_then(json_money)?;
    let resets_at = window.get("resetAt").and_then(json_timestamp);

    // An enforced window reads as spent. max() rather than assignment so a
    // provider that already over-reports usage is not quietly clamped back to
    // the cap.
    let used = if enforced { used.max(cap) } else { used };

    Some(gate(kind, cap, used, resets_at))
}

/// org.id from a whoami payload, tolerating the surrounding envelope.
fn whoami_org_id(value: &Value) -> Option<String> {
    let object = value.as_object()?;
    for container in ["org", "organization"] {
        if let Some(id) = object
            .get(container)
            .and_then(Value::as_object)
            .and_then(|o| o.get("id"))
            .and_then(json_scalar_string)
        {
            return Some(id);
        }
    }
    object.get("orgId").and_then(json_scalar_string)
}

/// currentPeriodStart / currentPeriodEnd, under data or at the top level.
fn current_period(value: &Value) -> Option<Period> {
    let object = value.as_object()?;
    let data = object
        .get("data")
        .and_then(Value::as_object)
        .unwrap_or(object);

    let start = data.get("currentPeriodStart").and_then(json_scalar_string)?;
    let end = data.get("currentPeriodEnd").and_then(json_timestamp);
    Some(Period { start, end })
}

/// totalCost from a usage summary, under the top level or under data.
fn total_cost(value: &Value) -> Option<Decimal> {
    let object = value.as_object()?;
    for key in ["totalCost", "total_cost"] {
        if let Some(cost) = object.get(key).and_then(json_money) {
            return Some(cost);
        }
    }
    let data = object.get("data").and_then(Value::as_object)?;
    for key in ["totalCost", "total_cost"] {
        if let Some(cost) = data.get(key).and_then(json_money) {
            return Some(cost);
        }
    }
    None
}

/// Append a query parameter with minimal, predictable percent-encoding.
///
/// Built by hand rather than through a form encoder: the period start is an
/// opaque token that must reach the provider byte-for-byte, and form encoding
/// turns spaces into "+" and colons into escapes that providers then have to
/// guess about.
fn append_query(url: &str, key: &str, value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b':'
            | b'/' => encoded.push(byte as char),
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}{key}={encoded}")
}

#[async_trait]
impl QuotaProbe for CommandCodeProbe {
    fn name(&self) -> &'static str {
        "command-code"
    }

    async fn probe(
        &self,
        account: &Account,
        client: &reqwest::Client,
    ) -> Result<ProbeOutcome, ProbeError> {
        let key = &account.api_key;
        let base = self.base_url.trim_end_matches('/');

        // whoami comes first because the credits call below is org-scoped. It
        // is optional: a personal account has no org.
        let whoami_url = format!("{base}{WHOAMI_PATH}");
        let org_id = match get_json(client, &whoami_url, key, "command-code /alpha/whoami").await {
            Ok(value) => whoami_org_id(&value),
            Err(e) => {
                tracing::debug!(error = %e, "command-code whoami unavailable; probing without an org id");
                None
            }
        };

        let credits_url = match org_id {
            Some(org) => append_query(&format!("{base}{CREDITS_PATH}"), "orgId", &org),
            None => format!("{base}{CREDITS_PATH}"),
        };
        let credits_body = get_body(
            client,
            &credits_url,
            key,
            "command-code /alpha/billing/credits",
        )
        .await?;

        let subscriptions_url = format!("{base}{SUBSCRIPTIONS_PATH}");
        let period = match get_json(
            client,
            &subscriptions_url,
            key,
            "command-code /alpha/billing/subscriptions",
        )
        .await
        {
            Ok(value) => current_period(&value),
            Err(e) => {
                tracing::debug!(error = %e, "command-code subscriptions unavailable");
                None
            }
        };

        // Only a period-scoped summary is meaningful; never fall back to the
        // unscoped (lifetime) spend.
        let period_spend = match &period {
            Some(period) => {
                let url = append_query(&format!("{base}{SUMMARY_PATH}"), "since", &period.start);
                match get_json(
                    client,
                    &url,
                    key,
                    "command-code /alpha/usage/summary",
                )
                .await
                {
                    Ok(value) => total_cost(&value),
                    Err(e) => {
                        tracing::debug!(error = %e, "command-code usage summary unavailable");
                        None
                    }
                }
            }
            None => None,
        };

        outcome_from_credits_body(
            &credits_body,
            period_spend,
            period.as_ref().and_then(|p| p.end),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::parse_money;
    use crate::quota::testutil::{strict_client, test_account, Reply, TestServer};

    /// Recorded /alpha/billing/credits response for a pro account mid-cycle.
    const CREDITS_BODY: &str = r#"{
      "credits": {
        "monthlyCredits": 100,
        "purchasedCredits": 12.5,
        "freeCredits": 5
      },
      "windowLimits": {
        "fiveHour": {
          "cap": 10,
          "used": 2.5,
          "resetAt": "2025-06-01T10:00:00Z"
        },
        "weekly": {
          "cap": 100,
          "used": 20,
          "resetAt": "2025-06-05T00:00:00Z"
        },
        "exceeded": false,
        "limited": false
      }
    }"#;

    fn gate_of(outcome: &ProbeOutcome, kind: GateKind) -> Option<&Gate> {
        outcome.gates.iter().find(|g| g.kind == kind)
    }

    #[test]
    fn commandcode_maps_credits_windows_and_purchased_credits() {
        let outcome = outcome_from_credits_body(CREDITS_BODY, None, None)
            .expect("recorded payload must parse");

        let five = gate_of(&outcome, GateKind::FiveHour).expect("five hour window");
        assert_eq!(five.cap, parse_money("10").unwrap());
        assert_eq!(five.used, parse_money("2.5").unwrap());
        assert!(five.resets_at.is_some(), "resetAt must become resets_at");

        let weekly = gate_of(&outcome, GateKind::Weekly).expect("weekly window");
        assert_eq!(weekly.cap, parse_money("100").unwrap());
        assert_eq!(weekly.used, parse_money("20").unwrap());

        let purchased =
            gate_of(&outcome, GateKind::PurchasedCredits).expect("purchased credits");
        assert_eq!(purchased.cap, parse_money("12.5").unwrap());
        assert_eq!(purchased.used, Decimal::ZERO);
        assert_eq!(purchased.class(), crate::domain::GateClass::Metered);

        // No period was supplied, so the monthly allowance cannot be judged and
        // must be omitted rather than reported as untouched.
        assert!(gate_of(&outcome, GateKind::Monthly).is_none());
        assert_eq!(outcome.gates.len(), 3);
    }

    #[test]
    fn commandcode_monthly_gate_is_scoped_to_the_current_period() {
        let end = parse_timestamp("2025-06-30T00:00:00Z");
        let outcome =
            outcome_from_credits_body(CREDITS_BODY, Some(parse_money("31.75").unwrap()), end)
                .expect("recorded payload must parse");

        let monthly = gate_of(&outcome, GateKind::Monthly).expect("monthly allowance");
        assert_eq!(monthly.cap, parse_money("100").unwrap());
        assert_eq!(monthly.used, parse_money("31.75").unwrap());
        assert_eq!(monthly.resets_at, end);
        assert_eq!(monthly.class(), crate::domain::GateClass::Covered);
    }

    /// Command Code does not error on exhaustion: the only warning is the
    /// window flag, so a flagged window must read as spent.
    #[test]
    fn commandcode_window_flags_read_a_window_as_spent() {
        let per_window = r#"{
          "windowLimits": {
            "fiveHour": { "cap": 10, "used": 2, "resetAt": "2025-06-01T10:00:00Z", "exceeded": true },
            "weekly": { "cap": 100, "used": 20 }
          }
        }"#;
        let outcome = outcome_from_credits_body(per_window, None, None).expect("must parse");
        assert_eq!(
            gate_of(&outcome, GateKind::FiveHour).unwrap().used,
            parse_money("10").unwrap(),
            "an enforced window reads as spent even though used < cap"
        );
        assert_eq!(
            gate_of(&outcome, GateKind::Weekly).unwrap().used,
            parse_money("20").unwrap(),
            "an unflagged window keeps its reported usage"
        );

        // A top-level flag marks every window: the provider does not say which
        // one is binding, and under-reporting headroom spends real money.
        let account_wide = r#"{
          "windowLimits": {
            "fiveHour": { "cap": 10, "used": 2 },
            "weekly": { "cap": 100, "used": 20 },
            "limited": 1
          }
        }"#;
        let outcome = outcome_from_credits_body(account_wide, None, None).expect("must parse");
        assert_eq!(
            gate_of(&outcome, GateKind::FiveHour).unwrap().used,
            parse_money("10").unwrap()
        );
        assert_eq!(
            gate_of(&outcome, GateKind::Weekly).unwrap().used,
            parse_money("100").unwrap()
        );
        assert_eq!(
            gate_of(&outcome, GateKind::Weekly).unwrap().remaining(),
            Decimal::ZERO
        );
    }

    #[test]
    fn commandcode_tolerates_missing_optional_fields() {
        // No windowLimits, no freeCredits, no plan: the two credit pools are
        // still readable, and the monthly gate appears once a period is known.
        let body = r#"{ "credits": { "monthlyCredits": 25, "purchasedCredits": 3.5 } }"#;
        let outcome = outcome_from_credits_body(body, Some(parse_money("10").unwrap()), None)
            .expect("must parse");

        assert!(gate_of(&outcome, GateKind::FiveHour).is_none());
        assert!(gate_of(&outcome, GateKind::Weekly).is_none());
        assert_eq!(
            gate_of(&outcome, GateKind::Monthly).unwrap().cap,
            parse_money("25").unwrap()
        );
        assert_eq!(
            gate_of(&outcome, GateKind::PurchasedCredits).unwrap().cap,
            parse_money("3.5").unwrap()
        );
    }

    #[test]
    fn commandcode_a_present_but_empty_credits_block_is_not_a_zero_reading() {
        let body = r#"{ "credits": { "monthlyCredits": null }, "windowLimits": {} }"#;
        let err = outcome_from_credits_body(body, Some(parse_money("10").unwrap()), None)
            .expect_err("nothing usable must fail");
        assert!(err.message.contains("no usable"), "got {:?}", err.message);
    }

    #[test]
    fn commandcode_rejects_a_malformed_body() {
        let err = outcome_from_credits_body("<html>nope</html>", None, None)
            .expect_err("non-JSON must fail");
        assert!(err.message.contains("not valid JSON"), "got {:?}", err.message);

        let err = outcome_from_credits_body("[]", None, None)
            .expect_err("non-object must fail");
        assert!(err.message.contains("not a JSON object"), "got {:?}", err.message);
    }

    #[test]
    fn commandcode_reads_org_and_period_from_their_envelopes() {
        let whoami = serde_json::json!({ "org": { "id": "org_123", "name": "Acme" } });
        assert_eq!(whoami_org_id(&whoami).as_deref(), Some("org_123"));

        let flat = serde_json::json!({ "orgId": "org_456" });
        assert_eq!(whoami_org_id(&flat).as_deref(), Some("org_456"));
        assert_eq!(whoami_org_id(&serde_json::json!({"user": {}})), None);

        let subs = serde_json::json!({
            "data": {
                "currentPeriodStart": "2025-06-01T00:00:00Z",
                "currentPeriodEnd": "2025-07-01T00:00:00Z"
            }
        });
        let period = current_period(&subs).expect("period");
        assert_eq!(period.start, "2025-06-01T00:00:00Z");
        assert_eq!(period.end, parse_timestamp("2025-07-01T00:00:00Z"));

        // Missing period, and a period with no end, both stay absent.
        assert!(current_period(&serde_json::json!({"data": {}})).is_none());
        let open = serde_json::json!({ "currentPeriodStart": 1735689600000i64 });
        let period = current_period(&open).expect("period");
        assert_eq!(period.start, "1735689600000");
        assert_eq!(period.end, None);
    }

    #[test]
    fn commandcode_reads_total_cost_from_either_shape() {
        assert_eq!(
            total_cost(&serde_json::json!({ "totalCost": "12.50" })),
            Some(parse_money("12.50").unwrap())
        );
        assert_eq!(
            total_cost(&serde_json::json!({ "data": { "totalCost": 12.5 } })),
            Some(parse_money("12.5").unwrap())
        );
        assert_eq!(total_cost(&serde_json::json!({ "rows": [] })), None);
    }

    #[test]
    fn commandcode_query_params_are_appended_and_encoded() {
        assert_eq!(
            append_query("https://api.commandcode.ai/alpha/usage/summary", "since", "2025-06-01T00:00:00Z"),
            "https://api.commandcode.ai/alpha/usage/summary?since=2025-06-01T00:00:00Z"
        );
        assert_eq!(
            append_query("https://x/y?a=1", "orgId", "org 1&2"),
            "https://x/y?a=1&orgId=org%201%262"
        );
    }

    fn parse_timestamp(raw: &str) -> Option<OffsetDateTime> {
        json_timestamp(&Value::String(raw.to_string()))
    }

    /// Full probe: whoami gives the org id, credits is org-scoped, and the
    /// monthly gate uses spend since the period start only.
    #[tokio::test]
    async fn commandcode_probe_uses_the_org_id_and_scopes_spend_to_the_period() {
        let credits = r#"{
          "credits": { "monthlyCredits": 100, "purchasedCredits": 12.5 },
          "windowLimits": {
            "fiveHour": { "cap": 10, "used": 2.5, "resetAt": "2025-06-01T10:00:00Z" },
            "weekly": { "cap": 100, "used": 20, "resetAt": "2025-06-05T00:00:00Z" }
          }
        }"#;
        let server = TestServer::start(vec![
            Reply::json(r#"{"org":{"id":"org_123"},"user":{"email":"a@b.c"}}"#),
            Reply::json(credits),
            Reply::json(
                r#"{"data":{"currentPeriodStart":"2025-06-01T00:00:00Z","currentPeriodEnd":"2025-07-01T00:00:00Z"}}"#,
            ),
            Reply::json(r#"{"totalCost":"31.75","requests":128}"#),
        ])
        .await;

        let client = strict_client();
        let account = test_account("commandcode", "sk-cc-test-key");
        let probe = CommandCodeProbe::new(server.base_url.clone());
        let outcome = probe.probe(&account, &client).await.expect("probe must succeed");

        let requests: Vec<String> = server
            .requests()
            .iter()
            .map(|r| r.to_ascii_lowercase())
            .collect();
        assert_eq!(requests.len(), 4, "whoami, credits, subscriptions, summary");
        assert!(requests[0].starts_with("get /alpha/whoami "), "got {:?}", requests[0]);
        assert!(
            requests[1].starts_with("get /alpha/billing/credits?orgid=org_123 "),
            "credits must carry the org id: {:?}",
            requests[1]
        );
        assert!(requests[2].starts_with("get /alpha/billing/subscriptions "));
        assert!(
            requests[3].starts_with("get /alpha/usage/summary?since=2025-06-01t00:00:00z "),
            "the summary must be scoped to the period: {:?}",
            requests[3]
        );
        for request in &requests {
            assert!(
                request.contains("authorization: bearer sk-cc-test-key"),
                "every call is authenticated: {request:?}"
            );
        }

        let monthly = gate_of(&outcome, GateKind::Monthly).expect("monthly gate");
        assert_eq!(monthly.cap, parse_money("100").unwrap());
        assert_eq!(monthly.used, parse_money("31.75").unwrap());
        assert!(monthly.resets_at.is_some());
        assert_eq!(gate_of(&outcome, GateKind::FiveHour).unwrap().used, parse_money("2.5").unwrap());
        assert_eq!(gate_of(&outcome, GateKind::Weekly).unwrap().used, parse_money("20").unwrap());
        assert_eq!(
            gate_of(&outcome, GateKind::PurchasedCredits).unwrap().cap,
            parse_money("12.5").unwrap()
        );
    }

    /// whoami and the two period endpoints are advisory: losing them must not
    /// lose the window readings.
    #[tokio::test]
    async fn commandcode_probe_survives_failing_optional_endpoints() {
        let credits = r#"{
          "credits": { "monthlyCredits": 100 },
          "windowLimits": { "fiveHour": { "cap": 10, "used": 2.5 } }
        }"#;
        let server = TestServer::start(vec![
            Reply::status(500, r#"{"error":"boom"}"#),
            Reply::json(credits),
            Reply::status(503, r#"{"error":"boom"}"#),
        ])
        .await;

        let client = strict_client();
        let account = test_account("commandcode", "sk-cc-test-key");
        let probe = CommandCodeProbe::new(server.base_url.clone());
        let outcome = probe.probe(&account, &client).await.expect("windows must survive");

        assert!(gate_of(&outcome, GateKind::FiveHour).is_some());
        assert!(
            gate_of(&outcome, GateKind::Monthly).is_none(),
            "no period means no monthly gate, never a zero-spend claim"
        );
        let requests: Vec<String> = server
            .requests()
            .iter()
            .map(|r| r.to_ascii_lowercase())
            .collect();
        assert_eq!(requests.len(), 3, "whoami, credits, subscriptions");
        assert!(
            requests[1].starts_with("get /alpha/billing/credits "),
            "without an org id the credits call carries no query: {:?}",
            requests[1]
        );
    }

    #[tokio::test]
    async fn commandcode_probe_fails_when_the_credits_endpoint_fails() {
        let server = TestServer::start(vec![
            Reply::status(401, r#"{"error":{"code":"invalid_api_key"}}"#),
            Reply::status(500, r#"{"error":"boom"}"#),
        ])
        .await;

        let client = strict_client();
        let account = test_account("commandcode", "sk-cc-test-key");
        let probe = CommandCodeProbe::new(server.base_url.clone());
        let err = probe
            .probe(&account, &client)
            .await
            .expect_err("the required endpoint must fail the probe");
        assert!(err.message.contains("500"), "got {:?}", err.message);
        assert!(!err.message.contains("sk-cc-test-key"));
    }

    #[tokio::test]
    async fn commandcode_probe_fails_on_a_malformed_credits_body() {
        let server = TestServer::start(vec![
            Reply::json(r#"{"org":{"id":"org_1"}}"#),
            Reply::json("<html>nope</html>"),
        ])
        .await;

        let client = strict_client();
        let account = test_account("commandcode", "sk-cc-test-key");
        let probe = CommandCodeProbe::new(server.base_url.clone());
        let err = probe
            .probe(&account, &client)
            .await
            .expect_err("malformed credits body must fail");
        assert!(err.message.contains("not valid JSON"), "got {:?}", err.message);
    }
}
