//! Configuration: TOML schema, validation, and account construction.
//!
//! FROZEN CONTRACT (Lead-owned).
//!
//! Model routing is deliberately static: a model name maps to exactly one
//! provider. Cross-provider fallback and cross-model downgrade are out of
//! scope by design (see docs/DESIGN.md section 1).

use crate::domain::{Account, AccountState, ApiKey, ProviderId};
use anyhow::{anyhow, bail, Context, Result};
use rust_decimal::Decimal;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Which quota probe implementation a provider uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QuotaAdapter {
    /// GET /v1/usage on api.xkiro.com.
    Xkiro,
    /// GET /alpha/billing/credits on api.commandcode.ai.
    CommandCode,
    /// GET /user/balance on api.deepseek.com.
    Deepseek,
    /// No probe available: local ledger only.
    None,
}

/// Where a secret comes from.
///
/// Deserialized by hand rather than via an untagged enum: this type is used as
/// a plain struct field, and serde cannot combine untagged enums with the map
/// deserializer that fields like this produce.
#[derive(Debug, Clone)]
pub enum SecretRef {
    /// Read from an environment variable at startup.
    Env { env: String },
    /// Read from a file (mode 0600 recommended).
    File { key_file: String },
    /// Read from the operating system credential store, registered with
    /// ai-gw keys add. This is the recommended form.
    Keychain,
    /// Inline literal. Supported but discouraged.
    Literal(String),
}

impl<'de> Deserialize<'de> for SecretRef {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct TableRef {
            #[serde(default)]
            env: Option<String>,
            #[serde(default)]
            key_file: Option<String>,
            /// key = { keychain = true }
            #[serde(default)]
            keychain: Option<bool>,
        }

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Literal(String),
            Table(TableRef),
        }

        match Repr::deserialize(deserializer)? {
            Repr::Literal(s) => Ok(SecretRef::Literal(s)),
            Repr::Table(t) => {
                if t.keychain == Some(true) {
                    Ok(SecretRef::Keychain)
                } else if let Some(env) = t.env {
                    Ok(SecretRef::Env { env })
                } else if let Some(key_file) = t.key_file {
                    Ok(SecretRef::File { key_file })
                } else {
                    Err(serde::de::Error::custom(
                        "secret must set env, key_file, keychain, or be a literal string",
                    ))
                }
            }
        }
    }
}

impl SecretRef {
    /// Where this reference reads from, for display. Never includes the secret.
    pub fn describe(&self) -> String {
        match self {
            SecretRef::Env { env } => format!("env:{env}"),
            SecretRef::File { key_file } => format!("file:{key_file}"),
            SecretRef::Keychain => "credential-store".to_string(),
            SecretRef::Literal(_) => "inline-literal".to_string(),
        }
    }

    /// Resolve this reference to an actual key.
    ///
    /// provider and account are required for every variant, not just the
    /// credential store, so a misconfiguration surfaces as one clear message.
    pub fn resolve(
        &self,
        store: &crate::secrets::CredentialStore,
        provider: &str,
        account: &str,
    ) -> Result<ApiKey> {
        if let SecretRef::Keychain = self {
            return match store.get(provider, account)? {
                Some(key) => Ok(key),
                None => bail!(
                    "{}",
                    crate::secrets::missing_credential_message(provider, account)
                ),
            };
        }
        match self {
            SecretRef::Keychain => unreachable!("handled above"),
            SecretRef::Env { env } => {
                let v = std::env::var(env).map_err(|_| {
                    anyhow!("environment variable {env} is not set (required by config)")
                })?;
                if v.trim().is_empty() {
                    bail!("environment variable {env} is empty");
                }
                Ok(ApiKey::new(v.trim().to_string()))
            }
            SecretRef::File { key_file } => {
                let raw = std::fs::read_to_string(key_file)
                    .with_context(|| format!("reading key_file {key_file}"))?;
                let v = raw.trim();
                if v.is_empty() {
                    bail!("key_file {key_file} is empty");
                }
                Ok(ApiKey::new(v.to_string()))
            }
            SecretRef::Literal(s) => {
                if s.trim().is_empty() {
                    bail!("inline api key is empty");
                }
                Ok(ApiKey::new(s.trim().to_string()))
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    /// Defaults to loopback: this proxy holds paid subscription credentials.
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Bearer token clients must present. Strongly recommended.
    #[serde(default)]
    pub local_key: Option<SecretRef>,
    #[serde(default = "default_db_path")]
    pub db_path: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            listen: default_listen(),
            local_key: None,
            db_path: default_db_path(),
        }
    }
}

fn default_listen() -> String {
    "127.0.0.1:8787".to_string()
}

fn default_db_path() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    format!("{home}/.light-ai-gateway/gateway.db")
}

