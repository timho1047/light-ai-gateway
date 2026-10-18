//! SQLite persistence.
//!
//! Requirements:
//!   - rusqlite, WAL mode, busy_timeout set.
//!   - Writes go through a single writer task fed by an unbounded channel.
//!     Nothing here may block the request path.
//!   - Account and gate state MUST be persisted: losing the ledger across a
//!     restart means the gateway re-spends quota it already used, which on
//!     Command Code silently becomes real money.
//!   - Prompt and response bodies are never stored.
//!
//! ## Encoding
//!
//! Times are unix epoch seconds (INTEGER). Money is TEXT produced from
//! rust_decimal::Decimal::to_string, so no precision is lost on the way in or
//! out (xKiro reports amounts such as "0.000356"); every money column is read
//! back through crate::domain::parse_money. Token counts and booleans are
//! INTEGER. Enums are their stable snake_case names. No prompt or completion
//! body is ever placed in a column.
//!
//! ## Durability policy for the account ledger
//!
//! The account/gate ledger is the one thing that must not be lost: on restart
//! the pool is restored from accounts + gates, and a stale ledger is how a
//! gateway re-spends quota it already consumed. Callers therefore hand a fresh
//! pool snapshot to StoreHandle::save_accounts at every point the ledger moves:
//! every probe result, every settle, and every error transition. The writer
//! commits each delivered batch in one transaction, so the snapshot lands on
//! disk within milliseconds of the call. Account writes are deliberately NOT
//! debounced: spent_local that is lost is money that gets spent twice. Request
//! and attempt rows are ordinary write-behind telemetry and are batched freely.
//!
//! WAL journal mode plus synchronous=NORMAL is used: WAL survives a process
//! crash (the case that matters for a restarted gateway), and NORMAL avoids an
//! fsync per commit while still checkpointing on clean shutdown.

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, Transaction, params};
use rust_decimal::Decimal;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;
use time::OffsetDateTime;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::domain::{
    Account, AccountOverview, AccountState, AttemptRecord, Confidence, Gate, GateClass, GateKind,
    GateSource, GroupBy, RequestRecord, RequestStatus, RequestSummary, UsageRow,
};
use crate::tier::TierKey;

/// How long a writer waits for a competing process lock before giving up.
const BUSY_TIMEOUT: StdDuration = StdDuration::from_millis(5_000);

/// Maximum number of queued messages committed in one transaction.
const WRITE_BATCH_MAX: usize = 256;

/// Schema DDL. Every statement is IF NOT EXISTS, so init_schema is idempotent
/// and safe to run against a database created by an older build.
const SCHEMA: &str = r#"
-- Account identity is (provider, id), never id alone. Two providers may both
-- have an account called "acc-1"; keying on the bare id silently merged them,
-- so one account's quota reading landed on the other.
CREATE TABLE IF NOT EXISTS accounts (
    provider          TEXT    NOT NULL,
    id                TEXT    NOT NULL,
    label             TEXT    NOT NULL,
    base_url          TEXT    NOT NULL,
    currency          TEXT    NOT NULL,
    credit_multiplier TEXT    NOT NULL,
    state             TEXT    NOT NULL,
    available_at      INTEGER,
    window_started_at INTEGER,
    last_error        TEXT,
    last_probe_at     INTEGER,
    rpm_limit         INTEGER,
    concurrency_limit INTEGER,
    updated_at        INTEGER NOT NULL,
    PRIMARY KEY (provider, id)
);

CREATE TABLE IF NOT EXISTS gates (
    provider   TEXT    NOT NULL,
    account_id TEXT    NOT NULL,
    kind       TEXT    NOT NULL,
    cap        TEXT    NOT NULL,
    used       TEXT    NOT NULL,
    resets_at  INTEGER,
    confidence TEXT    NOT NULL,
    source     TEXT    NOT NULL,
    checked_at INTEGER NOT NULL,
    stale      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (provider, account_id, kind)
);

CREATE TABLE IF NOT EXISTS requests (
    id                  TEXT PRIMARY KEY,
    ts                  INTEGER NOT NULL,
    session_key         TEXT,
    client_model        TEXT    NOT NULL,
    provider            TEXT    NOT NULL,
    account_id          TEXT    NOT NULL,
    model               TEXT    NOT NULL,
    prompt_tokens       INTEGER NOT NULL,
    completion_tokens   INTEGER NOT NULL,
    cached_tokens       INTEGER NOT NULL,
    reasoning_tokens    INTEGER NOT NULL,
    measured            INTEGER NOT NULL,
    ttft_ms             INTEGER,
    total_ms            INTEGER NOT NULL,
    tps                 REAL,
    cost                TEXT    NOT NULL,
    currency            TEXT    NOT NULL,
    credit_multiplier   TEXT    NOT NULL,
    coverage            TEXT    NOT NULL,
    status              TEXT    NOT NULL,
    retries             INTEGER NOT NULL,
    provider_request_id TEXT
);

CREATE TABLE IF NOT EXISTS attempts (
    request_id     TEXT    NOT NULL,
    attempt_no     INTEGER NOT NULL,
    account_id     TEXT    NOT NULL,
    provider       TEXT    NOT NULL,
    started_at     INTEGER NOT NULL,
    ttft_ms        INTEGER,
    status_code    INTEGER,
    error_class    TEXT,
    error_code     TEXT,
    retry_after_ms INTEGER,
    billed         INTEGER NOT NULL,
    bytes_sent     INTEGER NOT NULL,
    PRIMARY KEY (request_id, attempt_no)
);

CREATE TABLE IF NOT EXISTS usage_daily (
    day               TEXT NOT NULL,
    provider          TEXT NOT NULL,
    account_id        TEXT NOT NULL,
    model             TEXT NOT NULL,
    requests          INTEGER NOT NULL DEFAULT 0,
    prompt_tokens     INTEGER NOT NULL DEFAULT 0,
    completion_tokens INTEGER NOT NULL DEFAULT 0,
    cached_tokens     INTEGER NOT NULL DEFAULT 0,
    cost              TEXT NOT NULL DEFAULT '0',
    currency          TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (day, provider, account_id, model)
);

-- Prices recovered from provider spend, one row per (provider, model).
-- Persisted so a restart does not fall back to the conservative default, which
-- was measured against live providers to be off by an order of magnitude in
-- both directions. The token columns are the usage the learned rate was derived
-- from, needed to compare it against the configured price over the same mix.
CREATE TABLE IF NOT EXISTS prices (
    provider                  TEXT    NOT NULL,
    model                     TEXT    NOT NULL,
    tier                      TEXT    NOT NULL,
    input_per_token           TEXT    NOT NULL,
    output_per_token          TEXT    NOT NULL,
    cache_read_per_token      TEXT    NOT NULL,
    samples                   INTEGER NOT NULL,
    observed_prompt_tokens    INTEGER NOT NULL DEFAULT 0,
    observed_completion_tokens INTEGER NOT NULL DEFAULT 0,
    observed_cached_tokens    INTEGER NOT NULL DEFAULT 0,
    updated_at                INTEGER NOT NULL,
    PRIMARY KEY (provider, model, tier)
);

-- Timestamp alone answers "the newest N requests". The console's usage,
-- requests, latency and failure views add a time range to a provider, model,
-- account or coverage filter, so each of those leads with the filter column and
-- ends with ts; one index per filtered dimension keeps those scans narrow without
-- changing what is stored. attempts is the latency and failure source, filtered
-- by provider over a window.
CREATE INDEX IF NOT EXISTS idx_requests_ts            ON requests(ts);
CREATE INDEX IF NOT EXISTS idx_requests_provider_ts   ON requests(provider, ts);
CREATE INDEX IF NOT EXISTS idx_requests_model_ts      ON requests(model, ts);
CREATE INDEX IF NOT EXISTS idx_requests_account_ts    ON requests(account_id, ts);
CREATE INDEX IF NOT EXISTS idx_requests_coverage_ts   ON requests(coverage, ts);
CREATE INDEX IF NOT EXISTS idx_attempts_provider_started
    ON attempts(provider, started_at);
"#;

/// Bumped whenever a change cannot be applied to existing rows in place.
///
/// 1 -> 2: account identity changed from id alone to (provider, id). Rows
/// written under the old key cannot be attributed to a provider, so the account
/// and gate tables are rebuilt rather than guessed at. Usage history already
/// carried provider and is preserved.
///
/// 8: the console's usage, requests and latency views need indexes on the
/// filtered dimensions. Additive only: the statements are in SCHEMA and
/// IF NOT EXISTS, so an older ledger gains them when it is opened for writing
/// and no row is rewritten.
pub const SCHEMA_VERSION: i64 = 8;

/// The version at which account identity changed from id alone to
/// (provider, id). Only a database older than THIS needs the rebuild; later
/// versions are additive and must not wipe the ledger.
const IDENTITY_CHANGE_VERSION: i64 = 2;

fn schema_version(conn: &Connection) -> Result<i64> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
        [],
    )?;
    let v: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    Ok(v.and_then(|s| s.parse().ok()).unwrap_or(0))
}

fn set_schema_version(conn: &Connection, version: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![version.to_string()],
    )?;
    Ok(())
}

pub fn init_schema(conn: &Connection) -> Result<()> {
    let existing_accounts: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'accounts'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n > 0)
        .unwrap_or(false);
    let version = schema_version(conn)?;

    if existing_accounts && version < IDENTITY_CHANGE_VERSION {
        // Accounts written before version 2 were keyed by the bare account id,
        // so two providers sharing an id are indistinguishable in them and the
        // rows cannot be attributed with certainty. Rebuild rather than guess:
        // a wrong attribution would show one account's quota on another.
        tracing::warn!(
            from = version,
            to = SCHEMA_VERSION,
            "store: account identity changed to (provider, id); rebuilding the account and \
             quota tables. Usage history is preserved."
        );
        conn.execute_batch("DROP TABLE IF EXISTS gates; DROP TABLE IF EXISTS accounts;")
            .context("dropping pre-v2 account tables")?;
    }

    if version < 5 && table_exists(conn, "prices")? {
        // The price columns were renamed when cache read became a price rather
        // than a ratio. These rows are observations that cost nothing to
        // re-learn, so they are rebuilt rather than migrated.
        tracing::warn!("store: rebuilding the learned price table after a column change");
        conn.execute_batch("DROP TABLE IF EXISTS prices;")
            .context("dropping the older price table")?;
    }

    if version < 7 && table_exists(conn, "prices")? {
        // A learned rate used to be identified by the tier's POSITION in the
        // configuration, so reordering two [[tiers]] blocks re-associated every
        // rate with another window's price. The key is the window's name now,
        // and a positional row cannot be mapped to one -- the store does not
        // know the config. Learned prices are re-derived on the next probe.
        tracing::warn!("store: rebuilding the learned price table to key it by window name");
        conn.execute_batch("DROP TABLE IF EXISTS prices;")
            .context("dropping the positionally-keyed price table")?;
    }

    if version < 6 {
        // Four indexes with no reader: account_id and session_key are written
        // and never filtered on, and attempts(request_id) and usage_daily(day)
        // duplicate the leftmost column of their table's primary key. Each one
        // is an insert cost on every ledger write.
        conn.execute_batch(
            "DROP INDEX IF EXISTS idx_requests_account;
             DROP INDEX IF EXISTS idx_requests_session;
             DROP INDEX IF EXISTS idx_attempts_request;
             DROP INDEX IF EXISTS idx_usage_daily_day;",
        )
        .context("dropping the write-only indexes")?;
    }

    conn.execute_batch(SCHEMA)
        .context("applying store schema")?;
    set_schema_version(conn, SCHEMA_VERSION)?;
    Ok(())
}

