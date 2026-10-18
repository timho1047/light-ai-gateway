//! HTTP front end: the OpenAI-compatible `/v1` surface.
//!
//! [`build_router`] is the one assembly point: the routes, the local-key
//! middleware, the request body limit and the listener. Handlers live in the
//! modules beside this file; the chat-completions retry and relay rules are
//! documented in `chat`.

pub(crate) mod apply;
pub(crate) mod catalog;
mod chat;
pub(crate) mod console;
mod models;
mod usage;

#[cfg(test)]
mod tests;

use crate::accounts::{AttemptPlan, PlanError, Reservation};
use crate::config::{Config, ResolvedRoute};
use crate::domain::{Account, ApiKey, AttemptRecord, RequestRecord, TokenUsage};
use crate::error::UpstreamErrorClass;
use crate::router::Gateway;
use crate::runtime::Runtime;
use crate::tier::TierKey;
use anyhow::{Context, Result};
use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;

/// Providers whose endpoint or adapter changed. Their accounts have to be
/// re-probed before they serve: every reading they hold was taken against a
/// different endpoint.
fn providers_needing_reset(old: &Config, new: &Config) -> Vec<String> {
    let mut changed = Vec::new();
    for (name, provider) in &new.providers {
        if let Some(previous) = old.providers.get(name)
            && (previous.adapter != provider.adapter
                || previous.base_url != provider.base_url
                || previous.probe_base_url != provider.probe_base_url)
        {
            changed.push(name.clone());
        }
    }
    changed
}

/// Why a validated configuration could not be made live. The file and the
/// running snapshot are both unchanged when this is returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyError(pub String);

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ApplyError {}

