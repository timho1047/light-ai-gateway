//! HaoAI quota adapter.
//!
//! Endpoint: GET {pinned probe host}/v1/user/balance with Authorization: Bearer.
//!
//! Recorded payload shape (hao.ai/docs/zh/api/openapi/balance):
//! {
//!   "is_active": true,
//!   "balance": 42.1357,
//!   "total": 100.0000,
//!   "used": 57.8643,
//!   "currency": "USD"
//! }
//!
//! HaoAI is pay-as-you-go against a prepaid wallet: no plan windows, so there is
//! no covered allowance and no 80% concept. Exactly one Wallet gate is emitted,
//! and the account is admitted only when spending real money is allowed for it
//! ([providers.<name>] allow_metered, or the global policy).
//!
//! The provider's "used" and "total" are LIFETIME figures and deliberately do
//! not reach the gate. A gate's "used" is the local ledger's debit since the last
//! reading, and every probe replaces the reading wholesale; charging lifetime
//! spend into that field would make utilization climb forever against a wallet
//! that refills, and the soft threshold would then retire an account that still
//! has money in it.

use super::{ProbeError, ProbeOutcome, QuotaProbe, get_body, json_money, sat_sub};
use crate::accounts::gate;
use crate::domain::{Account, GateKind};
use async_trait::async_trait;
use rust_decimal::Decimal;
use serde_json::Value;

/// Path on the pinned probe host.
pub(crate) const BALANCE_PATH: &str = "/v1/user/balance";

/// HaoAI probe adapter.
pub struct HaoaiProbe {
    /// Pinned probe host, from configuration.
    pub base_url: String,
}

impl HaoaiProbe {
    pub fn new(base_url: String) -> Self {
        HaoaiProbe { base_url }
    }
}

/// Map a recorded /v1/user/balance body to one Wallet gate.
pub(crate) fn outcome_from_body(body: &str) -> Result<ProbeOutcome, ProbeError> {
    let value: Value = serde_json::from_str(body)
        .map_err(|_| ProbeError::new("haoai /v1/user/balance: response is not valid JSON"))?;
    let object = value
        .as_object()
        .ok_or_else(|| ProbeError::new("haoai /v1/user/balance: response is not a JSON object"))?;

    let currency = object
        .get("currency")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_string);

    // A key the provider itself calls inactive cannot pay, whatever number sits
    // beside it; reporting its balance anyway would keep an unusable account in
    // rotation until the first request failed upstream. This is not a probe
    // failure: an Err would carry the previous, possibly healthy reading
    // forward, which is the opposite of what was just learned.
    if object.get("is_active").and_then(Value::as_bool) == Some(false) {
        return Ok(ProbeOutcome {
            gates: vec![gate(GateKind::Wallet, Decimal::ZERO, Decimal::ZERO, None)],
            plan: None,
            currency,
        });
    }

    // An absent balance must not become a zero-balance gate: the caller carries
    // the previous reading forward and marks it stale, while a zero would claim
    // the account is empty.
    let balance = object
        .get("balance")
        .and_then(json_money)
        // The documented identity is balance = total - used, so deriving it is
        // exact arithmetic on the provider's own figures rather than a guess.
        .or_else(|| {
            let total = object.get("total").and_then(json_money)?;
            let used = object.get("used").and_then(json_money)?;
            Some(sat_sub(total, used))
        })
        .ok_or_else(|| {
            ProbeError::new("haoai /v1/user/balance: response contained no usable balance")
        })?;

    Ok(ProbeOutcome {
        gates: vec![gate(GateKind::Wallet, balance, Decimal::ZERO, None)],
        plan: None,
        currency,
    })
}

