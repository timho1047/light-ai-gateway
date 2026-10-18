//! The immutable runtime snapshot a request or a probe works against.
//!
//! Everything configuration-derived hangs off one snapshot: the parsed config,
//! the pre-resolved route table, the probe set, the price windows, the upstream
//! HTTP client and the price table current when the snapshot was published.
//! Requests and probes capture it once at admission and use it to settlement, so
//! a later publication can never move the route, timeout or price an in-flight
//! request is running under. Old snapshots stay alive as long as any request
//! still holds an Arc to them.
//!
//! Later work (the apply engine) builds the replacement snapshot and publishes it
//! through `Gateway::install_runtime`; this module deliberately contains no
//! reload or watcher logic of its own.

use crate::config::{Config, ResolvedRoute};
use crate::domain::ProviderId;
use crate::meter::PriceTable;
use crate::quota::{QuotaProbe, probe_for};
use crate::tier::{Schedule, TierKey};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// One immutable, configuration-derived view of the gateway.
pub struct Runtime {
    config: Arc<Config>,
    /// `[routes]`: client model -> provider, pre-resolved so admission is one
    /// hash lookup rather than a tree walk.
    routes: HashMap<String, ProviderId>,
    /// `provider_prefix_aliases`: leading path segment -> provider.
    aliases: HashMap<String, ProviderId>,
    probes: HashMap<ProviderId, Arc<dyn QuotaProbe>>,
    /// Parsed price windows, keyed by (provider, model). Only models with a
    /// configured schedule appear here; everything else is priced at tier 0.
    schedules: HashMap<(String, String), Schedule>,
    http: reqwest::Client,
    prices: Arc<PriceTable>,
}

impl Runtime {
    /// Build a snapshot from a validated configuration and the price table
    /// current at publication time.
    pub fn new(config: Arc<Config>, prices: Arc<PriceTable>) -> Result<Runtime> {
        let routes = config.routes.clone().into_iter().collect();
        let aliases = config.provider_prefix_aliases.clone().into_iter().collect();

        // Probes are pinned to configured hosts so credentials can never be
        // redirected to an address that came in with a request.
        let mut probes: HashMap<ProviderId, Arc<dyn QuotaProbe>> = HashMap::new();
        for (provider, adapter, base_url) in config.probe_targets() {
            if let Some(p) = probe_for(adapter, &base_url) {
                probes.insert(provider, p);
            }
        }

        // Parse the configured price windows once. Only models that have one
        // appear here; every other model is priced at tier 0.
        let mut schedules: HashMap<(String, String), Schedule> = HashMap::new();
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

        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(config.policy.connect_timeout_secs))
            .pool_idle_timeout(Duration::from_secs(90))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("building upstream HTTP client")?;

        Ok(Runtime {
            config,
            routes,
            aliases,
            probes,
            schedules,
            http,
            prices,
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// The price table current when this snapshot was published. Settlement uses
    /// this one, never whatever a later probe has learned since.
    pub fn prices(&self) -> &PriceTable {
        &self.prices
    }

    pub(crate) fn prices_arc(&self) -> Arc<PriceTable> {
        self.prices.clone()
    }

    /// The probe adapter for a provider, if it has one.
    pub fn probe(&self, provider: &str) -> Option<Arc<dyn QuotaProbe>> {
        self.probes.get(provider).cloned()
    }

    /// Resolve a client model name to a provider and upstream model.
    ///
    /// The two configured answers are hash lookups. The unknown-model error, and
    /// its list of configured models, come from `Config::resolve` so there is
    /// only one copy of that wording.
    pub fn resolve(&self, client_model: &str) -> Result<ResolvedRoute> {
        if let Some(provider) = self.routes.get(client_model) {
            return Ok(ResolvedRoute {
                provider: provider.clone(),
                model: client_model.to_string(),
            });
        }
        if let Some((head, rest)) = client_model.split_once('/')
            && let Some(provider) = self.aliases.get(head)
            && !rest.is_empty()
        {
            return Ok(ResolvedRoute {
                provider: provider.clone(),
                model: rest.to_string(),
            });
        }
        self.config.resolve(client_model)
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

    /// The same snapshot with a newer price table: names, probes and timeouts are
    /// unchanged, so a learned rate does not rebuild anything else.
    pub(crate) fn with_prices(&self, prices: Arc<PriceTable>) -> Arc<Runtime> {
        Arc::new(Runtime {
            config: self.config.clone(),
            routes: self.routes.clone(),
            aliases: self.aliases.clone(),
            probes: self.probes.clone(),
            schedules: self.schedules.clone(),
            http: self.http.clone(),
            prices,
        })
    }
}
