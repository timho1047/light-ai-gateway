//! Command-line interface.
//!
//! Commands: serve, status, usage, accounts, tail, doctor, init.
//!
//! The status view keeps covered and metered allowances visually distinct and
//! never merges them into one number: conflating them is how an operator
//! mistakes spending real money for consuming a prepaid subscription.
//!
//! Configuration path resolution, in order:
//!   1. --config PATH
//!   2. $LGW_CONFIG
//!   3. ./lgw.toml, when it exists
//!   4. ~/.light-ai-gateway/config.toml
//!
//! `lgw init` writes to exactly the path this resolution picks, so the file
//! it creates is the file the other commands read back.
//!
//! Read-only commands consult the persisted ledger (src/store.rs) rather than
//! building a live pool: they must work without credentials and without
//! touching the network. Only status --refresh, accounts probe and doctor reach
//! out to providers.

mod accounts;
mod doctor;
mod init;
mod keys;
mod prices;
mod serve;
mod status;
mod tail;
#[cfg(test)]
mod test_helpers;
mod usage;

use crate::ansi::Palette;
use crate::config::Config;
use crate::secrets::CredentialStore;
use crate::store::{self, Ledger};
use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use std::path::{Path, PathBuf};

/// Where the configuration file was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigOrigin {
    /// --config PATH
    Flag,
    /// $LGW_CONFIG
    Env,
    /// ./lgw.toml
    Local,
    /// ~/.light-ai-gateway/config.toml
    Home,
}

impl ConfigOrigin {
    fn label(self) -> &'static str {
        match self {
            ConfigOrigin::Flag => "--config",
            ConfigOrigin::Env => "$LGW_CONFIG",
            ConfigOrigin::Local => "./lgw.toml",
            ConfigOrigin::Home => "~/.light-ai-gateway/config.toml",
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "lgw",
    version,
    about = "Light AI gateway: pool provider accounts and route around exhausted quota"
)]
struct Cli {
    /// Path to the TOML configuration file.
    #[arg(long, short = 'c', global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    /// When to colour the output: auto, always, never.
    ///
    /// `auto` colours only a terminal, so a pipe or a redirect -- every script
    /// and log that reads this program -- gets the same bytes it always did.
    #[arg(long, global = true, value_enum, default_value_t = ColorWhen::Auto, value_name = "WHEN")]
    color: ColorWhen,

    #[command(subcommand)]
    command: Command,
}

/// What `--color` was told to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum ColorWhen {
    /// Colour a terminal, leave a pipe alone.
    Auto,
    /// Colour even when the output is not a terminal.
    Always,
    /// Never colour.
    Never,
}

impl ColorWhen {
    fn palette(self) -> Palette {
        match self {
            ColorWhen::Auto => Palette::auto(),
            ColorWhen::Always => Palette::on(),
            ColorWhen::Never => Palette::off(),
        }
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start the OpenAI-compatible proxy.
    Serve,
    /// Account board: covered allowance versus metered money.
    Status(StatusArgs),
    /// Aggregated spend and token usage.
    Usage(UsageArgs),
    /// Account maintenance.
    Accounts(AccountsArgs),
    /// Most recent requests, newest first.
    Tail(TailArgs),
    /// Check config, credentials, and probe reachability.
    Doctor,
    /// Write an example configuration file.
    Init(InitArgs),
    /// Register, list, and remove provider credentials.
    Keys(KeysArgs),
    /// Show configured prices against the prices learned from real spend.
    Prices(PricesArgs),
}

#[derive(Debug, Args)]
struct PricesArgs {
    /// Show only models whose observed price has moved away from the configured one.
    #[arg(long)]
    drift_only: bool,

