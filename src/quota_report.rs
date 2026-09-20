//! The account pool's last-known quota, projected for a dashboard.
//!
//! `lgw status` answers the operator's question -- which account can still
//! serve? A desktop widget asks a narrower one, "how much of my allowance is
//! gone and when does it come back?", and asks it with JSON paths rather than a
//! schema: it cannot loop over an array, so every window it reads has to have a
//! name of its own. This module turns a pool of accounts into that answer.
//!
//! Three rules shape all of it.
//!
//! Nothing is invented. A window with no reading is left out rather than sent
//! as `percent: 0`, because a dashboard cannot tell a zero it was given from a
//! zero it was owed, and "0% used" is the most reassuring lie available here.
//!
//! Covered and metered stay apart, exactly as they do on the board. A prepaid
//! allowance is not a balance, so covered headroom is never added into
//! `balance`, and an amount is never summed across currencies.
//!
//! The pooled percentage is INVENTORY, not the next account: sum(used) over
//! sum(cap) across every account in scope, parked and disabled included. One
//! spent account beside one fresh account reads 50%, because half the pool is
//! gone. Reporting whichever account is next would read 0% right up to the
//! moment everything stopped, which is the one moment the number matters.
//!
//! Nothing here touches the network. The report renders what the poller last
//! saw, so a widget's refresh interval never becomes provider traffic, and a
//! scope that has never been probed is an error rather than an empty report.
//!
//! Every type here is a DTO built field by field. `domain::Account` must never
//! be serialised: it holds an `ApiKey` whose `Debug` and `Display` are redacted
//! by hand but whose `Serialize` is derived and transparent, so serialising an
//! account would put a live upstream key in the body.

use crate::domain::{Account, AccountState, Gate, GateClass, GateKind};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::Serialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Which accounts a report covers.
#[derive(Debug, Clone, Copy, Default)]
pub struct Scope<'a> {
    /// A `[providers.*]` key.
    pub provider: Option<&'a str>,
    /// A qualified account id (`xkiro/acc-1`). A bare id is refused rather than
    /// guessed: an account id is unique only within its provider, so `acc-1`
    /// can name two different accounts.
    pub account: Option<&'a str>,
}

/// Why a report could not be produced.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReportError {
    #[error(
        "account {0:?} must be a qualified id, as in \"xkiro/acc-1\": an account id is unique \
         only within its provider"
    )]
    UnqualifiedAccount(String),
    #[error("unknown provider {0}")]
    UnknownProvider(String),
    #[error("unknown account {0}")]
    UnknownAccount(String),
    #[error(
        "no account in this scope has been probed yet; run `lgw status --refresh` or let the \
         poller run once"
    )]
    NeverProbed,
}

impl ReportError {
    /// The `type`/`code` the error envelope carries.
    pub fn kind(&self) -> &'static str {
        match self {
            ReportError::UnqualifiedAccount(_) => "invalid_request_error",
            ReportError::UnknownProvider(_) => "unknown_provider",
            ReportError::UnknownAccount(_) => "unknown_account",
            ReportError::NeverProbed => "not_probed",
        }
    }
}

/// The three window keys a dashboard knows, each named rather than indexed.
///
/// One generic type serves both the pooled projection and the per-account view,
/// so the two cannot come to disagree about which gate `rolling` means.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Windows<T> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rolling: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weekly: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monthly: Option<T>,
}

impl<T> Windows<T> {
    fn build(mut of_kind: impl FnMut(GateKind) -> Option<T>) -> Self {
        Windows {
            rolling: of_kind(GateKind::FiveHour),
            weekly: of_kind(GateKind::Weekly),
            monthly: of_kind(GateKind::Monthly),
        }
    }

    fn is_empty(&self) -> bool {
        self.rolling.is_none() && self.weekly.is_none() && self.monthly.is_none()
    }
}

/// One window of the pooled projection.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WindowPercent {
    /// Consumed, 0-100, to one decimal. A JSON number: the dashboard compares
    /// and draws it, and a quoted number is a string to everything that does
    /// not coerce.
    pub percent: f64,
    #[serde(rename = "resetsAt", skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<String>,
}

