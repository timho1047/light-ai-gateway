//! Integration layer: routing, probe orchestration, and persistence glue.
//!
//! FILE OWNER: Lead.
//!
//! The Gateway is the single object the HTTP layer talks to. It owns the account
//! pool, the price table, the quota probes, and the database writer, and it is
//! the only place that decides which pot of money pays for a request.

use crate::accounts::{AttemptPlan, PlanError, Pool};
use crate::config::{Config, ResolvedRoute};
use crate::domain::{
    now, Account, AttemptRecord, GateClass, RequestRecord, TokenUsage,
};
use crate::error::UpstreamErrorClass;
use crate::meter::PriceTable;
use crate::quota::{probe_for, QuotaProbe};
use crate::store::StoreHandle;
use anyhow::{Context, Result};
use rust_decimal::Decimal;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use time::OffsetDateTime;

/// Provider spend observed at the last probe, for price learning.
#[derive(Debug, Clone, Copy, Default)]
struct SpendSample {
    covered_spend: Decimal,
}

pub struct Gateway {
    pub config: Config,
    pub pool: Pool,
    pub store: StoreHandle,
    /// Where provider credentials live. The operator's real OS credential store
    /// in production; an in-memory store in tests.
    pub credentials: crate::secrets::CredentialStore,
    pub http: reqwest::Client,
    pub prices: Mutex<PriceTable>,
    pub probes: HashMap<String, Arc<dyn QuotaProbe>>,
    /// Parsed price windows, keyed by (provider, model). Only models with a
    /// configured schedule appear here; everything else is priced at tier 0.
    pub schedules: HashMap<(String, String), crate::tier::Schedule>,
    /// Token usage accumulated since the last successful probe, keyed by
    /// (provider, model, tier). The tier is part of the key so an interval's
    /// spend can be attributed to the window that actually earned it instead of
    /// being smeared across the peak and off-peak rates.
    pending: Mutex<HashMap<(String, String, u32), TokenUsage>>,
    last_spend: Mutex<HashMap<String, SpendSample>>,
}

impl Gateway {
    pub async fn new(config: Config) -> Result<Arc<Self>> {
        Self::with_credentials(config, crate::secrets::CredentialStore::keychain()).await
    }

    /// Build a gateway against an explicit credential store.
    ///
    /// Tests pass an in-memory store: touching the operator's real keychain from
    /// a test would both pollute it and be non-reproducible.
    pub async fn with_credentials(
        config: Config,
        credentials: crate::secrets::CredentialStore,
    ) -> Result<Arc<Self>> {
        // Tolerate individual accounts whose credential is missing: one stale
        // config entry should not stop the gateway from serving with the accounts
        // that do work. Unusable accounts join the pool in Disabled state, so
        // they render on the board but can never be selected.
        let (mut accounts, unusable) = config.build_accounts_lenient(&credentials)?;
        for acct in &unusable {
            tracing::warn!(
                provider = %acct.provider,
                account = %acct.id,
                reason = %acct.last_error.as_deref().unwrap_or("unavailable"),
                "account disabled: its credential could not be resolved"
            );
        }
        if accounts.is_empty() {
            anyhow::bail!(
                "no account could be loaded: every configured account is missing a credential.                  Run ai-gw keys list to see which ones, then ai-gw keys add <provider> <account>"
            );
        }
        accounts.extend(unusable);
        let pool = Pool::new(accounts, &config.policy);

        // Probes are pinned to configured hosts so credentials can never be
        // redirected to an address that came in with a request.
        let mut probes: HashMap<String, Arc<dyn QuotaProbe>> = HashMap::new();
        for (provider, adapter, base_url) in config.probe_targets() {
            if let Some(p) = probe_for(adapter, &base_url) {
                probes.insert(provider, p);
            }
        }

        let db_path = config.db_path();
        if let Some(parent) = db_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let store = StoreHandle::spawn(&db_path)
            .with_context(|| format!("opening database {}", db_path.display()))?;

        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(config.policy.connect_timeout_secs))
            .pool_idle_timeout(Duration::from_secs(90))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("building upstream HTTP client")?;

