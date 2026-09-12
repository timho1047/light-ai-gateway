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

use crate::config::{Config, QuotaAdapter, SecretRef};
use crate::domain::{
    Account, AccountOverview, AccountState, Confidence, Gate, GateClass, GateKind, GroupBy,
    RequestStatus, RequestSummary, UsageRow,
};
use crate::quota::probe_for;
use crate::router::Gateway;
use crate::secrets::CredentialStore;
use crate::store;
use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use rusqlite::Connection;
use rust_decimal::Decimal;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use time::OffsetDateTime;

/// Where the configuration file was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigOrigin {
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
    /// Show configured prices against the prices learned from real spend.
    Prices(PricesArgs),
}

#[derive(Debug, Args)]
pub struct PricesArgs {
    /// Show only models whose observed price has moved away from the configured one.
    #[arg(long)]
    pub drift_only: bool,
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
    /// Fold every per-account credential-store item into the single vault item.
    ///
    /// On macOS an ad-hoc signed binary is identified by the hash of its code, so
    /// an "Always Allow" grant cannot survive a rebuild. Fewer items is the only
    /// durable answer, and this turns N authorisation prompts into one.
    Migrate {
        /// Delete the per-account items once the vault is confirmed to hold them.
        #[arg(long)]
        prune: bool,
    },
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
    /// Put a disabled account back in the pool, after a probe proves it works.
    Enable {
        /// Account id to enable.
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
                "lgw: starting proxy from {} ({})",
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
        Command::Prices(args) => cmd_prices(&path, &args),
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
fn open_ledger(db: &Path) -> Result<Connection> {
    if !store::db_exists(db) {
        bail!(
            "no ledger at {} yet. Run `lgw serve` once (or `lgw status --refresh`) \
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
            "no accounts in the ledger yet at {}; run lgw status --refresh",
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

const BOARD_HEADERS: [(&str, crate::table::Align); 6] = [
    ("PROVIDER", crate::table::Align::Left),
    ("ACCOUNT", crate::table::Align::Left),
    ("COVERED", crate::table::Align::Left),
    ("METERED", crate::table::Align::Left),
    ("RESETS", crate::table::Align::Right),
    ("STATE", crate::table::Align::Left),
];

/// Render the account board.
///
/// COVERED shows what has been consumed out of a prepaid allowance (used/cap).
/// METERED shows the money that is left, and says plainly when none of it has
/// been touched. The two are deliberately different shapes so that an operator
/// scanning the table cannot mistake one for the other.
///
/// Both are composite columns: several readings, each with its own label, plus a
/// unit. Their sub-columns are measured across the whole board (see BoardWidths)
/// rather than per cell, so a fraction, a window label and a currency code line
/// up down the page even though accounts hold different numbers of windows.
fn render_board(rows: &[AccountOverview], now: OffsetDateTime) -> String {
    // Measured widths, like every other table here. The board used to carry its
    // own [14, 18, 26, 26, 8], and an account whose state explanation ran long --
    // "DISABLED (insufficient_credit)" is exactly 32 -- pulled the columns after
    // it out of line.
    let covered: Vec<Option<Covered>> = rows.iter().map(covered_of).collect();
    let metered: Vec<Option<Metered>> = rows.iter().map(metered_of).collect();
    let covered_refs: Vec<&Covered> = covered.iter().flatten().collect();
    let metered_refs: Vec<&Metered> = metered.iter().flatten().collect();
    let widths = BoardWidths::measure(&covered_refs, &metered_refs);

    let mut table = crate::table::Table::new(&BOARD_HEADERS);
    for (i, r) in rows.iter().enumerate() {
        table.push(vec![
            r.provider.clone(),
            r.account_id.clone(),
            render_or_dash(covered[i].as_ref(), |c| widths.covered(c)),
            render_or_dash(metered[i].as_ref(), |m| widths.metered(m)),
            reset_cell(r, now),
            state_cell(r),
        ]);
    }
    let mut out = table.render();
    out.push('\n');
    out.push_str(
        "covered = prepaid allowance consumed (used/cap, forfeited at reset)   \
         metered = real money remaining (spending it is permanent)\n",
    );
    out.push_str(
        "~ = estimated reading   STALE = last probe failed, previous value carried over\n",
    );
    out
}

/// An account with nothing to show in a composite column gets a dash rather than
/// a blank: a blank cell reads as "nobody looked", a dash as "there is nothing
/// there to look at".
fn render_or_dash<T>(cell: Option<&T>, render: impl Fn(&T) -> String) -> String {
    match cell {
        Some(cell) => render(cell),
        None => "-".to_string(),
    }
}

/// One window of prepaid allowance: its label, what has been consumed, and its cap.
struct Window {
    label: String,
    used: String,
    cap: String,
}

/// The covered column of one account, before the board's shared widths are known.
struct Covered {
    windows: Vec<Window>,
    currency: String,
    marks: String,
}

impl Covered {
    /// The windows on one line, with each sub-column padded to the shared widths.
    ///
    /// The cap is padded on the right, so the slashes of two accounts sit in the
    /// same column and "which of these is closer to its limit" is answered by
    /// reading down instead of by counting characters.
    fn windows_text(&self, w: &BoardWidths) -> String {
        let (lw, uw, cw) = (w.window, w.used, w.cap);
        self.windows
            .iter()
            .map(|win| {
                format!(
                    "{label:<lw$} {used:>uw$}/{cap:<cw$}",
                    label = win.label,
                    used = win.used,
                    cap = win.cap
                )
            })
            .collect::<Vec<_>>()
            .join("  ")
    }
}

/// One money pot: what it is, what is left, and what has happened to it.
struct Pot {
    label: String,
    left: String,
    note: String,
}

/// The metered column of one account, before the board's shared widths are known.
struct Metered {
    pots: Vec<Pot>,
    currency: String,
    marks: String,
}

/// The widths of the sub-columns inside the two composite cells, measured across
/// every row of the board.
///
/// Without this each account brought its own widths: a "1320.1/1320" pushed that
/// row's unit a column right of everyone else's, and two fractions could not be
/// compared by scanning one vertical line. Measuring the sub-columns the way the
/// outer table measures its own columns is what makes the board a table rather
/// than a stack of formatted strings.
#[derive(Clone, Copy, Default)]
struct BoardWidths {
    window: usize,
    used: usize,
    cap: usize,
    pot: usize,
    left: usize,
    /// Width of the window part of the widest row. Rows carry different numbers
    /// of windows, so the unit is another column that has to be lined up rather
    /// than simply appended.
    windows_part: usize,
}

impl BoardWidths {
    fn measure(covered: &[&Covered], metered: &[&Metered]) -> Self {
        let mut w = BoardWidths::default();
        for c in covered {
            for win in &c.windows {
                w.window = w.window.max(win.label.chars().count());
                w.used = w.used.max(win.used.chars().count());
                w.cap = w.cap.max(win.cap.chars().count());
            }
        }
        for m in metered {
            for p in &m.pots {
                w.pot = w.pot.max(p.label.chars().count());
                w.left = w.left.max(p.left.chars().count());
            }
        }
        // A second pass, because the width of the window part depends on the
        // widths just measured.
        for c in covered {
            w.windows_part = w.windows_part.max(c.windows_text(&w).chars().count());
        }
        w
    }

    fn covered(&self, c: &Covered) -> String {
        let mut out = c.windows_text(self);
        // The unit is a column of its own: two spaces past the widest row's
        // windows, so it lands in the same place on every line.
        let gap = 2 + self.windows_part.saturating_sub(out.chars().count());
        out.push_str(&" ".repeat(gap));
        out.push_str(&c.currency);
        out.push_str(&c.marks);
        out
    }

    fn metered(&self, m: &Metered) -> String {
        let (pw, lw) = (self.pot, self.left);
        let pots = m
            .pots
            .iter()
            .map(|p| {
                format!(
                    "{label:<pw$} {left:>lw$} {currency} ({note})",
                    label = p.label,
                    left = p.left,
                    currency = m.currency,
                    note = p.note
                )
            })
            .collect::<Vec<_>>()
            .join("  ");
        format!("{pots}{}", m.marks)
    }
}

/// The covered column: the money-denominated allowances this account still has.
///
/// Only money-denominated gates appear. A gate counted in tokens is a limit on
/// a different thing -- the free-token allowance gates free models, not the paid
/// traffic this board is about -- and printing its raw number next to a currency
/// code claimed that 30000000 tokens were 30000000 dollars. It is hidden rather
/// than converted because an operator paying for every request has no use for
/// it, and an unused allowance nobody spends is not worth a column.
///
/// Every gate carries its own label. Without one the column is a row of bare
/// fractions whose meaning depends on knowing the order, which is exactly how an
/// allowance gets mistaken for a monthly cap.
fn covered_of(r: &AccountOverview) -> Option<Covered> {
    let mut gates: Vec<&Gate> = r
        .gates
        .iter()
        .filter(|g| g.class() == GateClass::Covered && g.kind != GateKind::FreeTokens)
        .collect();
    if gates.is_empty() {
        return None;
    }
    gates.sort_by_key(|g| gate_rank(g.kind));
    let windows = gates
        .iter()
        .map(|g| Window {
            label: g.kind.label().to_string(),
            used: board_amount(g.used),
            cap: board_amount(g.cap),
        })
        .collect();
    Some(Covered {
        windows,
        currency: r.currency.clone(),
        marks: markers(&gates),
    })
}

fn metered_of(r: &AccountOverview) -> Option<Metered> {
    let mut gates: Vec<&Gate> = r
        .gates
        .iter()
        .filter(|g| g.class() == GateClass::Metered)
        .collect();
    if gates.is_empty() {
        return None;
    }
    gates.sort_by_key(|g| gate_rank(g.kind));
    let pots = gates
        .iter()
        .map(|g| Pot {
            label: g.kind.label().to_string(),
            left: board_amount(g.remaining()),
            // Spelled out rather than left to the reader: a balance that has
            // never been touched is the strongest evidence the account is whole,
            // and "0 spent" is a number to decode.
            note: if g.used <= Decimal::ZERO {
                "unused".to_string()
            } else {
                format!("{} spent", board_amount(g.used))
            },
        })
        .collect();
    Some(Metered {
        pots,
        currency: r.currency.clone(),
        marks: markers(&gates),
    })
}

/// The two cells as the board would render them for one account on its own.
///
/// The board itself measures across every row, so these single-row forms exist
/// only for the tests that assert on one cell at a time.
#[cfg(test)]
fn covered_cell(r: &AccountOverview) -> String {
    match covered_of(r) {
        Some(c) => BoardWidths::measure(&[&c], &[]).covered(&c),
        None => "-".to_string(),
    }
}

#[cfg(test)]
fn metered_cell(r: &AccountOverview) -> String {
    match metered_of(r) {
        Some(m) => BoardWidths::measure(&[], &[&m]).metered(&m),
        None => "-".to_string(),
    }
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

/// An amount on the account board, rounded to ONE decimal place.
///
/// The ledger is Decimal arithmetic, so a running balance carries fifteen digits
/// nobody wants to read: 1.113194698/14 is a wall of digits wrapped around the
/// one that matters.
///
/// Two departures from a plain one-decimal rounding, both to avoid printing
/// something untrue:
///   - a small but REAL balance would round to 0 and read as untouched, so
///     anything under 0.05 keeps enough digits to stay visible;
///   - an exact integer keeps no decimal point, so 368 does not become 368.0.
///
/// Full precision stays available in the usage and prices reports; the board is
/// meant to be read at a glance.
fn board_amount(d: Decimal) -> String {
    use rust_decimal::prelude::ToPrimitive;
    let v = d.to_f64().unwrap_or(0.0);
    if v == 0.0 {
        return "0".to_string();
    }

    let trim = |s: &str| -> String {
        let mut s = s.to_string();
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
        if s.is_empty() || s == "-" || s == "0" || s == "-0" {
            "0".to_string()
        } else {
            s
        }
    };

    if v.abs() < 0.05 {
        // Three decimals is enough to show that something was spent.
        let three = trim(&format!("{v:.3}"));
        if three != "0" {
            return three;
        }
        let six = trim(&format!("{v:.6}"));
        if six != "0" {
            return six;
        }
        // Non-zero in the ledger, below what six decimals can show.
        return if v < 0.0 {
            "<-0.000001".to_string()
        } else {
            "<0.000001".to_string()
        };
    }

    trim(&format!("{v:.1}"))
}

// ---------------------------------------------------------------------------
// usage
// ---------------------------------------------------------------------------

/// Rebuild the price table from the config file plus whatever the ledger has
/// learned. Used by every command that reports on prices, so they cannot drift
/// apart.
fn load_price_table(config: &Config) -> crate::meter::PriceTable {
    let mut table = crate::meter::PriceTable::new();
    table.seed_from_config(&config.prices);
    let db = config.db_path();
    if store::db_exists(&db)
        && let Ok(rows) = store::open_readonly(&db).and_then(|conn| store::load_prices(&conn))
    {
        table.load_learned(rows);
    }
    table
}

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

    // Rates in effect, then what each model is actually costing against them.
    // The panel comes first because the drift table is unreadable without it.
    let table = load_price_table(&config);
    println!();
    print_price_panel(&config, &table);

    let names = tier_names(&config);
    let drift_rows = price_rows(&table, &names);
    if !drift_rows.is_empty() {
        println!();
        print!("{}", render_price_rows(&drift_rows));
    }
    Ok(())
}

/// The drift table, shared by the usage and prices commands.
fn render_price_rows(rows: &[PriceRow]) -> String {
    use crate::table::{Align, Table};

    let mut out = String::new();
    out.push_str(
        "DRIFT   observed spend against the configured schedule, USD per million tokens\n\n",
    );
    let mut table = Table::new(&[
        ("PROVIDER", Align::Left),
        ("MODEL", Align::Left),
        ("TIER", Align::Left),
        ("SOURCE", Align::Left),
        ("MIX", Align::Left),
        ("EXPECTED", Align::Right),
        ("ACTUAL", Align::Right),
        ("DRIFT", Align::Right),
        ("SAMPLES", Align::Right),
    ]);
    for r in rows {
        table.push(vec![
            r.provider.clone(),
            r.model.clone(),
            r.tier_name.clone(),
            r.source.clone(),
            r.mix.clone(),
            r.expected_per_million.clone(),
            r.actual_per_million.clone(),
            if r.significant {
                format!("{} !", r.drift)
            } else {
                r.drift.clone()
            },
            r.samples.to_string(),
        ]);
    }
    out.push_str(&table.render());
    out.push('\n');
    out.push_str(
        "EXPECTED is your configured rates applied to the observed usage for that tier.\n",
    );
    out.push_str("ACTUAL is what the observed spend works out to over the same usage.\n");
    out.push_str("MIX is your input / output / cache-read share of tokens, per tier.\n");
    out.push_str(
        "DRIFT is ACTUAL against EXPECTED; ! marks 20% or more, meaning the upstream is\n",
    );
    out.push_str("no longer charging what you configured - for that tier in particular.\n");
    out
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

    bail!(
        "could not parse --since {spec:?}; use 7d, 24h, 90m, 2w, an RFC3339 timestamp, or YYYY-MM-DD"
    )
}

/// Parse "7d" / "24h" / "90m" / "45s" / "2w" into a duration.
fn parse_relative(spec: &str) -> Option<time::Duration> {
    // Split on the last CHARACTER, not the last byte: split_at panics when the
    // index lands inside a multi-byte character, so "--since 1天" crashed the
    // whole CLI with a char-boundary panic instead of the tidy parse error five
    // lines up. A Chinese or accented unit is an easy thing for a human to type.
    let (index, unit) = spec.char_indices().next_back()?;
    let digits = &spec[..index];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: i64 = digits.parse().ok()?;
    let secs = match unit {
        's' => n,
        'm' => n.checked_mul(60)?,
        'h' => n.checked_mul(3_600)?,
        'd' => n.checked_mul(86_400)?,
        'w' => n.checked_mul(604_800)?,
        _ => return None,
    };
    Some(time::Duration::seconds(secs))
}

/// Aggregate over several dimensions at once.
///
/// Every dimension expression is NOT NULL in practice (provider, model and
/// account_id are NOT NULL columns; day is derived), but COALESCE keeps a
/// surprising NULL from collapsing the whole concatenated key.
fn usage_rows_multi(
    conn: &Connection,
    since: OffsetDateTime,
    dims: &[GroupBy],
) -> Result<Vec<UsageRow>> {
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
        "USAGE since {} (grouped by {})\n\n",
        stamp(since),
        dims.iter()
            .map(|d| d.column_header().to_ascii_lowercase())
            .collect::<Vec<_>>()
            .join(", ")
    ));

    // Widths are measured, and the counts are scaled, so a nine-digit token count
    // cannot push the cost column out of line the way it used to.
    let mut table = crate::table::Table::new(&[
        (&header, crate::table::Align::Left),
        ("REQUESTS", crate::table::Align::Right),
        ("IN", crate::table::Align::Right),
        ("OUT", crate::table::Align::Right),
        ("CACHED", crate::table::Align::Right),
        ("COST", crate::table::Align::Right),
    ]);
    for r in rows {
        table.push(vec![
            display_key(r),
            r.requests.to_string(),
            crate::table::tokens(r.input_tokens),
            crate::table::tokens(r.output_tokens),
            crate::table::tokens(r.cached_tokens),
            crate::table::money_with(r.cost, &r.currency),
        ]);
    }
    out.push_str(&table.render());

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
                "TOTAL {currency}: {requests} requests, in {}, out {}, cached {}, cost {} {currency}\n",
                crate::table::tokens(input),
                crate::table::tokens(output),
                crate::table::tokens(cached),
                crate::table::money(cost),
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
    use crate::table::{Align, Table, tokens};

    let mut out = String::new();

    // MODEL is truncated rather than measured: a model name can be sixty
    // characters, and one long row would otherwise stretch every column of the
    // table to fit it. Everything else is measured.
    const MODEL_WIDTH: usize = 28;

    let mut table = Table::new(&[
        ("TIME", Align::Left),
        ("STATUS", Align::Left),
        ("PROVIDER", Align::Left),
        ("ACCOUNT", Align::Left),
        ("MODEL", Align::Left),
        ("IN", Align::Right),
        ("OUT", Align::Right),
        ("CACHED", Align::Right),
        ("TTFT", Align::Right),
        ("TPS", Align::Right),
        ("COVERAGE", Align::Left),
    ]);
    for r in rows {
        let ttft = r
            .ttft_ms
            .map(|ms| format!("{ms}ms"))
            .unwrap_or_else(|| "-".to_string());
        let tps = r
            .tps
            .map(|t| format!("{t:.1}"))
            .unwrap_or_else(|| "-".to_string());
        table.push(vec![
            clock(r.ts),
            status_label(r.status).to_string(),
            r.provider.clone(),
            r.account_id.clone(),
            truncate(&r.model, MODEL_WIDTH),
            tokens(r.input_tokens),
            tokens(r.output_tokens),
            tokens(r.cached_tokens),
            ttft,
            tps,
            coverage_label(r.coverage).to_string(),
        ]);
    }
    out.push_str(&table.render());
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
    t.time()
        .format(HMS)
        .unwrap_or_else(|_| "--:--:--".to_string())
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
        AccountsCommand::Enable { account } => cmd_accounts_enable(path, &account),
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

/// Whether a listen address is confined to this machine.
///
/// The address is a string in the config, so this has to be a textual check:
/// anything that does not name the loopback interface is treated as reachable
/// by others, which is the safe direction.
fn listens_on_loopback(listen: &str) -> bool {
    let host = listen.rsplit_once(':').map(|(h, _)| h).unwrap_or(listen);
    let host = host.trim_start_matches('[').trim_end_matches(']');
    matches!(host, "127.0.0.1" | "localhost" | "::1") || host.starts_with("127.")
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

fn cmd_accounts_disable(path: &Path, account_id: &str) -> Result<()> {
    let config = load_config(path)?;
    let provider = provider_of(&config, account_id)?;
    // Deliberately unresolved: disabling an account must work even when its
    // credential is missing, which is exactly when you want to disable it.
    // Target the canonical identity: the same account id can exist under
    // several providers, and disabling the wrong one would silently take a
    // healthy account out of the pool.
    let qualified = crate::domain::qualified_id(&provider, account_id);
    let accounts = config.build_accounts_unresolved()?;
    let mut account = accounts
        .into_iter()
        .find(|a| a.qualified() == qualified)
        .with_context(|| format!("account {qualified} is not in the configuration"))?;

    let db = config.db_path();

    // Overlay what the ledger already knows before writing anything back.
    //
    // This command used to save a full pool snapshot built from configuration
    // alone: those accounts carry no gates and state Unknown, so the upsert
    // deleted the gate rows of EVERY account in the file and reset their state
    // and reset times. Disabling one account quietly erased the quota readings
    // of all the others -- hours of probing, and on Command Code the difference
    // between routing on real numbers and routing blind. It also created a
    // fresh empty ledger on a machine that had none.
    if let Ok(conn) = store::open_readonly(&db)
        && let Ok(rows) = store::load_accounts(&conn)
        && let Some(row) = rows
            .into_iter()
            .find(|r| crate::domain::qualified_id(&r.provider, &r.account_id) == qualified)
    {
        account.gates = row.gates;
        account.available_at = row.available_at;
        account.window_started_at = row.window_started_at;
    }

    // There is no dedicated "operator disabled" transition in the frozen pool
    // API; an account that can never be billed again is exactly the Disabled
    // state that insufficient credit produces, so it is used here and the
    // reason is rewritten to say what really happened.
    account.state = AccountState::Disabled;
    account.last_error = Some(crate::accounts::OPERATOR_DISABLED.to_string());

    let handle = store::StoreHandle::spawn(&db)?;
    // One account, one row. Never a snapshot.
    handle.save_account(&account);
    handle.flush_blocking()?;

    println!(
        "{provider}/{account_id}: DISABLED (persisted to {})",
        db.display()
    );
    println!(
        "note: a running lgw process keeps its own in-memory pool; restart lgw serve \
         for this to take effect there"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

/// Which credential backend to use. See CredentialStore::from_env.
fn credential_store() -> CredentialStore {
    CredentialStore::from_env()
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
    // Resolve every credential ONCE and reuse the result below.
    //
    // The account list used to be built twice -- once here to report, once in
    // the probe section -- so every key was read twice. On macOS each read of an
    // item the app is not already trusted for is another authorisation prompt,
    // which made "lgw doctor" ask twice per account for a read-only check.
    let (usable, unusable) = config.build_accounts_lenient(&creds)?;
    for (provider, p) in &config.providers {
        if p.accounts.is_empty() {
            println!("  {provider}: no accounts configured");
            problems += 1;
        }
        for (id, a) in &p.accounts {
            let secret_ref = a.key.clone().unwrap_or(SecretRef::Keychain);
            let source = secret_ref.describe();
            // The key itself is never printed; ApiKey redacts Debug/Display and
            // this path never formats it at all.
            let qualified = crate::domain::qualified_id(provider, id);
            if usable.iter().any(|acct| acct.qualified() == qualified) {
                println!("  {provider}/{id}: ok ({source})");
                continue;
            }
            let reason = unusable
                .iter()
                .find(|acct| acct.qualified() == qualified)
                .and_then(|acct| acct.last_error.clone())
                .unwrap_or_else(|| "could not be resolved".to_string());
            println!("  {provider}/{id}: MISSING ({source}) - {reason}");
            problems += 1;
        }
    }

    println!("probes      :");
    let client = probe_client(&config)?;

    let mut targets: HashMap<String, (QuotaAdapter, String)> = HashMap::new();
    for (provider, adapter, base) in config.probe_targets() {
        targets.insert(provider, (adapter, base));
    }

    // Doctor is deliberately strict: it exists to report problems.
    // The accounts were already built above, so this reuses them rather than
    // reading every credential a second time.
    if usable.is_empty() {
        println!("  skipped: no account has a usable credential");
        println!("  (fix the missing credentials above, then re-run doctor)");
    } else {
        {
            for account in &usable {
                let label = format!("{}/{}", account.provider, account.id);
                match targets.get(&account.provider) {
                    None => println!("  {label}: no probe target configured"),
                    Some((QuotaAdapter::None, _)) => {
                        // There is no supported "local ledger only" mode: an
                        // account that is never probed never leaves Unknown, and
                        // Unknown is not selectable. Saying "local ledger only"
                        // made a dead account look like a configured one.
                        println!(
                            "  {label}: UNUSABLE - adapter = none has no quota source, so this \
                             account can never leave the UNKNOWN state"
                        );
                        problems += 1;
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

    // Two settings whose absence is invisible until it costs money or leaks
    // quota. Neither was checked anywhere: the proxy opened its port silently
    // and doctor reported "no problems found".
    if config.server.local_key.is_none() {
        println!(
            "local auth  : MISSING - anything that can reach {} can spend the pooled quota.",
            config.server.listen
        );
        println!("              fix: lgw keys add local gateway, then set local_key");
        problems += 1;
    } else {
        println!("local auth  : configured");
    }
    if !listens_on_loopback(&config.server.listen) {
        println!(
            "listen      : {} is not a loopback address; this process holds paid credentials",
            config.server.listen
        );
        problems += 1;
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
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    // 0600 from the moment it exists. The template carries no secret, but it is
    // the file people then put credentials-adjacent settings in, and a config
    // written world-readable is world-readable for the rest of its life.
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("writing {}", path.display()))?;
        use std::io::Write as _;
        file.write_all(crate::config::sample_toml().as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
        // --force over an existing file keeps that file's mode, so make it
        // explicit rather than inheriting whatever the old file had.
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    std::fs::write(path, crate::config::sample_toml())
        .with_context(|| format!("writing {}", path.display()))?;
    println!("wrote example config to {}", path.display());
    println!("next: set the credentials it references, then run lgw doctor");
    Ok(())
}

/// Put a disabled account back in the pool.
///
/// The state becomes Unknown rather than Active on purpose: the next probe has
/// to confirm the account can actually serve before it takes traffic, which is
/// the same rule a restart follows. "accounts enable" means "try again", not
/// "trust me" -- and it is the only thing that can undo an operator's disable,
/// because a successful probe restores the credential, not the decision.
fn cmd_accounts_enable(path: &Path, account_id: &str) -> Result<()> {
    let config = load_config(path)?;
    let provider = provider_of(&config, account_id)?;
    let qualified = crate::domain::qualified_id(&provider, account_id);
    let db = config.db_path();

    let accounts = config.build_accounts_unresolved()?;
    let mut account = accounts
        .into_iter()
        .find(|a| a.qualified() == qualified)
        .with_context(|| format!("account {qualified} is not in the configuration"))?;

    if let Ok(conn) = store::open_readonly(&db)
        && let Ok(rows) = store::load_accounts(&conn)
        && let Some(row) = rows
            .into_iter()
            .find(|r| crate::domain::qualified_id(&r.provider, &r.account_id) == qualified)
    {
        if row.state != AccountState::Disabled {
            bail!(
                "{qualified} is {}, not DISABLED; there is nothing to enable",
                row.state.label()
            );
        }
        account.gates = row.gates;
        account.available_at = row.available_at;
        account.window_started_at = row.window_started_at;
        account.last_probe_at = None;
    }

    account.state = AccountState::Unknown;
    account.last_error = None;

    let handle = store::StoreHandle::spawn(&db)?;
    handle.save_account(&account);
    handle.flush_blocking()?;

    println!("{provider}/{account_id}: ENABLED (probed before it serves again)");
    println!(
        "note: restart lgw serve for this to take effect there, or run          'lgw accounts probe' to confirm it now"
    );
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
        KeysCommand::Migrate { prune } => cmd_keys_migrate(path, prune),
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
        rpassword::prompt_password(format!("API key for {provider}/{account} (input hidden): "))
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
            // Say which item the key came out of. An install that still keeps one
            // item per account pays one authorisation prompt per account on
            // macOS, and "lgw keys migrate" is how that stops.
            let mut source = secret_ref.describe();
            if matches!(secret_ref, SecretRef::Keychain) {
                let origin = creds.origin(provider, id);
                if origin != crate::secrets::CredentialOrigin::Absent {
                    source = format!("{source} ({})", origin.short());
                }
            }
            rows.push(CredentialRow {
                provider: provider.clone(),
                account: id.clone(),
                source,
                status,
            });
        }
    }
    rows
}

fn cmd_keys_list(config: &Config, creds: &CredentialStore) -> Result<()> {
    println!("backend: {}", creds.backend_name());
    let rows = credential_rows(config, creds);
    let per_account = rows
        .iter()
        .filter(|r| r.source.contains("per-account"))
        .count();
    if creds.is_vaulted() {
        println!(
            "vault   : {}",
            if per_account == 0 {
                "every key in one credential-store item".to_string()
            } else {
                format!("{} key(s) still in separate per-account items", per_account)
            }
        );
    }
    println!();
    let mut table = crate::table::Table::new(&[
        ("PROVIDER", crate::table::Align::Left),
        ("ACCOUNT", crate::table::Align::Left),
        ("SOURCE", crate::table::Align::Left),
        ("STATUS", crate::table::Align::Left),
    ]);
    for row in &rows {
        table.push(vec![
            row.provider.clone(),
            row.account.clone(),
            row.source.clone(),
            row.status.clone(),
        ]);
    }
    print!("{}", table.render());
    println!();
    if per_account > 0 {
        println!(
            "On macOS every separate item is a separate authorisation prompt for this binary."
        );
        println!("Fold them into one item with: lgw keys migrate --prune");
        println!();
    }
    println!("Register one with: lgw keys add <provider> <account>");
    Ok(())
}

/// Every configured credential that is expected to be in the credential store.
///
/// Only these can be migrated: the OS credential store has no portable way to
/// enumerate its items, so a key belonging to an account no longer in the config
/// is invisible here. That is harmless -- the per-account fallback still reads
/// it -- but it will not be folded into the vault.
fn keychain_pairs(config: &Config) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for (provider, p) in &config.providers {
        for (id, a) in &p.accounts {
            if matches!(a.key, None | Some(SecretRef::Keychain)) {
                pairs.push((provider.clone(), id.clone()));
            }
        }
    }
    if matches!(config.server.local_key, Some(SecretRef::Keychain)) {
        pairs.push((
            crate::secrets::LOCAL_KEY_PROVIDER.to_string(),
            crate::secrets::LOCAL_KEY_ACCOUNT.to_string(),
        ));
    }
    pairs
}

/// Fold every per-account credential-store item into the single vault item.
///
/// Why it exists: macOS identifies an ad-hoc signed binary by the hash of its
/// code, so "Always Allow" never survives a rebuild, and the only durable fix is
/// to have fewer items to authorise. This moves N items into one, and --prune
/// removes the old ones once the vault is confirmed to hold their contents.
fn cmd_keys_migrate(path: &Path, prune: bool) -> Result<()> {
    let config = load_config(path)?;
    let creds = credential_store();

    if !creds.is_vaulted() {
        println!(
            "the {} backend keeps no vault; there is nothing to migrate",
            creds.backend_name()
        );
        return Ok(());
    }

    let pairs = keychain_pairs(&config);
    if pairs.is_empty() {
        println!(
            "no account in {} uses the OS credential store; nothing to migrate",
            path.display()
        );
        return Ok(());
    }

    let report = creds.migrate(&pairs, prune)?;
    println!(
        "merged {} key(s) into the vault; {} already there, {} with nothing stored",
        report.merged, report.already, report.missing
    );
    if prune {
        println!("removed {} per-account item(s)", report.pruned);
    } else if report.merged > 0 {
        println!();
        println!("The old items are still there. Run the same command with --prune once");
        println!("'lgw doctor' resolves every account, and they will be deleted.");
    }
    println!();
    println!("Backend: {}", creds.backend_name());
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

    let Some(probe) = crate::quota::probe_for(p.adapter, &p.probe_base()) else {
        println!(
            "{provider}/{account}: credential found ({} chars), but this provider has no quota probe to test against",
            key.expose().len()
        );
        return Ok(());
    };

    let account_model = crate::domain::Account {
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
        last_probe_at: None,
        last_error: None,
    };

    let client = probe_client(config)?;

    match probe.probe(&account_model, &client).await {
        Ok(outcome) => {
            println!(
                "{provider}/{account}: ok - {} credentials accepted, {} gates read",
                probe.name(),
                outcome.gates.len()
            );
            println!("{}", gate_reading(&outcome.gates, &account_model.currency));
            Ok(())
        }
        Err(e) => {
            println!("{provider}/{account}: FAILED - {e}");
            bail!("the stored credential was rejected or the probe endpoint is unreachable")
        }
    }
}

/// Every gate the probe read, one per line, in the shape the board uses.
///
/// The unit is a column rather than an assumption: a free-token allowance is
/// counted in tokens and an rpm gate in requests per minute, so a raw 30000000
/// sitting where a balance goes is how an allowance gets read as money. The class
/// is spelled out in the same words as the board's columns rather than as a Rust
/// enum name.
fn gate_reading(gates: &[Gate], currency: &str) -> String {
    use crate::table::{Align, Table};
    use rust_decimal::prelude::ToPrimitive;

    if gates.is_empty() {
        return "    no gates in the response".to_string();
    }
    let mut table = Table::new(&[
        ("GATE", Align::Left),
        ("USED", Align::Right),
        ("CAP", Align::Right),
        ("UNIT", Align::Left),
        ("POT", Align::Left),
    ]);
    for g in gates {
        // A token allowance is read at the scale it is spent at, exactly as the
        // usage tables read it; everything else is money and keeps its decimals.
        let amount = |d: Decimal| match g.kind {
            GateKind::FreeTokens => crate::table::tokens(d.to_u64().unwrap_or_default()),
            _ => board_amount(d),
        };
        table.push(vec![
            g.kind.label().to_string(),
            amount(g.used),
            amount(g.cap),
            gate_unit(g.kind, currency).to_string(),
            class_label(g.class()).to_string(),
        ]);
    }
    table
        .render()
        .lines()
        .map(|l| format!("    {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// What a gate is counted in.
fn gate_unit(kind: GateKind, currency: &str) -> &str {
    match kind {
        GateKind::FreeTokens => "tokens",
        GateKind::Rpm => "req/min",
        _ => currency,
    }
}

/// A gate class in the words the board uses for its two money columns.
fn class_label(class: GateClass) -> &'static str {
    match class {
        GateClass::Covered => "covered",
        GateClass::Metered => "metered",
        GateClass::RateLimit => "rate limit",
    }
}

// ---------------------------------------------------------------------------
// prices panel
// ---------------------------------------------------------------------------

/// One line of the rates-in-effect panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PricePanelRow {
    /// "peak", "default", "learned".
    pub label: String,
    /// When it applies, or a note for the learned row.
    pub when: String,
    /// The tier's multiplier against the default tier.
    pub ratio: String,
    pub input: String,
    pub output: String,
    pub cache_read: String,
}

/// USD per million tokens, prefixed. Built by pushing the sign so a format
/// string never has to start with a dollar sign followed by a brace.
fn usd(per_token: rust_decimal::Decimal) -> String {
    let v = f64_of(per_token) * 1_000_000.0;
    let mut out = String::new();
    out.push('$');
    out.push_str(&format!("{v:.4}"));
    out
}

/// A rate, already per million tokens, with a dollar sign and a readable scale.
///
/// The old name said "from_million" but nothing was converted: it just printed
/// three decimals whether or not the number had any, so $0.30 read as $0.300 and
/// $15 as $15.000.
fn usd_rate(per_million: f64) -> String {
    match Decimal::try_from(per_million) {
        Ok(rate) => format!("${}", crate::table::money(rate)),
        Err(_) => format!("${per_million:.3}"),
    }
}

/// A human reading of a configured window: "Mon-Fri 01:00-04:00, 06:00-10:00 UTC".
fn window_label(days: &str, hours: &str, tz: Option<&str>) -> String {
    let days = match days.trim().to_ascii_lowercase().as_str() {
        "mon-fri" => "Mon-Fri".to_string(),
        "sat,sun" | "sat-sun" => "Sat-Sun".to_string(),
        "*" => "Every day".to_string(),
        other => other.to_string(),
    };
    let hours = hours
        .split(',')
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(" ");
    let tz = tz.unwrap_or("UTC");
    format!("{days} {hours} {tz}")
}

/// The rates in effect, per configured model, plus the built-in fallback.
///
/// This is the panel that answers "what am I actually being charged, and what
/// would be used for a model I have not priced". The fallback row matters
/// because it is what the hard gate uses for an unconfigured model, and it is
/// deliberately far above real prices.
pub fn price_panel(
    config: &Config,
    table: &crate::meter::PriceTable,
) -> Vec<(String, Vec<PricePanelRow>)> {
    let mut out = Vec::new();

    for (provider, models) in &config.prices {
        for (model, price) in models {
            let label = format!("{provider}/{model}");
            let tiers = price.tiers();
            let default_input = tiers.first().map(|t| t.input).unwrap_or(0.0);
            let mut rows = Vec::new();

            // Windows first: they are the exception the reader is looking for.
            let parsed = price.schedule(provider, model).ok();
            for (i, s) in price.schedules.iter().enumerate() {
                let tier = &tiers[i + 1];
                let ratio = if default_input > 0.0 {
                    format!("x{:.2}", tier.input / default_input)
                } else {
                    "-".to_string()
                };
                let _ = parsed.as_ref();
                rows.push(PricePanelRow {
                    label: tier.name.clone(),
                    when: window_label(&s.days, &s.hours, s.tz.as_deref()),
                    ratio,
                    input: usd_rate(tier.input),
                    output: usd_rate(tier.output),
                    cache_read: usd_rate(tier.cache_read),
                });
            }

            let d = &tiers[0];
            rows.push(PricePanelRow {
                label: "default".to_string(),
                when: if price.schedules.is_empty() {
                    "no schedule configured".to_string()
                } else {
                    "every other hour".to_string()
                },
                ratio: "x1.00".to_string(),
                input: usd_rate(d.input),
                output: usd_rate(d.output),
                cache_read: usd_rate(d.cache_read),
            });

            // A learned rate for a model with no configured price is the only
            // number that exists for it, so show it in the same shape.
            for (p, m, tier, entry) in table.entries() {
                if p != provider.as_str() || m != model.as_str() {
                    continue;
                }
                let Some(learned) = entry.learned else {
                    continue;
                };
                let name = tiers
                    .get(tier as usize)
                    .map(|t| t.name.clone())
                    .unwrap_or_else(|| format!("tier {tier}"));
                rows.push(PricePanelRow {
                    label: format!("learned ({name})"),
                    when: "recovered from observed spend".to_string(),
                    ratio: "-".to_string(),
                    input: usd(learned.input_per_token),
                    output: usd(learned.output_per_token),
                    cache_read: usd(learned.cache_read_per_token),
                });
            }

            out.push((label, rows));
        }
    }

    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The built-in fallback, as one line, for models with no configured price.
pub fn fallback_row() -> PricePanelRow {
    use crate::meter::{
        CONSERVATIVE_COMPLETION_PER_MILLION as OUT, CONSERVATIVE_PROMPT_PER_MILLION as IN,
    };
    let input = IN as f64;
    let cache_read = input * crate::config::default_cache_read_ratio();
    PricePanelRow {
        label: "fallback".to_string(),
        when: "any model with no configured price".to_string(),
        ratio: "-".to_string(),
        input: usd_rate(input),
        output: usd_rate(OUT as f64),
        cache_read: usd_rate(cache_read),
    }
}

fn print_price_panel(config: &Config, table: &crate::meter::PriceTable) {
    use crate::table::{Align, Table};

    /// The columns every panel shares, so a reader compares like with like across
    /// models instead of re-reading an inline "in ... out ..." label each time.
    const HEADERS: [(&str, Align); 6] = [
        ("RATE", Align::Left),
        ("WINDOW", Align::Left),
        ("RATIO", Align::Right),
        ("INPUT", Align::Right),
        ("OUTPUT", Align::Right),
        ("CACHE READ", Align::Right),
    ];

    let row_cells = |r: &PricePanelRow| {
        vec![
            r.label.clone(),
            r.when.clone(),
            r.ratio.clone(),
            r.input.clone(),
            r.output.clone(),
            r.cache_read.clone(),
        ]
    };

    // Rows sit under the model they belong to, so they are indented by two and
    // the header goes with them.
    let block = |rows: Vec<Vec<String>>| -> String {
        let mut t = Table::new(&HEADERS);
        for r in rows {
            t.push(r);
        }
        t.render()
            .lines()
            .map(|l| format!("  {l}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let panels = price_panel(config, table);
    println!("PRICES   rates in effect, USD per million tokens");
    for (label, rows) in &panels {
        println!();
        println!("{label}");
        println!("{}", block(rows.iter().map(row_cells).collect()));
    }
    let f = fallback_row();
    println!();
    println!("{}", block(vec![row_cells(&f)]));
    println!("  deliberately high - under-estimating is what spends money");
}

// ---------------------------------------------------------------------------
// prices
// ---------------------------------------------------------------------------

/// One line of the price report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriceRow {
    pub provider: String,
    pub model: String,
    /// 0 is the default price tier; higher values are configured time windows.
    pub tier: u32,
    /// "default", or the window's name from the configuration.
    pub tier_name: String,
    pub source: String,
    /// Input / output / cache-read share of the tokens actually used.
    pub mix: String,
    /// Configured prices applied to that mix, per million tokens.
    pub expected_per_million: String,
    /// Observed spend over that same mix, per million tokens.
    pub actual_per_million: String,
    /// Cost drift against the configured price, or a dash when not comparable.
    pub drift: String,
    pub significant: bool,
    pub samples: u64,
}

fn f64_of(d: Decimal) -> f64 {
    use rust_decimal::prelude::ToPrimitive;
    d.to_f64().unwrap_or(0.0)
}

/// Names for price tiers, keyed by (provider, model, tier). Built from the
/// configuration, since the learned table knows only the tier index.
pub type TierNames = std::collections::HashMap<(String, String, u32), String>;

/// Tier names for every configured model, so a report can label a window the
/// way the operator wrote it.
pub fn tier_names(config: &Config) -> TierNames {
    let mut out = TierNames::new();
    for (provider, models) in &config.prices {
        for (model, price) in models {
            for (tier, t) in price.tiers().into_iter().enumerate() {
                out.insert((provider.clone(), model.clone(), tier as u32), t.name);
            }
        }
    }
    out
}

/// Build the price report. Pure, so it can be tested without a terminal.
pub fn price_rows(table: &crate::meter::PriceTable, names: &TierNames) -> Vec<PriceRow> {
    let mut rows: Vec<PriceRow> = table
        .entries()
        .filter_map(|(provider, model, tier, entry)| {
            let source = entry.source()?.label().to_string();
            let tier_name = names
                .get(&(provider.to_string(), model.to_string(), tier))
                .cloned()
                .unwrap_or_else(|| {
                    if tier == 0 {
                        "default".to_string()
                    } else {
                        format!("tier {tier}")
                    }
                });

            // The mix is what makes a per-million figure mean anything: the same
            // prices produce very different per-million costs on different mixes.
            let mix = match entry.mix_percentages() {
                Some((i, o, c)) => format!("{i:.0}/{o:.0}/{c:.0}%"),
                None => "-".to_string(),
            };
            // The two per-million figures are the same comparison as the ratio,
            // expressed in money over the SAME observed mix, which is what makes
            // them readable: I expected this much per million, I am paying that.
            let drift = table.drift(provider, model, tier);
            // Formatted straight from the Decimal: the f64 detour this used to
            // take could not represent the value it was printing.
            let expected_per_million = drift
                .as_ref()
                .map(|d| format!("${}", crate::table::money(d.expected_per_million)))
                .unwrap_or_else(|| "-".to_string());
            let actual_per_million = table
                .actual_per_million(provider, model, tier)
                .map(|(v, _)| format!("${}", crate::table::money(v)))
                .unwrap_or_else(|| "-".to_string());

            let samples = entry.learned.map(|p| p.samples).unwrap_or(0);
            Some(PriceRow {
                provider: provider.to_string(),
                model: model.to_string(),
                tier,
                tier_name,
                source,
                mix,
                expected_per_million,
                actual_per_million,
                drift: match drift {
                    Some(d) => format!("{:+.1}%", d.ratio * 100.0),
                    None => "-".to_string(),
                },
                significant: drift.is_some_and(|d| d.significant),
                samples,
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        b.significant
            .cmp(&a.significant)
            .then_with(|| a.provider.cmp(&b.provider))
            .then_with(|| a.model.cmp(&b.model))
    });
    rows
}

fn cmd_prices(path: &Path, args: &PricesArgs) -> Result<()> {
    let config = load_config(path)?;

    // Configured prices come from the config file; learned prices come from the
    // ledger, so this works whether or not a gateway is running.
    let table = load_price_table(&config);
    print_price_panel(&config, &table);
    println!();

    let names = tier_names(&config);
    let mut rows = price_rows(&table, &names);
    if args.drift_only {
        rows.retain(|r| r.significant);
    }

    if rows.is_empty() {
        if args.drift_only {
            println!("no model has drifted materially from its configured price");
        } else {
            println!("no prices are configured or learned yet.");
            println!();
            println!(
                "Set a starting price in the config file, and the gateway learns the real one"
            );
            println!("from the spend difference between quota probes:");
            println!();
            println!("  [prices.xkiro.\"z-ai/glm-5.3-flash\"]");
            println!("  input = 0.30");
            println!("  output = 1.20");
            println!("  \"cache read\" = 0.03   # optional; defaults to input / 10");
        }
        return Ok(());
    }

    // One renderer for both commands, so they cannot disagree.
    print!("{}", render_price_rows(&rows));

    let drifted = rows.iter().filter(|r| r.significant).count();
    if drifted > 0 {
        println!();
        println!("{drifted} model(s) have drifted; investigate before trusting the hard gate.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::GateSource;
    use std::str::FromStr;

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).expect("valid decimal")
    }

    #[test]
    fn board_amounts_round_to_one_decimal_without_lying() {
        // The values that prompted this: fifteen digits wrapped around one.
        assert_eq!(board_amount(dec("1.113194698")), "1.1");
        assert_eq!(board_amount(dec("4.175475087")), "4.2");
        assert_eq!(board_amount(dec("4.182168809")), "4.2");
        assert_eq!(board_amount(dec("65.824524913")), "65.8");
        assert_eq!(board_amount(dec("1320.065041")), "1320.1");
        assert_eq!(board_amount(dec("32.042344")), "32");

        // An exact integer keeps no decimal point.
        assert_eq!(board_amount(dec("368")), "368");
        assert_eq!(board_amount(dec("30000000")), "30000000");
        assert_eq!(board_amount(Decimal::ZERO), "0");

        // A small but real balance must never read as untouched: that is the
        // difference between "this account has not been used" and "it has been
        // used a little", and the board is where that judgement gets made.
        assert_eq!(board_amount(dec("0.001221")), "0.001");
        assert_eq!(board_amount(dec("0.0004")), "0.0004");
        assert_ne!(board_amount(dec("0.0000001")), "0");
        assert_eq!(board_amount(dec("-0.0000001")), "<-0.000001");

        // Money is not rounded away in the cost columns either. board_amount is
        // for the board; the report columns go through table::money, which keeps a
        // per-request cost like this one intact.
        assert_eq!(crate::table::money(dec("0.0000036")), "0.0000036");
    }
    // -- prices --------------------------------------------------------------

    fn priced_table() -> crate::meter::PriceTable {
        use crate::config::ModelPriceConfig;
        use std::collections::BTreeMap;
        let mut table = crate::meter::PriceTable::new();
        let mut cfg: BTreeMap<String, BTreeMap<String, ModelPriceConfig>> = BTreeMap::new();
        cfg.entry("xkiro".to_string()).or_default().insert(
            "z-ai/glm-5.3-flash".to_string(),
            ModelPriceConfig {
                input: 0.30,
                output: 1.20,
                cache_read: None,
                schedules: Vec::new(),
            },
        );
        table.seed_from_config(&cfg);
        // Reality is 3x the configured prompt price: 0.90/M over 1M tokens.
        table.observe(
            "xkiro",
            "z-ai/glm-5.3-flash",
            0,
            dec("0.9"),
            &crate::domain::TokenUsage {
                prompt_tokens: 1_000_000,
                ..Default::default()
            },
        );
        // A second model with no configured price: nothing to compare against.
        table.observe(
            "commandcode",
            "deepseek/deepseek-v4.1-flash",
            0,
            dec("0.05"),
            &crate::domain::TokenUsage {
                prompt_tokens: 1_000,
                completion_tokens: 25,
                ..Default::default()
            },
        );
        table
    }

    #[test]
    fn price_rows_show_configured_against_learned() {
        let rows = price_rows(&priced_table(), &TierNames::new());
        assert_eq!(rows.len(), 2);

        let glm = rows
            .iter()
            .find(|r| r.model == "z-ai/glm-5.3-flash")
            .unwrap();
        assert_eq!(glm.provider, "xkiro");
        assert_eq!(
            glm.source, "learned",
            "an observed price outranks the expectation"
        );
        // 1M input tokens, nothing else: the mix says so.
        assert_eq!(glm.mix, "100/0/0%");
        // Configured $0.30/M input applied to that mix, against the $0.90/M the
        // observed spend actually works out to.
        // Trailing zeros are not precision: $0.3000 and $0.3 are the same money,
        // and the column is right-aligned so the decimal points still line up.
        assert_eq!(glm.expected_per_million, "$0.3");
        assert_eq!(glm.actual_per_million, "$0.9");
        assert_eq!(glm.drift, "+200.0%");
        assert!(glm.significant, "a tripling must be flagged");
        assert_eq!(glm.samples, 1);

        // No configured price: the actual figure is still useful, but there is
        // nothing to compare it against, so no drift claim is made.
        let ds = rows
            .iter()
            .find(|r| r.model == "deepseek/deepseek-v4.1-flash")
            .unwrap();
        assert_eq!(ds.expected_per_million, "-");
        assert_ne!(
            ds.actual_per_million, "-",
            "the observed cost is still shown"
        );
        assert_eq!(ds.drift, "-");
        assert!(!ds.significant);
    }

    #[test]
    fn drifted_rows_sort_first_so_they_are_seen() {
        let rows = price_rows(&priced_table(), &TierNames::new());
        assert!(rows[0].significant, "the drifted model must come first");
    }

    #[test]
    fn a_configured_price_with_no_observation_is_shown_as_configured() {
        use crate::config::ModelPriceConfig;
        use std::collections::BTreeMap;
        let mut table = crate::meter::PriceTable::new();
        let mut cfg: BTreeMap<String, BTreeMap<String, ModelPriceConfig>> = BTreeMap::new();
        cfg.entry("xkiro".to_string()).or_default().insert(
            "m".to_string(),
            ModelPriceConfig {
                input: 1.0,
                output: 4.0,
                cache_read: None,
                schedules: Vec::new(),
            },
        );
        table.seed_from_config(&cfg);

        let rows = price_rows(&table, &TierNames::new());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source, "configured");
        // Nothing observed yet, so there is no mix and no per-million figure:
        // a dash rather than a claim computed from nothing.
        assert_eq!(rows[0].mix, "-");
        assert_eq!(rows[0].expected_per_million, "-");
        assert_eq!(rows[0].actual_per_million, "-");
        assert_eq!(rows[0].drift, "-");
        assert_eq!(rows[0].samples, 0);
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
        let p = std::env::temp_dir().join(format!("lgw-key-{}.txt", uuid::Uuid::new_v4()));
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
        assert!(
            stored.status.starts_with("MISSING"),
            "got {}",
            stored.status
        );
        // And it names the exact command that fixes it.
        assert!(stored.status.contains("lgw keys add xkiro from-store"));
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
        assert!(
            msg.contains("from-store"),
            "should list known accounts: {msg}"
        );
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
                gate(
                    GateKind::FiveHour,
                    "100",
                    "40",
                    Some(4_200),
                    Confidence::Exact,
                    false,
                    t,
                ),
                gate(
                    GateKind::Weekly,
                    "500",
                    "25",
                    None,
                    Confidence::Exact,
                    false,
                    t,
                ),
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
        assert_eq!(
            covered, "5h 40/100  7d 25/500  USD",
            "covered shows used/cap and its unit"
        );
        assert_eq!(
            metered, "credits 12 USD (unused)",
            "metered shows the money left and says it is untouched"
        );
        assert_ne!(covered, metered);

        let board = render_board(&[acct], t);
        assert!(board.contains("COVERED"));
        assert!(board.contains("METERED"));
        assert!(board.contains("5h 40/100  7d 25/500  USD"));
        assert!(board.contains("credits 12 USD (unused)"));
        assert!(board.contains("DRAINING"));
        assert!(
            board.contains("metered = real money remaining"),
            "the legend must explain the two columns: {board}"
        );
    }

    /// The composite cells are measured across the whole board, not per row.
    ///
    /// Two accounts with different numbers of windows and wildly different
    /// fractions must still agree on where the slashes and the unit sit: that is
    /// the difference between a table of allowances and two lines of text that
    /// happen to be stacked.
    #[test]
    fn board_lines_up_the_sub_columns_of_every_row() {
        let t = now();
        let short = overview(
            "xkiro",
            "xk-1",
            "USD",
            AccountState::Active,
            vec![
                gate(
                    GateKind::FiveHour,
                    "20",
                    "0",
                    None,
                    Confidence::Exact,
                    false,
                    t,
                ),
                gate(
                    GateKind::Weekly,
                    "132",
                    "0.001",
                    None,
                    Confidence::Exact,
                    false,
                    t,
                ),
                gate(
                    GateKind::PurchasedCredits,
                    "4",
                    "0",
                    None,
                    Confidence::Exact,
                    false,
                    t,
                ),
            ],
        );
        let long = overview(
            "deepseek",
            "ds-1",
            "USD",
            AccountState::Active,
            vec![
                gate(
                    GateKind::FiveHour,
                    "200",
                    "0",
                    None,
                    Confidence::Exact,
                    false,
                    t,
                ),
                gate(
                    GateKind::Weekly,
                    "1320",
                    "1320.1",
                    None,
                    Confidence::Exact,
                    false,
                    t,
                ),
                gate(
                    GateKind::Wallet,
                    "700",
                    "16.05",
                    None,
                    Confidence::Exact,
                    false,
                    t,
                ),
            ],
        );

        let board = render_board(&[short, long], t);
        let rows: Vec<&str> = board
            .lines()
            .filter(|l| l.starts_with("xkiro") || l.starts_with("deepseek"))
            .collect();
        assert_eq!(rows.len(), 2, "{board}");

        let slashes = |l: &str| l.match_indices('/').map(|(i, _)| i).collect::<Vec<_>>();
        assert_eq!(
            slashes(rows[0]),
            slashes(rows[1]),
            "the fractions drifted apart: {board}"
        );
        let unit = |l: &str| l.find("USD").expect("every row carries its unit");
        assert_eq!(
            unit(rows[0]),
            unit(rows[1]),
            "the unit column drifted: {board}"
        );
        let note = |l: &str| l.find('(').expect("the note follows the money");
        assert_eq!(
            note(rows[0]),
            note(rows[1]),
            "the pot labels are not the same width: {board}"
        );
        assert!(rows[0].contains("credits"), "the pot is labelled: {board}");
        assert!(rows[1].contains("wallet"), "the pot is labelled: {board}");
    }

    /// `lgw keys test` prints one row per gate. The unit column is the point: a
    /// free-token allowance and a balance are not the same kind of number, and a
    /// raw 30000000 next to a currency code is how one gets read as the other.
    #[test]
    fn a_gate_reading_names_its_unit_and_keeps_its_columns_straight() {
        let t = now();
        let gates = vec![
            gate(
                GateKind::FiveHour,
                "100",
                "40",
                None,
                Confidence::Exact,
                false,
                t,
            ),
            gate(
                GateKind::FreeTokens,
                "30000000",
                "368",
                None,
                Confidence::Exact,
                false,
                t,
            ),
            gate(
                GateKind::Rpm,
                "600",
                "12",
                None,
                Confidence::Exact,
                false,
                t,
            ),
            gate(
                GateKind::Wallet,
                "12.34",
                "0",
                None,
                Confidence::Exact,
                false,
                t,
            ),
        ];
        let out = gate_reading(&gates, "USD");
        assert!(out.contains("30.0M"), "a token allowance is scaled: {out}");
        assert!(out.contains("tokens"), "{out}");
        assert!(out.contains("req/min"), "{out}");
        assert!(!out.contains("Metered"), "no Rust enum names: {out}");
        assert!(!out.contains("RateLimit"), "no Rust enum names: {out}");

        let pot = |line: &str| {
            ["covered", "metered", "rate limit"]
                .iter()
                .filter_map(|word| line.find(*word))
                .next()
                .expect("every row names its pot")
        };
        let columns: Vec<usize> = out.lines().skip(1).map(pot).collect();
        assert_eq!(columns.len(), gates.len(), "{out}");
        assert!(
            columns.iter().all(|c| *c == columns[0]),
            "the pot column drifted: {out}"
        );
    }

    /// The board must not print a free-token allowance.
    ///
    /// It is counted in tokens rather than money, so it has no place sitting
    /// under a currency code, and an operator who never calls a free model has
    /// no use for the number. The gate is still probed and stored -- this board
    /// simply does not show it.
    #[test]
    fn board_hides_the_free_token_allowance() {
        let t = now();
        let acct = overview(
            "xkiro",
            "xk-1",
            "USD",
            AccountState::Active,
            vec![
                gate(
                    GateKind::FiveHour,
                    "20",
                    "0.5",
                    None,
                    Confidence::Exact,
                    false,
                    t,
                ),
                gate(
                    GateKind::FreeTokens,
                    "30000000",
                    "368",
                    None,
                    Confidence::Exact,
                    false,
                    t,
                ),
            ],
        );

        let covered = covered_cell(&acct);
        assert_eq!(covered, "5h 0.5/20  USD");
        assert!(!covered.contains("free"), "no free allowance: {covered}");
        assert!(!covered.contains("tok"), "no token count: {covered}");
    }

    /// A free allowance on its own is not a covered reading at all.
    #[test]
    fn a_free_allowance_alone_leaves_the_covered_column_blank() {
        let t = now();
        let acct = overview(
            "xkiro",
            "xk-2",
            "USD",
            AccountState::Active,
            vec![gate(
                GateKind::FreeTokens,
                "30000000",
                "0",
                None,
                Confidence::Exact,
                false,
                t,
            )],
        );
        assert_eq!(covered_cell(&acct), "-");
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
        assert_eq!(metered_cell(&acct), "wallet 12.3 USD (unused)");
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
        assert_eq!(metered_cell(&acct), "wallet 684 USD (16.1 spent)");
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
            board
                .lines()
                .any(|l| l.starts_with("xkiro") && l.contains("STALE")),
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
                gate(
                    GateKind::FiveHour,
                    "200",
                    "178",
                    Some(4_200),
                    Confidence::Exact,
                    false,
                    t,
                ),
                gate(
                    GateKind::Weekly,
                    "1000",
                    "100",
                    Some(90_000),
                    Confidence::Exact,
                    false,
                    t,
                ),
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
        assert_eq!(
            format_countdown(t, t + time::Duration::seconds(4_200)),
            "1h10m"
        );
        assert_eq!(
            format_countdown(t, t + time::Duration::seconds(93_600)),
            "1d2h"
        );
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
        assert_eq!(
            parse_since("24h", t).unwrap(),
            t - time::Duration::hours(24)
        );
        assert_eq!(
            parse_since("90m", t).unwrap(),
            t - time::Duration::minutes(90)
        );
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
            "lgw-cli-{}-{}",
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
        assert_eq!(ConfigOrigin::Env.label(), "$LGW_CONFIG");
        assert_eq!(ConfigOrigin::Local.label(), "./lgw.toml");
        assert_eq!(
            ConfigOrigin::Home.label(),
            "~/.light-ai-gateway/config.toml"
        );
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

    /// A multi-byte unit is a parse error, not a panic.
    ///
    /// The parser split at spec.len() - 1, a BYTE index, so "--since 1天" aborted
    /// the process with "byte index 3 is not a char boundary" instead of the tidy
    /// message the function already had. A Chinese unit is an easy thing to type.
    #[test]
    fn a_non_ascii_since_unit_is_refused_instead_of_panicking() {
        assert!(parse_relative("1天").is_none());
        assert!(parse_relative("7天前").is_none());
        assert!(parse_relative("é").is_none());
        assert!(parse_relative("").is_none());
        // And the ASCII spellings still work.
        assert_eq!(parse_relative("7d"), Some(time::Duration::days(7)));
        assert_eq!(parse_relative("90m"), Some(time::Duration::minutes(90)));
    }

    /// Only a loopback address counts as local.
    ///
    /// The proxy holds paid credentials and doctor said nothing about an address
    /// the rest of the network could reach.
    #[test]
    fn only_loopback_addresses_are_treated_as_local() {
        assert!(listens_on_loopback("127.0.0.1:8787"));
        assert!(listens_on_loopback("localhost:8787"));
        assert!(listens_on_loopback("[::1]:8787"));
        assert!(listens_on_loopback("127.0.0.53:8787"));
        assert!(!listens_on_loopback("0.0.0.0:8787"));
        assert!(!listens_on_loopback("[::]:8787"));
        assert!(!listens_on_loopback("192.168.1.10:8787"));
    }
}