/// One window of one account, in that account's own currency.
///
/// Amounts are decimal TEXT, not numbers. This layer is the audit trail behind
/// the projection, and a figure that survived the ledger as an exact decimal
/// should not lose digits on the way out of it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WindowAmounts {
    pub used: String,
    pub cap: String,
    #[serde(rename = "resetsAt", skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<String>,
}

/// The pooled metered money: what real spending is left.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Balance {
    pub remaining: f64,
    pub used: f64,
    pub total: f64,
    pub currency: String,
}

/// One metered pot of one account.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Pot {
    /// `wallet` or `purchased_credits`.
    pub kind: GateKind,
    pub remaining: String,
    pub used: String,
}

/// One account, as the board would describe it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AccountView {
    /// The canonical `provider/id`.
    pub id: String,
    pub provider: String,
    pub label: String,
    pub state: AccountState,
    pub currency: String,
    pub stale: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_probe_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_at: Option<String>,
    pub windows: Windows<WindowAmounts>,
    pub pots: Vec<Pot>,
}

/// The whole answer: a projection a dashboard reads by path, over a native view
/// that mirrors `lgw status`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UsageReport {
    pub object: &'static str,
    /// The most recent probe among the accounts in scope.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<String>,
    pub stale: bool,
    /// Present only when every amount in this report is in one currency.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Windows<WindowPercent>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub balance: Option<Balance>,
    pub accounts: Vec<AccountView>,
}

/// Project a pool snapshot, narrowed by `scope`.
///
/// Pure: the caller supplies the accounts, so the whole contract is testable
/// without a database, a probe or a socket.
pub fn usage_report(accounts: &[Account], scope: Scope<'_>) -> Result<UsageReport, ReportError> {
    if let Some(id) = scope.account
        && !is_qualified(id)
    {
        return Err(ReportError::UnqualifiedAccount(id.to_string()));
    }
    // The snapshot is the authority on which providers exist: the configuration
    // refuses a provider with no accounts, so a name missing from here is a name
    // that was never configured.
    if let Some(provider) = scope.provider
        && !accounts.iter().any(|a| a.provider == provider)
    {
        return Err(ReportError::UnknownProvider(provider.to_string()));
    }

    let selected: Vec<&Account> = accounts
        .iter()
        .filter(|a| scope.provider.is_none_or(|p| a.provider == p))
        .filter(|a| scope.account.is_none_or(|id| a.qualified() == id))
        .collect();

    // One answer for all three ways an account filter can miss: an id nobody
    // has, the right id under another provider, and a provider/account pair
    // that do not belong together.
    if let Some(id) = scope.account
        && selected.is_empty()
    {
        return Err(ReportError::UnknownAccount(id.to_string()));
    }

    // Never probed is not an empty report. A dashboard cannot tell "nothing is
    // used" from "nobody has looked yet", and only one of those is safe to act
    // on, so the scope reports itself unavailable instead.
    if !selected
        .iter()
        .any(|a| a.last_probe_at.is_some() || !a.gates.is_empty())
    {
        return Err(ReportError::NeverProbed);
    }

    // A percentage spanning two currencies is a percentage of nothing, so the
    // projection is offered only when the accounts behind it agree. The two
    // projections ask their own contributors, which lets an all-USD balance
    // survive a pool whose covered windows disagree.
    let usage =
        single_currency(selected.iter().copied().filter(has_projectable_window)).and_then(|_| {
            let windows = Windows::build(|kind| pooled_window(&selected, kind));
            (!windows.is_empty()).then_some(windows)
        });

    Ok(UsageReport {
        object: "usage",
        checked_at: selected
            .iter()
            .filter_map(|a| a.last_probe_at)
            .max()
            .and_then(stamp),
        stale: selected.iter().copied().any(is_stale),
        currency: single_currency(
            selected
                .iter()
                .copied()
                .filter(|a| a.gates.iter().any(is_rendered)),
        ),
        usage,
        balance: pooled_balance(&selected),
        accounts: selected.iter().copied().map(account_view).collect(),
    })
}

