//! The console's HTTP surface: pages, the board partial, embedded assets and
//! the guard that keeps all of it to the loopback operator.
//!
//! The guard runs before every console route and answers 404 to a peer or Host
//! the console does not serve, so a network-exposed proxy listener does not
//! advertise an admin surface. Writes must be same-origin JSON with the
//! console's own header; a cross-site form can send none of the three.

use super::AppState;
use crate::console::guard::{self, Verdict};
use crate::console::{assets, board, copy::Lang, layout, layout::Nav};
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
}

impl ConsoleContext {
    pub fn new(listen: &str, port: u16, config_path: &str) -> ConsoleContext {
        ConsoleContext {
            listen: listen.to_string(),
            port,
            config_path: config_path.to_string(),
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
    let snapshot = state.backend.snapshot();
    let at = crate::domain::now();
    let inner = board::render(&snapshot, at, lang);
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
    let snapshot = state.backend.snapshot();
    let html = board::render(&snapshot, crate::domain::now(), lang);
    (
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
