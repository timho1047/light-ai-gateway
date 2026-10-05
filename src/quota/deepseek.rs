//! DeepSeek quota adapter.
//!
//! Endpoint: GET {pinned probe host}/user/balance with Authorization: Bearer.
//!
//! Recorded payload shape (api-docs.deepseek.com/api/get-user-balance):
//! {
//!   "is_available": true,
//!   "balance_infos": [
//!     { "currency": "USD", "total_balance": "110.00",
//!       "granted_balance": "10.00", "topped_up_balance": "100.00" }
//!   ]
//! }
//!
//! DeepSeek is a pure wallet provider: every balance is Metered (real money),
//! there is no covered window and therefore no 80% concept. One gate is emitted
//! per currency entry and the amounts are never summed across currencies -- CNY
//! and USD are not the same number.

use super::{ProbeError, ProbeOutcome, QuotaProbe, get_body, json_money, sat_add};
use crate::accounts::gate;
use crate::domain::{Account, GateKind};
use async_trait::async_trait;
use rust_decimal::Decimal;
use serde_json::{Map, Value};

/// Path on the pinned probe host.
pub(crate) const BALANCE_PATH: &str = "/user/balance";

/// DeepSeek probe adapter.
pub struct DeepseekProbe {
    /// Pinned probe host, from configuration.
    pub base_url: String,
}

impl DeepseekProbe {
    pub fn new(base_url: String) -> Self {
        DeepseekProbe { base_url }
    }
}

/// Map a recorded /user/balance body to one Wallet gate per currency.
pub(crate) fn outcome_from_body(body: &str) -> Result<ProbeOutcome, ProbeError> {
    let value: Value = serde_json::from_str(body)
        .map_err(|_| ProbeError::new("deepseek /user/balance: response is not valid JSON"))?;
    let object = value
        .as_object()
        .ok_or_else(|| ProbeError::new("deepseek /user/balance: response is not a JSON object"))?;

    let infos = object
        .get("balance_infos")
        .and_then(Value::as_array)
        .ok_or_else(|| ProbeError::new("deepseek /user/balance: response has no balance_infos"))?;

    let mut gates = Vec::new();
    let mut currency = None;

    for info in infos {
        let Some(info) = info.as_object() else {
            continue;
        };
        let Some(entry_currency) = info
            .get("currency")
            .and_then(|c| c.as_str())
            .map(str::trim)
            .filter(|c| !c.is_empty())
        else {
            continue;
        };
        let Some(total) = balance(info) else {
            continue;
        };

        // Reported currency: the first entry that produced a gate. Entries are
        // never combined, so naming one of them is honest as long as the gates
        // stay separate.
        if currency.is_none() {
            currency = Some(entry_currency.to_string());
        }
        gates.push(gate(GateKind::Wallet, total, Decimal::ZERO, None));
    }

    if gates.is_empty() {
        // An absent or unusable balance must not become a zero-balance gate:
        // the caller carries the previous reading forward and marks it stale.
        return Err(ProbeError::new(
            "deepseek /user/balance: response contained no usable balance",
        ));
    }

    // is_available is deliberately not turned into an error: a failure would
    // make the caller carry a stale (possibly healthy) reading forward, which
    // is exactly wrong when the account has genuinely run out. An unavailable
    // account shows up as an exhausted balance gate instead.
    Ok(ProbeOutcome {
        gates,
        plan: None,
        currency,
    })
}

/// total_balance, or granted + topped_up when the total is missing.
fn balance(info: &Map<String, Value>) -> Option<Decimal> {
    if let Some(total) = info.get("total_balance").and_then(json_money) {
        return Some(total);
    }
    let granted = info.get("granted_balance").and_then(json_money);
    let topped_up = info.get("topped_up_balance").and_then(json_money);
    // Both parts are provider-supplied and the sum can overflow a Decimal
    // (two balances of "7e28"); rust_decimal's `+` panics there, which would
    // kill the probe task rather than report one account. Saturating keeps the
    // reading alive and errs towards "plenty of money left" only in a range no
    // real balance reaches.
    Some(sat_add(granted?, topped_up?))
}

