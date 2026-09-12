//! light-ai-gateway: a small OpenAI-compatible gateway that pools multiple
//! provider accounts and routes around exhausted quota windows.
//!
//! See docs/DESIGN.md for the design this implements.

pub mod accounts;
pub mod cli;
pub mod config;
pub mod domain;
pub mod error;
pub mod meter;
pub mod proxy;
pub mod quota;
pub mod router;
pub mod secrets;
pub mod sse;
pub mod store;
pub mod tier;

pub use config::Config;
pub use domain::{Account, Gate, GateClass, GateKind, TokenUsage};
pub use error::{ErrorSignals, UpstreamErrorClass};

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
