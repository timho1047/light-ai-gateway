//! The configuration file as a document, not as a parsed model.
//!
//! The console edits the file the operator already maintains, so it needs the
//! file's own shape: comments, key order and fields this build does not know.
//! This module is the one place that reads it as a TOML document, fingerprints
//! its exact bytes and renders a view with every literal secret redacted.
//!
//! Loading a `Config` and reading a `ConfigDocument` are different acts: the
//! former decides whether the gateway can run, the latter decides what the
//! console may show and, later, what a save should preserve.

use anyhow::{Context, Result};
use std::path::Path;
use toml_edit::{DocumentMut, Item, Value};

use crate::editable::{FieldClass, ValueKind};

/// One read of the configuration file, kept as bytes-shaped TOML.
#[derive(Debug)]
pub struct ConfigDocument {
    raw: String,
    document: DocumentMut,
}

impl ConfigDocument {
    pub fn read(path: &Path) -> Result<ConfigDocument> {
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        ConfigDocument::parse(&raw)
    }

    pub fn parse(raw: &str) -> Result<ConfigDocument> {
        let document = raw
            .parse::<DocumentMut>()
            .context("parsing the configuration as TOML")?;
        Ok(ConfigDocument {
            raw: raw.to_string(),
            document,
        })
    }

    /// Exact bytes as read, for a fingerprint and for a save to compare against.
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// A stable, cheap content identity. Enough to tell a form that the file
    /// moved underneath it; not a security boundary.
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.raw)
    }

    pub fn document(&self) -> &DocumentMut {
        &self.document
    }

    /// The document as text with every literal credential replaced.
    ///
    /// A source from the environment or the credential store is not a secret, so
    /// it survives; only a key written into the file itself is hidden. The
    /// replacement keeps the value's whitespace and trailing comment, so the
    /// view still reads like the file it is showing.
    pub fn redacted(&self) -> String {
        let mut document = self.document.clone();
        redact(&mut document);
        document.to_string()
    }
}

