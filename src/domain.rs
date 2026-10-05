//! Core domain types shared by every module.
//!
//! FROZEN CONTRACT: other modules depend on these shapes; changing them requires a
//! coordinated update across the crate.
//!
//! Central model:
//!   - An account holds a set of Gates. EVERY gate must pass for a request
//!     to be accepted (AND semantics).
//!   - GateClass::Covered gates are money already paid for by a subscription
//!     and are forfeited when their window resets.
//!   - GateClass::Metered gates are real money (wallet / purchased credits).
//!     Command Code silently overflows into purchased credits when a window is
//!     exhausted, so routers MUST prefer Covered and must never silently spend
//!     Metered while a Covered gate is available.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::fmt;
use time::OffsetDateTime;

pub type AccountId = String;
pub type ProviderId = String;

/// Which pot of money a gate meters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateClass {
    /// Prepaid by a subscription. Forfeited when the window resets; free to spend.
    Covered,
    /// Real money or purchased credit. Spending it is permanent.
    Metered,
    /// Not money at all (requests-per-minute and similar pacing limits).
    RateLimit,
}

/// The concrete gate a provider enforces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateKind {
    FiveHour,
    Weekly,
    Monthly,
    /// Free-tier daily token allowance (rolling 24h).
    FreeTokens,
    /// Wallet balance (xKiro wallet, DeepSeek balance).
    Wallet,
    /// Purchased, non-expiring credits (Command Code).
    PurchasedCredits,
    Rpm,
}

impl GateKind {
    pub fn class(self) -> GateClass {
        match self {
            GateKind::FiveHour | GateKind::Weekly | GateKind::Monthly | GateKind::FreeTokens => {
                GateClass::Covered
            }
            GateKind::Wallet | GateKind::PurchasedCredits => GateClass::Metered,
            GateKind::Rpm => GateClass::RateLimit,
        }
    }

    /// True for gates whose quota resets on a rolling window.
    pub fn is_window(self) -> bool {
        matches!(
            self,
            GateKind::FiveHour | GateKind::Weekly | GateKind::Monthly
        )
    }

    /// Stable label used by the CLI.
    pub fn label(self) -> &'static str {
        match self {
            GateKind::FiveHour => "5h",
            GateKind::Weekly => "7d",
            GateKind::Monthly => "1mo",
            GateKind::FreeTokens => "free",
            GateKind::Wallet => "wallet",
            GateKind::PurchasedCredits => "credits",
            GateKind::Rpm => "rpm",
        }
    }
}

/// How much we trust a gate reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// Read directly from the provider.
    Exact,
    /// Derived from the local ledger or a fallback estimate.
    Estimated,
}

/// Where a gate reading came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateSource {
    /// Returned by a provider quota endpoint.
    Polled,
    /// Observed from response headers.
    ResponseHeader,
    /// Maintained by our own per-request accounting.
    LocalLedger,
}

/// A single quota gate on an account.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Gate {
    pub kind: GateKind,
    /// Capacity of this gate in its own unit (USD, credits, tokens).
    pub cap: Decimal,
    /// Amount consumed in the current window.
    pub used: Decimal,
    /// When this gate resets, if it is a rolling window.
    pub resets_at: Option<OffsetDateTime>,
    pub confidence: Confidence,
    pub source: GateSource,
    /// When this reading was taken.
    pub checked_at: OffsetDateTime,
    /// True when the last probe failed and this is a carried-over reading.
    /// A stale gate must never be rendered as zero.
    pub stale: bool,
}

impl Gate {
    pub fn class(&self) -> GateClass {
        self.kind.class()
    }

    pub fn remaining(&self) -> Decimal {
        let r = self.cap - self.used;
        if r < Decimal::ZERO { Decimal::ZERO } else { r }
    }

    /// Fraction consumed, 0.0 and up. Not clamped at 1.0: a provider that
    /// over-reports is signalling real trouble and we want to see it.
    pub fn utilization(&self) -> Decimal {
        if self.cap <= Decimal::ZERO {
            return Decimal::ZERO;
        }
        self.used / self.cap
    }

    pub fn utilization_f64(&self) -> f64 {
        use rust_decimal::prelude::ToPrimitive;
        self.utilization().to_f64().unwrap_or(0.0)
    }
}

/// Lifecycle state of an account in the pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountState {
    /// Eligible for new work.
    Active,
    /// Soft threshold crossed: no NEW sessions, but sticky sessions finish.
    Draining,
    /// A gate is exhausted; returns when available_at passes.
    Parked,
    /// Auth failure or credit exhaustion. Requires operator action.
    Disabled,
    /// Restored from disk and not yet probed. Must not serve traffic.
    Unknown,
}