#[async_trait]
impl QuotaProbe for DeepseekProbe {
    fn name(&self) -> &'static str {
        "deepseek"
    }

    async fn probe(
        &self,
        account: &Account,
        client: &reqwest::Client,
    ) -> Result<ProbeOutcome, ProbeError> {
        let url = format!("{}{}", self.base_url.trim_end_matches('/'), BALANCE_PATH);
        let body = get_body(client, &url, &account.api_key, "deepseek /user/balance").await?;
        outcome_from_body(&body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{GateClass, parse_money};
    use crate::quota::testutil::{Reply, TestServer, strict_client, test_account};

    /// Recorded /user/balance response.
    const BALANCE_BODY: &str = r#"{
      "is_available": true,
      "balance_infos": [
        {
          "currency": "USD",
          "total_balance": "110.00",
          "granted_balance": "10.00",
          "topped_up_balance": "100.00"
        }
      ]
    }"#;

    #[test]
    fn deepseek_maps_one_wallet_gate_per_currency() {
        let outcome = outcome_from_body(BALANCE_BODY).expect("recorded payload must parse");
        assert_eq!(outcome.currency.as_deref(), Some("USD"));
        assert_eq!(outcome.gates.len(), 1);

        let wallet = &outcome.gates[0];
        assert_eq!(wallet.kind, GateKind::Wallet);
        assert_eq!(wallet.cap, parse_money("110.00").unwrap());
        assert_eq!(wallet.used, Decimal::ZERO);
        assert_eq!(wallet.class(), GateClass::Metered);
        assert!(wallet.resets_at.is_none(), "a wallet does not reset");
    }

    #[test]
    fn deepseek_never_sums_across_currencies() {
        let body = r#"{
          "is_available": true,
          "balance_infos": [
            { "currency": "CNY", "total_balance": "700.00" },
            { "currency": "USD", "total_balance": "12.00" }
          ]
        }"#;
        let outcome = outcome_from_body(body).expect("payload must parse");

        assert_eq!(outcome.gates.len(), 2, "one gate per currency entry");
        assert_eq!(outcome.gates[0].cap, parse_money("700.00").unwrap());
        assert_eq!(outcome.gates[1].cap, parse_money("12.00").unwrap());
        assert_eq!(outcome.currency.as_deref(), Some("CNY"));
    }

    #[test]
    fn deepseek_tolerates_missing_optional_fields() {
        // granted/topped_up absent is fine when total_balance is present, and
        // an entry without a currency is skipped rather than guessed at.
        let body = r#"{
          "balance_infos": [
            { "currency": "USD", "total_balance": "8.5" },
            { "total_balance": "1.0" },
            { "currency": "USD" }
          ]
        }"#;
        let outcome = outcome_from_body(body).expect("payload must parse");
        assert_eq!(outcome.gates.len(), 1);
        assert_eq!(outcome.gates[0].cap, parse_money("8.5").unwrap());
    }

    #[test]
    fn deepseek_derives_a_missing_total_from_its_partition() {
        let body = r#"{
          "balance_infos": [
            { "currency": "USD", "granted_balance": "10.00", "topped_up_balance": "100.00" }
          ]
        }"#;
        let outcome = outcome_from_body(body).expect("payload must parse");
        assert_eq!(outcome.gates[0].cap, parse_money("110.00").unwrap());
    }

    /// granted_balance and topped_up_balance are provider-supplied, and their sum
    /// can overflow a Decimal (two balances of 7e28 do): rust_decimal's `+`
    /// panics there, which would kill the probing task instead of reporting one
    /// account. Saturation keeps the reading alive at the top of the range.
    #[test]
    fn deepseek_extreme_partitions_saturate_instead_of_panicking() {
        let body = r#"{ "balance_infos": [ { "currency": "USD",
                       "granted_balance": "7e28", "topped_up_balance": "7e28" } ] }"#;
        let outcome = outcome_from_body(body).expect("must parse");
        assert_eq!(outcome.gates[0].cap, Decimal::MAX);
    }

    #[test]
    fn deepseek_a_missing_balance_is_not_coerced_to_zero() {
        // The currency is known but no amount is: a zero gate would claim the
        // account is empty (and could not be told apart from a real zero), so
        // this must be a probe failure.
        let body = r#"{ "balance_infos": [ { "currency": "USD" } ] }"#;
        let err = outcome_from_body(body).expect_err("must not invent a balance");
        assert!(
            err.message.contains("no usable balance"),
            "got {:?}",
            err.message
        );

        let err = outcome_from_body(r#"{ "balance_infos": [] }"#).expect_err("empty list");
        assert!(
            err.message.contains("no usable balance"),
            "got {:?}",
            err.message
        );

        let err = outcome_from_body(r#"{ "is_available": true }"#).expect_err("missing key");
        assert!(
            err.message.contains("no balance_infos"),
            "got {:?}",
            err.message
        );
    }

    #[test]
    fn deepseek_accepts_numeric_balances() {
        let body = r#"{ "balance_infos": [ { "currency": "USD", "total_balance": 4.125 } ] }"#;
        let outcome = outcome_from_body(body).expect("payload must parse");
        assert_eq!(outcome.gates[0].cap, parse_money("4.125").unwrap());
    }

    #[test]
    fn deepseek_rejects_a_malformed_body() {
        let err = outcome_from_body("<html>502</html>").expect_err("non-JSON must fail");
        assert!(
            err.message.contains("not valid JSON"),
            "got {:?}",
            err.message
        );

        let err = outcome_from_body("{}").expect_err("non-object must fail");
        assert!(
            err.message.contains("no balance_infos"),
            "got {:?}",
            err.message
        );

        let err = outcome_from_body(r#"[{"currency":"USD"}]"#).expect_err("array");
        assert!(
            err.message.contains("not a JSON object"),
            "got {:?}",
            err.message
        );
    }

    #[tokio::test]
    async fn deepseek_probe_reads_the_pinned_balance_endpoint() {
        let server = TestServer::start(vec![Reply::json(BALANCE_BODY)]).await;
        let client = strict_client();
        let account = test_account("deepseek", "sk-ds-test-key");

        let probe = DeepseekProbe::new(server.base_url.clone());
        let outcome = probe
            .probe(&account, &client)
            .await
            .expect("probe must succeed");

        assert_eq!(outcome.gates.len(), 1);
        assert_eq!(outcome.currency.as_deref(), Some("USD"));
        assert_eq!(server.call_count(), 1);
        let request = server.requests()[0].to_ascii_lowercase();
        assert!(
            request.starts_with("get /user/balance http/1.1"),
            "got {:?}",
            request
        );
        assert!(
            request.contains("authorization: bearer sk-ds-test-key"),
            "the bearer key must be sent: {request:?}"
        );
    }

    #[tokio::test]
    async fn deepseek_probe_fails_on_a_non_200_status() {
        let server = TestServer::start(vec![Reply::status(
            402,
            r#"{"error":{"code":"insufficient_balance","message":"no money"}}"#,
        )])
        .await;
        let client = strict_client();
        let account = test_account("deepseek", "sk-ds-test-key");

        let probe = DeepseekProbe::new(server.base_url.clone());
        let err = probe
            .probe(&account, &client)
            .await
            .expect_err("non-200 must fail");
        assert!(err.message.contains("402"), "got {:?}", err.message);
        assert!(
            err.message.contains("insufficient_balance"),
            "the machine-readable code should be surfaced: {:?}",
            err.message
        );
        assert!(!err.message.contains("sk-ds-test-key"));
    }

    #[tokio::test]
    async fn deepseek_probe_fails_on_a_malformed_body() {
        let server = TestServer::start(vec![Reply::json("not json")]).await;
        let client = strict_client();
        let account = test_account("deepseek", "sk-ds-test-key");

        let probe = DeepseekProbe::new(server.base_url.clone());
        let err = probe
            .probe(&account, &client)
            .await
            .expect_err("malformed body must fail");
        assert!(
            err.message.contains("not valid JSON"),
            "got {:?}",
            err.message
        );
    }
}
