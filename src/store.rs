//! SQLite persistence.
//!
//! FILE OWNER: teammate store-dev. Replace stub bodies; keep public signatures.
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
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Row, Transaction};
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
    tier                      INTEGER NOT NULL,
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

CREATE INDEX IF NOT EXISTS idx_requests_ts       ON requests(ts);
CREATE INDEX IF NOT EXISTS idx_requests_account  ON requests(account_id);
CREATE INDEX IF NOT EXISTS idx_requests_session  ON requests(session_key);
CREATE INDEX IF NOT EXISTS idx_attempts_request  ON attempts(request_id);
CREATE INDEX IF NOT EXISTS idx_usage_daily_day   ON usage_daily(day);
"#;

/// Apply the schema. Safe to call repeatedly.
/// Bumped whenever a change cannot be applied to existing rows in place.
///
/// 1 -> 2: account identity changed from id alone to (provider, id). Rows
/// written under the old key cannot be attributed to a provider, so the account
/// and gate tables are rebuilt rather than guessed at. Usage history already
/// carried provider and is preserved.
pub const SCHEMA_VERSION: i64 = 5;

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
            "store: account identity changed to (provider, id); rebuilding the              account and quota tables. Usage history is preserved."
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
            .context("dropping the pre-v4 price table")?;
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

/// Open (creating if needed) the database and apply the schema.
pub fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating database directory {}", parent.display()))?;
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
pub fn open_readonly(path: &Path) -> Result<Connection> {
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
    Request(Box<RequestRecord>),
    Attempt(Box<AttemptRecord>),
    Accounts(Vec<Account>),
    /// Barrier used by flush_blocking and by tests: acknowledge only after
    /// every message queued before this one has been committed.
    Flush(std::sync::mpsc::Sender<()>),
}

/// Fire-and-forget handle to the database writer.
pub struct StoreHandle {
    tx: UnboundedSender<WriteMsg>,
}

impl StoreHandle {
    /// Spawn the writer task against db_path.
    ///
    /// The writer is a plain OS thread rather than a tokio task because
    /// rusqlite is synchronous; send on the unbounded channel never blocks, so
    /// no failure below can stall the request path.
    pub fn spawn(db_path: &Path) -> Result<Self> {
        let conn = open(db_path)?;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<WriteMsg>();
        std::thread::Builder::new()
            .name("lai-gw-store-writer".to_string())
            .spawn(move || writer_loop(conn, rx))
            .context("spawning store writer thread")?;
        Ok(StoreHandle { tx })
    }

    pub fn record_request(&self, r: RequestRecord) {
        if self.tx.send(WriteMsg::Request(Box::new(r))).is_err() {
            tracing::warn!("store writer is gone; dropping request record");
        }
    }

    pub fn record_attempt(&self, a: AttemptRecord) {
        if self.tx.send(WriteMsg::Attempt(Box::new(a))).is_err() {
            tracing::warn!("store writer is gone; dropping attempt record");
        }
    }

    /// Persist the current pool snapshot.
    ///
    /// Must be called after every probe and every settle; see the module docs
    /// for why the ledger is written eagerly while telemetry is batched.
    pub fn save_accounts(&self, accounts: &[Account]) {
        if self
            .tx
            .send(WriteMsg::Accounts(accounts.to_vec()))
            .is_err()
        {
            tracing::warn!("store writer is gone; dropping account ledger snapshot");
        }
    }

    /// Persist learned prices. Sent after every probe that learned something.
    pub fn save_prices(&self, prices: Vec<crate::meter::PersistedPrice>) {
        if self.tx.send(WriteMsg::Prices(prices)).is_err() {
            tracing::warn!("store writer is gone; dropping learned prices");
        }
    }

    /// Block until everything queued so far is committed. Used on shutdown and
    /// by tests; not meant to run on the request path.
    pub fn flush_blocking(&self) -> Result<()> {
        let (ack_tx, ack_rx) = std::sync::mpsc::channel();
        self.tx
            .send(WriteMsg::Flush(ack_tx))
            .map_err(|_| anyhow::anyhow!("store writer is gone"))?;
        ack_rx
            .recv()
            .map_err(|_| anyhow::anyhow!("store writer dropped the flush acknowledgement"))?;
        Ok(())
    }
}

