//! The console's HTTP surface: pages, the board partial, embedded assets and
//! the guard that keeps all of it to the loopback operator.
//!
//! The guard runs before every console route and answers 404 to a peer or Host
//! the console does not serve, so a network-exposed proxy listener does not
//! advertise an admin surface. Writes must be same-origin JSON with the
//! console's own header; a cross-site form can send none of the three.

use super::AppState;
use crate::config::Config;
use crate::config_doc::{DraftAction, DraftChange, KeySpec};
use crate::console::guard::{self, Verdict};
use crate::console::layout::esc;
use crate::console::{
    assets, board, config_page, copy::Lang, doctor as doctor_view, layout, layout::Nav, prices,
    requests, usage,
};
use crate::http::apply;
use axum::Json;
use axum::body::Body;
use axum::extract::{ConnectInfo, Path, Query, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use std::net::SocketAddr;
use std::sync::Arc;

/// Where the console is reached from and which file its pages describe.
#[derive(Debug, Clone)]
pub struct ConsoleContext {
    /// The address the listener actually bound, for display and for the Host
    /// allowlist. Port 0 is resolved before the router is built.
    pub listen: String,
    pub port: u16,
    pub config_path: String,
    /// When this process loaded the file its pages describe. The read-only
    /// configuration page reports it as the last applied time until a real
    /// apply engine exists.
    pub started_at: time::OffsetDateTime,
    /// One apply at a time: read, fingerprint check, write and publish are one
    /// critical section, so two concurrent applies cannot interleave.
    pub apply_lock: Arc<tokio::sync::Mutex<()>>,
    /// What the file watcher last saw and why an external edit was refused.
    pub reload: crate::reload::SharedReload,
}

impl ConsoleContext {
    pub fn new(listen: &str, port: u16, config_path: &str) -> ConsoleContext {
        ConsoleContext {
            listen: listen.to_string(),
            port,
            config_path: config_path.to_string(),
            started_at: crate::domain::now(),
            apply_lock: Arc::new(tokio::sync::Mutex::new(())),
            reload: crate::reload::new_shared(),
        }
    }

    /// A context for tests that exercise routes without a real listener.
    pub fn for_tests() -> ConsoleContext {
        ConsoleContext::new("127.0.0.1:8787", 8787, "/tmp/lgw-test-config.toml")
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct LangQuery {
    lang: Option<String>,
}

impl LangQuery {
    fn lang(&self) -> Lang {
        Lang::from_param(self.lang.as_deref())
    }
}

/// The root page: the Board, wrapped in the auto-refresh partial.
pub(super) async fn board_page(
    State(state): State<Arc<AppState>>,
    Query(query): Query<LangQuery>,
) -> Response {
    let lang = query.lang();
    let inner = board_html(&state, lang);
    let body = format!(
        "<div data-poll data-url=\"/console/partials/board\" data-interval=\"10000\">{inner}</div>"
    );
    page(&state, lang, Nav::Board, body)
}

/// The board fragment the page polls.
pub(super) async fn board_partial(
    State(state): State<Arc<AppState>>,
    Query(query): Query<LangQuery>,
) -> Response {
    let lang = query.lang();
    let html = board_html(&state, lang);
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        Body::from(html),
    )
        .into_response()
}

/// One renderer for the page and the polled fragment. The vitals and drift come
/// from the same seams the pool exposes, so a refresh cannot show numbers from
/// two different moments in different places.
fn board_html(state: &AppState, lang: Lang) -> String {
    let snapshot = state.backend.snapshot();
    let vitals = state.backend.board_vitals();
    let drift = state.backend.price_drift();
    let input = board::BoardInput {
        accounts: &snapshot,
        vitals: vitals.as_ref(),
        in_flight: state.backend.in_flight(),
        drift: &drift,
    };
    board::render(&input, crate::domain::now(), lang)
}

/// The read-only configuration page: the file the gateway started from, its
/// fingerprint, every known value with its live/restart marker, and a raw view
/// with literal credentials replaced.
pub(super) async fn config_page_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<LangQuery>,
) -> Response {
    let lang = query.lang();
    let strings = lang.strings();
    let runtime = state.backend.runtime();
    let path = state.console.config_path.clone();

    let (fingerprint, raw, mut body) =
        match crate::config_doc::ConfigDocument::read(std::path::Path::new(&path)) {
            Ok(document) => (
                document.fingerprint(),
                Some(document.redacted()),
                String::new(),
            ),
            Err(error) => {
                let message = format!("{} {error}", strings.config_unreadable);
                let banner = format!("<div class=\"banner error\"><b>{}</b></div>", esc(&message));
                (String::from("—"), None, banner)
            }
        };

    let (reload_error, last_success) = match state.console.reload.lock() {
        Ok(state) => (state.last_error.clone(), state.last_success),
        Err(poisoned) => {
            let state = poisoned.into_inner();
            (state.last_error.clone(), state.last_success)
        }
    };
    // The console's own apply and an external reload both count as "applied".
    let applied_at = last_success.unwrap_or(state.console.started_at);
    let applied_at = applied_at
        .replace_nanosecond(0)
        .unwrap_or(state.console.started_at);
    let last_applied = applied_at
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string());
    let view = config_page::ConfigView {
        reload_error: reload_error.as_ref(),
        config: runtime.config(),
        path: &path,
        fingerprint: &fingerprint,
        last_applied: &last_applied,
        raw: raw.as_deref(),
        lang,
    };
    body.push_str(&config_page::render(&view));
    page(&state, lang, Nav::Config, body)
}

