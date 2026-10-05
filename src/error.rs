//! Upstream error classification.
//!
//! FROZEN CONTRACT: every module classifies upstream failures through these shapes.
//!
//! Why this exists: a 429 has several completely different meanings and the
//! correct reaction to each is different. Treating a burst rate limit as
//! quota exhausted rotates the account pointlessly and destroys the prompt
//! cache; treating exhausted quota as a burst limit lets the request stream
//! straight into real money. Providers document that branching must be done on
//! machine-readable fields, never on the human-readable message, which is
//! reworded freely between releases.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// What an upstream failure actually means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamErrorClass {
    /// Short-lived pacing limit. Retry on the SAME account; do not rotate,
    /// so the prompt cache stays warm.
    RateLimitBurst,
    /// The account's window is used up. Park it and move on.
    QuotaExhausted,
    /// Out of money (HTTP 402, insufficient_quota). Retrying cannot help.
    InsufficientCredit,
    /// Upstream overloaded or 5xx. Bounded retry, then another account.
    Overloaded,
    /// Bad or revoked credentials.
    Auth,
    /// The request itself is invalid; retrying it unchanged will fail again.
    BadRequest,
    /// Upstream timed out.
    Timeout,
    /// Could not reach upstream at all.
    Transport,
    Other,
}

impl UpstreamErrorClass {
    /// Is backing off on the same account the right move?
    pub fn retry_same_account(self) -> bool {
        matches!(
            self,
            UpstreamErrorClass::RateLimitBurst | UpstreamErrorClass::Overloaded
        )
    }

    /// Is trying a DIFFERENT account the right move?
    ///
    /// Timeout belongs here: the client has received nothing, so a switch is
    /// still invisible, and another account may well be healthy. The cost of
    /// being wrong is bounded by the retry budget, whereas refusing to rotate
    /// turns one slow account into a failed request.
    pub fn retry_other_account(self) -> bool {
        matches!(
            self,
            UpstreamErrorClass::QuotaExhausted
                | UpstreamErrorClass::InsufficientCredit
                | UpstreamErrorClass::Overloaded
                | UpstreamErrorClass::Auth
                | UpstreamErrorClass::Transport
                | UpstreamErrorClass::Timeout
        )
    }

    /// Should the account be taken out of the pool until an operator acts?
    pub fn disables_account(self) -> bool {
        matches!(
            self,
            UpstreamErrorClass::InsufficientCredit | UpstreamErrorClass::Auth
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            UpstreamErrorClass::RateLimitBurst => "rate_limit_burst",
            UpstreamErrorClass::QuotaExhausted => "quota_exhausted",
            UpstreamErrorClass::InsufficientCredit => "insufficient_credit",
            UpstreamErrorClass::Overloaded => "overloaded",
            UpstreamErrorClass::Auth => "auth",
            UpstreamErrorClass::BadRequest => "bad_request",
            UpstreamErrorClass::Timeout => "timeout",
            UpstreamErrorClass::Transport => "transport",
            UpstreamErrorClass::Other => "other",
        }
    }
}

/// Everything we can learn from an upstream failure.
#[derive(Debug, Clone, Default)]
pub struct ErrorSignals {
    pub status: Option<u16>,
    /// Parsed from the Retry-After header.
    pub retry_after: Option<Duration>,
    /// Machine-readable error type, for example "rate_limit_error".
    pub error_type: Option<String>,
    /// Machine-readable error code, for example "insufficient_quota".
    pub error_code: Option<String>,
    /// Set by the caller when a quota probe independently shows the account is
    /// at or over its cap. This is the only reliable way to tell an exhausted
    /// window from a burst limit on providers that reuse one error code for
    /// both, which xKiro does.
    pub window_exhausted_hint: bool,
    pub transport_failure: bool,
    pub timed_out: bool,
}

/// Retry-After at or below this is treated as pacing, not exhaustion.
pub const BURST_RETRY_AFTER: Duration = Duration::from_secs(20);

impl ErrorSignals {
    pub fn classify(&self) -> UpstreamErrorClass {
        if self.transport_failure {
            return UpstreamErrorClass::Transport;
        }
        if self.timed_out {
            return UpstreamErrorClass::Timeout;
        }
        if self.window_exhausted_hint {
            return UpstreamErrorClass::QuotaExhausted;
        }

        let code = self
            .error_code
            .as_deref()
            .unwrap_or("")
            .to_ascii_lowercase();
        let ty = self
            .error_type
            .as_deref()
            .unwrap_or("")
            .to_ascii_lowercase();

        // A few upstreams use their own vocabularies for the same conditions.
        let known_quota = matches!(
            code.as_str(),
            "insufficient_quota"
                | "quota_exceeded"
                | "exceeded_current_quota_error"
                | "usage_exceeded"
        );
        let known_auth = matches!(
            code.as_str(),
            "invalid_api_key" | "authentication_error" | "permission_denied"
        );

        match self.status {
            Some(402) => UpstreamErrorClass::InsufficientCredit,
            Some(401) | Some(403) => UpstreamErrorClass::Auth,
            Some(429) => {
                // The crux: one status code, two meanings. A short Retry-After
                // is pacing; anything longer, or a quota-shaped code, means the
                // window is gone and the account should be parked.
                let quota_shaped =
                    known_quota || ty.contains("quota") || ty.contains("insufficient");
                let short_pacing = self
                    .retry_after
                    .map(|d| d <= BURST_RETRY_AFTER)
                    .unwrap_or(false);
                if quota_shaped {
                    // An explicit quota code outranks Retry-After: a provider
                    // that names the condition means it.
                    UpstreamErrorClass::QuotaExhausted
                } else if short_pacing {
                    UpstreamErrorClass::RateLimitBurst
                } else if self.retry_after.is_none() {
                    // No hint at all: assume the cheap, cache-preserving option
                    // and let the caller escalate via a quota probe if needed.
                    UpstreamErrorClass::RateLimitBurst
                } else {
                    UpstreamErrorClass::QuotaExhausted
                }
            }
            Some(408) => UpstreamErrorClass::Timeout,
            Some(500) | Some(502) | Some(503) | Some(529) => UpstreamErrorClass::Overloaded,
            Some(400) | Some(404) | Some(422) => {
                if known_quota {
                    UpstreamErrorClass::QuotaExhausted
                } else if known_auth {
                    UpstreamErrorClass::Auth
                } else {
                    UpstreamErrorClass::BadRequest
                }
            }
            _ => UpstreamErrorClass::Other,
        }
    }
}

