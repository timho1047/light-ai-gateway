//! Command-line interface.
//!
//! FILE OWNER: teammate meter-dev.
//!
//! Commands: serve, status, usage, accounts, tail, doctor, init.
//!
//! The status view keeps covered and metered allowances visually distinct and
//! never merges them into one number: conflating them is how an operator
//! mistakes spending real money for consuming a prepaid subscription.
//!
//! Configuration path resolution, in order:
//!   1. --config PATH
//!   2. $AI_GW_CONFIG
//!   3. ./ai-gw.toml, when it exists
//!   4. ~/.light-ai-gateway/config.toml
//!
//! `ai-gw init` writes to exactly the path this resolution picks, so the file
//! it creates is the file the other commands read back.
//!
//! Read-only commands consult the persisted ledger (src/store.rs) rather than
//! building a live pool: they must work without credentials and without
//! touching the network. Only status --refresh, accounts probe and doctor reach
//! out to providers.

use crate::accounts::Pool;
use crate::config::{Config, QuotaAdapter, SecretRef};
use crate::secrets::CredentialStore;
use crate::domain::{
    Account, AccountOverview, AccountState, Confidence, Gate, GateClass, GateKind, GroupBy,
    RequestStatus, RequestSummary, UsageRow,
};
use crate::error::UpstreamErrorClass;
use crate::quota::probe_for;
use crate::router::Gateway;
use crate::store;
use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use rust_decimal::Decimal;
use rusqlite::Connection;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use time::OffsetDateTime;

/// Where the configuration file was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigOrigin {
    /// --config PATH
    Flag,
    /// $AI_GW_CONFIG
    Env,
    /// ./ai-gw.toml
    Local,
    /// ~/.light-ai-gateway/config.toml
    Home,
}

impl ConfigOrigin {
    fn label(self) -> &'static str {
        match self {
            ConfigOrigin::Flag => "--config",
            ConfigOrigin::Env => "$AI_GW_CONFIG",
            ConfigOrigin::Local => "./ai-gw.toml",
            ConfigOrigin::Home => "~/.light-ai-gateway/config.toml",
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "ai-gw",
    version,
    about = "Light AI gateway: pool provider accounts and route around exhausted quota"
)]
pub struct Cli {
    /// Path to the TOML configuration file.
    #[arg(long, short = 'c', global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
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
}

#[derive(Debug, Args)]
pub struct KeysArgs {
    #[command(subcommand)]
    pub command: KeysCommand,
}

#[derive(Debug, Subcommand)]
pub enum KeysCommand {
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
pub struct StatusArgs {
    /// Probe every account before rendering the board.
    #[arg(long)]
    pub refresh: bool,
}

#[derive(Debug, Args)]
pub struct UsageArgs {
    /// Window to aggregate over: 7d, 24h, 90m, 2w, or an RFC3339 timestamp.
    #[arg(long, default_value = "7d")]
    pub since: String,
    /// Comma-separated dimensions: provider, model, account, day.
    #[arg(long, default_value = "provider")]
    pub group_by: String,
}

#[derive(Debug, Args)]
pub struct AccountsArgs {
    #[command(subcommand)]
    pub command: AccountsCommand,
}

#[derive(Debug, Subcommand)]
pub enum AccountsCommand {
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
}

#[derive(Debug, Args)]
pub struct TailArgs {
    /// How many recent requests to show.
    #[arg(long, default_value_t = 20)]
    pub limit: u32,
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Write here instead of the resolved config path.
    #[arg(long, value_name = "PATH")]
    pub path: Option<PathBuf>,
    /// Overwrite an existing file.
    #[arg(long)]
    pub force: bool,
}

/// Parse arguments and dispatch.
pub async fn run() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    dispatch(cli).await
}