/// FNV-1a over the exact bytes: cheap, stable across machines, and only ever
/// compared for change, never for authenticity.
pub fn fingerprint(raw: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in raw.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn redact(document: &mut DocumentMut) {
    if let Some(server) = document.get_mut("server").and_then(Item::as_table_like_mut)
        && let Some(item) = server.get_mut("local_key")
    {
        redact_item(item);
    }

    let Some(providers) = document
        .get_mut("providers")
        .and_then(Item::as_table_like_mut)
    else {
        return;
    };
    for (_, provider) in providers.iter_mut() {
        let Some(provider) = provider.as_table_like_mut() else {
            continue;
        };
        let Some(accounts) = provider
            .get_mut("accounts")
            .and_then(Item::as_table_like_mut)
        else {
            continue;
        };
        for (_, account) in accounts.iter_mut() {
            let Some(account) = account.as_table_like_mut() else {
                continue;
            };
            if let Some(item) = account.get_mut("key") {
                redact_item(item);
            }
        }
    }
}

fn redact_item(item: &mut Item) {
    let Some(value) = item.as_value() else {
        return;
    };
    if !matches!(value, Value::String(_)) {
        // A table form names an environment variable, a file or the credential
        // store; only the bare string is a secret written into the file.
        return;
    }
    let decoration = value.decor().clone();
    let mut redacted = Value::from("<redacted>");
    *redacted.decor_mut() = decoration;
    *item = Item::Value(redacted);
}

/// Replace a file in one step, with mode 0600 from the moment it exists.
///
/// The bytes go to a sibling temp file first, are flushed to disk, and only then
/// are renamed over the target. A reader (including this process after a
/// restart) sees either the whole old file or the whole new one. The writer is
/// shared by the console's apply and by `lgw init`; a partial configuration is
/// never a state either of them can leave behind.
pub fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.toml");
    let temporary = parent.join(format!(
        ".{file_name}.tmp-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));

    let written = (|| -> Result<()> {
        #[cfg(unix)]
        let mut file = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)
                .with_context(|| format!("creating {}", temporary.display()))?
        };
        #[cfg(not(unix))]
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| format!("creating {}", temporary.display()))?;
        file.write_all(contents.as_bytes())
            .with_context(|| format!("writing {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("flushing {}", temporary.display()))?;
        drop(file);
        std::fs::rename(&temporary, path)
            .with_context(|| format!("replacing {}", path.display()))?;
        #[cfg(unix)]
        if let Ok(directory) = std::fs::File::open(parent) {
            let _ = directory.sync_all();
        }
        Ok(())
    })();

    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

/// Where a newly added account's key comes from. The console never accepts a
/// literal secret: the file names a source, and the credential stays out of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySpec {
    Env(String),
    File(String),
    Keychain,
}

/// One edit to the configuration document.
#[derive(Debug, Clone, PartialEq)]
pub enum DraftAction {
    Set(DraftChange),
    /// Remove a whole account table. Only accounts can be removed this way; a
    /// provider, route or price needs its own operation in a later task.
    Remove {
        path: Vec<String>,
    },
    AddAccount {
        provider: String,
        id: String,
        label: Option<String>,
        key: KeySpec,
    },
    AddRoute {
        model: String,
        provider: String,
    },
    RemoveRoute {
        model: String,
    },
    AddAlias {
        alias: String,
        provider: String,
    },
    RemoveAlias {
        alias: String,
    },
    AddPrice {
        provider: String,
        model: String,
        input: f64,
        output: f64,
        cache_read: Option<f64>,
    },
    RemovePrice {
        provider: String,
        model: String,
    },
    AddSchedule {
        provider: String,
        model: String,
        entry: ScheduleEntry,
    },
    RemoveSchedule {
        provider: String,
        model: String,
        index: usize,
    },
}

impl ConfigDocument {
    /// Apply a mixed list of edits to a copy and parse the result back. The
    /// parse is the guarantee: a document the gateway cannot load is a refused
    /// draft, never a broken file.
    pub fn apply_actions(&self, actions: &[DraftAction]) -> Result<ConfigDocument, DraftError> {
        fn rebuilt(document: DocumentMut) -> ConfigDocument {
            ConfigDocument {
                raw: document.to_string(),
                document,
            }
        }
        let mut document = self.document.clone();
        for action in actions {
            match action {
                DraftAction::Set(change) => apply_change(&mut document, change)?,
                DraftAction::Remove { path } => remove_path(&mut document, path)?,
                DraftAction::AddAccount {
                    provider,
                    id,
                    label,
                    key,
                } => add_account(&mut document, provider, id, label.as_deref(), key)?,
                DraftAction::AddRoute { model, provider } => {
                    document = rebuilt(document).add_route(model, provider)?.document;
                }
                DraftAction::RemoveRoute { model } => {
                    document = rebuilt(document).remove_route(model)?.document;
                }
                DraftAction::AddAlias { alias, provider } => {
                    document = rebuilt(document).add_alias(alias, provider)?.document;
                }
                DraftAction::RemoveAlias { alias } => {
                    document = rebuilt(document).remove_alias(alias)?.document;
                }
                DraftAction::AddPrice {
                    provider,
                    model,
                    input,
                    output,
                    cache_read,
                } => {
                    document = rebuilt(document)
                        .add_price(provider, model, *input, *output, *cache_read)?
                        .document;
                }
                DraftAction::RemovePrice { provider, model } => {
                    document = rebuilt(document).remove_price(provider, model)?.document;
                }
                DraftAction::AddSchedule {
                    provider,
                    model,
                    entry,
                } => {
                    document = rebuilt(document)
                        .add_schedule(provider, model, entry)?
                        .document;
                }
                DraftAction::RemoveSchedule {
                    provider,
                    model,
                    index,
                } => {
                    document = rebuilt(document)
                        .remove_schedule(provider, model, *index)?
                        .document;
                }
            }
        }
        let raw = document.to_string();
        ConfigDocument::parse(&raw).map_err(|error| DraftError {
            field: "config".to_string(),
            message: error.to_string(),
        })
    }
}

fn remove_path(document: &mut DocumentMut, path: &[String]) -> Result<(), DraftError> {
    let field = path.join(".");
    let [providers, provider, accounts, id] = path else {
        return Err(DraftError {
            field,
            message: "only a whole account can be removed here".to_string(),
        });
    };
    if providers != "providers" || accounts != "accounts" {
        return Err(DraftError {
            field,
            message: "only a whole account can be removed here".to_string(),
        });
    }
    let provider_table = document
        .get_mut("providers")
        .and_then(Item::as_table_like_mut)
        .and_then(|providers| providers.get_mut(provider))
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| DraftError {
            field: field.clone(),
            message: format!("no provider {provider}"),
        })?;
    let accounts_table = provider_table
        .get_mut("accounts")
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| DraftError {
            field: field.clone(),
            message: "this provider has no accounts table".to_string(),
        })?;
    if accounts_table.remove(id).is_none() {
        return Err(DraftError {
            field,
            message: format!("no account {id}"),
        });
    }
    if accounts_table.is_empty() {
        provider_table.remove("accounts");
    }
    Ok(())
}

fn add_account(
    document: &mut DocumentMut,
    provider: &str,
    id: &str,
    label: Option<&str>,
    key: &KeySpec,
) -> Result<(), DraftError> {
    let field = format!("providers.{provider}.accounts.{id}");
    if id.trim().is_empty() || id.contains('/') {
        return Err(DraftError {
            field: "providers".to_string(),
            message: "an account id must be non-empty and must not contain a slash".to_string(),
        });
    }
    let provider_table = document
        .get_mut("providers")
        .and_then(Item::as_table_like_mut)
        .and_then(|providers| providers.get_mut(provider))
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| DraftError {
            field: "providers".to_string(),
            message: format!("no provider {provider}"),
        })?;
    if provider_table.get("accounts").is_none() {
        provider_table.insert("accounts", Item::Table(toml_edit::Table::new()));
    }
    let accounts = provider_table
        .get_mut("accounts")
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| DraftError {
            field: field.clone(),
            message: "accounts is not a table".to_string(),
        })?;
    if accounts.get(id).is_some() {
        return Err(DraftError {
            field,
            message: "an account with that id already exists".to_string(),
        });
    }
    let mut account = toml_edit::Table::new();
    account.insert("key", Item::Value(key_value(key)));
    if let Some(label) = label.filter(|label| !label.trim().is_empty()) {
        account.insert("label", Item::Value(Value::from(label)));
    }
    accounts.insert(id, Item::Table(account));
    Ok(())
}

fn key_value(key: &KeySpec) -> Value {
    let mut table = toml_edit::InlineTable::new();
    match key {
        KeySpec::Env(name) => {
            table.insert("env", Value::from(name.as_str()));
        }
        KeySpec::File(path) => {
            table.insert("key_file", Value::from(path.as_str()));
        }
        KeySpec::Keychain => {
            table.insert("keychain", Value::from(true));
        }
    }
    Value::InlineTable(table)
}