/// The slice of gateway behaviour the HTTP layer needs.
///
/// This seam exists so the router can be driven end-to-end in tests without a
/// live database or a probed pool. GatewayBackend is the production wiring;
/// every method mirrors a frozen Gateway/pool signature.
///
/// Configuration is never lent out by the backend: a handler captures the
/// [`Runtime`] once at admission with [`ProxyBackend::runtime`] and passes it to
/// every method that needs configuration, so a later publication cannot change
/// the route, timeout or price an in-flight request is running under.
pub trait ProxyBackend: Send + Sync + 'static {
    /// The configuration snapshot new requests are admitted under.
    fn runtime(&self) -> Arc<Runtime>;

    fn resolve(&self, runtime: &Runtime, client_model: &str) -> Result<ResolvedRoute>;

    // Eight arguments, each a different fact about the attempt: the snapshot it
    // admits under, the model, the session, the estimate, and which attempt of
    // which request this is. A struct would move the same list one line up.
    #[allow(clippy::too_many_arguments)]
    fn plan(
        &self,
        runtime: &Runtime,
        client_model: &str,
        session_key: Option<&str>,
        est_cost: Decimal,
        request_id: &str,
        attempt_no: u32,
        exclude: &[String],
    ) -> std::result::Result<(ResolvedRoute, Reservation), PlanError>;

    /// Pick an account for a request that has no model of its own, such as the
    /// model-list passthrough.
    fn pick_account(
        &self,
        provider: &str,
        est_cost: Decimal,
        request_id: &str,
        attempt_no: u32,
    ) -> std::result::Result<AttemptPlan, PlanError>;

    /// Look up an account by canonical provider/id identity.
    fn account(&self, qualified_id: &str) -> Option<Account>;

    /// Every account the pool knows, with its last-known gates.
    ///
    /// What the poller last saw, never a fresh probe. The usage report is read
    /// on a dashboard's timer, and a report that costs an upstream call is one
    /// nobody can afford to leave open.
    fn snapshot(&self) -> Vec<Account>;

    /// Requests currently committed across the pool. A dashboard number, not a
    /// routing input; backends without a pool report zero.
    fn in_flight(&self) -> u32 {
        0
    }

    /// The Board's vitals from the request history, or None when there is no
    /// ledger to read yet.
    fn board_vitals(&self) -> Option<crate::store::BoardVitals> {
        None
    }

    /// Learned prices that have drifted materially from the configured rate.
    fn price_drift(&self) -> Vec<(String, String, TierKey, crate::meter::PriceDrift)> {
        Vec::new()
    }

    /// The Usage page's ledger aggregation, read-only. None when the ledger
    /// cannot be opened; the page then says so instead of showing zeros.
    fn usage_report(
        &self,
        _now: time::OffsetDateTime,
        _since: time::OffsetDateTime,
        _dims: &[crate::domain::GroupBy],
    ) -> Option<crate::store::UsageReport> {
        None
    }

    /// The credential report and operations the Keys page uses. The value never
    /// crosses any of these seams.
    fn keys_report(&self) -> Option<crate::keys::KeysReport> {
        None
    }

    fn key_add(
        &self,
        _provider: &str,
        _account: &str,
        _secret: &str,
    ) -> std::result::Result<(), ApplyError> {
        Err(ApplyError(
            "this backend cannot store credentials".to_string(),
        ))
    }

    fn key_remove(&self, _provider: &str, _account: &str) -> std::result::Result<bool, ApplyError> {
        Err(ApplyError(
            "this backend cannot remove credentials".to_string(),
        ))
    }

    fn key_test<'a>(
        &'a self,
        _provider: &'a str,
        _account: &'a str,
    ) -> Pin<
        Box<
            dyn std::future::Future<
                    Output = std::result::Result<crate::keys::KeyTestOutcome, ApplyError>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async {
            Err(ApplyError(
                "this backend cannot test credentials".to_string(),
            ))
        })
    }

    /// Probe one account or all of them, through the configured providers.
    fn probe_accounts<'a>(
        &'a self,
        _provider: Option<&'a str>,
        _account: Option<&'a str>,
    ) -> Pin<Box<dyn std::future::Future<Output = std::result::Result<(), ApplyError>> + Send + 'a>>
    {
        Box::pin(async { Err(ApplyError("this backend cannot probe accounts".to_string())) })
    }

    /// Operator disable/enable on the live pool. Enable returns the account to
    /// Unknown; only a probe makes it serve again.
    fn set_account_enabled(
        &self,
        _provider: &str,
        _account: &str,
        _enabled: bool,
    ) -> std::result::Result<(), ApplyError> {
        Err(ApplyError(
            "this backend cannot change account state".to_string(),
        ))
    }

    /// The configured-plus-learned price table the page reports on, read-only.
    fn price_table(&self) -> Option<crate::meter::PriceTable> {
        None
    }

    /// Forget learned prices in the given scope, write the ledger, and republish
    /// the live table so the configured card is back in force.
    fn forget_prices(
        &self,
        _provider: Option<&str>,
        _model: Option<&str>,
    ) -> std::result::Result<usize, ApplyError> {
        Err(ApplyError("this backend cannot forget prices".to_string()))
    }

    /// Recent requests with their attempt-level detail, read-only.
    fn request_history(
        &self,
        _limit: u32,
        _errors_only: bool,
        _coverage: Option<crate::domain::GateClass>,
    ) -> Option<Vec<crate::store::RequestHistory>> {
        None
    }

    /// Build the next snapshot, persist the file through `write`, then publish.
    ///
    /// Order is the guarantee: a build failure happens before the write, and a
    /// write failure before the publish, so a failed apply leaves both the file
    /// and the running snapshot exactly as they were.
    fn apply_config(
        &self,
        _config: Config,
        _write: &mut dyn FnMut() -> std::io::Result<()>,
    ) -> std::result::Result<(), ApplyError> {
        Err(ApplyError(
            "this backend cannot apply configuration".to_string(),
        ))
    }

    /// Refresh an account's quota reading immediately before switching to it.
    ///
    /// A boxed future rather than an async-trait method so the default can be a
    /// no-op and test backends need no implementation. The real gateway honours
    /// policy.probe_before_switch; without this call that setting was parsed and
    /// then never read, so a switch could land on an account that had already
    /// filled up since the last poll.
    fn ensure_fresh<'a>(
        &'a self,
        _runtime: &'a Runtime,
        _provider: &'a str,
        _account_id: &'a str,
    ) -> Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async {})
    }

    fn retry_after_for(&self, provider: &str) -> Option<Duration>;

    fn http(&self, runtime: &Runtime) -> reqwest::Client;

    /// Which price window a request starting now falls into.
    fn tier_at(&self, runtime: &Runtime, provider: &str, model: &str) -> TierKey;

    /// Price a usage sample through the learned price table. None means the
    /// model is unknown and the caller must fall back conservatively.
    fn estimate_cost(
        &self,
        runtime: &Runtime,
        provider: &str,
        model: &str,
        tier: &TierKey,
        usage: &TokenUsage,
        multiplier: Decimal,
    ) -> Option<Decimal>;

    fn record_success(
        &self,
        reservation: Reservation,
        usage: &TokenUsage,
        cost: Decimal,
        attempt: AttemptRecord,
        record: RequestRecord,
    );

    fn record_failure(
        &self,
        reservation: Reservation,
        class: UpstreamErrorClass,
        retry_after: Option<Duration>,
        window_reset: Option<OffsetDateTime>,
        attempt: AttemptRecord,
    );

    /// Persist a request-level row once every attempt has failed.
    ///
    /// Attempts are already stored individually; this exists so a total failure
    /// still shows up in history and usage reports instead of leaving orphaned
    /// attempt rows behind.
    fn record_failed_request(&self, _record: RequestRecord) {}
}