impl AccountState {
    pub fn label(self) -> &'static str {
        match self {
            AccountState::Active => "ACTIVE",
            AccountState::Draining => "DRAINING",
            AccountState::Parked => "PARKED",
            AccountState::Disabled => "DISABLED",
            AccountState::Unknown => "UNKNOWN",
        }
    }
}

/// An API key that will not leak into logs.
#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn new(s: impl Into<String>) -> Self {
        ApiKey(s.into())
    }
    /// This text with the key replaced by ***, for anything that leaves the
    /// process: a provider that echoes the credential it was given must not
    /// turn an error message into a leak.
    pub fn redact(&self, text: &str) -> String {
        if self.0.is_empty() {
            return text.to_string();
        }
        text.replace(&self.0, "***")
    }

    /// Explicit accessor; greppable so key use stays auditable.
    pub fn expose(&self) -> &str {
        &self.0
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(***)")
    }
}

impl fmt::Display for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

/// A provider account plus its last-known quota state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: AccountId,
    pub provider: ProviderId,
    /// Friendly name for the CLI.
    pub label: String,
    pub base_url: String,
    pub api_key: ApiKey,
    pub currency: String,
    pub gates: Vec<Gate>,
    pub state: AccountState,
    /// When a parked account becomes eligible again.
    pub available_at: Option<OffsetDateTime>,
    /// When the current rolling window opened (first request after a reset).
    pub window_started_at: Option<OffsetDateTime>,
    /// Learned or configured effective price multiplier for this account.
    pub credit_multiplier: Decimal,
    pub rpm_limit: Option<u32>,
    pub concurrency_limit: Option<u32>,
    /// Whether this account's wallet may be spent, when its provider says
    /// something different from `[policy] allow_metered`.
    ///
    /// None follows the policy. The override exists because the policy flag is
    /// all-or-nothing: admitting one wallet-only provider by turning it on also
    /// unlocks the overflow path for every other provider, Command Code's
    /// purchased credits among them.
    pub allow_metered: Option<bool>,
    pub last_probe_at: Option<OffsetDateTime>,
    pub last_error: Option<String>,
}

/// Canonical account identity.
///
/// An account id is unique only WITHIN a provider. Keying anything -- pool
/// lookups, sticky sessions, database rows -- by the bare id silently merged
/// two different accounts: with xkiro/acc-1 and commandcode/acc-1 configured,
/// both quota probes wrote onto whichever sorted first, so one account showed
/// the other's allowances. Provider and id are therefore always taken together.
pub fn qualified_id(provider: &str, account_id: &str) -> String {
    format!("{provider}/{account_id}")
}

impl Account {
    /// This account's canonical identity: provider/id.
    pub fn qualified(&self) -> String {
        qualified_id(&self.provider, &self.id)
    }

    /// Covered gates only.
    pub fn covered_gates(&self) -> impl Iterator<Item = &Gate> {
        self.gates
            .iter()
            .filter(|g| g.class() == GateClass::Covered)
    }

    /// Metered gates only.
    pub fn metered_gates(&self) -> impl Iterator<Item = &Gate> {
        self.gates
            .iter()
            .filter(|g| g.class() == GateClass::Metered)
    }

    /// Covered gates that gate the ACCOUNT itself, which means the
    /// money-denominated ones.
    ///
    /// The free-token allowance is deliberately excluded. Exhausting it blocks
    /// only free models; paid models keep working. Letting it drive the account
    /// threshold would retire an account from paid traffic because an allowance
    /// nobody was using ran out, which is not what that limit means.
    pub fn account_gates(&self) -> impl Iterator<Item = &Gate> {
        self.covered_gates()
            .filter(|g| g.kind != GateKind::FreeTokens)
    }

    /// Highest utilization across the gates that actually gate the account.
    pub fn peak_covered_utilization(&self) -> Option<f64> {
        self.account_gates()
            .map(|g| g.utilization_f64())
            .reduce(f64::max)
    }

    /// Lowest account-level covered remaining, for consume-first ordering.
    pub fn min_covered_remaining(&self) -> Option<Decimal> {
        self.account_gates()
            .map(|g| g.remaining())
            .reduce(Decimal::min)
    }

    /// Whether any prepaid allowance is reported at all.
    ///
    /// Any covered gate counts, including the free-token allowance, because this
    /// answers "did the provider report a prepaid pot". Whether that pot may be
    /// spent is a different question -- see account_gates() and the headroom
    /// calculation in the pool, which returns None for a free-only account and
    /// so routes it down the metered path.
    pub fn has_covered(&self) -> bool {
        self.covered_gates().next().is_some()
    }

