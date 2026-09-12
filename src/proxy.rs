//! HTTP front end: the OpenAI-compatible endpoint and the retry loop.
//!
//! FILE OWNER: teammate proxy-dev. Replace stub bodies; keep public signatures.
//!
//! Hard rules:
//!   - An account switch may only happen BEFORE the first byte is written to
//!     the client. Connect upstream, inspect the status, and only then start
//!     relaying.
//!   - Never buffer a whole response body. Relay bytes as they arrive and tee
//!     them into the SSE meter on the side.
//!   - When the client disconnects, abort the upstream request so billing stops.
//!   - Bound retries. Streaming retries can double-bill, so a request that has
//!     already delivered bytes must never be retried.
//!
//! The handler therefore works in two phases:
//!   1. Plan and probe: ask the pool for an account, send the upstream request,
//!      classify a non-2xx response and retry or fail over. Nothing reaches the
//!      client in this phase, so an account switch is still safe.
//!   2. Relay: wrap the upstream byte stream in RelayStream, which forwards
//!      every chunk unchanged, feeds a StreamMeter on the side, stamps TTFT,
//!      and records the request when the stream ends or the client goes away.

use crate::accounts::{AttemptPlan, PlanError};
use crate::config::{Config, ResolvedRoute};
use crate::domain::{
    now, Account, ApiKey, AttemptRecord, GateClass, RequestRecord, RequestStatus, TokenUsage,
};
use crate::error::{parse_error_body, parse_retry_after, ErrorSignals, UpstreamErrorClass};
use crate::router::Gateway;
use crate::sse::{inject_include_usage, MeterMode, StreamMeter};
use anyhow::{Context, Result};
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use futures_util::Stream;
use rust_decimal::Decimal;
use serde_json::{json, Value};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant};
use time::OffsetDateTime;

/// Longest backoff the retry loop will sleep. A quota window is handled by
/// rotating accounts, not by stalling the client for an hour.
const MAX_BACKOFF: Duration = Duration::from_secs(2);

/// The slice of gateway behaviour the HTTP layer needs.
///
/// This seam exists so the router can be driven end-to-end in tests without a
/// live database or a probed pool. GatewayBackend is the production wiring;
/// every method mirrors a frozen Gateway/pool signature.
pub trait ProxyBackend: Send + Sync + 'static {
    fn config(&self) -> &Config;

    fn resolve(&self, client_model: &str) -> Result<ResolvedRoute>;

    fn plan(
        &self,
        client_model: &str,
        session_key: Option<&str>,
        est_cost: Decimal,
        request_id: &str,
        attempt_no: u32,
    ) -> std::result::Result<(ResolvedRoute, AttemptPlan), PlanError>;

    /// Pick an account for a request that has no model of its own, such as the
    /// model-list passthrough.
    fn pick_account(
        &self,
        provider: &str,
        est_cost: Decimal,
        request_id: &str,
        attempt_no: u32,
    ) -> std::result::Result<AttemptPlan, PlanError>;

    fn account(&self, account_id: &str) -> Option<Account>;

    fn retry_after_for(&self, provider: &str) -> Option<Duration>;

    fn http(&self) -> reqwest::Client;

    /// Price a usage sample through the learned price table. None means the
    /// model is unknown and the caller must fall back conservatively.
    fn estimate_cost(&self, model: &str, usage: &TokenUsage, multiplier: Decimal) -> Option<Decimal>;

    fn record_success(
        &self,
        plan: &AttemptPlan,
        usage: &TokenUsage,
        cost: Decimal,
        attempt: AttemptRecord,
        record: RequestRecord,
    );

    fn record_failure(
        &self,
        plan: &AttemptPlan,
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
    fn config(&self) -> &Config {
        &self.gateway.config
    }

    fn resolve(&self, client_model: &str) -> Result<ResolvedRoute> {
        self.gateway.resolve(client_model)
    }

    fn plan(
        &self,
        client_model: &str,
        session_key: Option<&str>,
        est_cost: Decimal,
        request_id: &str,
        attempt_no: u32,
    ) -> std::result::Result<(ResolvedRoute, AttemptPlan), PlanError> {
        self.gateway
            .plan(client_model, session_key, est_cost, request_id, attempt_no)
    }

    fn pick_account(
        &self,
        provider: &str,
        est_cost: Decimal,
        request_id: &str,
        attempt_no: u32,
    ) -> std::result::Result<AttemptPlan, PlanError> {
        self.gateway
            .pool
            .select(provider, "", None, est_cost, request_id, attempt_no)
    }

    fn account(&self, account_id: &str) -> Option<Account> {
        self.gateway.pool.account(account_id)
    }

    fn retry_after_for(&self, provider: &str) -> Option<Duration> {
        self.gateway.pool.retry_after_for(provider)
    }

    fn http(&self) -> reqwest::Client {
        self.gateway.http.clone()
    }

    fn estimate_cost(&self, model: &str, usage: &TokenUsage, multiplier: Decimal) -> Option<Decimal> {
        self.gateway
            .prices
            .lock()
            .ok()
            .and_then(|prices| prices.estimate(model, usage, multiplier))
    }

    fn record_success(
        &self,
        plan: &AttemptPlan,
        usage: &TokenUsage,
        cost: Decimal,
        attempt: AttemptRecord,
        record: RequestRecord,
    ) {
        self.gateway
            .record_success(plan, usage, cost, attempt, record);
    }

    fn record_failure(
        &self,
        plan: &AttemptPlan,
        class: UpstreamErrorClass,
        retry_after: Option<Duration>,
        window_reset: Option<OffsetDateTime>,
        attempt: AttemptRecord,
    ) {
        self.gateway
            .record_failure(plan, class, retry_after, window_reset, attempt);
    }

    fn record_failed_request(&self, record: RequestRecord) {
        self.gateway.record_failed_request(record);
    }
}