        // Seed the price table from configuration. seed_from_config skips
        // non-finite and negative entries instead of coercing them: a NaN price
        // silently becoming zero would make the model look free, and an
        // under-estimated cost is what lets a request slip into metered spend.
        let mut prices = PriceTable::new();
        let configured_models: usize = config.prices.values().map(BTreeMap::len).sum();
        let seeded = prices.seed_from_config(&config.prices);
        if seeded < configured_models {
            tracing::warn!(
                skipped = configured_models - seeded,
                "ignored configured prices that are not usable"
            );
        }

        // Restore prices learned in an earlier run. Without this every restart
        // reverts to the conservative default until the next probe, and that
        // default was measured against live providers to be off by an order of
        // magnitude in both directions.
        match crate::store::open_readonly(&db_path) {
            Ok(conn) => match crate::store::load_prices(&conn) {
                Ok(rows) if !rows.is_empty() => {
                    let n = prices.load_learned(rows);
                    tracing::info!(count = n, "restored learned prices from disk");
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "could not read learned prices"),
            },
            Err(e) => tracing::debug!(error = %e, "no price store to read yet"),
        }

        // Parse the configured price windows once. Only models that have one
        // appear here; every other model is priced at tier 0.
        let mut schedules: HashMap<(String, String), crate::tier::Schedule> = HashMap::new();
        for (provider, models) in &config.prices {
            for (model, p) in models {
                let Ok(s) = p.schedule(provider, model) else {
                    continue;
                };
                if !s.is_empty() {
                    schedules.insert((provider.clone(), model.clone()), s);
                }
            }
        }

        let gateway = Arc::new(Gateway {
            config,
            pool,
            store,
            credentials,
            schedules,
            http,
            prices: Mutex::new(prices),
            probes,
            pending: Mutex::new(HashMap::new()),
            last_spend: Mutex::new(HashMap::new()),
        });

        // Restore the persisted ledger. Accounts come back in Unknown state, so
        // nothing can serve traffic until it has been probed: resuming with a
        // stale ledger is how a gateway re-spends quota it already consumed.
        gateway.restore_from_disk();

        // Probe everything once before accepting traffic.
        gateway.probe_all().await;
        gateway.save_accounts();