#[derive(Debug, Clone, Deserialize)]
pub struct PolicyConfig {
    /// Soft threshold. Crossed accounts stop taking NEW sessions but keep
    /// serving sticky ones. This is a routing PREFERENCE, not a hard stop --
    /// keeping it at 0.80 preserves the safety reserve the hard gate needs.
    #[serde(default = "default_soft_threshold")]
    pub soft_threshold: f64,
    /// When false (default), requests are refused rather than allowed to
    /// silently spend wallet or purchased credits. Command Code overflows into
    /// purchased credits without returning an error, so this is the only
    /// guard against accidental spend.
    #[serde(default)]
    pub allow_metered: bool,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    #[serde(default = "default_poll_active")]
    pub poll_interval_active_secs: u64,
    #[serde(default = "default_poll_idle")]
    pub poll_interval_idle_secs: u64,
    /// Relative drift between the local ledger and a probe before the probe
    /// value replaces the ledger.
    #[serde(default = "default_drift")]
    pub drift_threshold: f64,
    /// Extra fixed safety margin, in the gate's own unit, added to the
    /// in-flight p95 when evaluating the hard gate.
    #[serde(default)]
    pub hard_margin: f64,
    /// Maximum attempts across accounts for one client request.
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_secs: u64,
    #[serde(default = "default_ttft_timeout")]
    pub ttft_timeout_secs: u64,
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_secs: u64,
    #[serde(default = "default_max_body")]
    pub max_body_bytes: usize,
    /// Force a probe of the target account immediately before switching to it.
    #[serde(default = "default_true")]
    pub probe_before_switch: bool,
}

fn default_soft_threshold() -> f64 {
    0.80
}
fn default_poll_interval() -> u64 {
    1200
}
fn default_poll_active() -> u64 {
    300
}
fn default_poll_idle() -> u64 {
    3600
}
fn default_drift() -> f64 {
    0.05
}
fn default_max_retries() -> u32 {
    2
}
fn default_connect_timeout() -> u64 {
    10
}
fn default_ttft_timeout() -> u64 {
    120
}
fn default_idle_timeout() -> u64 {
    120
}
fn default_max_body() -> usize {
    32 * 1024 * 1024
}
fn default_true() -> bool {
    true
}

