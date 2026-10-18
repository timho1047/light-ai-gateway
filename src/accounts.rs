//! The account pool: gate evaluation, selection, and the local ledger.
//!
//! FROZEN CONTRACT: the router's decisions and the ledger read this pool's interface.
//!
//! This is where every economic decision happens. Three rules drive it:
//!
//! 1. HARD GATE (anti-spend). A request is accepted only when the account's
//!    covered headroom covers the estimated cost plus a safety reserve for
//!    requests already in flight. Providers such as Command Code do not return
//!    an error when a window is exhausted -- they silently bill purchased
//!    credits instead -- so this gate is the only thing standing between a
//!    window limit and real money.
//!
//! 2. SOFT THRESHOLD (preference, not a wall). Accounts at or above
//!    soft_threshold stop taking NEW sessions but keep serving sticky ones.
//!    When every account is above the threshold the least-utilised one is used
//!    anyway; that keeps a self-inflicted rule from locking the pool out.
//!
//! 3. CONSUME-FIRST. Among eligible accounts below the soft threshold pick the
//!    MOST utilised. Picking the least-utilised instead makes selection flip
//!    between accounts on every request; measured over 72 simulated hours that
//!    produced roughly 1400 switches versus 27, and every switch throws away
//!    the upstream prompt cache.
//!
//! Unit note: gate arithmetic assumes a money-like unit (USD or credits).
//! FreeTokens gates are a token allowance rather than money
//! (GateClass::TokenAllowance), so every account-level decision here -- the hard
//! gate, the soft threshold and the consume-first ordering -- excludes them by
//! class instead of by a filter each site has to remember.
//! Exhausting a free allowance blocks free models only;
//! letting it retire the account would take paid models offline because an
//! allowance nobody was spending ran out. It reaches the client as the
//! provider's own 429.

use crate::config::PolicyConfig;
use crate::domain::{
    Account, AccountId, AccountState, Confidence, Gate, GateClass, GateKind, GateSource,
    ProviderId, now,
};
use crate::error::UpstreamErrorClass;
use crate::tier::TierKey;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use time::OffsetDateTime;

/// Why a request could not be planned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// No account for this provider could satisfy the hard gate.
    ///
    /// The arithmetic that produced the refusal travels with the error. "No
    /// eligible account" alone is unactionable: it cannot distinguish a spent
    /// allowance from a request that was priced too high from a model with no
    /// price at all, and those need three different responses. Answering that
    /// question against a live gateway cost an hour of forensics, which is the
    /// whole reason these fields exist.
    NoEligibleAccount {
        provider: ProviderId,
        /// How long until the earliest account becomes eligible again.
        retry_after: Option<Duration>,
        /// Estimated cost plus the safety reserve: what the request needed.
        needed: Decimal,
        /// The account that came closest, and the headroom it could offer.
        best_account: Option<AccountId>,
        best_headroom: Option<Decimal>,
        /// Why even that account was refused.
        reason: Option<RejectReason>,
    },
    UnknownProvider(ProviderId),
}

impl std::fmt::Display for RejectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            RejectReason::Unknown => "not probed yet",
            RejectReason::Parked => "parked after a quota error",
            RejectReason::Disabled => "disabled",
            RejectReason::InsufficientHeadroom => "headroom below the estimate",
            RejectReason::MeteredNotAllowed => "only metered money left, and spending it is off",
            RejectReason::NoGates => "no quota reading yet",
        })
    }
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::NoEligibleAccount {
                provider,
                retry_after,
                needed,
                best_account,
                best_headroom,
                reason,
            } => {
                write!(f, "no eligible account for provider {provider}")?;
                if let (Some(acct), Some(head)) = (best_account, best_headroom) {
                    write!(
                        f,
                        ": the request was estimated at {needed}, and {acct} has the most room, \
                         with {head} left"
                    )?;
                    if let Some(r) = reason {
                        write!(f, " ({r})")?;
                    }
                }
                if let Some(d) = retry_after {
                    write!(f, "; earliest reset in {}s", d.as_secs())?;
                }
                Ok(())
            }
            PlanError::UnknownProvider(p) => write!(f, "unknown provider {p}"),
        }
    }
}

impl std::error::Error for PlanError {}

/// A decision about which account should serve a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptPlan {
    pub request_id: String,
    pub account_id: AccountId,
    pub provider: ProviderId,
    /// Model name to send upstream.
    pub model: String,
    pub session_key: Option<String>,
    /// Which pot the router expects to pay from.
    pub coverage: GateClass,
    /// Price window this request was priced at, decided when it was planned.
    /// The pool knows nothing about schedules, so it starts at the default and
    /// the router fixes it.
    pub tier: TierKey,
    pub attempt_no: u32,
    /// What this attempt committed against its account's hard gate when it was
    /// planned. Released on settle, and read by the next request's reserve.
    ///
    /// This is what makes the in-flight term a real number instead of a
    /// decoration: without it, twenty concurrent requests each
    /// size the gate as if they were alone and can commit twenty times the
    /// headroom the account actually has.
    pub estimated_cost: Decimal,
}

impl AttemptPlan {
    /// Stable key used for sticky session binding.
    pub fn sticky_key(&self) -> Option<&str> {
        self.session_key.as_deref()
    }

    /// Canonical identity of the account this plan targets: provider/id.
    pub fn qualified_account(&self) -> String {
        crate::domain::qualified_id(&self.provider, &self.account_id)
    }
}

/// A committed attempt: the account that was chosen, and the obligation to give
/// its commitment back.
///
/// select() commits the estimate under the lock that chose the account, and this
/// value owns that commitment. settle() spends it against what the attempt
/// actually cost; dropping it unsettled returns it. It is deliberately not
/// Clone, so two owners cannot each return the same commitment: settle()
/// reconciles on the account alone, and a second return would decrement the
/// in-flight count twice and hand the next request headroom that is not there.
pub struct Reservation {
    pool: Pool,
    plan: AttemptPlan,
    settled: bool,
}

impl Reservation {
    /// What this attempt committed, for reading or describing.
    pub fn plan(&self) -> &AttemptPlan {
        &self.plan
    }

    /// Fix the price tier, once the router has resolved the model's schedule.
    ///
    /// The rest of the plan is read-only: the committed estimate is what the
    /// hard gate sized itself against, and letting a caller revise it afterwards
    /// would unbalance the in-flight total that settle() subtracts.
    pub fn set_tier(&mut self, tier: TierKey) {
        self.plan.tier = tier;
    }

    /// Reconcile the commitment with what the attempt actually cost.
    pub fn settle(mut self, actual_cost: Decimal, billed: bool) {
        self.pool.settle(&self.plan, actual_cost, billed);
        self.settled = true;
    }
}

/// Debug prints the plan and whether the commitment is still outstanding.
///
/// Pool is a handle to shared state, not something worth printing, and a test
/// that unwraps a select() needs to see which account it got.
impl std::fmt::Debug for Reservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reservation")
            .field("plan", &self.plan)
            .field("settled", &self.settled)
            .finish_non_exhaustive()
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.settled {
            self.pool.settle(&self.plan, Decimal::ZERO, false);
        }
    }
}

impl std::ops::Deref for Reservation {
    type Target = AttemptPlan;

    fn deref(&self) -> &AttemptPlan {
        &self.plan
    }
}

/// Outcome of evaluating one account against the hard gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateDecision {
    /// Acceptable, paid from covered allowance.
    Covered,
    /// Acceptable, but would spend real money.
    Metered,
    /// Not acceptable.
    Reject(RejectReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// Not probed yet.
    Unknown,
    Parked,
    Disabled,
    /// Headroom below estimated cost plus reserve.
    InsufficientHeadroom,
    /// Only metered headroom exists and policy forbids spending it.
    MeteredNotAllowed,
    /// No gates at all.
    NoGates,
}

struct Inflight {
    /// Estimated cost currently committed on this account.
    committed: Decimal,
    count: u32,
}

/// Find an account by its canonical provider/id identity.
///
/// Never match on the bare id: it is unique only within a provider.
fn find_mut<'a>(accounts: &'a mut [Account], qualified: &str) -> Option<&'a mut Account> {
    accounts.iter_mut().find(|a| a.qualified() == qualified)
}

fn find<'a>(accounts: &'a [Account], qualified: &str) -> Option<&'a Account> {
    accounts.iter().find(|a| a.qualified() == qualified)
}

/// Bind a session to an account, keeping the eviction order in step.
fn bind_sticky(inner: &mut PoolInner, key: &str, qualified: &str) {
    let PoolInner {
        sticky,
        sticky_order,
        ..
    } = inner;
    if sticky
        .insert(key.to_string(), qualified.to_string())
        .is_some()
    {
        // Refresh recency: this session is live, so it evicts last.
        if let Some(pos) = sticky_order.iter().position(|k| k == key) {
            sticky_order.remove(pos);
        }
    }
    sticky_order.push_back(key.to_string());

    while sticky.len() > MAX_STICKY_SESSIONS {
        match sticky_order.pop_front() {
            Some(oldest) => {
                sticky.remove(&oldest);
            }
            None => break,
        }
    }
}

/// Drop one account's session bindings and keep the eviction order consistent.
///
/// The match is on the canonical provider/id. Comparing against the bare id
/// never matched anything, so a parked or disabled account kept every session
/// bound to it and every one of those sessions had to fail before rebinding.
fn forget_sticky(inner: &mut PoolInner, qualified: &str) {
    let PoolInner {
        sticky,
        sticky_order,
        ..
    } = inner;
    sticky.retain(|_, v| v != qualified);
    sticky_order.retain(|k| sticky.contains_key(k));
}

/// Why an account was taken out of the pool by the operator.
///
/// Recorded in the account's own error text so the reason survives a restart,
/// and read back by apply_probe: a successful probe proves the CREDENTIAL works
/// again, which is not the same thing as the operator changing their mind.
/// The only way back is the operator saying so, with "accounts enable".
pub const OPERATOR_DISABLED: &str = "disabled by operator (lgw accounts disable)";

/// The window an account's rpm_limit is measured over.
const RATE_WINDOW: Duration = Duration::from_secs(60);

/// How many session bindings are remembered at once.
///
/// Session keys come straight from client-controlled headers, so an unbounded
/// map is an unbounded allocation driven by whoever can reach the port. At 4096
/// bindings the worst case is a few hundred kilobytes, and evicting the oldest
/// only costs a prompt-cache miss for a session nobody has used in a long while.
const MAX_STICKY_SESSIONS: usize = 4096;

struct PoolInner {
    accounts: Vec<Account>,
    /// session key -> canonical provider/id of the bound account.
    sticky: HashMap<String, String>,
    /// Session keys in eviction order, oldest first. Kept beside the map so the
    /// map cannot grow without bound; see MAX_STICKY_SESSIONS.
    sticky_order: std::collections::VecDeque<String>,
    inflight: HashMap<AccountId, Inflight>,
    /// Recent completed request costs, used to size the safety reserve.
    recent_costs: std::collections::VecDeque<Decimal>,
    /// When each rate-limited account last started a request, for rpm_limit.
    rpm_window: HashMap<AccountId, std::collections::VecDeque<Instant>>,
}

/// The pool. All methods are synchronous and short; state is guarded by a
/// plain Mutex that is never held across an await point.
///
/// Clone is a second handle to the same pool, not a copy of it: a Reservation
/// carries one so it can give its commitment back without borrowing the pool it
/// came from.
#[derive(Clone)]
pub struct Pool {
    inner: Arc<Mutex<PoolInner>>,
    /// The policy knobs an apply is allowed to retune on a running pool. Behind
    /// one lock so a request never reads half of an apply's new policy.
    policy: Arc<RwLock<PoolPolicy>>,
    /// Last time a request was served, used to pick the polling cadence.
    last_activity: Arc<Mutex<Instant>>,
}

/// The three policy values admission reads. Copied under one lock so an apply
/// cannot expose a half-changed policy to a request.
#[derive(Debug, Clone, Copy, PartialEq)]
struct PoolPolicy {
    soft_threshold: f64,
    allow_metered: bool,
    hard_margin: Decimal,
}

impl From<&PolicyConfig> for PoolPolicy {
    fn from(policy: &PolicyConfig) -> PoolPolicy {
        PoolPolicy {
            soft_threshold: policy.soft_threshold,
            allow_metered: policy.allow_metered,
            hard_margin: Decimal::try_from(policy.hard_margin).unwrap_or(Decimal::ZERO),
        }
    }
}

/// What a reconciliation changed, including which accounts the caller should
/// probe now.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Reconfigured {
    pub added: Vec<String>,
    pub reset: Vec<String>,
    pub retired: Vec<String>,
    /// Retired accounts whose last in-flight attempt has now settled.
    pub dropped: Vec<String>,
    /// (provider, account id) pairs that need a fresh probe before serving.
    pub probe: Vec<(ProviderId, AccountId)>,
}