        Ok(gateway)
    }

    /// Load persisted accounts into the pool, then persist the fresh snapshot.
    fn restore_from_disk(&self) {
        let path = self.config.db_path();
        let Ok(conn) = crate::store::open_readonly(&path) else {
            return;
        };
        let Ok(accounts) = crate::store::load_accounts(&conn) else {
            return;
        };
        if !accounts.is_empty() {
            tracing::info!(count = accounts.len(), "restored account ledger from disk");
            self.pool.restore(accounts);
        }
    }

    pub fn resolve(&self, client_model: &str) -> Result<ResolvedRoute> {
        self.config.resolve(client_model)
    }

    /// Choose an account for a request.
    ///
    /// A routing failure is not a generic error: it carries how long the client
    /// should wait, so the HTTP layer can answer 429 with a real Retry-After.
    pub fn plan(
        &self,
        client_model: &str,
        session_key: Option<&str>,
        est_cost: Decimal,
        request_id: &str,
        attempt_no: u32,
    ) -> Result<(ResolvedRoute, AttemptPlan), PlanError> {
        let route = self
            .config
            .resolve(client_model)
            .map_err(|_| PlanError::UnknownProvider(client_model.to_string()))?;
        let mut plan = self.pool.select(
            &route.provider,
            &route.model,
            session_key,
            est_cost,
            request_id,
            attempt_no,
        )?;
        // Fix the price tier now, from the time the request starts. Deciding it
        // later would let a request that began in one window be billed against
        // another.
        plan.tier = self.tier_at(&route.provider, &route.model, now());
        Ok((route, plan))
    }

    /// Estimated cost of a request, used to size the hard gate.
    ///
    /// Unknown models are priced at the conservative fallback rather than at
    /// zero, so an unpriced model can never silently drain a wallet.
    /// Which price tier a request starting now falls into.
    ///
    /// Tier 0 is the default price. A configured window makes a request cost
    /// the tier price instead, and getting this wrong is off by the peak
    /// multiplier -- toward spending when the peak rate is higher.
    pub fn tier_at(&self, provider: &str, model: &str, at: time::OffsetDateTime) -> u32 {
        let Some(schedule) = self.schedules.get(&(provider.to_string(), model.to_string()))
        else {
            return 0;
        };
        match schedule.tier_index_at(at) {
            Some(i) => (i + 1) as u32,
            None => 0,
        }
    }

    pub fn estimate_cost(
        &self,
        provider: &str,
        model: &str,
        prompt_tokens: u64,
        completion_tokens: u64,
        multiplier: Decimal,
    ) -> Decimal {
        let usage = TokenUsage {
            prompt_tokens,
            completion_tokens,
            cached_tokens: 0,
            reasoning_tokens: 0,
            measured: false,
        };
        let tier = self.tier_at(provider, model, now());
        if let Ok(prices) = self.prices.lock()
            && let Some(c) = prices.estimate(provider, model, tier, &usage, multiplier)
        {
            return c;
        }
        // Nothing known about this model: over-state rather than under-state.
        crate::meter::conservative_estimate(&usage, multiplier)
    }

    /// Commit the estimated cost before the upstream call.
    pub fn reserve(&self, plan: &AttemptPlan, est_cost: Decimal) {
        self.pool.reserve(plan, est_cost);
    }

    /// Record a completed attempt that succeeded.
    pub fn record_success(
        &self,
        plan: &AttemptPlan,
        usage: &TokenUsage,
        cost: Decimal,
        attempt: AttemptRecord,
        record: RequestRecord,
    ) {
        self.pool.settle(plan, cost, true);
        self.pool.note_activity();
        self.accumulate_usage(&plan.provider, &plan.model, plan.tier, usage);
        // Standing contract with the store: persist the ledger after EVERY
        // state transition. A lost ledger is not just a stale display -- on
        // restart it means re-spending quota already consumed, which on
        // Command Code silently becomes real money.
        self.save_accounts();
        self.store.record_attempt(attempt);
        self.store.record_request(record);
    }

    /// Record a failed attempt.
    ///
    /// A failure that delivered nothing must not be charged to the gate, so the
    /// reservation is released with billed = false.
    pub fn record_failure(
        &self,
        plan: &AttemptPlan,
        class: UpstreamErrorClass,
        retry_after: Option<Duration>,
        window_reset: Option<OffsetDateTime>,
        attempt: AttemptRecord,
    ) {
        let billed = attempt.billed;
        self.pool.settle(plan, Decimal::ZERO, billed);
        self.pool
            .note_error(&plan.qualified_account(), class, retry_after, window_reset);
        self.save_accounts();
        self.store.record_attempt(attempt);
    }

    fn accumulate_usage(&self, provider: &str, model: &str, tier: u32, usage: &TokenUsage) {
        let Ok(mut pending) = self.pending.lock() else {
            return;
        };
        let e = pending
            .entry((provider.to_string(), model.to_string(), tier))
            .or_default();
        e.prompt_tokens += usage.prompt_tokens;
        e.completion_tokens += usage.completion_tokens;
        e.cached_tokens += usage.cached_tokens;
        e.reasoning_tokens += usage.reasoning_tokens;
    }

    /// Probe every account of every provider that has an adapter.
    pub async fn probe_all(&self) {
        for provider in self.pool.providers() {
            let Some(_probe) = self.probes.get(&provider) else {
                continue;
            };
            for account in self.pool.accounts_of(&provider) {
                self.probe_one(&provider, &account).await;
            }
            self.learn_from_probe(&provider);
        }
    }

    /// Probe a single account by id, looking up which provider owns it.
    pub async fn probe_account(&self, provider: &str, account_id: &str) {
        let Some(account) = self
            .pool
            .accounts_of(provider)
            .into_iter()
            .find(|a| a.id == account_id)
        else {
            return;
        };
        self.probe_one(provider, &account).await;
        self.learn_from_probe(provider);
    }

    /// Refresh one account if policy asks for it before switching to it.
    pub async fn ensure_fresh(&self, provider: &str, account_id: &str) {
        if !self.config.policy.probe_before_switch {
            return;
        }
        self.probe_account(provider, account_id).await;
    }

    async fn probe_one(&self, provider: &str, account: &Account) {
        // Never probe an account we have no credential for. The request would be
        // sent unauthenticated and return 401, overwriting the far more useful
        // "no credential, run ai-gw keys add ..." message that is already on the
        // account, and burning a round trip to say nothing.
        if account.api_key.is_empty() {
            tracing::debug!(
                provider,
                account = %account.id,
                "skipping probe: no credential stored for this account"
            );
            return;
        }
        let Some(probe) = self.probes.get(provider).cloned() else {
            return;
        };
        match probe.probe(account, &self.http).await {
            Ok(outcome) => {
                if outcome.gates.is_empty() {
                    self.pool
                        .mark_stale(&account.qualified(), "probe returned no gates");
                    self.save_accounts();
                    return;
                }
                tracing::debug!(
                    provider,
                    account = %account.id,
                    gates = outcome.gates.len(),
                    plan = ?outcome.plan,
                    "quota probe ok"
                );
                self.pool
                    .apply_probe(&account.qualified(), outcome.gates, now());
                self.save_accounts();
            }
            Err(e) => {
                // Keep the previous reading and flag it stale. Writing zeros on
                // failure would both overstate availability and make the CLI
                // claim a funded account is empty.
                tracing::warn!(provider, account = %account.id, error = %e, "quota probe failed");
                self.pool.mark_stale(&account.qualified(), &e.message);
                self.save_accounts();
            }
        }
    }

    /// Attribute spend observed between probes to the models used in that
    /// interval, so per-model effective prices (including provider multipliers
    /// and promotions) are learned from real numbers rather than maintained by
    /// hand.
    fn learn_from_probe(&self, provider: &str) {
        let accounts = self.pool.accounts_of(provider);
        let covered: Decimal = accounts
            .iter()
            .flat_map(|a| a.covered_gates())
            .filter(|g| !g.stale)
            .map(|g| g.used)
            .sum();

        let previous = {
            let Ok(mut last) = self.last_spend.lock() else {
                return;
            };
            let prev = last.get(provider).copied().unwrap_or_default();
            last.insert(
                provider.to_string(),
                SpendSample {
                    covered_spend: covered,
                },
            );
            prev
        };

        let delta = covered - previous.covered_spend;
        if delta <= Decimal::ZERO {
            return;
        }

        let drained: Vec<((String, String, u32), TokenUsage)> = {
            let Ok(mut pending) = self.pending.lock() else {
                return;
            };
            let keys: Vec<(String, String, u32)> = pending
                .keys()
                .filter(|(p, _, _)| p == provider)
                .cloned()
                .collect();
            keys.into_iter()
                .filter_map(|k| pending.remove(&k).map(|u| (k, u)))
                .collect()
        };
        if drained.is_empty() {
            return;
        }

        let Ok(mut prices) = self.prices.lock() else {
            return;
        };

        // Group the interval by model, then split its spend across the tiers that
        // were actually used, in proportion to what each tier was expected to
        // cost. A tick that only touched one tier is exact; one that straddles a
        // window boundary is a proportional approximation, which is the best a
        // single spend figure per interval allows.
        let mut by_model: HashMap<String, Vec<(u32, TokenUsage)>> = HashMap::new();
        for ((_, model, tier), usage) in drained {
            by_model.entry(model).or_default().push((tier, usage));
        }

        for (model, tiers) in by_model {
            let expected: Vec<Decimal> = tiers
                .iter()
                .map(|(tier, usage)| {
                    prices
                        .estimate(provider, &model, *tier, usage, Decimal::ONE)
                        .unwrap_or_else(|| {
                            crate::meter::conservative_estimate(usage, Decimal::ONE)
                        })
                })
                .collect();
            let total_expected: Decimal = expected.iter().copied().sum();
            for ((tier, usage), exp) in tiers.iter().zip(expected.iter()) {
                let share = if total_expected > Decimal::ZERO {
                    *exp / total_expected
                } else {
                    Decimal::ONE / Decimal::from(tiers.len().max(1) as u64)
                };
                prices.observe(provider, &model, *tier, delta * share, usage);
            }
        }
        drop(prices);

        // Surface a model whose observed price no longer matches what was
        // configured: that is either a real repricing or a quiet one, and both
        // are worth knowing before the hard gate is sized from the wrong number.
        self.warn_on_price_drift();
        self.save_prices();
    }

    /// Log every model whose observed price has diverged materially from the
    /// configured expectation.
    fn warn_on_price_drift(&self) {
        let Ok(prices) = self.prices.lock() else {
            return;
        };
        for (provider, model, tier, d) in prices.drifted() {
            tracing::warn!(
                provider,
                model,
                tier,
                configured_cost = %d.expected,
                observed_cost = %d.observed,
                drift_pct = format_args!("{:+.1}%", d.ratio * 100.0),
                samples = d.samples,
                "learned price has drifted from the configured price; the upstream \
                 may have changed its rates"
            );
        }
    }

    /// Persist learned prices so a restart does not fall back to the default.
    pub fn save_prices(&self) {
        let rows = match self.prices.lock() {
            Ok(p) => p.learned_rows(),
            Err(_) => return,
        };
        self.store.save_prices(rows);
    }

    /// Persist a request that failed across every attempt.
    ///
    /// Individual attempts are recorded as they happen. Without this the
    /// request-level row for a total failure would be missing, so failures
    /// would never appear in history or usage reports.
    pub fn record_failed_request(&self, record: RequestRecord) {
        self.save_accounts();
        self.store.record_request(record);
    }

    /// Persist the current ledger.
    pub fn save_accounts(&self) {
        self.store.save_accounts(&self.pool.snapshot());
    }

    /// Background probe loop. Cadence tightens while traffic is flowing and
    /// relaxes when the gateway is idle, which keeps probe volume low without
    /// letting the ledger drift during a burst.
    pub fn spawn_poller(self: &Arc<Self>) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            // Give the initial probe a moment to settle before the first tick.
            tokio::time::sleep(Duration::from_secs(5)).await;
            loop {
                let idle = this.pool.idle_for();
                let p = &this.config.policy;
                let secs = if idle < Duration::from_secs(300) {
                    p.poll_interval_active_secs
                } else if idle < Duration::from_secs(1800) {
                    p.poll_interval_secs
                } else {
                    p.poll_interval_idle_secs
                }
                .max(15);

                tokio::time::sleep(Duration::from_secs(secs)).await;

                this.probe_all().await;
                this.save_accounts();
            }
        });
    }

    /// Persist the ledger and wait for the writer to commit, so a shutdown
    /// never loses the most recent spend.
    pub fn shutdown(&self) -> Result<()> {
        self.save_accounts();
        self.store.flush_blocking()
    }

    pub fn snapshot(&self) -> Vec<Account> {
        self.pool.snapshot()
    }

    /// Which pot will pay for an account's next request, for display.
    pub fn coverage_of(&self, account: &Account) -> GateClass {
        if account.has_covered() {
            GateClass::Covered
        } else {
            GateClass::Metered
        }
    }
}
