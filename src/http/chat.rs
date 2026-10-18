//! The OpenAI-compatible chat-completions endpoint and the retry loop.
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

use super::{
    AppState, ProxyBackend, error_response, error_response_retry, read_bounded,
    upstream_error_message,
};
use crate::accounts::{AttemptPlan, PlanError, Reservation};
use crate::domain::{AttemptRecord, GateClass, RequestRecord, RequestStatus, TokenUsage, now};
use crate::error::{ErrorSignals, UpstreamErrorClass, parse_error_body, parse_retry_after};
use crate::sse::{MeterMode, StreamMeter, inject_include_usage};
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use futures_util::Stream;
use rust_decimal::Decimal;
use serde_json::Value;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant};
use time::OffsetDateTime;

/// Longest backoff the retry loop will sleep. A quota window is handled by
/// rotating accounts, not by stalling the client for an hour.
const MAX_BACKOFF: Duration = Duration::from_secs(2);

/// How long to wait before the next attempt.
///
/// A Retry-After the upstream gave us wins. Otherwise a retryable failure still
/// waits: retrying instantly turns one transient blip into a thundering herd
/// against the next account, and for a transport failure there is nothing to
/// gain by being the fastest to ask again.
fn backoff_for(
    class: UpstreamErrorClass,
    attempt_no: u32,
    retry_after: Option<Duration>,
) -> Duration {
    if let Some(delay) = retry_after {
        return delay.min(MAX_BACKOFF);
    }
    if !(class.retry_other_account() || class.retry_same_account()) {
        return Duration::ZERO;
    }
    let shift = attempt_no.saturating_sub(1).min(4);
    Duration::from_millis(250u64 << shift).min(MAX_BACKOFF)
}