#[derive(Debug, Deserialize)]
pub(super) struct ValidateRequest {
    fingerprint: String,
    draft: Vec<DraftEntry>,
}

/// One edit from the form. A plain value edit omits `action`; the same body
/// also carries whole-account additions and removals so one Apply can express
/// everything the page shows.
#[derive(Debug, Deserialize)]
struct DraftEntry {
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    path: Vec<String>,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    label: Option<String>,
    /// "keychain", "env" or "file".
    #[serde(default)]
    key_source: Option<String>,
    /// The environment variable name or the key file path.
    #[serde(default)]
    key_value: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    alias: Option<String>,
    #[serde(default)]
    input: Option<f64>,
    #[serde(default)]
    output: Option<f64>,
    #[serde(default)]
    cache_read: Option<f64>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    days: Option<String>,
    #[serde(default)]
    hours: Option<String>,
    #[serde(default)]
    tz: Option<String>,
    #[serde(default)]
    index: Option<usize>,
}

fn required(value: Option<String>, message: &str) -> std::result::Result<String, String> {
    match value {
        Some(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(message.to_string()),
    }
}

/// Turn the wire form into document actions, refusing a half-specified one
/// before it can touch the document.
fn draft_actions(entries: Vec<DraftEntry>) -> std::result::Result<Vec<DraftAction>, String> {
    let mut actions = Vec::with_capacity(entries.len());
    for entry in entries {
        match entry.action.as_deref().unwrap_or("set") {
            "set" => actions.push(DraftAction::Set(DraftChange {
                path: entry.path,
                value: entry.value.unwrap_or_default(),
            })),
            "remove" => actions.push(DraftAction::Remove { path: entry.path }),
            "add_account" => {
                let provider = entry
                    .provider
                    .filter(|provider| !provider.trim().is_empty())
                    .ok_or_else(|| "add_account needs a provider".to_string())?;
                let id = entry
                    .id
                    .filter(|id| !id.trim().is_empty())
                    .ok_or_else(|| "add_account needs an account id".to_string())?;
                let key = match entry.key_source.as_deref().unwrap_or("keychain") {
                    "keychain" => KeySpec::Keychain,
                    "env" => KeySpec::Env(
                        entry
                            .key_value
                            .filter(|name| !name.trim().is_empty())
                            .ok_or_else(|| "an env key source needs a variable name".to_string())?,
                    ),
                    "file" => KeySpec::File(
                        entry
                            .key_value
                            .filter(|path| !path.trim().is_empty())
                            .ok_or_else(|| "a file key source needs a path".to_string())?,
                    ),
                    other => return Err(format!("unknown key source {other}")),
                };
                actions.push(DraftAction::AddAccount {
                    provider,
                    id,
                    label: entry.label,
                    key,
                });
            }
            "add_route" => actions.push(DraftAction::AddRoute {
                model: required(entry.model, "a route needs a model")?,
                provider: required(entry.provider, "a route needs a provider")?,
            }),
            "remove_route" => actions.push(DraftAction::RemoveRoute {
                model: required(entry.model, "a route needs a model")?,
            }),
            "add_alias" => actions.push(DraftAction::AddAlias {
                alias: required(entry.alias, "an alias needs a name")?,
                provider: required(entry.provider, "an alias needs a provider")?,
            }),
            "remove_alias" => actions.push(DraftAction::RemoveAlias {
                alias: required(entry.alias, "an alias needs a name")?,
            }),
            "add_price" => actions.push(DraftAction::AddPrice {
                provider: required(entry.provider, "a price needs a provider")?,
                model: required(entry.model, "a price needs a model")?,
                input: entry
                    .input
                    .ok_or_else(|| "a price needs an input rate".to_string())?,
                output: entry
                    .output
                    .ok_or_else(|| "a price needs an output rate".to_string())?,
                cache_read: entry.cache_read,
            }),
            "remove_price" => actions.push(DraftAction::RemovePrice {
                provider: required(entry.provider, "a price needs a provider")?,
                model: required(entry.model, "a price needs a model")?,
            }),
            "add_schedule" => actions.push(DraftAction::AddSchedule {
                provider: required(entry.provider, "a schedule needs a provider")?,
                model: required(entry.model, "a schedule needs a model")?,
                entry: crate::config_doc::ScheduleEntry {
                    name: entry.name,
                    days: required(entry.days, "a schedule needs days")?,
                    hours: required(entry.hours, "a schedule needs hours")?,
                    tz: entry.tz,
                    input: entry
                        .input
                        .ok_or_else(|| "a schedule needs an input rate".to_string())?,
                    output: entry
                        .output
                        .ok_or_else(|| "a schedule needs an output rate".to_string())?,
                    cache_read: entry.cache_read,
                },
            }),
            "remove_schedule" => actions.push(DraftAction::RemoveSchedule {
                provider: required(entry.provider, "a schedule needs a provider")?,
                model: required(entry.model, "a schedule needs a model")?,
                index: entry
                    .index
                    .ok_or_else(|| "a schedule needs its index".to_string())?,
            }),
            other => return Err(format!("unknown draft action {other}")),
        }
    }
    Ok(actions)
}

/// Check a draft without writing it: apply the changes to a copy of the file,
/// run the same validator every command uses, and return the lines that would
/// move. A form built against an older file is refused instead of validated
/// against the wrong document.
pub(super) async fn config_validate(
    State(state): State<Arc<AppState>>,
    Query(query): Query<LangQuery>,
    Json(request): Json<ValidateRequest>,
) -> Response {
    let lang = query.lang();
    let path = state.console.config_path.clone();
    let actions = match draft_actions(request.draft) {
        Ok(actions) => actions,
        Err(error) => {
            return fragment(
                StatusCode::UNPROCESSABLE_ENTITY,
                config_page::failed("draft", &error, lang),
            );
        }
    };
    match apply::prepare_draft(
        std::path::Path::new(&path),
        &request.fingerprint,
        actions,
        state.backend.runtime().config(),
    ) {
        Ok(prepared) => fragment(StatusCode::OK, config_page::validated(&prepared.diff, lang)),
        Err(error) => apply_failure(error, &path, lang),
    }
}

/// Persist a validated draft and publish it to the running gateway.
///
/// Everything that can fail happens before anything changes: the file is read,
/// the draft is applied to a copy, the result is validated, the restart-only
/// fields are compared and the next snapshot is built. Only then does the
/// atomic write happen, and only after the write succeeds does the snapshot go
/// live. The apply lock makes the read-write-publish sequence one unit.
pub(super) async fn config_apply(
    State(state): State<Arc<AppState>>,
    Query(query): Query<LangQuery>,
    Json(request): Json<ValidateRequest>,
) -> Response {
    let lang = query.lang();
    let path = state.console.config_path.clone();
    let actions = match draft_actions(request.draft) {
        Ok(actions) => actions,
        Err(error) => {
            return fragment(
                StatusCode::UNPROCESSABLE_ENTITY,
                config_page::failed("draft", &error, lang),
            );
        }
    };
    let prepared = match apply::prepare_draft(
        std::path::Path::new(&path),
        &request.fingerprint,
        actions,
        state.backend.runtime().config(),
    ) {
        Ok(prepared) => prepared,
        Err(error) => return apply_failure(error, &path, lang),
    };
    if !prepared.restart.is_empty() {
        return fragment(
            StatusCode::UNPROCESSABLE_ENTITY,
            config_page::restart_only(&prepared.restart, lang),
        );
    }
    match apply::commit(
        state.backend.as_ref(),
        &state.console,
        std::path::Path::new(&path),
        &prepared,
        apply::CommitMode::ReplaceFile,
    )
    .await
    {
        Ok(()) => fragment(
            StatusCode::OK,
            config_page::applied(
                &prepared.diff,
                &prepared.fingerprint,
                prepared.structural,
                lang,
            ),
        ),
        Err(apply::CommitError::Stale) => fragment(StatusCode::CONFLICT, config_page::stale(lang)),
        Err(apply::CommitError::Write(message)) | Err(apply::CommitError::Publish(message)) => {
            fragment(
                StatusCode::INTERNAL_SERVER_ERROR,
                config_page::failed(&path, &message, lang),
            )
        }
    }
}

fn apply_failure(error: apply::Failure, path: &str, lang: Lang) -> Response {
    match error {
        apply::Failure::Stale => fragment(StatusCode::CONFLICT, config_page::stale(lang)),
        apply::Failure::Read { message } => fragment(
            StatusCode::UNPROCESSABLE_ENTITY,
            config_page::failed(path, &message, lang),
        ),
        apply::Failure::Scope { fields } => fragment(
            StatusCode::UNPROCESSABLE_ENTITY,
            config_page::not_live(&fields, lang),
        ),
        apply::Failure::Document(error) => fragment(
            StatusCode::UNPROCESSABLE_ENTITY,
            config_page::rejected(&error.field, None, &error.message, lang),
        ),
        apply::Failure::Invalid(violation) => fragment(
            StatusCode::UNPROCESSABLE_ENTITY,
            config_page::rejected(
                &violation.field,
                Some(violation.code),
                &violation.message,
                lang,
            ),
        ),
        apply::Failure::Toml { message } => fragment(
            StatusCode::UNPROCESSABLE_ENTITY,
            config_page::rejected("config", None, &message, lang),
        ),
    }
}

fn fragment(status: StatusCode, html: String) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        Body::from(html),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
pub(super) struct UsageQuery {
    range: Option<String>,
    group: Option<String>,
    lang: Option<String>,
}

/// The Usage page. Read-only: the backend's aggregation opens the ledger
/// read-only and never contacts a provider.
pub(super) async fn usage_page(
    State(state): State<Arc<AppState>>,
    Query(query): Query<UsageQuery>,
) -> Response {
    let lang = Lang::from_param(query.lang.as_deref());
    let range = usage::Range::parse(query.range.as_deref());
    let group = usage::Group::parse(query.group.as_deref());
    let now = crate::domain::now();
    let report = state
        .backend
        .usage_report(now, range.since(now), &[group.group_by()]);
    let body = usage::render(report.as_ref(), range, group, lang);
    page(&state, lang, Nav::Usage, body)
}

#[derive(Debug, Deserialize)]
pub(super) struct RequestsQuery {
    limit: Option<u32>,
    errors: Option<String>,
    coverage: Option<String>,
    lang: Option<String>,
}

/// The Requests page. Read-only: both the request rows and their attempts come
/// from the ledger assembly the terminal tail also uses.
pub(super) async fn requests_page(
    State(state): State<Arc<AppState>>,
    Query(query): Query<RequestsQuery>,
) -> Response {
    let lang = Lang::from_param(query.lang.as_deref());
    let coverage = match query.coverage.as_deref() {
        Some("covered") => Some(crate::domain::GateClass::Covered),
        Some("metered") => Some(crate::domain::GateClass::Metered),
        _ => None,
    };
    let filters =
        requests::Filters::sanitise(query.limit, query.errors.as_deref() == Some("1"), coverage);
    let history =
        state
            .backend
            .request_history(filters.limit, filters.errors_only, filters.coverage);
    let body = requests::render(history.as_deref(), filters, lang);
    page(&state, lang, Nav::Requests, body)
}

#[derive(Debug, Deserialize)]
pub(super) struct PricesQuery {
    lang: Option<String>,
}

/// The Prices page: configured rates against learned rates. Read-only.
pub(super) async fn prices_page(
    State(state): State<Arc<AppState>>,
    Query(query): Query<PricesQuery>,
) -> Response {
    let lang = Lang::from_param(query.lang.as_deref());
    let table = state.backend.price_table();
    let body = prices::render(table.as_ref(), lang);
    page(&state, lang, Nav::Prices, body)
}

#[derive(Debug, Deserialize)]
pub(super) struct ForgetRequest {
    provider: Option<String>,
    model: Option<String>,
}

/// Forget learned prices in one scope. A guarded mutation: the console guard has
/// already required JSON, the custom header and a same-origin request; the
/// write is the same ledger write the CLI command performs, and the live table
/// is republished so the configured card takes over immediately.
pub(super) async fn prices_forget(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ForgetRequest>,
) -> Response {
    let provider = request.provider.filter(|value| !value.trim().is_empty());
    let model = request.model.filter(|value| !value.trim().is_empty());
    if model.is_some() && provider.is_none() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({ "error": "a model scope needs its provider" })),
        )
            .into_response();
    }
    let _serialized = state.console.apply_lock.lock().await;
    match state
        .backend
        .forget_prices(provider.as_deref(), model.as_deref())
    {
        Ok(forgotten) => (
            StatusCode::OK,
            Json(serde_json::json!({ "forgotten": forgotten })),
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": error.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct ProbeRequest {
    provider: Option<String>,
    account: Option<String>,
}

/// Probe one account or every account. The network call happens inside this
/// request with a bounded timeout: a provider that never answers cannot hang the
/// page, it gets a timeout fragment.
pub(super) async fn accounts_probe(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ProbeRequest>,
) -> Response {
    let provider = request.provider.filter(|value| !value.trim().is_empty());
    let account = request.account.filter(|value| !value.trim().is_empty());
    if provider.is_some() != account.is_some() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({ "error": "a probe needs both a provider and an account, or neither" })),
        )
            .into_response();
    }
    let probing = state
        .backend
        .probe_accounts(provider.as_deref(), account.as_deref());
    match tokio::time::timeout(std::time::Duration::from_secs(15), probing).await {
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(serde_json::json!({ "error": "the provider did not answer within 15 seconds" })),
        )
            .into_response(),
        Ok(Err(error)) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": sanitised(&error.0) })),
        )
            .into_response(),
        Ok(Ok(())) => (StatusCode::OK, Json(serde_json::json!({ "probed": true }))).into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct AccountStateRequest {
    provider: Option<String>,
    account: Option<String>,
    enabled: bool,
}