impl Default for PolicyConfig {
    fn default() -> Self {
        PolicyConfig {
            soft_threshold: default_soft_threshold(),
            allow_metered: false,
            poll_interval_secs: default_poll_interval(),
            poll_interval_active_secs: default_poll_active(),
            poll_interval_idle_secs: default_poll_idle(),
            drift_threshold: default_drift(),
            hard_margin: 0.0,
            max_retries: default_max_retries(),
            connect_timeout_secs: default_connect_timeout(),
            ttft_timeout_secs: default_ttft_timeout(),
            idle_timeout_secs: default_idle_timeout(),
            max_body_bytes: default_max_body(),
            probe_before_switch: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AccountConfig {
    /// Where this account's credential comes from.
    ///
    /// Omitting it means "use the OS credential store", which is the form the
    /// ai-gw keys add command registers. Requiring a key here would force the
    /// operator to prepare secrets before every start, which is the friction
    /// this whole mechanism exists to remove.
    #[serde(default)]
    pub key: Option<SecretRef>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub currency: Option<String>,
    #[serde(default)]
    pub credit_multiplier: Option<f64>,
    #[serde(default)]
    pub concurrency_limit: Option<u32>,
    #[serde(default)]
    pub rpm_limit: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    pub adapter: QuotaAdapter,
    pub base_url: String,
    /// Host used for quota probes when it differs from base_url. Command Code
    /// is exactly this case: traffic goes to /provider/v1 while probes live on
    /// the bare api.commandcode.ai host. The probe host is pinned deliberately
    /// so credentials never follow a user-supplied base_url elsewhere.
    #[serde(default)]
    pub probe_base_url: Option<String>,
    #[serde(default = "default_currency")]
    pub currency: String,
    #[serde(default)]
    pub credit_multiplier: Option<f64>,
    #[serde(default)]
    pub accounts: BTreeMap<String, AccountConfig>,
}

fn default_currency() -> String {
    "USD".to_string()
}

/// Optional per-model pricing. Models absent from this table are priced at a
/// deliberately conservative default, because under-estimating cost makes the
/// hard gate too permissive and spends real money.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelPriceConfig {
    pub prompt_per_million: f64,
    pub completion_per_million: f64,
    #[serde(default = "default_cached_discount")]
    pub cached_prompt_discount: f64,
}

fn default_cached_discount() -> f64 {
    0.1
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub policy: PolicyConfig,
    #[serde(default)]
    pub providers: BTreeMap<ProviderId, ProviderConfig>,
    /// model name -> provider name. Exactly one provider per model.
    #[serde(default)]
    pub routes: BTreeMap<String, String>,
    /// Optional escape hatch for model-name collisions between providers:
    /// aliases maps a leading path segment to a provider, so a client can ask
    /// for "xkiro/deepseek/deepseek-v3.2". Disabled when empty.
    #[serde(default)]
    pub provider_prefix_aliases: BTreeMap<String, String>,
    /// model name -> price, in USD per million tokens.
    #[serde(default)]
    pub prices: BTreeMap<String, ModelPriceConfig>,
}

/// A resolved routing decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRoute {
    pub provider: ProviderId,
    /// Model name to send upstream.
    pub model: String,
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Config> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let cfg: Config = toml::from_str(&raw)
            .with_context(|| format!("parsing config {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn from_toml_str(raw: &str) -> Result<Config> {
        let cfg: Config = toml::from_str(raw).context("parsing config")?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        if !(0.0..=1.0).contains(&self.policy.soft_threshold) {
            bail!(
                "policy.soft_threshold must be within 0.0..=1.0, got {}",
                self.policy.soft_threshold
            );
        }
        if self.policy.soft_threshold >= 1.0 {
            bail!(
                "policy.soft_threshold must stay below 1.0: the gap between the soft \
                 threshold and the hard gate is the reserve that stops requests \
                 overflowing into metered spend"
            );
        }
        if self.policy.max_retries > 8 {
            bail!("policy.max_retries must be <= 8, got {}", self.policy.max_retries);
        }

        if self.providers.is_empty() {
            bail!("no providers configured");
        }

        for (name, p) in &self.providers {
            if p.base_url.trim().is_empty() {
                bail!("provider {name}: base_url must not be empty");
            }
            // Credential-store entries are keyed by "provider/account". A slash
            // inside either half would let two different accounts resolve to the
            // same stored secret, so it is rejected at the boundary rather than
            // discovered later as a mysteriously shared key.
            if name.contains('/') {
                bail!("provider name {name:?} must not contain a slash");
            }
            if p.accounts.is_empty() {
                bail!("provider {name}: no accounts configured");
            }
            for (acct, a) in &p.accounts {
                let _ = a; // secret resolution happens in build_accounts
                if acct.trim().is_empty() {
                    bail!("provider {name}: account id must not be empty");
                }
                if acct.contains('/') {
                    bail!(
                        "provider {name}: account id {acct:?} must not contain a slash,                          because provider/account is the credential-store key"
                    );
                }
            }
        }

        for (model, provider) in &self.routes {
            if !self.providers.contains_key(provider) {
                bail!("route {model} points at unknown provider {provider}");
            }
        }

        for (alias, provider) in &self.provider_prefix_aliases {
            if !self.providers.contains_key(provider) {
                bail!("provider_prefix_aliases {alias} points at unknown provider {provider}");
            }
        }

        // Two providers claiming the same base model name is the collision this
        // design deliberately avoids; the prefix alias is the escape hatch.
        Ok(())
    }

    /// Resolve a client-supplied model name to a provider and upstream model.
    pub fn resolve(&self, client_model: &str) -> Result<ResolvedRoute> {
        if let Some(provider) = self.routes.get(client_model) {
            return Ok(ResolvedRoute {
                provider: provider.clone(),
                model: client_model.to_string(),
            });
        }

        // Optional provider-prefix escape hatch for name collisions.
        if !self.provider_prefix_aliases.is_empty()
            && let Some((head, rest)) = client_model.split_once('/')
                && let Some(provider) = self.provider_prefix_aliases.get(head)
                    && !rest.is_empty() {
                        return Ok(ResolvedRoute {
                            provider: provider.clone(),
                            model: rest.to_string(),
                        });
                    }

        let mut known: Vec<&str> = self.routes.keys().map(String::as_str).collect();
        known.sort_unstable();
        bail!(
            "no route for model {client_model:?}. Configured models: [{}]. \
             Add a [routes] entry mapping this model to a provider.",
            known.join(", ")
        )
    }

    /// Build the initial account pool from configuration.
    ///
    /// Every account starts in AccountState::Unknown with no gates. It must be
    /// probed before it serves traffic: restoring from disk and assuming zero
    /// spend is exactly how a gateway silently spends real money.
    pub fn build_accounts(
        &self,
        store: &crate::secrets::CredentialStore,
    ) -> Result<Vec<Account>> {
        let mut out = Vec::new();
        for (provider_name, p) in &self.providers {
            for (account_id, a) in &p.accounts {
                // No key configured means the credential store, looked up by
                // provider/account.
                let secret_ref = a.key.clone().unwrap_or(SecretRef::Keychain);
                let key = secret_ref
                    .resolve(store, provider_name, account_id)
                    .with_context(|| format!("provider {provider_name} account {account_id}"))?;
                let currency = a
                    .currency
                    .clone()
                    .unwrap_or_else(|| p.currency.clone());
                let multiplier = a
                    .credit_multiplier
                    .or(p.credit_multiplier)
                    .map(|f| Decimal::try_from(f).unwrap_or(Decimal::ONE))
                    .unwrap_or(Decimal::ONE);
                out.push(Account {
                    id: account_id.clone(),
                    provider: provider_name.clone(),
                    label: a.label.clone().unwrap_or_else(|| account_id.clone()),
                    base_url: p.base_url.trim_end_matches('/').to_string(),
                    api_key: key,
                    currency,
                    gates: Vec::new(),
                    state: AccountState::Unknown,
                    available_at: None,
                    window_started_at: None,
                    credit_multiplier: multiplier,
                    rpm_limit: a.rpm_limit,
                    concurrency_limit: a.concurrency_limit,
                    last_probe_at: None,
                    last_error: None,
                });
            }
        }
        if out.is_empty() {
            bail!("configuration produced no accounts");
        }
        Ok(out)
    }

    /// Build the account list WITHOUT resolving credentials.
    ///
    /// Only for operations that need identity and state and never make an
    /// upstream call: listing accounts and disabling one. The key is left empty,
    /// and any attempt to use it against a provider will fail authentication
    /// rather than silently borrow another account's identity. This exists so an
    /// account whose key is missing can still be listed and disabled.
    pub fn build_accounts_unresolved(&self) -> Result<Vec<Account>> {
        let mut out = Vec::new();
        for (provider_name, p) in &self.providers {
            for (account_id, a) in &p.accounts {
                out.push(Account {
                    id: account_id.clone(),
                    provider: provider_name.clone(),
                    label: a.label.clone().unwrap_or_else(|| account_id.clone()),
                    base_url: p.base_url.trim_end_matches('/').to_string(),
                    api_key: ApiKey::new(""),
                    currency: a.currency.clone().unwrap_or_else(|| p.currency.clone()),
                    gates: Vec::new(),
                    state: AccountState::Unknown,
                    available_at: None,
                    window_started_at: None,
                    credit_multiplier: a
                        .credit_multiplier
                        .or(p.credit_multiplier)
                        .map(|f| Decimal::try_from(f).unwrap_or(Decimal::ONE))
                        .unwrap_or(Decimal::ONE),
                    rpm_limit: a.rpm_limit,
                    concurrency_limit: a.concurrency_limit,
                    last_probe_at: None,
                    last_error: None,
                });
            }
        }
        Ok(out)
    }

    /// Providers that need a probe adapter, as (provider, adapter, probe url).
    pub fn probe_targets(&self) -> Vec<(ProviderId, QuotaAdapter, String)> {
        self.providers
            .iter()
            .map(|(name, p)| {
                let base = p
                    .probe_base_url
                    .clone()
                    .unwrap_or_else(|| p.base_url.clone());
                (name.clone(), p.adapter, base.trim_end_matches('/').to_string())
            })
            .collect()
    }

    pub fn db_path(&self) -> PathBuf {
        PathBuf::from(&self.server.db_path)
    }

    /// Soft threshold as Decimal for exact comparisons.
    pub fn soft_threshold_decimal(&self) -> Decimal {
        Decimal::try_from(self.policy.soft_threshold).unwrap_or(Decimal::new(80, 2))
    }
}

impl FromStr for Config {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Config::from_toml_str(s)
    }
}

const SAMPLE: &str = r#"
[server]
listen = "127.0.0.1:8787"
db_path = "~/.light-ai-gateway/gateway.db"
# Strongly recommended: without it any local process can spend your quota.
# local_key = { env = "AI_GW_LOCAL_KEY" }

[policy]
soft_threshold = 0.80
allow_metered = false
poll_interval_secs = 1200
poll_interval_active_secs = 300
poll_interval_idle_secs = 3600

[providers.xkiro]
adapter = "xkiro"
base_url = "https://api.xkiro.com/v1"
currency = "USD"

# Accounts need no key here: omitting it means "read from the OS credential
# store", registered once with:
#     ai-gw keys add xkiro acc-1
# Explicit alternatives, if you prefer them:
#     key = { env = "XKIRO_KEY_1" }
#     key = { key_file = "/run/secrets/xkiro-1" }
#     key = "sk-literal-not-recommended"

[providers.xkiro.accounts.acc-1]

[providers.xkiro.accounts.acc-2]

[providers.commandcode]
adapter = "command-code"
base_url = "https://api.commandcode.ai/provider/v1"
probe_base_url = "https://api.commandcode.ai"
currency = "USD"

[providers.commandcode.accounts.acc-1]

[providers.deepseek]
adapter = "deepseek"
base_url = "https://api.deepseek.com/v1"
probe_base_url = "https://api.deepseek.com"
currency = "USD"

[providers.deepseek.accounts.main]

[routes]
"deepseek/deepseek-v3.2" = "xkiro"
"openai/gpt-5.6-sol" = "xkiro"
"claude/opus-4.8" = "commandcode"
"deepseek/deepseek-chat" = "deepseek"
"#;

/// A documented starter configuration.
pub fn sample_toml() -> &'static str {
    SAMPLE
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_two_providers() -> Config {
        Config::from_toml_str(
            r#"
[providers.xkiro]
adapter = "xkiro"
base_url = "https://api.xkiro.com/v1"
[providers.xkiro.accounts.a1]
key = "sk-test-1"
[providers.deepseek]
adapter = "deepseek"
base_url = "https://api.deepseek.com/v1"
[providers.deepseek.accounts.d1]
key = { keychain = true }
[routes]
"deepseek/deepseek-v3.2" = "xkiro"
"deepseek/deepseek-chat" = "deepseek"
"#,
        )
        .unwrap()
    }