/// A qualified id names both halves. `xkiro/` and `/acc-1` name neither.
fn is_qualified(id: &str) -> bool {
    id.split_once('/')
        .is_some_and(|(provider, account)| !provider.is_empty() && !account.is_empty())
}

/// The gates this report renders: the money windows and the money pots.
///
/// A rate limit is not an allowance, and a free-token allowance is counted in
/// tokens rather than money -- the same two exclusions the account board makes,
/// for the same reason. Printing either beside a currency turns a token count
/// into a sum of money.
fn is_rendered(gate: &Gate) -> bool {
    gate.kind.is_window() || gate.class() == GateClass::Metered
}

/// Stale means the numbers in THIS report were carried over from a probe that
/// failed. A rate-limit reading going stale says nothing about an allowance, so
/// only the rendered gates count.
fn is_stale(a: &Account) -> bool {
    a.gates.iter().filter(|g| is_rendered(g)).any(|g| g.stale)
}

fn has_projectable_window(a: &&Account) -> bool {
    a.gates
        .iter()
        .any(|g| g.kind.is_window() && g.cap > Decimal::ZERO)
}

/// One window across the whole scope.
///
/// A gate with no capacity is left out rather than counted as full: a cap of
/// zero is a reading the provider has not given us, and dividing what is left
/// by the rest of the pool is the honest answer.
fn pooled_window(selected: &[&Account], kind: GateKind) -> Option<WindowPercent> {
    let contributing: Vec<(&Account, &Gate)> = selected
        .iter()
        .flat_map(|a| a.gates.iter().map(move |g| (*a, g)))
        .filter(|(_, g)| g.kind == kind && g.cap > Decimal::ZERO)
        .collect();
    if contributing.is_empty() {
        return None;
    }

    let used: Decimal = contributing.iter().map(|(_, g)| g.used).sum();
    let cap: Decimal = contributing.iter().map(|(_, g)| g.cap).sum();
    if cap <= Decimal::ZERO {
        return None;
    }
    // Clamped because a provider that over-reports would otherwise send a
    // dashboard a bar past its own end, and the honest reading of "more than
    // spent" is "spent".
    let percent = (used * Decimal::ONE_HUNDRED / cap)
        .round_dp(1)
        .clamp(Decimal::ZERO, Decimal::ONE_HUNDRED)
        .to_f64()?;
    Some(WindowPercent {
        percent,
        resets_at: pooled_reset(&contributing),
    })
}

/// When this window's capacity next comes back.
///
/// A pool with room resets whenever its earliest window does. A pool with none
/// is being asked a different question -- when can I work again? -- and the
/// answer is the first SPENT account to recover, not an untouched account whose
/// window happens to roll sooner and will free nothing when it does.
fn pooled_reset(contributing: &[(&Account, &Gate)]) -> Option<String> {
    let spent = |(a, g): &&(&Account, &Gate)| {
        g.remaining() <= Decimal::ZERO || a.state == AccountState::Parked
    };
    let earliest = |only_spent: bool| {
        contributing
            .iter()
            .filter(|pair| !only_spent || spent(pair))
            .filter_map(|(_, g)| g.resets_at)
            .min()
    };
    earliest(true).or_else(|| earliest(false)).and_then(stamp)
}

/// The metered money across the scope.
///
/// Covered headroom is not money that is left; it is money already spent on a
/// subscription. Adding it here is how a dashboard talks an operator into a
/// purchase they had already made.
fn pooled_balance(selected: &[&Account]) -> Option<Balance> {
    let pots: Vec<&Gate> = selected
        .iter()
        .flat_map(|a| a.gates.iter())
        .filter(|g| g.class() == GateClass::Metered)
        .collect();
    if pots.is_empty() {
        return None;
    }
    let currency = single_currency(
        selected
            .iter()
            .copied()
            .filter(|a| a.gates.iter().any(|g| g.class() == GateClass::Metered)),
    )?;

    let remaining: Decimal = pots.iter().map(|g| g.remaining()).sum();
    let used: Decimal = pots.iter().map(|g| g.used).sum();
    Some(Balance {
        remaining: remaining.to_f64()?,
        used: used.to_f64()?,
        total: (remaining + used).to_f64()?,
        currency,
    })
}

