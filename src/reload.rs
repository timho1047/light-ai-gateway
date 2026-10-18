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

use crate::config::Config;
use crate::config_doc::ConfigDocument;
use crate::http::ProxyBackend;
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
        state.last_checked = Some(fingerprint.to_string());
        state.last_success = Some(at);
    }
}

/// One poll of the file. Public for tests; the running gateway calls it from
/// spawn() on a one-second cadence.
pub async fn reload_once(
    backend: &dyn ProxyBackend,
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

    let document = match ConfigDocument::parse(&bytes) {
        Ok(document) => document,
        Err(error) => return reject(console, &fingerprint, &error.to_string()),
    };
    let next: Config = match toml::from_str(document.raw()) {
        Ok(config) => config,
        Err(error) => return reject(console, &fingerprint, &error.to_string()),
    };
    if let Some(violation) = next.violation() {
        return reject(
            console,
            &fingerprint,
            &format!("{}: {}", violation.field, violation.message),
        );
    }

    let restart = restart_only_changes(backend.runtime().config(), &next);
    if !restart.is_empty() {
        return reject(
            console,
            &fingerprint,
            &format!(
                "these fields need a restart, so the running configuration is unchanged: {}",
                restart.join(", ")
            ),
        );
    }

    // Same serialised critical section as a console apply.
    let _serialized = console.apply_lock.lock().await;
    // The editor may have saved again while we waited. Leave that newer file to
    // the next tick rather than publishing bytes we did not fingerprint.
    let current = match std::fs::read_to_string(path) {
        Ok(current) => current,
        Err(error) => {
            return reject(
                console,
                &fingerprint,
                &format!("reading {}: {error}", path.display()),
            );
        }
    };
    if crate::config_doc::fingerprint(&current) != fingerprint {
        return Outcome::Unchanged;
    }
    let mut external_write = || Ok(());
    let applied = backend.apply_config(next, &mut external_write);
    match applied {
        Ok(()) => {
            if let Ok(mut state) = console.reload.lock() {
                state.last_checked = Some(fingerprint.clone());
                state.last_error = None;
                state.last_success = Some(OffsetDateTime::now_utc());
            }
            tracing::info!(
                config = %path.display(),
                fingerprint = %fingerprint,
                "configuration file changed outside the console; new snapshot published"
            );
            Outcome::Applied { fingerprint }
        }
        Err(error) => reject(console, &fingerprint, &error.to_string()),
    }
}

/// Poll the file forever. A failed poll is recorded in ReloadState and the
/// running snapshot keeps serving; a later fixed file is picked up normally.
pub fn spawn(
    backend: Arc<dyn ProxyBackend>,
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
        if !fingerprint.is_empty() {
            state.last_checked = Some(fingerprint.to_string());
        }
        state.last_error = Some(ReloadError {
            fingerprint: fingerprint.to_string(),
            message: message.to_string(),
            at: OffsetDateTime::now_utc(),
        });
    }
    Outcome::Rejected {
        fingerprint: fingerprint.to_string(),
        message: message.to_string(),
    }
}

/// Fields that only take effect on restart. Compared against the running
/// configuration, so a file that already differed from it is not mistaken for a
/// new edit.
pub fn restart_only_changes(old: &Config, new: &Config) -> Vec<String> {
    let mut fields = Vec::new();
    if old.server.listen != new.server.listen {
        fields.push("server.listen".to_string());
    }
    if old.server.db_path != new.server.db_path {
        fields.push("server.db_path".to_string());
    }
    let key_changed = match (&old.server.local_key, &new.server.local_key) {
        (None, None) => false,
        (Some(a), Some(b)) => !a.same_source(b),
        _ => true,
    };
    if key_changed {
        fields.push("server.local_key".to_string());
    }
    if old.policy.max_body_bytes != new.policy.max_body_bytes {
        fields.push("policy.max_body_bytes".to_string());
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::*;

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