async fn dispatch(cli: Cli) -> Result<()> {
    let (path, origin) = resolve_config_path(cli.config.clone());
    match cli.command {
        Command::Init(args) => {
            let target = args.path.clone().unwrap_or_else(|| path.clone());
            cmd_init(&target, args.force)
        }
        Command::Serve => {
            let config = load_config(&path)?;
            println!(
                "ai-gw: starting proxy from {} ({})",
                path.display(),
                origin.label()
            );
            crate::proxy::serve(config).await
        }
        Command::Status(args) => cmd_status(&path, args.refresh).await,
        Command::Usage(args) => cmd_usage(&path, &args),
        Command::Accounts(args) => cmd_accounts(&path, args).await,
        Command::Tail(args) => cmd_tail(&path, args.limit),
        Command::Doctor => cmd_doctor(&path, origin).await,
        Command::Keys(args) => cmd_keys(&path, args).await,
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
    let env = std::env::var_os("AI_GW_CONFIG")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty());
    let local = PathBuf::from("ai-gw.toml");
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
            "no config file at {}. Run `ai-gw init` to write a starter, or pass --config PATH.",
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
fn open_ledger(db: &Path) -> Result<Connection> {
    if !store::db_exists(db) {
        bail!(
            "no ledger at {} yet. Run `ai-gw serve` once (or `ai-gw status --refresh`) \
             so accounts are probed and persisted.",
            db.display()
        );
    }
    store::open_readonly(db).with_context(|| format!("opening ledger {}", db.display()))
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

async fn cmd_status(path: &Path, refresh: bool) -> Result<()> {
    let config = load_config(path)?;
    let now = OffsetDateTime::now_utc();

    if refresh {
        let gateway = Gateway::new(config).await?;
        let rows: Vec<AccountOverview> = gateway.snapshot().iter().map(overview_of).collect();
        print!("{}", render_board(&rows, now));
        let _ = gateway.shutdown();
        return Ok(());
    }

    let conn = open_ledger(&config.db_path())?;
    let rows = store::load_accounts(&conn)?;
    if rows.is_empty() {
        println!(
            "no accounts in the ledger yet at {}; run ai-gw status --refresh",
            config.db_path().display()
        );
        return Ok(());
    }
    print!("{}", render_board(&rows, now));
    Ok(())
}

fn overview_of(a: &Account) -> AccountOverview {
    AccountOverview {
        account_id: a.id.clone(),
        provider: a.provider.clone(),
        label: a.label.clone(),
        currency: a.currency.clone(),
        state: a.state,
        gates: a.gates.clone(),
        available_at: a.available_at,
        window_started_at: a.window_started_at,
        last_error: a.last_error.clone(),
    }
}

const BOARD_HEADERS: [&str; 6] = ["PROVIDER", "ACCOUNT", "COVERED", "METERED", "RESETS", "STATE"];
const BOARD_WIDTHS: [usize; 5] = [14, 18, 26, 26, 8];

/// Render the account board.
///
/// COVERED shows what has been consumed out of a prepaid allowance (used/cap).
/// METERED shows the money that is left, and says plainly when none of it has
/// been touched. The two are deliberately different shapes so that an operator
/// scanning the table cannot mistake one for the other.
fn render_board(rows: &[AccountOverview], now: OffsetDateTime) -> String {
    let mut out = String::new();
    out.push_str(&board_row(&BOARD_HEADERS));
    for r in rows {
        let covered = covered_cell(r);
        let metered = metered_cell(r);
        let resets = reset_cell(r, now);
        let state = state_cell(r);
        out.push_str(&board_row(&[
            r.provider.as_str(),
            r.account_id.as_str(),
            &covered,
            &metered,
            &resets,
            &state,
        ]));
    }
    out.push('\n');
    out.push_str(
        "covered = prepaid allowance consumed (used/cap, forfeited at reset)   \
         metered = real money remaining (spending it is permanent)\n",
    );
    out.push_str("~ = estimated reading   STALE = last probe failed, previous value carried over\n");
    out
}

fn board_row(cells: &[&str; 6]) -> String {
    let mut line = String::new();
    for (i, cell) in cells.iter().enumerate() {
        if i == cells.len() - 1 {
            line.push_str(cell);
        } else {
            line.push_str(&format!("{:<width$}", cell, width = BOARD_WIDTHS[i]));
            line.push_str("  ");
        }
    }
    line.push('\n');
    line
}

fn covered_cell(r: &AccountOverview) -> String {
    let mut gates: Vec<&Gate> = r
        .gates
        .iter()
        .filter(|g| g.class() == GateClass::Covered)
        .collect();
    if gates.is_empty() {
        return "-".to_string();
    }
    gates.sort_by_key(|g| gate_rank(g.kind));
    let parts: Vec<String> = gates
        .iter()
        .map(|g| format!("{}/{}", short(g.used), short(g.cap)))
        .collect();
    // The unit matters: "40/100" is credits on Command Code and USD on xKiro,
    // and an operator reading the board has to know which pot it is.
    format!(
        "{} {}{}",
        parts.join(" "),
        r.currency,
        markers(&gates)
    )
}

fn metered_cell(r: &AccountOverview) -> String {
    let mut gates: Vec<&Gate> = r
        .gates
        .iter()
        .filter(|g| g.class() == GateClass::Metered)
        .collect();
    if gates.is_empty() {
        return "-".to_string();
    }
    gates.sort_by_key(|g| gate_rank(g.kind));
    let parts: Vec<String> = gates
        .iter()
        .map(|g| {
            let note = if g.used <= Decimal::ZERO {
                "unused".to_string()
            } else {
                format!("{} spent", short(g.used))
            };
            format!("{} {}({})", short(g.remaining()), r.currency, note)
        })
        .collect();
    format!("{}{}", parts.join(" "), markers(&gates))
}

/// A stale reading dominates an estimated one: it is the reading most likely to
/// be wrong, and it must never be silently rendered as a fresh number.
fn markers(gates: &[&Gate]) -> String {
    if gates.iter().any(|g| g.stale) {
        " STALE".to_string()
    } else if gates.iter().any(|g| g.confidence == Confidence::Estimated) {
        " ~EST".to_string()
    } else {
        String::new()
    }
}

fn reset_cell(r: &AccountOverview, now: OffsetDateTime) -> String {
    match next_reset(r) {
        Some(at) => format_countdown(now, at),
        None => "-".to_string(),
    }
}

fn next_reset(r: &AccountOverview) -> Option<OffsetDateTime> {
    let mut best = r.available_at;
    for g in r.gates.iter().filter(|g| g.kind.is_window()) {
        if let Some(t) = g.resets_at {
            best = Some(match best {
                Some(b) if b <= t => b,
                _ => t,
            });
        }
    }
    best
}

fn format_countdown(now: OffsetDateTime, at: OffsetDateTime) -> String {
    let delta = at - now;
    if delta.is_negative() {
        return "now".to_string();
    }
    let secs = delta.whole_seconds().max(0) as u64;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3_600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h{}m", secs / 3_600, (secs % 3_600) / 60)
    } else {
        format!("{}d{}h", secs / 86_400, (secs % 86_400) / 3_600)
    }
}

fn state_cell(r: &AccountOverview) -> String {
    let mut s = r.state.label().to_string();

    // A terminal state explains itself first; otherwise describe the shape of
    // what was read. Only one parenthetical is shown so the column stays short.
    let diagnosis = match r.state {
        AccountState::Disabled | AccountState::Parked => r.last_error.clone(),
        _ => None,
    };
    let has_covered = r.gates.iter().any(|g| g.class() == GateClass::Covered);
    let has_metered = r.gates.iter().any(|g| g.class() == GateClass::Metered);
    let shape = if !has_covered && has_metered {
        // DeepSeek is the archetype: no window, no "80%", just a balance line.
        Some("metered only".to_string())
    } else if !has_covered && !has_metered {
        // A missing reading must never look like a funded account.
        Some("no reading".to_string())
    } else {
        None
    };

    if let Some(detail) = diagnosis.or(shape) {
        s.push_str(&format!(" ({detail})"));
    }
    s
}

fn gate_rank(kind: GateKind) -> u8 {
    match kind {
        GateKind::FiveHour => 0,
        GateKind::Weekly => 1,
        GateKind::Monthly => 2,
        GateKind::FreeTokens => 3,
        GateKind::Wallet => 4,
        GateKind::PurchasedCredits => 5,
        GateKind::Rpm => 6,
    }
}

/// Decimal without trailing zeros: 12.3400 prints as 12.34, 0 as 0.
fn short(d: Decimal) -> String {
    d.normalize().to_string()
}

// ---------------------------------------------------------------------------
// usage
// ---------------------------------------------------------------------------

fn cmd_usage(path: &Path, args: &UsageArgs) -> Result<()> {
    let config = load_config(path)?;
    let conn = open_ledger(&config.db_path())?;
    let now = OffsetDateTime::now_utc();
    let since = parse_since(&args.since, now)?;
    let dims = parse_group_by(&args.group_by)?;

    let rows = if dims.len() == 1 {
        store::usage_rows(&conn, since, dims[0])?
    } else {
        // store::usage_rows aggregates by exactly one dimension, and the CLI
        // advertises "provider,model,account". Multi-dimension grouping is done
        // here, with the same discipline as the store: money stays TEXT and is
        // added as Decimal, never through a SQL float SUM.
        usage_rows_multi(&conn, since, &dims)?
    };

    if rows.is_empty() {
        println!(
            "no requests since {} in {}",
            since,
            config.db_path().display()
        );
        return Ok(());
    }
    print!("{}", render_usage(&rows, &dims, since));
    Ok(())
}

fn parse_group_by(spec: &str) -> Result<Vec<GroupBy>> {
    let mut out: Vec<GroupBy> = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let dim = GroupBy::parse(part).ok_or_else(|| {
            anyhow!("unknown --group-by value {part:?}; expected provider, model, account, or day")
        })?;
        if !out.contains(&dim) {
            out.push(dim);
        }
    }
    if out.is_empty() {
        out.push(GroupBy::Provider);
    }
    Ok(out)
}