/// Operator disable/enable. A disable is not undone by probes; an enable only
/// returns the account to Unknown until a probe proves it can pay.
pub(super) async fn accounts_state(
    State(state): State<Arc<AppState>>,
    Json(request): Json<AccountStateRequest>,
) -> Response {
    let provider = request.provider.filter(|value| !value.trim().is_empty());
    let account = request.account.filter(|value| !value.trim().is_empty());
    let (Some(provider), Some(account)) = (provider, account) else {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({ "error": "a state change needs a provider and an account" })),
        )
            .into_response();
    };
    match state
        .backend
        .set_account_enabled(&provider, &account, request.enabled)
    {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({ "enabled": request.enabled })),
        )
            .into_response(),
        Err(error) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({ "error": sanitised(&error.0) })),
        )
            .into_response(),
    }
}

/// Provider error text is displayed to an operator, not written to a log: keep
/// it to one short line and drop control characters.
pub(super) fn sanitised(message: &str) -> String {
    let flat: String = message
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let clean = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if clean.chars().count() > 300 {
        clean.chars().take(300).collect()
    } else {
        clean
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct DoctorQuery {
    lang: Option<String>,
}

/// The Doctor page. Runs the same checks as the terminal command, with every
/// network step bounded; the file it reads is the one the console describes.
pub(super) async fn doctor_page(
    State(state): State<Arc<AppState>>,
    Query(query): Query<DoctorQuery>,
) -> Response {
    let lang = Lang::from_param(query.lang.as_deref());
    let path = std::path::PathBuf::from(&state.console.config_path);
    let report = match Config::load(&path) {
        Err(error) => doctor_view::unavailable(&crate::doctor::sanitise(&format!("{error:#}"))),
        Ok(config) => {
            let credentials = crate::secrets::CredentialStore::from_env();
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(6))
                .build()
                .unwrap_or_default();
            match tokio::time::timeout(
                std::time::Duration::from_secs(20),
                crate::doctor::run(
                    &config,
                    &credentials,
                    &client,
                    std::time::Duration::from_secs(6),
                ),
            )
            .await
            {
                Ok(report) => report,
                Err(_) => doctor_view::unavailable("doctor timed out while checking providers"),
            }
        }
    };
    let body = doctor_view::render(&report, lang);
    page(&state, lang, Nav::Doctor, body)
}

#[derive(Debug, Deserialize)]
pub(super) struct KeysQuery {
    lang: Option<String>,
}

/// The Keys page: sources and statuses, never values.
pub(super) async fn keys_page(
    State(state): State<Arc<AppState>>,
    Query(query): Query<KeysQuery>,
) -> Response {
    let lang = Lang::from_param(query.lang.as_deref());
    let report = state.backend.keys_report();
    let body = crate::console::keys::render(report.as_ref(), lang);
    page(&state, lang, Nav::Keys, body)
}

#[derive(Debug, Deserialize)]
pub(super) struct KeyAddRequest {
    provider: Option<String>,
    account: Option<String>,
    key: Option<String>,
}

pub(super) async fn keys_add(
    State(state): State<Arc<AppState>>,
    Json(request): Json<KeyAddRequest>,
) -> Response {
    let provider = request.provider.filter(|value| !value.trim().is_empty());
    let account = request.account.filter(|value| !value.trim().is_empty());
    let secret = request.key.filter(|value| !value.trim().is_empty());
    let (Some(provider), Some(account), Some(secret)) = (provider, account, secret) else {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({ "error": "provider, account and key are required" })),
        )
            .into_response();
    };
    match state.backend.key_add(&provider, &account, &secret) {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({ "stored": true }))).into_response(),
        Err(error) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({ "error": sanitised(&error.0) })),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct KeyTargetRequest {
    provider: Option<String>,
    account: Option<String>,
}