/// Extract error type/code from a provider error body, tolerating both the
/// OpenAI envelope (error.type / error.code) and the Anthropic one
/// (top-level type plus error.type).
pub fn parse_error_body(body: &str) -> (Option<String>, Option<String>) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return (None, None);
    };
    let obj = v.get("error").unwrap_or(&v);
    let ty = obj.get("type").and_then(|x| x.as_str()).map(str::to_string);
    let code = obj.get("code").and_then(|x| x.as_str()).map(str::to_string);
    (ty, code)
}

/// Parse a Retry-After value: delta-seconds, or an HTTP date.
pub fn parse_retry_after(raw: &str, now: time::OffsetDateTime) -> Option<Duration> {
    let t = raw.trim();
    if t.is_empty() {
        return None;
    }
    if let Ok(secs) = t.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    if let Ok(parsed) =
        time::OffsetDateTime::parse(t, &time::format_description::well_known::Rfc2822)
    {
        let delta = parsed - now;
        if delta.is_negative() {
            return Some(Duration::ZERO);
        }
        return Some(delta.unsigned_abs());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(status: u16) -> ErrorSignals {
        ErrorSignals {
            status: Some(status),
            ..Default::default()
        }
    }

    #[test]
    fn short_retry_after_on_429_is_pacing_not_quota() {
        let mut s = sig(429);
        s.retry_after = Some(Duration::from_secs(3));
        assert_eq!(s.classify(), UpstreamErrorClass::RateLimitBurst);
    }

    #[test]
    fn long_retry_after_on_429_is_quota() {
        let mut s = sig(429);
        s.retry_after = Some(Duration::from_secs(3_600));
        assert_eq!(s.classify(), UpstreamErrorClass::QuotaExhausted);
    }

    #[test]
    fn quota_shaped_code_wins_over_short_retry_after() {
        let mut s = sig(429);
        s.retry_after = Some(Duration::from_secs(2));
        s.error_code = Some("insufficient_quota".into());
        assert_eq!(s.classify(), UpstreamErrorClass::QuotaExhausted);
    }

    #[test]
    fn probe_hint_overrides_everything() {
        let mut s = sig(429);
        s.retry_after = Some(Duration::from_secs(1));
        s.window_exhausted_hint = true;
        assert_eq!(s.classify(), UpstreamErrorClass::QuotaExhausted);
    }

    #[test]
    fn payment_required_is_insufficient_credit_and_disables() {
        let c = sig(402).classify();
        assert_eq!(c, UpstreamErrorClass::InsufficientCredit);
        assert!(c.disables_account());
        assert!(!c.retry_same_account());
    }

    #[test]
    fn auth_disables_account() {
        assert!(sig(401).classify().disables_account());
        assert!(sig(403).classify().disables_account());
    }

    #[test]
    fn timeout_rotates_while_nothing_has_been_delivered() {
        let c = ErrorSignals {
            timed_out: true,
            ..Default::default()
        }
        .classify();
        assert_eq!(c, UpstreamErrorClass::Timeout);
        assert!(c.retry_other_account());
        assert!(!c.retry_same_account(), "do not sit on a stalled account");
        assert!(!c.disables_account());
    }

    #[test]
    fn bad_request_is_never_retried() {
        let c = sig(400).classify();
        assert_eq!(c, UpstreamErrorClass::BadRequest);
        assert!(!c.retry_same_account());
        assert!(!c.retry_other_account());
    }

    #[test]
    fn overloaded_retries_elsewhere() {
        for code in [500u16, 502, 503, 529] {
            assert_eq!(sig(code).classify(), UpstreamErrorClass::Overloaded);
        }
    }

    #[test]
    fn parses_openai_and_anthropic_error_envelopes() {
        let (t, c) = parse_error_body(
            r#"{"error":{"message":"x","type":"rate_limit_error","code":"rate_limit_exceeded"}}"#,
        );
        assert_eq!(t.as_deref(), Some("rate_limit_error"));
        assert_eq!(c.as_deref(), Some("rate_limit_exceeded"));

        let (t2, _) = parse_error_body(
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"y"}}"#,
        );
        assert_eq!(t2.as_deref(), Some("invalid_request_error"));
    }

    #[test]
    fn retry_after_parses_seconds() {
        let n = crate::domain::now();
        assert_eq!(parse_retry_after("120", n), Some(Duration::from_secs(120)));
        assert_eq!(parse_retry_after("", n), None);
    }
}