    #[test]
    fn shipped_example_config_parses() {
        // The example is documentation people copy; keep it honest.
        let raw = include_str!("../config.example.toml");
        let cfg = Config::from_toml_str(raw).expect("config.example.toml must stay valid");
        assert_eq!(cfg.providers.len(), 3);
        assert_eq!(
            cfg.resolve("deepseek/deepseek-v3.2").unwrap().provider,
            "xkiro"
        );
        assert!(!cfg.policy.allow_metered, "default must not spend metered credit");
    }

    #[test]
    fn sample_config_is_valid() {
        let cfg = Config::from_toml_str(sample_toml()).unwrap();
        assert!(cfg.providers.len() >= 3);
    }

    #[test]
    fn resolve_is_exact_and_does_not_guess() {
        let cfg = cfg_with_two_providers();
        assert_eq!(
            cfg.resolve("deepseek/deepseek-v3.2").unwrap().provider,
            "xkiro"
        );
        assert_eq!(
            cfg.resolve("deepseek/deepseek-chat").unwrap().provider,
            "deepseek"
        );
        // A model with no route must fail loudly rather than default anywhere.
        assert!(cfg.resolve("deepseek/deepseek-v3.3").is_err());
    }

    #[test]
    fn prefix_alias_resolves_and_strips() {
        let mut cfg = cfg_with_two_providers();
        cfg.provider_prefix_aliases
            .insert("xkiro".into(), "xkiro".into());
        cfg.validate().unwrap();
        let r = cfg.resolve("xkiro/deepseek/deepseek-v3.2").unwrap();
        assert_eq!(r.provider, "xkiro");
        assert_eq!(r.model, "deepseek/deepseek-v3.2");
    }

