//! The one Apply sequence: prepare a draft against the file, write it, publish
//! the next configuration and record the result.
//!
//! The Console form and the external file watcher differ only in where the
//! draft comes from and whether the file has already been written. Everything
//! else -- fingerprint checks, document actions, validation, the restart-only
//! guard, the apply lock, the write/publish order and ReloadState -- lives here.

use crate::config::{Config, ConfigViolation};
use crate::config_doc::{
    self, ConfigDocument, DiffLine, DraftAction, DraftChange, DraftError, KeySpec,
};
use crate::http::OperatorBackend;
use crate::http::console::ConsoleContext;
use serde::Deserialize;
use std::path::Path;

/// One edit from the form. A plain value edit omits `action`; the same body
/// also carries whole-account additions and removals so one Apply can express
/// everything the page shows.
#[derive(Debug, Deserialize)]
pub(crate) struct DraftEntry {
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    path: Vec<String>,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    label: Option<String>,
    /// "keychain", "env" or "file".
    #[serde(default)]
    key_source: Option<String>,
    /// The environment variable name or the key file path.
    #[serde(default)]
    key_value: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    alias: Option<String>,
    #[serde(default)]
    input: Option<f64>,
    #[serde(default)]
    output: Option<f64>,
    #[serde(default)]
    cache_read: Option<f64>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    days: Option<String>,
    #[serde(default)]
    hours: Option<String>,
    #[serde(default)]
    tz: Option<String>,
    #[serde(default)]
    index: Option<usize>,
}

fn required(value: Option<String>, message: &str) -> std::result::Result<String, String> {
    match value {
        Some(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(message.to_string()),
    }
}

/// Turn the wire form into document actions, refusing a half-specified one
/// before it can touch the document.
fn draft_actions(entries: Vec<DraftEntry>) -> std::result::Result<Vec<DraftAction>, String> {
    let mut actions = Vec::with_capacity(entries.len());
    for entry in entries {
        match entry.action.as_deref().unwrap_or("set") {
            "set" => actions.push(DraftAction::Set(DraftChange {
                path: entry.path,
                value: entry.value.unwrap_or_default(),
            })),
            "remove" => actions.push(DraftAction::Remove { path: entry.path }),
            "add_account" => {
                let provider = entry
                    .provider
                    .filter(|provider| !provider.trim().is_empty())
                    .ok_or_else(|| "add_account needs a provider".to_string())?;
                let id = entry
                    .id
                    .filter(|id| !id.trim().is_empty())
                    .ok_or_else(|| "add_account needs an account id".to_string())?;
                let key = match entry.key_source.as_deref().unwrap_or("keychain") {
                    "keychain" => KeySpec::Keychain,
                    "env" => KeySpec::Env(
                        entry
                            .key_value
                            .filter(|name| !name.trim().is_empty())
                            .ok_or_else(|| "an env key source needs a variable name".to_string())?,
                    ),
                    "file" => KeySpec::File(
                        entry
                            .key_value
                            .filter(|path| !path.trim().is_empty())
                            .ok_or_else(|| "a file key source needs a path".to_string())?,
                    ),
                    other => return Err(format!("unknown key source {other}")),
                };
                actions.push(DraftAction::AddAccount {
                    provider,
                    id,
                    label: entry.label,
                    key,
                });
            }
            "add_route" => actions.push(DraftAction::AddRoute {
                model: required(entry.model, "a route needs a model")?,
                provider: required(entry.provider, "a route needs a provider")?,
            }),
            "remove_route" => actions.push(DraftAction::RemoveRoute {
                model: required(entry.model, "a route needs a model")?,
            }),
            "add_alias" => actions.push(DraftAction::AddAlias {
                alias: required(entry.alias, "an alias needs a name")?,
                provider: required(entry.provider, "an alias needs a provider")?,
            }),
            "remove_alias" => actions.push(DraftAction::RemoveAlias {
                alias: required(entry.alias, "an alias needs a name")?,
            }),
            "add_price" => actions.push(DraftAction::AddPrice {
                provider: required(entry.provider, "a price needs a provider")?,
                model: required(entry.model, "a price needs a model")?,
                input: entry
                    .input
                    .ok_or_else(|| "a price needs an input rate".to_string())?,
                output: entry
                    .output
                    .ok_or_else(|| "a price needs an output rate".to_string())?,
                cache_read: entry.cache_read,
            }),
            "remove_price" => actions.push(DraftAction::RemovePrice {
                provider: required(entry.provider, "a price needs a provider")?,
                model: required(entry.model, "a price needs a model")?,
            }),
            "add_schedule" => actions.push(DraftAction::AddSchedule {
                provider: required(entry.provider, "a schedule needs a provider")?,
                model: required(entry.model, "a schedule needs a model")?,
                entry: crate::config_doc::ScheduleEntry {
                    name: entry.name,
                    days: required(entry.days, "a schedule needs days")?,
                    hours: required(entry.hours, "a schedule needs hours")?,
                    tz: entry.tz,
                    input: entry
                        .input
                        .ok_or_else(|| "a schedule needs an input rate".to_string())?,
                    output: entry
                        .output
                        .ok_or_else(|| "a schedule needs an output rate".to_string())?,
                    cache_read: entry.cache_read,
                },
            }),
            "remove_schedule" => actions.push(DraftAction::RemoveSchedule {
                provider: required(entry.provider, "a schedule needs a provider")?,
                model: required(entry.model, "a schedule needs a model")?,
                index: entry
                    .index
                    .ok_or_else(|| "a schedule needs its index".to_string())?,
            }),
            other => return Err(format!("unknown draft action {other}")),
        }
    }
    Ok(actions)
}

/// Why a draft could not be prepared. The HTTP layer maps these to status
/// codes; the watcher maps them to one rejected reload.
#[derive(Debug)]
pub enum Failure {
    /// The form was built from a different file than the one on disk.
    Stale,
    /// The file could not be read.
    Read { message: String },
    /// Applying an action to the TOML document failed.
    Document(DraftError),
    /// The edited document is not a valid configuration.
    Invalid(ConfigViolation),
    /// The file parses as TOML but not as a configuration at all.
    Toml { message: String },
    /// The form's JSON draft could not be turned into document actions.
    Wire(String),
}

impl Failure {
    /// A plain message for a non-HTTP surface. The HTTP layer matches the
    /// variants so each failure keeps its 409/422 distinction.
    pub fn message(&self) -> String {
        match self {
            Failure::Stale => "the file changed after the form loaded".to_string(),
            Failure::Read { message } => message.clone(),
            Failure::Document(error) => format!("{}: {}", error.field, error.message),
            Failure::Invalid(violation) => {
                format!("{}: {}", violation.field, violation.message)
            }
            Failure::Toml { message } => message.clone(),
            Failure::Wire(message) => message.clone(),
        }
    }
}

/// A prepared draft. It has been validated and diffed but nothing has been
/// written or published yet.
#[derive(Debug)]
pub struct Prepared {
    pub original: ConfigDocument,
    pub edited: ConfigDocument,
    pub next: Config,
    pub diff: Vec<DiffLine>,
    pub fingerprint: String,
    pub structural: bool,
    /// Fields that only take effect after a restart. The Apply adapter refuses
    /// these; Validate can still show the diff.
    pub restart: Vec<String>,
}

/// What happens after preparation. A Console apply replaces the file and then
/// publishes; an external apply publishes bytes the editor already wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitMode {
    ReplaceFile,
    ExternalFile,
}