    /// Forget learned prices and fall back to the configured card.
    ///
    /// With no value: every provider. With one: that provider. With two: that
    /// provider and model. Learning is only sound while an account is used
    /// exclusively through this gateway -- spend from anywhere else lands in the
    /// same meter and gets attributed to these tokens -- so this is the way back
    /// to the price the operator wrote down.
    #[arg(long, value_names = ["PROVIDER", "MODEL"], num_args = 0..=2)]
    forget: Option<Vec<String>>,
}

#[derive(Debug, Args)]
struct KeysArgs {
    #[command(subcommand)]
    command: KeysCommand,
}

#[derive(Debug, Subcommand)]
enum KeysCommand {
    /// Store a credential in the OS credential store.
    ///
    /// The key is read from a hidden prompt, never from a command-line argument:
    /// arguments are visible in the process table and land in shell history.
    Add {
        provider: String,
        account: String,
        /// Read the key from stdin instead of prompting, for scripting.
        #[arg(long)]
        stdin: bool,
    },
    /// Show every configured account, where its credential comes from, and whether it resolves.
    List,
    /// Remove a stored credential.
    Remove { provider: String, account: String },
    /// Check a stored credential by probing the provider with it.
    Test { provider: String, account: String },
}

#[derive(Debug, Args)]
struct StatusArgs {
    /// Probe every account before rendering the board.
    #[arg(long)]
    refresh: bool,
}

#[derive(Debug, Args)]
struct UsageArgs {
    /// Window to aggregate over: 7d, 24h, 90m, 2w, or an RFC3339 timestamp.
    #[arg(long, default_value = "7d")]
    since: String,
    /// Comma-separated dimensions: provider, model, account, day.
    #[arg(long, default_value = "provider")]
    group_by: String,
}

#[derive(Debug, Args)]
struct AccountsArgs {
    #[command(subcommand)]
    command: AccountsCommand,
}

#[derive(Debug, Subcommand)]
enum AccountsCommand {
    /// List known accounts from the ledger.
    List,
    /// Probe all accounts, or one account by id.
    Probe {
        /// Account id. Omit to probe every account.
        account: Option<String>,
    },
    /// Take an account out of the pool until an operator re-enables it.
    Disable {
        /// Account id to disable.
        account: String,
    },
    /// Put a disabled account back in the pool, after a probe proves it works.
    Enable {
        /// Account id to enable.
        account: String,
    },
}

#[derive(Debug, Args)]
struct TailArgs {
    /// How many recent requests to show.
    #[arg(long, default_value_t = 20)]
    limit: u32,
}

#[derive(Debug, Args)]
struct InitArgs {
    /// Write here instead of the resolved config path.
    #[arg(long, value_name = "PATH")]
    path: Option<PathBuf>,
    /// Overwrite an existing file.
    #[arg(long)]
    force: bool,
}

/// Parse arguments and dispatch.
pub async fn run() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    dispatch(cli).await
}

async fn dispatch(cli: Cli) -> Result<()> {
    let (path, origin) = resolve_config_path(cli.config.clone());
    // One decision, made once: every renderer below either has a palette or
    // prints the plain text, and no command decides for itself.
    let pal = cli.color.palette();
    // One credential store per run, so a command never reaches for the
    // operator's keychain itself: a test hands in an in-memory one.
    let creds = credential_store();
    match cli.command {
        Command::Init(args) => {
            let target = args.path.clone().unwrap_or_else(|| path.clone());
            init::cmd_init(&target, args.force)
        }
        Command::Serve => serve::cmd_serve(&path, origin).await,
        // Each command is handed what it reads. A handler that takes a path
        // rebuilds its own config, which is what kept every one of them out of
        // reach of a test.
        Command::Status(args) => {
            let config = load_config(&path)?;
            status::cmd_status(&config, args.refresh, &pal).await
        }
        Command::Usage(args) => {
            let config = load_config(&path)?;
            usage::cmd_usage(&config, &args, &pal)
        }
        Command::Accounts(args) => {
            let config = load_config(&path)?;
            accounts::cmd_accounts(&config, args, &pal).await
        }
        Command::Tail(args) => {
            let config = load_config(&path)?;
            tail::cmd_tail(&config, args.limit, &pal)
        }
        // Doctor reads the config itself: reporting a config that will not load
        // is the one thing it exists to do.
        Command::Doctor => doctor::cmd_doctor(&path, origin, &pal, &creds).await,
        Command::Keys(args) => {
            let config = load_config(&path)?;
            keys::cmd_keys(&config, &creds, args, &pal).await
        }
        Command::Prices(args) => {
            let config = load_config(&path)?;
            prices::cmd_prices(&config, &args, &pal)
        }
    }
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

// ---------------------------------------------------------------------------
// config path resolution
// ---------------------------------------------------------------------------

fn resolve_config_path(flag: Option<PathBuf>) -> (PathBuf, ConfigOrigin) {
    let env = crate::env_preferring("LGW_CONFIG", "AI_GW_CONFIG")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty());
    let local = PathBuf::from("lgw.toml");
    let local_exists = local.is_file();
    pick_config_path(flag, env, local, local_exists, home_config_path())
}