fn key_target(request: KeyTargetRequest) -> Option<(String, String)> {
    let provider = request.provider.filter(|value| !value.trim().is_empty())?;
    let account = request.account.filter(|value| !value.trim().is_empty())?;
    Some((provider, account))
}

pub(super) async fn keys_test(
    State(state): State<Arc<AppState>>,
    Query(query): Query<LangQuery>,
    Json(request): Json<KeyTargetRequest>,
) -> Response {
    let lang = query.lang();
    let s = lang.strings();
    let Some((provider, account)) = key_target(request) else {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({ "error": "provider and account are required" })),
        )
            .into_response();
    };
    let tested = state.backend.key_test(&provider, &account);
    match tokio::time::timeout(std::time::Duration::from_secs(15), tested).await {
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(serde_json::json!({ "error": "the provider did not answer within 15 seconds" })),
        )
            .into_response(),
        Ok(Err(error)) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": sanitised(&error.0) })),
        )
            .into_response(),
        Ok(Ok(outcome)) => {
            let detail = match outcome {
                crate::keys::KeyTestOutcome::LocalToken { chars } => {
                    s.keys_test_local.replacen("{}", &chars.to_string(), 1)
                }
                crate::keys::KeyTestOutcome::NoProbe { chars } => {
                    s.keys_test_no_probe.replacen("{}", &chars.to_string(), 1)
                }
                crate::keys::KeyTestOutcome::Probed { probe, gates } => s
                    .keys_test_probed
                    .replacen("{}", &probe, 1)
                    .replacen("{}", &gates.to_string(), 1),
            };
            (
                StatusCode::OK,
                Json(serde_json::json!({ "detail": detail })),
            )
                .into_response()
        }
    }
}