/// Configure a connection for concurrent, single-writer use.
fn configure(conn: &Connection) -> Result<()> {
    conn.busy_timeout(BUSY_TIMEOUT)
        .context("setting busy_timeout")?;
    // journal_mode returns the resulting mode, which execute_batch tolerates.
    // On an in-memory database this is a no-op that returns "memory".
    conn.pragma_update(None, "journal_mode", "WAL")
        .context("enabling WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .context("setting synchronous=NORMAL")?;
    Ok(())
}

/// Restrict a path to the operator, on the platforms that have modes.
#[cfg(unix)]
fn restrict(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("setting mode {mode:o} on {}", path.display()))
}

#[cfg(not(unix))]
fn restrict(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

/// Whether other users can reach into this directory.
#[cfg(unix)]
fn shared_by_others(dir: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(dir)
        .map(|m| m.permissions().mode() & 0o077 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn shared_by_others(_dir: &Path) -> bool {
    false
}

/// Open (creating if needed) the database and apply the schema.
///
/// The ledger holds request history -- client session keys, account ids, model
/// names, spend -- so it gets the config file's treatment: the directory is
/// created 0700, and the database file 0600 before SQLite ever writes to it.
/// SQLite gives -wal and -shm the database's mode, which is why the database
/// file is the one that has to be private. A ledger written before this rule is
/// pulled in line on the way in; a directory somebody else created is reported
/// rather than chmod'ed, because the configured path may point somewhere shared.
pub fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        if !parent.exists() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating database directory {}", parent.display()))?;
            restrict(parent, 0o700)?;
        } else if shared_by_others(parent) {
            tracing::warn!(
                dir = %parent.display(),
                "the ledger directory is reachable by other users; chmod 700 it to hide the \
                 request history it holds (the database file itself is kept 0600)"
            );
        }
    }
    let created_here = if path.exists() {
        restrict(path, 0o600)?;
        false
    } else {
        true
    };
    if created_here {
        // Created private first rather than chmod'ed after SQLite opened it.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .with_context(|| format!("creating {}", path.display()))?;
        }
    }
    for suffix in ["-wal", "-shm"] {
        let mut sibling = path.as_os_str().to_os_string();
        sibling.push(suffix);
        let sibling = PathBuf::from(sibling);
        if sibling.exists() {
            restrict(&sibling, 0o600)?;
        }
    }
    let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    configure(&conn)?;
    init_schema(&conn)?;
    // A brand-new database keeps its schema pages in the -wal file, leaving the
    // main file zero bytes long. db_exists (and an operator looking at the
    // directory) expects a non-empty file, so fold the WAL back in once here.
    // Best effort: a competing reader can make this return "busy" harmlessly.
    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    Ok(conn)
}

/// Open an existing database read-only, for CLI commands.
fn open_readonly(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {} read-only", path.display()))?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    Ok(conn)
}

/// Messages accepted by the writer task.
#[derive(Debug)]
pub enum WriteMsg {
    /// Replace the learned price table.
    Prices(Vec<crate::meter::PersistedPrice>),
    /// Drop learned prices: all of them, one provider's, or one model's.
    ///
    /// Not expressed as `Prices(vec![])`: that would clear rows this process has
    /// not loaded, and it cannot say "forget just this model" at all.
    ForgetPrices {
        provider: Option<String>,
        model: Option<String>,
    },
    Request(Box<RequestRecord>),
    Attempt(Box<AttemptRecord>),
    Accounts(Vec<Account>),
    /// Upsert exactly ONE account and its gates.
    ///
    /// Separate from Accounts because a full snapshot is destructive: it
    /// upserts every configured account and deletes the gate rows of any whose
    /// in-memory copy has none. "accounts disable X" built its snapshot without
    /// reading the ledger, so disabling one account wiped the quota readings of
    /// every other account in the pool.
    Account(Box<Account>),
    /// Barrier used by flush_blocking and by tests: acknowledge only after
    /// every message queued before this one has been committed. The
    /// acknowledgement carries the first write error, if any.
    Flush(std::sync::mpsc::Sender<std::result::Result<(), String>>),
}

/// How long per-request detail is kept. The daily rollup is permanent.
pub const DETAIL_RETENTION_DAYS: i64 = 30;

/// Delete per-attempt telemetry older than the retention window.
///
/// Only the attempts table is pruned, and the reason is worth writing down:
/// "lgw usage" aggregates the LIVE requests table, not usage_daily (the daily
/// rollup is keyed by provider and day, so it cannot answer the model and
/// account groupings the command offers). Deleting request rows would therefore
/// make a 90-day report quietly return less than the truth, and a number that
/// shrinks without saying so is worse than a table that grows. Requests are kept
/// until there is a per-dimension rollup for them to fall back on.
///
/// Attempts are pure telemetry -- one row per upstream try -- and are where the
/// growth actually comes from once retries and rotations are in play.
pub fn prune_detail(conn: &Connection, at: OffsetDateTime) -> Result<u64> {
    let cutoff = e2i(at - time::Duration::days(DETAIL_RETENTION_DAYS));
    let attempts = conn.execute(
        "DELETE FROM attempts WHERE request_id IN (SELECT id FROM requests WHERE ts < ?1)",
        params![cutoff],
    )? as u64;
    Ok(attempts)
}

/// Fire-and-forget handle to the database writer.
pub struct StoreHandle {
    tx: UnboundedSender<WriteMsg>,
    /// Writes that failed to commit since the process started. Read by
    /// flush_blocking so a shutdown cannot claim success over a broken write.
    failures: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl StoreHandle {
    /// Spawn the writer task against db_path.
    ///
    /// The writer is a plain OS thread rather than a tokio task because
    /// rusqlite is synchronous; send on the unbounded channel never blocks, so
    /// no failure below can stall the request path.
    pub fn spawn(db_path: &Path) -> Result<Self> {
        let conn = open(db_path)?;
        // Retention runs once at startup: it is bounded work (one indexed range
        // delete) and it keeps the ledger from growing forever.
        match prune_detail(&conn, OffsetDateTime::now_utc()) {
            Ok(0) => {}
            Ok(n) => tracing::info!(rows = n, "store: pruned request detail past retention"),
            Err(e) => tracing::warn!(error = %e, "store: could not prune old request detail"),
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<WriteMsg>();
        let failures = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let writer_failures = std::sync::Arc::clone(&failures);
        std::thread::Builder::new()
            .name("lgw-store-writer".to_string())
            .spawn(move || writer_loop(conn, rx, writer_failures))
            .context("spawning store writer thread")?;
        Ok(StoreHandle { tx, failures })
    }

    pub fn record_request(&self, r: RequestRecord) {
        if self.tx.send(WriteMsg::Request(Box::new(r))).is_err() {
            // error!, not warn!: once the writer is gone nothing is persisted,
            // and on Command Code an unpersisted ledger becomes real money.
            tracing::error!("store writer is gone; dropping request record");
        }
    }

    pub fn record_attempt(&self, a: AttemptRecord) {
        if self.tx.send(WriteMsg::Attempt(Box::new(a))).is_err() {
            tracing::error!("store writer is gone; dropping attempt record");
        }
    }

    /// Persist the current pool snapshot.
    ///
    /// Must be called after every probe and every settle; see the module docs
    /// for why the ledger is written eagerly while telemetry is batched.
    ///
    /// Destructive by nature: it replaces the rows of every account it covers,
    /// so an account in the slice that carries no gates loses the gates the
    /// ledger holds for it. Pass a snapshot whose accounts were read from the
    /// pool, never one built from configuration alone.
    pub fn save_accounts(&self, accounts: &[Account]) {
        if self.tx.send(WriteMsg::Accounts(accounts.to_vec())).is_err() {
            tracing::error!("store writer is gone; dropping account ledger snapshot");
        }
    }

    /// Persist ONE account without disturbing any other account's rows.
    ///
    /// This is what a targeted operation such as "accounts disable" must use: a
    /// full-pool snapshot taken from configuration carries no gates, and saving
    /// it deletes the gates of every account it covers.
    pub fn save_account(&self, account: &Account) {
        if self
            .tx
            .send(WriteMsg::Account(Box::new(account.clone())))
            .is_err()
        {
            tracing::error!("store writer is gone; dropping account update");
        }
    }

    /// Forget learned prices, so the configured card is what the model costs
    /// again.
    ///
    /// A running gateway holds its own in-memory table and writes it back on the
    /// next probe, so this only sticks while `lgw serve` is stopped or after it
    /// is restarted; the command that calls this says so.
    pub fn forget_prices(&self, provider: Option<&str>, model: Option<&str>) {
        let msg = WriteMsg::ForgetPrices {
            provider: provider.map(str::to_string),
            model: model.map(str::to_string),
        };
        if self.tx.send(msg).is_err() {
            tracing::error!("store writer is gone; learned prices were not forgotten");
        }
    }

    /// Persist learned prices. Sent after every probe that learned something.
    pub fn save_prices(&self, prices: Vec<crate::meter::PersistedPrice>) {
        if self.tx.send(WriteMsg::Prices(prices)).is_err() {
            tracing::error!("store writer is gone; dropping learned prices");
        }
    }

    /// Block until everything queued so far is committed.
    ///
    /// Reports the writes that FAILED, not just the ones that were queued: a
    /// shutdown that silently swallows a commit failure claims the ledger is on
    /// disk when it is not, and the next start then re-spends quota that was
    /// already consumed.
    pub fn flush_blocking(&self) -> Result<()> {
        let before = self.failures.load(std::sync::atomic::Ordering::SeqCst);
        let (ack_tx, ack_rx) = std::sync::mpsc::channel();
        self.tx
            .send(WriteMsg::Flush(ack_tx))
            .map_err(|_| anyhow::anyhow!("store writer is gone"))?;
        let first_error = ack_rx
            .recv()
            .map_err(|_| anyhow::anyhow!("store writer dropped the flush acknowledgement"))?;
        if let Err(e) = first_error {
            anyhow::bail!("a queued write failed to commit: {e}");
        }
        let after = self.failures.load(std::sync::atomic::Ordering::SeqCst);
        if after > before {
            anyhow::bail!(
                "{} write(s) failed to commit since the last flush",
                after - before
            );
        }
        Ok(())
    }
}

/// Drain the channel, committing messages in batches.
fn writer_loop(
    mut conn: Connection,
    mut rx: UnboundedReceiver<WriteMsg>,
    failures: std::sync::Arc<std::sync::atomic::AtomicU64>,
) {
    loop {
        let Some(first) = rx.blocking_recv() else {
            break;
        };
        let mut batch = vec![first];
        let mut disconnected = false;
        while batch.len() < WRITE_BATCH_MAX {
            match rx.try_recv() {
                Ok(msg) => batch.push(msg),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        apply_batch(&mut conn, batch, &failures);
        if disconnected {
            break;
        }
    }
    // Fold the WAL back into the main file so the database is self-contained
    // after shutdown. Best effort.
    if let Err(e) = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);") {
        tracing::debug!(error = %e, "store: final WAL checkpoint failed");
    }
}

/// Commit one batch.
///
/// Every failure is an ERROR and is counted, then reported through the next
/// flush acknowledgement so a shutdown cannot claim success over a broken
/// write. The acknowledgement is still always sent, so flush_blocking cannot
/// hang on a database that has stopped working.
fn apply_batch(
    conn: &mut Connection,
    batch: Vec<WriteMsg>,
    failures: &std::sync::atomic::AtomicU64,
) {
    type Ack = std::sync::mpsc::Sender<std::result::Result<(), String>>;
    let mut flushes: Vec<Ack> = Vec::new();
    let tx = match conn.transaction() {
        Ok(tx) => tx,
        Err(e) => {
            let message = format!("could not begin a write transaction: {e}");
            tracing::error!("store: {message}");
            failures.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            for msg in &batch {
                if let WriteMsg::Flush(ack) = msg {
                    let _ = ack.send(Err(message.clone()));
                }
            }
            return;
        }
    };

    let mut first_error: Option<String> = None;
    for msg in &batch {
        let outcome = match msg {
            WriteMsg::Request(r) => insert_request(&tx, r),
            WriteMsg::Attempt(a) => insert_attempt(&tx, a),
            WriteMsg::Accounts(accounts) => save_accounts_tx(&tx, accounts),
            WriteMsg::Account(account) => {
                save_accounts_tx(&tx, std::slice::from_ref(account.as_ref()))
            }
            WriteMsg::Prices(prices) => save_prices_tx(&tx, prices),
            WriteMsg::ForgetPrices { provider, model } => {
                forget_prices_tx(&tx, provider.as_deref(), model.as_deref()).map(|_| ())
            }
            WriteMsg::Flush(ack) => {
                flushes.push(ack.clone());
                Ok(())
            }
        };
        if let Err(e) = outcome {
            // A dropped write is not cosmetic: the ledger is exactly what stops
            // the next restart from re-spending quota it already consumed.
            tracing::error!(error = %e, "store: a write failed and was dropped");
            failures.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if first_error.is_none() {
                first_error = Some(format!("{e:#}"));
            }
        }
    }

    if let Err(e) = tx.commit() {
        tracing::error!(error = %e, "store: write transaction commit failed");
        failures.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if first_error.is_none() {
            first_error = Some(format!("commit failed: {e}"));
        }
    }
    for ack in flushes {
        let _ = ack.send(match &first_error {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        });
    }
}

fn insert_request(tx: &Transaction<'_>, r: &RequestRecord) -> Result<()> {
    let inserted = tx.execute(
        "INSERT OR IGNORE INTO requests (
             id, ts, session_key, client_model, provider, account_id, model,
             prompt_tokens, completion_tokens, cached_tokens, reasoning_tokens, measured,
             ttft_ms, total_ms, tps, cost, currency, credit_multiplier, coverage, status,
             retries, provider_request_id
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                   ?17, ?18, ?19, ?20, ?21, ?22)",
        params![
            r.request_id,
            e2i(r.ts),
            r.session_key,
            r.client_model,
            r.provider,
            r.account_id,
            r.model,
            u2i(r.usage.prompt_tokens),
            u2i(r.usage.completion_tokens),
            u2i(r.usage.cached_tokens),
            u2i(r.usage.reasoning_tokens),
            i64::from(r.usage.measured),
            r.ttft_ms.map(u2i),
            u2i(r.total_ms),
            r.tps,
            dec_str(r.cost),
            r.currency,
            dec_str(r.credit_multiplier),
            coverage_str(r.coverage),
            status_str(r.status),
            i64::from(r.retries),
            r.provider_request_id,
        ],
    )?;

    if inserted == 0 {
        // Re-writing a request must not double-count it in usage_daily.
        tracing::warn!(request_id = %r.request_id, "store: duplicate request id; keeping the first row");
        return Ok(());
    }
    bump_usage_daily(tx, r)
}

/// Fold one request into its daily aggregate.
///
/// Counts are incremented in SQL, but money is TEXT (see the module docs), so
/// the cost is read back, added with Decimal, and written again. This is exact:
/// a SUM() over TEXT would coerce it to a float and lose the low-order digits
/// that small per-request costs live in.
fn bump_usage_daily(tx: &Transaction<'_>, r: &RequestRecord) -> Result<()> {
    let day = r.ts.date().to_string();
    let previous: Option<String> = tx
        .query_row(
            "SELECT cost FROM usage_daily
             WHERE day = ?1 AND provider = ?2 AND account_id = ?3 AND model = ?4",
            params![day, r.provider, r.account_id, r.model],
            |row| row.get(0),
        )
        .optional()?;
    let total = previous.as_deref().map(parse_dec).unwrap_or(Decimal::ZERO) + r.cost;

    tx.execute(
        "INSERT INTO usage_daily
             (day, provider, account_id, model, requests, prompt_tokens, completion_tokens,
              cached_tokens, cost, currency)
         VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(day, provider, account_id, model) DO UPDATE SET
             requests          = requests + 1,
             prompt_tokens     = prompt_tokens + excluded.prompt_tokens,
             completion_tokens = completion_tokens + excluded.completion_tokens,
             cached_tokens     = cached_tokens + excluded.cached_tokens,
             cost              = excluded.cost,
             currency          = excluded.currency",
        params![
            day,
            r.provider,
            r.account_id,
            r.model,
            u2i(r.usage.prompt_tokens),
            u2i(r.usage.completion_tokens),
            u2i(r.usage.cached_tokens),
            dec_str(total),
            r.currency,
        ],
    )?;
    Ok(())
}

fn insert_attempt(tx: &Transaction<'_>, a: &AttemptRecord) -> Result<()> {
    tx.execute(
        "INSERT OR REPLACE INTO attempts (
             request_id, attempt_no, account_id, provider, started_at, ttft_ms, status_code,
             error_class, error_code, retry_after_ms, billed, bytes_sent
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            a.request_id,
            i64::from(a.attempt_no),
            a.account_id,
            a.provider,
            e2i(a.started_at),
            a.ttft_ms.map(u2i),
            a.status_code.map(i64::from),
            a.error_class.map(|c| c.as_str()),
            a.error_code,
            a.retry_after_ms.map(u2i),
            i64::from(a.billed),
            u2i(a.bytes_sent),
        ],
    )?;
    Ok(())
}