/// One field the configuration form wants to change: a path from the document
/// root (a key per segment) and the text the operator typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftChange {
    pub path: Vec<String>,
    pub value: String,
}

/// A draft that cannot be applied to the document, named by the field it
/// touches. The message is technical detail; the console localises the field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftError {
    pub field: String,
    pub message: String,
}

impl ConfigDocument {
    /// Apply changes to a copy of this document.
    ///
    /// Only existing leaves can be changed, and the leaf's own type decides how
    /// the text is parsed: an integer stays an integer, a boolean stays a
    /// boolean. Nothing is added or removed, so comments, key order, unknown
    /// fields and array-of-table blocks all survive by construction. The result
    /// is parsed back, which is what makes a malformed edit an error instead of
    /// a broken file.
    pub fn with_draft(&self, changes: &[DraftChange]) -> Result<ConfigDocument, DraftError> {
        let mut document = self.document.clone();
        for change in changes {
            apply_change(&mut document, change)?;
        }
        let raw = document.to_string();
        ConfigDocument::parse(&raw).map_err(|error| DraftError {
            field: "config".to_string(),
            message: error.to_string(),
        })
    }
}

/// One schedule row as the form supplies it; the validator decides whether the
/// day and hour strings are usable.
#[derive(Debug, Clone, PartialEq)]
pub struct ScheduleEntry {
    pub name: Option<String>,
    pub days: String,
    pub hours: String,
    pub tz: Option<String>,
    pub input: f64,
    pub output: f64,
    pub cache_read: Option<f64>,
}

impl ConfigDocument {
    pub fn add_route(&self, model: &str, provider: &str) -> Result<ConfigDocument, DraftError> {
        self.mutate(|document| {
            let table = named_table(document, "routes", "routes")?;
            table.insert(model, Item::Value(Value::from(provider)));
            Ok(())
        })
    }

    pub fn remove_route(&self, model: &str) -> Result<ConfigDocument, DraftError> {
        self.mutate(|document| {
            let table = named_table(document, "routes", "routes")?;
            if table.remove(model).is_none() {
                return Err(DraftError {
                    field: format!("routes.{model}"),
                    message: "no such route".to_string(),
                });
            }
            if table.is_empty() {
                document.as_table_mut().remove("routes");
            }
            Ok(())
        })
    }

    pub fn add_alias(&self, alias: &str, provider: &str) -> Result<ConfigDocument, DraftError> {
        self.mutate(|document| {
            let table = named_table(
                document,
                "provider_prefix_aliases",
                "provider_prefix_aliases",
            )?;
            table.insert(alias, Item::Value(Value::from(provider)));
            Ok(())
        })
    }

    pub fn remove_alias(&self, alias: &str) -> Result<ConfigDocument, DraftError> {
        self.mutate(|document| {
            let table = named_table(
                document,
                "provider_prefix_aliases",
                "provider_prefix_aliases",
            )?;
            if table.remove(alias).is_none() {
                return Err(DraftError {
                    field: format!("provider_prefix_aliases.{alias}"),
                    message: "no such alias".to_string(),
                });
            }
            if table.is_empty() {
                document.as_table_mut().remove("provider_prefix_aliases");
            }
            Ok(())
        })
    }

    pub fn add_price(
        &self,
        provider: &str,
        model: &str,
        input: f64,
        output: f64,
        cache_read: Option<f64>,
    ) -> Result<ConfigDocument, DraftError> {
        self.mutate(|document| {
            let table = price_table(document, provider, model)?;
            table.insert("input", Item::Value(Value::from(input)));
            table.insert("output", Item::Value(Value::from(output)));
            if let Some(cache_read) = cache_read {
                table.insert("cache_read", Item::Value(Value::from(cache_read)));
            }
            Ok(())
        })
    }

    pub fn remove_price(&self, provider: &str, model: &str) -> Result<ConfigDocument, DraftError> {
        self.mutate(|document| {
            let providers = document
                .get_mut("prices")
                .and_then(Item::as_table_like_mut)
                .ok_or_else(|| DraftError {
                    field: "prices".to_string(),
                    message: "no prices configured".to_string(),
                })?;
            let table = providers
                .get_mut(provider)
                .and_then(Item::as_table_like_mut)
                .ok_or_else(|| DraftError {
                    field: format!("prices.{provider}"),
                    message: "no prices for this provider".to_string(),
                })?;
            if table.remove(model).is_none() {
                return Err(DraftError {
                    field: format!("prices.{provider}.{model}"),
                    message: "no price for this model".to_string(),
                });
            }
            if table.is_empty() {
                providers.remove(provider);
            }
            if providers.is_empty() {
                document.as_table_mut().remove("prices");
            }
            Ok(())
        })
    }

    pub fn add_schedule(
        &self,
        provider: &str,
        model: &str,
        entry: &ScheduleEntry,
    ) -> Result<ConfigDocument, DraftError> {
        self.mutate(|document| {
            let table = price_table(document, provider, model)?;
            if table.get("schedules").is_none() {
                table.insert(
                    "schedules",
                    Item::ArrayOfTables(toml_edit::ArrayOfTables::new()),
                );
            }
            let schedules = table
                .get_mut("schedules")
                .and_then(Item::as_array_of_tables_mut)
                .ok_or_else(|| DraftError {
                    field: format!("prices.{provider}.{model}.schedules"),
                    message: "schedules is not an array of tables".to_string(),
                })?;
            schedules.push(schedule_table(entry));
            Ok(())
        })
    }