pub(super) async fn chat_completions(
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
    let stream = value
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
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
    let est_cost = estimate_request_cost(
        state.backend.as_ref(),
        &route.provider,
        &route.model,
        &value,
    );

    // Without this flag the upstream never emits a usage frame and the local
    // ledger records zero forever, which silently disables cost control.
    let mut upstream_body = body.clone();
    if stream
        && inject_include_usage(&mut value)
        && let Ok(bytes) = serde_json::to_vec(&value)
    {
        upstream_body = Bytes::from(bytes);
    }

    let max_attempts = state.backend.config().policy.max_retries.max(1);
    // How long we wait for the upstream to START answering.
    //
    // This used to be connect_timeout_secs, which is a TCP-connect budget. A
    // non-streaming request only receives its headers after the upstream has
    // generated the entire completion, so any legitimate non-streaming request
    // slower than ten seconds was cancelled, marked unbilled, and retried --
    // possibly paying for the same completion twice.
    let header_timeout =
        Duration::from_secs(state.backend.config().policy.ttft_timeout_secs.max(1));

    let mut attempt_no: u32 = 1;
    let mut last_failure: Option<Failure> = None;
    let mut last_plan: Option<AttemptPlan> = None;
    // Accounts this request has already attempted. Passed to the pool so the
    // next attempt reaches a DIFFERENT account instead of spending the whole
    // retry budget on the one that just failed.
    let mut tried: Vec<String> = Vec::new();
    let mut previous_account: Option<String> = None;

    while attempt_no <= max_attempts {
        let reservation = match state.backend.plan(
            &client_model,
            session_key.as_deref(),
            est_cost,
            &request_id,
            attempt_no,
            &tried,
        ) {
            Ok((_route, reservation)) => reservation,
            Err(err) => return plan_error_response(&err, state.backend.as_ref()),
        };
        // Switching accounts is worth one extra round trip: it happens only
        // after the previous account has already failed, so the client is
        // waiting anyway, and it stops a switch from landing on an account whose
        // window filled up since the last poll.
        let qualified = reservation.qualified_account();
        if let Some(previous) = previous_account.as_deref()
            && previous != qualified
        {
            state
                .backend
                .ensure_fresh(&reservation.provider, &reservation.account_id)
                .await;
        }
        previous_account = Some(qualified.clone());
        // Recorded before the attempt, not after: whatever the outcome, this
        // account has now been used by this request.
        tried.push(qualified);
        // A description of the last attempt, for the failure record at the end
        // of the loop. It carries no claim: only the reservation can settle.
        last_plan = Some(reservation.plan().clone());
        // Any exit from this iteration that does not settle -- an early return,
        // or the client hanging up while we wait -- gives the commitment back
        // through the reservation's Drop.

        let Some(account) = state.backend.account(&reservation.qualified_account()) else {
            tracing::error!(account = %reservation.account_id, "the planned account vanished from the pool");
            // Release the reservation that select() just committed. Returning
            // without recording the attempt left an estimate committed against
            // an account that nothing would ever settle, and the in-flight term
            // in the hard gate would keep counting it for the life of the
            // process -- shrinking the headroom every later request sees.
            record_attempt_failure(
                state.backend.as_ref(),
                reservation,
                UpstreamErrorClass::Other,
                None,
                Some(StatusCode::INTERNAL_SERVER_ERROR.as_u16()),
                None,
                now(),
                attempt_no,
                &request_id,
            );
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "the planned account is no longer in the pool",
            );
        };

        let url = format!(
            "{}/chat/completions",
            account.base_url.trim_end_matches('/')
        );
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
                if stream {
                    "text/event-stream"
                } else {
                    "application/json"
                },
            )
            .body(upstream_body.clone());

        // Phase 1: nothing has reached the client yet, so every failure below
        // is still retryable.
        let response = match tokio::time::timeout(header_timeout, request.send()).await {
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
                    reservation,
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
                let delay = backoff_for(class, attempt_no, None);
                attempt_no += 1;
                if !retryable {
                    break;
                }
                // A transport failure or a timeout backs off as well: retrying
                // instantly turns one blip into a thundering herd, and there is
                // nothing to gain by being the fastest to ask a dead host again.
                tokio::time::sleep(delay).await;
                continue;
            }
            Err(_) => {
                let class = UpstreamErrorClass::Timeout;
                record_attempt_failure(
                    state.backend.as_ref(),
                    reservation,
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
                        header_timeout.as_secs()
                    ),
                    retry_after: None,
                });
                let retryable = should_retry(class, attempt_no, max_attempts);
                let delay = backoff_for(class, attempt_no, None);
                attempt_no += 1;
                if !retryable {
                    break;
                }
                // A transport failure or a timeout backs off as well: retrying
                // instantly turns one blip into a thundering herd, and there is
                // nothing to gain by being the fastest to ask a dead host again.
                tokio::time::sleep(delay).await;
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
            // Canonical provider/id: account() looks up a qualified identity,
            // and passing the bare id meant this never matched anything, so the
            // probe-confirmed "the window is spent" branch was unreachable.
            let window_exhausted_hint = state
                .backend
                .account(&reservation.qualified_account())
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

            // Captured before the handover: recording the failure parks the
            // account, and the Retry-After below is read from the pool it just
            // updated.
            let provider = reservation.provider.clone();
            record_attempt_failure(
                state.backend.as_ref(),
                reservation,
                class,
                retry_after,
                Some(status.as_u16()),
                error_code,
                started_at,
                attempt_no,
                &request_id,
            );

            let detail = upstream_error_message(&text, &account.api_key)
                .unwrap_or_else(|| format!("upstream returned HTTP {}", status.as_u16()));
            // Other maps back to whatever the upstream said; the rest are
            // remapped so the client sees a class, not a provider quirk.
            let client_status = if class == UpstreamErrorClass::Other {
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY)
            } else {
                status_for_class(class)
            };
            let client_retry = retry_after.or_else(|| state.backend.retry_after_for(&provider));
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
            let delay = backoff_for(class, attempt_no, retry_after);
            attempt_no += 1;
            if !retryable {
                break;
            }
            tokio::time::sleep(delay).await;
            continue;
        }

        // Phase 2: 2xx means bytes are about to be delivered. From here the
        // request is not retryable, so the relay owns the record.
        let coverage = reservation.coverage;
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
        // Judged from the response, not from the client's "stream": true. A
        // client that asks for SSE and gets a plain JSON 200 back was being
        // marked as a truncated stream for missing a terminator that a JSON
        // body never had.
        let is_sse = content_type.contains("text/event-stream");
        let finalizer = Finalizer {
            backend: state.backend.clone(),
            reservation,
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
        let idle_timeout =
            Duration::from_secs(state.backend.config().policy.idle_timeout_secs.max(1));
        let relay = RelayStream {
            inner: Box::pin(response.bytes_stream()),
            meter,
            started,
            first_content_at: None,
            bytes_sent: 0,
            finalizer: Some(finalizer),
            is_sse,
            idle_timeout,
            idle: Box::pin(tokio::time::sleep(idle_timeout)),
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
                && let Some(account) = state.backend.account(&plan.qualified_account())
            {
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
                    // attempt_no is already one past the last attempt: every
                    // failure path increments it before deciding whether to
                    // retry. Reporting it directly counted the first attempt
                    // as a retry.
                    retries: attempt_no.saturating_sub(2),
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
            ..
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

// Seven arguments, and each one is a different fact about the attempt: which
// plan, how it failed, what the upstream said, when it started, and which
// attempt and request it belongs to. Bundling them into a struct would move the
// same list one line up without making anything clearer.
#[allow(clippy::too_many_arguments)]
fn record_attempt_failure(
    backend: &dyn ProxyBackend,
    reservation: Reservation,
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
        account_id: reservation.account_id.clone(),
        provider: reservation.provider.clone(),
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
    backend.record_failure(reservation, class, retry_after, None, attempt);
}

fn estimate_request_cost(
    backend: &dyn ProxyBackend,
    provider: &str,
    model: &str,
    body: &Value,
) -> Decimal {
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
    let tier = backend.tier_at(provider, model);
    backend
        .estimate_cost(provider, model, &tier, &probe, Decimal::ONE)
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

/// Everything needed to close out a delivered request once its stream ends.
///
/// The record is written even when the client disconnects half-way: the
/// upstream has already produced (and billed) whatever was streamed, so
/// dropping the partial usage on the floor would understate the ledger.
pub(super) struct Finalizer {
    pub(super) backend: Arc<dyn ProxyBackend>,
    /// The attempt's commitment. The relay settles it when the body ends, and
    /// dropping it unsettled returns it.
    pub(super) reservation: Reservation,
    pub(super) request_id: String,
    pub(super) client_model: String,
    pub(super) provider_request_id: Option<String>,
    pub(super) currency: String,
    pub(super) credit_multiplier: Decimal,
    pub(super) coverage: GateClass,
    pub(super) attempt_no: u32,
    pub(super) started_at: OffsetDateTime,
    pub(super) estimated_prompt_tokens: u64,
    pub(super) status_code: u16,
}

/// Forwards upstream bytes unchanged while metering them on the side.
pub(super) struct RelayStream {
    pub(super) inner: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>,
    pub(super) meter: StreamMeter,
    pub(super) started: Instant,
    pub(super) first_content_at: Option<Instant>,
    pub(super) bytes_sent: u64,
    pub(super) finalizer: Option<Finalizer>,
    /// Whether this body is an SSE stream, whose terminator is meaningful.
    pub(super) is_sse: bool,
    /// A silence longer than this ends the stream: policy.idle_timeout_secs.
    pub(super) idle_timeout: Duration,
    /// Fires once the upstream has been silent for idle_timeout.
    pub(super) idle: Pin<Box<tokio::time::Sleep>>,
}

impl RelayStream {
    fn complete(&mut self, status: RequestStatus) {
        let Some(f) = self.finalizer.take() else {
            return;
        };
        let mut result = self.meter.finish();

        // A stream that carried an error frame is a failure even though the
        // HTTP status was 200 and the bytes were relayed intact. Recording it
        // as a success inflates request counts and hides a failing provider.
        let status = if result.error.is_some() {
            RequestStatus::Error
        } else if matches!(status, RequestStatus::Ok) && self.is_sse && !result.saw_done() {
            // An SSE body that ended without its terminator was cut short --
            // by the idle timeout below, or by an upstream that closed early.
            // Calling that a success hides a broken response and inflates the
            // request count.
            tracing::warn!(
                request_id = %f.request_id,
                bytes = self.bytes_sent,
                "the SSE stream ended without its [DONE] terminator"
            );
            RequestStatus::Error
        } else {
            status
        };

        if !result.usage.measured {
            // The upstream never sent a usage frame. Estimate conservatively
            // from the request and the content we saw, and keep measured=false
            // so the estimate never contaminates real statistics.
            result.usage.prompt_tokens = result.usage.prompt_tokens.max(f.estimated_prompt_tokens);
            let completion_hint = self.meter.content_chars() / 4;
            result.usage.completion_tokens = result.usage.completion_tokens.max(completion_hint);
            result.usage.measured = false;
        }

        let cost = f
            .backend
            .estimate_cost(
                &f.reservation.provider,
                &f.reservation.model,
                &f.reservation.tier,
                &result.usage,
                f.credit_multiplier,
            )
            .unwrap_or_else(|| {
                crate::meter::conservative_estimate(&result.usage, f.credit_multiplier)
            });

        let total_ms = self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
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
            account_id: f.reservation.account_id.clone(),
            provider: f.reservation.provider.clone(),
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
            session_key: f.reservation.session_key.clone(),
            client_model: f.client_model.clone(),
            provider: f.reservation.provider.clone(),
            account_id: f.reservation.account_id.clone(),
            model: f.reservation.model.clone(),
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
            .record_success(f.reservation, &result.usage, cost, attempt, record);
    }
}

impl Stream for RelayStream {
    type Item = reqwest::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Pending => {
                // A stalled upstream ends the stream. policy.idle_timeout_secs
                // used to be parsed and never read, so a provider that simply
                // stopped sending held the client, the connection and the
                // account's concurrency slot for as long as it liked. Ending the
                // body is the honest signal for SSE: the client sees no
                // terminator and the ledger records an error.
                if std::future::Future::poll(this.idle.as_mut(), cx).is_ready() {
                    tracing::warn!(
                        bytes = this.bytes_sent,
                        idle_secs = this.idle_timeout.as_secs(),
                        "upstream went silent; ending the relayed stream"
                    );
                    this.complete(RequestStatus::Error);
                    return Poll::Ready(None);
                }
                Poll::Pending
            }
            Poll::Ready(Some(Ok(chunk))) => {
                let first = this.meter.push(&chunk);
                if first && this.first_content_at.is_none() {
                    this.first_content_at = Some(Instant::now());
                }
                this.bytes_sent = this.bytes_sent.saturating_add(chunk.len() as u64);
                this.idle
                    .as_mut()
                    .reset(tokio::time::Instant::now() + this.idle_timeout);
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