/// Replace the learned price table.
///
/// Wholesale rather than incremental: the in-memory table is the source of
/// truth and a price that disappeared there must not linger here.
fn save_prices_tx(tx: &Transaction<'_>, prices: &[crate::meter::PersistedPrice]) -> Result<()> {
    let updated_at = OffsetDateTime::now_utc().unix_timestamp();
    tx.execute("DELETE FROM prices", [])?;
    for p in prices {
        tx.execute(
            "INSERT OR REPLACE INTO prices (
                 provider, model, tier, input_per_token, output_per_token,
                 cache_read_per_token, samples, observed_prompt_tokens,
                 observed_completion_tokens, observed_cached_tokens, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                p.provider,
                p.model,
                p.tier.as_key(),
                dec_str(p.price.input_per_token),
                dec_str(p.price.output_per_token),
                dec_str(p.price.cache_read_per_token),
                p.price.samples as i64,
                p.observed_prompt_tokens as i64,
                p.observed_completion_tokens as i64,
                p.observed_cached_tokens as i64,
                updated_at,
            ],
        )?;
    }
    Ok(())
}

/// Drop learned prices.
///
/// Scoped rather than wholesale because the useful case is "this one model was
/// learned from an account that is also used elsewhere -- forget it and go back
/// to the card". A model with no provider cannot be expressed from the command
/// line, but the wildcard case is handled here so the function stays total.
fn forget_prices_tx(
    tx: &Transaction<'_>,
    provider: Option<&str>,
    model: Option<&str>,
) -> Result<u64> {
    let removed = match (provider, model) {
        (Some(provider), Some(model)) => tx.execute(
            "DELETE FROM prices WHERE provider = ?1 AND model = ?2",
            params![provider, model],
        )?,
        (Some(provider), None) => {
            tx.execute("DELETE FROM prices WHERE provider = ?1", params![provider])?
        }
        (None, Some(model)) => tx.execute("DELETE FROM prices WHERE model = ?1", params![model])?,
        (None, None) => tx.execute("DELETE FROM prices", [])?,
    };
    Ok(removed as u64)
}

/// Whether a table exists in this database.
fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        params![name],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Read the learned price table back.
///
/// A ledger written by a build that predates price learning simply has no such
/// table. That is a normal state, not a failure, so it reads as "nothing learned
/// yet" rather than surfacing an error the operator can do nothing about. Any
/// other read failure still propagates.
fn load_prices(conn: &Connection) -> Result<Vec<crate::meter::PersistedPrice>> {
    use crate::meter::{ModelPrice, PersistedPrice};

    if !table_exists(conn, "prices")? {
        return Ok(Vec::new());
    }

    /// provider, model, tier, input, output, cache_read, samples, prompt_tok,
    /// completion_tok, cached_tok
    type PriceRowTuple = (
        String,
        String,
        String,
        String,
        String,
        String,
        i64,
        i64,
        i64,
        i64,
    );
    let rows: Vec<PriceRowTuple> = {
        let mut stmt = conn.prepare(
            "SELECT provider, model, tier, input_per_token, output_per_token,
                    cache_read_per_token, samples, observed_prompt_tokens,
                    observed_completion_tokens, observed_cached_tokens
             FROM prices ORDER BY provider, model, tier",
        )?;
        let mapped = stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
                row.get(9)?,
            ))
        })?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let mut out = Vec::with_capacity(rows.len());
    for (provider, model, tier, input, output, cache_read, samples, p_tok, c_tok, cached_tok) in
        rows
    {
        // A row we cannot parse is skipped, never defaulted: a price that
        // silently became zero would make a paid model look free.
        let (Some(input), Some(output), Some(cache_read)) = (
            crate::domain::parse_money(&input),
            crate::domain::parse_money(&output),
            crate::domain::parse_money(&cache_read),
        ) else {
            tracing::warn!(provider = %provider, model = %model, "store: unreadable price row; skipping");
            continue;
        };
        out.push(PersistedPrice {
            provider,
            model,
            tier: TierKey::from_key(&tier),
            price: ModelPrice {
                input_per_token: input,
                output_per_token: output,
                cache_read_per_token: cache_read,
                samples: samples.max(0) as u64,
            },
            observed_prompt_tokens: p_tok.max(0) as u64,
            observed_completion_tokens: c_tok.max(0) as u64,
            observed_cached_tokens: cached_tok.max(0) as u64,
        });
    }
    Ok(out)
}

/// Upsert accounts and replace their gates, all inside the caller transaction
/// so a reader never observes half a ledger.
fn save_accounts_tx(tx: &Transaction<'_>, accounts: &[Account]) -> Result<()> {
    let updated_at = OffsetDateTime::now_utc().unix_timestamp();
    for a in accounts {
        tx.execute(
            "INSERT INTO accounts (
                 id, provider, label, base_url, currency, credit_multiplier, state,
                 available_at, window_started_at, last_error, last_probe_at, rpm_limit,
                 concurrency_limit, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
             ON CONFLICT(provider, id) DO UPDATE SET
                 label             = excluded.label,
                 base_url          = excluded.base_url,
                 currency          = excluded.currency,
                 credit_multiplier = excluded.credit_multiplier,
                 state             = excluded.state,
                 available_at      = excluded.available_at,
                 window_started_at = excluded.window_started_at,
                 last_error        = excluded.last_error,
                 last_probe_at     = excluded.last_probe_at,
                 rpm_limit         = excluded.rpm_limit,
                 concurrency_limit = excluded.concurrency_limit,
                 updated_at        = excluded.updated_at",
            params![
                a.id,
                a.provider,
                a.label,
                a.base_url,
                a.currency,
                dec_str(a.credit_multiplier),
                state_str(a.state),
                a.available_at.map(e2i),
                a.window_started_at.map(e2i),
                a.last_error,
                a.last_probe_at.map(e2i),
                a.rpm_limit.map(i64::from),
                a.concurrency_limit.map(i64::from),
                updated_at,
            ],
        )?;

        // Gates are a set keyed by kind: replace wholesale so a gate that
        // disappeared upstream cannot linger forever.
        tx.execute(
            "DELETE FROM gates WHERE provider = ?1 AND account_id = ?2",
            params![a.provider, a.id],
        )?;
        for g in &a.gates {
            tx.execute(
                "INSERT OR REPLACE INTO gates (
                     provider, account_id, kind, cap, used, resets_at, confidence, source, checked_at, stale
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    a.provider,
                    a.id,
                    gate_kind_str(g.kind),
                    dec_str(g.cap),
                    dec_str(g.used),
                    g.resets_at.map(e2i),
                    confidence_str(g.confidence),
                    source_str(g.source),
                    e2i(g.checked_at),
                    i64::from(g.stale),
                ],
            )?;
        }
    }
    Ok(())
}

