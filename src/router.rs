//! Integration layer: routing, probe orchestration, and persistence glue.
//!
//! The Gateway is the single object the HTTP layer talks to. It owns the account
//! pool, the price table, the quota probes, and the database writer, and it is
//! the only place that decides which pot of money pays for a request.

use crate::accounts::{PlanError, Pool, Reservation};
use crate::config::{Config, ResolvedRoute};
use crate::domain::{Account, AttemptRecord, GateKind, RequestRecord, TokenUsage, now};
use crate::error::UpstreamErrorClass;
use crate::interval::IntervalLedger;
use crate::meter::PriceTable;
use crate::quota::{QuotaProbe, probe_for};
use crate::store::StoreHandle;
use crate::tier::TierKey;
use anyhow::{Context, Result};
use rust_decimal::Decimal;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use time::OffsetDateTime;

pub struct Gateway {
    pub config: Config,
    pub pool: Pool,
    pub store: StoreHandle,
    /// Where provider credentials live. The operator's real OS credential store
    /// in production; an in-memory store in tests.
    pub credentials: crate::secrets::CredentialStore,
    pub http: reqwest::Client,
    pub prices: Mutex<PriceTable>,
    /// Tokens served since the last probe, and the spend each interval moved.
    interval: Mutex<IntervalLedger>,
    pub probes: HashMap<String, Arc<dyn QuotaProbe>>,
    /// Parsed price windows, keyed by (provider, model). Only models with a
    /// configured schedule appear here; everything else is priced at tier 0.
    pub schedules: HashMap<(String, String), crate::tier::Schedule>,
}

