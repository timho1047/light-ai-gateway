//! The operator console: server-rendered HTML, one stylesheet, two themes,
//! two languages.
//!
//! Pages are pure functions from gateway data to HTML; the HTTP layer owns the
//! routes, the loopback guard and the apply endpoints. Keeping rendering free of
//! I/O is what lets the page tests run without a database or a provider.

pub mod assets;
pub mod board;
pub mod config_page;
pub mod copy;
pub mod doctor;
pub mod guard;
pub mod layout;
pub mod prices;
pub mod requests;
pub mod usage;
pub mod violations;