/// Accounts plus gates, for the status command.
fn load_accounts(conn: &Connection) -> Result<Vec<AccountOverview>> {
    type AccountRow = (
        String,
        String,
        String,
        String,
        String,
        Option<i64>,
        Option<i64>,
        Option<String>,
    );

    let rows: Vec<AccountRow> = {
        let mut stmt = conn.prepare(
            "SELECT id, provider, label, currency, state, available_at, window_started_at, last_error
             FROM accounts ORDER BY provider, id",
        )?;
        let mapped = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, Option<String>>(7)?,
            ))
        })?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let mut out = Vec::with_capacity(rows.len());
    for (id, provider, label, currency, state, available_at, window_started_at, last_error) in rows
    {
        let gates = load_gates(conn, &provider, &id)?;
        let state = parse_state(&state).unwrap_or_else(|| {
            // Never guess Active: an unreadable state must not serve traffic.
            tracing::warn!(state = %state, account = %id, "store: unknown account state; using Unknown");
            AccountState::Unknown
        });
        out.push(AccountOverview {
            account_id: id,
            provider,
            label,
            currency,
            state,
            gates,
            available_at: available_at.map(i2t),
            window_started_at: window_started_at.map(i2t),
            last_error,
        });
    }
    Ok(out)
}

fn load_gates(conn: &Connection, provider: &str, account_id: &str) -> Result<Vec<Gate>> {
    let rows = {
        let mut stmt = conn.prepare(
            "SELECT kind, cap, used, resets_at, confidence, source, checked_at, stale
             FROM gates WHERE provider = ?1 AND account_id = ?2 ORDER BY kind",
        )?;
        let mapped = stmt.query_map(params![provider, account_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                dec_at(row, 1)?,
                dec_at(row, 2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
            ))
        })?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let mut gates = Vec::with_capacity(rows.len());
    for (kind, cap, used, resets_at, confidence, source, checked_at, stale) in rows {
        let Some(kind) = parse_gate_kind(&kind) else {
            tracing::warn!(provider = %provider, account = %account_id, kind = %kind, "store: unknown gate kind; skipping row");
            continue;
        };
        gates.push(Gate {
            kind,
            cap,
            used,
            // Same rule as the probe parser: zero is no reset, not 1970. A
            // ledger written before that rule existed still holds those zeros.
            resets_at: resets_at.filter(|secs| *secs > 0).map(i2t),
            confidence: parse_confidence(&confidence).unwrap_or(Confidence::Estimated),
            source: parse_source(&source).unwrap_or(GateSource::LocalLedger),
            checked_at: i2t(checked_at),
            stale: stale != 0,
        });
    }
    Ok(gates)
}

/// A read-only handle to the ledger.
///
/// The write side is StoreHandle; this is the read side, and it exists so a
/// reader asks questions instead of holding a Connection. The schema, the column
/// names and the aggregate SQL stay in this module, and a caller that wants usage
/// by two dimensions no longer writes the query itself.
pub struct Ledger {
    conn: Connection,
}

/// Why a ledger could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    /// Nothing has ever been written here.
    ///
    /// A distinct answer rather than an empty ledger: an empty board reads as
    /// "no accounts" when the truth is "never probed", and on Command Code that
    /// confusion is how real money gets spent.
    #[error("no ledger yet")]
    Missing(PathBuf),
    #[error(transparent)]
    Sql(anyhow::Error),
}

impl Ledger {
    pub fn open(path: &Path) -> std::result::Result<Ledger, LedgerError> {
        if !db_exists(path) {
            return Err(LedgerError::Missing(path.to_path_buf()));
        }
        let conn = open_readonly(path)
            .with_context(|| format!("opening ledger {}", path.display()))
            .map_err(LedgerError::Sql)?;
        Ok(Ledger { conn })
    }

    pub fn accounts(&self) -> Result<Vec<AccountOverview>> {
        load_accounts(&self.conn)
    }

    pub fn prices(&self) -> Result<Vec<crate::meter::PersistedPrice>> {
        load_prices(&self.conn)
    }

    pub fn tail(&self, limit: u32) -> Result<Vec<RequestSummary>> {
        recent_requests(&self.conn, limit)
    }

    /// The Board's live-ish vitals, read from the request history.
    pub fn board_vitals(&self, now: OffsetDateTime) -> Result<BoardVitals> {
        board_vitals(&self.conn, now)
    }

    /// Usage aggregated by the dimensions asked for, in order.
    ///
    /// The grouping key is built by concatenating the dimensions, which SQLite
    /// cannot do in a GROUP BY, so the rows are folded in Rust. Money is folded
    /// there too, because cost is stored as TEXT and a SUM() over it would
    /// silently go through a float.
    pub fn usage_by(&self, since: OffsetDateTime, dims: &[GroupBy]) -> Result<Vec<UsageRow>> {
        usage_rows_multi(&self.conn, since, dims)
    }

    /// The Usage page's report: the same fold as usage_by, with spend split by
    /// coverage, failure counts, daily totals and the 30-day timing summary.
    pub fn usage_report(
        &self,
        now: OffsetDateTime,
        since: OffsetDateTime,
        dims: &[GroupBy],
    ) -> Result<UsageReport> {
        usage_report(&self.conn, now, since, dims)
    }
}

/// The numbers the Board's rail shows: how busy the gateway is right now and
/// how healthy the last day was. One short read over the requests table, never
/// a provider call.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BoardVitals {
    /// Requests recorded in the last minute.
    pub requests_last_minute: u64,
    /// Requests whose last status was not ok in the last hour.
    pub errors_last_hour: u64,
    /// 95th percentile time to first token over the last day, in milliseconds.
    pub p95_ttft_ms: Option<f64>,
    /// Cached tokens over prompt tokens in the last day.
    pub cache_hit: Option<f64>,
}

/// Board vitals as of now, from the request history only.
pub fn board_vitals(conn: &Connection, now: OffsetDateTime) -> Result<BoardVitals> {
    let minutes = e2i(now - time::Duration::minutes(1));
    let hour = e2i(now - time::Duration::hours(1));
    let day = e2i(now - time::Duration::days(1));

    let requests_last_minute: i64 = conn.query_row(
        "SELECT COUNT(*) FROM requests WHERE ts >= ?1",
        params![minutes],
        |row| row.get(0),
    )?;
    let errors_last_hour: i64 = conn.query_row(
        "SELECT COUNT(*) FROM requests WHERE ts >= ?1 AND status != 'ok'",
        params![hour],
        |row| row.get(0),
    )?;

    let mut ttfts: Vec<i64> = {
        let mut stmt =
            conn.prepare("SELECT ttft_ms FROM requests WHERE ts >= ?1 AND ttft_ms IS NOT NULL")?;
        let rows = stmt.query_map(params![day], |row| row.get::<_, i64>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    ttfts.sort_unstable();
    let p95_ttft_ms = if ttfts.is_empty() {
        None
    } else {
        let rank = ((ttfts.len() as f64) * 0.95).ceil() as usize;
        let index = rank.saturating_sub(1).min(ttfts.len() - 1);
        Some(ttfts[index] as f64)
    };

    let (prompt, cached): (Option<i64>, Option<i64>) = conn.query_row(
        "SELECT SUM(prompt_tokens), SUM(cached_tokens) FROM requests WHERE ts >= ?1",
        params![day],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let cache_hit = match (prompt, cached) {
        (Some(prompt), Some(cached)) if prompt > 0 => Some(cached as f64 / prompt as f64),
        _ => None,
    };

    Ok(BoardVitals {
        requests_last_minute: requests_last_minute.max(0) as u64,
        errors_last_hour: errors_last_hour.max(0) as u64,
        p95_ttft_ms,
        cache_hit,
    })
}

/// The column a usage dimension groups by.
///
/// A migration changes this and the queries together; nothing outside this
/// module has to know a column name.
fn group_by_sql(d: GroupBy) -> &'static str {
    match d {
        GroupBy::Provider => "provider",
        GroupBy::Model => "model",
        GroupBy::Account => "account_id",
        GroupBy::Day => "date(ts, 'unixepoch')",
    }
}

/// One usage row with its spend split by coverage, plus failure and timing
/// facts the console can show. Money is kept per currency and never added
/// across currencies.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageFullRow {
    pub key: String,
    pub currency: String,
    pub requests: u64,
    pub failed: u64,
    pub covered_requests: u64,
    pub metered_requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub covered_cost: Decimal,
    pub metered_cost: Decimal,
    /// Spend under a coverage class that is not money (allowances, pacing).
    /// Kept separate so the two money columns never absorb it.
    pub other_cost: Decimal,
}

impl UsageFullRow {
    pub fn total_cost(&self) -> Decimal {
        self.covered_cost + self.metered_cost + self.other_cost
    }

    pub fn cache_hit(&self) -> Option<f64> {
        if self.input_tokens == 0 {
            None
        } else {
            Some(self.cached_tokens as f64 / self.input_tokens as f64)
        }
    }
}

impl From<&UsageFullRow> for UsageRow {
    fn from(row: &UsageFullRow) -> UsageRow {
        UsageRow {
            key: row.key.clone(),
            requests: row.requests,
            input_tokens: row.input_tokens,
            output_tokens: row.output_tokens,
            cached_tokens: row.cached_tokens,
            cost: row.total_cost(),
            currency: row.currency.clone(),
        }
    }
}

/// Timing history is short by design: the ledger keeps it for 30 days, so a
/// range longer than that must say what it is actually showing.
pub const LATENCY_HISTORY_DAYS: i64 = 30;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LatencySummary {
    pub p50_ttft_ms: Option<u64>,
    pub p95_ttft_ms: Option<u64>,
    pub samples: u64,
}

/// Everything the Usage page renders, aggregated once from the ledger.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageReport {
    /// The requested grouping, ordered by key then currency.
    pub rows: Vec<UsageFullRow>,
    /// Requests per UTC day, independent of the requested grouping. This is the
    /// chart's data.
    pub daily: Vec<UsageFullRow>,
    pub latency: LatencySummary,
    /// The start of the window the latency figures cover.
    pub latency_since: Option<OffsetDateTime>,
    /// True when the selected range is longer than the latency history.
    pub latency_truncated: bool,
}

/// Aggregated usage rows for a composite grouping key.
fn usage_rows_multi(
    conn: &Connection,
    since: OffsetDateTime,
    dims: &[GroupBy],
) -> Result<Vec<UsageRow>> {
    Ok(usage_report(conn, crate::domain::now(), since, dims)?
        .rows
        .iter()
        .map(UsageRow::from)
        .collect())
}