    #[test]
    fn accounts_start_unknown_and_unprobed() {
        let cfg = cfg_with_two_providers();
        let store = crate::secrets::CredentialStore::memory();
        // The keychain-backed account takes its key from the credential store;
        // the other uses an inline literal, so nothing here touches a real store.
        store.set("deepseek", "d1", "sk-from-store").unwrap();
        let accts = cfg.build_accounts(&store).unwrap();
        assert_eq!(accts.len(), 2);
        for a in &accts {
            assert_eq!(a.state, AccountState::Unknown);
            assert!(a.gates.is_empty(), "must not assume zero spend before probing");
        }
        let d1 = accts.iter().find(|a| a.id == "d1").unwrap();
        assert_eq!(d1.api_key.expose(), "sk-from-store");
    }

    #[test]
    fn an_account_with_no_key_is_read_from_the_credential_store() {
        // Omitting key entirely is the recommended form; it must resolve from the
        // store without any environment variable being prepared.
        let cfg = Config::from_toml_str(
            r#"
[providers.xkiro]
adapter = "xkiro"
base_url = "https://api.xkiro.com/v1"
[providers.xkiro.accounts.acc-1]
[routes]
"m" = "xkiro"
"#,
        )
        .unwrap();
        let store = crate::secrets::CredentialStore::memory();
        store.set("xkiro", "acc-1", "sk-stored").unwrap();
        let accts = cfg.build_accounts(&store).unwrap();
        assert_eq!(accts[0].api_key.expose(), "sk-stored");
    }

