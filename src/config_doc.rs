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

/// One read of the configuration file, kept as bytes-shaped TOML.
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

    #[test]
    fn an_unreadable_or_invalid_file_is_an_error_not_an_empty_document() {
        let missing = Path::new("/definitely/not/a/config.toml");
        assert!(ConfigDocument::read(missing).is_err());
        assert!(ConfigDocument::parse("[server").is_err());
    }
}