/// Production wiring: a thin adapter over the frozen Gateway API.
pub struct GatewayBackend {
    gateway: Arc<Gateway>,
}

impl GatewayBackend {
    pub fn new(gateway: Arc<Gateway>) -> Self {
        GatewayBackend { gateway }
    }
}

impl ProxyBackend for GatewayBackend {
    fn runtime(&self) -> Arc<Runtime> {
        self.gateway.runtime()
    }

    fn resolve(&self, runtime: &Runtime, client_model: &str) -> Result<ResolvedRoute> {
        runtime.resolve(client_model)
    }

    fn plan(
        &self,
        runtime: &Runtime,
        client_model: &str,
        session_key: Option<&str>,
        est_cost: Decimal,
        request_id: &str,
        attempt_no: u32,
        exclude: &[String],
    ) -> std::result::Result<(ResolvedRoute, Reservation), PlanError> {
        self.gateway.plan(
            runtime,
            client_model,
            session_key,
            est_cost,
            request_id,
            attempt_no,
            exclude,
        )
    }

    fn pick_account(
        &self,
        provider: &str,
        est_cost: Decimal,
        request_id: &str,
        attempt_no: u32,
    ) -> std::result::Result<AttemptPlan, PlanError> {
        let reservation =
            self.gateway
                .pool
                .select(provider, "", None, est_cost, request_id, attempt_no, &[])?;
        // A model-list passthrough spends no tokens, so it gives the commitment
        // straight back. Leaving it committed burned one concurrency slot per
        // call against the account's own concurrency_limit, and after N calls
        // that account could never be chosen again.
        let plan = reservation.plan().clone();
        reservation.settle(Decimal::ZERO, false);
        Ok(plan)
    }

    fn account(&self, qualified_id: &str) -> Option<Account> {
        self.gateway.pool.account(qualified_id)
    }

    fn snapshot(&self) -> Vec<Account> {
        self.gateway.snapshot()
    }

    fn in_flight(&self) -> u32 {
        self.gateway.pool.in_flight_total()
    }

    fn board_vitals(&self) -> Option<crate::store::BoardVitals> {
        let path = self.gateway.runtime().config().db_path();
        crate::store::Ledger::open(&path)
            .ok()
            .and_then(|ledger| ledger.board_vitals(crate::domain::now()).ok())
    }

    fn price_drift(&self) -> Vec<(String, String, TierKey, crate::meter::PriceDrift)> {
        self.gateway.runtime().prices().drifted()
    }

    fn keys_report(&self) -> Option<crate::keys::KeysReport> {
        Some(crate::keys::report(
            self.gateway.runtime().config(),
            &self.gateway.credentials,
        ))
    }

    fn key_add(
        &self,
        provider: &str,
        account: &str,
        secret: &str,
    ) -> std::result::Result<(), ApplyError> {
        let config = self.gateway.runtime().config().clone();
        crate::keys::store(
            &config,
            &self.gateway.credentials,
            provider,
            account,
            secret,
        )
        .map_err(|error| ApplyError(error.to_string()))
    }

    fn key_remove(&self, provider: &str, account: &str) -> std::result::Result<bool, ApplyError> {
        crate::keys::remove(&self.gateway.credentials, provider, account)
            .map_err(|error| ApplyError(error.to_string()))
    }

