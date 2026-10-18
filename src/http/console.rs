//! The console's HTTP surface: pages, the board partial, embedded assets and
//! the guard that keeps all of it to the loopback operator.
//!
//! The guard runs before every console route and answers 404 to a peer or Host
//! the console does not serve, so a network-exposed proxy listener does not
//! advertise an admin surface. Writes must be same-origin JSON with the
//! console's own header; a cross-site form can send none of the three.

use super::AppState;
use crate::config::Config;
use crate::config_doc::{self, ConfigDocument, DraftChange};
use crate::console::guard::{self, Verdict};
use crate::console::layout::esc;
use crate::console::{assets, board, config_page, copy::Lang, layout, layout::Nav};
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
}

impl ConsoleContext {
    pub fn new(listen: &str, port: u16, config_path: &str) -> ConsoleContext {
        ConsoleContext {
            listen: listen.to_string(),
            port,
            config_path: config_path.to_string(),
            started_at: crate::domain::now(),
            apply_lock: Arc::new(tokio::sync::Mutex::new(())),
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

    let started = state
        .console
        .started_at
        .replace_nanosecond(0)
        .unwrap_or(state.console.started_at);
    let last_applied = started
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string());
    let view = config_page::ConfigView {
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

#[derive(Debug, Deserialize)]
struct DraftEntry {
    path: Vec<String>,
    value: String,
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

    let original = match ConfigDocument::read(std::path::Path::new(&path)) {
        Ok(document) => document,
        Err(error) => {
            return fragment(
                StatusCode::UNPROCESSABLE_ENTITY,
                config_page::failed(&path, &error.to_string(), lang),
            );
        }
    };
    if original.fingerprint() != request.fingerprint {
        return fragment(StatusCode::CONFLICT, config_page::stale(lang));
    }

    let changes: Vec<DraftChange> = request
        .draft
        .into_iter()
        .map(|entry| DraftChange {
            path: entry.path,
            value: entry.value,
        })
        .collect();
    let edited = match original.with_draft(&changes) {
        Ok(document) => document,
        Err(error) => {
            return fragment(
                StatusCode::UNPROCESSABLE_ENTITY,
                config_page::rejected(&error.field, None, &error.message, lang),
            );
        }
    };

    let config: Config = match toml::from_str(edited.raw()) {
        Ok(config) => config,
        Err(error) => {
            return fragment(
                StatusCode::UNPROCESSABLE_ENTITY,
                config_page::rejected("config", None, &error.to_string(), lang),
            );
        }
    };
    if let Some(violation) = config.violation() {
        return fragment(
            StatusCode::UNPROCESSABLE_ENTITY,
            config_page::rejected(
                &violation.field,
                Some(violation.code),
                &violation.message,
                lang,
            ),
        );
    }

    let diff = config_doc::diff(&original, &edited);
    fragment(StatusCode::OK, config_page::validated(&diff, lang))
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
    let _serialized = state.console.apply_lock.lock().await;
    let path = state.console.config_path.clone();

    let original = match ConfigDocument::read(std::path::Path::new(&path)) {
        Ok(document) => document,
        Err(error) => {
            return fragment(
                StatusCode::UNPROCESSABLE_ENTITY,
                config_page::failed(&path, &error.to_string(), lang),
            );
        }
    };
    if original.fingerprint() != request.fingerprint {
        return fragment(StatusCode::CONFLICT, config_page::stale(lang));
    }

    let changes: Vec<DraftChange> = request
        .draft
        .into_iter()
        .map(|entry| DraftChange {
            path: entry.path,
            value: entry.value,
        })
        .collect();
    let not_live = live_scope_refusals(&changes);
    if !not_live.is_empty() {
        return fragment(
            StatusCode::UNPROCESSABLE_ENTITY,
            config_page::not_live(&not_live, lang),
        );
    }

    let edited = match original.with_draft(&changes) {
        Ok(document) => document,
        Err(error) => {
            return fragment(
                StatusCode::UNPROCESSABLE_ENTITY,
                config_page::rejected(&error.field, None, &error.message, lang),
            );
        }
    };
    let next: Config = match toml::from_str(edited.raw()) {
        Ok(config) => config,
        Err(error) => {
            return fragment(
                StatusCode::UNPROCESSABLE_ENTITY,
                config_page::rejected("config", None, &error.to_string(), lang),
            );
        }
    };
    if let Some(violation) = next.violation() {
        return fragment(
            StatusCode::UNPROCESSABLE_ENTITY,
            config_page::rejected(
                &violation.field,
                Some(violation.code),
                &violation.message,
                lang,
            ),
        );
    }

    let restart = restart_only_changes(state.backend.runtime().config(), &next);
    if !restart.is_empty() {
        return fragment(
            StatusCode::UNPROCESSABLE_ENTITY,
            config_page::restart_only(&restart, lang),
        );
    }

    let diff = config_doc::diff(&original, &edited);
    let fingerprint = edited.fingerprint();
    let raw = edited.raw().to_string();
    let write_path = path.clone();
    let mut write = move || {
        crate::config_doc::write_atomic(std::path::Path::new(&write_path), &raw)
            .map_err(|error| std::io::Error::other(error.to_string()))
    };
    match state.backend.apply_config(next, &mut write) {
        Ok(()) => fragment(
            StatusCode::OK,
            config_page::applied(&diff, &fingerprint, lang),
        ),
        Err(error) => fragment(
            StatusCode::INTERNAL_SERVER_ERROR,
            config_page::failed(&path, &error.to_string(), lang),
        ),
    }
}

/// Fields that only take effect on restart. Compared against the running
/// configuration, so a file that already differed from it is not mistaken for a
/// new edit.
fn restart_only_changes(old: &Config, new: &Config) -> Vec<String> {
    let mut fields = Vec::new();
    if old.server.listen != new.server.listen {
        fields.push("server.listen".to_string());
    }
    if old.server.db_path != new.server.db_path {
        fields.push("server.db_path".to_string());
    }
    let key_changed = match (&old.server.local_key, &new.server.local_key) {
        (None, None) => false,
        (Some(a), Some(b)) => !a.same_source(b),
        _ => true,
    };
    if key_changed {
        fields.push("server.local_key".to_string());
    }
    if old.policy.max_body_bytes != new.policy.max_body_bytes {
        fields.push("policy.max_body_bytes".to_string());
    }
    fields
}

/// The sections whose live apply has not landed yet. The form only edits
/// policy, so this is the guard for a crafted request: refusing loudly is
/// better than writing a file the running gateway would ignore.
fn live_scope_refusals(changes: &[DraftChange]) -> Vec<String> {
    let mut fields: Vec<String> = Vec::new();
    for change in changes {
        let section = change.path.first().map(String::as_str).unwrap_or("");
        if section != "policy" && section != "providers" {
            let field = change.path.join(".");
            if !fields.contains(&field) {
                fields.push(field);
            }
        }
    }
    fields
}

fn fragment(status: StatusCode, html: String) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        Body::from(html),
    )
        .into_response()
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