/// Pure precedence rule, so it can be tested without touching the environment.
fn pick_config_path(
    flag: Option<PathBuf>,
    env: Option<PathBuf>,
    local: PathBuf,
    local_exists: bool,
    home: PathBuf,
) -> (PathBuf, ConfigOrigin) {
    if let Some(p) = flag {
        return (p, ConfigOrigin::Flag);
    }
    if let Some(p) = env {
        return (p, ConfigOrigin::Env);
    }
    if local_exists {
        return (local, ConfigOrigin::Local);
    }
    (home, ConfigOrigin::Home)
}

fn home_config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".light-ai-gateway/config.toml")
}

fn load_config(path: &Path) -> Result<Config> {
    if !path.is_file() {
        bail!(
            "no config file at {}. Run `lgw init` to write a starter, or pass --config PATH.",
            path.display()
        );
    }
    Config::load(path)
}

/// Open the persisted ledger read-only.
///
/// A missing ledger is reported rather than treated as an empty account list:
/// an empty board would read as "no accounts" when the truth is "not probed
/// yet", and on Command Code that confusion is how real money gets spent.
fn open_ledger(db: &Path) -> Result<Ledger> {
    Ledger::open(db).map_err(|e| match e {
        store::LedgerError::Missing(path) => anyhow::anyhow!(
            "no ledger at {} yet. Run `lgw serve` once (or `lgw status --refresh`) \
             so accounts are probed and persisted.",
            path.display()
        ),
        other => anyhow::Error::new(other),
    })
}

/// HTTP client for quota probes.
///
/// ONE constructor on purpose. The policy here is load-bearing and two call
/// sites had already drifted apart once: the daemon pinned redirects and the CLI
/// probes did not, so a 3xx could have carried a bearer token to whatever host a
/// Location header named. Two copies of a security policy is one copy too many.
fn probe_client(config: &Config) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(
            config.policy.connect_timeout_secs.max(1),
        ))
        // The bearer token is attached before the request leaves, so following a
        // redirect would hand the credential to the host it names.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("building the probe HTTP client")
}

/// Which credential backend to use. See CredentialStore::from_env.
fn credential_store() -> CredentialStore {
    CredentialStore::from_env()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_path_precedence_is_flag_env_local_home() {
        let flag = PathBuf::from("/tmp/flag.toml");
        let env = PathBuf::from("/tmp/env.toml");
        let local = PathBuf::from("lgw.toml");
        let home = PathBuf::from("/tmp/home/config.toml");

        assert_eq!(
            pick_config_path(
                Some(flag.clone()),
                Some(env.clone()),
                local.clone(),
                true,
                home.clone()
            ),
            (flag.clone(), ConfigOrigin::Flag)
        );
        assert_eq!(
            pick_config_path(None, Some(env.clone()), local.clone(), true, home.clone()),
            (env.clone(), ConfigOrigin::Env)
        );
        assert_eq!(
            pick_config_path(None, None, local.clone(), true, home.clone()),
            (local.clone(), ConfigOrigin::Local)
        );
        // A ./lgw.toml that does not exist must not win over the home default.
        assert_eq!(
            pick_config_path(None, None, local.clone(), false, home.clone()),
            (home.clone(), ConfigOrigin::Home)
        );
    }

    #[test]
    fn config_origin_labels_name_the_source() {
        assert_eq!(ConfigOrigin::Flag.label(), "--config");
        assert_eq!(ConfigOrigin::Env.label(), "$LGW_CONFIG");
        assert_eq!(ConfigOrigin::Local.label(), "./lgw.toml");
        assert_eq!(
            ConfigOrigin::Home.label(),
            "~/.light-ai-gateway/config.toml"
        );
    }
}