/// The last_error marker that records "no longer configured", distinct from an
/// operator disabling an account: only adding the account back clears it.
pub const RETIRED_BY_APPLY: &str = "retired by configuration apply (no longer in the file)";

fn retired_by_apply(account: &Account) -> bool {
    account.state == AccountState::Disabled
        && account.last_error.as_deref() == Some(RETIRED_BY_APPLY)
}

/// Copy the fields that come from configuration onto the account that keeps the
/// runtime state. Readings, sticky bindings, pacing windows and in-flight
/// commitments are deliberately absent: they belong to the live account.
fn adopt_config(keep: &mut Account, fresh: &Account) {
    keep.label = fresh.label.clone();
    keep.base_url = fresh.base_url.clone();
    keep.api_key = fresh.api_key.clone();
    keep.currency = fresh.currency.clone();
    keep.credit_multiplier = fresh.credit_multiplier;
    keep.rpm_limit = fresh.rpm_limit;
    keep.concurrency_limit = fresh.concurrency_limit;
    keep.allow_metered = fresh.allow_metered;
}

fn forget_reading(account: &mut Account) {
    account.state = AccountState::Unknown;
    account.gates.clear();
    account.available_at = None;
    account.window_started_at = None;
    account.last_probe_at = None;
    account.last_error = None;
}

/// A newly added account serves nothing until its first probe. An account whose
/// credential could not be resolved keeps the Disabled state and its reason.
fn as_added(mut fresh: Account) -> Account {
    if fresh.state != AccountState::Disabled {
        forget_reading(&mut fresh);
    }
    fresh
}

fn has_in_flight(inner: &PoolInner, qualified: &str) -> bool {
    inner
        .inflight
        .get(qualified)
        .is_some_and(|entry| entry.count > 0)
}

impl Pool {
    pub fn new(accounts: Vec<Account>, policy: &PolicyConfig) -> Self {
        Pool {
            inner: Arc::new(Mutex::new(PoolInner {
                accounts,
                sticky: HashMap::new(),
                sticky_order: std::collections::VecDeque::new(),
                inflight: HashMap::new(),
                recent_costs: std::collections::VecDeque::new(),
                rpm_window: HashMap::new(),
            })),
            policy: Arc::new(RwLock::new(PoolPolicy::from(policy))),
            last_activity: Arc::new(Mutex::new(Instant::now())),
        }
    }

    pub fn allow_metered(&self) -> bool {
        self.policy().allow_metered
    }

    pub fn soft_threshold(&self) -> f64 {
        self.policy().soft_threshold
    }

    fn hard_margin(&self) -> Decimal {
        self.policy().hard_margin
    }

    fn policy(&self) -> PoolPolicy {
        match self.policy.read() {
            Ok(policy) => *policy,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }

    /// Accept a new policy on the running pool. Accounts, sticky sessions and
    /// in-flight reservations stay exactly as they are: only the three values
    /// the next admission reads move.
    pub fn retune(&self, policy: &PolicyConfig) {
        let next = PoolPolicy::from(policy);
        match self.policy.write() {
            Ok(mut current) => *current = next,
            Err(poisoned) => *poisoned.into_inner() = next,
        }
    }

    /// Reconcile the pool with the accounts a configuration describes.
    ///
    /// Accounts that stay keep everything runtime-shaped: readings, sticky
    /// bindings, pacing windows and in-flight commitments. Their
    /// configuration-derived fields are updated in place, so the next attempt
    /// uses the new limits without losing what the account knows about itself.
    /// Accounts that leave are retired rather than deleted: they stop being
    /// selected, but an attempt already in flight can still settle. A retired
    /// account is dropped once it is settled and the file still leaves it out.
    ///
    /// `reset_providers` names providers whose adapter or base URL changed; their
    /// accounts go back to Unknown and stay out of selection until re-probed,
    /// because every reading they hold was taken against a different endpoint.
    pub fn reconfigure(&self, fresh: Vec<Account>, reset_providers: &[ProviderId]) -> Reconfigured {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut outcome = Reconfigured::default();
        let mut pending: HashMap<String, Account> =
            fresh.into_iter().map(|a| (a.qualified(), a)).collect();
        let existing = std::mem::take(&mut inner.accounts);
        let mut accounts: Vec<Account> = Vec::with_capacity(existing.len() + pending.len());
        let mut forget_sticky_for: Vec<String> = Vec::new();

        for mut account in existing {
            let qualified = account.qualified();
            let Some(fresh) = pending.remove(&qualified) else {
                // No longer configured.
                if retired_by_apply(&account) {
                    if has_in_flight(&inner, &qualified) {
                        accounts.push(account);
                    } else {
                        outcome.dropped.push(qualified);
                    }
                } else if has_in_flight(&inner, &qualified) {
                    account.state = AccountState::Disabled;
                    account.last_error = Some(RETIRED_BY_APPLY.to_string());
                    outcome.retired.push(qualified.clone());
                    forget_sticky_for.push(qualified);
                    accounts.push(account);
                } else {
                    account.state = AccountState::Disabled;
                    account.last_error = Some(RETIRED_BY_APPLY.to_string());
                    outcome.retired.push(qualified.clone());
                    outcome.dropped.push(qualified.clone());
                    forget_sticky_for.push(qualified);
                }
                continue;
            };

            if retired_by_apply(&account) {
                // Added back: this is a new account, not the retired shell.
                accounts.push(as_added(fresh));
                outcome.added.push(qualified);
                continue;
            }

            adopt_config(&mut account, &fresh);
            if fresh.state == AccountState::Disabled {
                // The new credential source cannot be resolved; keep the
                // readings but stop selecting the account until it is fixed.
                account.state = AccountState::Disabled;
                account.last_error = fresh.last_error.clone();
            } else if reset_providers.contains(&account.provider) {
                forget_reading(&mut account);
                outcome
                    .probe
                    .push((account.provider.clone(), account.id.clone()));
                outcome.reset.push(qualified);
            }
            accounts.push(account);
        }

        for (qualified, fresh) in pending {
            let account = as_added(fresh);
            if account.state != AccountState::Disabled {
                outcome
                    .probe
                    .push((account.provider.clone(), account.id.clone()));
            }
            outcome.added.push(qualified);
            accounts.push(account);
        }

        for qualified in &outcome.dropped {
            inner.inflight.remove(qualified);
            inner.rpm_window.remove(qualified);
        }
        for qualified in &forget_sticky_for {
            forget_sticky(&mut inner, qualified);
        }
        inner.accounts = accounts;
        outcome
    }

    /// Take an account out of rotation for the operator, or put it back.
    ///
    /// Disabling is explicit and survives probes; enabling only lifts the
    /// operator's decision and returns the account to Unknown, so traffic
    /// resumes only after a probe proves it can pay. Returns the account as it
    /// now is, for persisting.
    pub fn set_operator_state(
        &self,
        qualified_id: &str,
        enabled: bool,
    ) -> std::result::Result<Account, String> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(acct) = find_mut(&mut inner.accounts, qualified_id) else {
            return Err(format!("no account {qualified_id}"));
        };
        if enabled {
            if acct.state != AccountState::Disabled {
                return Err(format!(
                    "{qualified_id} is {}, not DISABLED; there is nothing to enable",
                    acct.state.label()
                ));
            }
            acct.state = AccountState::Unknown;
            acct.last_error = None;
            acct.last_probe_at = None;
        } else {
            acct.state = AccountState::Disabled;
            acct.last_error = Some(OPERATOR_DISABLED.to_string());
        }
        Ok(acct.clone())
    }

    pub fn note_activity(&self) {
        if let Ok(mut t) = self.last_activity.lock() {
            *t = Instant::now();
        }
    }

    /// Requests currently committed against any account, for the console's
    /// vitals. A number for a dashboard, not a routing input.
    pub fn in_flight_total(&self) -> u32 {
        match self.inner.lock() {
            Ok(inner) => inner.inflight.values().map(|entry| entry.count).sum(),
            Err(poisoned) => poisoned
                .into_inner()
                .inflight
                .values()
                .map(|entry| entry.count)
                .sum(),
        }
    }

    /// Requests currently committed against an account.
    ///
    /// Test-only: the in-flight counter is what the hard gate and the pacing
    /// limits read, and a reservation that never comes back is invisible
    /// except as an account that quietly stops accepting work.
    #[cfg(test)]
    pub fn inflight_count(&self, qualified: &str) -> u32 {
        self.inner
            .lock()
            .unwrap()
            .inflight
            .get(qualified)
            .map(|e| e.count)
            .unwrap_or(0)
    }

    /// Number of sessions currently bound to an account.
    ///
    /// Test-only: the map is an implementation detail, and an unbounded one was
    /// exactly the defect.
    #[cfg(test)]
    fn tracked_sessions(&self) -> usize {
        self.inner.lock().unwrap().sticky.len()
    }

    pub fn idle_for(&self) -> Duration {
        self.last_activity
            .lock()
            .map(|t| t.elapsed())
            .unwrap_or_default()
    }

    /// Safety reserve for ONE account: the larger of what recent completed
    /// requests cost (p95) and what is actually committed on this account right
    /// now, plus the operator's fixed margin.
    ///
    /// The in-flight term is per account on purpose. A burst aimed at a single
    /// account is the case that overflows a window, and a pool-wide p95 cannot
    /// see it: every request in the burst would otherwise size its own reserve
    /// as though the other nineteen were not there.
    fn safety_reserve(&self, p95: Decimal, qualified: &str, inner: &PoolInner) -> Decimal {
        let inflight = inner
            .inflight
            .get(qualified)
            .map(|e| e.committed)
            .unwrap_or(Decimal::ZERO);
        p95.max(inflight) + self.hard_margin()
    }

    /// p95 of recently completed request costs.
    ///
    /// Depends on no particular account, so select computes it once instead of
    /// re-sorting the window for every candidate -- which it did, inside the
    /// pool lock, on the hot path.
    fn recent_p95(inner: &PoolInner) -> Decimal {
        if inner.recent_costs.is_empty() {
            return Decimal::ZERO;
        }
        let mut v: Vec<Decimal> = inner.recent_costs.iter().copied().collect();
        v.sort();
        let idx = ((v.len() as f64 * 0.95) as usize).min(v.len() - 1);
        v[idx]
    }

    /// Headroom on covered gates only, in gate units.
    ///
    /// A token allowance is not in this class, so a USD estimate is never
    /// compared against a token cap.
    fn covered_headroom(&self, acct: &Account) -> Option<Decimal> {
        let mut min: Option<Decimal> = None;
        for g in acct.covered_gates() {
            if g.cap <= Decimal::ZERO {
                continue;
            }
            let rem = g.remaining();
            min = Some(match min {
                Some(m) if m <= rem => m,
                _ => rem,
            });
        }
        min
    }

    fn metered_headroom(&self, acct: &Account) -> Option<Decimal> {
        let mut min: Option<Decimal> = None;
        for g in acct.metered_gates() {
            if g.cap <= Decimal::ZERO {
                continue;
            }
            let rem = g.remaining();
            min = Some(match min {
                Some(m) if m <= rem => m,
                _ => rem,
            });
        }
        min
    }

    /// Evaluate one account against the hard gate.
    pub fn evaluate(&self, acct: &Account, est_cost: Decimal, reserve: Decimal) -> GateDecision {
        if acct.state == AccountState::Disabled {
            return GateDecision::Reject(RejectReason::Disabled);
        }
        if acct.state == AccountState::Unknown {
            return GateDecision::Reject(RejectReason::Unknown);
        }
        if acct.state == AccountState::Parked {
            return GateDecision::Reject(RejectReason::Parked);
        }
        if acct.gates.is_empty() {
            return GateDecision::Reject(RejectReason::NoGates);
        }

        let needed = est_cost + reserve;
        // Spending real money is a per-account question: the provider's own
        // answer outranks the policy, in both directions.
        let allow_metered = acct.allow_metered.unwrap_or_else(|| self.allow_metered());

        if acct.has_covered() {
            match self.covered_headroom(acct) {
                Some(h) if h >= needed => return GateDecision::Covered,
                // Covered is exhausted; whether metered may absorb the overflow
                // is a policy question, and the default answer is no.
                Some(_) => {
                    if allow_metered && self.metered_headroom(acct).is_some_and(|m| m >= needed) {
                        return GateDecision::Metered;
                    }
                    return GateDecision::Reject(RejectReason::InsufficientHeadroom);
                }
                // No money-denominated covered gate at all.
                None => {
                    if allow_metered && self.metered_headroom(acct).is_some_and(|m| m >= needed) {
                        return GateDecision::Metered;
                    }
                    return GateDecision::Reject(RejectReason::InsufficientHeadroom);
                }
            }
        }

        // Pure metered provider (for example DeepSeek, HaoAI): nothing is
        // prepaid, so every request costs real money.
        if allow_metered && self.metered_headroom(acct).is_some_and(|m| m >= needed) {
            return GateDecision::Metered;
        }
        GateDecision::Reject(RejectReason::MeteredNotAllowed)
    }