#[async_trait]
impl QuotaProbe for HaoaiProbe {
    fn name(&self) -> &'static str {
        "haoai"
    }

    async fn probe(
        &self,
        account: &Account,
        client: &reqwest::Client,
    ) -> Result<ProbeOutcome, ProbeError> {
        let url = format!("{}{}", self.base_url.trim_end_matches('/'), BALANCE_PATH);
        let body = get_body(client, &url, &account.api_key, "haoai /v1/user/balance").await?;
        outcome_from_body(&body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{GateClass, parse_money};
    use crate::quota::testutil::{Reply, TestServer, strict_client, test_account};

    /// Recorded /v1/user/balance response, as documented.
    const BALANCE_BODY: &str = r#"{
      "is_active": true,
      "balance": 42.1357,
      "total": 100.0000,
      "used": 57.8643,
      "currency": "USD"
    }"#;

    #[test]
    fn haoai_maps_the_documented_payload_to_one_wallet_gate() {
        let outcome = outcome_from_body(BALANCE_BODY).expect("recorded payload must parse");
        assert_eq!(outcome.currency.as_deref(), Some("USD"));
        assert_eq!(outcome.gates.len(), 1);

        let wallet = &outcome.gates[0];
        assert_eq!(wallet.kind, GateKind::Wallet);
        assert_eq!(wallet.cap, parse_money("42.1357").unwrap());
        assert_eq!(wallet.used, Decimal::ZERO);
        assert_eq!(wallet.class(), GateClass::Metered);
        assert!(wallet.resets_at.is_none(), "a wallet does not reset");
    }

    /// Lifetime figures must not leak into the gate: total is what was ever
    /// topped up, not what is left, and billing it as a cap would let the hard
    /// gate spend money the wallet does not have.
    #[test]
    fn haoai_bills_the_balance_and_ignores_the_lifetime_totals() {
        let body = r#"{ "is_active": true, "balance": 5.0, "total": 100.0, "used": 95.0,
                       "currency": "USD" }"#;
        let outcome = outcome_from_body(body).expect("payload must parse");
        assert_eq!(outcome.gates[0].cap, parse_money("5.0").unwrap());
        assert_eq!(outcome.gates[0].used, Decimal::ZERO);
    }

    #[test]
    fn haoai_derives_a_missing_balance_from_its_lifetime_figures() {
        let body = r#"{ "total": 100.0, "used": 57.5, "currency": "USD" }"#;
        let outcome = outcome_from_body(body).expect("payload must parse");
        assert_eq!(outcome.gates[0].cap, parse_money("42.5").unwrap());
    }

    /// A key the provider calls inactive is out of money as far as this gateway
    /// is concerned, and a zero gate says exactly that. Failing the probe
    /// instead would leave the last healthy reading in place and keep selecting
    /// an account that cannot pay.
    #[test]
    fn haoai_an_inactive_key_reports_no_headroom_rather_than_a_failure() {
        let body = r#"{ "is_active": false, "balance": 42.1357, "currency": "USD" }"#;
        let outcome = outcome_from_body(body).expect("an inactive key is a reading, not an error");
        assert_eq!(outcome.gates[0].cap, Decimal::ZERO);
        assert_eq!(outcome.gates[0].remaining(), Decimal::ZERO);
    }

    #[test]
    fn haoai_a_missing_balance_is_not_coerced_to_zero() {
        let body = r#"{ "is_active": true, "currency": "USD" }"#;
        let err = outcome_from_body(body).expect_err("must not invent a balance");
        assert!(
            err.message.contains("no usable balance"),
            "got {:?}",
            err.message
        );

        let err = outcome_from_body("{}").expect_err("empty object");
        assert!(
            err.message.contains("no usable balance"),
            "got {:?}",
            err.message
        );
    }

    #[test]
    fn haoai_accepts_a_string_balance() {
        let body = r#"{ "balance": "12.5000", "currency": "USD" }"#;
        let outcome = outcome_from_body(body).expect("payload must parse");
        assert_eq!(outcome.gates[0].cap, parse_money("12.5000").unwrap());
    }

    /// total and used are provider-supplied and their difference can overflow a
    /// Decimal (total 7e28 against used -7e28): rust_decimal's subtraction panics
    /// there, which would kill the probing task instead of reporting one account.
    #[test]
    fn haoai_extreme_lifetime_figures_saturate_instead_of_panicking() {
        let body = r#"{ "total": "7e28", "used": "-7e28", "currency": "USD" }"#;
        let outcome = outcome_from_body(body).expect("must parse");
        assert_eq!(outcome.gates[0].cap, Decimal::MAX);
    }

    #[test]
    fn haoai_rejects_a_malformed_body() {
        let err = outcome_from_body("<html>502</html>").expect_err("non-JSON must fail");
        assert!(
            err.message.contains("not valid JSON"),
            "got {:?}",
            err.message
        );

        let err = outcome_from_body(r#"[{"balance": 1.0}]"#).expect_err("array");
        assert!(
            err.message.contains("not a JSON object"),
            "got {:?}",
            err.message
        );
    }

    #[tokio::test]
    async fn haoai_probe_reads_the_pinned_balance_endpoint() {
        let server = TestServer::start(vec![Reply::json(BALANCE_BODY)]).await;
        let client = strict_client();
        let account = test_account("haoai", "sk-hao-test-key");

        let probe = HaoaiProbe::new(server.base_url.clone());
        let outcome = probe
            .probe(&account, &client)
            .await
            .expect("probe must succeed");

        assert_eq!(outcome.gates.len(), 1);
        assert_eq!(outcome.currency.as_deref(), Some("USD"));
        assert_eq!(server.call_count(), 1);
        let request = server.requests()[0].to_ascii_lowercase();
        assert!(
            request.starts_with("get /v1/user/balance http/1.1"),
            "got {request:?}"
        );
        assert!(
            request.contains("authorization: bearer sk-hao-test-key"),
            "the bearer key must be sent: {request:?}"
        );
    }

    #[tokio::test]
    async fn haoai_probe_fails_on_a_non_200_status_without_leaking_the_key() {
        let server = TestServer::start(vec![Reply::status(
            401,
            r#"{"error":"unauthenticated","is_active":false}"#,
        )])
        .await;
        let client = strict_client();
        let account = test_account("haoai", "sk-hao-test-key");

        let probe = HaoaiProbe::new(server.base_url.clone());
        let err = probe
            .probe(&account, &client)
            .await
            .expect_err("non-200 must fail");
        assert!(err.message.contains("401"), "got {:?}", err.message);
        assert!(
            err.message.contains("/v1/user/balance"),
            "the failing endpoint belongs in the message: {:?}",
            err.message
        );
        assert!(!err.message.contains("sk-hao-test-key"));
    }
}