/// Drain the channel, committing messages in batches.
fn writer_loop(mut conn: Connection, mut rx: UnboundedReceiver<WriteMsg>) {
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
        apply_batch(&mut conn, batch);
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

/// Commit one batch. Individual failures are logged and skipped; a flush
/// acknowledgement is always sent so flush_blocking cannot hang.
fn apply_batch(conn: &mut Connection, batch: Vec<WriteMsg>) {
    let mut flushes: Vec<std::sync::mpsc::Sender<()>> = Vec::new();
    let tx = match conn.transaction() {
        Ok(tx) => tx,
        Err(e) => {
            tracing::warn!(error = %e, "store: could not begin a write transaction");
            for msg in &batch {
                if let WriteMsg::Flush(ack) = msg {
                    let _ = ack.send(());
                }
            }
            return;
        }
    };

    for msg in &batch {
        let outcome = match msg {
            WriteMsg::Request(r) => insert_request(&tx, r),
            WriteMsg::Attempt(a) => insert_attempt(&tx, a),
            WriteMsg::Accounts(accounts) => save_accounts_tx(&tx, accounts),
            WriteMsg::Prices(prices) => save_prices_tx(&tx, prices),
            WriteMsg::Flush(ack) => {
                flushes.push(ack.clone());
                Ok(())
            }
        };
        if let Err(e) = outcome {
            tracing::warn!(error = %e, "store: dropping a failed write");
        }
    }

    if let Err(e) = tx.commit() {
        tracing::warn!(error = %e, "store: write transaction commit failed");
    }
    for ack in flushes {
        let _ = ack.send(());
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
    let total = previous
        .as_deref()
        .map(parse_dec)
        .unwrap_or(Decimal::ZERO)
        + r.cost;

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
                i64::from(p.tier),
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
pub fn load_prices(conn: &Connection) -> Result<Vec<crate::meter::PersistedPrice>> {
    use crate::meter::{ModelPrice, PersistedPrice};

    if !table_exists(conn, "prices")? {
        return Ok(Vec::new());
    }

    /// provider, model, tier, input, output, cache_read, samples, prompt_tok,
    /// completion_tok, cached_tok
    type PriceRowTuple = (String, String, i64, String, String, String, i64, i64, i64, i64);
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
    for (provider, model, tier, input, output, cache_read, samples, p_tok, c_tok, cached_tok) in rows
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
            tier: tier.max(0) as u32,
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
pub fn load_accounts(conn: &Connection) -> Result<Vec<AccountOverview>> {
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
            resets_at: resets_at.map(i2t),
            confidence: parse_confidence(&confidence).unwrap_or(Confidence::Estimated),
            source: parse_source(&source).unwrap_or(GateSource::LocalLedger),
            checked_at: i2t(checked_at),
            stale: stale != 0,
        });
    }
    Ok(gates)
}

/// Aggregated usage rows.
///
/// The grouping key comes from GroupBy::as_sql_expr(), so the CLI contract and
/// the SQL cannot drift apart. Integer token counters are summed in SQLite;
/// money is folded in Rust because cost is stored as TEXT and a SUM() over it
/// would silently go through a float.
pub fn usage_rows(
    conn: &Connection,
    since: OffsetDateTime,
    group_by: GroupBy,
) -> Result<Vec<UsageRow>> {
    let expr = group_by.as_sql_expr();
    let since_ts = since.unix_timestamp();

    let count_sql = format!(
        "SELECT {expr} AS k, currency, COUNT(*), \
                COALESCE(SUM(prompt_tokens), 0), COALESCE(SUM(completion_tokens), 0), \
                COALESCE(SUM(cached_tokens), 0) \
         FROM requests WHERE ts >= ?1 GROUP BY k, currency"
    );
    let mut counts: BTreeMap<(String, String), (u64, u64, u64, u64)> = BTreeMap::new();
    {
        let mut stmt = conn.prepare(&count_sql)?;
        let mapped = stmt.query_map(params![since_ts], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?;
        for row in mapped {
            let (key, currency, requests, prompt, completion, cached) = row?;
            counts.insert(
                (key, currency),
                (i2u(requests), i2u(prompt), i2u(completion), i2u(cached)),
            );
        }
    }

    let money_sql = format!("SELECT {expr} AS k, currency, cost FROM requests WHERE ts >= ?1");
    let mut costs: BTreeMap<(String, String), Decimal> = BTreeMap::new();
    {
        let mut stmt = conn.prepare(&money_sql)?;
        let mapped = stmt.query_map(params![since_ts], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                row.get::<_, Option<String>>(2)?,
            ))
        })?;
        for row in mapped {
            let (key, currency, cost) = row?;
            let amount = cost.as_deref().map(parse_dec).unwrap_or(Decimal::ZERO);
            *costs.entry((key, currency)).or_insert(Decimal::ZERO) += amount;
        }
    }

    let mut out = Vec::with_capacity(counts.len());
    for ((key, currency), (requests, input_tokens, output_tokens, cached_tokens)) in counts {
        let cost = costs
            .remove(&(key.clone(), currency.clone()))
            .unwrap_or(Decimal::ZERO);
        out.push(UsageRow {
            key,
            requests,
            input_tokens,
            output_tokens,
            cached_tokens,
            cost,
            currency,
        });
    }
    Ok(out)
}

/// Most recent requests, newest first.
///
/// ts only has one-second resolution, so ties are broken by insertion order
/// (rowid) to keep the tail stable across calls.
pub fn recent_requests(conn: &Connection, limit: u32) -> Result<Vec<RequestSummary>> {
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
    std::fs::metadata(path).map(|m| m.len() > 0).unwrap_or(false)
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
        GateClass::RateLimit => "rate_limit",
    }
}

fn parse_coverage(s: &str) -> Option<GateClass> {
    Some(match s {
        "covered" => GateClass::Covered,
        "metered" => GateClass::Metered,
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
                "lai-gw-store-{}-{}-{}",
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
        assert_eq!(loaded.len(), 2, "both accounts must survive, got {loaded:?}");

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

    #[test]
    fn learned_prices_round_trip_through_the_writer() {
        let tmp = TempDb::new("prices");
        let handle = StoreHandle::spawn(&tmp.path).expect("spawn writer");

        let rows = vec![
            crate::meter::PersistedPrice {
                provider: "xkiro".to_string(),
                model: "z-ai/glm-5.3-flash".to_string(),
                tier: 0,
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
                tier: 0,
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
        insert_req(&mut conn, &request("r4", BEFORE, "ghost", "ghost", "m0", "9"));
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
        type AttemptRow = (i64, Option<i64>, Option<String>, Option<String>, Option<i64>, i64, i64);
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
}