#[derive(Debug)]
pub enum CommitError {
    /// The file changed again after preparation. Nothing was written.
    Stale,
    /// A prepared change includes a field that only takes effect on restart.
    RestartOnly(Vec<String>),
    Write(String),
    Publish(String),
}

/// Prepare a Console draft from the form's JSON entries. The wire parser lives
/// here with the document actions so one sealed interface covers the whole draft.
pub fn prepare_request(
    path: &Path,
    fingerprint: &str,
    entries: Vec<DraftEntry>,
    current: &Config,
) -> std::result::Result<Prepared, Failure> {
    let actions = draft_actions(entries).map_err(Failure::Wire)?;
    prepare_draft(path, fingerprint, actions, current)
}

/// Prepare a Console draft. The caller has already parsed the wire entries into
/// actions; this is the one place that turns them into a validated document.
pub fn prepare_draft(
    path: &Path,
    fingerprint: &str,
    actions: Vec<DraftAction>,
    current: &Config,
) -> std::result::Result<Prepared, Failure> {
    let original = ConfigDocument::read(path).map_err(|error| Failure::Read {
        message: error.to_string(),
    })?;
    if original.fingerprint() != fingerprint {
        return Err(Failure::Stale);
    }
    prepare_actions(original, &actions, current)
}

/// Prepare an edit the watcher read from disk. The file is already the edited
/// document, so there are no actions to apply.
pub fn prepare_external(bytes: &str, current: &Config) -> std::result::Result<Prepared, Failure> {
    let original = ConfigDocument::parse(bytes).map_err(|error| Failure::Read {
        message: error.to_string(),
    })?;
    prepare_actions(original, &[], current)
}

fn prepare_actions(
    original: ConfigDocument,
    actions: &[DraftAction],
    current: &Config,
) -> std::result::Result<Prepared, Failure> {
    let edited = original.apply_actions(actions).map_err(Failure::Document)?;
    let next: Config = toml::from_str(edited.raw()).map_err(|error| Failure::Toml {
        message: error.to_string(),
    })?;
    if let Some(violation) = next.violation() {
        return Err(Failure::Invalid(violation));
    }
    let restart = restart_only_changes(current, &next);
    let diff = config_doc::diff(&original, &edited);
    let fingerprint = edited.fingerprint();
    let structural = actions
        .iter()
        .any(|action| !matches!(action, DraftAction::Set(_)));
    Ok(Prepared {
        original,
        edited,
        next,
        diff,
        fingerprint,
        structural,
        restart,
    })
}

/// Commit a prepared edit under the apply lock. This is the only place that
/// writes the file, publishes a snapshot and records ReloadState.
pub async fn commit(
    backend: &dyn OperatorBackend,
    console: &ConsoleContext,
    prepared: &Prepared,
    mode: CommitMode,
) -> std::result::Result<(), CommitError> {
    if !prepared.restart.is_empty() {
        return Err(CommitError::RestartOnly(prepared.restart.clone()));
    }
    let path = Path::new(&console.config_path);
    let _serialized = console.apply_lock.lock().await;

    // An editor may have saved between preparation and this lock. Re-read the
    // file and refuse to publish bytes that no longer match what was prepared.
    let current = std::fs::read_to_string(path)
        .map_err(|error| CommitError::Write(format!("reading {}: {error}", path.display())))?;
    if config_doc::fingerprint(&current) != prepared.original.fingerprint() {
        return Err(CommitError::Stale);
    }

    let mut write = || -> std::io::Result<()> {
        match mode {
            CommitMode::ReplaceFile => config_doc::write_atomic(path, prepared.edited.raw())
                .map_err(|error| std::io::Error::other(error.to_string())),
            CommitMode::ExternalFile => Ok(()),
        }
    };
    backend
        .apply_config(prepared.next.clone(), &mut write)
        .map_err(|error| CommitError::Publish(error.to_string()))?;

    if let Ok(mut state) = console.reload.lock() {
        state.applied(&prepared.fingerprint, crate::domain::now());
    }
    Ok(())
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