/// Run the gateway until shutdown.
pub async fn serve(config: Config) -> Result<()> {
    let listen = config.server.listen.clone();
    let addr: std::net::SocketAddr = listen
        .parse()
        .with_context(|| format!("invalid server.listen address {listen}"))?;
    let gateway = Gateway::new(config).await?;
    gateway.spawn_poller();
    let app = build_router(gateway);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!(%addr, "light-ai-gateway listening");
    axum::serve(listener, app).await.context("serving HTTP")?;
    Ok(())
}

/// Build the axum router. Exposed for integration tests.
pub fn build_router(gateway: Arc<Gateway>) -> axum::Router {
    build_router_with(Arc::new(GatewayBackend::new(gateway)))
}

/// Build the router against any backend. Exposed for integration tests.
pub fn build_router_with(backend: Arc<dyn ProxyBackend>) -> axum::Router {
    let max_body_bytes = backend.config().policy.max_body_bytes;
    let auth = match &backend.config().server.local_key {
        None => LocalAuth::Open,
        Some(secret) => match secret.resolve(
            &crate::secrets::CredentialStore::keychain(),
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
    let state = Arc::new(AppState { backend, auth });
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/models", get(list_models))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_local_key,
        ))
        .layer(DefaultBodyLimit::max(max_body_bytes))
        .with_state(state)
}