    /// Accounts held back by their own pacing limits.
    ///
    /// rpm_limit and concurrency_limit were parsed, stored in the database and
    /// then never read: setting either one changed nothing at all. They are
    /// pacing, not quota, so a held-back account is simply not a candidate --
    /// it is not parked and it does not fail the request.
    fn pacing_blocks(inner: &mut PoolInner, at: Instant) -> std::collections::HashSet<String> {
        // Nothing to look at when no account has a limit, which is the usual
        // case: this runs under the pool lock on every request.
        if !inner
            .accounts
            .iter()
            .any(|a| a.rpm_limit.is_some() || a.concurrency_limit.is_some())
        {
            return std::collections::HashSet::new();
        }
        let PoolInner {
            accounts,
            inflight,
            rpm_window,
            ..
        } = inner;
        let mut blocked = std::collections::HashSet::new();
        for acct in accounts.iter() {
            let qualified = acct.qualified();
            if let Some(limit) = acct.concurrency_limit
                && inflight.get(&qualified).is_some_and(|e| e.count >= limit)
            {
                blocked.insert(qualified.clone());
            }
            if let Some(limit) = acct.rpm_limit {
                let window = rpm_window.entry(qualified.clone()).or_default();
                // A rolling minute, which is how every provider states it.
                while window
                    .front()
                    .is_some_and(|t| at.duration_since(*t) >= RATE_WINDOW)
                {
                    window.pop_front();
                }
                if window.len() >= limit as usize {
                    blocked.insert(qualified);
                }
            }
        }
        blocked
    }

    /// Move a parked account back to active once its window has reset.
    fn reap(&self, inner: &mut PoolInner, at: OffsetDateTime) {
        for acct in inner.accounts.iter_mut() {
            if acct.state == AccountState::Parked
                && let Some(when) = acct.available_at
                && when <= at
            {
                acct.state = AccountState::Active;
                acct.available_at = None;
                acct.window_started_at = None;
                // Reset ONLY the windows that have actually rolled over.
                //
                // Zeroing every window because one of them came due was
                // a systematic under-count: a 5-hour reset wiped the
                // 7-day and monthly usage that was still very much live,
                // and the account then looked empty for up to a poll
                // interval -- long enough to spend past a cap the
                // provider still enforces.
                //
                // available_at speaks for exactly one window: the one
                // that was spent when the account was parked. A window
                // with a reset time still in the future has NOT rolled
                // over and keeps its usage. A window with no reset time
                // at all is cleared only when it is the spent one, so an
                // unknown-but-live weekly reading is not thrown away on
                // the strength of a 5-hour timer.
                for g in acct.gates.iter_mut() {
                    if !g.kind.is_window() {
                        continue;
                    }
                    // A window is known to have rolled over only when
                    // the provider said when it resets AND that time has
                    // passed. An unknown reset time is not evidence:
                    // note_error marks EVERY window spent when an account
                    // is parked, so a saturated window with no reset time
                    // may simply be collateral damage, and clearing it
                    // erased usage the provider still enforces. Leaving it
                    // costs at most one poll interval before the next
                    // probe replaces the whole reading.
                    if g.resets_at.is_some_and(|r| r <= at) {
                        g.used = Decimal::ZERO;
                        g.resets_at = None;
                        g.stale = false;
                    }
                }
            }
        }
    }