    fn key_test<'a>(
        &'a self,
        provider: &'a str,
        account: &'a str,
    ) -> Pin<
        Box<
            dyn std::future::Future<
                    Output = std::result::Result<crate::keys::KeyTestOutcome, ApplyError>,
                > + Send
                + 'a,
        >,
    > {
        let config = self.gateway.runtime().config().clone();
        let credentials = self.gateway.credentials.clone();
        let provider = provider.to_string();
        let account = account.to_string();
        Box::pin(async move {
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .map_err(|error| ApplyError(error.to_string()))?;
            crate::keys::probe(
                &config,
                &credentials,
                &provider,
                &account,
                &client,
                std::time::Duration::from_secs(10),
            )
            .await
            .map_err(|error| ApplyError(error.to_string()))
        })
    }

    fn probe_accounts<'a>(
        &'a self,
        provider: Option<&'a str>,
        account: Option<&'a str>,
    ) -> Pin<Box<dyn std::future::Future<Output = std::result::Result<(), ApplyError>> + Send + 'a>>
    {
        let gateway = self.gateway.clone();
        let provider = provider.map(str::to_string);
        let account = account.map(str::to_string);
        Box::pin(async move {
            match (provider.as_deref(), account.as_deref()) {
                (Some(provider), Some(account)) => {
                    gateway.probe_account(provider, account).await;
                }
                (None, None) => gateway.probe_all().await,
                _ => {
                    return Err(ApplyError(
                        "a probe needs both a provider and an account, or neither".to_string(),
                    ));
                }
            }
            Ok(())
        })
    }

    fn set_account_enabled(
        &self,
        provider: &str,
        account: &str,
        enabled: bool,
    ) -> std::result::Result<(), ApplyError> {
        let qualified = crate::domain::qualified_id(provider, account);
        let snapshot = self
            .gateway
            .pool
            .set_operator_state(&qualified, enabled)
            .map_err(ApplyError)?;
        self.gateway.store.save_account(&snapshot);
        self.gateway
            .store
            .flush_blocking()
            .map_err(|error| ApplyError(error.to_string()))?;
        Ok(())
    }

    fn price_table(&self) -> Option<crate::meter::PriceTable> {
        Some(crate::prices::load(self.gateway.runtime().config()))
    }

    fn forget_prices(
        &self,
        provider: Option<&str>,
        model: Option<&str>,
    ) -> std::result::Result<usize, ApplyError> {
        let current = self.gateway.runtime();
        let table = current.prices();
        let in_scope = |row: &crate::meter::PersistedPrice| {
            provider.is_none_or(|provider| row.provider == provider)
                && model.is_none_or(|model| row.model == model)
        };
        let learned = table.learned_rows();
        let forgotten = learned.iter().filter(|row| in_scope(row)).count();
        if forgotten == 0 {
            return Ok(0);
        }

        // Same ledger write as the CLI, then wait for it so the page reads the
        // new state immediately.
        self.gateway.store.forget_prices(provider, model);
        self.gateway
            .store
            .flush_blocking()
            .map_err(|error| ApplyError(error.to_string()))?;

        // Rebuild without the forgotten rates: residual learned entries plus the
        // configured card, and no second trip through the ledger.
        let keep: Vec<crate::meter::PersistedPrice> =
            learned.into_iter().filter(|row| !in_scope(row)).collect();
        let mut next = crate::meter::PriceTable::new();
        next.load_learned(keep);
        next.seed_from_config(&current.config().prices);
        self.gateway
            .install_runtime(current.with_prices(Arc::new(next)));
        Ok(forgotten)
    }

    fn request_history(
        &self,
        limit: u32,
        errors_only: bool,
        coverage: Option<crate::domain::GateClass>,
    ) -> Option<Vec<crate::store::RequestHistory>> {
        let path = self.gateway.runtime().config().db_path();
        crate::store::Ledger::open(&path)
            .ok()?
            .request_history(limit, errors_only, coverage)
            .ok()
    }

    fn usage_report(
        &self,
        now: time::OffsetDateTime,
        since: time::OffsetDateTime,
        dims: &[crate::domain::GroupBy],
    ) -> Option<crate::store::UsageReport> {
        let path = self.gateway.runtime().config().db_path();
        crate::store::Ledger::open(&path)
            .ok()?
            .usage_report(now, since, dims)
            .ok()
    }

    fn apply_config(
        &self,
        config: Config,
        write: &mut dyn FnMut() -> std::io::Result<()>,
    ) -> std::result::Result<(), ApplyError> {
        let current = self.gateway.runtime();
        let reset_providers = providers_needing_reset(current.config(), &config);
        let prices = Arc::new(current.prices().reseeded(&config.prices));
        let runtime = Runtime::new(Arc::new(config.clone()), prices)
            .map_err(|error| ApplyError(error.to_string()))?;

        // Everything the credential store can refuse happens before the write.
        let (mut accounts, unusable) = config
            .build_accounts_lenient(&self.gateway.credentials)
            .map_err(|error| ApplyError(error.to_string()))?;
        accounts.extend(unusable);
        if accounts.is_empty() {
            return Err(ApplyError(
                "no account could be loaded: every configured account is missing a credential"
                    .to_string(),
            ));
        }

        write().map_err(|error| ApplyError(format!("writing the configuration: {error}")))?;
        self.gateway.pool.retune(&config.policy);
        self.gateway.install_runtime(Arc::new(runtime));
        let outcome = self.gateway.pool.reconfigure(accounts, &reset_providers);

        // New and reset accounts stay out of selection until a probe succeeds.
        // The apply has already succeeded; a probe that fails publishes its
        // error through the account's last_error on the next attempt.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            for (provider, account) in outcome.probe {
                let gateway = self.gateway.clone();
                handle.spawn(async move {
                    gateway.probe_account(&provider, &account).await;
                });
            }
        }
        Ok(())
    }

    fn ensure_fresh<'a>(
        &'a self,
        runtime: &'a Runtime,
        provider: &'a str,
        account_id: &'a str,
    ) -> Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            self.gateway
                .ensure_fresh(runtime, provider, account_id)
                .await;
        })
    }

    fn retry_after_for(&self, provider: &str) -> Option<Duration> {
        self.gateway.pool.retry_after_for(provider)
    }

    fn http(&self, runtime: &Runtime) -> reqwest::Client {
        runtime.http().clone()
    }

    fn tier_at(&self, runtime: &Runtime, provider: &str, model: &str) -> TierKey {
        runtime.tier_at(provider, model, crate::domain::now())
    }

    fn estimate_cost(
        &self,
        runtime: &Runtime,
        provider: &str,
        model: &str,
        tier: &TierKey,
        usage: &TokenUsage,
        multiplier: Decimal,
    ) -> Option<Decimal> {
        runtime
            .prices()
            .estimate(provider, model, tier, usage, multiplier)
    }

    fn record_success(
        &self,
        reservation: Reservation,
        usage: &TokenUsage,
        cost: Decimal,
        attempt: AttemptRecord,
        record: RequestRecord,
    ) {
        self.gateway
            .record_success(reservation, usage, cost, attempt, record);
    }

    fn record_failure(
        &self,
        reservation: Reservation,
        class: UpstreamErrorClass,
        retry_after: Option<Duration>,
        window_reset: Option<OffsetDateTime>,
        attempt: AttemptRecord,
    ) {
        self.gateway
            .record_failure(reservation, class, retry_after, window_reset, attempt);
    }

    fn record_failed_request(&self, record: RequestRecord) {
        self.gateway.record_failed_request(record);
    }
}