    pub fn remove_schedule(
        &self,
        provider: &str,
        model: &str,
        index: usize,
    ) -> Result<ConfigDocument, DraftError> {
        self.mutate(|document| {
            let field = format!("prices.{provider}.{model}.schedules");
            let table = document
                .get_mut("prices")
                .and_then(Item::as_table_like_mut)
                .and_then(|prices| prices.get_mut(provider))
                .and_then(Item::as_table_like_mut)
                .and_then(|prices| prices.get_mut(model))
                .and_then(Item::as_table_like_mut)
                .ok_or_else(|| DraftError {
                    field: field.clone(),
                    message: "no price for this model".to_string(),
                })?;
            let schedules = table
                .get_mut("schedules")
                .and_then(Item::as_array_of_tables_mut)
                .ok_or_else(|| DraftError {
                    field: field.clone(),
                    message: "no schedules".to_string(),
                })?;
            if index >= schedules.len() {
                return Err(DraftError {
                    field: format!("{field}.{index}"),
                    message: "no such schedule".to_string(),
                });
            }
            schedules.remove(index);
            if schedules.is_empty() {
                table.remove("schedules");
            }
            Ok(())
        })
    }

    /// Mutate a copy of the document and parse the result back. Every structural
    /// action goes through here, so a malformed edit is a refused draft rather
    /// than a broken file.
    fn mutate(
        &self,
        edit: impl FnOnce(&mut DocumentMut) -> Result<(), DraftError>,
    ) -> Result<ConfigDocument, DraftError> {
        let mut document = self.document.clone();
        edit(&mut document)?;
        let raw = document.to_string();
        ConfigDocument::parse(&raw).map_err(|error| DraftError {
            field: "config".to_string(),
            message: error.to_string(),
        })
    }
}

fn named_table<'a>(
    document: &'a mut DocumentMut,
    name: &str,
    field: &str,
) -> Result<&'a mut dyn toml_edit::TableLike, DraftError> {
    if document.get(name).is_none() {
        document.insert(name, Item::Table(toml_edit::Table::new()));
    }
    document
        .get_mut(name)
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| DraftError {
            field: field.to_string(),
            message: format!("{name} is not a table"),
        })
}

fn price_table<'a>(
    document: &'a mut DocumentMut,
    provider: &str,
    model: &str,
) -> Result<&'a mut dyn toml_edit::TableLike, DraftError> {
    let field = format!("prices.{provider}.{model}");
    let prices = named_table(document, "prices", "prices")?;
    if prices.get(provider).is_none() {
        prices.insert(provider, Item::Table(toml_edit::Table::new()));
    }
    let table = prices
        .get_mut(provider)
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| DraftError {
            field: field.clone(),
            message: format!("prices.{provider} is not a table"),
        })?;
    if table.get(model).is_none() {
        table.insert(model, Item::Table(toml_edit::Table::new()));
    }
    table
        .get_mut(model)
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| DraftError {
            field,
            message: format!("prices.{provider}.{model} is not a table"),
        })
}

fn schedule_table(entry: &ScheduleEntry) -> toml_edit::Table {
    let mut table = toml_edit::Table::new();
    if let Some(name) = entry.name.as_deref().filter(|name| !name.trim().is_empty()) {
        table.insert("name", Item::Value(Value::from(name)));
    }
    table.insert("days", Item::Value(Value::from(entry.days.as_str())));
    table.insert("hours", Item::Value(Value::from(entry.hours.as_str())));
    if let Some(tz) = entry.tz.as_deref().filter(|tz| !tz.trim().is_empty()) {
        table.insert("tz", Item::Value(Value::from(tz)));
    }
    table.insert("input", Item::Value(Value::from(entry.input)));
    table.insert("output", Item::Value(Value::from(entry.output)));
    if let Some(cache_read) = entry.cache_read {
        table.insert("cache_read", Item::Value(Value::from(cache_read)));
    }
    table
}

/// Apply one draft leaf after asking the editable-field policy whether the
/// Console may write it at all. The policy, not the renderer, owns secrecy and
/// read-only fields: a crafted request must not be able to write a literal key
/// that the page happens not to render.
fn apply_change(document: &mut DocumentMut, change: &DraftChange) -> Result<(), DraftError> {
    let field = change.path.join(".");
    if change.path.is_empty() {
        return Err(DraftError {
            field,
            message: "a change needs a field path".to_string(),
        });
    }
    let Some(policy) = crate::editable::field(&change.path) else {
        return Err(DraftError {
            field: field.clone(),
            message: "unknown field".to_string(),
        });
    };
    if policy.secret {
        return Err(DraftError {
            field: field.clone(),
            message: "this field is a secret; store it with lgw keys add instead of writing it                       into the configuration file"
                .to_string(),
        });
    }
    if policy.class == FieldClass::ReadOnly {
        return Err(DraftError {
            field: field.clone(),
            message: "this field is read-only in the console".to_string(),
        });
    }
    let kind = policy.kind;
    let (parents, leaf) = change.path.split_at(change.path.len() - 1);
    let table = parent_table(document, parents, &field)?;
    if let Some(item) = table.get_mut(leaf[0].as_str()) {
        return set_leaf(item, &change.value).map_err(|message| DraftError {
            field: field.clone(),
            message,
        });
    }
    let value = value_for(kind, &change.value).map_err(|message| DraftError {
        field: field.clone(),
        message,
    })?;
    table.insert(leaf[0].as_str(), Item::Value(value));
    Ok(())
}

