//! light-ai-gateway: a small OpenAI-compatible gateway that pools multiple
//! provider accounts and routes around exhausted quota windows.

pub mod accounts;
pub mod ansi;
pub mod cli;
pub mod config;
pub mod console;
pub mod domain;
pub mod error;
pub mod http;
pub mod interval;
pub mod meter;
pub mod quota;
pub mod quota_report;
pub mod router;
pub mod secrets;
pub mod sse;
pub mod store;
pub mod table;
pub mod tier;

pub use config::Config;
pub use domain::{Account, Gate, GateClass, GateKind, TokenUsage};
pub use error::{ErrorSignals, UpstreamErrorClass};

/// The candidate names that look most like the one that was not found.
///
/// The reason a model name is missing from a provider's catalog is almost always
/// its vendor prefix: `glm/glm-5.3-flash` against a catalog full of
/// `z-ai/glm-5.3-flash`. So the names worth showing are the ones that share the
/// model's own last path segment, not the first few alphabetically -- those are
/// just whichever vendor sorts first. Empty when nothing is close, so a caller
/// can leave the hint out rather than print a list that answers nothing.
pub fn closest_names<'a>(
    candidates: impl IntoIterator<Item = &'a str>,
    wanted: &str,
    limit: usize,
) -> Vec<&'a str> {
    let tail = wanted
        .rsplit('/')
        .next()
        .unwrap_or(wanted)
        .to_ascii_lowercase();
    if tail.is_empty() {
        return Vec::new();
    }
    let mut close: Vec<&'a str> = candidates
        .into_iter()
        .filter(|name| name.to_ascii_lowercase().contains(tail.as_str()))
        .collect();
    close.truncate(limit);
    close
}

/// Read an environment variable, falling back to the spelling it had before the
/// command was renamed from ai-gw to lgw.
///
/// An AI_GW_* exported in a shell profile or a CI job keeps working rather than
/// silently losing its meaning, and the warning names the replacement. Returns
/// None when neither spelling is set.
pub fn env_preferring(current: &str, legacy: &str) -> Option<String> {
    if let Some(value) = std::env::var_os(current) {
        return Some(value.to_string_lossy().into_owned());
    }
    let value = std::env::var_os(legacy)?;
    tracing::warn!("{legacy} is deprecated; use {current} instead");
    Some(value.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The case this exists for: a wrong vendor prefix, same model name.
    #[test]
    fn the_closest_names_are_the_ones_that_share_the_model_name() {
        let catalog = [
            "anthropic/claude-opus-5",
            "openai/gpt-5.6-sol",
            "z-ai/glm-5.3-flash",
        ];
        assert_eq!(
            closest_names(catalog, "glm/glm-5.3-flash", 4),
            vec!["z-ai/glm-5.3-flash"],
            "the vendor prefix is the part that differs"
        );
        // Case is not the point: an id spelled differently is still the id.
        assert_eq!(
            closest_names(catalog, "z-ai/GLM-5.3-Flash", 4),
            vec!["z-ai/glm-5.3-flash"]
        );
    }

    #[test]
    fn nothing_close_is_an_empty_hint_rather_than_a_guess() {
        let catalog = ["anthropic/claude-opus-5", "openai/gpt-5.6-sol"];
        assert!(closest_names(catalog, "z-ai/glm-5.3-flash", 4).is_empty());
        assert!(closest_names(catalog, "", 4).is_empty());
    }

    #[test]
    fn the_hint_is_bounded() {
        let catalog: Vec<String> = (0..20).map(|i| format!("z-ai/glm-5.3-{i}")).collect();
        let names = closest_names(catalog.iter().map(String::as_str), "glm/x", 4);
        // "glm/x" has no tail in common, so the hint is empty rather than 20 ids.
        assert!(names.is_empty());
        let names = closest_names(catalog.iter().map(String::as_str), "other/glm-5.3-1", 4);
        assert_eq!(names.len(), 4);
    }
}
