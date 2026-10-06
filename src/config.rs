//! Configuration: TOML schema, validation, and account construction.
//!
//! FROZEN CONTRACT: the CLI, the router and the proxy are written against this shape.
//!
//! Model routing is deliberately static: a model name maps to exactly one
//! provider. Cross-provider fallback and cross-model downgrade are out of
//! scope by design.

use crate::domain::{Account, AccountState, ApiKey, ProviderId};
use anyhow::{Context, Result, anyhow, bail};
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
    /// GET /v1/user/balance on api.hao.ai.
    Haoai,
    /// No probe available.
    ///
    /// Warned about in validate() and counted as a problem by doctor: an
    /// account whose provider has no quota source never leaves
    /// AccountState::Unknown, and only a successful probe can clear that, so it
    /// can never be selected. Accepting the setting silently produced a
    /// permanently idle account and a doctor line describing it as fine.
    None,
}

impl QuotaAdapter {
    /// Whether every gate this provider can report is a wallet.
    ///
    /// A wallet-only provider has nothing to admit an account with while
    /// real-money spending is disabled for it, so none of its accounts can ever
    /// be selected. validate() warns about that combination rather than leaving
    /// the operator to work out why a configured provider serves nothing.
    ///
    /// Deliberately no wildcard arm. This is a restatement of what each adapter
    /// emits, so it is the one place that can disagree with them; a new variant
    /// that inherited "false" would silently drop the warning that explains why
    /// its accounts never serve.
    pub fn wallet_only(self) -> bool {
        match self {
            QuotaAdapter::None | QuotaAdapter::Xkiro | QuotaAdapter::CommandCode => false,
            QuotaAdapter::Deepseek | QuotaAdapter::Haoai => true,
        }
    }
}

/// Where a secret comes from.
///
/// Deserialized by hand rather than via an untagged enum: this type is used as
/// a plain struct field, and serde cannot combine untagged enums with the map
/// deserializer that fields like this produce.
#[derive(Clone)]
pub enum SecretRef {
    /// Read from an environment variable at startup.
    Env { env: String },
    /// Read from a file (mode 0600 recommended).
    File { key_file: String },
    /// Read from the operating system credential store, registered with
    /// lgw keys add. This is the recommended form.
    Keychain,
    /// Inline literal. Supported but discouraged.
    Literal(String),
}

/// Never print the secret.
///
/// The derived Debug printed Literal in full, so any future log line, error
/// context or panic message that happened to carry a SecretRef would have
/// written an API key into the terminal and into whatever collected the
/// output. ApiKey already had this guarantee; SecretRef did not, and it is the
/// one that can hold the key before it becomes an ApiKey.
impl std::fmt::Debug for SecretRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SecretRef::Env { env } => write!(f, "Env({env})"),
            SecretRef::File { key_file } => write!(f, "File({key_file})"),
            SecretRef::Keychain => f.write_str("Keychain"),
            SecretRef::Literal(_) => f.write_str("Literal(***)"),
        }
    }
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
                let resolved = expand_tilde(key_file);
                let raw = std::fs::read_to_string(&resolved)
                    .with_context(|| format!("reading key_file {}", resolved.display()))?;
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
    /// lgw keys add command registers. Requiring a key here would force the
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
    /// Overrides `[policy] allow_metered` for this account only.
    #[serde(default)]
    pub allow_metered: Option<bool>,
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
    /// Whether this provider's accounts may spend real money, when the answer
    /// differs from `[policy] allow_metered`.
    ///
    /// The policy flag is all-or-nothing, and turning it on to admit one
    /// wallet-only provider also unlocks the overflow path for every other
    /// provider -- Command Code's purchased credits among them. This is the
    /// narrower switch: omitted follows the policy, set decides for this
    /// provider alone.
    #[serde(default)]
    pub allow_metered: Option<bool>,
    #[serde(default)]
    pub accounts: BTreeMap<String, AccountConfig>,
}

fn default_currency() -> String {
    "USD".to_string()
}

impl ProviderConfig {
    /// The bare host every quota adapter appends its documented path to.
    ///
    /// The single source of truth: a second copy of this normalisation drifts,
    /// and the version-suffix rule is what such a copy gets wrong -- it turns
    /// https://api.xkiro.com/v1 into /v1/v1/usage.
    pub fn probe_base(&self) -> String {
        match &self.probe_base_url {
            Some(explicit) => explicit.trim_end_matches('/').to_string(),
            None => strip_api_version_suffix(self.base_url.trim_end_matches('/')),
        }
    }
}