fn usage_report(
    conn: &Connection,
    now: OffsetDateTime,
    since: OffsetDateTime,
    dims: &[GroupBy],
) -> Result<UsageReport> {
    let key_expr = if dims.is_empty() {
        "currency".to_string()
    } else {
        dims.iter()
            .map(|d| format!("COALESCE({}, '')", group_by_sql(*d)))
            .collect::<Vec<_>>()
            .join(" || '|' || ")
    };
    let sql = format!(
        "SELECT {key_expr} AS k, currency, prompt_tokens, completion_tokens, cached_tokens, cost,                 coverage, status, ttft_ms, ts          FROM requests WHERE ts >= ?1"
    );

    let latency_since = {
        let cap = now - time::Duration::days(LATENCY_HISTORY_DAYS);
        if since > cap { since } else { cap }
    };
    let mut acc: BTreeMap<(String, String), UsageFullRow> = BTreeMap::new();
    let mut daily: BTreeMap<(String, String), UsageFullRow> = BTreeMap::new();
    let mut ttfts: Vec<u64> = Vec::new();

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
        let coverage = row.get::<_, Option<String>>(6)?.unwrap_or_default();
        let status = row.get::<_, Option<String>>(7)?.unwrap_or_default();
        let ttft = row.get::<_, Option<i64>>(8)?.map(|ms| ms.max(0) as u64);
        let at = i2t(row.get::<_, i64>(9)?);

        let entry = acc
            .entry((key.clone(), currency.clone()))
            .or_insert_with(|| UsageFullRow {
                key,
                currency: currency.clone(),
                ..UsageFullRow::default()
            });
        add_request(entry, prompt, completion, cached, cost, &coverage, &status);

        let day = at.date().to_string();
        let day_entry = daily
            .entry((day.clone(), currency.clone()))
            .or_insert_with(|| UsageFullRow {
                key: day,
                currency,
                ..UsageFullRow::default()
            });
        add_request(
            day_entry, prompt, completion, cached, cost, &coverage, &status,
        );

        if at >= latency_since
            && let Some(ms) = ttft
        {
            ttfts.push(ms);
        }
    }

    let latency = latency_summary(ttfts);
    Ok(UsageReport {
        rows: acc.into_values().collect(),
        daily: daily.into_values().collect(),
        latency,
        latency_since: Some(latency_since),
        latency_truncated: since < latency_since,
    })
}

fn add_request(
    row: &mut UsageFullRow,
    prompt: u64,
    completion: u64,
    cached: u64,
    cost: Decimal,
    coverage: &str,
    status: &str,
) {
    row.requests += 1;
    row.input_tokens += prompt;
    row.output_tokens += completion;
    row.cached_tokens += cached;
    if status != "ok" {
        row.failed += 1;
    }
    match coverage {
        "covered" => {
            row.covered_requests += 1;
            row.covered_cost += cost;
        }
        "metered" => {
            row.metered_requests += 1;
            row.metered_cost += cost;
        }
        _ => row.other_cost += cost,
    }
}

fn latency_summary(mut samples: Vec<u64>) -> LatencySummary {
    if samples.is_empty() {
        return LatencySummary::default();
    }
    samples.sort_unstable();
    let count = samples.len();
    let pick = |share: f64| -> u64 {
        let rank = ((count as f64) * share).ceil() as usize;
        samples[rank.saturating_sub(1).min(count - 1)]
    };
    LatencySummary {
        p50_ttft_ms: Some(pick(0.50)),
        p95_ttft_ms: Some(pick(0.95)),
        samples: count as u64,
    }
}

/// Aggregated usage rows for one dimension, read by the tests.
///
/// The same composite query as the multi-dimension form, so the aggregation and
/// the column names have exactly one implementation.
#[cfg(test)]
fn usage_rows(
    conn: &Connection,
    since: OffsetDateTime,
    group_by: GroupBy,
) -> Result<Vec<UsageRow>> {
    usage_rows_multi(conn, since, &[group_by])
}

/// Most recent requests, newest first.
///
/// ts only has one-second resolution, so ties are broken by insertion order
/// (rowid) to keep the tail stable across calls.
fn recent_requests(conn: &Connection, limit: u32) -> Result<Vec<RequestSummary>> {
    let mut stmt = conn.prepare(
        "SELECT ts, id, provider, account_id, model, prompt_tokens, completion_tokens,
                cached_tokens, ttft_ms, tps, coverage, status
         FROM requests ORDER BY ts DESC, rowid DESC LIMIT ?1",
    )?;
    let mapped = stmt.query_map(params![i64::from(limit)], |row| {
        Ok(RequestSummary {
            ts: i2t(row.get::<_, i64>(0)?),
            request_id: row.get(1)?,
            provider: row.get(2)?,
            account_id: row.get(3)?,
            model: row.get(4)?,
            input_tokens: i2u(row.get::<_, i64>(5)?),
            output_tokens: i2u(row.get::<_, i64>(6)?),
            cached_tokens: i2u(row.get::<_, i64>(7)?),
            ttft_ms: row.get::<_, Option<i64>>(8)?.map(i2u),
            tps: row.get(9)?,
            coverage: row
                .get::<_, Option<String>>(10)?
                .as_deref()
                .and_then(parse_coverage)
                .unwrap_or(GateClass::Metered),
            status: row
                .get::<_, Option<String>>(11)?
                .as_deref()
                .and_then(parse_status)
                .unwrap_or(RequestStatus::Error),
        })
    })?;
    Ok(mapped.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// True when the database file exists and is non-empty.
pub fn db_exists(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.len() > 0)
        .unwrap_or(false)
}

/// Default database location.
pub fn default_db_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(format!("{home}/.light-ai-gateway/gateway.db"))
}

// ---------------------------------------------------------------------------
// Encoding helpers
// ---------------------------------------------------------------------------

fn e2i(t: OffsetDateTime) -> i64 {
    t.unix_timestamp()
}

fn i2t(secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(secs).unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

fn u2i(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn i2u(v: i64) -> u64 {
    v.max(0) as u64
}

fn dec_str(d: Decimal) -> String {
    d.to_string()
}

/// Read a money column. Malformed text is reported and treated as zero rather
/// than panicking or aborting a whole status listing; the columns are NOT NULL
/// in the schema, so a NULL here is already a corruption signal.
fn parse_dec(raw: &str) -> Decimal {
    match crate::domain::parse_money(raw) {
        Some(d) => d,
        None => {
            tracing::warn!(value = %raw, "store: malformed decimal in database; using zero");
            Decimal::ZERO
        }
    }
}

fn dec_at(row: &Row<'_>, idx: usize) -> rusqlite::Result<Decimal> {
    let raw: Option<String> = row.get(idx)?;
    Ok(raw.as_deref().map(parse_dec).unwrap_or(Decimal::ZERO))
}

fn gate_kind_str(k: GateKind) -> &'static str {
    match k {
        GateKind::FiveHour => "five_hour",
        GateKind::Weekly => "weekly",
        GateKind::Monthly => "monthly",
        GateKind::FreeTokens => "free_tokens",
        GateKind::Wallet => "wallet",
        GateKind::PurchasedCredits => "purchased_credits",
        GateKind::Rpm => "rpm",
    }
}

fn parse_gate_kind(s: &str) -> Option<GateKind> {
    Some(match s {
        "five_hour" | "5h" => GateKind::FiveHour,
        "weekly" | "7d" => GateKind::Weekly,
        "monthly" | "1mo" => GateKind::Monthly,
        "free_tokens" | "free" => GateKind::FreeTokens,
        "wallet" => GateKind::Wallet,
        "purchased_credits" | "credits" => GateKind::PurchasedCredits,
        "rpm" => GateKind::Rpm,
        _ => return None,
    })
}

fn state_str(s: AccountState) -> &'static str {
    match s {
        AccountState::Active => "active",
        AccountState::Draining => "draining",
        AccountState::Parked => "parked",
        AccountState::Disabled => "disabled",
        AccountState::Unknown => "unknown",
    }
}

fn parse_state(s: &str) -> Option<AccountState> {
    Some(match s {
        "active" => AccountState::Active,
        "draining" => AccountState::Draining,
        "parked" => AccountState::Parked,
        "disabled" => AccountState::Disabled,
        "unknown" => AccountState::Unknown,
        _ => return None,
    })
}

fn confidence_str(c: Confidence) -> &'static str {
    match c {
        Confidence::Exact => "exact",
        Confidence::Estimated => "estimated",
    }
}

fn parse_confidence(s: &str) -> Option<Confidence> {
    Some(match s {
        "exact" => Confidence::Exact,
        "estimated" => Confidence::Estimated,
        _ => return None,
    })
}

fn source_str(s: GateSource) -> &'static str {
    match s {
        GateSource::Polled => "polled",
        GateSource::ResponseHeader => "response_header",
        GateSource::LocalLedger => "local_ledger",
    }
}

fn parse_source(s: &str) -> Option<GateSource> {
    Some(match s {
        "polled" => GateSource::Polled,
        "response_header" => GateSource::ResponseHeader,
        "local_ledger" => GateSource::LocalLedger,
        _ => return None,
    })
}

fn coverage_str(c: GateClass) -> &'static str {
    match c {
        GateClass::Covered => "covered",
        GateClass::Metered => "metered",
        // Routing never pays from an allowance, so no request carries this yet.
        // Spelled out anyway: the column is a persisted vocabulary, and a class
        // with no spelling would round-trip as nothing the day one does.
        GateClass::TokenAllowance => "token_allowance",
        GateClass::RateLimit => "rate_limit",
    }
}

fn parse_coverage(s: &str) -> Option<GateClass> {
    Some(match s {
        "covered" => GateClass::Covered,
        "metered" => GateClass::Metered,
        "token_allowance" => GateClass::TokenAllowance,
        "rate_limit" => GateClass::RateLimit,
        _ => return None,
    })
}

fn status_str(s: RequestStatus) -> &'static str {
    match s {
        RequestStatus::Ok => "ok",
        RequestStatus::Error => "error",
        RequestStatus::Aborted => "aborted",
    }
}