/// Walk (and, for whitelisted leaves, create) the parent path of a change.
///
/// A numeric segment is an index into an array of tables: schedules are
/// addressed as schedules.0.name, which is how a form field can edit one
/// schedule row without replacing the whole block.
fn parent_table<'a>(
    document: &'a mut DocumentMut,
    segments: &[String],
    field: &str,
) -> Result<&'a mut dyn toml_edit::TableLike, DraftError> {
    descend(document.as_table_mut(), segments, field)
}

fn descend<'a>(
    table: &'a mut dyn toml_edit::TableLike,
    segments: &[String],
    field: &str,
) -> Result<&'a mut dyn toml_edit::TableLike, DraftError> {
    let Some((segment, rest)) = segments.split_first() else {
        return Ok(table);
    };
    if segment.parse::<usize>().is_ok() {
        return Err(DraftError {
            field: field.to_string(),
            message: "no such schedule".to_string(),
        });
    }
    if table.get(segment).is_none() {
        table.insert(segment, Item::Table(toml_edit::Table::new()));
    }
    let item = table.get_mut(segment).ok_or_else(|| DraftError {
        field: field.to_string(),
        message: "unknown field".to_string(),
    })?;
    if let Item::ArrayOfTables(entries) = item {
        let Some((index_segment, rest)) = rest.split_first() else {
            return Err(DraftError {
                field: field.to_string(),
                message: "a schedule needs a numeric index".to_string(),
            });
        };
        let index = index_segment.parse::<usize>().map_err(|_| DraftError {
            field: field.to_string(),
            message: "a schedule needs a numeric index".to_string(),
        })?;
        let entry = entries.get_mut(index).ok_or_else(|| DraftError {
            field: field.to_string(),
            message: "no such schedule".to_string(),
        })?;
        return descend(entry, rest, field);
    }
    let Some(next) = item.as_table_like_mut() else {
        return Err(DraftError {
            field: field.to_string(),
            message: format!("{segment} is not a table"),
        });
    };
    descend(next, rest, field)
}

fn value_for(kind: ValueKind, text: &str) -> Result<Value, String> {
    Ok(match kind {
        ValueKind::String => Value::from(text),
        ValueKind::Integer => Value::from(
            text.trim()
                .parse::<i64>()
                .map_err(|_| "not an integer".to_string())?,
        ),
        ValueKind::Float => Value::from(
            text.trim()
                .parse::<f64>()
                .map_err(|_| "not a number".to_string())?,
        ),
        ValueKind::Boolean => Value::from(
            text.trim()
                .parse::<bool>()
                .map_err(|_| "not true or false".to_string())?,
        ),
    })
}

/// Replace a leaf with newly typed text while keeping its whitespace and any
/// trailing comment. The replacement's type follows the value already there.
fn set_leaf(item: &mut Item, text: &str) -> Result<(), String> {
    let Some(value) = item.as_value() else {
        return Err("this field is not a single value".to_string());
    };
    let decoration = value.decor().clone();
    let mut replacement = match value {
        Value::String(_) => Value::from(text),
        Value::Integer(_) => Value::from(
            text.trim()
                .parse::<i64>()
                .map_err(|_| "not an integer".to_string())?,
        ),
        Value::Float(_) => Value::from(
            text.trim()
                .parse::<f64>()
                .map_err(|_| "not a number".to_string())?,
        ),
        Value::Boolean(_) => Value::from(
            text.trim()
                .parse::<bool>()
                .map_err(|_| "not true or false".to_string())?,
        ),
        _ => return Err("this field type cannot be edited here".to_string()),
    };
    *replacement.decor_mut() = decoration;
    *item = Item::Value(replacement);
    Ok(())
}

/// What one line of a document diff is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    Same,
    Removed,
    Added,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffKind,
    pub text: String,
}

/// A line diff between two reads of the file. Longest common subsequence, which
/// is enough for a configuration file and never reorders a line to make a
/// smaller diff.
pub fn diff(left: &ConfigDocument, right: &ConfigDocument) -> Vec<DiffLine> {
    let a: Vec<&str> = left.raw().lines().collect();
    let b: Vec<&str> = right.raw().lines().collect();
    let mut lengths = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lengths[i][j] = if a[i] == b[j] {
                lengths[i + 1][j + 1] + 1
            } else {
                lengths[i + 1][j].max(lengths[i][j + 1])
            };
        }
    }
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            out.push(DiffLine {
                kind: DiffKind::Same,
                text: a[i].to_string(),
            });
            i += 1;
            j += 1;
        } else if lengths[i + 1][j] >= lengths[i][j + 1] {
            out.push(DiffLine {
                kind: DiffKind::Removed,
                text: a[i].to_string(),
            });
            i += 1;
        } else {
            out.push(DiffLine {
                kind: DiffKind::Added,
                text: b[j].to_string(),
            });
            j += 1;
        }
    }
    while i < a.len() {
        out.push(DiffLine {
            kind: DiffKind::Removed,
            text: a[i].to_string(),
        });
        i += 1;
    }
    while j < b.len() {
        out.push(DiffLine {
            kind: DiffKind::Added,
            text: b[j].to_string(),
        });
        j += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const BARE_KEYS: &str = r#"
[server]
listen = "127.0.0.1:8787"
local_key = "sk-local-SECRET"

[providers.xkiro]
adapter = "xkiro"
base_url = "https://api.xkiro.com/v1"

[providers.xkiro.accounts.acc-1]
key = "sk-account-SECRET"   # why this comment exists

[providers.xkiro.accounts.acc-2]
key = { env = "XKIRO_KEY_2" }
"#;

    #[test]
    fn the_atomic_writer_replaces_in_one_step_with_a_private_mode() {
        let dir = std::env::temp_dir().join(format!(
            "lgw-atomic-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        write_atomic(&path, "listen = \"127.0.0.1:8787\"\n").expect("first write");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "listen = \"127.0.0.1:8787\"\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        write_atomic(&path, "listen = \"127.0.0.1:8788\"\n").expect("second write");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "listen = \"127.0.0.1:8788\"\n"
        );

        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "a completed write leaves no temp file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_write_leaves_neither_a_target_nor_a_temp_file() {
        let missing = std::env::temp_dir()
            .join("lgw-no-such-dir")
            .join("config.toml");
        assert!(write_atomic(&missing, "listen = \"x\"\n").is_err());
        assert!(!missing.exists());
    }

    const ACCOUNTS_DOC: &str = r#"