pub(super) async fn keys_remove(
    State(state): State<Arc<AppState>>,
    Json(request): Json<KeyTargetRequest>,
) -> Response {
    let Some((provider, account)) = key_target(request) else {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({ "error": "provider and account are required" })),
        )
            .into_response();
    };
    match state.backend.key_remove(&provider, &account) {
        Ok(removed) => (
            StatusCode::OK,
            Json(serde_json::json!({ "removed": removed })),
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": sanitised(&error.0) })),
        )
            .into_response(),
    }
}

/// CSS, JavaScript, the theme bootstrap and the favicon, all embedded in the
/// binary. Only exact names resolve, so a path traversal has nothing to reach.
pub(super) async fn asset(Path(path): Path<String>) -> Response {
    match assets::get(&path) {
        Some(asset) => {
            let body = match asset.body {
                crate::console::assets::Body::Embedded(bytes) => Body::from(bytes),
                crate::console::assets::Body::Owned(bytes) => Body::from(bytes),
            };
            ([(header::CONTENT_TYPE, asset.content_type)], body).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// The guard every console route runs behind.
pub(super) async fn guard_console(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0);
    let headers = request.headers();
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    let console_header = headers.get("x-lgw-console").and_then(|v| v.to_str().ok());
    let mutation = !matches!(*request.method(), Method::GET | Method::HEAD);

    match guard::decide(
        peer,
        host,
        state.console.port,
        mutation,
        content_type,
        console_header,
        origin,
    ) {
        Verdict::Allow => next.run(request).await,
        Verdict::NotFound => plain(StatusCode::NOT_FOUND, "not found"),
        Verdict::Forbidden => plain(StatusCode::FORBIDDEN, "forbidden"),
        Verdict::UnsupportedMediaType => plain(StatusCode::UNSUPPORTED_MEDIA_TYPE, "json required"),
    }
}

/// Headers on every console response, including the guard's refusals: no
/// framing, no sniffing, no caching, and scripts only from the console itself.
/// Inline style attributes are allowed because the charts set bar widths there;
/// no inline script is.
pub(super) async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}

fn page(state: &AppState, lang: Lang, nav: Nav, body: String) -> Response {
    let strings = lang.strings();
    let title = match nav {
        Nav::Board => strings.nav_board,
        Nav::Usage => strings.nav_usage,
        Nav::Requests => strings.nav_requests,
        Nav::Prices => strings.nav_prices,
        Nav::Config => strings.nav_config,
        Nav::Keys => strings.nav_keys,
        Nav::Doctor => strings.nav_doctor,
    };
    let html = layout::render(
        lang,
        nav,
        title,
        &state.console.listen,
        &state.console.config_path,
        &body,
    );
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        Body::from(html),
    )
        .into_response()
}

fn plain(status: StatusCode, text: &'static str) -> Response {
    (status, text).into_response()
}