/// Optional per-model pricing, in USD per MILLION tokens.
///
/// The names are the ones every provider's pricing page uses. Models absent from
/// this table are priced at a deliberately conservative default, because
/// under-estimating cost makes the hard gate too permissive and spends real
/// money.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelPriceConfig {
    /// USD per million input tokens.
    pub input: f64,
    /// USD per million output tokens.
    pub output: f64,
    /// USD per million cache-read input tokens.
    ///
    /// A price, not a ratio, so it reads the same way as input and output.
    /// Providers discount cache reads heavily (commonly to a tenth of input), so
    /// omitting this defaults to input / 10 rather than to zero. Zero would make
    /// a cache read look free, and under-estimating is the direction that spends.
    #[serde(rename = "cache read", alias = "cache_read", default)]
    pub cache_read: Option<f64>,
    /// Time windows charged at a different rate.
    ///
    /// The prices above are the DEFAULT tier, charged whenever no window here
    /// matches. Command Code, for example, bills deepseek-v4.1-flash at double
    /// the rate Mon-Fri 01:00-04:00 and 06:00-10:00 UTC:
    ///
    /// ```toml
    /// [prices.commandcode."deepseek/deepseek-v4.1-flash"]
    /// input = 0.15              # off-peak, the default tier
    /// output = 0.60
    /// "cache read" = 0.003
    ///
    /// [[prices.commandcode."deepseek/deepseek-v4.1-flash".schedules]]
    /// name  = "peak"
    /// days  = "mon-fri"
    /// hours = "01:00-04:00, 06:00-10:00"
    /// tz    = "UTC"             # or a fixed offset like "+08:00"
    /// input = 0.30
    /// output = 1.20
    /// "cache read" = 0.006
    /// ```
    ///
    /// Without this, a request during a peak window is priced at the default
    /// rate, which is wrong by the peak multiplier -- toward spending when the
    /// peak rate is higher.
    #[serde(default)]
    pub schedules: Vec<ScheduleConfig>,
}

/// One time window charged at its own rate.
#[derive(Debug, Clone, Deserialize)]
pub struct ScheduleConfig {
    /// Label used in reports. Defaults to "schedule N".
    #[serde(default)]
    pub name: Option<String>,
    /// "mon-fri", "sat,sun", "mon,wed,fri" or "*".
    pub days: String,
    /// "01:00-04:00, 06:00-10:00". Each range is half-open on the end.
    pub hours: String,
    /// "UTC" (the default) or a fixed offset such as "+08:00".
    #[serde(default)]
    pub tz: Option<String>,
    pub input: f64,
    pub output: f64,
    #[serde(rename = "cache read", alias = "cache_read", default)]
    pub cache_read: Option<f64>,
}

/// One tier's prices in USD per million tokens, with the cache-read price
/// already resolved so callers never re-derive it.
#[derive(Debug, Clone, PartialEq)]
pub struct TierPrices {
    pub name: String,
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
}

impl ModelPriceConfig {
    /// The default tier first, then each schedule in the order they are tested.
    /// Index 0 is always the default tier.
    pub fn tiers(&self) -> Vec<TierPrices> {
        let cache_read =
            |input: f64, given: Option<f64>| given.unwrap_or(input * default_cache_read_ratio());
        let mut out = vec![TierPrices {
            name: "default".to_string(),
            input: self.input,
            output: self.output,
            cache_read: cache_read(self.input, self.cache_read),
        }];
        for (i, s) in self.schedules.iter().enumerate() {
            out.push(TierPrices {
                name: s
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("schedule {}", i + 1)),
                input: s.input,
                output: s.output,
                cache_read: cache_read(s.input, s.cache_read),
            });
        }
        out
    }

    /// The parsed time windows. An error names the provider and model, because
    /// a schedule that fails to parse would otherwise silently price every
    /// request at the default rate.
    pub fn schedule(&self, provider: &str, model: &str) -> Result<crate::tier::Schedule> {
        let mut tiers = Vec::new();
        for (i, s) in self.schedules.iter().enumerate() {
            let label = s
                .name
                .clone()
                .unwrap_or_else(|| format!("schedule {}", i + 1));
            let ctx = format!("[prices.{provider}.{model}] {label}");
            tiers.push(crate::tier::Tier {
                name: label.clone(),
                days: crate::tier::parse_days(&s.days).with_context(|| format!("{ctx}: days"))?,
                hours: crate::tier::parse_hours(&s.hours)
                    .with_context(|| format!("{ctx}: hours"))?,
                offset_minutes: crate::tier::parse_tz(s.tz.as_deref().unwrap_or("UTC"))
                    .with_context(|| format!("{ctx}: tz"))?,
            });
        }
        Ok(crate::tier::Schedule { tiers })
    }
}