    #[test]
    fn a_missing_stored_credential_names_the_command_that_fixes_it() {
        let cfg = cfg_with_two_providers();
        let store = crate::secrets::CredentialStore::memory();
        let err = format!("{:#}", cfg.build_accounts(&store).unwrap_err());
        assert!(
            err.contains("ai-gw keys add deepseek d1"),
            "the error must tell the operator what to run; got: {err}"
        );
    }

    #[test]
    fn slashes_are_rejected_because_they_would_alias_credentials() {
        let mut cfg = cfg_with_two_providers();
        cfg.providers
            .get_mut("deepseek")
            .unwrap()
            .accounts
            .insert("bad/id".into(), AccountConfig {
                key: None,
                label: None,
                currency: None,
                credit_multiplier: None,
                concurrency_limit: None,
                rpm_limit: None,
            });
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("slash"), "got: {err}");
    }

    #[test]
    fn soft_threshold_of_one_is_rejected() {
        let mut cfg = cfg_with_two_providers();
        cfg.policy.soft_threshold = 1.0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn route_to_unknown_provider_is_rejected() {
        let mut cfg = cfg_with_two_providers();
        cfg.routes.insert("m".into(), "nope".into());
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn debug_never_prints_the_key() {
        let k = ApiKey::new("sk-super-secret");
        assert!(!format!("{k:?}").contains("super"));
        assert!(!format!("{k}").contains("super"));
    }
}
