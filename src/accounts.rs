//! The account pool: gate evaluation, selection, and the local ledger.
//!
//! FROZEN CONTRACT (Lead-owned).
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
//! FreeTokens gates are counted in tokens, so they are excluded from every
//! account-level decision here -- the hard gate, the soft threshold and the
//! consume-first ordering. Exhausting a free allowance blocks free models only;
//! letting it retire the account would take paid models offline because an
//! allowance nobody was spending ran out. It reaches the client as the
//! provider's own 429.

use crate::config::PolicyConfig;
use crate::domain::{
    now, Account, AccountId, AccountState, Confidence, Gate, GateClass, GateKind, GateSource,
    ProviderId,
};
use crate::error::UpstreamErrorClass;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::sync::Mutex;
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
    /// Price tier this request was priced at, decided when it was planned.
    /// 0 is the default tier; higher values are configured time windows.
    pub tier: u32,
    pub attempt_no: u32,
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

struct PoolInner {
    accounts: Vec<Account>,
    /// session key -> canonical provider/id of the bound account.
    sticky: HashMap<String, String>,
    inflight: HashMap<AccountId, Inflight>,
    /// Recent completed request costs, used to size the safety reserve.
    recent_costs: std::collections::VecDeque<Decimal>,
}

/// The pool. All methods are synchronous and short; state is guarded by a
/// plain Mutex that is never held across an await point.
pub struct Pool {
    inner: Mutex<PoolInner>,
    soft_threshold: f64,
    allow_metered: bool,
    hard_margin: Decimal,
    /// Last time a request was served, used to pick the polling cadence.
    last_activity: Mutex<Instant>,
}

impl Pool {
    pub fn new(accounts: Vec<Account>, policy: &PolicyConfig) -> Self {
        Pool {
            inner: Mutex::new(PoolInner {
                accounts,
                sticky: HashMap::new(),
                inflight: HashMap::new(),
                recent_costs: std::collections::VecDeque::new(),
            }),
            soft_threshold: policy.soft_threshold,
            allow_metered: policy.allow_metered,
            hard_margin: Decimal::try_from(policy.hard_margin).unwrap_or(Decimal::ZERO),
            last_activity: Mutex::new(Instant::now()),
        }
    }

    pub fn allow_metered(&self) -> bool {
        self.allow_metered
    }

    pub fn soft_threshold(&self) -> f64 {
        self.soft_threshold
    }

    pub fn note_activity(&self) {
        if let Ok(mut t) = self.last_activity.lock() {
            *t = Instant::now();
        }
    }

    pub fn idle_for(&self) -> Duration {
        self.last_activity
            .lock()
            .map(|t| t.elapsed())
            .unwrap_or_default()
    }

    /// Safety reserve: the p95 of recently completed request costs, so that
    /// requests already on the wire cannot push an account into overflow.
    fn safety_reserve(&self, inner: &PoolInner) -> Decimal {
        if inner.recent_costs.is_empty() {
            return self.hard_margin;
        }
        let mut v: Vec<Decimal> = inner.recent_costs.iter().copied().collect();
        v.sort();
        let idx = ((v.len() as f64 * 0.95) as usize).min(v.len() - 1);
        v[idx] + self.hard_margin
    }

