//! The decision behind every console request: who may reach it and which
//! mutations are allowed.
//!
//! The rules are pure functions of request metadata so they can be exhaustively
//! tested without a listener: a peer address, a Host header, and for mutations
//! the Origin, content type and the console's own header. The HTTP layer turns a
//! Verdict into the status code and the security headers.

use std::net::SocketAddr;

/// What the console decided about a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Serve it.
    Allow,
    /// Answer 404: the console does not exist for this caller.
    NotFound,
    /// Answer 403: the caller may see the console but not this write.
    Forbidden,
    /// Answer 415: a write must be JSON.
    UnsupportedMediaType,
}

/// The console answers loopback peers only, whether or not a local key is set.
pub fn peer_allowed(peer: Option<SocketAddr>) -> bool {
    peer.is_some_and(|addr| addr.ip().is_loopback())
}

/// The port the listener was configured with, used to pin the Host allowlist.
pub fn listen_port(listen: &str) -> Option<u16> {
    listen.rsplit(':').next()?.trim().parse().ok()
}

/// A Host header must name the loopback listener itself; anything else is a
/// name that resolved here (DNS rebinding) or a proxy we did not ask for.
pub fn host_allowed(host: Option<&str>, port: u16) -> bool {
    let Some(host) = host else {
        return false;
    };
    let host = host.trim();
    let suffix = format!(":{port}");
    let Some(name) = host.strip_suffix(&suffix) else {
        return false;
    };
    ["127.0.0.1", "localhost", "[::1]"]
        .iter()
        .any(|allowed| name.eq_ignore_ascii_case(allowed))
}

/// A browser sends Origin on a cross-origin request; only the console's own
/// loopback origins are accepted. A client without one (curl) passes this and
/// still has to carry the console header.
pub fn origin_allowed(origin: Option<&str>, port: u16) -> bool {
    let Some(origin) = origin else {
        return true;
    };
    let origin = origin.trim();
    [
        format!("http://127.0.0.1:{port}"),
        format!("http://localhost:{port}"),
        format!("http://[::1]:{port}"),
    ]
    .iter()
    .any(|allowed| origin.eq_ignore_ascii_case(allowed))
}

/// Reads need the peer and Host rules; writes additionally need a same-origin
/// JSON request carrying the console header, which no cross-site form can send.
pub fn decide(
    peer: Option<SocketAddr>,
    host: Option<&str>,
    port: u16,
    mutation: bool,
    content_type: Option<&str>,
    console_header: Option<&str>,
    origin: Option<&str>,
) -> Verdict {
    if !peer_allowed(peer) || !host_allowed(host, port) {
        return Verdict::NotFound;
    }
    if !mutation {
        return Verdict::Allow;
    }
    if !is_json(content_type) {
        return Verdict::UnsupportedMediaType;
    }
    if console_header.is_none_or(|value| value.trim() != "1") {
        return Verdict::Forbidden;
    }
    if !origin_allowed(origin, port) {
        return Verdict::Forbidden;
    }
    Verdict::Allow
}

fn is_json(content_type: Option<&str>) -> bool {
    let Some(value) = content_type else {
        return false;
    };
    value
        .split(';')
        .next()
        .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("application/json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PORT: u16 = 8787;
    fn loopback() -> Option<SocketAddr> {
        Some("127.0.0.1:50000".parse().unwrap())
    }
    fn lan() -> Option<SocketAddr> {
        Some("192.168.1.10:50000".parse().unwrap())
    }

    #[test]
    fn only_a_loopback_peer_reaches_the_console() {
        assert!(peer_allowed(loopback()));
        assert!(peer_allowed(Some("[::1]:50000".parse().unwrap())));
        assert!(!peer_allowed(lan()));
        assert!(!peer_allowed(None));
    }

    #[test]
    fn the_host_must_name_the_loopback_listener() {
        assert!(host_allowed(Some("127.0.0.1:8787"), PORT));
        assert!(host_allowed(Some("localhost:8787"), PORT));
        assert!(host_allowed(Some("[::1]:8787"), PORT));
        assert!(host_allowed(Some("LOCALHOST:8787"), PORT));

        assert!(!host_allowed(Some("evil.example:8787"), PORT));
        assert!(!host_allowed(Some("127.0.0.1:9999"), PORT));
        assert!(!host_allowed(Some("127.0.0.1"), PORT));
        assert!(!host_allowed(None, PORT));
    }

    #[test]
    fn an_origin_must_be_the_console_itself() {
        assert!(origin_allowed(None, PORT));
        assert!(origin_allowed(Some("http://127.0.0.1:8787"), PORT));
        assert!(origin_allowed(Some("http://localhost:8787"), PORT));
        assert!(!origin_allowed(Some("https://evil.example"), PORT));
        assert!(!origin_allowed(Some("null"), PORT));
        assert!(!origin_allowed(Some("http://127.0.0.1:9999"), PORT));
    }

    #[test]
    fn a_read_needs_only_peer_and_host() {
        assert_eq!(
            decide(
                loopback(),
                Some("127.0.0.1:8787"),
                PORT,
                false,
                None,
                None,
                None
            ),
            Verdict::Allow
        );
    }

    #[test]
    fn a_write_needs_json_and_the_console_header() {
        let allowed = decide(
            loopback(),
            Some("127.0.0.1:8787"),
            PORT,
            true,
            Some("application/json"),
            Some("1"),
            None,
        );
        assert_eq!(allowed, Verdict::Allow);

        // A plain cross-site form can send neither JSON nor the header.
        assert_eq!(
            decide(
                loopback(),
                Some("127.0.0.1:8787"),
                PORT,
                true,
                Some("application/x-www-form-urlencoded"),
                Some("1"),
                None
            ),
            Verdict::UnsupportedMediaType
        );
        assert_eq!(
            decide(
                loopback(),
                Some("127.0.0.1:8787"),
                PORT,
                true,
                Some("application/json"),
                None,
                None
            ),
            Verdict::Forbidden
        );
        assert_eq!(
            decide(
                loopback(),
                Some("127.0.0.1:8787"),
                PORT,
                true,
                Some("application/json; charset=utf-8"),
                Some("1"),
                Some("http://evil.example")
            ),
            Verdict::Forbidden
        );
    }

    #[test]
    fn a_non_loopback_peer_or_foreign_host_is_a_404_even_for_reads() {
        assert_eq!(
            decide(lan(), Some("127.0.0.1:8787"), PORT, false, None, None, None),
            Verdict::NotFound
        );
        assert_eq!(
            decide(
                loopback(),
                Some("evil.example:8787"),
                PORT,
                false,
                None,
                None,
                None
            ),
            Verdict::NotFound
        );
    }

    #[test]
    fn the_listener_port_is_read_from_the_configured_address() {
        assert_eq!(listen_port("127.0.0.1:8787"), Some(8787));
        assert_eq!(listen_port("0.0.0.0:9000"), Some(9000));
        assert_eq!(listen_port("[::1]:8787"), Some(8787));
        assert_eq!(listen_port("localhost:8787"), Some(8787));
        assert_eq!(listen_port("localhost"), None);
    }
}