fn parse_since(spec: &str, now: OffsetDateTime) -> Result<OffsetDateTime> {
    let spec = spec.trim();
    if spec.is_empty() {
        bail!("--since must not be empty");
    }

    if let Some(offset) = parse_relative(spec) {
        return Ok(now - offset);
    }
    if let Ok(t) = OffsetDateTime::parse(spec, &time::format_description::well_known::Rfc3339) {
        return Ok(t);
    }
    const DATE: &[time::format_description::FormatItem<'_>] =
        time::macros::format_description!("[year]-[month]-[day]");
    if let Ok(date) = time::Date::parse(spec, DATE) {
        return Ok(date.midnight().assume_utc());
    }

    bail!("could not parse --since {spec:?}; use 7d, 24h, 90m, 2w, an RFC3339 timestamp, or YYYY-MM-DD")
}

/// Parse "7d" / "24h" / "90m" / "45s" / "2w" into a duration.
fn parse_relative(spec: &str) -> Option<time::Duration> {
    let (digits, unit) = spec.split_at(spec.len().checked_sub(1)?);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: i64 = digits.parse().ok()?;
    let secs = match unit {
        "s" => n,
        "m" => n.checked_mul(60)?,
        "h" => n.checked_mul(3_600)?,
        "d" => n.checked_mul(86_400)?,
        "w" => n.checked_mul(604_800)?,
        _ => return None,
    };
    Some(time::Duration::seconds(secs))
}

/// Aggregate over several dimensions at once.
///
/// Every dimension expression is NOT NULL in practice (provider, model and
/// account_id are NOT NULL columns; day is derived), but COALESCE keeps a
/// surprising NULL from collapsing the whole concatenated key.
fn usage_rows_multi(conn: &Connection, since: OffsetDateTime, dims: &[GroupBy]) -> Result<Vec<UsageRow>> {
    let key_expr = dims
        .iter()
        .map(|d| format!("COALESCE({}, '')", d.as_sql_expr()))
        .collect::<Vec<_>>()
        .join(" || '|' || ");
    let sql = format!(
        "SELECT {key_expr} AS k, currency, prompt_tokens, completion_tokens, cached_tokens, cost \
         FROM requests WHERE ts >= ?1"
    );

    let mut acc: BTreeMap<(String, String), UsageRow> = BTreeMap::new();
    let mut stmt = conn.prepare(&sql).context("preparing usage query")?;
    let mut rows = stmt.query(rusqlite::params![since.unix_timestamp()])?;
    while let Some(row) = rows.next()? {
        let key: String = row.get::<_, Option<String>>(0)?.unwrap_or_default();
        let currency: String = row.get::<_, Option<String>>(1)?.unwrap_or_default();
        let prompt = row.get::<_, i64>(2)?.max(0) as u64;
        let completion = row.get::<_, i64>(3)?.max(0) as u64;
        let cached = row.get::<_, i64>(4)?.max(0) as u64;
        let cost = row
            .get::<_, Option<String>>(5)?
            .as_deref()
            .and_then(crate::domain::parse_money)
            .unwrap_or(Decimal::ZERO);

        let entry = acc
            .entry((key.clone(), currency.clone()))
            .or_insert_with(|| UsageRow {
                key,
                requests: 0,
                input_tokens: 0,
                output_tokens: 0,
                cached_tokens: 0,
                cost: Decimal::ZERO,
                currency,
            });
        entry.requests += 1;
        entry.input_tokens += prompt;
        entry.output_tokens += completion;
        entry.cached_tokens += cached;
        entry.cost += cost;
    }
    Ok(acc.into_values().collect())
}

fn render_usage(rows: &[UsageRow], dims: &[GroupBy], since: OffsetDateTime) -> String {
    let header = dims
        .iter()
        .map(|d| d.column_header())
        .collect::<Vec<_>>()
        .join("+");
    let key_width = header.len().max(16);

    // One key can legitimately appear once per currency, and the two must not
    // be added together (DESIGN section 7). Suffix the key in exactly that case
    // so the two rows do not look like an accidental duplicate.
    let mut keys_per_currency: BTreeMap<&str, std::collections::BTreeSet<&str>> = BTreeMap::new();
    for r in rows {
        keys_per_currency
            .entry(r.key.as_str())
            .or_default()
            .insert(r.currency.as_str());
    }
    let display_key = |r: &UsageRow| -> String {
        if keys_per_currency
            .get(r.key.as_str())
            .is_some_and(|c| c.len() > 1)
        {
            format!("{} [{}]", r.key, r.currency)
        } else {
            r.key.clone()
        }
    };

    let mut out = String::new();
    out.push_str(&format!(
        "USAGE since {} (grouped by {})\n",
        stamp(since),
        dims.iter()
            .map(|d| d.column_header().to_ascii_lowercase())
            .collect::<Vec<_>>()
            .join(", ")
    ));
    out.push_str(&format!(
        "{:<key_width$}  {:>9}  {:>8}  {:>8}  {:>8}  {}\n",
        header, "REQUESTS", "IN", "OUT", "CACHED", "COST"
    ));
    for r in rows {
        out.push_str(&format!(
            "{:<key_width$}  {:>9}  {:>8}  {:>8}  {:>8}  {} {}\n",
            display_key(r), r.requests, r.input_tokens, r.output_tokens, r.cached_tokens,
            short(r.cost), r.currency
        ));
    }

    // Totals are per currency. Adding CNY to USD would be meaningless, and
    // DESIGN section 7 calls that out explicitly.
    let mut totals: BTreeMap<String, (u64, u64, u64, u64, Decimal)> = BTreeMap::new();
    for r in rows {
        let t = totals.entry(r.currency.clone()).or_default();
        t.0 += r.requests;
        t.1 += r.input_tokens;
        t.2 += r.output_tokens;
        t.3 += r.cached_tokens;
        t.4 += r.cost;
    }
    if rows.len() > 1 && !totals.is_empty() {
        out.push('\n');
        for (currency, (requests, input, output, cached, cost)) in totals {
            out.push_str(&format!(
                "TOTAL {currency}: {requests} requests, in {input}, out {output}, cached {cached}, cost {} {currency}\n",
                short(cost)
            ));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// tail
// ---------------------------------------------------------------------------

fn cmd_tail(path: &Path, limit: u32) -> Result<()> {
    let config = load_config(path)?;
    let conn = open_ledger(&config.db_path())?;
    let rows = store::recent_requests(&conn, limit)?;
    if rows.is_empty() {
        println!("no requests recorded yet in {}", config.db_path().display());
        return Ok(());
    }
    print!("{}", render_tail(&rows));
    Ok(())
}

fn render_tail(rows: &[RequestSummary]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{:<8}  {:<9}  {:<12}  {:<10}  {:<28}  {:>7}  {:>7}  {:>7}  {:>8}  {:>7}  {}\n",
        "TIME", "STATUS", "PROVIDER", "ACCOUNT", "MODEL", "IN", "OUT", "CACHED", "TTFT", "TPS", "COVERAGE"
    ));
    for r in rows {
        let ttft = r
            .ttft_ms
            .map(|ms| format!("{ms}ms"))
            .unwrap_or_else(|| "-".to_string());
        let tps = r
            .tps
            .map(|t| format!("{t:.1}"))
            .unwrap_or_else(|| "-".to_string());
        out.push_str(&format!(
            "{:<8}  {:<9}  {:<12}  {:<10}  {:<28}  {:>7}  {:>7}  {:>7}  {:>8}  {:>7}  {}\n",
            clock(r.ts),
            status_label(r.status),
            r.provider,
            r.account_id,
            truncate(&r.model, 28),
            r.input_tokens,
            r.output_tokens,
            r.cached_tokens,
            ttft,
            tps,
            coverage_label(r.coverage),
        ));
    }
    out
}

/// UTC stamp for headers: "2026-09-04 14:18:13Z".
fn stamp(t: OffsetDateTime) -> String {
    const STAMP: &[time::format_description::FormatItem<'_>] =
        time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second]Z");
    t.format(STAMP).unwrap_or_else(|_| t.to_string())
}

fn clock(t: OffsetDateTime) -> String {
    const HMS: &[time::format_description::FormatItem<'_>] =
        time::macros::format_description!("[hour]:[minute]:[second]");
    t.time().format(HMS).unwrap_or_else(|_| "--:--:--".to_string())
}

fn status_label(s: RequestStatus) -> &'static str {
    match s {
        RequestStatus::Ok => "ok",
        RequestStatus::Error => "error",
        RequestStatus::Aborted => "aborted",
    }
}

fn coverage_label(c: GateClass) -> &'static str {
    match c {
        GateClass::Covered => "covered",
        GateClass::Metered => "metered",
        GateClass::RateLimit => "rate_limit",
    }
}

fn truncate(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    let head: String = s.chars().take(width.saturating_sub(1)).collect();
    format!("{head}…")
}

// ---------------------------------------------------------------------------
// accounts
// ---------------------------------------------------------------------------

async fn cmd_accounts(path: &Path, args: AccountsArgs) -> Result<()> {
    match args.command {
        AccountsCommand::List => cmd_status(path, false).await,
        AccountsCommand::Probe { account } => cmd_accounts_probe(path, account).await,
        AccountsCommand::Disable { account } => cmd_accounts_disable(path, &account),
    }
}

async fn cmd_accounts_probe(path: &Path, account: Option<String>) -> Result<()> {
    let config = load_config(path)?;
    let target_provider = match &account {
        Some(id) => Some(provider_of(&config, id)?),
        None => None,
    };

    // Gateway::new probes every account once, then we probe the named one again
    // so a single-account probe is authoritative rather than a cached reading.
    let gateway = Gateway::new(config).await?;
    if let (Some(provider), Some(id)) = (&target_provider, &account) {
        println!("probing {provider}/{id} ...");
        gateway.probe_account(provider, id).await;
    }
    let rows: Vec<AccountOverview> = gateway.snapshot().iter().map(overview_of).collect();
    print!("{}", render_board(&rows, OffsetDateTime::now_utc()));
    let _ = gateway.shutdown();
    Ok(())
}

/// Find the provider that owns an account id, refusing to guess when the id is
/// ambiguous or unknown.
fn provider_of(config: &Config, account_id: &str) -> Result<String> {
    let matches: Vec<&str> = config
        .providers
        .iter()
        .filter(|(_, p)| p.accounts.contains_key(account_id))
        .map(|(name, _)| name.as_str())
        .collect();
    match matches.as_slice() {
        [] => bail!(
            "no account {account_id:?} in the configuration (provider account ids are the \
             table keys under [providers.<name>.accounts])"
        ),
        [only] => Ok((*only).to_string()),
        many => bail!(
            "account id {account_id:?} is ambiguous; it exists under providers: {}",
            many.join(", ")
        ),
    }
}

fn cmd_accounts_disable(path: &Path, account_id: &str) -> Result<()> {
    let config = load_config(path)?;
    let provider = provider_of(&config, account_id)?;
    // Deliberately unresolved: disabling an account must work even when its
    // credential is missing, which is exactly when you want to disable it.
    let accounts = config.build_accounts_unresolved()?;
    let pool = Pool::new(accounts, &config.policy);

    // There is no dedicated "operator disabled" transition in the frozen pool
    // API; an account that can never be billed again is exactly the Disabled
    // state that insufficient credit produces, so it is used here and the
    // reason is rewritten to say what really happened.
    pool.note_error(
        account_id,
        UpstreamErrorClass::InsufficientCredit,
        None,
        None,
    );
    let mut snapshot = pool.snapshot();
    let Some(account) = snapshot.iter_mut().find(|a| a.id == account_id) else {
        bail!("account {account_id:?} disappeared while disabling it");
    };
    account.last_error = Some("disabled by operator (ai-gw accounts disable)".to_string());

    let db = config.db_path();
    let handle = store::StoreHandle::spawn(&db)?;
    handle.save_accounts(&snapshot);
    handle.flush_blocking()?;

    println!("{provider}/{account_id}: DISABLED (persisted to {})", db.display());
    println!(
        "note: a running ai-gw process keeps its own in-memory pool; restart ai-gw serve \
         for this to take effect there"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

/// Which credential backend to use.
///
/// AI_GW_CREDENTIAL_BACKEND=memory forces an ephemeral store, so tests, dry runs
/// and CI never read or write the operator's real OS keychain.
fn credential_store() -> CredentialStore {
    match std::env::var("AI_GW_CREDENTIAL_BACKEND").as_deref() {
        Ok("memory") => CredentialStore::memory(),
        _ => CredentialStore::keychain(),
    }
}

async fn cmd_doctor(path: &Path, origin: ConfigOrigin) -> Result<()> {
    let mut problems = 0u32;
    println!("config path : {} (from {})", path.display(), origin.label());

    let config = match Config::load(path) {
        Ok(c) => {
            println!("config file : parsed ok");
            c
        }
        Err(e) => {
            println!("config file : FAILED");
            println!("              {e:#}");
            bail!("configuration is unusable; nothing else could be checked");
        }
    };

    let creds = credential_store();

    let accounts: usize = config.providers.values().map(|p| p.accounts.len()).sum();
    println!(
        "providers   : {} ({} accounts, {} routes)",
        config.providers.len(),
        accounts,
        config.routes.len()
    );
    println!(
        "policy      : soft={:.2} allow_metered={} probe_before_switch={} max_retries={}",
        config.policy.soft_threshold,
        config.policy.allow_metered,
        config.policy.probe_before_switch,
        config.policy.max_retries
    );
    if config.policy.allow_metered {
        println!(
            "              WARNING: allow_metered is on; requests may silently spend wallet \
             or purchased credits"
        );
    }

    let db = config.db_path();
    println!(
        "database    : {} ({})",
        db.display(),
        if store::db_exists(&db) {
            "present"
        } else {
            "not created yet"
        }
    );

    println!("credentials :");
    for (provider, p) in &config.providers {
        if p.accounts.is_empty() {
            println!("  {provider}: no accounts configured");
            problems += 1;
        }
        for (id, a) in &p.accounts {
            let secret_ref = a.key.clone().unwrap_or(SecretRef::Keychain);
            let source = secret_ref.describe();
            match secret_ref.resolve(&creds, provider, id) {
                // The key itself is never printed; ApiKey redacts Debug/Display
                // and this path never formats it at all.
                Ok(_) => println!("  {provider}/{id}: ok ({source})"),
                Err(e) => {
                    println!("  {provider}/{id}: MISSING ({source}) - {e:#}");
                    problems += 1;
                }
            }
        }
    }

    println!("probes      :");
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(
            config.policy.connect_timeout_secs.max(1),
        ))
        .build()
        .context("building the probe HTTP client")?;

    let mut targets: HashMap<String, (QuotaAdapter, String)> = HashMap::new();
    for (provider, adapter, base) in config.probe_targets() {
        targets.insert(provider, (adapter, base));
    }

    match config.build_accounts(&creds) {
        Err(e) => {
            println!("  skipped: {e:#}");
            println!("  (fix the missing credentials above, then re-run doctor)");
        }
        Ok(built) => {
            for account in &built {
                let label = format!("{}/{}", account.provider, account.id);
                match targets.get(&account.provider) {
                    None => println!("  {label}: no probe target configured"),
                    Some((QuotaAdapter::None, _)) => {
                        println!("  {label}: local ledger only (adapter = none)")
                    }
                    Some((adapter, base)) => match probe_for(*adapter, base) {
                        None => println!("  {label}: no adapter for {adapter:?}"),
                        Some(probe) => match probe.probe(account, &client).await {
                            Ok(outcome) => {
                                println!("  {label}: ok ({} gates)", outcome.gates.len())
                            }
                            Err(e) => {
                                println!("  {label}: UNREACHABLE - {}", e.message);
                                problems += 1;
                            }
                        },
                    },
                }
            }
        }
    }

    if problems == 0 {
        println!("doctor      : no problems found");
        Ok(())
    } else {
        bail!("doctor found {problems} problem(s)")
    }
}

// ---------------------------------------------------------------------------
// init
// ---------------------------------------------------------------------------

fn cmd_init(path: &Path, force: bool) -> Result<()> {
    if path.exists() && !force {
        bail!(
            "{} already exists; pass --force to overwrite it",
            path.display()
        );
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
    std::fs::write(path, crate::config::sample_toml())
        .with_context(|| format!("writing {}", path.display()))?;
    println!("wrote example config to {}", path.display());
    println!("next: set the credentials it references, then run ai-gw doctor");
    Ok(())
}


// ---------------------------------------------------------------------------
// keys
// ---------------------------------------------------------------------------

async fn cmd_keys(path: &Path, args: KeysArgs) -> Result<()> {
    let config = load_config(path)?;
    let creds = credential_store();
    match args.command {
        KeysCommand::Add {
            provider,
            account,
            stdin,
        } => cmd_keys_add(&config, &creds, &provider, &account, stdin),
        KeysCommand::List => cmd_keys_list(&config, &creds),
        KeysCommand::Remove { provider, account } => cmd_keys_remove(&creds, &provider, &account),
        KeysCommand::Test { provider, account } => {
            cmd_keys_test(&config, &creds, &provider, &account).await
        }
    }
}

/// Read the secret from a hidden prompt, or from stdin when asked.
///
/// Deliberately not a command-line flag: an argument is visible to every process
/// on the machine via ps, and is written to shell history.
fn read_secret(provider: &str, account: &str, from_stdin: bool) -> Result<String> {
    let secret = if from_stdin {
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .context("reading the key from stdin")?;
        line
    } else {
        rpassword::prompt_password(format!(
            "API key for {provider}/{account} (input hidden): "
        ))
        .context("reading the key from the terminal")?
    };
    let trimmed = secret.trim().to_string();
    if trimmed.is_empty() {
        bail!("no key was entered");
    }
    Ok(trimmed)
}

fn cmd_keys_add(
    config: &Config,
    creds: &CredentialStore,
    provider: &str,
    account: &str,
    from_stdin: bool,
) -> Result<()> {
    // The credential is looked up by provider/account, so both must exist in the
    // configuration or the entry could never be reached.
    let p = config
        .providers
        .get(provider)
        .with_context(|| {
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

    let secret = read_secret(provider, account, from_stdin)?;
    creds
        .set(provider, account, &secret)
        .with_context(|| format!("storing the credential for {provider}/{account}"))?;

    // Read it back: a store that silently drops writes is worse than one that
    // fails, because the operator only discovers it at request time.
    match creds.get(provider, account)? {
        Some(stored) if stored.expose() == secret => {
            println!(
                "stored  {provider}/{account} in {} ({})",
                creds.backend_name(),
                crate::secrets::describe_secret(&secret)
            );
            Ok(())
        }
        _ => bail!(
            "the credential was written but could not be read back from {}; \
             the store may be unavailable",
            creds.backend_name()
        ),
    }
}

/// One row of the credential report. Pure, so it can be tested without
/// capturing stdout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialRow {
    pub provider: String,
    pub account: String,
    /// Where the secret comes from, never the secret itself.
    pub source: String,
    pub status: String,
}

fn credential_rows(config: &Config, creds: &CredentialStore) -> Vec<CredentialRow> {
    let mut rows = Vec::new();
    for (provider, p) in &config.providers {
        for (id, a) in &p.accounts {
            let secret_ref = a.key.clone().unwrap_or(SecretRef::Keychain);
            let status = match secret_ref.resolve(creds, provider, id) {
                Ok(_) => "ok".to_string(),
                Err(e) => format!("MISSING ({e})"),
            };
            rows.push(CredentialRow {
                provider: provider.clone(),
                account: id.clone(),
                source: secret_ref.describe(),
                status,
            });
        }
    }
    rows
}

fn cmd_keys_list(config: &Config, creds: &CredentialStore) -> Result<()> {
    println!("backend: {}", creds.backend_name());
    println!();
    println!("{:<16} {:<20} {:<22} STATUS", "PROVIDER", "ACCOUNT", "SOURCE");
    for row in credential_rows(config, creds) {
        println!(
            "{:<16} {:<20} {:<22} {}",
            row.provider, row.account, row.source, row.status
        );
    }
    println!();
    println!("Register one with: ai-gw keys add <provider> <account>");
    Ok(())
}

fn cmd_keys_remove(creds: &CredentialStore, provider: &str, account: &str) -> Result<()> {
    if creds.delete(provider, account)? {
        println!("removed {provider}/{account} from {}", creds.backend_name());
    } else {
        println!("nothing stored for {provider}/{account}");
    }
    Ok(())
}

/// Resolve the stored credential and prove it works against the provider.
async fn cmd_keys_test(
    config: &Config,
    creds: &CredentialStore,
    provider: &str,
    account: &str,
) -> Result<()> {
    let p = config
        .providers
        .get(provider)
        .with_context(|| format!("unknown provider {provider:?}"))?;
    let account_cfg = p
        .accounts
        .get(account)
        .with_context(|| format!("unknown account {account:?} under provider {provider:?}"))?;

    // Build just this one account so the test cannot be satisfied by another
    // account's credential.
    let secret_ref = account_cfg.key.clone().unwrap_or(SecretRef::Keychain);
    let key = secret_ref
        .resolve(creds, provider, account)
        .with_context(|| format!("resolving the credential for {provider}/{account}"))?;

    let Some(probe) = crate::quota::probe_for(p.adapter, p.probe_base_url.as_deref().unwrap_or(&p.base_url)) else {
        println!("{provider}/{account}: credential found ({} chars), but this provider has no quota probe to test against",
            key.expose().len());
        return Ok(());
    };

    let account_model = crate::domain::Account {
        id: account.to_string(),
        provider: provider.to_string(),
        label: account_cfg.label.clone().unwrap_or_else(|| account.to_string()),
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
        last_probe_at: None,
        last_error: None,
    };

    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(
            config.policy.connect_timeout_secs.max(1),
        ))
        .build()
        .context("building the probe HTTP client")?;

    match probe.probe(&account_model, &client).await {
        Ok(outcome) => {
            println!(
                "{provider}/{account}: ok - {} credentials accepted, {} gates read",
                probe.name(),
                outcome.gates.len()
            );
            for g in &outcome.gates {
                println!(
                    "    {:<18} {}/{} {:?}",
                    g.kind.label(),
                    g.used,
                    g.cap,
                    g.class()
                );
            }
            Ok(())
        }
        Err(e) => {
            println!("{provider}/{account}: FAILED - {e}");
            bail!("the stored credential was rejected or the probe endpoint is unreachable")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::GateSource;
    use std::str::FromStr;

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).expect("valid decimal")
    }

    // -- credentials ---------------------------------------------------------

    /// A config exercising every credential source except the real keychain.
    /// Uses a key_file rather than an environment variable: mutating process-wide
    /// env in a test is racy across threads and needs unsafe in edition 2024.
    fn keys_config(key_file: &str) -> Config {
        Config::from_toml_str(&format!(
            r#"
[providers.xkiro]
adapter = "xkiro"
base_url = "https://api.xkiro.com/v1"
[providers.xkiro.accounts.from-store]
[providers.xkiro.accounts.from-file]
key = {{ key_file = "{key_file}" }}
[routes]
"m" = "xkiro"
"#
        ))
        .unwrap()
    }

    fn temp_key_file(contents: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("lai-gw-key-{}.txt", uuid::Uuid::new_v4()));
        std::fs::write(&p, contents).unwrap();
        p
    }

    #[test]
    fn credential_rows_report_source_and_status_without_the_secret() {
        let path = temp_key_file("sk-envvalue");
        let config = keys_config(path.to_str().unwrap());
        let creds = CredentialStore::memory();
        creds.set("xkiro", "from-store", "sk-abcdefghij").unwrap();

        let rows = credential_rows(&config, &creds);
        assert_eq!(rows.len(), 2);

        let stored = rows.iter().find(|r| r.account == "from-store").unwrap();
        assert_eq!(stored.source, "credential-store");
        assert_eq!(stored.status, "ok");

        let file = rows.iter().find(|r| r.account == "from-file").unwrap();
        assert!(file.source.starts_with("file:"), "got {}", file.source);
        assert_eq!(file.status, "ok");

        // The rendered output must never contain either secret.
        let rendered = format!("{rows:?}");
        assert!(!rendered.contains("sk-abcdefghij"));
        assert!(!rendered.contains("sk-envvalue"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_credential_is_reported_as_missing_not_as_a_pass() {
        let path = temp_key_file("unused");
        let config = keys_config(path.to_str().unwrap());
        let creds = CredentialStore::memory();
        let rows = credential_rows(&config, &creds);
        let stored = rows.iter().find(|r| r.account == "from-store").unwrap();
        assert!(stored.status.starts_with("MISSING"), "got {}", stored.status);
        // And it names the exact command that fixes it.
        assert!(stored.status.contains("ai-gw keys add xkiro from-store"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn keys_add_rejects_an_unknown_provider_before_reading_a_secret() {
        let config = keys_config("/nonexistent");
        let creds = CredentialStore::memory();
        // from_stdin is irrelevant: validation must fail first.
        let err = cmd_keys_add(&config, &creds, "nope", "acc", true).unwrap_err();
        assert!(err.to_string().contains("unknown provider"), "got: {err}");
    }

    #[test]
    fn keys_add_rejects_an_unknown_account_and_lists_the_known_ones() {
        let config = keys_config("/nonexistent");
        let creds = CredentialStore::memory();
        let err = cmd_keys_add(&config, &creds, "xkiro", "nope", true).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown account"), "got: {msg}");
        assert!(msg.contains("from-store"), "should list known accounts: {msg}");
    }

    #[test]
    fn keys_remove_is_idempotent() {
        let creds = CredentialStore::memory();
        creds.set("xkiro", "acc", "sk-x").unwrap();
        assert!(cmd_keys_remove(&creds, "xkiro", "acc").is_ok());
        assert!(cmd_keys_remove(&creds, "xkiro", "acc").is_ok());
        assert!(creds.get("xkiro", "acc").unwrap().is_none());
    }


    fn gate(
        kind: GateKind,
        cap: &str,
        used: &str,
        resets_in_secs: Option<i64>,
        confidence: Confidence,
        stale: bool,
        now: OffsetDateTime,
    ) -> Gate {
        Gate {
            kind,
            cap: dec(cap),
            used: dec(used),
            resets_at: resets_in_secs.map(|s| now + time::Duration::seconds(s)),
            confidence,
            source: GateSource::Polled,
            checked_at: now,
            stale,
        }
    }

    fn overview(
        provider: &str,
        id: &str,
        currency: &str,
        state: AccountState,
        gates: Vec<Gate>,
    ) -> AccountOverview {
        AccountOverview {
            account_id: id.to_string(),
            provider: provider.to_string(),
            label: id.to_string(),
            currency: currency.to_string(),
            state,
            gates,
            available_at: None,
            window_started_at: None,
            last_error: None,
        }
    }

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    // ------------------------------------------------------------- config path

    #[test]
    fn config_path_precedence_is_flag_env_local_home() {
        let flag = PathBuf::from("/tmp/flag.toml");
        let env = PathBuf::from("/tmp/env.toml");
        let local = PathBuf::from("ai-gw.toml");
        let home = PathBuf::from("/tmp/home/config.toml");

        assert_eq!(
            pick_config_path(Some(flag.clone()), Some(env.clone()), local.clone(), true, home.clone()),
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
        // A ./ai-gw.toml that does not exist must not win over the home default.
        assert_eq!(
            pick_config_path(None, None, local.clone(), false, home.clone()),
            (home.clone(), ConfigOrigin::Home)
        );
    }

    // ------------------------------------------------------------------ board

    #[test]
    fn board_keeps_covered_and_metered_visually_distinct() {
        let t = now();
        let acct = overview(
            "commandcode",
            "cc-1",
            "USD",
            AccountState::Draining,
            vec![
                gate(GateKind::FiveHour, "100", "40", Some(4_200), Confidence::Exact, false, t),
                gate(GateKind::Weekly, "500", "25", None, Confidence::Exact, false, t),
                gate(
                    GateKind::PurchasedCredits,
                    "12",
                    "0",
                    None,
                    Confidence::Exact,
                    false,
                    t,
                ),
            ],
        );

        let covered = covered_cell(&acct);
        let metered = metered_cell(&acct);
        assert_eq!(covered, "40/100 25/500 USD", "covered shows used/cap and its unit");
        assert_eq!(
            metered, "12 USD(unused)",
            "metered shows the money left and says it is untouched"
        );
        assert_ne!(covered, metered);

        let board = render_board(&[acct], t);
        assert!(board.contains("COVERED"));
        assert!(board.contains("METERED"));
        assert!(board.contains("40/100 25/500 USD"));
        assert!(board.contains("12 USD(unused)"));
        assert!(board.contains("DRAINING"));
        assert!(
            board.contains("metered = real money remaining"),
            "the legend must explain the two columns: {board}"
        );
    }

    #[test]
    fn board_shows_metered_only_accounts_without_a_covered_reading() {
        let t = now();
        let acct = overview(
            "deepseek",
            "ds-1",
            "USD",
            AccountState::Active,
            vec![gate(
                GateKind::Wallet,
                "12.34",
                "0",
                None,
                Confidence::Exact,
                false,
                t,
            )],
        );
        assert_eq!(covered_cell(&acct), "-", "covered must be blank, not 0/0");
        assert_eq!(metered_cell(&acct), "12.34 USD(unused)");
        assert!(state_cell(&acct).contains("metered only"));
        assert_eq!(reset_cell(&acct, t), "-", "a wallet has no reset");
    }

    #[test]
    fn board_reports_spent_metered_money_rather_than_claiming_it_is_unused() {
        let t = now();
        let acct = overview(
            "xkiro",
            "xk-1",
            "USD",
            AccountState::Active,
            vec![gate(
                GateKind::Wallet,
                "700",
                "16.05",
                None,
                Confidence::Exact,
                false,
                t,
            )],
        );
        assert_eq!(metered_cell(&acct), "683.95 USD(16.05 spent)");
    }

    #[test]
    fn board_marks_stale_and_estimated_readings() {
        let t = now();
        let estimated = overview(
            "xkiro",
            "xk-1",
            "USD",
            AccountState::Active,
            vec![gate(
                GateKind::FiveHour,
                "200",
                "178",
                Some(600),
                Confidence::Estimated,
                false,
                t,
            )],
        );
        assert!(covered_cell(&estimated).ends_with(" ~EST"));

        let stale = overview(
            "xkiro",
            "xk-2",
            "USD",
            AccountState::Active,
            vec![gate(
                GateKind::FiveHour,
                "200",
                "178",
                Some(600),
                Confidence::Estimated,
                true,
                t,
            )],
        );
        let cell = covered_cell(&stale);
        assert!(cell.ends_with(" STALE"), "got {cell}");
        assert!(
            !cell.contains("~EST"),
            "a stale reading is not presented as merely estimated"
        );
        let board = render_board(&[stale], t);
        assert!(
            board.lines().any(|l| l.starts_with("xkiro") && l.contains("STALE")),
            "the stale row itself must carry the marker: {board}"
        );
    }

    #[test]
    fn board_prints_a_reset_countdown() {
        let t = now();
        let acct = overview(
            "xkiro",
            "xk-1",
            "USD",
            AccountState::Active,
            vec![
                gate(GateKind::FiveHour, "200", "178", Some(4_200), Confidence::Exact, false, t),
                gate(GateKind::Weekly, "1000", "100", Some(90_000), Confidence::Exact, false, t),
            ],
        );
        // The earliest reset wins.
        assert_eq!(reset_cell(&acct, t), "1h10m");
        assert!(render_board(&[acct], t).contains("1h10m"));
    }

    #[test]
    fn countdown_formatting_is_readable_at_every_scale() {
        let t = now();
        assert_eq!(format_countdown(t, t + time::Duration::seconds(45)), "45s");
        assert_eq!(format_countdown(t, t + time::Duration::seconds(600)), "10m");
        assert_eq!(format_countdown(t, t + time::Duration::seconds(4_200)), "1h10m");
        assert_eq!(format_countdown(t, t + time::Duration::seconds(93_600)), "1d2h");
        assert_eq!(format_countdown(t, t - time::Duration::seconds(5)), "now");
    }

    #[test]
    fn account_with_no_gates_is_marked_as_having_no_reading() {
        let acct = overview("xkiro", "xk-1", "USD", AccountState::Unknown, vec![]);
        assert_eq!(covered_cell(&acct), "-");
        assert_eq!(metered_cell(&acct), "-");
        assert!(state_cell(&acct).contains("no reading"));
        assert!(!state_cell(&acct).contains("metered only"));
    }

    #[test]
    fn disabled_account_shows_its_reason() {
        let mut acct = overview("xkiro", "xk-1", "USD", AccountState::Disabled, vec![]);
        acct.last_error = Some("auth".to_string());
        assert_eq!(state_cell(&acct), "DISABLED (auth)");
    }

    // ------------------------------------------------------------------ usage

    #[test]
    fn group_by_parses_a_comma_separated_list() {
        assert_eq!(parse_group_by("provider").unwrap(), vec![GroupBy::Provider]);
        assert_eq!(
            parse_group_by("provider,model,account").unwrap(),
            vec![GroupBy::Provider, GroupBy::Model, GroupBy::Account]
        );
        // Whitespace and repeated dimensions are tolerated.
        assert_eq!(
            parse_group_by(" model , model ").unwrap(),
            vec![GroupBy::Model]
        );
        assert_eq!(parse_group_by("").unwrap(), vec![GroupBy::Provider]);
        assert!(parse_group_by("provider,session").is_err());
    }

    #[test]
    fn since_accepts_durations_timestamps_and_dates() {
        let t = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        assert_eq!(parse_since("7d", t).unwrap(), t - time::Duration::days(7));
        assert_eq!(parse_since("24h", t).unwrap(), t - time::Duration::hours(24));
        assert_eq!(parse_since("90m", t).unwrap(), t - time::Duration::minutes(90));
        assert_eq!(parse_since("2w", t).unwrap(), t - time::Duration::weeks(2));

        assert_eq!(
            parse_since("2023-11-14T22:13:20Z", t).unwrap(),
            t,
            "an RFC3339 timestamp is used as-is"
        );
        assert_eq!(
            parse_since("2023-11-14", t).unwrap().unix_timestamp(),
            1_699_920_000
        );

        assert!(parse_since("", t).is_err());
        assert!(parse_since("soon", t).is_err());
        assert!(parse_since("7", t).is_err(), "a bare number has no unit");
    }

    #[test]
    fn usage_totals_are_kept_per_currency() {
        let rows = vec![
            UsageRow {
                key: "xkiro".into(),
                requests: 2,
                input_tokens: 200,
                output_tokens: 40,
                cached_tokens: 80,
                cost: dec("0.75"),
                currency: "USD".into(),
            },
            UsageRow {
                key: "deepseek".into(),
                requests: 1,
                input_tokens: 10,
                output_tokens: 5,
                cached_tokens: 0,
                cost: dec("0.5"),
                currency: "CNY".into(),
            },
        ];
        let text = render_usage(&rows, &[GroupBy::Provider], now());
        assert!(text.contains("USAGE since"));
        assert!(text.contains("PROVIDER"));
        assert!(text.contains("0.75 USD"));
        assert!(text.contains("0.5 CNY"));
        assert!(text.contains("TOTAL USD: 2 requests"));
        assert!(text.contains("TOTAL CNY: 1 requests"));
        assert!(
            !text.contains("1.25"),
            "USD and CNY must never be added together: {text}"
        );
        assert!(text.contains("2023-11-14 22:13:20Z"), "got: {text}");
    }

    #[test]
    fn a_key_billed_in_two_currencies_is_disambiguated() {
        let rows = vec![
            UsageRow {
                key: "m".into(),
                requests: 2,
                input_tokens: 2_100,
                output_tokens: 420,
                cached_tokens: 1_024,
                cost: dec("0.0053"),
                currency: "USD".into(),
            },
            UsageRow {
                key: "m".into(),
                requests: 1,
                input_tokens: 10,
                output_tokens: 5,
                cached_tokens: 0,
                cost: dec("0.000356"),
                currency: "CNY".into(),
            },
        ];
        let text = render_usage(&rows, &[GroupBy::Model], now());
        assert!(text.contains("m [USD]"), "got: {text}");
        assert!(text.contains("m [CNY]"), "got: {text}");

        // A key seen in one currency only keeps its bare name.
        let single = vec![UsageRow {
            key: "m".into(),
            requests: 1,
            input_tokens: 1,
            output_tokens: 1,
            cached_tokens: 0,
            cost: dec("0.1"),
            currency: "USD".into(),
        }];
        assert!(!render_usage(&single, &[GroupBy::Model], now()).contains("m [USD]"));
    }

    // ------------------------------------------------------------------- tail

    #[test]
    fn tail_renders_one_row_per_request() {
        let rows = vec![RequestSummary {
            ts: now(),
            request_id: "r1".into(),
            provider: "xkiro".into(),
            account_id: "xk-1".into(),
            model: "deepseek/deepseek-v3.2".into(),
            input_tokens: 1200,
            output_tokens: 300,
            cached_tokens: 1024,
            ttft_ms: Some(850),
            tps: Some(45.25),
            coverage: GateClass::Covered,
            status: RequestStatus::Ok,
        }];
        let text = render_tail(&rows);
        assert!(text.contains("TIME"));
        assert!(text.contains("COVERAGE"));
        assert!(text.contains("xkiro"));
        assert!(text.contains("850ms"));
        assert!(text.contains("45.2"));
        assert!(text.contains("covered"));
    }

    // ------------------------------------------------------------------- init

    #[test]
    fn init_writes_the_sample_and_refuses_to_clobber() {
        let dir = std::env::temp_dir().join(format!(
            "lai-gw-cli-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        cmd_init(&path, false).unwrap();
        assert!(path.exists());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            crate::config::sample_toml()
        );
        // The written sample must be loadable as-is (no live credentials are
        // needed to parse it, only to build accounts).
        Config::load(&path).expect("the sample config must parse");

        let err = cmd_init(&path, false).unwrap_err().to_string();
        assert!(err.contains("--force"), "got: {err}");

        std::fs::write(&path, "clobbered").unwrap();
        cmd_init(&path, true).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            crate::config::sample_toml()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_origin_labels_name_the_source() {
        assert_eq!(ConfigOrigin::Flag.label(), "--config");
        assert_eq!(ConfigOrigin::Env.label(), "$AI_GW_CONFIG");
        assert_eq!(ConfigOrigin::Local.label(), "./ai-gw.toml");
        assert_eq!(ConfigOrigin::Home.label(), "~/.light-ai-gateway/config.toml");
    }

    #[test]
    fn ambiguous_account_ids_are_refused() {
        let config = Config::from_toml_str(
            r#"
[providers.a]
adapter = "none"
base_url = "https://a.test/v1"
[providers.a.accounts.shared]
key = "sk-a"
[providers.b]
adapter = "none"
base_url = "https://b.test/v1"
[providers.b.accounts.shared]
key = "sk-b"
[providers.b.accounts.only-b]
key = "sk-b2"
[routes]
"m" = "a"
"#,
        )
        .unwrap();
        assert_eq!(provider_of(&config, "only-b").unwrap(), "b");
        let err = provider_of(&config, "shared").unwrap_err().to_string();
        assert!(err.contains("ambiguous"), "got: {err}");
        let err = provider_of(&config, "nope").unwrap_err().to_string();
        assert!(err.contains("no account"), "got: {err}");
    }
}