/// The share of the input price a cache read costs when none is given. Matches
/// the ratio every major provider uses.
pub fn default_cache_read_ratio() -> f64 {
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
    /// provider -> model -> price, in USD per million tokens.
    ///
    /// Scoped by provider because the same model name can be billed differently
    /// by each: one provider sells tokens at list price while another meters
    /// them against a plan's credit allowance.
    ///
    /// ```toml
    /// [prices.xkiro."z-ai/glm-5.3-flash"]
    /// input = 0.28
    /// output = 0.42
    /// "cache read" = 0.028
    /// ```
    #[serde(default)]
    pub prices: BTreeMap<ProviderId, BTreeMap<String, ModelPriceConfig>>,
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
        let cfg: Config =
            toml::from_str(&raw).with_context(|| format!("parsing config {}", path.display()))?;
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
            bail!(
                "policy.max_retries must be <= 8, got {}",
                self.policy.max_retries
            );
        }
        // Every unknown in this file is resolved in the direction that cannot
        // spend money. A negative or non-finite margin silently shrinks the
        // reserve that keeps a burst out of paid credit, and the gate turns an
        // unconvertible value into zero -- removing, without a word, exactly the
        // protection the operator thought they had configured.
        if !self.policy.hard_margin.is_finite() || self.policy.hard_margin < 0.0 {
            bail!(
                "policy.hard_margin must be a finite value >= 0.0, got {}",
                self.policy.hard_margin
            );
        }
        if !self.policy.drift_threshold.is_finite() || self.policy.drift_threshold < 0.0 {
            bail!(
                "policy.drift_threshold must be a finite value >= 0.0, got {}",
                self.policy.drift_threshold
            );
        }

        // A loopback bind with no local key is a deliberate local-trust
        // decision; anything else is a door. The key is what stands between the
        // network and the pooled subscription quota, and a warning in the log is
        // not a control. The unreadable-key case already fails closed when the
        // router is built; this is the other half of the same rule.
        if self.server.local_key.is_none()
            && let Ok(addr) = self.server.listen.parse::<std::net::SocketAddr>()
            && !addr.ip().is_loopback()
        {
            bail!(
                "server.listen {} is not loopback and [server] local_key is unset, so anything \
                 able to reach that address could spend the pooled quota. Set local_key (lgw \
                 keys add local gateway) or bind 127.0.0.1",
                self.server.listen
            );
        }

        if self.providers.is_empty() {
            bail!("no providers configured");
        }

        for (provider, models) in &self.prices {
            for (model, p) in models {
                // A window's NAME is the key its learned rate is stored under, and
                // the empty key is the default price: an unnamed window would
                // silently take the default price's row, and two windows sharing a
                // name would share one learned rate between them.
                let mut seen: Vec<String> = Vec::new();
                for (i, s) in p.schedules.iter().enumerate() {
                    if s.name.as_deref().is_some_and(|n| n.trim().is_empty()) {
                        bail!(
                            "[prices.{provider}.{model}] a tier name must not be empty: it is \
                             the key its learned price is stored under"
                        );
                    }
                    let name = s
                        .name
                        .clone()
                        .unwrap_or_else(|| format!("schedule {}", i + 1));
                    if seen.contains(&name) {
                        bail!(
                            "[prices.{provider}.{model}] two price tiers are both named {name:?}: \
                             the name is the key a learned price is stored under"
                        );
                    }
                    seen.push(name);
                }
            }
        }

        for (name, p) in &self.providers {
            if p.base_url.trim().is_empty() {
                bail!("provider {name}: base_url must not be empty");
            }
            // An account only leaves AccountState::Unknown after a successful
            // probe, so a provider with no quota source can never be selected.
            // This used to pass validation silently and doctor described the
            // account as "local ledger only", which reads like a supported mode.
            // It is not reported as a hard error only because the same setting
            // is a convenient fixture in tests that drive the pool directly.
            if matches!(p.adapter, QuotaAdapter::None) {
                tracing::warn!(
                    provider = %name,
                    "adapter = \"none\" cannot serve traffic: an account is admitted to the \
                     pool only after a successful quota probe, and with no probe source \
                     there is nothing to admit it. This account will stay UNKNOWN forever. \
                     Use one of: xkiro, command-code, deepseek, haoai, or remove the provider."
                );
            }
            // The mirror of the case above: every gate a wallet-only provider
            // can report is metered, so with real-money spending off for it
            // there is nothing to admit an account with and none of its
            // accounts can ever be selected. Say so, rather than leave a
            // configured provider sitting idle with no explanation.
            if p.adapter.wallet_only() && !p.allow_metered.unwrap_or(self.policy.allow_metered) {
                tracing::warn!(
                    provider = %name,
                    "provider {name} has no covered allowance, only a wallet, and spending real \
                     money is off for it, so no account here can ever be selected. Set \
                     [providers.{name}] allow_metered = true to spend THIS provider's wallet \
                     without unlocking the others."
                );
            }
            // Credential-store entries are keyed by "provider/account". A slash
            // inside either half would let two different accounts resolve to the
            // same stored secret, so it is rejected at the boundary rather than
            // discovered later as a mysteriously shared key.
            if name.contains('/') {
                bail!("provider name {name:?} must not contain a slash");
            }
            // A malformed URL otherwise survives every check here and only
            // fails at request time, on the traffic path, for every client --
            // while doctor, which probes through probe_base_url or a different
            // string, can still report everything fine. The price table and the
            // probe path are both validated at startup; this one was not.
            for (field, url) in [
                ("base_url", Some(p.base_url.as_str())),
                ("probe_base_url", p.probe_base_url.as_deref()),
            ] {
                let Some(url) = url else { continue };
                let parsed = reqwest::Url::parse(url).map_err(|e| {
                    anyhow!("provider {name}: {field} {url:?} is not an absolute URL: {e}")
                })?;
                if !matches!(parsed.scheme(), "http" | "https") {
                    bail!("provider {name}: {field} {url:?} must use http or https");
                }
                let Some(host) = parsed.host_str() else {
                    bail!("provider {name}: {field} {url:?} names no host");
                };
                // The key rides this URL in an Authorization header. Cleartext
                // to a remote host hands it to anything on the path; loopback is
                // the one case where that wire belongs to the operator.
                if parsed.scheme() == "http" && !is_loopback_host(host) {
                    tracing::warn!(
                        provider = %name,
                        %field,
                        "provider URL is http:// to a non-loopback host: the API key crosses \
                         the network in the clear"
                    );
                }
            }
            for multiplier in [p.credit_multiplier]
                .into_iter()
                .chain(p.accounts.values().map(|a| a.credit_multiplier))
            {
                let Some(m) = multiplier else { continue };
                if !m.is_finite() || m <= 0.0 {
                    bail!("provider {name}: credit_multiplier must be finite and > 0, got {m}");
                }
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

        for (provider, models) in &self.prices {
            if !self.providers.contains_key(provider) {
                bail!("[prices.{provider}] names an unknown provider");
            }
            for (model, price) in models {
                if model.trim().is_empty() {
                    bail!("[prices.{provider}] has an empty model name");
                }
                // Surface a malformed window at startup rather than as a
                // silently wrong price at request time.
                price.schedule(provider, model)?;
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
            && !rest.is_empty()
        {
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
    pub fn build_accounts(&self, store: &crate::secrets::CredentialStore) -> Result<Vec<Account>> {
        let mut out = Vec::new();
        for (provider_name, p) in &self.providers {
            for (account_id, a) in &p.accounts {
                // No key configured means the credential store, looked up by
                // provider/account.
                let secret_ref = a.key.clone().unwrap_or(SecretRef::Keychain);
                let key = secret_ref
                    .resolve(store, provider_name, account_id)
                    .with_context(|| format!("provider {provider_name} account {account_id}"))?;
                out.push(build_one(provider_name, p, account_id, a, key));
            }
        }
        if out.is_empty() {
            bail!("configuration produced no accounts");
        }
        Ok(out)
    }

    /// Build accounts, tolerating ones whose credential cannot be resolved.
    ///
    /// Returns (usable, unusable). An unusable account keeps its identity so the
    /// board and doctor can show it, but it is marked Disabled and carries an
    /// empty key, so the pool can never select it and it can never be sent
    /// upstream. This exists because one stale account entry -- a provider you
    /// configured but never registered a key for -- otherwise takes down the
    /// whole board and stops the gateway from starting at all.
    pub fn build_accounts_lenient(
        &self,
        store: &crate::secrets::CredentialStore,
    ) -> Result<(Vec<Account>, Vec<Account>)> {
        let mut usable = Vec::new();
        let mut unusable = Vec::new();
        for (provider_name, p) in &self.providers {
            for (account_id, a) in &p.accounts {
                let secret_ref = a.key.clone().unwrap_or(SecretRef::Keychain);
                match secret_ref.resolve(store, provider_name, account_id) {
                    Ok(key) => usable.push(build_one(provider_name, p, account_id, a, key)),
                    Err(e) => {
                        let mut acct = build_one(provider_name, p, account_id, a, ApiKey::new(""));
                        acct.state = AccountState::Disabled;
                        acct.last_error = Some(format!("{e:#}"));
                        unusable.push(acct);
                    }
                }
            }
        }
        Ok((usable, unusable))
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
                    allow_metered: a.allow_metered.or(p.allow_metered),
                    last_probe_at: None,
                    last_error: None,
                });
            }
        }
        Ok(out)
    }

    /// Providers that need a probe adapter, as (provider, adapter, probe host).
    ///
    /// Every adapter appends a fully documented path to this host (xKiro
    /// /v1/usage, Command Code /alpha/billing/credits, DeepSeek /user/balance),
    /// so it must be the bare host. base_url is the OpenAI-compatible root and
    /// normally ends in /v1, so passing it through unchanged produced
    /// https://api.xkiro.com/v1/v1/usage, which is a 404 -- a real failure an
    /// operator hit. A trailing /v1 is therefore stripped when probe_base_url is
    /// not set explicitly.
    pub fn probe_targets(&self) -> Vec<(ProviderId, QuotaAdapter, String)> {
        self.providers
            .iter()
            .map(|(name, p)| (name.clone(), p.adapter, p.probe_base()))
            .collect()
    }

    pub fn db_path(&self) -> PathBuf {
        expand_tilde(&self.server.db_path)
    }

    /// Soft threshold as Decimal for exact comparisons.
    pub fn soft_threshold_decimal(&self) -> Decimal {
        Decimal::try_from(self.policy.soft_threshold).unwrap_or(Decimal::new(80, 2))
    }
}