    pub fn gate(&self, kind: GateKind) -> Option<&Gate> {
        self.gates.iter().find(|g| g.kind == kind)
    }
}

/// Token accounting for one upstream response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Portion of prompt tokens served from prompt cache.
    pub cached_tokens: u64,
    /// Reasoning / thinking tokens, when the provider reports them.
    pub reasoning_tokens: u64,
    /// False when these numbers were estimated locally rather than reported.
    pub measured: bool,
}

impl TokenUsage {
    pub fn total_tokens(&self) -> u64 {
        self.prompt_tokens.saturating_add(self.completion_tokens)
    }

    /// Cache hit ratio, the single most important cost metric: switching
    /// accounts mid-session destroys it.
    pub fn cache_hit_ratio(&self) -> Option<f64> {
        if self.prompt_tokens == 0 {
            return None;
        }
        Some(self.cached_tokens as f64 / self.prompt_tokens as f64)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestStatus {
    Ok,
    Error,
    Aborted,
}

/// One client request, after completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestRecord {
    pub request_id: String,
    pub ts: OffsetDateTime,
    pub session_key: Option<String>,
    /// Model exactly as the client asked for it.
    pub client_model: String,
    pub provider: ProviderId,
    pub account_id: AccountId,
    /// Model actually sent upstream.
    pub model: String,
    pub usage: TokenUsage,
    pub ttft_ms: Option<u64>,
    pub total_ms: u64,
    pub tps: Option<f64>,
    pub cost: Decimal,
    pub currency: String,
    pub credit_multiplier: Decimal,
    /// Which pot paid for this request.
    pub coverage: GateClass,
    pub status: RequestStatus,
    pub retries: u32,
    pub provider_request_id: Option<String>,
}

/// One attempt against one account. Persisted separately so retries and
/// account switches do not pollute TTFT/TPS.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttemptRecord {
    pub request_id: String,
    pub attempt_no: u32,
    pub account_id: AccountId,
    pub provider: ProviderId,
    pub started_at: OffsetDateTime,
    /// None when the attempt failed before any content was produced.
    pub ttft_ms: Option<u64>,
    pub status_code: Option<u16>,
    pub error_class: Option<UpstreamErrorClass>,
    pub error_code: Option<String>,
    pub retry_after_ms: Option<u64>,
    /// Whether this attempt is believed to have consumed quota.
    pub billed: bool,
    pub bytes_sent: u64,
}

pub use crate::error::UpstreamErrorClass;

/// How the CLI groups usage output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupBy {
    Provider,
    Model,
    Account,
    Day,
}

impl GroupBy {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "provider" => Some(GroupBy::Provider),
            "model" => Some(GroupBy::Model),
            "account" => Some(GroupBy::Account),
            "day" => Some(GroupBy::Day),
            _ => None,
        }
    }
    pub fn as_sql_expr(self) -> &'static str {
        match self {
            GroupBy::Provider => "provider",
            GroupBy::Model => "model",
            GroupBy::Account => "account_id",
            GroupBy::Day => "date(ts, 'unixepoch')",
        }
    }
    pub fn column_header(self) -> &'static str {
        match self {
            GroupBy::Provider => "PROVIDER",
            GroupBy::Model => "MODEL",
            GroupBy::Account => "ACCOUNT",
            GroupBy::Day => "DAY",
        }
    }
}

/// One aggregated row for the usage command.
#[derive(Debug, Clone)]
pub struct UsageRow {
    pub key: String,
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub cost: Decimal,
    pub currency: String,
}

/// One row for the tail command.
#[derive(Debug, Clone)]
pub struct RequestSummary {
    pub ts: OffsetDateTime,
    pub request_id: String,
    pub provider: ProviderId,
    pub account_id: AccountId,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub ttft_ms: Option<u64>,
    pub tps: Option<f64>,
    pub coverage: GateClass,
    pub status: RequestStatus,
}

/// Account plus gates, as restored from disk for the CLI.
#[derive(Debug, Clone)]
pub struct AccountOverview {
    pub account_id: AccountId,
    pub provider: ProviderId,
    pub label: String,
    pub currency: String,
    pub state: AccountState,
    pub gates: Vec<Gate>,
    pub available_at: Option<OffsetDateTime>,
    pub window_started_at: Option<OffsetDateTime>,
    pub last_error: Option<String>,
}

/// Convenience: current time.
pub fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

/// Money helper: parse a provider's fixed-point string without precision loss.
/// Providers such as xKiro send amounts as strings exactly so that small
/// per-request costs survive JSON parsing (for example "0.000356").
pub fn parse_money(s: &str) -> Option<Decimal> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse::<Decimal>().ok()
}