impl Gateway {
    pub async fn new(config: Config) -> Result<Arc<Self>> {
        Self::with_credentials(config, crate::secrets::CredentialStore::from_env()).await
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
                "no account could be loaded: every configured account is missing a credential. \
                 Run lgw keys list to see which ones, then lgw keys add <provider> <account>"
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

        // The ledger's directory and file modes are the store's business: it
        // creates both private, and a second create_dir_all here would make the
        // directory exist first and take that decision away from it.
        let db_path = config.db_path();
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
        match crate::store::Ledger::open(&db_path) {
            Ok(ledger) => match ledger.prices() {
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
            interval: Mutex::new(IntervalLedger::new()),
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
        let Ok(ledger) = crate::store::Ledger::open(&path) else {
            return;
        };
        let Ok(accounts) = ledger.accounts() else {
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
        exclude: &[String],
    ) -> Result<(ResolvedRoute, Reservation), PlanError> {
        let route = self
            .config
            .resolve(client_model)
            .map_err(|_| PlanError::UnknownProvider(client_model.to_string()))?;
        let mut reservation = self.pool.select(
            &route.provider,
            &route.model,
            session_key,
            est_cost,
            request_id,
            attempt_no,
            exclude,
        )?;
        // Fix the price tier now, from the time the request starts. Deciding it
        // later would let a request that began in one window be billed against
        // another.
        reservation.set_tier(self.tier_at(&route.provider, &route.model, now()));
        Ok((route, reservation))
    }

    /// Which price window a request starting now falls into.
    ///
    /// The default price applies when no configured window matches. Getting this
    /// wrong is off by the peak multiplier -- toward spending when the peak rate
    /// is higher.
    pub fn tier_at(&self, provider: &str, model: &str, at: time::OffsetDateTime) -> TierKey {
        let Some(schedule) = self
            .schedules
            .get(&(provider.to_string(), model.to_string()))
        else {
            return TierKey::Default;
        };
        schedule.tier_at(at)
    }

    /// Record a completed attempt that succeeded.
    pub fn record_success(
        &self,
        reservation: Reservation,
        usage: &TokenUsage,
        cost: Decimal,
        attempt: AttemptRecord,
        record: RequestRecord,
    ) {
        // Read what the records need first: settling consumes the reservation,
        // and everything below is written from these.
        let qualified = reservation.qualified_account();
        let provider = reservation.provider.clone();
        let model = reservation.model.clone();
        let tier = reservation.tier.clone();
        reservation.settle(cost, true);
        self.pool.note_activity();
        self.observe_usage(&provider, &model, tier, usage);
        // Standing contract with the store: persist the ledger after EVERY
        // state transition. A lost ledger is not just a stale display -- on
        // restart it means re-spending quota already consumed, which on
        // Command Code silently becomes real money.
        self.save_account(&qualified);
        self.store.record_attempt(attempt);
        self.store.record_request(record);
    }

    /// Record a failed attempt.
    ///
    /// A failure that delivered nothing must not be charged to the gate, so the
    /// reservation is released with billed = false.
    pub fn record_failure(
        &self,
        reservation: Reservation,
        class: UpstreamErrorClass,
        retry_after: Option<Duration>,
        window_reset: Option<OffsetDateTime>,
        attempt: AttemptRecord,
    ) {
        let billed = attempt.billed;
        let qualified = reservation.qualified_account();
        reservation.settle(Decimal::ZERO, billed);
        self.pool
            .note_error(&qualified, class, retry_after, window_reset);
        self.save_account(&qualified);
        self.store.record_attempt(attempt);
    }

    /// Count tokens the gateway served, for the next interval to learn from.
    fn observe_usage(&self, provider: &str, model: &str, tier: TierKey, usage: &TokenUsage) {
        if let Ok(mut ledger) = self.interval.lock() {
            ledger.observe(provider, model, tier, usage);
        }
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
        // "no credential, run lgw keys add ..." message that is already on the
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
                self.warn_on_ledger_drift(
                    &account.qualified(),
                    &outcome.gates,
                    self.config.policy.drift_threshold,
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

    /// Report a probe whose reading disagrees with the local ledger by more
    /// than policy.drift_threshold.
    ///
    /// This is the only reason the ledger can drift at all: the gateway sees
    /// every request it serves, so between probes its own accounting should
    /// track the provider's. A large gap means either the account is being used
    /// by something other than this gateway -- which the learned-price scheme
    /// cannot tolerate, because it would attribute that spend to these tokens --
    /// or a write was lost. The correction itself is the wholesale gate
    /// replacement that follows; this makes it visible instead of silent.
    fn warn_on_ledger_drift(&self, qualified: &str, fresh: &[crate::domain::Gate], threshold: f64) {
        if threshold <= 0.0 {
            return;
        }
        let Some(current) = self.pool.account(qualified) else {
            return;
        };
        for gate in fresh {
            let Some(previous) = current.gates.iter().find(|g| g.kind == gate.kind) else {
                continue;
            };
            // Only the direction that threatens the gate is worth a warning:
            // the provider believing MORE was spent than we recorded. The other
            // direction is a window rolling over, which is normal, and warning
            // about it fired on every reset.
            if gate.used <= previous.used {
                continue;
            }
            let scale = previous.used.abs().max(gate.used.abs());
            if scale <= Decimal::ZERO {
                continue;
            }
            let drift = (gate.used - previous.used) / scale;
            if drift > Decimal::try_from(threshold).unwrap_or(Decimal::ZERO) {
                tracing::warn!(
                    account = qualified,
                    gate = gate.kind.label(),
                    local = %previous.used,
                    provider = %gate.used,
                    drift_pct = %(drift * Decimal::from(100)).round_dp(1),
                    "the local ledger had drifted from the provider; the provider reading wins"
                );
            }
        }
    }

    /// Attribute spend observed between probes to the models used in that
    /// interval, so per-model effective prices (including provider multipliers
    /// and promotions) are learned from real numbers rather than maintained by
    /// hand.
    fn learn_from_probe(&self, provider: &str) {
        // One reading per account: gate kind -> that window's cumulative spend.
        let readings: Vec<(String, HashMap<GateKind, Decimal>)> = self
            .pool
            .accounts_of(provider)
            .iter()
            .map(|acct| {
                let gates = acct
                    .covered_gates()
                    .filter(|g| !g.stale)
                    .map(|g| (g.kind, g.used))
                    .collect();
                (acct.qualified(), gates)
            })
            .collect();

        // The ledger's lock is released before the price table's is taken: the
        // two are never held at once, so no caller can deadlock against the
        // poller.
        let interval = {
            let Ok(mut ledger) = self.interval.lock() else {
                return;
            };
            ledger.settle(provider, &readings)
        };

        if interval.spend <= Decimal::ZERO {
            return;
        }

        if let Some(total) = interval.unattributed {
            // Spend moved and none of our tokens did. That is either an account
            // used by something other than this gateway or traffic that predates
            // this process, and either way the money cannot be attributed to a
            // model: it is dropped from learning rather than smeared over these
            // tokens. Silently dropping it is how the same thing hid for a week,
            // so both the interval and the running total are reported -- the
            // interval alone looks like rounding, the total is the real number.
            tracing::warn!(
                provider,
                interval_spend = %interval.spend,
                unattributed_total = %total,
                "spend moved with no local tokens to attribute it to; price learning \
                 skipped this interval. Is this account used outside the gateway?"
            );
            return;
        }

        let spend = interval.spend;
        let Ok(mut prices) = self.prices.lock() else {
            return;
        };

        // Split the interval's spend across the tiers that were actually used, in
        // proportion to what each tier was expected to cost. A tick that only
        // touched one tier is exact; one that straddles a window boundary is a
        // proportional approximation, which is the best a single spend figure per
        // interval allows.
        for (model, tiers) in interval.tokens {
            let expected: Vec<Decimal> = tiers
                .iter()
                .map(|(tier, usage)| {
                    prices
                        .estimate(provider, &model, tier, usage, Decimal::ONE)
                        .unwrap_or_else(|| crate::meter::conservative_estimate(usage, Decimal::ONE))
                })
                .collect();
            let total_expected: Decimal = expected.iter().copied().sum();
            for ((tier, usage), exp) in tiers.iter().zip(expected.iter()) {
                let share = if total_expected > Decimal::ZERO {
                    *exp / total_expected
                } else {
                    Decimal::ONE / Decimal::from(tiers.len().max(1) as u64)
                };
                prices.observe(provider, &model, tier, spend * share, usage);
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
                tier = tier.label(),
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
    /// Persist the one account an attempt touched.
    ///
    /// save_accounts() is for probes and operator actions, where the whole pool
    /// is the subject. On the request path it rewrote every account and every
    /// one of their gates for one account's change -- work proportional to the
    /// pool, done on every completed request.
    fn save_account(&self, qualified: &str) {
        let Some(account) = self.pool.account(qualified) else {
            return;
        };
        self.store.save_account(&account);
    }

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
}