/// Expand a leading "~" the way a shell would.
///
/// Config paths are written by hand, so "~/.light-ai-gateway/gateway.db" is the
/// natural way to spell them. Without this, PathBuf treats "~" as an ordinary
/// directory name and the path resolves against the working directory, so the
/// gateway silently creates a literal "~" directory wherever it happens to have
/// been started. The same config then reads a different, empty ledger from a
/// different cwd: the board goes blank, every account comes back Unknown, and
/// nothing explains why.
///
/// Only the two spellings a shell would expand are handled. "~user/..." is left
/// alone deliberately: resolving it needs an account database, and guessing
/// would be worse than reporting a path that does not exist.
pub fn expand_tilde(path: &str) -> PathBuf {
    fn home() -> String {
        std::env::var("HOME").unwrap_or_else(|_| ".".to_string())
    }
    if path == "~" {
        return PathBuf::from(home());
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return PathBuf::from(home()).join(rest);
    }
    PathBuf::from(path)
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
# local_key = { env = "LGW_LOCAL_KEY" }

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
#     lgw keys add xkiro acc-1
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
/// Assemble one Account from its configuration. Shared by the strict and
/// lenient builders so the two cannot drift apart.
fn build_one(
    provider_name: &str,
    p: &ProviderConfig,
    account_id: &str,
    a: &AccountConfig,
    key: ApiKey,
) -> Account {
    Account {
        id: account_id.to_string(),
        provider: provider_name.to_string(),
        label: a.label.clone().unwrap_or_else(|| account_id.to_string()),
        base_url: p.base_url.trim_end_matches('/').to_string(),
        api_key: key,
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
        allow_metered: a.allow_metered.or(p.allow_metered),
        last_probe_at: None,
        last_error: None,
    }
}

/// Whether a URL host names this machine's own loopback interface.
fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Drop a trailing "/v1" (or "/v1beta" style) segment from an API root.
///
/// The OpenAI-compatible base_url points at the versioned root while quota
/// endpoints live above it, so the version segment has to come off. Only a
/// single trailing segment is removed, and only when it looks like an API
/// version, so a path that merely happens to be deep is left alone.
pub fn strip_api_version_suffix(base: &str) -> String {
    let trimmed = base.trim_end_matches('/');
    if let Some((head, tail)) = trimmed.rsplit_once('/') {
        let looks_like_version = tail
            .strip_prefix('v')
            .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()));
        if looks_like_version && !head.is_empty() {
            return head.to_string();
        }
    }
    trimmed.to_string()
}

