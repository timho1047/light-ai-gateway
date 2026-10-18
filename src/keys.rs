//! Credential operations shared by the terminal command and the console page.
//!
//! A credential value never leaves this module: callers get a row with a source
//! and a status, or a result that says only whether the write succeeded. The
//! stored value is read back once after a write to prove the store kept it, and
//! compared without branching on content.

use crate::config::{Config, SecretRef};
use crate::secrets::{CredentialStore, LOCAL_KEY_ACCOUNT, LOCAL_KEY_PROVIDER};
use anyhow::{Context, Result, bail};

/// One row of the credential report. Never holds the secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialRow {
    pub provider: String,
    pub account: String,
    /// Where the secret comes from, never the secret itself.
    pub source: String,
    pub status: String,
    /// Whether the credential resolved, for colouring without parsing the text.
    pub resolves: bool,
}

/// The gateway's credentials and the rows that describe them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeysReport {
    pub backend: String,
    pub vaulted: bool,
    pub rows: Vec<CredentialRow>,
    /// Character count of the gateway's own access token, when stored. A count
    /// catches a truncated paste and is the only fact about it a report prints.
    pub local_token_chars: Option<usize>,
}

pub fn report(config: &Config, creds: &CredentialStore) -> KeysReport {
    KeysReport {
        backend: creds.backend_name().to_string(),
        vaulted: creds.is_vaulted(),
        rows: rows(config, creds),
        local_token_chars: local_token_status(creds).ok().flatten(),
    }
}

pub fn rows(config: &Config, creds: &CredentialStore) -> Vec<CredentialRow> {
    let mut rows = Vec::new();
    for (provider, p) in &config.providers {
        for (id, a) in &p.accounts {
            let secret_ref = a.key.clone().unwrap_or(SecretRef::Keychain);
            rows.push(credential_row(creds, provider, id, &secret_ref));
        }
    }
    // The gateway's own access token is not a provider credential, but it is a
    // stored secret with a status, and "did I set that up?" is what this table
    // is for. It is listed only when the config says to read it from the store.
    if let Some(secret_ref @ SecretRef::Keychain) = &config.server.local_key {
        rows.push(credential_row(
            creds,
            LOCAL_KEY_PROVIDER,
            LOCAL_KEY_ACCOUNT,
            secret_ref,
        ));
    }
    rows
}

fn credential_row(
    creds: &CredentialStore,
    provider: &str,
    account: &str,
    secret_ref: &SecretRef,
) -> CredentialRow {
    let (status, resolves) = match secret_ref.resolve(creds, provider, account) {
        Ok(_) => ("ok".to_string(), true),
        Err(e) => (format!("MISSING ({e})"), false),
    };
    // Say which item the key came out of. Every key lives in the vault, and the
    // vault is the one item the OS asks about once.
    let mut source = secret_ref.describe();
    if matches!(secret_ref, SecretRef::Keychain) {
        let origin = creds.origin(provider, account);
        if origin != crate::secrets::CredentialOrigin::Absent {
            source = format!("{source} ({})", origin.short());
        }
    }
    CredentialRow {
        provider: provider.to_string(),
        account: account.to_string(),
        source,
        status,
        resolves,
    }
}

/// Character count of the stored gateway access token, if any.
pub fn local_token_status(creds: &CredentialStore) -> Result<Option<usize>> {
    Ok(creds
        .get(LOCAL_KEY_PROVIDER, LOCAL_KEY_ACCOUNT)?
        .map(|key| key.expose().len()))
}

/// Whether a key may be written for this provider/account, and why not.
pub fn add_target(config: &Config, provider: &str, account: &str) -> Result<()> {
    if provider == LOCAL_KEY_PROVIDER && account == LOCAL_KEY_ACCOUNT {
        return Ok(());
    }
    if provider == LOCAL_KEY_PROVIDER {
        bail!(
            "{provider:?} is reserved for the gateway's own access token, whose only \
             account is {LOCAL_KEY_ACCOUNT:?}"
        );
    }
    let p = config.providers.get(provider).with_context(|| {
        let known: Vec<&str> = config.providers.keys().map(String::as_str).collect();
        format!(
            "unknown provider {provider:?}. Configured providers: [{}]",
            known.join(", ")
        )
    })?;
    if !p.accounts.contains_key(account) {
        let known: Vec<&str> = p.accounts.keys().map(String::as_str).collect();
        bail!(
            "unknown account {account:?} under provider {provider:?}. \
             Configured accounts: [{}]",
            known.join(", ")
        );
    }
    Ok(())
}