/// Run the proxy and the console until shutdown.
pub async fn serve(config: Config, config_path: &std::path::Path) -> Result<()> {
    let listen = config.server.listen.clone();
    let addr: std::net::SocketAddr = listen
        .parse()
        .with_context(|| format!("invalid server.listen address {listen}"))?;
    let config_path = config_path.display().to_string();
    let gateway = Gateway::new(config).await?;
    gateway.spawn_poller();
    // Bind before building the router: the console's Host allowlist has to pin
    // the port the kernel actually gave us, not the one in the file (which may
    // be 0 for a test).
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    let local = listener.local_addr().context("reading the bound address")?;
    let console = console::ConsoleContext::new(&local.to_string(), local.port(), &config_path);
    if let Some(result) = crate::readiness::publish(&format!("http://{local}/"))
        && let Err(error) = result
    {
        tracing::warn!(error = %error, "could not publish the startup record");
    }
    let shutdown_gateway = gateway.clone();
    let backend = Arc::new(GatewayBackend::new(gateway));
    let app = build_router_with_console(backend.clone(), console.clone());
    // The file is already live: mark it handled so the first watcher tick does
    // not publish a second, identical snapshot.
    if let Ok(raw) = std::fs::read_to_string(&config_path) {
        crate::reload::mark_loaded(
            &console.reload,
            &crate::config_doc::fingerprint(&raw),
            crate::domain::now(),
        );
    }
    let _watcher = crate::reload::spawn(
        backend,
        std::path::PathBuf::from(&config_path),
        console.clone(),
    );
    tracing::info!(
        listen = %local,
        console = %format!("http://{local}/"),
        "light-ai-gateway listening"
    );
    // SIGTERM is how lgwl exit asks; SIGINT keeps Ctrl-C working. The server
    // stops accepting, lets open requests finish, then flushes the ledger. A
    // streaming response that never ends would hold the process open forever,
    // so a watchdog flushes and exits after a bounded wait.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = shutdown_tx.send(true);
    });
    let mut force_rx = shutdown_rx.clone();
    let force_gateway = shutdown_gateway.clone();
    let force = tokio::spawn(async move {
        while !*force_rx.borrow_and_update() {
            if force_rx.changed().await.is_err() {
                return;
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(8)).await;
        tracing::warn!("graceful shutdown timed out; flushing the ledger and exiting");
        if let Err(error) = force_gateway.shutdown() {
            tracing::error!(error = %error, "final ledger flush failed");
        }
        std::process::exit(0);
    });
    let shutdown = {
        let mut rx = shutdown_rx;
        async move {
            while !*rx.borrow_and_update() {
                if rx.changed().await.is_err() {
                    break;
                }
            }
        }
    };
    let served = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await;
    force.abort();
    served.context("serving HTTP")?;
    shutdown_gateway
        .shutdown()
        .context("flushing the ledger on shutdown")?;
    tracing::info!("light-ai-gateway stopped");
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
        let signal = async {
            match terminate.as_mut() {
                Some(terminate) => {
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {}
                        _ = terminate.recv() => {}
                    }
                }
                None => {
                    let _ = tokio::signal::ctrl_c().await;
                }
            }
        };
        signal.await;
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    tracing::info!("shutdown signal received; draining");
}

