//! Helpers shared by the CLI command tests.

use crate::domain::{AccountOverview, AccountState, Confidence, Gate, GateKind, GateSource};
use rust_decimal::Decimal;
use std::str::FromStr;
use time::OffsetDateTime;

pub(super) fn dec(s: &str) -> Decimal {
    Decimal::from_str(s).expect("valid decimal")
}

pub(super) fn gate(
    kind: GateKind,
    cap: &str,
    used: &str,
    resets_in_secs: Option<i64>,
    confidence: Confidence,
    stale: bool,
    now: OffsetDateTime,
) -> Gate {
    Gate {
        kind,
        cap: dec(cap),
        used: dec(used),
        resets_at: resets_in_secs.map(|s| now + time::Duration::seconds(s)),
        confidence,
        source: GateSource::Polled,
        checked_at: now,
        stale,
    }
}

pub(super) fn overview(
    provider: &str,
    id: &str,
    currency: &str,
    state: AccountState,
    gates: Vec<Gate>,
) -> AccountOverview {
    AccountOverview {
        account_id: id.to_string(),
        provider: provider.to_string(),
        label: id.to_string(),
        currency: currency.to_string(),
        state,
        gates,
        available_at: None,
        window_started_at: None,
        last_error: None,
    }
}

pub(super) fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
}
