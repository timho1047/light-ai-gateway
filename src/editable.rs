//! The editable-field policy: which document paths the Console may write, how
//! they are typed, whether an Apply can make them live, and which are secrets.
//!
//! This is a policy table over a TOML document, not a second Config model. It
//! names paths so the renderer, the draft editor and the Apply/restart checks
//! can ask the same question instead of maintaining parallel whitelists.

use serde::Deserialize;
use serde::Serialize;

/// The kinds of leaf the Console's forms can write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ValueKind {
    String,
    Integer,
    Float,
    Boolean,
}

/// When a change to a field can take effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldClass {
    /// The running snapshot can adopt it without a restart.
    Live,
    /// A draft may carry it, but Apply refuses it until the process restarts.
    RestartOnly,
    /// The Console never writes it. Secrets, derived markers and read-only
    /// conveniences live here.
    ReadOnly,
}

/// What the editable-field policy knows about one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldPolicy {
    pub kind: ValueKind,
    pub class: FieldClass,
    pub secret: bool,
}

impl FieldPolicy {
    const fn live(kind: ValueKind) -> Self {
        FieldPolicy {
            kind,
            class: FieldClass::Live,
            secret: false,
        }
    }

    const fn restart(kind: ValueKind) -> Self {
        FieldPolicy {
            kind,
            class: FieldClass::RestartOnly,
            secret: false,
        }
    }

    const fn readonly(kind: ValueKind) -> Self {
        FieldPolicy {
            kind,
            class: FieldClass::ReadOnly,
            secret: false,
        }
    }

    const fn secret(kind: ValueKind) -> Self {
        FieldPolicy {
            kind,
            class: FieldClass::ReadOnly,
            secret: true,
        }
    }
}

/// The policy for one path. Paths are document addresses, not rendered labels.
pub fn field(path: &[String]) -> Option<FieldPolicy> {
    let path: Vec<&str> = path.iter().map(String::as_str).collect();
    match path.as_slice() {
        ["server", "listen"] | ["server", "db_path"] => {
            Some(FieldPolicy::restart(ValueKind::String))
        }
        ["server", "local_key"] => Some(FieldPolicy::secret(ValueKind::String)),
        ["server", "local_key", ..] => Some(FieldPolicy::secret(ValueKind::String)),

        ["server", _] => None,

        ["providers", _, "adapter"]
        | ["providers", _, "base_url"]
        | ["providers", _, "probe_base_url"]
        | ["providers", _, "currency"] => Some(FieldPolicy::live(ValueKind::String)),
        ["providers", _, "credit_multiplier"] => Some(FieldPolicy::live(ValueKind::Float)),
        ["providers", _, "allow_metered"] => Some(FieldPolicy::readonly(ValueKind::Boolean)),
        ["providers", _, "accounts", _, "key", ..] => Some(FieldPolicy::secret(ValueKind::String)),
        ["providers", _, "accounts", _, "label"] | ["providers", _, "accounts", _, "currency"] => {
            Some(FieldPolicy::live(ValueKind::String))
        }
        ["providers", _, "accounts", _, "credit_multiplier"] => {
            Some(FieldPolicy::live(ValueKind::Float))
        }
        ["providers", _, "accounts", _, "concurrency_limit"]
        | ["providers", _, "accounts", _, "rpm_limit"] => {
            Some(FieldPolicy::live(ValueKind::Integer))
        }
        ["providers", _, "accounts", _, "allow_metered"] => {
            Some(FieldPolicy::readonly(ValueKind::Boolean))
        }

        ["routes", _] | ["provider_prefix_aliases", _] => {
            Some(FieldPolicy::live(ValueKind::String))
        }

        ["prices", _, _, "input"] | ["prices", _, _, "output"] | ["prices", _, _, "cache_read"] => {
            Some(FieldPolicy::live(ValueKind::Float))
        }
        ["prices", _, _, "schedules", _, "name"]
        | ["prices", _, _, "schedules", _, "days"]
        | ["prices", _, _, "schedules", _, "hours"]
        | ["prices", _, _, "schedules", _, "tz"] => Some(FieldPolicy::live(ValueKind::String)),
        ["prices", _, _, "schedules", _, "input"]
        | ["prices", _, _, "schedules", _, "output"]
        | ["prices", _, _, "schedules", _, "cache_read"] => {
            Some(FieldPolicy::live(ValueKind::Float))
        }

        ["policy", "soft_threshold"]
        | ["policy", "drift_threshold"]
        | ["policy", "hard_margin"] => Some(FieldPolicy::live(ValueKind::Float)),
        ["policy", "allow_metered"] | ["policy", "probe_before_switch"] => {
            Some(FieldPolicy::live(ValueKind::Boolean))
        }
        ["policy", "poll_interval_secs"]
        | ["policy", "poll_interval_active_secs"]
        | ["policy", "poll_interval_idle_secs"]
        | ["policy", "max_retries"]
        | ["policy", "connect_timeout_secs"]
        | ["policy", "ttft_timeout_secs"]
        | ["policy", "idle_timeout_secs"] => Some(FieldPolicy::live(ValueKind::Integer)),
        ["policy", "max_body_bytes"] => Some(FieldPolicy::restart(ValueKind::Integer)),

        _ => None,
    }
}

pub fn kind(path: &[String]) -> Option<ValueKind> {
    field(path).map(|policy| policy.kind)
}

pub fn is_restart_only(path: &[String]) -> bool {
    field(path).is_some_and(|policy| policy.class == FieldClass::RestartOnly)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(segments: &[&str]) -> Vec<String> {
        segments.iter().map(|segment| segment.to_string()).collect()
    }

    #[test]
    fn every_rendered_path_has_one_policy() {
        let required = [
            path(&["server", "listen"]),
            path(&["policy", "soft_threshold"]),
            path(&["policy", "max_body_bytes"]),
            path(&["providers", "mock", "base_url"]),
            path(&["providers", "mock", "accounts", "acc-1", "label"]),
            path(&["providers", "mock", "accounts", "acc-1", "rpm_limit"]),
            path(&["routes", "z-ai/glm"]),
            path(&["provider_prefix_aliases", "glm"]),
            path(&["prices", "mock", "z-ai/glm", "input"]),
            path(&["prices", "mock", "z-ai/glm", "schedules", "0", "days"]),
        ];
        for path in required {
            assert!(field(&path).is_some(), "missing policy for {path:?}");
        }
    }

    #[test]
    fn secret_paths_are_never_writable() {
        for path in [
            path(&["providers", "mock", "accounts", "acc-1", "key"]),
            path(&["providers", "mock", "accounts", "acc-1", "key", "literal"]),
            path(&["server", "local_key"]),
        ] {
            let policy = field(&path).expect("secret policy");
            assert!(policy.secret, "{path:?}");
            assert_eq!(policy.class, FieldClass::ReadOnly);
        }
    }

    #[test]
    fn read_only_conveniences_are_not_writable() {
        let policy = field(&path(&["providers", "mock", "allow_metered"])).expect("policy");
        assert_eq!(policy.class, FieldClass::ReadOnly);
        assert!(!policy.secret);
    }

    #[test]
    fn restart_only_fields_are_named() {
        assert!(is_restart_only(&path(&["server", "listen"])));
        assert!(is_restart_only(&path(&["policy", "max_body_bytes"])));
        assert!(!is_restart_only(&path(&["policy", "soft_threshold"])));
    }
}