/// Build the axum router. Exposed for integration tests.
pub fn build_router(gateway: Arc<Gateway>) -> axum::Router {
    build_router_with(Arc::new(GatewayBackend::new(gateway)))
}

/// Build the router against any backend. Exposed for integration tests.
pub fn build_router_with(backend: Arc<dyn ProxyBackend>) -> axum::Router {
    build_router_with_console(backend, console::ConsoleContext::for_tests())
}

/// Build the router with the listener context the console needs. Production
/// passes the bound address; tests pass an ephemeral one.
pub(crate) fn build_router_with_console(
    backend: Arc<dyn ProxyBackend>,
    console: console::ConsoleContext,
) -> axum::Router {
    // Authentication and the body limit are startup decisions: they follow the
    // snapshot installed when the listener was assembled, not a later publish.
    let runtime = backend.runtime();
    let max_body_bytes = runtime.config().policy.max_body_bytes;
    let auth = match &runtime.config().server.local_key {
        None => {
            // Serving without a local key means anything that can reach the port
            // can spend this account's quota. The configuration template calls
            // the key "strongly recommended", and then nothing said a word when
            // it was missing -- the operator's only evidence was a template they
            // had already closed.
            tracing::warn!(
                listen = %runtime.config().server.listen,
                "server.local_key is not configured: anything able to reach this address \
                 can spend the pooled quota. Set one with 'lgw keys add local gateway', \
                 then local_key = {{ keychain = true }}"
            );
            LocalAuth::Open
        }
        Some(secret) => match secret.resolve(
            &crate::secrets::CredentialStore::from_env(),
            crate::secrets::LOCAL_KEY_PROVIDER,
            crate::secrets::LOCAL_KEY_ACCOUNT,
        ) {
            Ok(key) => LocalAuth::Required(key),
            Err(e) => {
                // Fail closed: without our key on disk this proxy would be an
                // open door to paid subscription credentials.
                tracing::error!(error = %e, "local_key could not be resolved; refusing every request");
                LocalAuth::Broken
            }
        },
    };
    let state = Arc::new(AppState {
        backend,
        auth,
        console,
    });

    let api = Router::new()
        .route("/v1/chat/completions", post(chat::chat_completions))
        .route("/v1/models", get(models::list_models))
        .route("/v1/usage", get(usage::usage))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_local_key,
        ));

    // The console is a separate surface with its own guard: the local key
    // protects paid quota from clients, and the loopback rule protects the
    // configuration from the network. A request never passes through both.
    let operator = Router::new()
        .route("/", get(console::board_page))
        .route("/usage", get(console::usage_page))
        .route("/requests", get(console::requests_page))
        .route("/prices", get(console::prices_page))
        .route("/doctor", get(console::doctor_page))
        .route("/keys", get(console::keys_page))
        .route("/console/api/keys/add", post(console::keys_add))
        .route("/console/api/keys/test", post(console::keys_test))
        .route("/console/api/keys/remove", post(console::keys_remove))
        .route("/console/api/accounts/probe", post(console::accounts_probe))
        .route("/console/api/accounts/state", post(console::accounts_state))
        .route("/console/api/prices/forget", post(console::prices_forget))
        .route("/config", get(console::config_page_handler))
        .route(
            "/console/api/config/validate",
            post(console::config_validate),
        )
        .route("/console/api/config/apply", post(console::config_apply))
        .route("/console/partials/board", get(console::board_partial))
        .route("/console/assets/{*path}", get(console::asset))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            console::guard_console,
        ))
        .layer(middleware::from_fn(console::security_headers));

    Router::new()
        .merge(api)
        .merge(operator)
        .layer(DefaultBodyLimit::max(max_body_bytes))
        .with_state(state)
}