struct AppState {
    backend: Arc<dyn ProxyBackend>,
    auth: LocalAuth,
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
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
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

async fn chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Ok(mut value) = serde_json::from_slice::<Value>(&body) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "request body is not valid JSON",
        );
    };
    if !value.is_object() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "request body must be a JSON object",
        );
    }
    let Some(client_model) = value
        .get("model")
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty())
        .map(str::to_string)
    else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "request must carry a non-empty \"model\"",
        );
    };
    let stream = value.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let session_key = session_key(&headers, &value);
    let request_id = uuid::Uuid::new_v4().to_string();
    // Captured outside the retry loop so a total failure can still be recorded.
    let request_started = now();

    let route = match state.backend.resolve(&client_model) {
        Ok(route) => route,
        Err(e) => {
            return error_response(
                StatusCode::NOT_FOUND,
                "invalid_request_error",
                &e.to_string(),
            );
        }
    };

    let estimated_prompt_tokens = crate::meter::estimate_prompt_tokens(&value);
    let est_cost = estimate_request_cost(state.backend.as_ref(), &route.model, &value);

    // Without this flag the upstream never emits a usage frame and the local
    // ledger records zero forever, which silently disables cost control.
    let mut upstream_body = body.clone();
    if stream && inject_include_usage(&mut value)
        && let Ok(bytes) = serde_json::to_vec(&value) {
            upstream_body = Bytes::from(bytes);
        }

    let max_attempts = state.backend.config().policy.max_retries.max(1);
    let connect_timeout = Duration::from_secs(
        state.backend.config().policy.connect_timeout_secs.max(1),
    );

    let mut attempt_no: u32 = 1;
    let mut last_failure: Option<Failure> = None;
    let mut last_plan: Option<AttemptPlan> = None;

    while attempt_no <= max_attempts {
        let plan = match state.backend.plan(
            &client_model,
            session_key.as_deref(),
            est_cost,
            &request_id,
            attempt_no,
        ) {
            Ok((_route, plan)) => plan,
            Err(err) => return plan_error_response(&err, state.backend.as_ref()),
        };
        last_plan = Some(plan.clone());

        let Some(account) = state.backend.account(&plan.account_id) else {
            tracing::error!(account = %plan.account_id, "the planned account vanished from the pool");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "the planned account is no longer in the pool",
            );
        };

        let url = format!("{}/chat/completions", account.base_url.trim_end_matches('/'));
        let started_at = now();
        let started = Instant::now();
        let request = state
            .backend
            .http()
            .post(&url)
            .bearer_auth(account.api_key.expose())
            .header(header::CONTENT_TYPE, "application/json")
            .header(
                header::ACCEPT,
                if stream { "text/event-stream" } else { "application/json" },
            )
            .body(upstream_body.clone());

        // Phase 1: nothing has reached the client yet, so every failure below
        // is still retryable.
        let response = match tokio::time::timeout(connect_timeout, request.send()).await {
            Ok(Ok(response)) => response,
            Ok(Err(e)) => {
                let timed_out = e.is_timeout();
                let class = ErrorSignals {
                    timed_out,
                    transport_failure: !timed_out,
                    ..Default::default()
                }
                .classify();
                record_attempt_failure(
                    state.backend.as_ref(),
                    &plan,
                    class,
                    None,
                    None,
                    None,
                    started_at,
                    attempt_no,
                    &request_id,
                );
                last_failure = Some(Failure {
                    status: status_for_class(class),
                    kind: class,
                    message: format!("could not reach the upstream: {e}"),
                    retry_after: None,
                });
                let retryable = should_retry(class, attempt_no, max_attempts);
                attempt_no += 1;
                if !retryable {
                    break;
                }
                continue;
            }
            Err(_) => {
                let class = UpstreamErrorClass::Timeout;
                record_attempt_failure(
                    state.backend.as_ref(),
                    &plan,
                    class,
                    None,
                    None,
                    None,
                    started_at,
                    attempt_no,
                    &request_id,
                );
                last_failure = Some(Failure {
                    status: status_for_class(class),
                    kind: class,
                    message: format!(
                        "the upstream did not respond within {}s",
                        connect_timeout.as_secs()
                    ),
                    retry_after: None,
                });
                let retryable = should_retry(class, attempt_no, max_attempts);
                attempt_no += 1;
                if !retryable {
                    break;
                }
                continue;
            }
        };

        let status = response.status();
        if !status.is_success() {
            let response_headers = response.headers().clone();
            let text = read_bounded(response, 64 * 1024).await;
            let (error_type, error_code) = parse_error_body(&text);
            let retry_after = response_headers
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|raw| parse_retry_after(raw, now()));

            // xKiro reuses one 429 code for both pacing and exhaustion. Our own
            // ledger is the only independent evidence, and error.rs says a
            // probe-confirmed empty window outranks the header.
            let window_exhausted_hint = state
                .backend
                .account(&plan.account_id)
                .map(|acct| {
                    acct.covered_gates()
                        .any(|g| g.cap > Decimal::ZERO && g.remaining() <= Decimal::ZERO)
                })
                .unwrap_or(false);

            let class = ErrorSignals {
                status: Some(status.as_u16()),
                retry_after,
                error_type,
                error_code: error_code.clone(),
                window_exhausted_hint,
                transport_failure: false,
                timed_out: false,
            }
            .classify();

            record_attempt_failure(
                state.backend.as_ref(),
                &plan,
                class,
                retry_after,
                Some(status.as_u16()),
                error_code,
                started_at,
                attempt_no,
                &request_id,
            );

            let detail = upstream_error_message(&text)
                .unwrap_or_else(|| format!("upstream returned HTTP {}", status.as_u16()));
            // Other maps back to whatever the upstream said; the rest are
            // remapped so the client sees a class, not a provider quirk.
            let client_status = if class == UpstreamErrorClass::Other {
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY)
            } else {
                status_for_class(class)
            };
            let client_retry = retry_after.or_else(|| state.backend.retry_after_for(&plan.provider));
            last_failure = Some(Failure {
                status: client_status,
                kind: class,
                message: format!(
                    "upstream {} ({}) - {}",
                    status.as_u16(),
                    class.as_str(),
                    detail
                ),
                retry_after: client_retry,
            });

            let retryable = should_retry(class, attempt_no, max_attempts);
            attempt_no += 1;
            if !retryable {
                break;
            }
            if matches!(
                class,
                UpstreamErrorClass::RateLimitBurst | UpstreamErrorClass::Overloaded
            )
                && let Some(delay) = retry_after {
                    tokio::time::sleep(delay.min(MAX_BACKOFF)).await;
                }
            continue;
        }

        // Phase 2: 2xx means bytes are about to be delivered. From here the
        // request is not retryable, so the relay owns the record.
        let coverage = plan.coverage;
        let response_headers = response.headers().clone();
        let content_type = response_headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let provider_request_id = provider_request_id(&response_headers);
        let mode = if stream || content_type.contains("text/event-stream") {
            MeterMode::Sse
        } else {
            MeterMode::Json
        };
        let finalizer = Finalizer {
            backend: state.backend.clone(),
            plan,
            request_id: request_id.clone(),
            client_model: client_model.clone(),
            provider_request_id,
            currency: account.currency.clone(),
            credit_multiplier: account.credit_multiplier,
            coverage,
            attempt_no,
            started_at,
            estimated_prompt_tokens,
            status_code: status.as_u16(),
        };
        let meter = match mode {
            MeterMode::Sse => StreamMeter::sse(),
            MeterMode::Json => StreamMeter::json(),
        };
        let relay = RelayStream {
            inner: Box::pin(response.bytes_stream()),
            meter,
            started,
            first_content_at: None,
            bytes_sent: 0,
            finalizer: Some(finalizer),
        };
        let mut builder = Response::builder().status(status);
        if !content_type.is_empty() {
            builder = builder.header(header::CONTENT_TYPE, content_type);
        }
        return builder.body(Body::from_stream(relay)).unwrap_or_else(|_| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "failed to build the upstream response",
            )
        });
    }

    match last_failure {
        Some(failure) => {
            // Every attempt failed. Record the request itself so failures show
            // up in history instead of existing only as orphaned attempt rows.
            if let Some(plan) = last_plan.as_ref()
                && let Some(account) = state.backend.account(&plan.account_id) {
                    let total_ms = (now() - request_started).whole_milliseconds().max(0) as u64;
                    state.backend.record_failed_request(RequestRecord {
                        request_id: request_id.clone(),
                        ts: request_started,
                        session_key: session_key.clone(),
                        client_model: client_model.clone(),
                        provider: plan.provider.clone(),
                        account_id: plan.account_id.clone(),
                        model: plan.model.clone(),
                        usage: TokenUsage::default(),
                        ttft_ms: None,
                        total_ms,
                        tps: None,
                        cost: Decimal::ZERO,
                        currency: account.currency.clone(),
                        credit_multiplier: account.credit_multiplier,
                        coverage: plan.coverage,
                        status: RequestStatus::Error,
                        retries: attempt_no.saturating_sub(1),
                        provider_request_id: None,
                    });
                }
            error_response_retry(
                failure.status,
                failure.kind.as_str(),
                &failure.message,
                failure.retry_after,
            )
        }
        None => error_response(
            StatusCode::BAD_GATEWAY,
            "upstream_error",
            "no upstream attempt was made",
        ),
    }
}

#[derive(serde::Deserialize)]
struct ModelsQuery {
    #[serde(default)]
    provider: Option<String>,
}