[providers.mock]
adapter = "none"
base_url = "http://127.0.0.1:1/v1"

[providers.mock.accounts.a1]
key = "sk-a1"
label = "first"

[providers.mock.accounts.a2]
key = { env = "MOCK_KEY_2" }
"#;

    #[test]
    fn an_account_can_be_added_with_a_named_key_source() {
        let document = ConfigDocument::parse(ACCOUNTS_DOC).expect("parse");
        let edited = document
            .apply_actions(&[DraftAction::AddAccount {
                provider: "mock".to_string(),
                id: "a3".to_string(),
                label: Some("third".to_string()),
                key: KeySpec::Env("MOCK_KEY_3".to_string()),
            }])
            .expect("add an account");
        let raw = edited.raw();
        assert!(raw.contains("a3"), "the new account table is written");
        assert!(raw.contains("env = \"MOCK_KEY_3\""));
        assert!(raw.contains("label = \"third\""));
        assert!(raw.contains("sk-a1"), "existing accounts are untouched");
        crate::config::Config::from_toml_str(raw).expect("the result is a valid configuration");
    }

    #[test]
    fn an_account_can_be_added_to_a_provider_without_an_accounts_table() {
        let document = ConfigDocument::parse(
            "[providers.mock]\nadapter = \"none\"\nbase_url = \"http://127.0.0.1:1/v1\"\n",
        )
        .expect("parse");
        let edited = document
            .apply_actions(&[DraftAction::AddAccount {
                provider: "mock".to_string(),
                id: "first".to_string(),
                label: None,
                key: KeySpec::Keychain,
            }])
            .expect("add an account");
        assert!(edited.raw().contains("key = { keychain = true }"));
        crate::config::Config::from_toml_str(edited.raw()).expect("valid configuration");
    }

    #[test]
    fn removing_an_account_keeps_the_others_and_refuses_a_typo() {
        let document = ConfigDocument::parse(ACCOUNTS_DOC).expect("parse");
        let edited = document
            .apply_actions(&[DraftAction::Remove {
                path: vec![
                    "providers".to_string(),
                    "mock".to_string(),
                    "accounts".to_string(),
                    "a1".to_string(),
                ],
            }])
            .expect("remove an account");
        assert!(!edited.raw().contains("sk-a1"));
        assert!(edited.raw().contains("MOCK_KEY_2"));
        assert!(!edited.raw().contains("a1"));

        let missing = document
            .apply_actions(&[DraftAction::Remove {
                path: vec![
                    "providers".to_string(),
                    "mock".to_string(),
                    "accounts".to_string(),
                    "nope".to_string(),
                ],
            }])
            .expect_err("no such account");
        assert_eq!(missing.field, "providers.mock.accounts.nope");

        let not_an_account = document
            .apply_actions(&[DraftAction::Remove {
                path: vec![
                    "providers".to_string(),
                    "mock".to_string(),
                    "base_url".to_string(),
                ],
            }])
            .expect_err("only accounts are removable here");
        assert_eq!(not_an_account.field, "providers.mock.base_url");
    }

    #[test]
    fn a_duplicate_account_id_is_refused() {
        let document = ConfigDocument::parse(ACCOUNTS_DOC).expect("parse");
        let error = document
            .apply_actions(&[DraftAction::AddAccount {
                provider: "mock".to_string(),
                id: "a1".to_string(),
                label: None,
                key: KeySpec::Keychain,
            }])
            .expect_err("duplicate");
        assert!(error.message.contains("already exists"));
    }

    #[test]
    fn a_literal_secret_is_redacted_and_the_line_shape_survives() {
        let document = ConfigDocument::parse(BARE_KEYS).expect("parse");
        let redacted = document.redacted();
        assert!(!redacted.contains("sk-local-SECRET"));
        assert!(!redacted.contains("sk-account-SECRET"));
        assert!(redacted.contains("<redacted>"));
        assert!(
            redacted.contains("env = \"XKIRO_KEY_2\""),
            "a source name is not a secret"
        );
        assert!(redacted.contains("# why this comment exists"));
        assert!(
            document.raw().contains("sk-local-SECRET"),
            "the raw read is unchanged"
        );
    }

    #[test]
    fn the_fingerprint_changes_with_the_bytes_and_is_stable() {
        let a = fingerprint("listen = \"127.0.0.1:8787\"\n");
        let b = fingerprint("listen = \"127.0.0.1:8788\"\n");
        assert_eq!(a, fingerprint("listen = \"127.0.0.1:8787\"\n"));
        assert_ne!(a, b);
        assert_eq!(a.len(), 16);
    }

    const DRAFTABLE: &str = r#"