pub fn sample_toml() -> &'static str {
    SAMPLE
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tier::TierKey;

    /// Which adapters report nothing but a wallet. Pinned here because
    /// wallet_only() restates what the adapters emit, and validate()'s warning
    /// is the only thing that explains an idle wallet-only provider.
    #[test]
    fn only_the_wallet_only_adapters_say_so() {
        assert!(!QuotaAdapter::Xkiro.wallet_only());
        assert!(!QuotaAdapter::CommandCode.wallet_only());
        assert!(!QuotaAdapter::None.wallet_only());
        assert!(QuotaAdapter::Deepseek.wallet_only());
        assert!(QuotaAdapter::Haoai.wallet_only());
    }

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

    /// A routable bind without a local key is refused, not warned about.
    ///
    /// The key is the only thing between the network and the pooled quota, and
    /// the default bind is loopback for exactly that reason; accepting
    /// 0.0.0.0 while merely logging a warning made the missing key a footnote.
    #[test]
    fn a_non_loopback_listen_without_a_local_key_is_refused() {
        let cfg = |listen: &str, key: &str| {
            format!(
                r#"
[server]
listen = "{listen}"
db_path = "gateway.db"
{key}

[providers.p]
adapter = "xkiro"
base_url = "https://api.example.invalid/v1"

[providers.p.accounts.a]
key = "k"

[routes]
"m" = "p"
"#
            )
        };

        let err = Config::from_toml_str(&cfg("0.0.0.0:8787", ""))
            .expect_err("a routable bind with no key must not validate")
            .to_string();
        assert!(err.contains("not loopback"), "got: {err}");

        Config::from_toml_str(&cfg("0.0.0.0:8787", "local_key = { env = \"LGW_LOCAL\" }"))
            .expect("with a key, the bind is the operator's decision");
        Config::from_toml_str(&cfg("127.0.0.1:8787", "")).expect("loopback needs no key");
        Config::from_toml_str(&cfg("[::1]:8787", "")).expect("v6 loopback is loopback too");
    }

    /// A cleartext URL to a remote host is named, not waved through.
    ///
    /// The key rides this URL in an Authorization header, so an http:// target
    /// that is not this machine puts it on the wire for anything on the path.
    /// The warning is the only trace an operator gets, which makes it the
    /// behaviour worth pinning -- and the loopback and https cases are pinned
    /// too, because a warning that cries wolf gets ignored.
    #[tokio::test]
    async fn an_http_provider_url_to_a_remote_host_is_warned_about() {
        let toml = |url: &str| {
            format!(
                r#"
[providers.p]
adapter = "xkiro"
base_url = "{url}"

[providers.p.accounts.a]
key = "k"

[routes]
"m" = "p"
"#
            )
        };
        let warnings = |url: &'static str| async move {
            crate::quota::testutil::capture_warnings(
                async move { Config::from_toml_str(&toml(url)) },
            )
            .await
        };

        let (lines, remote) = warnings("http://api.example.invalid/v1").await;
        remote.expect("http to a remote host is a warning, not a refusal");
        assert!(
            lines
                .iter()
                .any(|l| l.contains("http:// to a non-loopback host")),
            "expected the cleartext warning, got {lines:?}"
        );

        let (lines, local) = warnings("http://127.0.0.1:8080/v1").await;
        local.expect("loopback is the operator's own wire");
        assert!(
            !lines.iter().any(|l| l.contains("non-loopback host")),
            "loopback must not be warned about: {lines:?}"
        );

        let (lines, secure) = warnings("https://api.example.invalid/v1").await;
        secure.expect("https to a remote host is fine");
        assert!(
            !lines.iter().any(|l| l.contains("non-loopback host")),
            "https is not cleartext: {lines:?}"
        );
    }

    /// The provider-level switch reaches its accounts, and an account can still
    /// answer for itself; anything without an override follows the policy.
    #[test]
    fn a_provider_metered_override_is_inherited_and_an_account_can_overrule_it() {
        let cfg = Config::from_toml_str(
            r#"
[policy]
allow_metered = false

[providers.haoai]
adapter = "haoai"
base_url = "https://api.hao.ai/v1"
currency = "USD"
allow_metered = true

[providers.haoai.accounts.acc-1]

[providers.haoai.accounts.acc-2]
allow_metered = false

[providers.deepseek]
adapter = "deepseek"
base_url = "https://api.deepseek.com/v1"

[providers.deepseek.accounts.main]
"#,
        )
        .expect("config must parse");

        let accounts = cfg.build_accounts_unresolved().expect("accounts");
        let flag = |qualified: &str| {
            accounts
                .iter()
                .find(|a| a.qualified() == qualified)
                .unwrap_or_else(|| panic!("no account {qualified}"))
                .allow_metered
        };

        assert_eq!(flag("haoai/acc-1"), Some(true));
        assert_eq!(flag("haoai/acc-2"), Some(false));
        assert_eq!(
            flag("deepseek/main"),
            None,
            "with no override the account follows the policy"
        );
    }

    #[test]
    fn probe_hosts_strip_the_version_segment_of_a_base_url() {
        // Regression: base_url is the OpenAI root (ends in /v1) while quota
        // endpoints live above it. Passing base_url through unchanged made the
        // xKiro probe call /v1/v1/usage and 404 on a real account.
        assert_eq!(
            strip_api_version_suffix("https://api.xkiro.com/v1"),
            "https://api.xkiro.com"
        );
        assert_eq!(
            strip_api_version_suffix("https://api.xkiro.com/v1/"),
            "https://api.xkiro.com"
        );
        assert_eq!(
            strip_api_version_suffix("https://api.deepseek.com"),
            "https://api.deepseek.com"
        );
        // Not a version segment: leave it alone.
        assert_eq!(
            strip_api_version_suffix("https://host/openai"),
            "https://host/openai"
        );
    }

    #[test]
    fn probe_targets_use_a_bare_host_even_when_base_url_is_versioned() {
        let cfg = Config::from_toml_str(
            r#"
[providers.xkiro]
adapter = "xkiro"
base_url = "https://api.xkiro.com/v1"
[providers.xkiro.accounts.a]
key = "k"
[providers.cc]
adapter = "command-code"
base_url = "https://api.commandcode.ai/provider/v1"
probe_base_url = "https://api.commandcode.ai"
[providers.cc.accounts.a]
key = "k"
[routes]
"m" = "xkiro"
"#,
        )
        .unwrap();
        let targets = cfg.probe_targets();
        let xkiro = targets.iter().find(|t| t.0 == "xkiro").unwrap();
        assert_eq!(xkiro.2, "https://api.xkiro.com");
        // The CLI's keys test resolves the same way; a duplicated copy of this
        // logic is what left it calling /v1/v1/usage after the gateway was fixed.
        assert_eq!(
            cfg.providers.get("xkiro").unwrap().probe_base(),
            "https://api.xkiro.com"
        );
        // An explicit probe_base_url always wins.
        let cc = targets.iter().find(|t| t.0 == "cc").unwrap();
        assert_eq!(cc.2, "https://api.commandcode.ai");
    }

    #[test]
    fn prices_use_the_vocabulary_of_a_pricing_page() {
        let cfg = Config::from_toml_str(
            r#"
[providers.xkiro]
adapter = "xkiro"
base_url = "https://api.xkiro.com/v1"

[providers.xkiro.accounts.a]
key = "k"

[routes]
"m" = "xkiro"

[prices.xkiro."m"]
input = 3.0
output = 15.0
"cache read" = 0.3

[prices.xkiro."no-cache-read"]
input = 3.0
output = 15.0
"#,
        )
        .expect("input / output / cache read must parse, including the spaced key");

        let m = &cfg.prices["xkiro"]["m"];
        assert_eq!(m.input, 3.0);
        assert_eq!(m.output, 15.0);
        assert_eq!(m.cache_read, Some(0.3));

        // Omitting it is allowed; the gateway then assumes a tenth of input.
        let n = &cfg.prices["xkiro"]["no-cache-read"];
        assert_eq!(n.cache_read, None);
    }

    #[test]
    fn cache_read_also_parses_with_an_underscore() {
        // A space in a key is legal TOML but easy to mistype, so the underscore
        // spelling is accepted too.
        let cfg = Config::from_toml_str(
            r#"
[providers.xkiro]
adapter = "xkiro"
base_url = "https://api.xkiro.com/v1"
[providers.xkiro.accounts.a]
key = "k"
[routes]
"m" = "xkiro"
[prices.xkiro."m"]
input = 3.0
output = 15.0
cache_read = 0.3
"#,
        )
        .unwrap();
        assert_eq!(cfg.prices["xkiro"]["m"].cache_read, Some(0.3));
    }

    #[test]
    fn an_omitted_cache_read_price_becomes_a_tenth_of_input() {
        use crate::meter::PriceTable;
        let cfg = Config::from_toml_str(
            r#"
[providers.xkiro]
adapter = "xkiro"
base_url = "https://api.xkiro.com/v1"
[providers.xkiro.accounts.a]
key = "k"
[routes]
"m" = "xkiro"
[prices.xkiro."m"]
input = 3.0
output = 15.0
"#,
        )
        .unwrap();
        let mut table = PriceTable::new();
        assert_eq!(table.seed_from_config(&cfg.prices), 1);
        let p = table.get("xkiro", "m", &TierKey::Default).unwrap();
        // $3/M input -> $0.30/M cache read, never zero: a free-looking cache
        // read would under-estimate cost, which is the spending direction.
        assert_eq!(p.cache_read_per_token, rust_decimal::Decimal::new(3, 7));
    }

    /// The bug this guard exists for.
    ///
    /// "lgw init" writes db_path = "~/.light-ai-gateway/gateway.db", and the
    /// real config kept that line. PathBuf has no idea what "~" means, so it
    /// resolved against the working directory: the live ledger ended up in
    /// <cwd>/~/.light-ai-gateway/gateway.db and the file named by the config was
    /// a 0-byte stranger. Restarting the gateway from another directory would
    /// have produced a blank board with no explanation.
    #[test]
    fn a_tilde_db_path_is_expanded_instead_of_staying_relative() {
        let cfg = Config::from_toml_str(
            "[server]\ndb_path = \"~/.light-ai-gateway/gateway.db\"\n\
             [providers.xkiro]\nadapter = \"xkiro\"\nbase_url = \"https://api.xkiro.com/v1\"\n\
             [providers.xkiro.accounts.a]\n",
        )
        .unwrap();
        let p = cfg.db_path();
        assert!(p.is_absolute(), "must expand, got {}", p.display());
        assert!(
            !p.components().any(|c| c.as_os_str() == "~"),
            "no literal ~ component: {}",
            p.display()
        );
        assert!(
            p.ends_with(".light-ai-gateway/gateway.db"),
            "{}",
            p.display()
        );
    }

    #[test]
    fn expand_tilde_handles_exactly_the_two_shell_spellings() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_tilde("~"), PathBuf::from(&home));
        assert_eq!(
            expand_tilde("~/.light-ai-gateway/gateway.db"),
            PathBuf::from(&home).join(".light-ai-gateway/gateway.db")
        );
        // "~user" needs an account database to resolve, so it is left alone
        // rather than guessed at; a relative or absolute path has no tilde.
        assert_eq!(
            expand_tilde("~someone/gw.db"),
            PathBuf::from("~someone/gw.db")
        );
        assert_eq!(
            expand_tilde("/var/lib/gw.db"),
            PathBuf::from("/var/lib/gw.db")
        );
        assert_eq!(expand_tilde("rel/gw.db"), PathBuf::from("rel/gw.db"));
    }

    #[test]
    fn a_peak_off_peak_schedule_parses_into_tiers() {
        // The real Command Code shape: off-peak as the default tier, peak as a
        // windowed override.
        let cfg = Config::from_toml_str(
            r#"
[providers.commandcode]
adapter = "command-code"
base_url = "https://api.commandcode.ai/provider/v1"

[providers.commandcode.accounts.acc-1]
key = "k"

[routes]
"deepseek/deepseek-v4.1-flash" = "commandcode"

[prices.commandcode."deepseek/deepseek-v4.1-flash"]
input = 0.15
output = 0.60
"cache read" = 0.003

[[prices.commandcode."deepseek/deepseek-v4.1-flash".schedules]]
name  = "peak"
days  = "mon-fri"
hours = "01:00-04:00, 06:00-10:00"
tz    = "UTC"
input = 0.30
output = 1.20
"cache read" = 0.006
"#,
        )
        .expect("a windowed price must parse");

        let m = &cfg.prices["commandcode"]["deepseek/deepseek-v4.1-flash"];
        let tiers = m.tiers();
        assert_eq!(tiers.len(), 2);
        assert_eq!(tiers[0].name, "default");
        assert_eq!(tiers[0].input, 0.15);
        assert_eq!(tiers[1].name, "peak");
        assert_eq!(tiers[1].input, 0.30);
        assert_eq!(tiers[1].cache_read, 0.006);

        let schedule = m
            .schedule("commandcode", "deepseek/deepseek-v4.1-flash")
            .unwrap();
        assert_eq!(schedule.tiers.len(), 1);
        assert_eq!(schedule.tiers[0].hours.len(), 2);

        // Both tiers must reach the PRICE TABLE, not just the parsed config. A
        // peak tier that failed to install would not fail loudly: the estimate
        // would fall back to the conservative $15/$75 per million, 100x the real
        // rate, and refuse ordinary requests for the seven hours a day the
        // window is open.
        let mut table = crate::meter::PriceTable::new();
        assert_eq!(table.seed_from_config(&cfg.prices), 2);
        let million = crate::domain::TokenUsage {
            prompt_tokens: 1_000_000,
            completion_tokens: 1_000_000,
            ..Default::default()
        };
        let at = |tier: &TierKey| {
            table.estimate(
                "commandcode",
                "deepseek/deepseek-v4.1-flash",
                tier,
                &million,
                rust_decimal::Decimal::ONE,
            )
        };
        assert_eq!(
            at(&TierKey::Default),
            Some(Decimal::new(75, 2)),
            "0.15 + 0.60 = 0.75 USD"
        );
        assert_eq!(
            at(&TierKey::Named("peak".into())),
            Some(Decimal::new(150, 2)),
            "0.30 + 1.20 = 1.50 USD"
        );

        // And the window that picks between them: 7 hours a day, weekdays only.
        use time::macros::datetime;
        assert_eq!(
            schedule.tier_at(datetime!(2026-09-15 02:00:00 UTC)),
            TierKey::Named("peak".into()),
            "Tuesday 02:00 is inside the window"
        );
        assert_eq!(
            schedule.tier_at(datetime!(2026-09-15 05:00:00 UTC)),
            TierKey::Default,
            "Tuesday 05:00 falls between the two windows"
        );
        assert_eq!(
            schedule.tier_at(datetime!(2026-09-19 02:00:00 UTC)),
            TierKey::Default,
            "Saturday is never peak"
        );
    }

    #[test]
    fn an_omitted_cache_read_on_a_tier_defaults_to_a_tenth_of_that_tier() {
        let cfg = Config::from_toml_str(
            r#"
[providers.p]
adapter = "none"
base_url = "https://example.invalid/v1"
[providers.p.accounts.a]
key = "k"
[routes]
"m" = "p"
[prices.p."m"]
input = 3.0
output = 15.0
[[prices.p."m".schedules]]
days = "sat,sun"
hours = "00:00-23:59"
input = 6.0
output = 30.0
"#,
        )
        .unwrap();
        let tiers = cfg.prices["p"]["m"].tiers();
        assert!(
            (tiers[1].cache_read - 0.6).abs() < 1e-9,
            "a tenth of that tier's input, got {}",
            tiers[1].cache_read
        );
    }

    #[test]
    fn a_malformed_window_fails_at_startup_not_at_request_time() {
        let bad = Config::from_toml_str(
            r#"
[providers.p]
adapter = "none"
base_url = "https://example.invalid/v1"
[providers.p.accounts.a]
key = "k"
[routes]
"m" = "p"
[prices.p."m"]
input = 1.0
output = 2.0
[[prices.p."m".schedules]]
days = "mon-fri"
hours = "01:00-01:00"
input = 1.0
output = 2.0
"#,
        );
        let err = format!("{:#}", bad.unwrap_err());
        assert!(err.contains("empty"), "got: {err}");
    }

    #[test]
    fn shipped_example_config_parses() {
        // The example is documentation people copy; keep it honest.
        let raw = include_str!("../config.example.toml");
        let cfg = Config::from_toml_str(raw).expect("config.example.toml must stay valid");
        assert_eq!(cfg.providers.len(), 4);
        assert!(!cfg.routes.is_empty(), "the example must route something");

        // A model name gets copied out of here into a real config, so both of the
        // ways it can be wrong are checked: a route that does not resolve to the
        // provider it names, and a price written for a model no route reaches. The
        // second is how a wrong vendor prefix hides, and it leaves the route
        // priced at the conservative default without saying so.
        for (model, provider) in &cfg.routes {
            assert_eq!(
                &cfg.resolve(model).unwrap().provider,
                provider,
                "the example routes {model} to {provider}"
            );
        }
        for (provider, models) in &cfg.prices {
            for model in models.keys() {
                assert_eq!(
                    cfg.routes.get(model),
                    Some(provider),
                    "[prices.{provider}] {model} is unreachable: no route sends it there"
                );
            }
        }

        assert!(
            !cfg.policy.allow_metered,
            "default must not spend metered credit"
        );
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
            assert!(
                a.gates.is_empty(),
                "must not assume zero spend before probing"
            );
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
            err.contains("lgw keys add deepseek d1"),
            "the error must tell the operator what to run; got: {err}"
        );
    }

    #[test]
    fn slashes_are_rejected_because_they_would_alias_credentials() {
        let mut cfg = cfg_with_two_providers();
        cfg.providers.get_mut("deepseek").unwrap().accounts.insert(
            "bad/id".into(),
            AccountConfig {
                key: None,
                label: None,
                currency: None,
                credit_multiplier: None,
                concurrency_limit: None,
                rpm_limit: None,
                allow_metered: None,
            },
        );
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

    /// A SecretRef holds the key BEFORE it becomes an ApiKey, so it must redact
    /// too. The derived Debug printed an inline literal in full, and any error
    /// context or log line carrying one would have written it to the terminal.
    #[test]
    fn debug_never_prints_an_inline_secret() {
        let r = SecretRef::Literal("sk-super-secret".to_string());
        assert!(!format!("{r:?}").contains("super"), "got {r:?}");
        assert_eq!(format!("{r:?}"), "Literal(***)");
        // The non-secret forms still say what they are.
        assert!(format!("{:?}", SecretRef::Env { env: "X".into() }).contains('X'));
        assert!(format!("{:?}", SecretRef::Keychain).contains("Keychain"));
    }

    /// A hostname without a scheme is the classic copy-paste error, and the URL
    /// was only checked for emptiness, so every request failed at runtime while
    /// doctor could still report everything fine.
    #[test]
    fn a_base_url_without_a_scheme_is_refused_at_startup() {
        let mut cfg = cfg_with_two_providers();
        cfg.providers.get_mut("xkiro").unwrap().base_url = "api.example.com/v1".into();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("absolute URL"), "got: {err}");

        let mut cfg = cfg_with_two_providers();
        cfg.providers.get_mut("xkiro").unwrap().base_url = "ftp://example.com".into();
        assert!(cfg.validate().is_err());

        let mut cfg = cfg_with_two_providers();
        cfg.providers.get_mut("xkiro").unwrap().base_url = "https://example.com/v1".into();
        assert!(cfg.validate().is_ok());
    }

    /// A margin that cannot be a margin is refused rather than silently turned
    /// into zero (or, worse, left negative) inside the hard gate.
    #[test]
    fn a_margin_that_would_weaken_the_gate_is_refused() {
        for bad in [-5.0, f64::NAN, f64::INFINITY] {
            let mut cfg = cfg_with_two_providers();
            cfg.policy.hard_margin = bad;
            let err = cfg.validate().unwrap_err().to_string();
            assert!(err.contains("hard_margin"), "got: {err}");
        }
        let mut cfg = cfg_with_two_providers();
        cfg.policy.hard_margin = 0.5;
        assert!(cfg.validate().is_ok());
    }

    /// Same direction, one level down: a multiplier of zero would make every
    /// model look free to the gate.
    #[test]
    fn a_credit_multiplier_that_is_not_positive_is_refused() {
        let mut cfg = cfg_with_two_providers();
        cfg.providers.get_mut("xkiro").unwrap().credit_multiplier = Some(0.0);
        assert!(cfg.validate().is_err());

        let mut cfg = cfg_with_two_providers();
        cfg.providers.get_mut("xkiro").unwrap().credit_multiplier = Some(f64::NAN);
        assert!(cfg.validate().is_err());
    }
}