/// The one currency these accounts are billed in, or None when they disagree or
/// when there are none. Amounts in different currencies cannot be summed, and a
/// total that quietly adds USD to CNY is worse than no total at all.
fn single_currency<'a>(accounts: impl IntoIterator<Item = &'a Account>) -> Option<String> {
    let mut found: Option<&str> = None;
    for a in accounts {
        match found {
            None => found = Some(&a.currency),
            Some(seen) if seen == a.currency => {}
            Some(_) => return None,
        }
    }
    found.map(str::to_string)
}

fn account_view(a: &Account) -> AccountView {
    AccountView {
        id: a.qualified(),
        provider: a.provider.clone(),
        label: a.label.clone(),
        state: a.state,
        currency: a.currency.clone(),
        stale: is_stale(a),
        last_probe_at: a.last_probe_at.and_then(stamp),
        last_error: a.last_error.as_deref().map(printable),
        available_at: a.available_at.and_then(stamp),
        windows: Windows::build(|kind| {
            a.gate(kind).map(|g| WindowAmounts {
                used: g.used.to_string(),
                cap: g.cap.to_string(),
                resets_at: g.resets_at.and_then(stamp),
            })
        }),
        pots: a
            .gates
            .iter()
            .filter(|g| g.class() == GateClass::Metered)
            .map(|g| Pot {
                kind: g.kind,
                remaining: g.remaining().to_string(),
                used: g.used.to_string(),
            })
            .collect(),
    }
}

fn stamp(t: OffsetDateTime) -> Option<String> {
    t.format(&Rfc3339).ok()
}