/// Pass the model list through for one provider.
async fn list_models(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ModelsQuery>,
) -> Response {
    let provider = {
        let cfg = state.backend.config();
        match query.provider {
            Some(p) if cfg.providers.contains_key(&p) => p,
            Some(p) => {
                return error_response(
                    StatusCode::NOT_FOUND,
                    "invalid_request_error",
                    &format!("unknown provider {p}"),
                );
            }
            None => match cfg.providers.keys().next() {
                Some(p) => p.clone(),
                None => {
                    return error_response(
                        StatusCode::NOT_FOUND,
                        "invalid_request_error",
                        "no providers are configured",
                    );
                }
            },
        }
    };

    let request_id = uuid::Uuid::new_v4().to_string();
    let plan = match state
        .backend
        .pick_account(&provider, Decimal::ZERO, &request_id, 1)
    {
        Ok(plan) => plan,
        Err(err) => return plan_error_response(&err, state.backend.as_ref()),
    };
    let Some(account) = state.backend.account(&plan.account_id) else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "the planned account is no longer in the pool",
        );
    };

    let url = format!("{}/models", account.base_url.trim_end_matches('/'));
    let response = match state
        .backend
        .http()
        .get(&url)
        .bearer_auth(account.api_key.expose())
        .header(header::ACCEPT, "application/json")
        .send()
        .await
    {
        Ok(response) => response,
        Err(e) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                "upstream_error",
                &format!("the upstream is unreachable: {e}"),
            );
        }
    };

    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    if !status.is_success() {
        let text = read_bounded(response, 8 * 1024).await;
        let detail = upstream_error_message(&text)
            .unwrap_or_else(|| format!("upstream returned HTTP {}", status.as_u16()));
        return error_response(StatusCode::BAD_GATEWAY, "upstream_error", &detail);
    }

    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from_stream(response.bytes_stream()))
        .unwrap_or_else(|_| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "failed to build the upstream response",
            )
        })
}

/// One upstream failure that is worth reporting to the client if no retry
/// succeeds.
struct Failure {
    status: StatusCode,
    kind: UpstreamErrorClass,
    message: String,
    retry_after: Option<Duration>,
}

fn should_retry(class: UpstreamErrorClass, attempt_no: u32, max_attempts: u32) -> bool {
    attempt_no < max_attempts && (class.retry_other_account() || class.retry_same_account())
}

fn status_for_class(class: UpstreamErrorClass) -> StatusCode {
    match class {
        UpstreamErrorClass::InsufficientCredit => StatusCode::PAYMENT_REQUIRED,
        UpstreamErrorClass::Auth => StatusCode::UNAUTHORIZED,
        UpstreamErrorClass::QuotaExhausted | UpstreamErrorClass::RateLimitBurst => {
            StatusCode::TOO_MANY_REQUESTS
        }
        UpstreamErrorClass::Overloaded | UpstreamErrorClass::Transport => StatusCode::BAD_GATEWAY,
        UpstreamErrorClass::Timeout => StatusCode::GATEWAY_TIMEOUT,
        UpstreamErrorClass::BadRequest => StatusCode::BAD_REQUEST,
        UpstreamErrorClass::Other => StatusCode::BAD_GATEWAY,
    }
}