[server]
listen = "127.0.0.1:8787"   # keep this comment

[policy]
soft_threshold = 0.80
max_retries = 2
allow_metered = false

[providers.mock]
adapter = "none"
base_url = "http://127.0.0.1:1/v1"

[providers.mock.accounts.acc-1]
label = "first"

[unknown]
keep = "this field"
"#;

    fn changes(pairs: &[(&[&str], &str)]) -> Vec<DraftChange> {
        pairs
            .iter()
            .map(|(path, value)| DraftChange {
                path: path.iter().map(|segment| (*segment).to_string()).collect(),
                value: (*value).to_string(),
            })
            .collect()
    }

    const STRUCTURAL_DOC: &str = r#"
[providers.mock]
adapter = "none"
base_url = "http://127.0.0.1:1/v1"

[providers.mock.accounts.a1]
key = "sk-a1"

[providers.alt]
adapter = "none"
base_url = "http://127.0.0.1:2/v1"

[providers.alt.accounts.a1]
key = "sk-alt"

[routes]
"mock/model" = "mock"

[provider_prefix_aliases]
"alias" = "mock"

[prices.mock."mock/model"]
input = 0.15
output = 0.60

[[prices.mock."mock/model".schedules]]
name = "peak"
days = "*"
hours = "00:00-23:59"
tz = "UTC"
input = 0.30
output = 1.20
"#;

    #[test]
    fn routes_and_aliases_can_be_added_changed_and_removed() {
        let document = ConfigDocument::parse(STRUCTURAL_DOC).expect("parse");
        let edited = document
            .add_route("mock/new", "alt")
            .expect("add route")
            .add_alias("short", "alt")
            .expect("add alias")
            .remove_route("mock/model")
            .expect("remove route");
        let raw = edited.raw();
        crate::config::Config::from_toml_str(raw).expect("valid configuration");
        assert!(raw.contains("mock/new"));
        assert!(raw.contains("short"));
        assert!(!raw.contains("\"mock/model\" = \"mock\""));

        let changed = edited
            .apply_actions(&[DraftAction::Set(DraftChange {
                path: vec!["routes".to_string(), "mock/new".to_string()],
                value: "mock".to_string(),
            })])
            .expect("remap route");
        let config = crate::config::Config::from_toml_str(changed.raw()).expect("valid");
        assert_eq!(config.routes["mock/new"], "mock");

        let last_route = changed.remove_route("mock/new").expect("remove last route");
        assert!(!last_route.raw().contains("[routes]"));
        let last_alias = document.remove_alias("alias").expect("remove last alias");
        assert!(!last_alias.raw().contains("provider_prefix_aliases"));
    }

    #[test]
    fn prices_can_be_added_removed_and_rescheduled() {
        let document = ConfigDocument::parse(STRUCTURAL_DOC).expect("parse");
        let with_price = document
            .add_price("mock", "newmodel", 1.0, 2.0, Some(0.1))
            .expect("add price");
        crate::config::Config::from_toml_str(with_price.raw()).expect("valid configuration");
        assert!(with_price.raw().contains("newmodel"));

        let without = document
            .remove_price("mock", "mock/model")
            .expect("remove price");
        crate::config::Config::from_toml_str(without.raw()).expect("valid configuration");
        assert!(!without.raw().contains("input = 0.15"));

        let entry = ScheduleEntry {
            name: Some("night".to_string()),
            days: "sat,sun".to_string(),
            hours: "00:00-06:00".to_string(),
            tz: Some("+08:00".to_string()),
            input: 0.05,
            output: 0.2,
            cache_read: None,
        };
        let scheduled = document
            .add_schedule("mock", "mock/model", &entry)
            .expect("add schedule");
        crate::config::Config::from_toml_str(scheduled.raw()).expect("valid configuration");
        assert!(scheduled.raw().contains("night"));

        // Edit one schedule in place through its index.
        let edited = scheduled
            .apply_actions(&[DraftAction::Set(DraftChange {
                path: vec![
                    "prices".to_string(),
                    "mock".to_string(),
                    "mock/model".to_string(),
                    "schedules".to_string(),
                    "1".to_string(),
                    "input".to_string(),
                ],
                value: "0.07".to_string(),
            })])
            .expect("edit schedule");
        crate::config::Config::from_toml_str(edited.raw()).expect("valid configuration");
        assert!(edited.raw().contains("0.07"));

        let removed = scheduled
            .remove_schedule("mock", "mock/model", 0)
            .expect("remove schedule");
        assert!(!removed.raw().contains("peak"));
    }

    #[test]
    fn a_draft_keeps_comments_order_and_unknown_fields() {
        let document = ConfigDocument::parse(DRAFTABLE).expect("parse");
        let edited = document
            .with_draft(&changes(&[
                (&["policy", "soft_threshold"], "0.75"),
                (&["policy", "max_retries"], "3"),
                (&["policy", "allow_metered"], "true"),
                (
                    &["providers", "mock", "base_url"],
                    "https://api.example.test/v1",
                ),
                (
                    &["providers", "mock", "accounts", "acc-1", "label"],
                    "renamed",
                ),
            ]))
            .expect("draft applies");
        let raw = edited.raw();
        assert!(raw.contains("soft_threshold = 0.75"));
        assert!(raw.contains("max_retries = 3"));
        assert!(raw.contains("allow_metered = true"));
        assert!(raw.contains("https://api.example.test/v1"));
        assert!(raw.contains("label = \"renamed\""));
        assert!(raw.contains("# keep this comment"));
        assert!(raw.contains("[unknown]"));
        assert!(raw.contains("keep = \"this field\""));
        assert!(
            raw.find("[server]").unwrap() < raw.find("[unknown]").unwrap(),
            "top-level table order is preserved"
        );
        assert!(
            document.raw().contains("soft_threshold = 0.80"),
            "the original is untouched"
        );
    }

    #[test]
    fn a_whitelisted_missing_field_is_created_with_its_type() {
        let document = ConfigDocument::parse("[policy]\nsoft_threshold = 0.8\n").expect("parse");
        let edited = document
            .with_draft(&changes(&[
                (&["policy", "poll_interval_secs"], "300"),
                (&["policy", "probe_before_switch"], "true"),
            ]))
            .expect("defaults can be written down");
        let raw = edited.raw();
        assert!(raw.contains("poll_interval_secs = 300"));
        assert!(raw.contains("probe_before_switch = true"));
        assert!(raw.contains("soft_threshold = 0.8"));
    }

    #[test]
    fn a_draft_cannot_invent_a_field_or_misread_its_type() {
        let document = ConfigDocument::parse(DRAFTABLE).expect("parse");
        let missing = document
            .with_draft(&changes(&[(&["policy", "nope"], "1")]))
            .expect_err("unknown field");
        assert_eq!(missing.field, "policy.nope");

        let wrong_type = document
            .with_draft(&changes(&[(&["policy", "max_retries"], "many")]))
            .expect_err("not an integer");
        assert_eq!(wrong_type.field, "policy.max_retries");

        let wrong_shape = document
            .with_draft(&changes(&[(&["policy", "soft_threshold", "deep"], "1")]))
            .expect_err("a float is not a table");
        assert_eq!(wrong_shape.field, "policy.soft_threshold.deep");
    }

    #[test]
    fn the_diff_names_the_removed_and_added_lines() {
        let document = ConfigDocument::parse(DRAFTABLE).expect("parse");
        let edited = document
            .with_draft(&changes(&[(&["policy", "soft_threshold"], "0.75")]))
            .expect("draft applies");
        let changes_diff = diff(&document, &edited);
        assert!(changes_diff.iter().any(|line| {
            line.kind == DiffKind::Removed && line.text.contains("soft_threshold = 0.80")
        }));
        assert!(changes_diff.iter().any(|line| {
            line.kind == DiffKind::Added && line.text.contains("soft_threshold = 0.75")
        }));
        assert!(
            changes_diff
                .iter()
                .any(|line| line.kind == DiffKind::Same && line.text.contains("[policy]"))
        );

        let same = diff(&document, &document);
        assert!(same.iter().all(|line| line.kind == DiffKind::Same));
    }

    #[test]
    fn an_unreadable_or_invalid_file_is_an_error_not_an_empty_document() {
        let missing = Path::new("/definitely/not/a/config.toml");
        assert!(ConfigDocument::read(missing).is_err());
        assert!(ConfigDocument::parse("[server").is_err());
    }
}

