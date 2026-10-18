//! Watching the configuration file for edits made outside the console.
//!
//! The console's apply path and this watcher converge on the same primitive:
//! read the file, fingerprint it, validate it, build the next snapshot, publish
//! it under the same apply lock. The watcher never writes: the operator's editor
//! already did, and rewriting the file would replace their formatting.
//!
//! Change detection is content, not timestamps. An editor that writes the same
//! bytes (or a console apply that has just written them) is not a new event, and
//! a rename-based save is just a different inode with new bytes.

use crate::http::OperatorBackend;
use crate::http::console::ConsoleContext;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use time::OffsetDateTime;

/// The last failure and the last success the watcher (or the console) knows
/// about. The configuration page reads this to explain a file the gateway is
/// refusing; the reconciler reads it so one bad save is reported once.
#[derive(Debug, Clone, Default)]
pub struct ReloadState {
    /// Fingerprint of the last file this process tried to load, successfully or
    /// not. Equal content is never handled twice.
    pub last_checked: Option<String>,
    /// Why the last external edit did not become live, if it did not.
    pub last_error: Option<ReloadError>,
    /// When a file was last made live through this process.
    pub last_success: Option<OffsetDateTime>,
}

impl ReloadState {
    /// A fresh process loaded this file; there is no rejection yet.
    pub fn loaded(&mut self, fingerprint: &str, at: OffsetDateTime) {
        self.last_checked = Some(fingerprint.to_string());
        self.last_success = Some(at);
        self.last_error = None;
    }

    /// An Apply made these bytes live.
    pub fn applied(&mut self, fingerprint: &str, at: OffsetDateTime) {
        self.last_checked = Some(fingerprint.to_string());
        self.last_success = Some(at);
        self.last_error = None;
    }

    /// A file was read and refused. The previous success stays as history.
    pub fn rejected(&mut self, fingerprint: &str, message: &str) {
        if !fingerprint.is_empty() {
            self.last_checked = Some(fingerprint.to_string());
        }
        self.last_error = Some(ReloadError {
            fingerprint: fingerprint.to_string(),
            message: message.to_string(),
            at: OffsetDateTime::now_utc(),
        });
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReloadError {
    pub fingerprint: String,
    pub message: String,
    pub at: OffsetDateTime,
}

pub type SharedReload = Arc<Mutex<ReloadState>>;

pub fn new_shared() -> SharedReload {
    Arc::new(Mutex::new(ReloadState::default()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The same bytes were already handled, or the file changed again while the
    /// watcher waited for the apply lock.
    Unchanged,
    Applied {
        fingerprint: String,
    },
    Rejected {
        fingerprint: String,
        message: String,
    },
}

/// Record the file a fresh process just loaded. Without this the first tick
/// would apply the same file a second time.
pub fn mark_loaded(shared: &SharedReload, fingerprint: &str, at: OffsetDateTime) {
    if let Ok(mut state) = shared.lock() {
        state.loaded(fingerprint, at);
    }
}

/// One poll of the file. Public for tests; the running gateway calls it from
/// spawn() on a one-second cadence.
pub async fn reload_once(
    backend: &dyn OperatorBackend,
    path: &Path,
    console: &ConsoleContext,
) -> Outcome {
    let bytes = match std::fs::read_to_string(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            let message = format!("reading {}: {error}", path.display());
            return reject(console, "", &message);
        }
    };
    let fingerprint = crate::config_doc::fingerprint(&bytes);
    {
        let state = lock(console);
        if state.last_checked.as_deref() == Some(fingerprint.as_str()) {
            return Outcome::Unchanged;
        }
    }

    let prepared = match crate::http::apply::prepare_external(&bytes, backend.runtime().config()) {
        Ok(prepared) => prepared,
        Err(error) => return reject(console, &fingerprint, &error.message()),
    };
    match crate::http::apply::commit(
        backend,
        console,
        &prepared,
        crate::http::apply::CommitMode::ExternalFile,
    )
    .await
    {
        Ok(()) => {
            tracing::info!(
                config = %path.display(),
                fingerprint = %fingerprint,
                "configuration file changed outside the console; new snapshot published"
            );
            Outcome::Applied { fingerprint }
        }
        Err(crate::http::apply::CommitError::Stale) => Outcome::Unchanged,
        Err(crate::http::apply::CommitError::RestartOnly(fields)) => reject(
            console,
            &fingerprint,
            &format!(
                "these fields need a restart, so the running configuration is unchanged: {}",
                fields.join(", ")
            ),
        ),
        Err(crate::http::apply::CommitError::Write(message))
        | Err(crate::http::apply::CommitError::Publish(message)) => {
            reject(console, &fingerprint, &message)
        }
    }
}

/// Poll the file forever. A failed poll is recorded in ReloadState and the
/// running snapshot keeps serving; a later fixed file is picked up normally.
pub fn spawn(
    backend: Arc<dyn OperatorBackend>,
    path: PathBuf,
    console: ConsoleContext,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let outcome = reload_once(backend.as_ref(), &path, &console).await;
            if let Outcome::Rejected { message, .. } = outcome {
                tracing::warn!(
                    config = %path.display(),
                    error = %message,
                    "external configuration edit was not applied"
                );
            }
        }
    })
}

fn lock(console: &ConsoleContext) -> std::sync::MutexGuard<'_, ReloadState> {
    console
        .reload
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn reject(console: &ConsoleContext, fingerprint: &str, message: &str) -> Outcome {
    {
        let mut state = lock(console);
        state.rejected(fingerprint, message);
    }
    Outcome::Rejected {
        fingerprint: fingerprint.to_string(),
        message: message.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use crate::http::apply::restart_only_changes;

    #[test]
    fn restart_only_fields_are_named() {
        let old = crate::config::Config::from_toml_str(
            "[server]\nlisten = \"127.0.0.1:8787\"\n[providers.mock]\nadapter = \"none\"\nbase_url = \"http://127.0.0.1:1/v1\"\n[providers.mock.accounts.a1]\nkey = \"k\"\n",
        )
        .expect("config");
        let mut next = old.clone();
        next.server.listen = "127.0.0.1:9999".to_string();
        assert_eq!(restart_only_changes(&old, &next), vec!["server.listen"]);
        assert!(restart_only_changes(&old, &old).is_empty());
    }
}