    /// Pick the best account for a request.
    ///
    /// session_key binds a conversation to one account. While that account
    /// remains acceptable it wins regardless of utilisation, because moving a
    /// session discards the upstream prompt cache and re-bills the whole
    /// conversation at full input price.
    // Seven arguments, and each one is a different question: which provider and
    // model, which session, what it costs, which request and attempt this is, and
    // which accounts are already spent for it. Bundling them into a struct would
    // move the same list one line up without making anything clearer.
    #[allow(clippy::too_many_arguments)]
    pub fn select(
        &self,
        provider: &str,
        model: &str,
        session_key: Option<&str>,
        est_cost: Decimal,
        request_id: &str,
        attempt_no: u32,
        // Accounts this request has already tried, as canonical provider/id.
        // They are not banned, only deprioritised, so a single-account pool
        // keeps its bounded retry while a multi-account pool finally reaches the
        // healthy account instead of spending every attempt on the fault.
        exclude: &[String],
    ) -> Result<Reservation, PlanError> {
        let at = now();
        let mut inner = self.inner.lock().unwrap();
        self.reap(&mut inner, at);
        let blocked = Self::pacing_blocks(&mut inner, Instant::now());
        let p95 = Self::recent_p95(&inner);

        struct Cand {
            /// Bare account id, as it appears in configuration.
            id: AccountId,
            /// Canonical provider/id, used for sticky binding.
            qualified: String,
            decision: GateDecision,
            util: f64,
            remaining: Decimal,
            draining: bool,
            /// Already attempted for this very request.
            tried: bool,
        }

        let provider_known = inner.accounts.iter().any(|a| a.provider == provider);
        if !provider_known {
            return Err(PlanError::UnknownProvider(provider.to_string()));
        }

        let mut covered_cands: Vec<Cand> = Vec::new();
        let mut metered_cands: Vec<Cand> = Vec::new();
        let mut earliest: Option<OffsetDateTime> = None;
        // The account that came closest and why it still failed. Reported back
        // through PlanError so the 429 can name the number that did not fit.
        let mut best: Option<(String, Decimal, RejectReason)> = None;
        // The largest reserve any account was asked to keep clear, so a refusal
        // can carry the arithmetic that produced it.
        let mut gate_reserve = self.hard_margin();

        for acct in inner.accounts.iter() {
            if acct.provider != provider {
                continue;
            }
            // When does this account next become usable? Either it is parked
            // with a scheduled return, or one of its covered windows is spent
            // and will roll over. The latter matters because an account can be
            // rejected by the hard gate while still nominally Active/Draining,
            // and without this the 429 we return would carry no Retry-After.
            let next_ready = acct.available_at.or_else(|| {
                acct.covered_gates()
                    .filter(|g| g.remaining() <= Decimal::ZERO)
                    .filter_map(|g| g.resets_at)
                    .min()
            });
            if let Some(w) = next_ready {
                earliest = Some(match earliest {
                    Some(e) if e <= w => e,
                    _ => w,
                });
            }
            let qualified = acct.qualified();
            // Pacing limits are the operator's own words about this account;
            // they are not a quota verdict, so the account is simply skipped.
            if blocked.contains(&qualified) {
                continue;
            }
            let tried = exclude.contains(&qualified);
            // Sized per account, so the commitments already in flight on THIS
            // account are part of what this request must fit inside.
            let reserve = self.safety_reserve(p95, &qualified, &inner);
            gate_reserve = gate_reserve.max(reserve);
            match self.evaluate(acct, est_cost, reserve) {
                GateDecision::Covered => {
                    covered_cands.push(Cand {
                        id: acct.id.clone(),
                        qualified,
                        decision: GateDecision::Covered,
                        util: acct.peak_covered_utilization().unwrap_or(0.0),
                        remaining: acct.min_covered_remaining().unwrap_or(Decimal::MAX),
                        draining: acct.state == AccountState::Draining,
                        tried,
                    });
                }
                GateDecision::Metered => {
                    metered_cands.push(Cand {
                        id: acct.id.clone(),
                        qualified,
                        decision: GateDecision::Metered,
                        util: 0.0,
                        remaining: self.metered_headroom(acct).unwrap_or(Decimal::MAX),
                        draining: acct.state == AccountState::Draining,
                        tried,
                    });
                }
                GateDecision::Reject(reason) => {
                    let headroom = self
                        .covered_headroom(acct)
                        .or_else(|| self.metered_headroom(acct));
                    if let Some(h) = headroom
                        && best.as_ref().is_none_or(|(_, best_h, _)| h > *best_h)
                    {
                        best = Some((acct.qualified(), h, reason));
                    }
                }
            }
        }

        // Sticky affinity first: keeping a live session on its account is worth
        // more than any utilisation ordering.
        if let Some(key) = session_key
            && let Some(bound) = inner.sticky.get(key).cloned()
        {
            let mut bound_untried: Option<&Cand> = None;
            let mut bound_tried: Option<&Cand> = None;
            let mut any_untried = false;
            for cand in covered_cands.iter().chain(metered_cands.iter()) {
                if !cand.tried {
                    any_untried = true;
                }
                if cand.qualified != bound {
                    continue;
                }
                if cand.tried {
                    bound_tried = Some(cand);
                } else {
                    bound_untried = Some(cand);
                }
            }
            // Keep the session where its prompt cache lives -- unless that
            // account is the one this very request already failed on AND
            // something else is still untried. Sticking there would spend the
            // entire retry budget on the same fault.
            //
            // The old "fresh.or(already_tried)" short-circuited before the
            // untried preference ever ran, so every session-bound retry went
            // back to the account that had just failed.
            let sticky_choice = match (bound_untried, any_untried) {
                (Some(cand), _) => Some(cand),
                (None, true) => None,
                (None, false) => bound_tried,
            };
            if let Some(cand) = sticky_choice {
                let coverage = match cand.decision {
                    GateDecision::Covered => GateClass::Covered,
                    GateDecision::Metered => GateClass::Metered,
                    GateDecision::Reject(_) => {
                        unreachable!("candidate lists only hold acceptable accounts")
                    }
                };
                let plan = AttemptPlan {
                    request_id: request_id.to_string(),
                    account_id: cand.id.clone(),
                    provider: provider.to_string(),
                    model: model.to_string(),
                    session_key: session_key.map(str::to_string),
                    coverage,
                    // The pool knows nothing about price windows; the router
                    // sets this once it has resolved the schedule.
                    tier: TierKey::Default,
                    attempt_no,
                    estimated_cost: est_cost,
                };
                self.commit(&mut inner, &plan);
                return Ok(self.reservation(plan));
            }
            // Bound account no longer acceptable: fall through and rebind.
        }

        // Prefer covered over metered, always. Within the chosen class, prefer
        // accounts this request has not already tried: retrying the account that
        // just answered 503 spends the whole budget on the same fault and never
        // reaches the healthy account sitting next to it.
        let (pool, coverage) = if !covered_cands.is_empty() {
            (covered_cands, GateClass::Covered)
        } else if !metered_cands.is_empty() {
            (metered_cands, GateClass::Metered)
        } else {
            let retry_after = earliest.map(|w| {
                let d = w - at;
                if d.is_negative() {
                    Duration::ZERO
                } else {
                    d.unsigned_abs()
                }
            });
            return Err(PlanError::NoEligibleAccount {
                provider: provider.to_string(),
                retry_after,
                needed: est_cost + gate_reserve,
                best_account: best.as_ref().map(|(a, _, _)| a.clone()),
                best_headroom: best.as_ref().map(|(_, h, _)| *h),
                reason: best.as_ref().map(|(_, _, r)| *r),
            });
        };

        // Fresh accounts first; if every candidate has already been tried, keep
        // the full list so a single-account pool still gets its bounded retry.
        let eligible: Vec<&Cand> = {
            let untried: Vec<&Cand> = pool.iter().filter(|c| !c.tried).collect();
            if untried.is_empty() {
                pool.iter().collect()
            } else {
                untried
            }
        };

        // Prefer accounts below the soft threshold; a draining account is one
        // that has already crossed it.
        let below: Vec<&Cand> = eligible
            .iter()
            .copied()
            .filter(|c| !c.draining && c.util < self.soft_threshold())
            .collect();

        let chosen: &Cand = if below.is_empty() {
            // Every account is at or above the soft threshold. Rather than
            // refusing, fall back to the least-utilised one: switching at 80%
            // is a preference, and a self-imposed preference must not become a
            // hard outage while headroom genuinely remains.
            eligible
                .iter()
                .copied()
                .min_by(|a, b| {
                    a.util
                        .partial_cmp(&b.util)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| a.qualified.cmp(&b.qualified))
                })
                .unwrap_or(eligible[0])
        } else {
            // CONSUME-FIRST: the most utilised still-eligible account, so the
            // pool burns down one account at a time and switches rarely. Every
            // switch costs a prompt-cache miss.
            below
                .iter()
                .copied()
                .max_by(|a, b| {
                    a.util
                        .partial_cmp(&b.util)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| b.remaining.cmp(&a.remaining))
                })
                .unwrap_or(below[0])
        };

        if let Some(key) = session_key {
            // Sticky binds to the canonical identity, so two providers using the
            // same account id can never bind to each other.
            bind_sticky(&mut inner, key, &chosen.qualified);
        }

        let plan = AttemptPlan {
            request_id: request_id.to_string(),
            account_id: chosen.id.clone(),
            provider: provider.to_string(),
            model: model.to_string(),
            session_key: session_key.map(str::to_string),
            coverage,
            tier: TierKey::Default,
            attempt_no,
            estimated_cost: est_cost,
        };
        // Committed under the very lock that chose the account, so a burst
        // cannot all read the same headroom before any of it is recorded.
        self.commit(&mut inner, &plan);
        Ok(self.reservation(plan))
    }

    /// Wrap a committed plan in the one value that can return the commitment.
    fn reservation(&self, plan: AttemptPlan) -> Reservation {
        Reservation {
            pool: self.clone(),
            plan,
            settled: false,
        }
    }

    /// Commit an attempt's estimated cost against its account.
    ///
    /// Called by select() while it still holds the lock, deliberately. Reserving
    /// in a separate call let two concurrent requests both read the same
    /// headroom before either of them recorded its commitment, which is exactly
    /// how a burst overflows a window. There is no longer a public reserve step
    /// for a caller to forget.
    fn commit(&self, inner: &mut PoolInner, plan: &AttemptPlan) {
        let qualified = plan.qualified_account();
        let e = inner.inflight.entry(qualified.clone()).or_insert(Inflight {
            committed: Decimal::ZERO,
            count: 0,
        });
        e.committed += plan.estimated_cost;
        e.count += 1;
        if let Some(acct) = find_mut(&mut inner.accounts, &qualified)
            && acct.window_started_at.is_none()
        {
            acct.window_started_at = Some(now());
        }
        // Give the rpm window something to count.
        let rpm_limited = find(&inner.accounts, &qualified).is_some_and(|a| a.rpm_limit.is_some());
        if rpm_limited {
            inner
                .rpm_window
                .entry(qualified)
                .or_default()
                .push_back(Instant::now());
        }
    }

    /// Reconcile a reservation with actual usage and update the ledger.
    ///
    /// When the billed flag is false the attempt failed before the upstream did
    /// any work, so the gate must not be charged for it.
    ///
    /// Private: the only way to reach it is through the Reservation that owns the
    /// commitment, which is what makes a double return unexpressible.
    fn settle(&self, plan: &AttemptPlan, actual_cost: Decimal, billed: bool) {
        let mut inner = self.inner.lock().unwrap();
        let qualified = plan.qualified_account();

        if let Some(e) = inner.inflight.get_mut(&qualified) {
            // Release exactly this attempt's reservation. Clearing the whole
            // account instead made every other concurrent request invisible to
            // the gate the moment the first of them finished.
            if e.count > 0 {
                e.count -= 1;
            }
            e.committed = (e.committed - plan.estimated_cost).max(Decimal::ZERO);
        }

        if !billed {
            self.maybe_forget_sticky(&mut inner, plan);
            return;
        }

        if let Some(acct) = find_mut(&mut inner.accounts, &qualified) {
            // Charge covered windows first (they are the ones that expire),
            // then metered pools.
            let mut left = actual_cost;
            for g in acct.gates.iter_mut() {
                if left <= Decimal::ZERO {
                    break;
                }
                if g.kind.class() != GateClass::Covered || g.cap <= Decimal::ZERO {
                    continue;
                }
                let head = g.remaining();
                let take = if head < left { head } else { left };
                g.used += take;
                g.source = GateSource::LocalLedger;
                left -= take;
            }
            if left > Decimal::ZERO {
                for g in acct.gates.iter_mut() {
                    if left <= Decimal::ZERO {
                        break;
                    }
                    if g.kind.class() != GateClass::Metered || g.cap <= Decimal::ZERO {
                        continue;
                    }
                    let head = g.remaining();
                    let take = if head < left { head } else { left };
                    g.used += take;
                    g.source = GateSource::LocalLedger;
                    left -= take;
                }
            }
        }

        inner.recent_costs.push_back(actual_cost);
        while inner.recent_costs.len() > 200 {
            inner.recent_costs.pop_front();
        }

        // Crossing the soft threshold retires the account from NEW sessions.
        // Sticky sessions are deliberately left bound so they keep their cache.
        let soft = self.soft_threshold();
        if let Some(acct) = find_mut(&mut inner.accounts, &qualified)
            && acct.state == AccountState::Active
            && let Some(u) = acct.peak_covered_utilization()
            && u >= soft
        {
            acct.state = AccountState::Draining;
        }

        self.maybe_forget_sticky(&mut inner, plan);
    }

    /// Drop the sticky binding when that account is finished for good, so the
    /// next request of this session rebinds instead of failing.
    fn maybe_forget_sticky(&self, inner: &mut PoolInner, plan: &AttemptPlan) {
        if let Some(key) = plan.sticky_key() {
            let unusable = find(&inner.accounts, &plan.qualified_account())
                .map(|a| {
                    matches!(
                        a.state,
                        AccountState::Parked | AccountState::Disabled | AccountState::Unknown
                    )
                })
                .unwrap_or(true);
            if unusable {
                inner.sticky.remove(key);
            }
        }
    }

    /// Record an upstream failure and transition the account if warranted.
    ///
    /// window_reset should come from a quota probe when one is available: on
    /// providers that reuse a single 429 code for pacing and exhaustion it is
    /// the only trustworthy signal.
    /// qualified_id is the canonical provider/id, never a bare account id.
    pub fn note_error(
        &self,
        qualified_id: &str,
        class: UpstreamErrorClass,
        retry_after: Option<Duration>,
        window_reset: Option<OffsetDateTime>,
    ) {
        let at = now();
        let mut inner = self.inner.lock().unwrap();
        let Some(acct) = find_mut(&mut inner.accounts, qualified_id) else {
            return;
        };

        match class {
            UpstreamErrorClass::QuotaExhausted => {
                acct.state = AccountState::Parked;
                acct.available_at = window_reset.or_else(|| {
                    retry_after.map(|d| at + time::Duration::seconds(d.as_secs() as i64))
                });
                acct.last_error = Some("quota exhausted".to_string());
                // The window is spent; reflect that in the ledger immediately.
                for g in acct.gates.iter_mut() {
                    if g.kind.is_window() {
                        g.used = g.cap;
                        g.source = GateSource::LocalLedger;
                        g.confidence = Confidence::Estimated;
                    }
                }
            }
            UpstreamErrorClass::InsufficientCredit | UpstreamErrorClass::Auth => {
                acct.state = AccountState::Disabled;
                acct.last_error = Some(class.as_str().to_string());
            }
            UpstreamErrorClass::RateLimitBurst => {
                // Pacing only: stay on this account and stay sticky.
                acct.last_error = Some("rate limited (burst)".to_string());
            }
            UpstreamErrorClass::Overloaded | UpstreamErrorClass::Transport => {
                // Transient. Do not change state; the retry loop moves on.
                acct.last_error = Some(class.as_str().to_string());
            }
            _ => {}
        }

        // A dying account must not keep stranding sessions on it.
        if matches!(acct.state, AccountState::Parked | AccountState::Disabled) {
            let dead = acct.qualified();
            forget_sticky(&mut inner, &dead);
        }
    }

    /// Apply fresh gate readings from a quota probe.
    pub fn apply_probe(&self, qualified_id: &str, gates: Vec<Gate>, at: OffsetDateTime) {
        let mut inner = self.inner.lock().unwrap();
        let policy_metered = self.allow_metered();
        let soft = self.soft_threshold();
        let Some(acct) = find_mut(&mut inner.accounts, qualified_id) else {
            return;
        };
        if gates.is_empty() {
            return;
        }
        // Read the reason BEFORE clearing it: it is the only record of who
        // disabled this account.
        let disabled_by_operator = acct
            .last_error
            .as_deref()
            .is_some_and(|reason| reason == OPERATOR_DISABLED);
        acct.gates = gates;
        acct.last_probe_at = Some(at);
        acct.last_error = None;

        if acct.state == AccountState::Unknown {
            acct.state = AccountState::Active;
        }

        let has_room = acct.covered_gates().any(|g| g.remaining() > Decimal::ZERO);
        let has_metered = acct.metered_gates().any(|g| g.remaining() > Decimal::ZERO);
        let allow_metered = acct.allow_metered.unwrap_or(policy_metered);
        let usable = has_room || (allow_metered && has_metered);

        // An account parked because its window was spent comes back once the
        // probe shows headroom again.
        if acct.state == AccountState::Parked && usable {
            acct.state = AccountState::Active;
            acct.available_at = None;
        }

        // Disabled had no exit at all: nothing else in the codebase leaves it, a
        // restart restores it from disk, and no CLI command clears it, so one
        // transient 401 removed the account for good. The operator's only escape
        // was deleting the ledger, which makes the gateway re-spend quota it
        // already consumed.
        //
        // The exit is deliberately narrower than "the probe answered": that would
        // put an account disabled for insufficient credit back into rotation once
        // per poll, where it would fail with 402 again every time. Coming back
        // requires the probe to show it can actually pay.
        if acct.state == AccountState::Disabled && usable && !disabled_by_operator {
            acct.state = AccountState::Active;
            acct.available_at = None;
        }
        if disabled_by_operator {
            // Put the reason back: the probe does not get to overwrite why the
            // operator took this account out.
            acct.last_error = Some(OPERATOR_DISABLED.to_string());
        }

        // Reflect the soft threshold against freshly probed numbers, in BOTH
        // directions.
        //
        // The way back out matters as much as the way in. Draining used to be a
        // one-way door: it was set here and in settle() when the threshold was
        // crossed, and nothing ever cleared it -- not the poller, not a probe
        // showing the window rolled back to zero -- while every save persisted
        // the stale state. New sessions then stayed off an account that had
        // full headroom again for the life of the process, and the board
        // reported DRAINING the whole time. Only a restart cleared it, because
        // restore() maps Draining to Unknown and the startup probe settles the
        // question again. Headroom decides both directions: the same reading
        // that retires an account has to be able to bring it back once the
        // reading itself says the window is whole.
        //
        // A reading with no account-level covered gate at all (None) changes
        // nothing, exactly as before: absence of evidence is not evidence the
        // window came back.
        if let Some(u) = acct.peak_covered_utilization() {
            acct.state = match acct.state {
                AccountState::Active if u >= soft => AccountState::Draining,
                AccountState::Draining if u < soft => AccountState::Active,
                other => other,
            };
        }
    }

    /// Mark a probe as failed: keep the last reading and flag it stale.
    ///
    /// Carrying the reading forward is important. Writing zero on failure would
    /// both overstate availability and make the CLI claim an empty account.
    pub fn mark_stale(&self, qualified_id: &str, reason: &str) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(acct) = find_mut(&mut inner.accounts, qualified_id) {
            for g in acct.gates.iter_mut() {
                g.stale = true;
            }
            acct.last_error = Some(format!("probe failed: {reason}"));
        }
    }

    pub fn snapshot(&self) -> Vec<Account> {
        self.inner.lock().unwrap().accounts.clone()
    }

    /// Look up by canonical provider/id identity.
    pub fn account(&self, qualified_id: &str) -> Option<Account> {
        let inner = self.inner.lock().unwrap();
        find(&inner.accounts, qualified_id).cloned()
    }

    pub fn providers(&self) -> Vec<ProviderId> {
        let inner = self.inner.lock().unwrap();
        let mut v: Vec<ProviderId> = inner
            .accounts
            .iter()
            .map(|a| a.provider.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        v.sort();
        v
    }

    pub fn accounts_of(&self, provider: &str) -> Vec<Account> {
        self.inner
            .lock()
            .unwrap()
            .accounts
            .iter()
            .filter(|a| a.provider == provider)
            .cloned()
            .collect()
    }

    /// How long until some account could serve this provider again.
    pub fn retry_after_for(&self, provider: &str) -> Option<Duration> {
        let at = now();
        let inner = self.inner.lock().unwrap();
        let mut best: Option<OffsetDateTime> = None;
        for a in inner.accounts.iter().filter(|a| a.provider == provider) {
            if let Some(w) = a.available_at {
                best = Some(match best {
                    Some(b) if b <= w => b,
                    _ => w,
                });
            }
        }
        best.map(|w| {
            let d = w - at;
            if d.is_negative() {
                Duration::ZERO
            } else {
                d.unsigned_abs()
            }
        })
    }

    /// Replace the sticky binding for a session, used when a switch happens.
    pub fn rebind(&self, session_key: &str, qualified_id: &str) {
        self.inner
            .lock()
            .unwrap()
            .sticky
            .insert(session_key.to_string(), qualified_id.to_string());
    }

    /// Restore persisted accounts into the pool. Freshly restored accounts are
    /// forced back to Unknown so nothing serves traffic until it has been
    /// probed: resuming with a stale ledger is how a gateway silently spends.
    pub fn restore(&self, restored: Vec<crate::domain::AccountOverview>) {
        let mut inner = self.inner.lock().unwrap();
        for r in restored {
            let qualified = crate::domain::qualified_id(&r.provider, &r.account_id);
            if let Some(a) = find_mut(&mut inner.accounts, &qualified) {
                a.gates = r.gates;
                a.available_at = r.available_at;
                a.window_started_at = r.window_started_at;
                // The reason travels with the state. Dropping it here lost the
                // board's explanation of why an account is out, and it lost the
                // record that an operator -- not a failure -- put it there, so
                // the next successful probe quietly put it back in the pool.
                a.last_error = r.last_error;
                a.state = match r.state {
                    AccountState::Active | AccountState::Draining => AccountState::Unknown,
                    other => other,
                };
            }
        }
    }
}