#[cfg(test)]
mod editable_policy_tests {
    use super::*;

    const CONFIG: &str = r#"
[server]
listen = "127.0.0.1:8787"

[providers.mock]
adapter = "none"
base_url = "http://127.0.0.1:1/v1"

[providers.mock.accounts.a1]
key = "sk-original"
"#;

    fn change(path: &[&str], value: &str) -> DraftChange {
        DraftChange {
            path: path.iter().map(|segment| segment.to_string()).collect(),
            value: value.to_string(),
        }
    }

    #[test]
    fn a_draft_cannot_write_a_literal_secret() {
        let document = ConfigDocument::parse(CONFIG).expect("parse");
        let error = document
            .with_draft(&[change(
                &["providers", "mock", "accounts", "a1", "key"],
                "sk-written-through-a-draft",
            )])
            .expect_err("a key path is not editable");
        assert_eq!(error.field, "providers.mock.accounts.a1.key");
        assert!(error.message.contains("secret"), "got {}", error.message);
        assert!(document.raw().contains("sk-original"));
        assert!(!document.raw().contains("sk-written-through-a-draft"));
    }

    #[test]
    fn a_read_only_field_is_refused_rather_than_written() {
        let document = ConfigDocument::parse(CONFIG).expect("parse");
        let error = document
            .with_draft(&[change(&["providers", "mock", "allow_metered"], "true")])
            .expect_err("read-only");
        assert_eq!(error.field, "providers.mock.allow_metered");
        assert!(error.message.contains("read-only"), "got {}", error.message);
    }

    #[test]
    fn an_unknown_leaf_is_still_refused() {
        let document = ConfigDocument::parse(CONFIG).expect("parse");
        let error = document
            .with_draft(&[change(&["unknown", "leaf"], "x")])
            .expect_err("unknown");
        assert!(error.message.contains("unknown field"));
    }
}
