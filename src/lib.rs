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
pub mod sse;
pub mod store;

pub use config::Config;
pub use domain::{Account, Gate, GateClass, GateKind, TokenUsage};
pub use error::{ErrorSignals, UpstreamErrorClass};