/// Store a credential and prove the store kept it.
pub fn store(
    config: &Config,
    creds: &CredentialStore,
    provider: &str,
    account: &str,
    secret: &str,
) -> Result<()> {
    add_target(config, provider, account)?;
    creds
        .set(provider, account, secret)
        .with_context(|| format!("storing the credential for {provider}/{account}"))?;
    match creds.get(provider, account)? {
        Some(stored)
            if crate::secrets::constant_time_eq(stored.expose().as_bytes(), secret.as_bytes()) =>
        {
            Ok(())
        }
        _ => bail!(
            "the credential was written but could not be read back from {}; \
             the store may be unavailable",
            creds.backend_name()
        ),
    }
}

/// Remove just the stored credential. False means nothing was stored.
pub fn remove(creds: &CredentialStore, provider: &str, account: &str) -> Result<bool> {
    creds.delete(provider, account)
}

/// What a credential test proved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyTestOutcome {
    /// The gateway's own access token: no provider has heard of it, so its
    /// presence and length are the whole check.
    LocalToken { chars: usize },
    /// A provider key resolved, but this provider exposes no quota endpoint to
    /// prove it against.
    NoProbe { chars: usize },
    /// The provider answered and read this many gates.
    Probed { probe: String, gates: usize },
}

/// Resolve one credential and, when the provider has a quota endpoint, prove it.
/// Network errors are returned as plain messages; callers sanitise them.
pub async fn probe(
    config: &Config,
    creds: &CredentialStore,
    provider: &str,
    account: &str,
    client: &reqwest::Client,
    timeout: std::time::Duration,
) -> Result<KeyTestOutcome> {
    if provider == LOCAL_KEY_PROVIDER && account == LOCAL_KEY_ACCOUNT {
        return match local_token_status(creds)? {
            Some(chars) => Ok(KeyTestOutcome::LocalToken { chars }),
            None => bail!("no local gateway token is stored"),
        };
    }
    let p = config
        .providers
        .get(provider)
        .with_context(|| format!("unknown provider {provider:?}"))?;
    let account_cfg = p
        .accounts
        .get(account)
        .with_context(|| format!("unknown account {account:?} under provider {provider:?}"))?;
    let secret_ref = account_cfg.key.clone().unwrap_or(SecretRef::Keychain);
    let key = secret_ref
        .resolve(creds, provider, account)
        .with_context(|| format!("resolving the credential for {provider}/{account}"))?;
    let chars = key.expose().len();
    let Some(quota_probe) = crate::quota::probe_for(p.adapter, &p.probe_base()) else {
        return Ok(KeyTestOutcome::NoProbe { chars });
    };
    let modelled = crate::domain::Account {
        id: account.to_string(),
        provider: provider.to_string(),
        label: account_cfg
            .label
            .clone()
            .unwrap_or_else(|| account.to_string()),
        base_url: p.base_url.trim_end_matches('/').to_string(),
        api_key: key,
        currency: account_cfg
            .currency
            .clone()
            .unwrap_or_else(|| p.currency.clone()),
        gates: Vec::new(),
        state: crate::domain::AccountState::Unknown,
        available_at: None,
        window_started_at: None,
        credit_multiplier: rust_decimal::Decimal::ONE,
        rpm_limit: None,
        concurrency_limit: None,
        allow_metered: None,
        last_probe_at: None,
        last_error: None,
    };
    match tokio::time::timeout(timeout, quota_probe.probe(&modelled, client)).await {
        Err(_) => bail!(
            "the provider did not answer within {} seconds",
            timeout.as_secs()
        ),
        Ok(Err(error)) => bail!("{}", error.message),
        Ok(Ok(outcome)) => Ok(KeyTestOutcome::Probed {
            probe: quota_probe.name().to_string(),
            gates: outcome.gates.len(),
        }),
    }
}