fn parse_status(s: &str) -> Option<RequestStatus> {
    Some(match s {
        "ok" => RequestStatus::Ok,
        "error" => RequestStatus::Error,
        "aborted" => RequestStatus::Aborted,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ApiKey, TokenUsage};
    use crate::error::UpstreamErrorClass;
    use crate::tier::TierKey;
    use std::str::FromStr;

    /// 2023-11-14, 2023-11-15 and an instant far before both.
    const DAY1: i64 = 1_700_000_000;
    const DAY2: i64 = 1_700_100_000;
    const BEFORE: i64 = 1_000_000_000;

    /// Temporary database directory, removed on drop.
    struct TempDb {
        path: PathBuf,
        dir: PathBuf,
    }

    impl TempDb {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "lgw-store-{}-{}-{}",
                std::process::id(),
                tag,
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            let path = dir.join("gateway.db");
            TempDb { path, dir }
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// The ledger is request history: session keys, account ids, model names,
    /// spend. It is created private from the start, like the config file.
    #[cfg(unix)]
    #[test]
    fn the_ledger_is_created_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDb::new("modes");
        let dir = tmp.dir.join("ledger");
        let db = dir.join("gateway.db");

        let _conn = open(&db).expect("open");

        let mode = |p: &Path| std::fs::metadata(p).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700, "the ledger directory is the operator's");
        assert_eq!(mode(&db), 0o600, "the ledger file is the operator's");
    }

    /// A ledger written before that rule is pulled in line on the way in.
    #[cfg(unix)]
    #[test]
    fn an_existing_ledger_is_tightened_on_open() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDb::new("tighten");
        open(&tmp.path).expect("open");
        std::fs::set_permissions(&tmp.path, std::fs::Permissions::from_mode(0o644))
            .expect("loosen");

        drop(open(&tmp.path).expect("reopen"));

        let mode = std::fs::metadata(&tmp.path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().expect("open memory db");
        configure(&conn).expect("configure");
        init_schema(&conn).expect("schema");
        conn
    }

    fn ts(epoch: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(epoch).expect("valid timestamp")
    }

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).expect("valid decimal")
    }

    fn gate(kind: GateKind, cap: &str, used: &str, resets_at: Option<i64>) -> Gate {
        Gate {
            kind,
            cap: dec(cap),
            used: dec(used),
            resets_at: resets_at.map(ts),
            confidence: Confidence::Exact,
            source: GateSource::Polled,
            checked_at: ts(DAY1),
            stale: false,
        }
    }

    fn account(id: &str) -> Account {
        Account {
            id: id.to_string(),
            provider: "xkiro".to_string(),
            label: format!("label-{id}"),
            base_url: "https://api.xkiro.test/v1".to_string(),
            api_key: ApiKey::new("sk-secret-must-not-be-stored"),
            currency: "USD".to_string(),
            gates: vec![
                gate(GateKind::FiveHour, "10.0", "3.5", Some(DAY1 + 600)),
                Gate {
                    // A stale reading carried over from a failed probe, with no
                    // reset time at all: both must survive the round trip.
                    stale: true,
                    resets_at: None,
                    confidence: Confidence::Estimated,
                    source: GateSource::LocalLedger,
                    ..gate(GateKind::Wallet, "0.000356", "0.000100", None)
                },
            ],
            state: AccountState::Draining,
            available_at: Some(ts(DAY2)),
            window_started_at: Some(ts(DAY1)),
            credit_multiplier: dec("0.000356"),
            rpm_limit: Some(600),
            concurrency_limit: Some(4),
            allow_metered: None,
            last_probe_at: Some(ts(DAY1 + 30)),
            last_error: None,
        }
    }

    fn request(
        id: &str,
        epoch: i64,
        provider: &str,
        account_id: &str,
        model: &str,
        cost: &str,
    ) -> RequestRecord {
        RequestRecord {
            request_id: id.to_string(),
            ts: ts(epoch),
            session_key: Some("sess-1".to_string()),
            client_model: format!("{model}-client"),
            provider: provider.to_string(),
            account_id: account_id.to_string(),
            model: model.to_string(),
            usage: TokenUsage {
                prompt_tokens: 100,
                completion_tokens: 20,
                cached_tokens: 40,
                reasoning_tokens: 5,
                measured: true,
            },
            ttft_ms: Some(123),
            total_ms: 900,
            tps: Some(22.5),
            cost: dec(cost),
            currency: "USD".to_string(),
            credit_multiplier: Decimal::ONE,
            coverage: GateClass::Covered,
            status: RequestStatus::Ok,
            retries: 1,
            provider_request_id: Some(format!("up-{id}")),
        }
    }

    fn insert_req(conn: &mut Connection, r: &RequestRecord) {
        let tx = conn.transaction().expect("begin");
        insert_request(&tx, r).expect("insert request");
        tx.commit().expect("commit");
    }

    fn keys(rows: &[UsageRow]) -> Vec<&str> {
        rows.iter().map(|r| r.key.as_str()).collect()
    }

    #[test]
    fn schema_records_its_version() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), SCHEMA_VERSION);
        // Idempotent: a second call must not rebuild anything.
        init_schema(&conn).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), SCHEMA_VERSION);
    }

    /// The console filters usage rows by provider, model, account and coverage
    /// inside a time range; those scans must stay indexed. A ledger written by
    /// an older build gains the indexes on the next write-open and keeps its rows.
    #[test]
    fn the_console_indexes_arrive_without_rewriting_rows() {
        let db = TempDb::new("console-indexes");
        {
            let mut conn = crate::store::open(&db.path).expect("create");
            insert_req(&mut conn, &request("kept", 1_000, "p", "a", "m", "1"));
            conn.execute_batch(
                "DROP INDEX IF EXISTS idx_requests_provider_ts;
                 DROP INDEX IF EXISTS idx_requests_model_ts;
                 DROP INDEX IF EXISTS idx_requests_account_ts;
                 DROP INDEX IF EXISTS idx_requests_coverage_ts;
                 DROP INDEX IF EXISTS idx_attempts_provider_started;
                 INSERT INTO meta (key, value) VALUES ('schema_version', '7')
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value;",
            )
            .expect("simulate a ledger written before the console indexes");
        }

        let conn = crate::store::open(&db.path).expect("open and migrate");

        let names: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT name FROM sqlite_master WHERE type = 'index' ORDER BY name")
                .expect("prepare");
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .expect("query");
            rows.collect::<rusqlite::Result<Vec<_>>>().expect("collect")
        };
        for expected in [
            "idx_requests_provider_ts",
            "idx_requests_model_ts",
            "idx_requests_account_ts",
            "idx_requests_coverage_ts",
            "idx_attempts_provider_started",
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "missing {expected}: {names:?}"
            );
        }
        assert_eq!(schema_version(&conn).expect("version"), SCHEMA_VERSION);
        let kept: i64 = conn
            .query_row("SELECT COUNT(*) FROM requests WHERE id = 'kept'", [], |r| {
                r.get(0)
            })
            .expect("count kept row");
        assert_eq!(kept, 1, "adding indexes must not rewrite request rows");
    }

    #[test]
    fn schema_creation_is_idempotent() {
        let conn = mem();
        init_schema(&conn).expect("second init must succeed");
        init_schema(&conn).expect("third init must succeed");

        let tables: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
                .expect("prepare");
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .expect("query");
            rows.collect::<rusqlite::Result<Vec<_>>>().expect("collect")
        };
        for expected in ["accounts", "attempts", "gates", "requests", "usage_daily"] {
            assert!(
                tables.iter().any(|t| t == expected),
                "missing table {expected}: {tables:?}"
            );
        }
    }

    #[test]
    fn open_enables_wal_and_busy_timeout() {
        let tmp = TempDb::new("wal");
        let conn = open(&tmp.path).expect("open");
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("journal_mode");
        assert_eq!(mode, "wal");
        let timeout: i64 = conn
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .expect("busy_timeout");
        assert_eq!(timeout, 5_000);
        assert!(
            db_exists(&tmp.path),
            "schema creation must leave a non-empty database file"
        );
    }

    /// Regression: the same account id under two providers.
    ///
    /// The tables were once keyed by account_id alone, so xkiro/acc-1 and
    /// commandcode/acc-1 were one row and one set of gates: whichever was saved
    /// last won, and the board showed one account's allowances on the other.
    #[test]
    fn accounts_with_the_same_id_under_different_providers_stay_separate() {
        let tmp = TempDb::new("shared-id");
        let handle = StoreHandle::spawn(&tmp.path).expect("spawn writer");

        let mut xk = account("acc-1");
        xk.provider = "xkiro".to_string();
        xk.gates = vec![gate(GateKind::FiveHour, "200.0", "20.0", None)];

        let mut cc = account("acc-1");
        cc.provider = "commandcode".to_string();
        cc.gates = vec![gate(GateKind::FiveHour, "14.0", "1.0", None)];

        handle.save_accounts(&[xk, cc]);
        handle.flush_blocking().expect("flush");

        let conn = open(&tmp.path).expect("reopen");
        let loaded = load_accounts(&conn).expect("load");
        assert_eq!(
            loaded.len(),
            2,
            "both accounts must survive, got {loaded:?}"
        );

        let xk_row = loaded.iter().find(|a| a.provider == "xkiro").unwrap();
        assert_eq!(xk_row.gates.len(), 1);
        assert_eq!(xk_row.gates[0].cap, dec("200.0"));
        assert_eq!(xk_row.gates[0].used, dec("20.0"));

        let cc_row = loaded.iter().find(|a| a.provider == "commandcode").unwrap();
        assert_eq!(cc_row.gates.len(), 1, "gates must not be shared");
        assert_eq!(
            cc_row.gates[0].cap,
            dec("14.0"),
            "the other provider's gates overwrote this one"
        );
        assert_eq!(cc_row.gates[0].used, dec("1.0"));
    }

    /// Learned prices must survive a restart: without them every start falls
    /// back to the conservative default, which was measured against live
    /// providers to be off by an order of magnitude in both directions.
    #[test]
    fn reading_prices_from_a_ledger_that_predates_them_is_empty_not_an_error() {
        // A database created before price learning has no prices table. Reading
        // it must yield "nothing learned yet", not an error the operator cannot
        // act on.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE requests (id TEXT PRIMARY KEY);")
            .unwrap();
        let loaded = load_prices(&conn).expect("a missing prices table is not a failure");
        assert!(loaded.is_empty());
    }

    /// Forgetting is scoped, and a scope that matches nothing removes nothing.
    ///
    /// The useful case is one model learned from a shared account, so the scope
    /// has to be exact: "forget xkiro" must not touch commandcode, and "forget
    /// xkiro/m" must leave that provider's other models alone.
    #[test]
    fn forgetting_learned_prices_is_scoped() {
        fn row(provider: &str, model: &str, samples: u64) -> crate::meter::PersistedPrice {
            crate::meter::PersistedPrice {
                provider: provider.to_string(),
                model: model.to_string(),
                tier: TierKey::Default,
                price: crate::meter::ModelPrice {
                    input_per_token: dec("0.0000004"),
                    output_per_token: dec("0.0000004"),
                    cache_read_per_token: dec("0.000000008"),
                    samples,
                },
                observed_prompt_tokens: 10,
                observed_completion_tokens: 10,
                observed_cached_tokens: 0,
            }
        }

        let tmp = TempDb::new("forget");
        let handle = StoreHandle::spawn(&tmp.path).expect("spawn writer");
        handle.save_prices(vec![
            row("xkiro", "m-one", 3),
            row("xkiro", "m-two", 4),
            row("commandcode", "m-one", 5),
        ]);
        handle.flush_blocking().expect("flush");

        // A scope that matches nothing leaves the table alone.
        handle.forget_prices(Some("xkiro"), Some("no-such-model"));
        handle.flush_blocking().expect("flush");
        let conn = open(&tmp.path).expect("reopen");
        assert_eq!(load_prices(&conn).expect("load").len(), 3);

        // One model of one provider.
        handle.forget_prices(Some("xkiro"), Some("m-one"));
        handle.flush_blocking().expect("flush");
        let loaded = load_prices(&open(&tmp.path).expect("reopen")).expect("load");
        assert_eq!(loaded.len(), 2);
        assert!(
            loaded
                .iter()
                .any(|p| p.provider == "commandcode" && p.model == "m-one"),
            "the same model under another provider must survive"
        );

        // One provider: the other model goes, the other provider stays.
        handle.forget_prices(Some("xkiro"), None);
        handle.flush_blocking().expect("flush");
        let loaded = load_prices(&open(&tmp.path).expect("reopen")).expect("load");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].provider, "commandcode");

        // Everything.
        handle.forget_prices(None, None);
        handle.flush_blocking().expect("flush");
        assert!(
            load_prices(&open(&tmp.path).expect("reopen"))
                .expect("load")
                .is_empty()
        );
    }

    #[test]
    fn learned_prices_round_trip_through_the_writer() {
        let tmp = TempDb::new("prices");
        let handle = StoreHandle::spawn(&tmp.path).expect("spawn writer");

        let rows = vec![
            crate::meter::PersistedPrice {
                provider: "xkiro".to_string(),
                model: "z-ai/glm-5.3-flash".to_string(),
                tier: TierKey::Default,
                price: crate::meter::ModelPrice {
                    input_per_token: dec("0.0000004"),
                    output_per_token: dec("0.0000012"),
                    cache_read_per_token: dec("0.1"),
                    samples: 7,
                },
                observed_prompt_tokens: 1_234,
                observed_completion_tokens: 567,
                observed_cached_tokens: 89,
            },
            crate::meter::PersistedPrice {
                provider: "commandcode".to_string(),
                model: "z-ai/glm-5.3-flash".to_string(),
                tier: TierKey::Default,
                price: crate::meter::ModelPrice {
                    input_per_token: dec("0.0000041"),
                    output_per_token: dec("0.0000041"),
                    cache_read_per_token: dec("0.1"),
                    samples: 2,
                },
                observed_prompt_tokens: 43,
                observed_completion_tokens: 25,
                observed_cached_tokens: 0,
            },
        ];
        handle.save_prices(rows.clone());
        handle.flush_blocking().expect("flush");

        let conn = open(&tmp.path).expect("reopen");
        let loaded = load_prices(&conn).expect("load_prices");
        assert_eq!(loaded.len(), 2, "got {loaded:?}");

        // Same model name under two providers must stay two rows.
        let xk = loaded.iter().find(|p| p.provider == "xkiro").unwrap();
        assert_eq!(xk.model, "z-ai/glm-5.3-flash");
        assert_eq!(xk.price.samples, 7);
        assert_eq!(
            xk.price.input_per_token,
            dec("0.0000004"),
            "Decimal precision must survive"
        );
        assert_eq!(xk.observed_prompt_tokens, 1_234);
        assert_eq!(xk.observed_completion_tokens, 567);
        assert_eq!(xk.observed_cached_tokens, 89);

        let cc = loaded.iter().find(|p| p.provider == "commandcode").unwrap();
        assert_eq!(cc.price.samples, 2);
        assert_ne!(
            cc.price.input_per_token, xk.price.input_per_token,
            "providers must not share a price"
        );

        // Saving again replaces wholesale rather than accumulating.
        handle.save_prices(vec![rows[0].clone()]);
        handle.flush_blocking().expect("flush");
        let conn = open(&tmp.path).expect("reopen");
        let after = load_prices(&conn).expect("load_prices");
        assert_eq!(after.len(), 1, "a removed price must not linger: {after:?}");
    }

    /// A zero in the ledger is a reset the provider never reported. Read back
    /// as 1970 it would put every window that has not started into the past.
    #[test]
    fn a_zero_reset_in_the_ledger_reads_back_as_no_reset() {
        let tmp = TempDb::new("zero-reset");
        let handle = StoreHandle::spawn(&tmp.path).expect("spawn writer");
        let mut acct = account("acct-zero");
        acct.gates = vec![gate(GateKind::FiveHour, "14", "0", Some(0))];
        handle.save_accounts(std::slice::from_ref(&acct));
        handle.flush_blocking().expect("flush");

        let conn = open(&tmp.path).expect("reopen for read");
        let stored: Option<i64> = conn
            .query_row(
                "SELECT resets_at FROM gates WHERE account_id = 'acct-zero'",
                [],
                |row| row.get(0),
            )
            .expect("the gate row is there");
        assert_eq!(stored, Some(0), "the zero really is on disk");

        let loaded = load_accounts(&conn).expect("load_accounts");
        assert_eq!(loaded[0].gates[0].resets_at, None);
    }

    #[test]
    fn account_and_gates_round_trip_through_the_writer() {
        let tmp = TempDb::new("accounts");
        let handle = StoreHandle::spawn(&tmp.path).expect("spawn writer");
        let acct = account("acct-1");
        handle.save_accounts(std::slice::from_ref(&acct));
        handle.flush_blocking().expect("flush");

        let conn = open(&tmp.path).expect("reopen for read");
        let loaded = load_accounts(&conn).expect("load_accounts");
        assert_eq!(loaded.len(), 1);
        let got = &loaded[0];
        assert_eq!(got.account_id, "acct-1");
        assert_eq!(got.provider, "xkiro");
        assert_eq!(got.label, "label-acct-1");
        assert_eq!(got.currency, "USD");
        assert_eq!(got.state, AccountState::Draining);
        assert_eq!(got.available_at, Some(ts(DAY2)));
        assert_eq!(got.window_started_at, Some(ts(DAY1)));
        assert_eq!(got.last_error, None);
        assert_eq!(got.gates.len(), 2);

        let five = got
            .gates
            .iter()
            .find(|g| g.kind == GateKind::FiveHour)
            .expect("5h gate");
        assert_eq!(five.cap, dec("10.0"));
        assert_eq!(five.used, dec("3.5"));
        assert_eq!(five.resets_at, Some(ts(DAY1 + 600)));
        assert!(!five.stale);
        assert_eq!(five.confidence, Confidence::Exact);
        assert_eq!(five.source, GateSource::Polled);

        let wallet = got
            .gates
            .iter()
            .find(|g| g.kind == GateKind::Wallet)
            .expect("wallet gate");
        assert!(wallet.stale, "stale flag must survive the round trip");
        assert_eq!(
            wallet.resets_at, None,
            "None resets_at must stay None rather than becoming epoch 0"
        );
        assert_eq!(wallet.cap, dec("0.000356"));
        assert_eq!(wallet.used, dec("0.000100"));
        assert_eq!(wallet.confidence, Confidence::Estimated);
        assert_eq!(wallet.source, GateSource::LocalLedger);

        let columns: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT name FROM pragma_table_info('accounts')")
                .expect("prepare");
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .expect("query");
            rows.collect::<rusqlite::Result<Vec<_>>>().expect("collect")
        };
        assert!(
            !columns.iter().any(|c| c.contains("key")),
            "the API key must never be persisted: {columns:?}"
        );
        let raw: String = conn
            .query_row("SELECT credit_multiplier FROM accounts", [], |row| {
                row.get(0)
            })
            .expect("read multiplier");
        assert_eq!(raw, "0.000356");

        // A later snapshot that no longer contains the wallet gate must drop
        // that row instead of leaving a phantom gate behind.
        let mut trimmed = acct.clone();
        trimmed.gates.truncate(1);
        handle.save_accounts(&[trimmed]);
        handle.flush_blocking().expect("flush");
        let loaded = load_accounts(&conn).expect("reload");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].gates.len(), 1);
        assert_eq!(loaded[0].gates[0].kind, GateKind::FiveHour);

        drop(handle);
    }

    #[test]
    fn decimal_precision_is_preserved_exactly() {
        let tmp = TempDb::new("precision");
        let mut conn = open(&tmp.path).expect("open");
        insert_req(
            &mut conn,
            &request("req-1", DAY1, "xkiro", "acct-1", "m1", "0.000356"),
        );

        let stored: (String, String) = conn
            .query_row(
                "SELECT cost, credit_multiplier FROM requests WHERE id = 'req-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read request");
        assert_eq!(stored.0, "0.000356");
        assert_eq!(stored.1, "1");

        let rows = usage_rows(&conn, ts(0), GroupBy::Provider).expect("usage");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].cost, dec("0.000356"));
        assert_eq!(rows[0].currency, "USD");

        // Two such requests must add without the rounding loss a SQL SUM over
        // TEXT would introduce.
        insert_req(
            &mut conn,
            &request("req-2", DAY1 + 10, "xkiro", "acct-1", "m1", "0.000356"),
        );
        let rows = usage_rows(&conn, ts(0), GroupBy::Provider).expect("usage");
        assert_eq!(rows[0].cost, dec("0.000712"));
        assert_eq!(rows[0].requests, 2);
    }

    #[test]
    fn usage_daily_accumulates_exactly() {
        let mut conn = mem();
        insert_req(
            &mut conn,
            &request("req-1", DAY1, "xkiro", "acct-1", "m1", "0.000356"),
        );
        insert_req(
            &mut conn,
            &request("req-2", DAY1 + 60, "xkiro", "acct-1", "m1", "0.000356"),
        );

        let (count, cost): (i64, String) = conn
            .query_row(
                "SELECT requests, cost FROM usage_daily WHERE provider = 'xkiro'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read usage_daily");
        assert_eq!(count, 2);
        assert_eq!(cost, "0.000712");

        // Re-recording the same request id must not double-count it.
        insert_req(
            &mut conn,
            &request("req-1", DAY1, "xkiro", "acct-1", "m1", "0.000356"),
        );
        let count: i64 = conn
            .query_row(
                "SELECT requests FROM usage_daily WHERE provider = 'xkiro'",
                [],
                |row| row.get(0),
            )
            .expect("read usage_daily");
        assert_eq!(count, 2);
    }

    #[test]
    fn the_usage_report_splits_coverage_and_caps_latency_at_thirty_days() {
        let mut conn = mem();
        let now = ts(2_000_000);
        let recent = now.unix_timestamp();

        let mut covered = request("r1", recent - 3_600, "xkiro", "a1", "m1", "0.75");
        covered.ttft_ms = Some(100);
        insert_req(&mut conn, &covered);

        let mut metered = request("r2", recent - 7_200, "xkiro", "a1", "m1", "0.5");
        metered.coverage = GateClass::Metered;
        metered.status = RequestStatus::Error;
        metered.currency = "CNY".to_string();
        metered.ttft_ms = Some(200);
        insert_req(&mut conn, &metered);

        // Older than the 30-day timing history but inside the selected range.
        let mut old = request("r3", recent - 40 * 86_400, "xkiro", "a1", "m1", "0.1");
        old.ttft_ms = Some(999);
        insert_req(&mut conn, &old);

        let since = now - time::Duration::days(90);
        let report = usage_report(&conn, now, since, &[GroupBy::Provider]).expect("report");
        assert_eq!(report.rows.len(), 2, "one row per currency");
        let usd = report
            .rows
            .iter()
            .find(|r| r.currency == "USD")
            .expect("USD row");
        assert_eq!(usd.requests, 2);
        assert_eq!(usd.covered_requests, 2);
        assert_eq!(usd.metered_requests, 0);
        assert_eq!(usd.covered_cost, dec("0.85"));
        assert_eq!(usd.metered_cost, Decimal::ZERO);
        assert_eq!(usd.failed, 0);
        let cny = report
            .rows
            .iter()
            .find(|r| r.currency == "CNY")
            .expect("CNY row");
        assert_eq!(cny.covered_requests, 0);
        assert_eq!(cny.metered_requests, 1);
        assert_eq!(cny.metered_cost, dec("0.5"));
        assert_eq!(cny.failed, 1);

        assert_eq!(
            report.latency.samples, 2,
            "the 40-day-old request has no timing history"
        );
        assert_eq!(report.latency.p50_ttft_ms, Some(100));
        assert_eq!(report.latency.p95_ttft_ms, Some(200));
        assert!(report.latency_truncated);
        assert_eq!(report.daily.len(), 3, "three UTC days have requests");

        // The CLI's rows still total the same money and counts from the same fold.
        let rows = usage_rows(&conn, since, GroupBy::Provider).expect("cli rows");
        let usd = rows.iter().find(|r| r.currency == "USD").expect("USD");
        assert_eq!(usd.cost, dec("0.85"));
        assert_eq!(usd.requests, 2);
        let cny = rows.iter().find(|r| r.currency == "CNY").expect("CNY");
        assert_eq!(cny.cost, dec("0.5"));
    }

    #[test]
    fn usage_rows_group_by_every_variant() {
        let mut conn = mem();
        insert_req(&mut conn, &request("r1", DAY1, "xkiro", "a1", "m1", "0.5"));
        insert_req(
            &mut conn,
            &request("r2", DAY1 + 60, "xkiro", "a1", "m2", "0.25"),
        );
        insert_req(
            &mut conn,
            &request("r3", DAY2, "deepseek", "d1", "m1", "0.000356"),
        );
        // Outside the window: must never appear in a filtered result.
        insert_req(
            &mut conn,
            &request("r4", BEFORE, "ghost", "ghost", "m0", "9"),
        );
        let since = ts(DAY1);

        let by_provider = usage_rows(&conn, since, GroupBy::Provider).expect("provider");
        assert_eq!(keys(&by_provider), vec!["deepseek", "xkiro"]);
        let xkiro = by_provider
            .iter()
            .find(|r| r.key == "xkiro")
            .expect("xkiro row");
        assert_eq!(xkiro.requests, 2);
        assert_eq!(xkiro.input_tokens, 200);
        assert_eq!(xkiro.output_tokens, 40);
        assert_eq!(xkiro.cached_tokens, 80);
        assert_eq!(xkiro.cost, dec("0.75"));
        let deepseek = by_provider
            .iter()
            .find(|r| r.key == "deepseek")
            .expect("deepseek row");
        assert_eq!(deepseek.requests, 1);
        assert_eq!(deepseek.cost, dec("0.000356"));

        let by_model = usage_rows(&conn, since, GroupBy::Model).expect("model");
        assert_eq!(keys(&by_model), vec!["m1", "m2"]);
        assert_eq!(by_model[0].requests, 2);
        assert_eq!(by_model[0].cost, dec("0.500356"));

        let by_account = usage_rows(&conn, since, GroupBy::Account).expect("account");
        assert_eq!(keys(&by_account), vec!["a1", "d1"]);
        assert_eq!(by_account[0].requests, 2);
        assert_eq!(by_account[0].cost, dec("0.75"));

        let by_day = usage_rows(&conn, since, GroupBy::Day).expect("day");
        let day1 = ts(DAY1).date().to_string();
        let day2 = ts(DAY2).date().to_string();
        assert_eq!(keys(&by_day), vec![day1.as_str(), day2.as_str()]);
        assert_eq!(by_day[0].requests, 2);
        assert_eq!(by_day[0].cost, dec("0.75"));
        assert_eq!(by_day[1].requests, 1);
        assert_eq!(by_day[1].cost, dec("0.000356"));

        // Without a filter the out-of-window request shows up again.
        let all = usage_rows(&conn, ts(0), GroupBy::Provider).expect("all");
        assert_eq!(all.len(), 3);
    }

    /// A price table keyed by the tier's POSITION is rebuilt, not migrated.
    ///
    /// The store cannot map an index to a window name -- only the config knows
    /// the order -- so the old rows could only be guessed at, and a guessed rate
    /// sizes the hard gate for every later request. Learned prices are re-derived
    /// on the next probe.
    #[test]
    fn a_positionally_keyed_price_table_is_rebuilt() {
        let db = TempDb::new("migrate-v7");
        drop(crate::store::open(&db.path).expect("create"));
        {
            // Put the v6 shape back by hand: tier was an INTEGER position.
            let conn = crate::store::open(&db.path).expect("reopen");
            conn.execute_batch(
                "DROP TABLE IF EXISTS prices;
                 CREATE TABLE prices (
                     provider TEXT NOT NULL, model TEXT NOT NULL, tier INTEGER NOT NULL,
                     input_per_token TEXT NOT NULL, output_per_token TEXT NOT NULL,
                     cache_read_per_token TEXT NOT NULL, samples INTEGER NOT NULL,
                     observed_prompt_tokens INTEGER NOT NULL DEFAULT 0,
                     observed_completion_tokens INTEGER NOT NULL DEFAULT 0,
                     observed_cached_tokens INTEGER NOT NULL DEFAULT 0,
                     updated_at INTEGER NOT NULL,
                     PRIMARY KEY (provider, model, tier)
                 );
                 INSERT INTO prices VALUES ('p', 'm', 1, '0.1', '0.2', '0.01', 5, 1, 1, 0, 0);
                 INSERT INTO meta (key, value) VALUES ('schema_version', '6')
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value;",
            )
            .expect("a v6 price table");
        }

        // Opening for write is what migrates.
        drop(crate::store::open(&db.path).expect("migrate"));

        let ledger = Ledger::open(&db.path).expect("open the ledger");
        assert!(
            ledger.prices().expect("prices").is_empty(),
            "a positional rate cannot be mapped to a window, so it is dropped"
        );
        let conn = crate::store::open(&db.path).expect("reopen");
        assert_eq!(schema_version(&conn).expect("version"), SCHEMA_VERSION);
    }

    #[test]
    fn board_vitals_read_the_recent_windows() {
        let mut conn = mem();
        let now = ts(1_000_000);
        let epoch = now.unix_timestamp();

        let mut fresh = request("fresh", epoch - 10, "p", "a", "m", "1");
        fresh.ttft_ms = Some(200);
        insert_req(&mut conn, &fresh);
        let mut broken = request("broken", epoch - 120, "p", "a", "m", "1");
        broken.status = RequestStatus::Error;
        broken.ttft_ms = Some(400);
        insert_req(&mut conn, &broken);
        let mut old = request("old", epoch - 90_000, "p", "a", "m", "1");
        old.ttft_ms = Some(900);
        insert_req(&mut conn, &old);

        let vitals = board_vitals(&conn, now).expect("vitals");
        assert_eq!(vitals.requests_last_minute, 1);
        assert_eq!(vitals.errors_last_hour, 1);
        assert_eq!(vitals.p95_ttft_ms, Some(400.0));
        assert_eq!(vitals.cache_hit, Some(0.4));
    }

    #[test]
    fn empty_board_vitals_are_empty_not_zero_percent() {
        let conn = mem();
        let vitals = board_vitals(&conn, ts(1_000_000)).expect("vitals");
        assert_eq!(vitals.requests_last_minute, 0);
        assert_eq!(vitals.errors_last_hour, 0);
        assert_eq!(vitals.p95_ttft_ms, None);
        assert_eq!(vitals.cache_hit, None);
    }

    #[test]
    fn recent_requests_are_newest_first_and_limited() {
        let mut conn = mem();
        insert_req(&mut conn, &request("old", 1_000, "p", "a", "m", "1"));
        insert_req(&mut conn, &request("mid", 2_000, "p", "a", "m", "1"));
        insert_req(&mut conn, &request("new", 3_000, "p", "a", "m", "1"));

        let all = recent_requests(&conn, 10).expect("recent");
        assert_eq!(
            all.iter()
                .map(|r| r.request_id.as_str())
                .collect::<Vec<_>>(),
            vec!["new", "mid", "old"]
        );

        let limited = recent_requests(&conn, 2).expect("recent");
        assert_eq!(limited.len(), 2);
        assert_eq!(limited[0].request_id, "new");
        assert_eq!(limited[1].request_id, "mid");
        assert_eq!(limited[0].ts, ts(3_000));
        assert_eq!(limited[0].coverage, GateClass::Covered);
        assert_eq!(limited[0].status, RequestStatus::Ok);
        assert_eq!(limited[0].ttft_ms, Some(123));
        assert_eq!(limited[0].tps, Some(22.5));

        assert!(recent_requests(&conn, 0).expect("recent").is_empty());
    }

    #[test]
    fn unknown_account_id_is_never_fabricated() {
        let mut conn = mem();
        insert_req(
            &mut conn,
            &request("r-ghost", DAY1, "ghost-provider", "ghost-account", "m", "1"),
        );

        // The request itself is recorded ...
        let tail = recent_requests(&conn, 10).expect("recent");
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].account_id, "ghost-account");

        // ... but no account and no gate is invented for an id we never saw.
        assert!(load_accounts(&conn).expect("accounts").is_empty());
        let gate_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM gates WHERE account_id = 'ghost-account'",
                [],
                |row| row.get(0),
            )
            .expect("count gates");
        assert_eq!(gate_rows, 0);
    }

    #[test]
    fn attempt_rows_are_persisted() {
        let tmp = TempDb::new("attempts");
        let handle = StoreHandle::spawn(&tmp.path).expect("spawn");
        handle.record_request(request("req-1", DAY1, "xkiro", "a1", "m1", "0.1"));
        handle.record_attempt(AttemptRecord {
            request_id: "req-1".to_string(),
            attempt_no: 2,
            account_id: "a1".to_string(),
            provider: "xkiro".to_string(),
            started_at: ts(DAY1),
            ttft_ms: None,
            status_code: Some(429),
            error_class: Some(UpstreamErrorClass::QuotaExhausted),
            error_code: Some("usage_exceeded".to_string()),
            retry_after_ms: Some(3_600_000),
            billed: true,
            bytes_sent: 4096,
        });
        handle.flush_blocking().expect("flush");

        let conn = open(&tmp.path).expect("open");
        /// attempt_no, ttft_ms, error_class, error_code, retry_after_ms, billed, bytes_sent
        type AttemptRow = (
            i64,
            Option<i64>,
            Option<String>,
            Option<String>,
            Option<i64>,
            i64,
            i64,
        );
        let row: AttemptRow = conn
            .query_row(
                "SELECT attempt_no, ttft_ms, error_class, error_code, retry_after_ms, billed, bytes_sent
                 FROM attempts WHERE request_id = 'req-1'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .expect("read attempt");
        assert_eq!(row.0, 2);
        assert_eq!(row.1, None);
        assert_eq!(row.2.as_deref(), Some("quota_exhausted"));
        assert_eq!(row.3.as_deref(), Some("usage_exceeded"));
        assert_eq!(row.4, Some(3_600_000));
        assert_eq!(row.5, 1);
        assert_eq!(row.6, 4096);

        drop(handle);
    }

    #[test]
    fn readonly_open_reads_committed_data() {
        let tmp = TempDb::new("readonly");
        {
            let mut conn = open(&tmp.path).expect("open");
            insert_req(
                &mut conn,
                &request("req-1", DAY1, "xkiro", "a1", "m1", "0.25"),
            );
        }
        let conn = open_readonly(&tmp.path).expect("readonly open");
        let rows = recent_requests(&conn, 10).expect("recent");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].request_id, "req-1");
        let usage = usage_rows(&conn, ts(0), GroupBy::Provider).expect("usage");
        assert_eq!(usage[0].cost, dec("0.25"));
    }

    /// Disabling one account must not erase the others' quota readings.
    ///
    /// "accounts disable X" saved a full pool snapshot built from configuration.
    /// Those accounts carry no gates and state Unknown, so the upsert deleted
    /// the gate rows of EVERY account it covered and reset their state -- one
    /// account's problem wiped hours of probing for all of them, and the command
    /// only printed the one it had been asked about.
    #[test]
    fn saving_one_account_leaves_another_accounts_gates_alone() {
        let tmp = TempDb::new("single_account_save");
        let handle = StoreHandle::spawn(&tmp.path).expect("spawn writer");
        let mut both = Vec::new();
        for id in ["acct-1", "acct-2"] {
            let mut a = account(id);
            a.state = AccountState::Active;
            both.push(a);
        }
        handle.save_accounts(&both);
        handle.flush_blocking().expect("flush");

        let mut target = account("acct-1");
        target.state = AccountState::Disabled;
        target.available_at = None;
        target.last_error = Some("disabled by operator".to_string());
        handle.save_account(&target);
        handle.flush_blocking().expect("flush");

        let conn = open(&tmp.path).expect("reopen");
        let loaded = load_accounts(&conn).expect("load_accounts");
        let other = loaded
            .iter()
            .find(|a| a.account_id == "acct-2")
            .expect("the untouched account is still there");
        assert_eq!(
            other.gates.len(),
            2,
            "the other account's readings must survive a targeted update"
        );
        assert_eq!(other.state, AccountState::Active);
        let disabled = loaded
            .iter()
            .find(|a| a.account_id == "acct-1")
            .expect("acct-1");
        assert_eq!(disabled.state, AccountState::Disabled);
    }

    /// Old attempt telemetry goes; reported history does not move.
    ///
    /// Nothing ever deleted anything, so the ledger grew for the life of the
    /// machine. Request rows are deliberately kept: "lgw usage" reads them
    /// directly, so trimming them would shrink a 90-day report without saying so.
    #[test]
    fn pruning_drops_old_attempts_and_leaves_reported_history_intact() {
        let tmp = TempDb::new("prune");
        let now = OffsetDateTime::now_utc();
        let long_ago = (now - time::Duration::days(60)).unix_timestamp();
        let recent = now.unix_timestamp();
        {
            let mut conn = open(&tmp.path).expect("open");
            insert_req(
                &mut conn,
                &request("old", long_ago, "xkiro", "a1", "m1", "0.25"),
            );
            insert_req(
                &mut conn,
                &request("new", recent, "xkiro", "a1", "m1", "0.50"),
            );
        }

        let conn = open(&tmp.path).expect("reopen");
        prune_detail(&conn, now).expect("prune");

        let requests: i64 = conn
            .query_row("SELECT COUNT(*) FROM requests", [], |r| r.get(0))
            .expect("count requests");
        assert_eq!(
            requests, 2,
            "request detail is what usage reports are built from"
        );

        let usage = usage_rows(&conn, ts(0), GroupBy::Provider).expect("usage");
        assert_eq!(
            usage[0].cost,
            dec("0.75"),
            "and the reported total is unchanged by pruning"
        );
    }
}
