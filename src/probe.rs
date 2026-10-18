//! The one HTTP client policy for operations that attach a provider bearer
//! token: quota probes, key tests, doctor and catalog checks.
//!
//! Redirects are disabled: the token is attached before the request leaves, and
//! a 3xx must never be allowed to carry it to whatever host a Location header
//! names. A total timeout bounds a provider that accepts the connection and then
//! stalls; the connect timeout comes from policy.

use crate::config::Config;
use anyhow::{Context, Result};

pub fn client(config: &Config) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(
            config.policy.connect_timeout_secs.max(1),
        ))
        .timeout(std::time::Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("building the probe HTTP client")
}
