//! The one Apply sequence: prepare a draft against the file, write it, publish
//! the next configuration and record the result.
//!
//! The Console form and the external file watcher differ only in where the
//! draft comes from and whether the file has already been written. Everything
//! else -- fingerprint checks, document actions, validation, the restart-only
//! guard, the apply lock, the write/publish order and ReloadState -- lives here.

use crate::config::{Config, ConfigViolation};
use crate::config_doc::{self, ConfigDocument, DiffLine, DraftAction, DraftError};
use crate::editable::FieldClass;
use crate::http::ProxyBackend;
use crate::http::console::ConsoleContext;
use std::path::Path;

/// Why a draft could not be prepared. The HTTP layer maps these to status
/// codes; the watcher maps them to one rejected reload.
#[derive(Debug)]
pub enum Failure {
    /// The form was built from a different file than the one on disk.
    Stale,
    /// The file could not be read.
    Read { message: String },
    /// A path the editable-field policy does not allow from the Console.
    Scope { fields: Vec<String> },
    /// Applying an action to the TOML document failed.
    Document(DraftError),
    /// The edited document is not a valid configuration.
    Invalid(ConfigViolation),
    /// The file parses as TOML but not as a configuration at all.
    Toml { message: String },
}

impl Failure {
    /// A plain message for a non-HTTP surface. The HTTP layer matches the
    /// variants so each failure keeps its 409/422 distinction.
    pub fn message(&self) -> String {
        match self {
            Failure::Stale => "the file changed after the form loaded".to_string(),
            Failure::Read { message } => message.clone(),
            Failure::Scope { fields } => format!(
                "these paths cannot be applied from the console: {}",
                fields.join(", ")
            ),
            Failure::Document(error) => format!("{}: {}", error.field, error.message),
            Failure::Invalid(violation) => {
                format!("{}: {}", violation.field, violation.message)
            }
            Failure::Toml { message } => message.clone(),
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
    Write(String),
    Publish(String),
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
    let refuse = scope_refusals(&actions);
    if !refuse.is_empty() {
        return Err(Failure::Scope { fields: refuse });
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
    let restart = crate::reload::restart_only_changes(current, &next);
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

/// The sections the form may write. Secrets are not refused here: the document
/// editor gives them a field-specific message, which is more useful than a
/// generic scope error.
fn scope_refusals(actions: &[DraftAction]) -> Vec<String> {
    let mut fields: Vec<String> = Vec::new();
    let mut refuse = |field: String| {
        if !fields.contains(&field) {
            fields.push(field);
        }
    };
    for action in actions {
        match action {
            DraftAction::Set(change) => match crate::editable::field(&change.path) {
                None => refuse(change.path.join(".")),
                Some(policy) if policy.class == FieldClass::ReadOnly && !policy.secret => {
                    refuse(change.path.join("."));
                }
                Some(_) => {}
            },
            DraftAction::Remove { path } => {
                let is_account = path.first().map(String::as_str) == Some("providers")
                    && path.get(2).map(String::as_str) == Some("accounts");
                if !is_account {
                    refuse(path.join("."));
                }
            }
            DraftAction::AddAccount { provider, id, .. } => {
                if provider.trim().is_empty() || id.trim().is_empty() {
                    refuse("providers".to_string());
                }
            }
            DraftAction::AddRoute { .. }
            | DraftAction::RemoveRoute { .. }
            | DraftAction::AddAlias { .. }
            | DraftAction::RemoveAlias { .. }
            | DraftAction::AddPrice { .. }
            | DraftAction::RemovePrice { .. }
            | DraftAction::AddSchedule { .. }
            | DraftAction::RemoveSchedule { .. } => {}
        }
    }
    fields
}

/// Commit a prepared edit under the apply lock. This is the only place that
/// writes the file, publishes a snapshot and records ReloadState.
pub async fn commit(
    backend: &dyn ProxyBackend,
    console: &ConsoleContext,
    path: &Path,
    prepared: &Prepared,
    mode: CommitMode,
) -> std::result::Result<(), CommitError> {
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
        state.last_checked = Some(prepared.fingerprint.clone());
        state.last_error = None;
        state.last_success = Some(crate::domain::now());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_refuses_unknown_paths_and_read_only_conveniences() {
        let actions = vec![
            DraftAction::Set(crate::config_doc::DraftChange {
                path: vec!["no_such_section".to_string(), "field".to_string()],
                value: "x".to_string(),
            }),
            DraftAction::Set(crate::config_doc::DraftChange {
                path: vec![
                    "providers".to_string(),
                    "mock".to_string(),
                    "allow_metered".to_string(),
                ],
                value: "true".to_string(),
            }),
        ];
        assert_eq!(
            scope_refusals(&actions),
            vec![
                "no_such_section.field".to_string(),
                "providers.mock.allow_metered".to_string()
            ]
        );
    }

    #[test]
    fn a_secret_is_not_a_scope_refusal() {
        let actions = vec![DraftAction::Set(crate::config_doc::DraftChange {
            path: vec![
                "providers".to_string(),
                "mock".to_string(),
                "accounts".to_string(),
                "a1".to_string(),
                "key".to_string(),
            ],
            value: "sk".to_string(),
        })];
        assert!(scope_refusals(&actions).is_empty());
    }
}