    /// Headroom on covered gates only, in gate units.
    ///
    /// account_gates() already drops the token-denominated allowance: comparing
    /// a USD estimate against a token cap would be meaningless.
    fn covered_headroom(&self, acct: &Account) -> Option<Decimal> {
        let mut min: Option<Decimal> = None;
        for g in acct.account_gates() {
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

        if acct.has_covered() {
            match self.covered_headroom(acct) {
                Some(h) if h >= needed => return GateDecision::Covered,
                // Covered is exhausted; whether metered may absorb the overflow
                // is a policy question, and the default answer is no.
                Some(_) => {
                    if self.allow_metered
                        && self.metered_headroom(acct).is_some_and(|m| m >= needed)
                    {
                        return GateDecision::Metered;
                    }
                    return GateDecision::Reject(RejectReason::InsufficientHeadroom);
                }
                // No money-denominated covered gate at all.
                None => {
                    if self.allow_metered
                        && self.metered_headroom(acct).is_some_and(|m| m >= needed)
                    {
                        return GateDecision::Metered;
                    }
                    return GateDecision::Reject(RejectReason::InsufficientHeadroom);
                }
            }
        }

        // Pure metered provider (for example DeepSeek): nothing is prepaid, so
        // every request costs real money.
        if self.allow_metered && self.metered_headroom(acct).is_some_and(|m| m >= needed) {
            return GateDecision::Metered;
        }
        GateDecision::Reject(RejectReason::MeteredNotAllowed)
    }

    /// Move a parked account back to active once its window has reset.
    fn reap(&self, inner: &mut PoolInner, at: OffsetDateTime) {
        for acct in inner.accounts.iter_mut() {
            if acct.state == AccountState::Parked
                && let Some(when) = acct.available_at
                    && when <= at {
                        acct.state = AccountState::Active;
                        acct.available_at = None;
                        acct.window_started_at = None;
                        // Window rolled over: its allowance is whole again.
                        for g in acct.gates.iter_mut() {
                            if g.kind.is_window() {
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
    pub fn select(
        &self,
        provider: &str,
        model: &str,
        session_key: Option<&str>,
        est_cost: Decimal,
        request_id: &str,
        attempt_no: u32,
    ) -> Result<AttemptPlan, PlanError> {
        let at = now();
        let mut inner = self.inner.lock().unwrap();
        let reserve = self.safety_reserve(&inner);
        self.reap(&mut inner, at);

        struct Cand {
            /// Bare account id, as it appears in configuration.
            id: AccountId,
            /// Canonical provider/id, used for sticky binding.
            qualified: String,
            decision: GateDecision,
            util: f64,
            remaining: Decimal,
            draining: bool,
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
                    .filter(|g| g.kind != GateKind::FreeTokens && g.remaining() <= Decimal::ZERO)
                    .filter_map(|g| g.resets_at)
                    .min()
            });
            if let Some(w) = next_ready {
                earliest = Some(match earliest {
                    Some(e) if e <= w => e,
                    _ => w,
                });
            }
            match self.evaluate(acct, est_cost, reserve) {
                GateDecision::Covered => {
                    covered_cands.push(Cand {
                        id: acct.id.clone(),
                        qualified: acct.qualified(),
                        decision: GateDecision::Covered,
                        util: acct.peak_covered_utilization().unwrap_or(0.0),
                        remaining: acct.min_covered_remaining().unwrap_or(Decimal::MAX),
                        draining: acct.state == AccountState::Draining,
                    });
                }
                GateDecision::Metered => {
                    metered_cands.push(Cand {
                        id: acct.id.clone(),
                        qualified: acct.qualified(),
                        decision: GateDecision::Metered,
                        util: 0.0,
                        remaining: self.metered_headroom(acct).unwrap_or(Decimal::MAX),
                        draining: acct.state == AccountState::Draining,
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
            && let Some(bound) = inner.sticky.get(key) {
                let bound_cand = covered_cands
                    .iter()
                    .find(|c| &c.qualified == bound)
                    .or_else(|| metered_cands.iter().find(|c| &c.qualified == bound));
                if let Some(cand) = bound_cand {
                    let coverage = match cand.decision {
                        GateDecision::Covered => GateClass::Covered,
                        GateDecision::Metered => GateClass::Metered,
                        GateDecision::Reject(_) => {
                            unreachable!("candidate lists only hold acceptable accounts")
                        }
                    };
                    return Ok(AttemptPlan {
                        request_id: request_id.to_string(),
                        account_id: cand.id.clone(),
                        provider: provider.to_string(),
                        model: model.to_string(),
                        session_key: session_key.map(str::to_string),
                        coverage,
                        // The pool knows nothing about price windows; the router
                        // overwrites this once it has resolved the schedule.
                        tier: 0,
                        attempt_no,
                    });
                }
                // Bound account no longer acceptable: fall through and rebind.
            }

        // Prefer covered over metered, always.
        let (mut pool, coverage) = if !covered_cands.is_empty() {
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
                needed: est_cost + reserve,
                best_account: best.as_ref().map(|(a, _, _)| a.clone()),
                best_headroom: best.as_ref().map(|(_, h, _)| *h),
                reason: best.as_ref().map(|(_, _, r)| *r),
            });
        };

        // Prefer accounts below the soft threshold; a draining account is one
        // that has already crossed it.
        let below: Vec<&Cand> = pool
            .iter()
            .filter(|c| !c.draining && c.util < self.soft_threshold)
            .collect();

        let chosen: &Cand = if let Some(first) = below.first() {
            // CONSUME-FIRST: the most utilised still-eligible account, so the
            // pool burns down one account at a time and switches rarely.
            below
                .iter()
                .max_by(|a, b| {
                    a.util
                        .partial_cmp(&b.util)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| b.remaining.cmp(&a.remaining))
                })
                .copied()
                .unwrap_or(first)
        } else {
            // Every account is at or above the soft threshold. Rather than
            // refusing, fall back to the least-utilised one: switching at 80%
            // is a preference, and a self-imposed preference must not become a
            // hard outage while headroom genuinely remains.
            pool.sort_by(|a, b| {
                a.util
                    .partial_cmp(&b.util)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.qualified.cmp(&b.qualified))
            });
            &pool[0]
        };

        if let Some(key) = session_key {
            // Sticky binds to the canonical identity, so two providers using the
            // same account id can never bind to each other.
            inner
                .sticky
                .insert(key.to_string(), chosen.qualified.clone());
        }

        Ok(AttemptPlan {
            request_id: request_id.to_string(),
            account_id: chosen.id.clone(),
            provider: provider.to_string(),
            model: model.to_string(),
            session_key: session_key.map(str::to_string),
            coverage,
            tier: 0,
            attempt_no,
        })
    }

    /// Commit estimated cost against an account before the request goes out.
    pub fn reserve(&self, plan: &AttemptPlan, est_cost: Decimal) {
        let mut inner = self.inner.lock().unwrap();
        let qualified = plan.qualified_account();
        let e = inner
            .inflight
            .entry(qualified.clone())
            .or_insert(Inflight {
                committed: Decimal::ZERO,
                count: 0,
            });
        e.committed += est_cost;
        e.count += 1;
        if let Some(acct) = find_mut(&mut inner.accounts, &qualified)
            && acct.window_started_at.is_none() {
                acct.window_started_at = Some(now());
            }
    }

    /// Reconcile a reservation with actual usage and update the ledger.
    ///
    /// When the billed flag is false the attempt failed before the upstream did
    /// any work, so the gate must not be charged for it.
    pub fn settle(&self, plan: &AttemptPlan, actual_cost: Decimal, billed: bool) {
        let mut inner = self.inner.lock().unwrap();
        let qualified = plan.qualified_account();

        if let Some(e) = inner.inflight.get_mut(&qualified) {
            // Release the reservation; the real number replaces it below.
            if e.count > 0 {
                e.count -= 1;
            }
            e.committed = Decimal::ZERO;
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
        let soft = self.soft_threshold;
        if let Some(acct) = find_mut(&mut inner.accounts, &qualified)
            && acct.state == AccountState::Active
                && let Some(u) = acct.peak_covered_utilization()
                    && u >= soft {
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
            let dead = acct.id.clone();
            inner.sticky.retain(|_, v| v != &dead);
        }
    }

    /// Apply fresh gate readings from a quota probe.
    pub fn apply_probe(&self, qualified_id: &str, gates: Vec<Gate>, at: OffsetDateTime) {
        let mut inner = self.inner.lock().unwrap();
        let allow_metered = self.allow_metered;
        let soft = self.soft_threshold;
        let Some(acct) = find_mut(&mut inner.accounts, qualified_id) else {
            return;
        };
        if gates.is_empty() {
            return;
        }
        acct.gates = gates;
        acct.last_probe_at = Some(at);
        acct.last_error = None;

        if acct.state == AccountState::Unknown {
            acct.state = AccountState::Active;
        }

        // An account parked because its window was spent comes back once the
        // probe shows headroom again.
        if acct.state == AccountState::Parked {
            let has_room = acct
                .covered_gates()
                .any(|g| g.kind != GateKind::FreeTokens && g.remaining() > Decimal::ZERO);
            let has_metered = acct.metered_gates().any(|g| g.remaining() > Decimal::ZERO);
            if has_room || (allow_metered && has_metered) {
                acct.state = AccountState::Active;
                acct.available_at = None;
            }
        }

        // Reflect the soft threshold against freshly probed numbers.
        if acct.state == AccountState::Active
            && let Some(u) = acct.peak_covered_utilization()
                && u >= soft {
                    acct.state = AccountState::Draining;
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

    /// Whether any account for this provider is live and has covered headroom.
    pub fn has_covered_capacity(&self, provider: &str) -> bool {
        let inner = self.inner.lock().unwrap();
        inner
            .accounts
            .iter()
            .filter(|a| a.provider == provider)
            .any(|a| a.has_covered() && a.state == AccountState::Active)
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
                a.state = match r.state {
                    AccountState::Active | AccountState::Draining => AccountState::Unknown,
                    other => other,
                };
            }
        }
    }

    /// Number of accounts currently able to serve.
    pub fn eligible_count(&self, provider: &str) -> usize {
        let inner = self.inner.lock().unwrap();
        inner
            .accounts
            .iter()
            .filter(|a| {
                a.provider == provider
                    && matches!(a.state, AccountState::Active | AccountState::Draining)
            })
            .count()
    }
}

/// Build a gate from a probe reading, normalising the common shapes.
pub fn gate(kind: GateKind, cap: Decimal, used: Decimal, resets_at: Option<OffsetDateTime>) -> Gate {
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
            last_probe_at: None,
            last_error: None,
        }
    }

    fn cover(cap: i64, used: i64) -> Gate {
        gate(GateKind::FiveHour, Decimal::from(cap), Decimal::from(used), None)
    }

    fn wallet(cap: i64, used: i64) -> Gate {
        gate(GateKind::Wallet, Decimal::from(cap), Decimal::from(used), None)
    }

    fn policy() -> PolicyConfig {
        PolicyConfig::default()
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
        p.apply_probe(
            "xkiro/acc-1",
            vec![cover(200, 55)],
            crate::domain::now(),
        );
        assert_eq!(p.account("xkiro/acc-1").unwrap().gates[0].used, Decimal::from(55));
        assert_eq!(
            p.account("commandcode/acc-1").unwrap().gates[0].used,
            Decimal::from(1),
            "the other provider's account must be untouched"
        );

        // Selection is scoped to the requested provider.
        let xk = p.select("xkiro", "m", None, Decimal::ONE, "r", 1).unwrap();
        assert_eq!(xk.account_id, "acc-1");
        assert_eq!(xk.qualified_account(), "xkiro/acc-1");
        let cc = p
            .select("commandcode", "m", None, Decimal::ONE, "r", 1)
            .unwrap();
        assert_eq!(cc.qualified_account(), "commandcode/acc-1");

        // An error on one must not park the other.
        p.note_error(
            "xkiro/acc-1",
            UpstreamErrorClass::QuotaExhausted,
            None,
            None,
        );
        assert_eq!(p.account("xkiro/acc-1").unwrap().state, AccountState::Parked);
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
            vec![acct("xk-1", "xkiro", vec![cover(20, 7)], AccountState::Active)],
            &policy(),
        );

        let err = p
            .select("xkiro", "m", None, Decimal::from(40), "r", 1)
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
            .select("xkiro", "m", None, Decimal::ONE, "r", 1)
            .expect_err("an account with no gates cannot serve");
        let msg = err.to_string();
        assert!(msg.starts_with("no eligible account for provider xkiro"), "{msg}");
        assert!(!msg.contains("most room"), "no headroom was measured: {msg}");
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
            vec![acct("xk-1", "xkiro", vec![cover(200, 20)], AccountState::Active)],
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
        let plan = p.select("xkiro", "m", None, Decimal::ONE, "r", 1).unwrap();
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
            .select("xkiro", "m", Some("sess"), Decimal::ONE, "r", 1)
            .unwrap();
        assert_eq!(first.qualified_account(), "xkiro/acc-1");
        // Same session key, different provider: must not follow the binding.
        let other = p
            .select("commandcode", "m", Some("sess"), Decimal::ONE, "r", 1)
            .unwrap();
        assert_eq!(other.qualified_account(), "commandcode/acc-1");
    }

    #[test]
    fn unknown_accounts_never_serve() {
        let p = Pool::new(
            vec![acct("a", "p", vec![cover(100, 0)], AccountState::Unknown)],
            &policy(),
        );
        assert!(p.select("p", "m", None, Decimal::ONE, "r", 1).is_err());
    }

    #[test]
    fn hard_gate_rejects_when_headroom_below_cost() {
        let p = Pool::new(
            vec![acct("a", "p", vec![cover(100, 99)], AccountState::Active)],
            &policy(),
        );
        // 2.0 of cost against 1.0 of headroom must be refused...
        assert!(p.select("p", "m", None, Decimal::from(2), "r", 1).is_err());
        // ...while a cost that exactly fits is still affordable.
        assert!(p.select("p", "m", None, Decimal::from(1), "r", 1).is_ok());
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
        let plan = p.select("p", "m", None, Decimal::ONE, "r", 1).unwrap();
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
            .select("p", "m", None, Decimal::ONE, "r", 1)
            .expect_err("a spent window must not be selected");
        match err {
            PlanError::NoEligibleAccount { retry_after, .. } => {
                let secs = retry_after.expect("spent window must yield a retry hint").as_secs();
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
        let plan = p.select("p", "m", None, Decimal::ONE, "r", 1).unwrap();
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
        let first = p.select("p", "m", Some("sess"), Decimal::ONE, "r", 1).unwrap();
        assert_eq!(first.account_id, "a");
        // Account a is now the busiest, but the session must not move: moving
        // it would discard the prompt cache.
        let second = p
            .select("p", "m", Some("sess"), Decimal::ONE, "r2", 1)
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
        assert!(p
            .select("deepseek", "m", None, Decimal::ONE, "r", 1)
            .is_err());
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
            .select("deepseek", "m", None, Decimal::ONE, "r", 1)
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
                acct("walletonly", "p", vec![wallet(100, 0)], AccountState::Active),
            ],
            &pol,
        );
        let plan = p.select("p", "m", None, Decimal::ONE, "r", 1).unwrap();
        assert_eq!(plan.account_id, "cc");
        assert_eq!(plan.coverage, GateClass::Covered);
    }

    #[test]
    fn settling_charges_the_covered_window() {
        let p = Pool::new(
            vec![acct("a", "p", vec![cover(100, 0)], AccountState::Active)],
            &policy(),
        );
        let plan = p.select("p", "m", None, Decimal::ONE, "r", 1).unwrap();
        p.reserve(&plan, Decimal::from(2));
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
        let plan = p.select("p", "m", None, Decimal::ONE, "r", 1).unwrap();
        p.settle(&plan, Decimal::from(85), true);
        assert_eq!(p.account("p/a").unwrap().state, AccountState::Draining);
    }

    #[test]
    fn quota_exhaustion_parks_and_clears_sticky() {
        let p = Pool::new(
            vec![acct("a", "p", vec![cover(100, 0)], AccountState::Active)],
            &policy(),
        );
        let _ = p.select("p", "m", Some("s"), Decimal::ONE, "r", 1).unwrap();
        p.note_error("p/a", UpstreamErrorClass::QuotaExhausted, None, None);
        let a = p.account("p/a").unwrap();
        assert_eq!(a.state, AccountState::Parked);
        assert!(p.select("p", "m", Some("s"), Decimal::ONE, "r2", 1).is_err());
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
        let mut a = acct("a", "p", vec![cover(100, 100)], AccountState::Parked);
        a.available_at = Some(past);
        let p = Pool::new(vec![a], &policy());
        let plan = p.select("p", "m", None, Decimal::ONE, "r", 1).unwrap();
        assert_eq!(plan.account_id, "a");
        let a = p.account("p/a").unwrap();
        assert_eq!(a.state, AccountState::Active);
        assert_eq!(
            a.gates[0].used,
            Decimal::ZERO,
            "a rolled window starts whole again"
        );
    }
}