fn plan_error_response(err: &PlanError, backend: &dyn ProxyBackend) -> Response {
    match err {
        PlanError::NoEligibleAccount {
            provider,
            retry_after,
        } => {
            // No fall-through to another provider and no silent metered spend:
            // the client is told to come back when the earliest window resets.
            let wait = retry_after.or_else(|| backend.retry_after_for(provider));
            error_response_retry(
                StatusCode::TOO_MANY_REQUESTS,
                "no_eligible_account",
                &err.to_string(),
                wait,
            )
        }
        PlanError::UnknownProvider(provider) => error_response(
            StatusCode::NOT_FOUND,
            "unknown_provider",
            &format!("unknown provider {provider}"),
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn record_attempt_failure(
    backend: &dyn ProxyBackend,
    plan: &AttemptPlan,
    class: UpstreamErrorClass,
    retry_after: Option<Duration>,
    status_code: Option<u16>,
    error_code: Option<String>,
    started_at: OffsetDateTime,
    attempt_no: u32,
    request_id: &str,
) {
    let attempt = AttemptRecord {
        request_id: request_id.to_string(),
        attempt_no,
        account_id: plan.account_id.clone(),
        provider: plan.provider.clone(),
        started_at,
        // The attempt failed before any content was produced, so it must not
        // contribute to TTFT or TPS statistics.
        ttft_ms: None,
        status_code,
        error_class: Some(class),
        error_code,
        retry_after_ms: retry_after.map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64),
        billed: false,
        bytes_sent: 0,
    };
    backend.record_failure(plan, class, retry_after, None, attempt);
}

fn estimate_request_cost(backend: &dyn ProxyBackend, model: &str, body: &Value) -> Decimal {
    let prompt = crate::meter::estimate_prompt_tokens(body);
    let max_tokens = body
        .get("max_tokens")
        .and_then(Value::as_u64)
        .or_else(|| body.get("max_completion_tokens").and_then(Value::as_u64));
    let completion = crate::meter::estimate_completion_tokens(max_tokens, prompt);
    let probe = TokenUsage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        ..Default::default()
    };
    backend
        .estimate_cost(model, &probe, Decimal::ONE)
        // One shared definition of "unknown model" across the whole request
        // path: two different fallback constants would mean the gate and the
        // finalizer disagreed about what a request costs.
        .unwrap_or_else(|| crate::meter::conservative_estimate(&probe, Decimal::ONE))
}

fn session_key(headers: &HeaderMap, body: &Value) -> Option<String> {
    for name in ["x-session-id", "x-session-key", "x-conversation-id"] {
        if let Some(value) = headers.get(name).and_then(|v| v.to_str().ok()) {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    body.get("user")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn provider_request_id(headers: &HeaderMap) -> Option<String> {
    for name in [
        "x-request-id",
        "request-id",
        "openai-request-id",
        "x-amzn-requestid",
    ] {
        if let Some(value) = headers.get(name).and_then(|v| v.to_str().ok()) {
            return Some(value.to_string());
        }
    }
    None
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
fn upstream_error_message(body: &str) -> Option<String> {
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
        Some(message.to_string())
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

/// Everything needed to close out a delivered request once its stream ends.
///
/// The record is written even when the client disconnects half-way: the
/// upstream has already produced (and billed) whatever was streamed, so
/// dropping the partial usage on the floor would understate the ledger.
struct Finalizer {
    backend: Arc<dyn ProxyBackend>,
    plan: AttemptPlan,
    request_id: String,
    client_model: String,
    provider_request_id: Option<String>,
    currency: String,
    credit_multiplier: Decimal,
    coverage: GateClass,
    attempt_no: u32,
    started_at: OffsetDateTime,
    estimated_prompt_tokens: u64,
    status_code: u16,
}

/// Forwards upstream bytes unchanged while metering them on the side.
struct RelayStream {
    inner: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>,
    meter: StreamMeter,
    started: Instant,
    first_content_at: Option<Instant>,
    bytes_sent: u64,
    finalizer: Option<Finalizer>,
}

impl RelayStream {
    fn complete(&mut self, status: RequestStatus) {
        let Some(f) = self.finalizer.take() else {
            return;
        };
        let mut result = self.meter.finish();

        if !result.usage.measured {
            // The upstream never sent a usage frame. Estimate conservatively
            // from the request and the content we saw, and keep measured=false
            // so the estimate never contaminates real statistics.
            result.usage.prompt_tokens = result.usage.prompt_tokens.max(f.estimated_prompt_tokens);
            let completion_hint = self.meter.content_chars() / 4;
            result.usage.completion_tokens =
                result.usage.completion_tokens.max(completion_hint);
            result.usage.measured = false;
        }

        let cost = f
            .backend
            .estimate_cost(&f.plan.model, &result.usage, f.credit_multiplier)
            .unwrap_or_else(|| {
                crate::meter::conservative_estimate(&result.usage, f.credit_multiplier)
            });

        let total_ms = self
            .started
            .elapsed()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        let ttft_ms = self.first_content_at.map(|t| {
            t.saturating_duration_since(self.started)
                .as_millis()
                .min(u128::from(u64::MAX)) as u64
        });
        let tps = self.first_content_at.and_then(|first| {
            crate::meter::compute_tps(result.usage.completion_tokens, first, Instant::now())
        });
        let billed = self.bytes_sent > 0 || result.usage.measured;

        let attempt = AttemptRecord {
            request_id: f.request_id.clone(),
            attempt_no: f.attempt_no,
            account_id: f.plan.account_id.clone(),
            provider: f.plan.provider.clone(),
            started_at: f.started_at,
            ttft_ms,
            status_code: Some(f.status_code),
            error_class: None,
            error_code: None,
            retry_after_ms: None,
            billed,
            bytes_sent: self.bytes_sent,
        };
        let record = RequestRecord {
            request_id: f.request_id.clone(),
            ts: f.started_at,
            session_key: f.plan.session_key.clone(),
            client_model: f.client_model.clone(),
            provider: f.plan.provider.clone(),
            account_id: f.plan.account_id.clone(),
            model: f.plan.model.clone(),
            usage: result.usage.clone(),
            ttft_ms,
            total_ms,
            tps,
            cost,
            currency: f.currency.clone(),
            credit_multiplier: f.credit_multiplier,
            coverage: f.coverage,
            status,
            retries: f.attempt_no.saturating_sub(1),
            provider_request_id: f.provider_request_id.clone(),
        };
        f.backend
            .record_success(&f.plan, &result.usage, cost, attempt, record);
    }
}

impl Stream for RelayStream {
    type Item = reqwest::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(chunk))) => {
                let first = this.meter.push(&chunk);
                if first && this.first_content_at.is_none() {
                    this.first_content_at = Some(Instant::now());
                }
                this.bytes_sent = this.bytes_sent.saturating_add(chunk.len() as u64);
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(e))) => {
                // The upstream died mid-stream: the response is already partly
                // delivered, so it cannot be retried, but the partial spend
                // still has to be recorded.
                this.complete(RequestStatus::Error);
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                this.complete(RequestStatus::Ok);
                Poll::Ready(None)
            }
        }
    }
}

impl Drop for RelayStream {
    fn drop(&mut self) {
        if self.finalizer.is_some() {
            // The client went away. Dropping the upstream body aborts the
            // upstream request so it stops billing, and we record what was
            // already delivered.
            self.complete(RequestStatus::Aborted);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::{gate, Pool};
    use crate::config::{Config, SecretRef};
    use crate::domain::GateKind;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    // ------------------------------------------------------------ mock upstream

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum MockMode {
        /// First call: HTTP 429 with a quota-shaped body. Later calls: a
        /// normal OpenAI SSE stream including the empty-choices usage frame.
        Rotate429ThenStream,
        /// Always a non-streaming JSON completion.
        JsonOk,
    }

    struct MockUpstream {
        mode: MockMode,
        calls: AtomicUsize,
        bearers: Mutex<Vec<String>>,
        bodies: Mutex<Vec<Value>>,
    }

    async fn mock_models(State(m): State<Arc<MockUpstream>>, headers: HeaderMap) -> Response {
        let bearer = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        m.bearers.lock().unwrap().push(bearer);
        let mut response = Response::new(Body::from(
            r#"{"object":"list","data":[{"id":"mock/model","object":"model"}]}"#,
        ));
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
        response
    }

    async fn mock_chat(
        State(m): State<Arc<MockUpstream>>,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        let bearer = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        m.bearers.lock().unwrap().push(bearer);
        m.bodies
            .lock()
            .unwrap()
            .push(serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null));
        let call = m.calls.fetch_add(1, Ordering::SeqCst);

        if m.mode == MockMode::Rotate429ThenStream && call == 0 {
            let mut response = Response::new(Body::from(
                r#"{"error":{"message":"window exhausted","type":"insufficient_quota","code":"usage_exceeded"}}"#,
            ));
            *response.status_mut() = StatusCode::TOO_MANY_REQUESTS;
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, "3600".parse().unwrap());
            return response;
        }

        if m.mode == MockMode::JsonOk {
            let body = r#"{"id":"c1","object":"chat.completion","model":"mock/model","choices":[{"index":0,"message":{"role":"assistant","content":"Hello"},"finish_reason":"stop"}],"usage":{"prompt_tokens":12,"completion_tokens":3,"total_tokens":15}}"#;
            let mut response = Response::new(Body::from(body));
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
            return response;
        }

        let mut sse = String::new();
        for chunk in [
            json!({"id":"c1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}),
            json!({"id":"c1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}),
            json!({"id":"c1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
            json!({"id":"c1","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":12,"completion_tokens":3,"total_tokens":15}}),
        ] {
            sse.push_str("data: ");
            sse.push_str(&chunk.to_string());
            sse.push_str("\n\n");
        }
        sse.push_str("data: [DONE]\n\n");
        let mut response = Response::new(Body::from(sse));
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, "text/event-stream".parse().unwrap());
        response
    }

    async fn spawn_mock(mode: MockMode) -> (String, Arc<MockUpstream>) {
        let state = Arc::new(MockUpstream {
            mode,
            calls: AtomicUsize::new(0),
            bearers: Mutex::new(Vec::new()),
            bodies: Mutex::new(Vec::new()),
        });
        let app = Router::new()
            .route("/chat/completions", post(mock_chat))
            .route("/models", get(mock_models))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), state)
    }

    // ------------------------------------------------------------ fake backend

    struct RecordedSuccess {
        plan: AttemptPlan,
        usage: TokenUsage,
        cost: Decimal,
        attempt: AttemptRecord,
        record: RequestRecord,
    }

    struct RecordedFailure {
        account_id: String,
        class: UpstreamErrorClass,
        attempt: AttemptRecord,
    }

    struct FakeBackend {
        config: Config,
        pool: Pool,
        http: reqwest::Client,
        successes: Mutex<Vec<RecordedSuccess>>,
        failures: Mutex<Vec<RecordedFailure>>,
    }

    impl FakeBackend {
        fn new(config: Config, pool: Pool) -> Self {
            FakeBackend {
                config,
                pool,
                http: reqwest::Client::new(),
                successes: Mutex::new(Vec::new()),
                failures: Mutex::new(Vec::new()),
            }
        }
    }

    impl ProxyBackend for FakeBackend {
        fn config(&self) -> &Config {
            &self.config
        }

        fn resolve(&self, client_model: &str) -> Result<ResolvedRoute> {
            self.config.resolve(client_model)
        }

        fn plan(
            &self,
            client_model: &str,
            session_key: Option<&str>,
            est_cost: Decimal,
            request_id: &str,
            attempt_no: u32,
        ) -> std::result::Result<(ResolvedRoute, AttemptPlan), PlanError> {
            let route = self
                .config
                .resolve(client_model)
                .map_err(|e| PlanError::UnknownProvider(e.to_string()))?;
            let plan = self.pool.select(
                &route.provider,
                &route.model,
                session_key,
                est_cost,
                request_id,
                attempt_no,
            )?;
            // The real Gateway::plan reserves; mirror that here.
            self.pool.reserve(&plan, est_cost);
            Ok((route, plan))
        }

        fn pick_account(
            &self,
            provider: &str,
            est_cost: Decimal,
            request_id: &str,
            attempt_no: u32,
        ) -> std::result::Result<AttemptPlan, PlanError> {
            self.pool
                .select(provider, "", None, est_cost, request_id, attempt_no)
        }

        fn account(&self, account_id: &str) -> Option<Account> {
            self.pool.account(account_id)
        }

        fn retry_after_for(&self, provider: &str) -> Option<Duration> {
            self.pool.retry_after_for(provider)
        }

        fn http(&self) -> reqwest::Client {
            self.http.clone()
        }

        fn estimate_cost(
            &self,
            _model: &str,
            _usage: &TokenUsage,
            _multiplier: Decimal,
        ) -> Option<Decimal> {
            None
        }

        fn record_success(
            &self,
            plan: &AttemptPlan,
            usage: &TokenUsage,
            cost: Decimal,
            attempt: AttemptRecord,
            record: RequestRecord,
        ) {
            self.pool.settle(plan, cost, true);
            self.successes.lock().unwrap().push(RecordedSuccess {
                plan: plan.clone(),
                usage: usage.clone(),
                cost,
                attempt,
                record,
            });
        }

        fn record_failure(
            &self,
            plan: &AttemptPlan,
            class: UpstreamErrorClass,
            retry_after: Option<Duration>,
            window_reset: Option<OffsetDateTime>,
            attempt: AttemptRecord,
        ) {
            self.pool.settle(plan, Decimal::ZERO, false);
            self.pool
                .note_error(&plan.account_id, class, retry_after, window_reset);
            self.failures.lock().unwrap().push(RecordedFailure {
                account_id: plan.account_id.clone(),
                class,
                attempt,
            });
        }
    }

    // ------------------------------------------------------------ helpers

    fn test_config(base_url: &str) -> Config {
        let toml = format!(
            r#"
[server]
listen = "127.0.0.1:0"

[policy]
max_retries = 3

[providers.mock]
adapter = "none"
base_url = "{base_url}"

[providers.mock.accounts.a1]
key = "sk-a1"

[providers.mock.accounts.a2]
key = "sk-a2"

[routes]
"mock/model" = "mock"
"#
        );
        Config::from_toml_str(&toml).unwrap()
    }

    fn activate(pool: &Pool, id: &str, used: i64) {
        let gates = vec![gate(
            GateKind::FiveHour,
            Decimal::from(100),
            Decimal::from(used),
            None,
        )];
        pool.apply_probe(id, gates, now());
    }

    fn fake_backend(base_url: &str) -> Arc<FakeBackend> {
        let config = test_config(base_url);
        let accounts = config
            .build_accounts(&crate::secrets::CredentialStore::memory())
            .unwrap();
        let pool = Pool::new(accounts, &config.policy);
        activate(&pool, "a1", 1);
        activate(&pool, "a2", 0);
        Arc::new(FakeBackend::new(config, pool))
    }

    async fn spawn_router(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    async fn wait_for_successes(fake: &Arc<FakeBackend>, want: usize) {
        for _ in 0..400 {
            if fake.successes.lock().unwrap().len() >= want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("timed out waiting for {want} recorded successes");
    }

    fn chat_body(stream: bool) -> Value {
        json!({
            "model": "mock/model",
            "stream": stream,
            "messages": [{"role": "user", "content": "hi"}],
        })
    }

    // ------------------------------------------------------------ tests

    #[tokio::test]
    async fn stream_is_relayed_usage_recorded_and_a_429_rotates_accounts() {
        let (upstream_url, mock) = spawn_mock(MockMode::Rotate429ThenStream).await;
        let fake = fake_backend(&upstream_url);
        let addr = spawn_router(build_router_with(fake.clone())).await;

        let response = reqwest::Client::new()
            .post(format!("{addr}/v1/chat/completions"))
            .json(&chat_body(true))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "the retry must be transparent");
        let text = response.text().await.unwrap();
        assert!(text.contains("Hello"), "the relayed body was: {text}");
        assert!(text.contains("[DONE]"), "the relayed body was: {text}");

        wait_for_successes(&fake, 1).await;
        let successes = fake.successes.lock().unwrap();
        assert_eq!(successes.len(), 1);
        let success = &successes[0];
        assert_eq!(
            success.plan.account_id, "a2",
            "the second account must serve after the first is parked"
        );
        assert_eq!(success.usage.prompt_tokens, 12);
        assert_eq!(success.usage.completion_tokens, 3);
        assert!(success.usage.measured, "usage must come from the usage frame");
        assert!(success.cost > Decimal::ZERO);
        assert!(success.attempt.billed);
        assert!(
            success.attempt.ttft_ms.is_some(),
            "a delivered stream must record TTFT"
        );
        assert_eq!(success.record.status, RequestStatus::Ok);
        assert_eq!(success.record.retries, 1);
        assert_eq!(success.record.coverage, GateClass::Covered);
        drop(successes);

        let failures = fake.failures.lock().unwrap();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].account_id, "a1");
        assert_eq!(failures[0].class, UpstreamErrorClass::QuotaExhausted);
        assert!(
            failures[0].attempt.ttft_ms.is_none(),
            "an attempt that failed before content has no TTFT"
        );
        assert!(!failures[0].attempt.billed);
        drop(failures);

        assert_eq!(
            *mock.bearers.lock().unwrap(),
            vec!["Bearer sk-a1".to_string(), "Bearer sk-a2".to_string()],
            "each attempt must carry its own account key"
        );
        let bodies = mock.bodies.lock().unwrap();
        assert_eq!(bodies.len(), 2, "both attempts reached the upstream");
        for body in bodies.iter() {
            assert_eq!(
                body["stream_options"]["include_usage"],
                json!(true),
                "include_usage must be injected or the ledger records zero forever"
            );
        }
    }

    #[tokio::test]
    async fn models_passthrough_uses_an_eligible_account() {
        let (upstream_url, mock) = spawn_mock(MockMode::JsonOk).await;
        let fake = fake_backend(&upstream_url);
        let addr = spawn_router(build_router_with(fake.clone())).await;

        let response = reqwest::Client::new()
            .get(format!("{addr}/v1/models"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let text = response.text().await.unwrap();
        assert!(text.contains("mock/model"), "the model list was: {text}");
        assert_eq!(
            *mock.bearers.lock().unwrap(),
            vec!["Bearer sk-a1".to_string()],
            "the passthrough must authenticate as an eligible account"
        );
    }

    #[tokio::test]
    async fn non_streaming_response_usage_is_recorded() {
        let (upstream_url, _mock) = spawn_mock(MockMode::JsonOk).await;
        let fake = fake_backend(&upstream_url);
        let addr = spawn_router(build_router_with(fake.clone())).await;

        let response = reqwest::Client::new()
            .post(format!("{addr}/v1/chat/completions"))
            .json(&chat_body(false))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let text = response.text().await.unwrap();
        assert!(text.contains("Hello"));

        wait_for_successes(&fake, 1).await;
        let successes = fake.successes.lock().unwrap();
        assert_eq!(successes[0].usage.prompt_tokens, 12);
        assert_eq!(successes[0].usage.completion_tokens, 3);
        assert!(successes[0].usage.measured);
        assert_eq!(successes[0].plan.account_id, "a1");
    }

    #[tokio::test]
    async fn no_eligible_account_is_429_with_retry_after_and_no_upstream_call() {
        let fake = fake_backend("http://127.0.0.1:1");
        let longest = now() + time::Duration::seconds(300);
        let earliest = now() + time::Duration::seconds(120);
        fake.pool.note_error(
            "a1",
            UpstreamErrorClass::QuotaExhausted,
            None,
            Some(earliest),
        );
        fake.pool.note_error(
            "a2",
            UpstreamErrorClass::QuotaExhausted,
            None,
            Some(longest),
        );
        let addr = spawn_router(build_router_with(fake.clone())).await;

        let response = reqwest::Client::new()
            .post(format!("{addr}/v1/chat/completions"))
            .json(&chat_body(true))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 429);
        let raw = response
            .headers()
            .get(header::RETRY_AFTER)
            .expect("a 429 must carry Retry-After")
            .to_str()
            .unwrap()
            .to_string();
        let secs: u64 = raw.parse().expect("Retry-After must be delta-seconds");
        assert!(
            (1..=120).contains(&secs),
            "the earliest reset wins, got {secs}"
        );
        assert!(fake.successes.lock().unwrap().is_empty());
        assert!(fake.failures.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn unknown_model_is_rejected_before_any_upstream_call() {
        let (upstream_url, mock) = spawn_mock(MockMode::JsonOk).await;
        let fake = fake_backend(&upstream_url);
        let addr = spawn_router(build_router_with(fake.clone())).await;

        let response = reqwest::Client::new()
            .post(format!("{addr}/v1/chat/completions"))
            .json(&json!({"model": "nope/model", "messages": []}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 404);
        assert_eq!(
            mock.calls.load(Ordering::SeqCst),
            0,
            "an unroutable model must never reach a provider"
        );
    }

    #[tokio::test]
    async fn local_bearer_auth_is_enforced_when_configured() {
        let (upstream_url, _mock) = spawn_mock(MockMode::JsonOk).await;
        let mut config = test_config(&upstream_url);
        config.server.local_key = Some(SecretRef::Literal("sk-local".into()));
        let accounts = config
            .build_accounts(&crate::secrets::CredentialStore::memory())
            .unwrap();
        let pool = Pool::new(accounts, &config.policy);
        activate(&pool, "a1", 1);
        activate(&pool, "a2", 0);
        let fake = Arc::new(FakeBackend::new(config, pool));
        let addr = spawn_router(build_router_with(fake)).await;
        let client = reqwest::Client::new();
        let url = format!("{addr}/v1/chat/completions");

        let missing = client
            .post(&url)
            .json(&chat_body(false))
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), 401);

        let wrong = client
            .post(&url)
            .bearer_auth("sk-nope")
            .json(&chat_body(false))
            .send()
            .await
            .unwrap();
        assert_eq!(wrong.status(), 401);

        let accepted = client
            .post(&url)
            .bearer_auth("sk-local")
            .json(&chat_body(false))
            .send()
            .await
            .unwrap();
        assert_eq!(accepted.status(), 200);
    }

    #[tokio::test]
    async fn request_body_over_the_configured_limit_is_rejected() {
        let (upstream_url, _mock) = spawn_mock(MockMode::JsonOk).await;
        let mut config = test_config(&upstream_url);
        config.policy.max_body_bytes = 64;
        let accounts = config
            .build_accounts(&crate::secrets::CredentialStore::memory())
            .unwrap();
        let pool = Pool::new(accounts, &config.policy);
        activate(&pool, "a1", 1);
        let fake = Arc::new(FakeBackend::new(config, pool));
        let addr = spawn_router(build_router_with(fake)).await;

        let big = json!({
            "model": "mock/model",
            "messages": [{"role": "user", "content": "x".repeat(4096)}],
        });
        let response = reqwest::Client::new()
            .post(format!("{addr}/v1/chat/completions"))
            .json(&big)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 413);
    }

    #[tokio::test]
    async fn dropping_the_relay_records_a_partial_aborted_request() {
        let fake = fake_backend("http://127.0.0.1:1");
        let plan = fake
            .pool
            .select("mock", "mock/model", None, Decimal::ZERO, "req-1", 1)
            .unwrap();
        let payload: &'static [u8] =
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n";
        let finalizer = Finalizer {
            backend: fake.clone(),
            plan,
            request_id: "req-1".to_string(),
            client_model: "mock/model".to_string(),
            provider_request_id: None,
            currency: "USD".to_string(),
            credit_multiplier: Decimal::ONE,
            coverage: GateClass::Covered,
            attempt_no: 1,
            started_at: now(),
            estimated_prompt_tokens: 10,
            status_code: 200,
        };
        let chunks: Vec<reqwest::Result<Bytes>> = vec![Ok(Bytes::from_static(payload))];
        let mut relay = RelayStream {
            inner: Box::pin(futures_util::stream::iter(chunks)),
            meter: StreamMeter::sse(),
            started: Instant::now(),
            first_content_at: None,
            bytes_sent: 0,
            finalizer: Some(finalizer),
        };

        use futures_util::StreamExt;
        assert!(relay.next().await.is_some());
        // The client disconnects here: dropping the stream aborts the upstream
        // response and records the partial spend.
        drop(relay);

        let successes = fake.successes.lock().unwrap();
        assert_eq!(successes.len(), 1);
        assert_eq!(successes[0].record.status, RequestStatus::Aborted);
        assert_eq!(successes[0].attempt.bytes_sent, payload.len() as u64);
        assert!(successes[0].attempt.billed);
        assert!(successes[0].attempt.ttft_ms.is_some());
    }
}