/// The pool's own reason text, made fit for a JSON body.
///
/// Most of these strings are ours ("quota exhausted", an error class), but
/// "probe failed: {reason}" wraps whatever the transport said: a URL, a TLS
/// message, something with a newline in the middle of it.
fn printable(message: &str) -> String {
    message
        .chars()
        .filter(|c| !c.is_control())
        .take(300)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ApiKey, Confidence, GateSource};
    use serde_json::{Value, json};

    const KEY: &str = "sk-must-never-appear";

    fn ts(offset_secs: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_775_000_000 + offset_secs).unwrap()
    }

    fn account(provider: &str, id: &str, gates: Vec<Gate>) -> Account {
        Account {
            id: id.into(),
            provider: provider.into(),
            label: id.into(),
            base_url: "https://example.invalid/v1".into(),
            api_key: ApiKey::new(KEY),
            currency: "USD".into(),
            gates,
            state: AccountState::Active,
            available_at: None,
            window_started_at: None,
            credit_multiplier: Decimal::ONE,
            rpm_limit: None,
            concurrency_limit: None,
            last_probe_at: Some(ts(0)),
            last_error: None,
        }
    }

    fn gate(kind: GateKind, cap: i64, used: i64, resets_at: Option<OffsetDateTime>) -> Gate {
        Gate {
            kind,
            cap: Decimal::from(cap),
            used: Decimal::from(used),
            resets_at,
            confidence: Confidence::Exact,
            source: GateSource::Polled,
            checked_at: ts(0),
            stale: false,
        }
    }

    fn five_hour(cap: i64, used: i64) -> Gate {
        gate(GateKind::FiveHour, cap, used, None)
    }

    fn report(accounts: &[Account]) -> UsageReport {
        usage_report(accounts, Scope::default()).expect("a probed scope reports")
    }

    fn rolling(report: &UsageReport) -> &WindowPercent {
        report
            .usage
            .as_ref()
            .expect("the projection is offered")
            .rolling
            .as_ref()
            .expect("the rolling window is present")
    }

    fn rfc3339(t: OffsetDateTime) -> String {
        t.format(&Rfc3339).unwrap()
    }

    fn cny_account() -> Account {
        let mut a = account("other", "b", vec![five_hour(100, 50)]);
        a.currency = "CNY".into();
        a
    }

    /// The number is how much of the POOL is gone, not how much of whichever
    /// account happens to be next. Reporting the next account reads 0% until
    /// the very last account fills, which is the one moment it has to warn.
    #[test]
    fn pool_used_percent_is_inventory_not_the_next_account() {
        let spent = account("xkiro", "spent", vec![five_hour(100, 100)]);
        let fresh = account("xkiro", "fresh", vec![five_hour(100, 0)]);
        assert_eq!(rolling(&report(&[spent, fresh])).percent, 50.0);
    }

    /// Different caps weigh differently: this is one pool of money, not the
    /// average of two percentages.
    #[test]
    fn unequal_caps_are_weighted_by_their_size() {
        let big = account("xkiro", "big", vec![five_hour(300, 300)]);
        let small = account("xkiro", "small", vec![five_hour(100, 0)]);
        assert_eq!(rolling(&report(&[big, small])).percent, 75.0);
    }

    /// A parked account's allowance is spent, not absent. Dropping it from the
    /// denominator would make a pool look healthier the more of it had failed.
    #[test]
    fn a_parked_account_still_counts_in_the_pool_percent() {
        let mut parked = account("xkiro", "parked", vec![five_hour(100, 100)]);
        parked.state = AccountState::Parked;
        let mut disabled = account("xkiro", "disabled", vec![five_hour(100, 100)]);
        disabled.state = AccountState::Disabled;
        let fresh = account("xkiro", "fresh", vec![five_hour(200, 0)]);

        let out = report(&[parked, disabled, fresh]);
        assert_eq!(rolling(&out).percent, 50.0);
        assert_eq!(out.accounts.len(), 3, "and all three are still listed");
    }

    /// When the pool is out of room, the reset that matters is the first SPENT
    /// account to recover. An untouched account with an earlier reset frees
    /// nothing, and a dashboard counting down to it counts down to no change.
    #[test]
    fn the_pooled_reset_is_the_first_spent_account_to_recover() {
        let spent = account(
            "xkiro",
            "spent",
            vec![gate(GateKind::FiveHour, 100, 100, Some(ts(9_000)))],
        );
        let idle = account(
            "xkiro",
            "idle",
            vec![gate(GateKind::FiveHour, 100, 0, Some(ts(60)))],
        );
        assert_eq!(
            rolling(&report(&[spent, idle])).resets_at,
            Some(rfc3339(ts(9_000)))
        );

        // With nothing spent, the earliest reset is simply the earliest.
        let a = account(
            "xkiro",
            "a",
            vec![gate(GateKind::FiveHour, 100, 10, Some(ts(9_000)))],
        );
        let b = account(
            "xkiro",
            "b",
            vec![gate(GateKind::FiveHour, 100, 10, Some(ts(60)))],
        );
        assert_eq!(rolling(&report(&[a, b])).resets_at, Some(rfc3339(ts(60))));
    }

    /// Covered headroom is a subscription already paid for. Folding it into the
    /// balance tells an operator they have money left that they do not.
    #[test]
    fn covered_remaining_is_not_added_to_balance() {
        let a = account(
            "xkiro",
            "a",
            vec![five_hour(100, 10), gate(GateKind::Wallet, 5, 0, None)],
        );
        let balance = report(&[a]).balance.expect("a wallet is a balance");
        assert_eq!(balance.remaining, 5.0, "not 95, and not 100");
        assert_eq!(balance.used, 0.0);
        assert_eq!(balance.total, 5.0);
        assert_eq!(balance.currency, "USD");
    }

    #[test]
    fn mixed_currencies_omit_the_top_level_projection() {
        let out = report(&[
            account("xkiro", "a", vec![five_hour(100, 50)]),
            cny_account(),
        ]);
        assert!(out.usage.is_none(), "a percent over two currencies is none");
        assert!(out.currency.is_none());
        assert_eq!(out.accounts.len(), 2, "the native view survives");

        // Narrowed to one provider, the same pool projects cleanly.
        let accounts = [
            account("xkiro", "a", vec![five_hour(100, 50)]),
            cny_account(),
        ];
        let scoped = usage_report(
            &accounts,
            Scope {
                provider: Some("xkiro"),
                account: None,
            },
        )
        .unwrap();
        assert_eq!(rolling(&scoped).percent, 50.0);
        assert_eq!(scoped.currency.as_deref(), Some("USD"));
    }

    /// An absent window is absent. `percent: 0` would be indistinguishable from
    /// an untouched allowance, and that is the reading nobody acts on.
    #[test]
    fn a_missing_window_is_omitted_not_zero() {
        let out = report(&[account("xkiro", "a", vec![five_hour(100, 20)])]);
        let usage = out.usage.as_ref().unwrap();
        assert!(usage.weekly.is_none());
        assert!(usage.monthly.is_none());

        let body = serde_json::to_value(&out).unwrap();
        assert!(body["usage"].get("weekly").is_none(), "{body}");
        assert!(body["usage"].get("monthly").is_none(), "{body}");
        assert_eq!(body["usage"]["rolling"]["percent"], json!(20.0));
    }

    /// A window whose cap is zero is a reading the provider has not given us,
    /// so it neither reports itself full nor drags the rest of the pool down.
    #[test]
    fn a_window_with_no_capacity_does_not_report_itself_as_full() {
        let empty = account("xkiro", "empty", vec![five_hour(0, 0)]);
        let real = account("xkiro", "real", vec![five_hour(100, 25)]);
        assert_eq!(rolling(&report(&[empty, real])).percent, 25.0);

        assert!(
            report(&[account("xkiro", "empty", vec![five_hour(0, 0)])])
                .usage
                .is_none()
        );
    }

    /// A free-token allowance is counted in tokens, not money. It has no place
    /// in a money percentage, and no dashboard window of its own.
    #[test]
    fn a_free_token_allowance_is_not_a_dashboard_window() {
        let a = account(
            "xkiro",
            "a",
            vec![
                five_hour(100, 40),
                gate(GateKind::FreeTokens, 30_000_000, 368, None),
                gate(GateKind::Rpm, 600, 12, None),
            ],
        );
        let out = report(&[a]);
        assert_eq!(rolling(&out).percent, 40.0, "the token gate is not money");

        let body = serde_json::to_value(&out).unwrap();
        let text = body.to_string();
        assert!(!text.contains("30000000"), "{text}");
        assert!(!text.contains("free_tokens"), "{text}");
        assert!(!text.contains("rpm"), "{text}");
        assert_eq!(body["accounts"][0]["pots"], json!([]));
    }

    /// A stale reading keeps its numbers and says so. Zeroing it would report
    /// an untouched allowance at the exact moment we stopped being able to see.
    #[test]
    fn a_stale_contributor_marks_the_report_stale() {
        let mut a = account("xkiro", "a", vec![five_hour(100, 80)]);
        a.gates[0].stale = true;
        let out = report(&[a]);
        assert!(out.stale);
        assert!(out.accounts[0].stale);
        assert_eq!(rolling(&out).percent, 80.0, "the carried-over number stays");
    }

    #[test]
    fn never_probed_scope_is_unavailable() {
        let mut never = account("xkiro", "a", vec![]);
        never.last_probe_at = None;
        never.state = AccountState::Unknown;
        assert_eq!(
            usage_report(std::slice::from_ref(&never), Scope::default()),
            Err(ReportError::NeverProbed)
        );

        // One probed account is enough. The unprobed one is still listed; it
        // simply contributes nothing.
        let out = report(&[never, account("xkiro", "b", vec![five_hour(100, 10)])]);
        assert_eq!(rolling(&out).percent, 10.0);
        assert_eq!(out.accounts.len(), 2);
    }

    /// A bare account id is ambiguous across providers, so it is refused rather
    /// than resolved to whichever account sorts first.
    #[test]
    fn account_filter_requires_a_qualified_id() {
        let accounts = [
            account("xkiro", "acc-1", vec![five_hour(100, 10)]),
            account("commandcode", "acc-1", vec![five_hour(100, 90)]),
        ];
        let scoped = |provider: Option<&str>, acct: Option<&str>| {
            usage_report(
                &accounts,
                Scope {
                    provider,
                    account: acct,
                },
            )
        };

        assert_eq!(
            scoped(None, Some("acc-1")),
            Err(ReportError::UnqualifiedAccount("acc-1".into()))
        );
        assert_eq!(
            scoped(None, Some("xkiro/")),
            Err(ReportError::UnqualifiedAccount("xkiro/".into()))
        );

        let one = scoped(None, Some("xkiro/acc-1")).unwrap();
        assert_eq!(one.accounts.len(), 1);
        assert_eq!(rolling(&one).percent, 10.0);

        assert_eq!(
            scoped(Some("nope"), None),
            Err(ReportError::UnknownProvider("nope".into()))
        );
        assert_eq!(
            scoped(None, Some("xkiro/nope")),
            Err(ReportError::UnknownAccount("xkiro/nope".into()))
        );
        assert_eq!(
            scoped(Some("commandcode"), Some("xkiro/acc-1")),
            Err(ReportError::UnknownAccount("xkiro/acc-1".into())),
            "an account that is not in the named provider is not found"
        );
    }

    /// The report is served to a desktop widget over the local key. An upstream
    /// account key reaching that body would hand a provider credential to
    /// anything that could already read a percentage.
    #[test]
    fn the_report_does_not_contain_an_api_key() {
        let mut a = account("xkiro", "a", vec![five_hour(100, 10)]);
        a.last_error = Some("probe failed: 401\nfrom https://api.example/v1".into());
        let text = serde_json::to_string(&report(&[a])).unwrap();

        assert!(!text.contains(KEY), "{text}");
        assert!(!text.contains("api_key"), "{text}");
        assert!(!text.contains("base_url"), "{text}");
        assert!(!text.contains("credit_multiplier"), "{text}");
        assert!(!text.contains('\n'), "a control character survived: {text}");
    }

    /// The two layers answer different questions and are shaped for it: the
    /// dashboard's numbers are JSON numbers, the audit trail keeps its exact
    /// decimals as text.
    #[test]
    fn the_projection_is_numeric_and_the_account_view_is_exact() {
        let mut a = account("xkiro", "a", vec![]);
        a.gates = vec![
            Gate {
                used: Decimal::new(950, 2),
                cap: Decimal::new(2000, 2),
                ..five_hour(0, 0)
            },
            Gate {
                used: Decimal::ZERO,
                cap: Decimal::new(500, 2),
                ..gate(GateKind::Wallet, 0, 0, None)
            },
        ];
        let body: Value = serde_json::to_value(report(&[a])).unwrap();

        assert!(body["usage"]["rolling"]["percent"].is_number());
        assert_eq!(body["usage"]["rolling"]["percent"], json!(47.5));
        assert!(body["balance"]["remaining"].is_number());
        assert_eq!(body["balance"]["remaining"], json!(5.0));

        assert_eq!(
            body["accounts"][0]["windows"]["rolling"]["used"],
            json!("9.50"),
            "the exact decimal survives as text"
        );
        assert_eq!(body["accounts"][0]["pots"][0]["kind"], json!("wallet"));
        assert_eq!(body["accounts"][0]["state"], json!("active"));
        assert_eq!(body["object"], json!("usage"));
    }

    /// A 200 body that happens to look like a failure breaks a path client: it
    /// reads `error.message` before it reads anything it asked for.
    #[test]
    fn a_report_never_looks_like_a_business_error() {
        let body = serde_json::to_value(report(&[account("xkiro", "a", vec![five_hour(100, 10)])]))
            .unwrap();
        for key in ["error", "ok", "success", "code", "msg"] {
            assert!(body.get(key).is_none(), "{key} is in {body}");
        }
    }
}