/// Build a gate from a probe reading, normalising the common shapes.
pub fn gate(
    kind: GateKind,
    cap: Decimal,
    used: Decimal,
    resets_at: Option<OffsetDateTime>,
) -> Gate {
    Gate {
        kind,
        cap,
        used,
        resets_at,
        confidence: Confidence::Exact,
        source: GateSource::Polled,
        checked_at: now(),
        stale: false,
    }
}

/// Utilisation as a display percentage.
pub fn pct(u: f64) -> String {
    format!("{:.0}%", (u * 100.0).clamp(0.0, 999.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PolicyConfig;
    use crate::domain::ApiKey;
    use rust_decimal::Decimal;

    fn acct(id: &str, provider: &str, gates: Vec<Gate>, state: AccountState) -> Account {
        Account {
            id: id.into(),
            provider: provider.into(),
            label: id.into(),
            base_url: "https://example.invalid/v1".into(),
            api_key: ApiKey::new("k"),
            currency: "USD".into(),
            gates,
            state,
            available_at: None,
            window_started_at: None,
            credit_multiplier: Decimal::ONE,
            rpm_limit: None,
            concurrency_limit: None,
            allow_metered: None,
            last_probe_at: None,
            last_error: None,
        }
    }

    fn cover(cap: i64, used: i64) -> Gate {
        gate(
            GateKind::FiveHour,
            Decimal::from(cap),
            Decimal::from(used),
            None,
        )
    }

    fn wallet(cap: i64, used: i64) -> Gate {
        gate(
            GateKind::Wallet,
            Decimal::from(cap),
            Decimal::from(used),
            None,
        )
    }

    fn policy() -> PolicyConfig {
        PolicyConfig::default()
    }

    #[test]
    fn retuning_changes_the_values_the_next_admission_reads() {
        let mut next = policy();
        next.soft_threshold = 0.42;
        next.allow_metered = true;
        let pool = Pool::new(Vec::new(), &next);
        assert_eq!(pool.soft_threshold(), 0.42);
        assert!(pool.allow_metered());

        next.soft_threshold = 0.31;
        next.allow_metered = false;
        pool.retune(&next);
        assert_eq!(pool.soft_threshold(), 0.31);
        assert!(!pool.allow_metered());
    }

    /// A free-token allowance is not covered money.
    ///
    /// It gates free models only and never pays for a request, so an account
    /// whose only allowance is that pot is refused for the same reason a
    /// wallet-only provider is: the pot that could have paid is metered, and
    /// spending it is off. Before the class split this account reported
    /// has_covered() == true and was refused as "headroom below the estimate",
    /// naming a covered window that does not exist.
    #[test]
    fn reconfiguring_keeps_readings_sticky_sessions_and_commitments() {
        let pool = Pool::new(
            vec![acct(
                "a",
                "p",
                vec![gate(
                    GateKind::FiveHour,
                    Decimal::from(10),
                    Decimal::from(4),
                    None,
                )],
                AccountState::Active,
            )],
            &policy(),
        );
        let first = pool
            .select("p", "", Some("s1"), Decimal::ONE, "r1", 0, &[])
            .expect("one account is eligible");
        assert_eq!(first.plan().account_id, "a");
        first.settle(Decimal::ZERO, false);

        let mut fresh = acct("a", "p", Vec::new(), AccountState::Active);
        fresh.label = "renamed".to_string();
        fresh.currency = "CNY".to_string();
        fresh.credit_multiplier = Decimal::from(2);
        fresh.rpm_limit = Some(7);
        fresh.concurrency_limit = Some(2);
        fresh.allow_metered = Some(true);
        let outcome = pool.reconfigure(vec![fresh], &[]);
        assert!(outcome.added.is_empty());
        assert!(outcome.reset.is_empty());
        assert!(outcome.retired.is_empty());

        let snapshot = pool.snapshot();
        let kept = snapshot.iter().find(|a| a.id == "a").expect("account kept");
        assert_eq!(kept.label, "renamed");
        assert_eq!(kept.currency, "CNY");
        assert_eq!(kept.credit_multiplier, Decimal::from(2));
        assert_eq!(kept.rpm_limit, Some(7));
        assert_eq!(kept.concurrency_limit, Some(2));
        assert_eq!(kept.allow_metered, Some(true));
        assert_eq!(kept.state, AccountState::Active);
        assert_eq!(kept.gates.len(), 1, "readings survive an apply");
        assert_eq!(kept.gates[0].used, Decimal::from(4));

        let again = pool
            .select("p", "", Some("s1"), Decimal::ONE, "r2", 0, &[])
            .expect("the sticky session still has an account");
        assert_eq!(again.plan().account_id, "a", "sticky binding survives");
        again.settle(Decimal::ZERO, false);
    }

    #[test]
    fn a_new_account_is_unknown_and_serves_nothing_until_probed() {
        let pool = Pool::new(
            vec![acct(
                "a",
                "p",
                vec![gate(
                    GateKind::FiveHour,
                    Decimal::from(10),
                    Decimal::from(1),
                    None,
                )],
                AccountState::Active,
            )],
            &policy(),
        );
        let outcome = pool.reconfigure(
            vec![
                acct("a", "p", Vec::new(), AccountState::Active),
                acct("b", "p", Vec::new(), AccountState::Active),
            ],
            &[],
        );
        assert_eq!(outcome.added, vec!["p/b".to_string()]);
        assert!(outcome.probe.contains(&("p".to_string(), "b".to_string())));

        let new = pool
            .snapshot()
            .into_iter()
            .find(|a| a.id == "b")
            .expect("the new account is in the pool");
        assert_eq!(new.state, AccountState::Unknown);
        assert!(new.gates.is_empty());

        let chosen = pool
            .select("p", "", None, Decimal::ONE, "r1", 0, &[])
            .expect("the known account serves");
        assert_eq!(
            chosen.plan().account_id,
            "a",
            "the unknown account is skipped"
        );
        chosen.settle(Decimal::ZERO, false);

        pool.apply_probe(
            "p/b",
            vec![gate(
                GateKind::FiveHour,
                Decimal::from(10),
                Decimal::from(1),
                None,
            )],
            now(),
        );
        let probed = pool.snapshot().into_iter().find(|a| a.id == "b").unwrap();
        assert_eq!(probed.state, AccountState::Active);
        assert_eq!(probed.gates.len(), 1);
    }

    #[test]
    fn a_removed_account_retires_after_its_in_flight_attempt_settles() {
        let pool = Pool::new(
            vec![
                acct(
                    "a",
                    "p",
                    vec![gate(
                        GateKind::FiveHour,
                        Decimal::from(10),
                        Decimal::from(1),
                        None,
                    )],
                    AccountState::Active,
                ),
                acct(
                    "b",
                    "p",
                    vec![gate(
                        GateKind::FiveHour,
                        Decimal::from(10),
                        Decimal::from(1),
                        None,
                    )],
                    AccountState::Active,
                ),
            ],
            &policy(),
        );
        let reservation = pool
            .select("p", "", None, Decimal::ONE, "r1", 0, &[])
            .expect("eligible");
        let victim = reservation.qualified_account();
        let (victim_id, keeper) = if victim == "p/a" {
            ("a", "b")
        } else {
            ("b", "a")
        };
        let keep_fresh = acct(keeper, "p", Vec::new(), AccountState::Active);

        let outcome = pool.reconfigure(vec![keep_fresh.clone()], &[]);
        assert_eq!(outcome.retired, vec![victim.clone()]);
        let retired = pool
            .snapshot()
            .into_iter()
            .find(|a| a.id == victim_id)
            .expect("the retired account stays until it settles");
        assert_eq!(retired.state, AccountState::Disabled);
        assert_eq!(retired.last_error.as_deref(), Some(RETIRED_BY_APPLY));

        reservation.settle(Decimal::ZERO, false);
        assert_eq!(pool.inflight_count(&victim), 0);

        let cleanup = pool.reconfigure(vec![keep_fresh], &[]);
        assert!(cleanup.dropped.contains(&victim));
        assert!(
            pool.snapshot().iter().all(|a| a.id != victim_id),
            "a settled retired account is dropped from the pool"
        );
    }

    #[test]
    fn a_changed_provider_binding_resets_its_accounts_to_unknown() {
        let pool = Pool::new(
            vec![acct(
                "a",
                "p",
                vec![gate(
                    GateKind::FiveHour,
                    Decimal::from(10),
                    Decimal::from(3),
                    None,
                )],
                AccountState::Active,
            )],
            &policy(),
        );
        let outcome = pool.reconfigure(
            vec![acct("a", "p", Vec::new(), AccountState::Active)],
            &["p".to_string()],
        );
        assert_eq!(outcome.reset, vec!["p/a".to_string()]);
        assert!(outcome.probe.contains(&("p".to_string(), "a".to_string())));
        let account = &pool.snapshot()[0];
        assert_eq!(account.state, AccountState::Unknown);
        assert!(
            account.gates.is_empty(),
            "a reading from the old endpoint is worthless"
        );
    }

    #[test]
    fn an_operator_disable_is_not_undone_by_a_probe_and_enable_returns_unknown() {
        let pool = Pool::new(
            vec![acct(
                "a",
                "p",
                vec![gate(
                    GateKind::FiveHour,
                    Decimal::from(10),
                    Decimal::from(1),
                    None,
                )],
                AccountState::Active,
            )],
            &policy(),
        );
        let disabled = pool
            .set_operator_state("p/a", false)
            .expect("disable is allowed");
        assert_eq!(disabled.state, AccountState::Disabled);
        assert_eq!(disabled.last_error.as_deref(), Some(OPERATOR_DISABLED));

        // A successful probe refreshes the reading but must not lift the
        // operator's decision.
        pool.apply_probe(
            "p/a",
            vec![gate(
                GateKind::FiveHour,
                Decimal::from(10),
                Decimal::from(0),
                None,
            )],
            now(),
        );
        let still = pool.account("p/a").expect("account");
        assert_eq!(still.state, AccountState::Disabled);
        assert_eq!(still.last_error.as_deref(), Some(OPERATOR_DISABLED));

        let enabled = pool.set_operator_state("p/a", true).expect("enable");
        assert_eq!(enabled.state, AccountState::Unknown);
        assert_eq!(enabled.last_error, None);
        assert_eq!(enabled.last_probe_at, None);

        pool.apply_probe(
            "p/a",
            vec![gate(
                GateKind::FiveHour,
                Decimal::from(10),
                Decimal::from(0),
                None,
            )],
            now(),
        );
        assert_eq!(pool.account("p/a").unwrap().state, AccountState::Active);
    }

    #[test]
    fn enabling_an_account_that_is_not_disabled_is_refused() {
        let pool = Pool::new(
            vec![acct(
                "a",
                "p",
                vec![gate(
                    GateKind::FiveHour,
                    Decimal::from(10),
                    Decimal::from(1),
                    None,
                )],
                AccountState::Active,
            )],
            &policy(),
        );
        assert!(pool.set_operator_state("p/a", true).is_err());
        assert!(pool.set_operator_state("p/nope", false).is_err());
    }

    #[test]
    fn a_free_allowance_alone_is_not_covered_money() {
        let p = Pool::new(
            vec![acct(
                "a",
                "xkiro",
                vec![gate(
                    GateKind::FreeTokens,
                    Decimal::from(1_000),
                    Decimal::ZERO,
                    None,
                )],
                AccountState::Active,
            )],
            &policy(),
        );
        let only = p.account("xkiro/a").unwrap();
        assert_eq!(
            only.covered_gates().count(),
            0,
            "an allowance is not covered"
        );
        assert_eq!(
            p.evaluate(&only, Decimal::ONE, Decimal::ZERO),
            GateDecision::Reject(RejectReason::MeteredNotAllowed)
        );
    }

    /// A reservation holds its commitment, and gives it back exactly once --
    /// whether it settles or is dropped.
    ///
    /// This is the invariant a hand-written `lease.settled()` at each exit could
    /// not hold: a path that forgot one left the estimate committed for the life
    /// of the process, and the in-flight term kept shrinking that account's
    /// headroom until nothing was eligible and only a restart could clear it.
    #[test]
    fn a_reservation_returns_its_commitment_exactly_once() {
        let p = Pool::new(
            vec![acct("a", "p", vec![cover(100, 0)], AccountState::Active)],
            &policy(),
        );

        let held = p
            .select("p", "m", None, Decimal::from(90), "r1", 1, &[])
            .unwrap();
        assert_eq!(p.inflight_count("p/a"), 1, "a live reservation holds one");
        assert!(
            p.select("p", "m", None, Decimal::from(90), "r2", 1, &[])
                .is_err(),
            "headroom a live reservation holds is not available to the next request"
        );

        // Abandoned without settling: the client hung up, or an early return.
        drop(held);
        assert_eq!(
            p.inflight_count("p/a"),
            0,
            "dropping releases the commitment"
        );

        // Settled: the same single release, not a second one.
        let settled = p
            .select("p", "m", None, Decimal::from(90), "r3", 1, &[])
            .unwrap();
        settled.settle(Decimal::from(1), true);
        assert_eq!(
            p.inflight_count("p/a"),
            0,
            "settling releases the commitment"
        );

        assert!(
            p.select("p", "m", None, Decimal::from(90), "r4", 1, &[])
                .is_ok(),
            "both commitments came back, so the pool is whole again"
        );
    }

    /// Regression, found against a real configuration holding both
    /// xkiro/acc-1 and commandcode/acc-1.
    ///
    /// Account ids are unique only within a provider. Keying pool state by the
    /// bare id made both quota probes write onto whichever account sorted first,
    /// so one account displayed the other's allowances and the other stayed
    /// unprobed. Identity is (provider, id) everywhere for exactly this reason.
    #[test]
    fn two_providers_may_share_an_account_id_without_colliding() {
        let p = Pool::new(
            vec![
                acct("acc-1", "xkiro", vec![cover(200, 20)], AccountState::Active),
                acct(
                    "acc-1",
                    "commandcode",
                    vec![cover(14, 1)],
                    AccountState::Active,
                ),
            ],
            &policy(),
        );

        // A probe for one provider must land only on that provider's account.
        p.apply_probe("xkiro/acc-1", vec![cover(200, 55)], crate::domain::now());
        assert_eq!(
            p.account("xkiro/acc-1").unwrap().gates[0].used,
            Decimal::from(55)
        );
        assert_eq!(
            p.account("commandcode/acc-1").unwrap().gates[0].used,
            Decimal::from(1),
            "the other provider's account must be untouched"
        );

        // Selection is scoped to the requested provider.
        let xk = p
            .select("xkiro", "m", None, Decimal::ONE, "r", 1, &[])
            .unwrap();
        assert_eq!(xk.account_id, "acc-1");
        assert_eq!(xk.qualified_account(), "xkiro/acc-1");
        let cc = p
            .select("commandcode", "m", None, Decimal::ONE, "r", 1, &[])
            .unwrap();
        assert_eq!(cc.qualified_account(), "commandcode/acc-1");

        // An error on one must not park the other.
        p.note_error(
            "xkiro/acc-1",
            UpstreamErrorClass::QuotaExhausted,
            None,
            None,
        );
        assert_eq!(
            p.account("xkiro/acc-1").unwrap().state,
            AccountState::Parked
        );
        assert_eq!(
            p.account("commandcode/acc-1").unwrap().state,
            AccountState::Active
        );
    }

    /// A refusal must carry the arithmetic that produced it.
    ///
    /// Addressed against a live gateway: a 1.2 MB request was refused with a
    /// bare "no eligible account" while the account still showed 12.67 USD of
    /// headroom. Nothing in that message said whether the allowance was spent,
    /// the request was priced above it, or the model had no price at all -- and
    /// those three need three different answers.
    #[test]
    fn a_refusal_names_the_estimate_and_the_account_that_came_closest() {
        let p = Pool::new(
            vec![acct(
                "xk-1",
                "xkiro",
                vec![cover(20, 7)],
                AccountState::Active,
            )],
            &policy(),
        );

        let err = p
            .select("xkiro", "m", None, Decimal::from(40), "r", 1, &[])
            .expect_err("an estimate above the headroom must be refused");
        let msg = err.to_string();

        assert!(msg.contains("estimated at 40"), "{msg}");
        assert!(msg.contains("xkiro/xk-1"), "{msg}");
        assert!(msg.contains("13 left"), "20 - 7 = 13: {msg}");
        assert!(msg.contains("headroom below the estimate"), "{msg}");
    }

    /// A refusal with nothing to compare against must still not read as if the
    /// account had room: no candidate means no claim about headroom.
    #[test]
    fn a_refusal_without_any_headroom_claims_nothing() {
        let p = Pool::new(
            vec![acct("xk-1", "xkiro", vec![], AccountState::Active)],
            &policy(),
        );
        let err = p
            .select("xkiro", "m", None, Decimal::ONE, "r", 1, &[])
            .expect_err("an account with no gates cannot serve");
        let msg = err.to_string();
        assert!(
            msg.starts_with("no eligible account for provider xkiro"),
            "{msg}"
        );
        assert!(
            !msg.contains("most room"),
            "no headroom was measured: {msg}"
        );
    }

    /// The free-token allowance must not gate paid traffic.
    ///
    /// Free models are not routed through this gateway, so an allowance nobody
    /// spends must not be able to retire an account that still has plenty of
    /// paid quota. Before this, a daily free allowance at 99% pushed the account
    /// past the soft threshold and stopped new sessions on models that never
    /// touch it -- an outage caused by a resource that was not being used.
    #[test]
    fn a_nearly_spent_free_allowance_does_not_retire_the_account() {
        let p = Pool::new(
            vec![acct(
                "xk-1",
                "xkiro",
                vec![cover(200, 20)],
                AccountState::Active,
            )],
            &policy(),
        );

        p.apply_probe(
            "xkiro/xk-1",
            vec![
                cover(200, 30),
                gate(
                    GateKind::FreeTokens,
                    Decimal::from(30_000_000u64),
                    Decimal::from(29_700_000u64),
                    None,
                ),
            ],
            crate::domain::now(),
        );

        let a = p.account("xkiro/xk-1").unwrap();
        let u = a.peak_covered_utilization().unwrap();
        assert!(
            (u - 0.15).abs() < 1e-9,
            "the threshold must read the paid window, not the free allowance: {u}"
        );
        assert_eq!(
            a.state,
            AccountState::Active,
            "99% of a free allowance must not drain an account at 15% of paid quota"
        );
    }

    /// The same allowance must not skew consume-first ordering either, or the
    /// account with the biggest free bucket wins every request.
    #[test]
    fn a_free_allowance_does_not_skew_consume_first_ordering() {
        let free = |cap: i64, used: i64| {
            gate(
                GateKind::FreeTokens,
                Decimal::from(cap),
                Decimal::from(used),
                None,
            )
        };
        let p = Pool::new(
            vec![
                acct(
                    "big-free",
                    "xkiro",
                    vec![cover(100, 10), free(30_000_000, 0)],
                    AccountState::Active,
                ),
                acct(
                    "no-free",
                    "xkiro",
                    vec![cover(100, 60)],
                    AccountState::Active,
                ),
            ],
            &policy(),
        );

        // Consume-first picks the MOST utilised account, and now only the paid
        // windows count: 60/100 beats 10/100 regardless of the free bucket.
        let plan = p
            .select("xkiro", "m", None, Decimal::ONE, "r", 1, &[])
            .unwrap();
        assert_eq!(plan.qualified_account(), "xkiro/no-free");
    }

    #[test]
    fn a_sticky_session_is_bound_to_one_provider_account() {
        let p = Pool::new(
            vec![
                acct("acc-1", "xkiro", vec![cover(200, 20)], AccountState::Active),
                acct(
                    "acc-1",
                    "commandcode",
                    vec![cover(14, 1)],
                    AccountState::Active,
                ),
            ],
            &policy(),
        );
        let first = p
            .select("xkiro", "m", Some("sess"), Decimal::ONE, "r", 1, &[])
            .unwrap();
        assert_eq!(first.qualified_account(), "xkiro/acc-1");
        // Same session key, different provider: must not follow the binding.
        let other = p
            .select("commandcode", "m", Some("sess"), Decimal::ONE, "r", 1, &[])
            .unwrap();
        assert_eq!(other.qualified_account(), "commandcode/acc-1");
    }

    #[test]
    fn unknown_accounts_never_serve() {
        let p = Pool::new(
            vec![acct("a", "p", vec![cover(100, 0)], AccountState::Unknown)],
            &policy(),
        );
        assert!(p.select("p", "m", None, Decimal::ONE, "r", 1, &[]).is_err());
    }

    #[test]
    fn hard_gate_rejects_when_headroom_below_cost() {
        let p = Pool::new(
            vec![acct("a", "p", vec![cover(100, 99)], AccountState::Active)],
            &policy(),
        );
        // 2.0 of cost against 1.0 of headroom must be refused...
        assert!(
            p.select("p", "m", None, Decimal::from(2), "r", 1, &[])
                .is_err()
        );
        // ...while a cost that exactly fits is still affordable.
        assert!(
            p.select("p", "m", None, Decimal::from(1), "r", 1, &[])
                .is_ok()
        );
    }

    #[test]
    fn consume_first_picks_the_most_utilised_below_soft() {
        let p = Pool::new(
            vec![
                acct("low", "p", vec![cover(100, 10)], AccountState::Active),
                acct("high", "p", vec![cover(100, 70)], AccountState::Active),
            ],
            &policy(),
        );
        let plan = p.select("p", "m", None, Decimal::ONE, "r", 1, &[]).unwrap();
        assert_eq!(
            plan.account_id, "high",
            "consume-first should drain one account at a time"
        );
        assert_eq!(plan.coverage, GateClass::Covered);
    }

    #[test]
    fn a_spent_window_reports_when_it_will_roll_over() {
        // The account is not Parked, but its covered window is gone. A client
        // that gets a 429 must be told when to come back.
        let reset = now() + time::Duration::seconds(1800);
        let g = gate(
            GateKind::FiveHour,
            Decimal::from(200),
            Decimal::from(200),
            Some(reset),
        );
        let p = Pool::new(
            vec![acct("a", "p", vec![g], AccountState::Active)],
            &policy(),
        );
        let err = p
            .select("p", "m", None, Decimal::ONE, "r", 1, &[])
            .expect_err("a spent window must not be selected");
        match err {
            PlanError::NoEligibleAccount { retry_after, .. } => {
                let secs = retry_after
                    .expect("spent window must yield a retry hint")
                    .as_secs();
                assert!(
                    (1700..=1800).contains(&secs),
                    "retry hint should point at the window reset, got {secs}s"
                );
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn above_soft_everywhere_falls_back_to_least_utilised() {
        let p = Pool::new(
            vec![
                acct("a", "p", vec![cover(100, 90)], AccountState::Active),
                acct("b", "p", vec![cover(100, 96)], AccountState::Active),
            ],
            &policy(),
        );
        let plan = p.select("p", "m", None, Decimal::ONE, "r", 1, &[]).unwrap();
        assert_eq!(
            plan.account_id, "a",
            "the soft threshold must not become a hard outage while headroom remains"
        );
    }

    #[test]
    fn sticky_session_stays_put_even_when_another_is_emptier() {
        let p = Pool::new(
            vec![
                acct("a", "p", vec![cover(100, 70)], AccountState::Active),
                acct("b", "p", vec![cover(100, 5)], AccountState::Active),
            ],
            &policy(),
        );
        let first = p
            .select("p", "m", Some("sess"), Decimal::ONE, "r", 1, &[])
            .unwrap();
        assert_eq!(first.account_id, "a");
        // Account a is now the busiest, but the session must not move: moving
        // it would discard the prompt cache.
        let second = p
            .select("p", "m", Some("sess"), Decimal::ONE, "r2", 1, &[])
            .unwrap();
        assert_eq!(second.account_id, "a");
    }

    #[test]
    fn metered_is_refused_by_default_even_with_balance() {
        // Pure wallet provider with plenty of money, but allow_metered is false.
        let p = Pool::new(
            vec![acct(
                "d",
                "deepseek",
                vec![wallet(100, 0)],
                AccountState::Active,
            )],
            &policy(),
        );
        assert!(
            p.select("deepseek", "m", None, Decimal::ONE, "r", 1, &[])
                .is_err()
        );
    }

    #[test]
    fn metered_used_only_when_policy_allows() {
        let mut pol = policy();
        pol.allow_metered = true;
        let p = Pool::new(
            vec![acct(
                "d",
                "deepseek",
                vec![wallet(100, 0)],
                AccountState::Active,
            )],
            &pol,
        );
        let plan = p
            .select("deepseek", "m", None, Decimal::ONE, "r", 1, &[])
            .unwrap();
        assert_eq!(plan.coverage, GateClass::Metered);
    }

    #[test]
    fn covered_wins_over_metered_while_any_covered_headroom_remains() {
        let mut pol = policy();
        pol.allow_metered = true;
        let p = Pool::new(
            vec![
                acct(
                    "cc",
                    "p",
                    vec![cover(100, 70), wallet(100, 0)],
                    AccountState::Active,
                ),
                acct(
                    "walletonly",
                    "p",
                    vec![wallet(100, 0)],
                    AccountState::Active,
                ),
            ],
            &pol,
        );
        let plan = p.select("p", "m", None, Decimal::ONE, "r", 1, &[]).unwrap();
        assert_eq!(plan.account_id, "cc");
        assert_eq!(plan.coverage, GateClass::Covered);
    }

    #[test]
    fn settling_charges_the_covered_window() {
        let p = Pool::new(
            vec![acct("a", "p", vec![cover(100, 0)], AccountState::Active)],
            &policy(),
        );
        let plan = p.select("p", "m", None, Decimal::ONE, "r", 1, &[]).unwrap();
        p.settle(&plan, Decimal::from(5), true);
        let a = p.account("p/a").unwrap();
        assert_eq!(a.gates[0].used, Decimal::from(5));
    }

    #[test]
    fn crossing_soft_threshold_retires_from_new_sessions() {
        let p = Pool::new(
            vec![acct("a", "p", vec![cover(100, 0)], AccountState::Active)],
            &policy(),
        );
        let plan = p.select("p", "m", None, Decimal::ONE, "r", 1, &[]).unwrap();
        p.settle(&plan, Decimal::from(85), true);
        assert_eq!(p.account("p/a").unwrap().state, AccountState::Draining);
    }

    /// A window that rolls over must put the account back in rotation.
    ///
    /// Draining used to be a one-way door. settle() and apply_probe() set it
    /// when the soft threshold was crossed and nothing ever cleared it -- not
    /// the poller, not a probe showing the window whole again -- while every
    /// save persisted the stale state. The board then reported DRAINING for an
    /// account with full headroom, and the account stayed out of consume-first
    /// ordering for the life of the process: only a restart cleared it, because
    /// restore() maps Draining to Unknown and the startup probe settles it.
    #[test]
    fn a_probe_that_shows_the_window_reset_clears_draining() {
        let p = Pool::new(
            vec![acct("a", "p", vec![cover(100, 95)], AccountState::Draining)],
            &policy(),
        );

        // The window rolled over: the provider now reports it whole.
        p.apply_probe("p/a", vec![cover(100, 0)], now());
        assert_eq!(
            p.account("p/a").unwrap().state,
            AccountState::Active,
            "an account whose window just reset is not draining anything"
        );

        // The way in must keep working, or the reset would be a one-way door
        // in the other direction.
        p.apply_probe("p/a", vec![cover(100, 90)], now());
        assert_eq!(p.account("p/a").unwrap().state, AccountState::Draining);
    }

    /// Draining is a preference about where new sessions go, never a refusal.
    ///
    /// The preference never surfaces as an error: a draining account is simply
    /// chosen last. A change that "cleared" Draining by refusing instead would
    /// turn a routing preference into an outage, and this is the test that says
    /// so.
    #[test]
    fn a_draining_account_yields_to_one_that_is_still_filling_up() {
        let p = Pool::new(
            vec![
                acct("drained", "p", vec![cover(100, 70)], AccountState::Draining),
                acct("filling", "p", vec![cover(100, 20)], AccountState::Active),
            ],
            &policy(),
        );
        let plan = p.select("p", "m", None, Decimal::ONE, "r", 1, &[]).unwrap();
        assert_eq!(
            plan.account_id, "filling",
            "consume-first should pass over the draining account for now"
        );

        // It still serves when it is the only account left standing.
        let only = Pool::new(
            vec![acct(
                "drained",
                "p",
                vec![cover(100, 70)],
                AccountState::Draining,
            )],
            &policy(),
        );
        let plan = only
            .select("p", "m", None, Decimal::ONE, "r", 1, &[])
            .unwrap();
        assert_eq!(
            plan.account_id, "drained",
            "draining must never become a hard outage"
        );
    }

    #[test]
    fn quota_exhaustion_parks_and_clears_sticky() {
        let p = Pool::new(
            vec![acct("a", "p", vec![cover(100, 0)], AccountState::Active)],
            &policy(),
        );
        let _ = p
            .select("p", "m", Some("s"), Decimal::ONE, "r", 1, &[])
            .unwrap();
        p.note_error("p/a", UpstreamErrorClass::QuotaExhausted, None, None);
        let a = p.account("p/a").unwrap();
        assert_eq!(a.state, AccountState::Parked);
        assert!(
            p.select("p", "m", Some("s"), Decimal::ONE, "r2", 1, &[])
                .is_err()
        );
    }

    #[test]
    fn credit_exhaustion_disables_the_account() {
        let p = Pool::new(
            vec![acct("a", "p", vec![cover(100, 0)], AccountState::Active)],
            &policy(),
        );
        p.note_error("p/a", UpstreamErrorClass::InsufficientCredit, None, None);
        assert_eq!(p.account("p/a").unwrap().state, AccountState::Disabled);
    }

    #[test]
    fn burst_rate_limit_does_not_park_the_account() {
        let p = Pool::new(
            vec![acct("a", "p", vec![cover(100, 0)], AccountState::Active)],
            &policy(),
        );
        p.note_error(
            "p/a",
            UpstreamErrorClass::RateLimitBurst,
            Some(Duration::from_secs(3)),
            None,
        );
        assert_eq!(p.account("p/a").unwrap().state, AccountState::Active);
    }

    #[test]
    fn probe_failure_keeps_the_last_reading_and_marks_it_stale() {
        let p = Pool::new(
            vec![acct("a", "p", vec![cover(100, 40)], AccountState::Active)],
            &policy(),
        );
        p.mark_stale("p/a", "timeout");
        let a = p.account("p/a").unwrap();
        assert_eq!(
            a.gates[0].used,
            Decimal::from(40),
            "a failed probe must not reset the reading to zero"
        );
        assert!(a.gates[0].stale);
    }

    #[test]
    fn restore_never_trusts_a_persisted_active_state() {
        let p = Pool::new(
            vec![acct("a", "p", vec![cover(100, 0)], AccountState::Unknown)],
            &policy(),
        );
        let a = acct("a", "p", vec![cover(100, 10)], AccountState::Active);
        p.restore(vec![crate::domain::AccountOverview {
            account_id: a.id.clone(),
            provider: a.provider.clone(),
            label: a.label.clone(),
            currency: a.currency.clone(),
            state: a.state,
            gates: a.gates.clone(),
            available_at: a.available_at,
            window_started_at: a.window_started_at,
            last_error: a.last_error.clone(),
        }]);
        assert_eq!(p.account("p/a").unwrap().state, AccountState::Unknown);
        assert_eq!(p.account("p/a").unwrap().gates[0].used, Decimal::from(10));
    }

    #[test]
    fn parked_account_returns_after_its_reset() {
        let past = now() - time::Duration::seconds(60);
        let mut a = acct(
            "a",
            "p",
            vec![gate(
                GateKind::FiveHour,
                Decimal::from(100),
                Decimal::from(100),
                Some(past),
            )],
            AccountState::Parked,
        );
        a.available_at = Some(past);
        let p = Pool::new(vec![a], &policy());
        let plan = p.select("p", "m", None, Decimal::ONE, "r", 1, &[]).unwrap();
        assert_eq!(plan.account_id, "a");
        let a = p.account("p/a").unwrap();
        assert_eq!(a.state, AccountState::Active);
        assert_eq!(
            a.gates[0].used,
            Decimal::ZERO,
            "a rolled window starts whole again"
        );
    }

    /// One transient 401 used to remove an account for good.
    ///
    /// Disabled had no exit at all: apply_probe revived only Unknown and Parked,
    /// a restart restored Disabled from disk, and no CLI command could clear it.
    /// Fixing the credential and probing again changed nothing, so the operator's
    /// only escape was deleting the ledger, which makes the gateway re-spend
    /// quota it already consumed.
    #[test]
    fn a_successful_probe_brings_a_disabled_account_back() {
        let p = Pool::new(
            vec![acct("a", "p", vec![cover(100, 0)], AccountState::Disabled)],
            &policy(),
        );
        assert!(
            p.select("p", "m", None, Decimal::ONE, "r", 1, &[]).is_err(),
            "a disabled account must not serve"
        );

        p.apply_probe("p/a", vec![cover(100, 0)], now());

        assert_eq!(p.account("p/a").unwrap().state, AccountState::Active);
        assert!(
            p.select("p", "m", None, Decimal::ONE, "r", 2, &[]).is_ok(),
            "a probe that succeeds is proof the account works again"
        );
    }

    /// The gate has to see the requests already on the wire.
    ///
    /// The in-flight commitment was written and never read, and the reserve step
    /// had no production caller, so concurrent requests each sized the gate as if
    /// they were alone and could together commit many times the real headroom.
    #[test]
    fn requests_in_flight_shrink_what_the_next_one_may_commit() {
        let p = Pool::new(
            vec![acct("a", "p", vec![cover(10, 0)], AccountState::Active)],
            &policy(),
        );
        let first = p
            .select("p", "m", None, Decimal::from(4), "r1", 1, &[])
            .unwrap();
        let second = p
            .select("p", "m", None, Decimal::from(4), "r2", 1, &[])
            .unwrap();
        assert!(
            p.select("p", "m", None, Decimal::from(4), "r3", 1, &[])
                .is_err(),
            "8 of 10 already committed, so another 4 cannot fit"
        );

        p.settle(&first, Decimal::from(4), true);
        p.settle(&second, Decimal::from(4), true);
        assert_eq!(p.account("p/a").unwrap().gates[0].used, Decimal::from(8));
        assert!(
            p.select("p", "m", None, Decimal::from(4), "r4", 1, &[])
                .is_err(),
            "and the money is spent now, not merely reserved"
        );
    }

    /// A retry must actually reach a different account.
    ///
    /// attempt_no was threaded all the way into select() and then only stored, so
    /// a 503 that does not change account state sent every retry straight back to
    /// the account that had just failed.
    #[test]
    fn a_retry_prefers_an_account_this_request_has_not_tried() {
        let p = Pool::new(
            vec![
                acct("a1", "p", vec![cover(100, 0)], AccountState::Active),
                acct("a2", "p", vec![cover(100, 0)], AccountState::Active),
            ],
            &policy(),
        );
        let first = p.select("p", "m", None, Decimal::ONE, "r", 1, &[]).unwrap();
        let second = p
            .select(
                "p",
                "m",
                None,
                Decimal::ONE,
                "r",
                2,
                &[first.qualified_account()],
            )
            .unwrap();
        assert_ne!(
            first.account_id, second.account_id,
            "the healthy account must be reached, not skipped"
        );
    }

    /// ... while a one-account pool keeps its bounded retry.
    #[test]
    fn a_retry_falls_back_to_the_only_account_when_there_is_no_alternative() {
        let p = Pool::new(
            vec![acct("a", "p", vec![cover(100, 0)], AccountState::Active)],
            &policy(),
        );
        let first = p.select("p", "m", None, Decimal::ONE, "r", 1, &[]).unwrap();
        let second = p
            .select(
                "p",
                "m",
                None,
                Decimal::ONE,
                "r",
                2,
                &[first.qualified_account()],
            )
            .unwrap();
        assert_eq!(first.account_id, second.account_id);
    }

    /// An unknown reset time is not evidence that a window rolled over.
    ///
    /// note_error marks EVERY window spent when an account is parked, so a
    /// saturated window with no reset time may be collateral. Clearing it
    /// claimed allowance the provider still enforces; leaving it costs one poll
    /// interval before the next probe replaces the whole reading.
    #[test]
    fn reaping_leaves_a_saturated_window_whose_reset_time_is_unknown() {
        let past = now() - time::Duration::seconds(60);
        let mut a = acct(
            "a",
            "p",
            vec![
                gate(
                    GateKind::FiveHour,
                    Decimal::from(100),
                    Decimal::from(100),
                    Some(past),
                ),
                gate(
                    GateKind::Weekly,
                    Decimal::from(100),
                    Decimal::from(100),
                    None,
                ),
            ],
            AccountState::Parked,
        );
        a.available_at = Some(past);
        let p = Pool::new(vec![a], &policy());
        let _ = p.select("p", "m", None, Decimal::ONE, "r", 1, &[]);

        let a = p.account("p/a").unwrap();
        let weekly = a.gates.iter().find(|g| g.kind == GateKind::Weekly).unwrap();
        assert_eq!(
            weekly.used,
            Decimal::from(100),
            "an unknown reset time must not be read as 'it rolled over'"
        );
    }

    /// A 5-hour reset must not wipe the weekly window that contains it.
    #[test]
    fn reaping_one_window_leaves_the_others_alone() {
        let past = now() - time::Duration::seconds(60);
        let soon = now() + time::Duration::seconds(3_600);
        let mut a = acct(
            "a",
            "p",
            vec![
                gate(
                    GateKind::FiveHour,
                    Decimal::from(100),
                    Decimal::from(100),
                    Some(past),
                ),
                gate(
                    GateKind::Weekly,
                    Decimal::from(100),
                    Decimal::from(95),
                    Some(soon),
                ),
            ],
            AccountState::Parked,
        );
        a.available_at = Some(past);
        let p = Pool::new(vec![a], &policy());
        let _ = p.select("p", "m", None, Decimal::ONE, "r", 1, &[]);

        let a = p.account("p/a").unwrap();
        let used = |kind: GateKind| {
            a.gates
                .iter()
                .find(|g| g.kind == kind)
                .map(|g| g.used)
                .unwrap()
        };
        assert_eq!(used(GateKind::FiveHour), Decimal::ZERO);
        assert_eq!(
            used(GateKind::Weekly),
            Decimal::from(95),
            "the weekly window has not reset, so its usage is still real"
        );
    }

    /// A token allowance is not a money gate.
    ///
    /// FreeTokens is covered by class but its cap counts tokens. Charging a
    /// dollar figure into it corrupted the counter, and once the money windows
    /// were exhausted it absorbed the whole cost before the metered gate -- the
    /// one that actually pays -- could be debited.
    #[test]
    fn a_free_token_allowance_is_never_charged_in_money() {
        let p = Pool::new(
            vec![acct(
                "a",
                "p",
                vec![
                    gate(GateKind::FiveHour, Decimal::from(1), Decimal::from(1), None),
                    gate(
                        GateKind::FreeTokens,
                        Decimal::from(10_000),
                        Decimal::from(1_200),
                        None,
                    ),
                    wallet(5, 0),
                ],
                AccountState::Active,
            )],
            &policy(),
        );
        let plan = AttemptPlan {
            request_id: "r".into(),
            account_id: "a".into(),
            provider: "p".into(),
            model: "m".into(),
            session_key: None,
            coverage: GateClass::Covered,
            tier: TierKey::Default,
            attempt_no: 1,
            estimated_cost: Decimal::ZERO,
        };
        p.settle(&plan, Decimal::from(3), true);

        let a = p.account("p/a").unwrap();
        let used = |kind: GateKind| {
            a.gates
                .iter()
                .find(|g| g.kind == kind)
                .map(|g| g.used)
                .unwrap()
        };
        assert_eq!(
            used(GateKind::FreeTokens),
            Decimal::from(1_200),
            "a dollar charge must never land in a token counter"
        );
        assert_eq!(
            used(GateKind::Wallet),
            Decimal::from(3),
            "the metered gate pays once the covered money is gone"
        );
    }

    /// ... but an account that is out of money stays out.
    ///
    /// Reviving on the mere fact that the probe answered would put a
    /// credit-exhausted account back into rotation once per poll, failing with
    /// 402 every time. A 200 on a balance endpoint does not mean there is money.
    #[test]
    fn a_probe_with_no_headroom_leaves_a_disabled_account_alone() {
        let p = Pool::new(
            vec![acct(
                "a",
                "p",
                vec![cover(100, 100), wallet(0, 0)],
                AccountState::Disabled,
            )],
            &policy(),
        );
        p.apply_probe("p/a", vec![cover(100, 100), wallet(0, 0)], now());
        assert_eq!(p.account("p/a").unwrap().state, AccountState::Disabled);
    }

    /// A wallet-only provider can be admitted without unlocking everyone else.
    ///
    /// The policy flag is all-or-nothing: turning it on to use one prepaid
    /// wallet also opens the overflow path for every other provider, Command
    /// Code's purchased credits among them. The per-provider override is what
    /// makes the narrow answer possible, so its wallet must be spendable with
    /// the policy off while a policy-governed wallet in the same pool is not.
    #[test]
    fn an_account_override_admits_its_wallet_while_the_policy_refuses_the_others() {
        let mut allowed = acct("acc-1", "haoai", vec![wallet(50, 0)], AccountState::Active);
        allowed.allow_metered = Some(true);
        let locked = acct(
            "acc-1",
            "commandcode",
            vec![wallet(50, 0)],
            AccountState::Active,
        );
        let p = Pool::new(vec![allowed, locked], &policy());
        assert!(!p.allow_metered(), "the policy itself stays off");

        let overridden = p.account("haoai/acc-1").unwrap();
        assert_eq!(
            p.evaluate(&overridden, Decimal::ONE, Decimal::ZERO),
            GateDecision::Metered
        );
        let governed = p.account("commandcode/acc-1").unwrap();
        assert_eq!(
            p.evaluate(&governed, Decimal::ONE, Decimal::ZERO),
            GateDecision::Reject(RejectReason::MeteredNotAllowed)
        );

        let plan = p
            .select("haoai", "m", None, Decimal::ONE, "r", 1, &[])
            .unwrap();
        assert_eq!(plan.coverage, GateClass::Metered);
        assert!(
            p.select("commandcode", "m", None, Decimal::ONE, "r", 1, &[])
                .is_err(),
            "one provider's override is not a global unlock"
        );
    }

    /// ... and the override can also say no: a provider's wallet stays shut
    /// while metered spending is allowed everywhere else.
    #[test]
    fn an_account_override_of_false_keeps_its_wallet_locked_while_the_policy_allows_it() {
        let mut locked = acct("acc-1", "haoai", vec![wallet(50, 0)], AccountState::Active);
        locked.allow_metered = Some(false);
        let mut pol = policy();
        pol.allow_metered = true;
        let p = Pool::new(vec![locked], &pol);

        let only = p.account("haoai/acc-1").unwrap();
        assert_eq!(
            p.evaluate(&only, Decimal::ONE, Decimal::ZERO),
            GateDecision::Reject(RejectReason::MeteredNotAllowed)
        );
        assert!(
            p.select("haoai", "m", None, Decimal::ONE, "r", 1, &[])
                .is_err()
        );
    }

    /// A probe only revives a parked wallet account when its provider may spend.
    ///
    /// apply_probe requires the reading to show the account can actually pay;
    /// for a wallet-only provider with real money off there is nothing to pay
    /// with, so a successful probe still leaves it out of rotation.
    #[test]
    fn a_probe_revives_a_parked_wallet_only_when_its_provider_may_spend() {
        let mut locked = acct("acc-1", "haoai", vec![wallet(1, 1)], AccountState::Parked);
        locked.allow_metered = None;
        let mut allowed = acct("acc-2", "haoai", vec![wallet(1, 1)], AccountState::Parked);
        allowed.allow_metered = Some(true);
        let p = Pool::new(vec![locked, allowed], &policy());

        p.apply_probe("haoai/acc-1", vec![wallet(50, 0)], now());
        p.apply_probe("haoai/acc-2", vec![wallet(50, 0)], now());

        assert_eq!(
            p.account("haoai/acc-1").unwrap().state,
            AccountState::Parked,
            "nothing may pay for this account's traffic"
        );
        assert_eq!(
            p.account("haoai/acc-2").unwrap().state,
            AccountState::Active
        );
    }

    /// rpm_limit and concurrency_limit were parsed, stored and then never read,
    /// so setting either one changed nothing at all.
    #[test]
    fn an_account_at_its_concurrency_limit_is_skipped_until_a_request_finishes() {
        let mut a = acct("a", "p", vec![cover(100, 0)], AccountState::Active);
        a.concurrency_limit = Some(1);
        let p = Pool::new(vec![a], &policy());

        let first = p
            .select("p", "m", None, Decimal::ZERO, "r1", 1, &[])
            .unwrap();
        assert!(
            p.select("p", "m", None, Decimal::ZERO, "r2", 1, &[])
                .is_err(),
            "the account is already serving one request"
        );

        p.settle(&first, Decimal::ZERO, false);
        assert!(
            p.select("p", "m", None, Decimal::ZERO, "r3", 1, &[])
                .is_ok(),
            "and it frees up when that request finishes"
        );
    }

    #[test]
    fn an_account_at_its_rpm_limit_is_skipped_for_the_rest_of_the_window() {
        let mut a = acct("a", "p", vec![cover(100, 0)], AccountState::Active);
        a.rpm_limit = Some(2);
        let p = Pool::new(vec![a], &policy());

        for i in 0..2 {
            let plan = p
                .select("p", "m", None, Decimal::ZERO, &format!("r{i}"), 1, &[])
                .unwrap();
            p.settle(&plan, Decimal::ZERO, false);
        }
        assert!(
            p.select("p", "m", None, Decimal::ZERO, "r3", 1, &[])
                .is_err(),
            "two requests inside the window is the limit"
        );
    }

    /// A sticky session's retry must reach the other account.
    ///
    /// The sticky branch used to short-circuit before the untried preference
    /// ever ran, so a session-bound request spent its whole retry budget on the
    /// account that had just failed -- the exact case the exclusion exists for,
    /// and the common case, because most clients send a session key.
    #[test]
    fn a_sticky_session_retries_on_a_different_account() {
        let p = Pool::new(
            vec![
                acct("a1", "p", vec![cover(100, 0)], AccountState::Active),
                acct("a2", "p", vec![cover(100, 0)], AccountState::Active),
            ],
            &policy(),
        );
        let first = p
            .select("p", "m", Some("sess"), Decimal::ONE, "r", 1, &[])
            .unwrap();
        let second = p
            .select(
                "p",
                "m",
                Some("sess"),
                Decimal::ONE,
                "r",
                2,
                &[first.qualified_account()],
            )
            .unwrap();
        assert_ne!(
            first.account_id, second.account_id,
            "the retry must not go back to the account that just failed"
        );
    }

    /// The reason a state was set has to survive a restart.
    ///
    /// restore() assigned gates, availability and state but never last_error, so
    /// "disabled by operator" was dropped on the way in -- which also broke the
    /// board's explanation of why an account is out, and let the next successful
    /// probe put the account back in the pool.
    #[test]
    fn restoring_an_account_keeps_the_reason_it_was_disabled() {
        let p = Pool::new(
            vec![acct("a", "p", vec![cover(100, 0)], AccountState::Unknown)],
            &policy(),
        );
        p.restore(vec![crate::domain::AccountOverview {
            account_id: "a".into(),
            provider: "p".into(),
            label: "a".into(),
            currency: "USD".into(),
            state: AccountState::Disabled,
            gates: vec![cover(100, 0)],
            available_at: None,
            window_started_at: None,
            last_error: Some(OPERATOR_DISABLED.to_string()),
        }]);

        p.apply_probe("p/a", vec![cover(100, 0)], now());

        let a = p.account("p/a").unwrap();
        assert_eq!(a.state, AccountState::Disabled);
        assert_eq!(a.last_error.as_deref(), Some(OPERATOR_DISABLED));
    }

    /// A probe restores the credential, not the operator's decision.
    #[test]
    fn a_successful_probe_does_not_undo_an_operator_disable() {
        let mut a = acct("a", "p", vec![cover(100, 0)], AccountState::Disabled);
        a.last_error = Some(OPERATOR_DISABLED.to_string());
        let p = Pool::new(vec![a], &policy());

        p.apply_probe("p/a", vec![cover(100, 0)], now());

        let a = p.account("p/a").unwrap();
        assert_eq!(
            a.state,
            AccountState::Disabled,
            "only an operator can put this account back"
        );
        assert_eq!(a.last_error.as_deref(), Some(OPERATOR_DISABLED));
    }

    /// Session keys come straight from client headers, so the map is bounded.
    #[test]
    fn session_bindings_are_bounded() {
        let p = Pool::new(
            vec![acct("a", "p", vec![cover(100, 0)], AccountState::Active)],
            &policy(),
        );
        // Zero estimated cost: this test is about the binding map, and the
        // in-flight gate would otherwise fill the account's headroom first.
        for i in 0..(MAX_STICKY_SESSIONS + 64) {
            let key = format!("s{i}");
            p.select("p", "m", Some(&key), Decimal::ZERO, "r", 1, &[])
                .unwrap();
        }
        assert!(
            p.tracked_sessions() <= MAX_STICKY_SESSIONS,
            "got {} bindings",
            p.tracked_sessions()
        );
    }

    /// A dying account must release the sessions bound to it.
    ///
    /// The pruning compared the BARE id against bindings stored as provider/id,
    /// so it never matched: every session stayed bound to a parked account and
    /// had to fail once before rebinding.
    #[test]
    fn a_parked_account_releases_its_sessions() {
        let p = Pool::new(
            vec![acct("a", "p", vec![cover(100, 0)], AccountState::Active)],
            &policy(),
        );
        p.select("p", "m", Some("sess"), Decimal::ONE, "r", 1, &[])
            .unwrap();
        assert_eq!(p.tracked_sessions(), 1);

        p.note_error("p/a", UpstreamErrorClass::QuotaExhausted, None, None);

        assert_eq!(
            p.tracked_sessions(),
            0,
            "the binding must be dropped so the session can rebind"
        );
    }
}