struct AppState {
    backend: Arc<dyn ProxyBackend>,
    auth: LocalAuth,
    console: console::ConsoleContext,
}

/// Local bearer authentication state.
enum LocalAuth {
    /// No local_key configured.
    Open,
    Required(ApiKey),
    /// local_key is configured but unreadable: reject everything.
    Broken,
}

impl LocalAuth {
    fn authorizes(&self, headers: &HeaderMap) -> bool {
        let required = match self {
            LocalAuth::Open => return true,
            LocalAuth::Broken => return false,
            LocalAuth::Required(key) => key,
        };
        let Some(value) = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
        else {
            return false;
        };
        let Some((scheme, token)) = value.split_at_checked(7) else {
            return false;
        };
        if !scheme.eq_ignore_ascii_case("bearer ") {
            return false;
        }
        constant_time_eq(token.trim().as_bytes(), required.expose().as_bytes())
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    crate::secrets::constant_time_eq(a, b)
}

async fn require_local_key(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    if state.auth.authorizes(request.headers()) {
        next.run(request).await
    } else {
        error_response(
            StatusCode::UNAUTHORIZED,
            "invalid_api_key",
            "missing or invalid Authorization bearer token",
        )
    }
}

/// Read at most the given number of bytes of an upstream error body.
async fn read_bounded(mut response: reqwest::Response, limit: usize) -> String {
    let mut buf: Vec<u8> = Vec::new();
    while buf.len() < limit {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let take = chunk.len().min(limit - buf.len());
                buf.extend_from_slice(&chunk[..take]);
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Pull the human-readable message out of an OpenAI/Anthropic error envelope.
fn upstream_error_message(body: &str, key: &ApiKey) -> Option<String> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }
    let value: Value = serde_json::from_str(trimmed).ok()?;
    let obj = value.get("error").unwrap_or(&value);
    let message = obj
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| value.get("message").and_then(Value::as_str))?;
    let message = message.trim();
    if message.is_empty() {
        None
    } else {
        // The text comes from the upstream, so it goes through the presented
        // key's redaction before it can reach the client: a provider that
        // echoes the credential back in its error body would otherwise hand it
        // to anyone who asked. The probe path has always done this.
        Some(key.redact(message))
    }
}

fn sanitize_message(message: &str) -> String {
    let cleaned: String = message
        .chars()
        .filter(|c| !c.is_control())
        .take(300)
        .collect();
    if cleaned.trim().is_empty() {
        "upstream request failed".to_string()
    } else {
        cleaned
    }
}

/// A JSON body with the status the OpenAI SDKs expect from a success.
fn json_response(payload: Value) -> Response {
    let mut response = Response::new(Body::from(payload.to_string()));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    response
}

fn error_response(status: StatusCode, kind: &str, message: &str) -> Response {
    error_response_retry(status, kind, message, None)
}

fn error_response_retry(
    status: StatusCode,
    kind: &str,
    message: &str,
    retry_after: Option<Duration>,
) -> Response {
    let payload = json!({
        "error": {
            "message": sanitize_message(message),
            "type": kind,
            "code": kind,
        }
    });
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(delay) = retry_after {
        builder = builder.header(header::RETRY_AFTER, delay.as_secs().to_string());
    }
    builder
        .body(Body::from(payload.to_string()))
        .unwrap_or_else(|_| Response::new(Body::from("{}")))
}
