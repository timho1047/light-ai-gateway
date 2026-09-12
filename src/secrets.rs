//! Credential storage for provider API keys.
//!
//! FILE OWNER: Lead.
//!
//! Why this module exists: asking the operator to export an environment variable
//! per account before every start is friction, and the obvious fix -- a file of
//! keys -- is a plaintext secret on disk.
//!
//! What this does instead: it hands the secret to the operating system's own
//! credential store (macOS Keychain, Windows Credential Manager, freedesktop
//! Secret Service) through the keyring crate. The OS owns the encryption and the
//! access control; we never choose an algorithm, never hold a key, and never
//! write a ciphertext we could be tempted to decrypt with a compiled-in secret.
//!
//! The honest security boundary, stated plainly:
//!   - Protected: other users on the machine, backups, disk images, accidental
//!     commits, and anything that reads files but not the credential store.
//!   - NOT protected: a process already running as you. It can ask the OS for the
//!     same secret. On some platforms the OS will prompt; on macOS the login
//!     keychain is unlocked for the session.
//!
//! A vault file encrypted with a key compiled into the binary would LOOK like it
//! protects the second case and would not: anyone with the binary has the key.
//! That is obfuscation, not encryption, so it is deliberately not implemented.
//!
//! Every key lives in ONE item (see VaultBackend), with reads falling back to the
//! per-account items of an older install. That is not only tidier: on macOS an
//! access-control grant is bound to the calling binary's code signature, and an
//! ad-hoc signed binary is identified by the hash of its code, so it can never be
//! remembered across a rebuild. Fewer items means fewer things to authorise, and
//! "lgw keys migrate --prune" is what folds them together.

use crate::domain::ApiKey;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Service name under which entries are filed in the OS credential store.
pub const SERVICE: &str = "light-ai-gateway";

/// Reserved provider/account pair for the gateway's own local access token.
/// Register it with: lgw keys add local gateway
pub const LOCAL_KEY_PROVIDER: &str = "local";
pub const LOCAL_KEY_ACCOUNT: &str = "gateway";

/// Result of asking whether a credential is available.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialStatus {
    /// Stored; carries the secret length so a truncated paste is visible.
    Present { len: usize },
    /// No entry for this provider/account.
    Missing,
    /// The store itself could not be reached.
    Unavailable { reason: String },
}

impl CredentialStatus {
    pub fn label(&self) -> String {
        match self {
            CredentialStatus::Present { len } => format!("stored ({len} chars)"),
            CredentialStatus::Missing => "missing".to_string(),
            CredentialStatus::Unavailable { reason } => format!("unavailable ({reason})"),
        }
    }
}

/// Backing store for secrets. Abstracted so tests never touch the real keychain.
pub trait SecretBackend: Send + Sync {
    fn name(&self) -> &'static str;
    fn set(&self, provider: &str, account: &str, secret: &str) -> Result<()>;
    /// Ok(None) means "no entry", which is different from a store failure.
    fn get(&self, provider: &str, account: &str) -> Result<Option<ApiKey>>;
    /// Ok(false) means there was nothing to delete.
    fn delete(&self, provider: &str, account: &str) -> Result<bool>;

    /// Whether every key lives in a single item.
    fn is_vaulted(&self) -> bool {
        false
    }

    /// Where this credential is stored, for display.
    fn origin(&self, _provider: &str, _account: &str) -> Result<CredentialOrigin> {
        Ok(CredentialOrigin::Absent)
    }

    /// Fold per-account items into the vault. Backends without a vault have
    /// nothing to do, so the default reports an empty migration rather than
    /// pretending to have moved something.
    fn migrate(&self, _pairs: &[(String, String)], _prune: bool) -> Result<MigrationReport> {
        Ok(MigrationReport::default())
    }
}

/// The composite key used inside the store.
pub fn entry_user(provider: &str, account: &str) -> String {
    format!("{provider}/{account}")
}

/// The message shown when an account has no credential. It names the exact
/// command that fixes the problem, because the alternative is an operator
/// restarting with an empty environment and guessing.
pub fn missing_credential_message(provider: &str, account: &str) -> String {
    format!("no credential for {provider}/{account}. Run: lgw keys add {provider} {account}")
}

/// Name of the one item that holds every key.
///
/// Every per-account item is named "provider/account", which always contains a
/// slash, so this name cannot collide with one.
pub const VAULT_ITEM: &str = "vault";

/// Where a credential was found, for display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialOrigin {
    /// The single vault item.
    Vault,
    /// A per-account item from an install that predates the vault.
    PerAccountItem,
    /// Nothing stored, or the backend has no such notion.
    Absent,
}

impl CredentialOrigin {
    pub fn describe(self) -> &'static str {
        match self {
            CredentialOrigin::Vault => "vault item",
            CredentialOrigin::PerAccountItem => "per-account item",
            CredentialOrigin::Absent => "not stored",
        }
    }

    /// Short form for a table column.
    pub fn short(self) -> &'static str {
        match self {
            CredentialOrigin::Vault => "vault",
            CredentialOrigin::PerAccountItem => "per-account",
            CredentialOrigin::Absent => "none",
        }
    }
}

/// What a migration did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    /// Keys copied out of per-account items.
    pub merged: usize,
    /// Keys the vault already had.
    pub already: usize,
    /// Configured accounts with nothing stored anywhere.
    pub missing: usize,
    /// Per-account items removed after the vault was confirmed to hold them.
    pub pruned: usize,
}

/// Raw access to one named item in the credential store.
///
/// A level below SecretBackend on purpose. The vault keeps every key in a single
/// item and falls back to the per-account ones, and putting that logic above a
/// seam that tests can drive is the difference between "the merge is probably
/// right" and a test that proves it.
pub trait ItemStore: Send + Sync {
    fn read(&self, item: &str) -> Result<Option<String>>;
    fn write(&self, item: &str, value: &str) -> Result<()>;
    fn delete(&self, item: &str) -> Result<bool>;
}

/// The real thing: one generic-password item per name.
pub struct KeychainItems {
    service: String,
}

impl Default for KeychainItems {
    fn default() -> Self {
        KeychainItems::new(SERVICE)
    }
}

impl KeychainItems {
    pub fn new(service: impl Into<String>) -> Self {
        KeychainItems {
            service: service.into(),
        }
    }

    fn entry(&self, item: &str) -> Result<keyring::Entry> {
        keyring::Entry::new(&self.service, item).context("opening the OS credential store entry")
    }
}

impl ItemStore for KeychainItems {
    fn read(&self, item: &str) -> Result<Option<String>> {
        match self.entry(item)?.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(anyhow::anyhow!("{e}")),
        }
    }

    fn write(&self, item: &str, value: &str) -> Result<()> {
        self.entry(item)?
            .set_password(value)
            .context("storing the credential in the OS credential store")
    }

    fn delete(&self, item: &str) -> Result<bool> {
        match self.entry(item)?.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(e) => Err(anyhow::anyhow!("{e}")),
        }
    }
}

/// Every key in ONE credential-store item.
///
/// One item means one access-control entry, so macOS asks the operator to
/// authorise this binary once instead of once per account. That only became the
/// deciding factor on macOS, where an ad-hoc signed binary is identified by the
/// hash of its code: "Always Allow" could never stick across a rebuild, and the
/// only durable fix is to have fewer things to authorise.
///
/// Reads fall back to per-account items so an install that predates the vault
/// keeps working until it migrates.
pub struct VaultBackend<S: ItemStore> {
    items: S,
    /// Parsed once per process: without it every account resolution is another
    /// round trip to the credential store for the very same item.
    cache: Mutex<Option<HashMap<String, String>>>,
}

impl<S: ItemStore> VaultBackend<S> {
    pub fn new(items: S) -> Self {
        VaultBackend {
            items,
            cache: Mutex::new(None),
        }
    }

    fn load(&self) -> Result<HashMap<String, String>> {
        if let Some(cached) = self.cache.lock().expect("vault cache").clone() {
            return Ok(cached);
        }
        let map = match self.items.read(VAULT_ITEM)? {
            None => HashMap::new(),
            Some(text) => parse_vault(&text)?,
        };
        *self.cache.lock().expect("vault cache") = Some(map.clone());
        Ok(map)
    }

    fn store(&self, map: &HashMap<String, String>) -> Result<()> {
        let text = serde_json::to_string(map).context("encoding the credential vault")?;
        self.items.write(VAULT_ITEM, &text)?;
        *self.cache.lock().expect("vault cache") = Some(map.clone());
        Ok(())
    }
}

/// Decode the vault item.
///
/// A vault that cannot be read is an ERROR, never an empty map: reporting every
/// key as missing would send the operator off to re-add credentials that are
/// sitting right there, and the next write would then overwrite them.
fn parse_vault(text: &str) -> Result<HashMap<String, String>> {
    serde_json::from_str(text).with_context(|| {
        format!(
            "the '{VAULT_ITEM}' item in the OS credential store is not readable as a JSON              object of provider/account to secret. Refusing to treat every credential as              missing. Restore the item from a backup, or delete it and re-add the keys."
        )
    })
}

impl<S: ItemStore> SecretBackend for VaultBackend<S> {
    fn name(&self) -> &'static str {
        "os-keychain"
    }

    fn is_vaulted(&self) -> bool {
        true
    }

    fn set(&self, provider: &str, account: &str, secret: &str) -> Result<()> {
        let mut map = self.load()?;
        map.insert(entry_user(provider, account), secret.to_string());
        self.store(&map)
    }

    fn get(&self, provider: &str, account: &str) -> Result<Option<ApiKey>> {
        let key = entry_user(provider, account);
        let map = self.load()?;
        if let Some(secret) = map.get(&key) {
            return Ok(Some(ApiKey::new(secret.clone())));
        }
        // Not in the vault yet: a pre-vault install still has one item per
        // account, and that key has to keep working until it is migrated.
        Ok(self.items.read(&key)?.map(ApiKey::new))
    }

    fn delete(&self, provider: &str, account: &str) -> Result<bool> {
        let key = entry_user(provider, account);
        let mut map = self.load()?;
        let in_vault = map.remove(&key).is_some();
        if in_vault {
            self.store(&map)?;
        }
        // Both copies: leaving the per-account item behind would resurrect a
        // removed key through the fallback above.
        let legacy = self.items.delete(&key)?;
        Ok(in_vault || legacy)
    }

    fn origin(&self, provider: &str, account: &str) -> Result<CredentialOrigin> {
        let key = entry_user(provider, account);
        if self.load()?.contains_key(&key) {
            return Ok(CredentialOrigin::Vault);
        }
        Ok(if self.items.read(&key)?.is_some() {
            CredentialOrigin::PerAccountItem
        } else {
            CredentialOrigin::Absent
        })
    }

    fn migrate(&self, pairs: &[(String, String)], prune: bool) -> Result<MigrationReport> {
        let mut map = self.load()?;
        let mut report = MigrationReport::default();
        for (provider, account) in pairs {
            let key = entry_user(provider, account);
            if map.contains_key(&key) {
                report.already += 1;
                continue;
            }
            match self.items.read(&key)? {
                Some(secret) => {
                    map.insert(key, secret);
                    report.merged += 1;
                }
                None => report.missing += 1,
            }
        }

        // Write the vault BEFORE pruning. A crash between the two leaves a
        // duplicate, which the next run reports as already-present; the other
        // order would delete the only copy of a key.
        if report.merged > 0 {
            self.store(&map)?;
        }
        if prune {
            for (provider, account) in pairs {
                let key = entry_user(provider, account);
                if map.contains_key(&key) && self.items.delete(&key)? {
                    report.pruned += 1;
                }
            }
        }
        Ok(report)
    }
}

/// In-memory backend. Used by tests and by any code path that must not touch the
/// operator's real credential store.
#[derive(Default)]
pub struct MemoryBackend {
    map: Mutex<HashMap<String, String>>,
}

impl MemoryBackend {
    pub fn new() -> Self {
        MemoryBackend::default()
    }
}

impl SecretBackend for MemoryBackend {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn set(&self, provider: &str, account: &str, secret: &str) -> Result<()> {
        self.map
            .lock()
            .unwrap()
            .insert(entry_user(provider, account), secret.to_string());
        Ok(())
    }

    fn get(&self, provider: &str, account: &str) -> Result<Option<ApiKey>> {
        Ok(self
            .map
            .lock()
            .unwrap()
            .get(&entry_user(provider, account))
            .map(|s| ApiKey::new(s.clone())))
    }

    fn delete(&self, provider: &str, account: &str) -> Result<bool> {
        Ok(self
            .map
            .lock()
            .unwrap()
            .remove(&entry_user(provider, account))
            .is_some())
    }
}

/// Facade over a secret backend.
#[derive(Clone)]
pub struct CredentialStore {
    backend: Arc<dyn SecretBackend>,
}

impl CredentialStore {
    /// The backend this process should use, from the environment.
    ///
    /// LGW_CREDENTIAL_BACKEND=memory forces an ephemeral store, so dry runs and
    /// CI never touch the operator's real keychain. EVERY caller has to come
    /// through here. When only doctor and keys honoured the setting, "serve"
    /// read the real keychain anyway -- so the documented promise was false
    /// exactly where it mattered, and a dry run that believed it was isolated
    /// still raised authorisation prompts.
    pub fn from_env() -> Self {
        match crate::env_preferring("LGW_CREDENTIAL_BACKEND", "AI_GW_CREDENTIAL_BACKEND").as_deref()
        {
            Some("memory") => Self::memory(),
            _ => Self::keychain(),
        }
    }

    /// The real store, backed by the OS credential store.
    pub fn keychain() -> Self {
        CredentialStore {
            backend: Arc::new(VaultBackend::new(KeychainItems::default())),
        }
    }

    /// An ephemeral store. Never persists anything.
    pub fn memory() -> Self {
        CredentialStore {
            backend: Arc::new(MemoryBackend::new()),
        }
    }

    pub fn with_backend(backend: Arc<dyn SecretBackend>) -> Self {
        CredentialStore { backend }
    }

    pub fn backend_name(&self) -> &'static str {
        self.backend.name()
    }

    /// Whether every key lives in a single credential-store item.
    pub fn is_vaulted(&self) -> bool {
        self.backend.is_vaulted()
    }

    /// Where a credential is stored, for display next to its status.
    pub fn origin(&self, provider: &str, account: &str) -> CredentialOrigin {
        self.backend
            .origin(provider, account)
            .unwrap_or(CredentialOrigin::Absent)
    }

    /// Fold per-account items into the vault, optionally removing them once the
    /// vault is confirmed to hold their contents.
    pub fn migrate(&self, pairs: &[(String, String)], prune: bool) -> Result<MigrationReport> {
        self.backend.migrate(pairs, prune)
    }

    pub fn set(&self, provider: &str, account: &str, secret: &str) -> Result<()> {
        let trimmed = secret.trim();
        if trimmed.is_empty() {
            anyhow::bail!("refusing to store an empty credential");
        }
        self.backend.set(provider, account, trimmed)
    }

    pub fn get(&self, provider: &str, account: &str) -> Result<Option<ApiKey>> {
        self.backend.get(provider, account)
    }

    pub fn delete(&self, provider: &str, account: &str) -> Result<bool> {
        self.backend.delete(provider, account)
    }

    /// Probe the store without surfacing the secret.
    pub fn status(&self, provider: &str, account: &str) -> CredentialStatus {
        match self.backend.get(provider, account) {
            Ok(Some(k)) => CredentialStatus::Present {
                len: k.expose().len(),
            },
            Ok(None) => CredentialStatus::Missing,
            Err(e) => CredentialStatus::Unavailable {
                reason: e.to_string(),
            },
        }
    }
}

impl Default for CredentialStore {
    fn default() -> Self {
        CredentialStore::keychain()
    }
}

/// Redact a secret for display: never show more than a length hint.
///
/// Deliberately not "first four and last four": a prefix of a provider API key is
/// enough to identify which account leaked in a screenshot, and the length alone
/// already catches the common failure, which is a truncated paste.
pub fn describe_secret(secret: &str) -> String {
    format!("{} chars", secret.trim().len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> CredentialStore {
        CredentialStore::memory()
    }

    #[test]
    fn round_trips_a_secret() {
        let s = store();
        s.set("xkiro", "acc-1", "sk-secret-value").unwrap();
        assert_eq!(
            s.get("xkiro", "acc-1").unwrap().unwrap().expose(),
            "sk-secret-value"
        );
    }

    #[test]
    fn missing_entries_are_not_errors() {
        let s = store();
        assert!(s.get("xkiro", "nope").unwrap().is_none());
        assert_eq!(s.status("xkiro", "nope"), CredentialStatus::Missing);
    }

    #[test]
    fn entries_are_scoped_per_provider_and_account() {
        let s = store();
        s.set("xkiro", "acc-1", "one").unwrap();
        s.set("xkiro", "acc-2", "two").unwrap();
        s.set("deepseek", "acc-1", "three").unwrap();
        assert_eq!(s.get("xkiro", "acc-1").unwrap().unwrap().expose(), "one");
        assert_eq!(s.get("xkiro", "acc-2").unwrap().unwrap().expose(), "two");
        assert_eq!(
            s.get("deepseek", "acc-1").unwrap().unwrap().expose(),
            "three"
        );
    }

    #[test]
    fn entry_user_is_ambiguous_when_an_id_contains_a_slash() {
        // entry_user joins with a slash, so provider "a" with account "b/c"
        // produces the same key as provider "a/b" with account "c". Config
        // validation rejects slashes in provider names and account ids for
        // exactly this reason: otherwise two accounts would silently share one
        // stored credential. This test pins the hazard that guard exists for.
        assert_eq!(entry_user("a", "b/c"), entry_user("a/b", "c"));
    }

    #[test]
    fn empty_secrets_are_refused() {
        let s = store();
        assert!(s.set("xkiro", "acc-1", "   ").is_err());
        assert!(s.get("xkiro", "acc-1").unwrap().is_none());
    }

    #[test]
    fn secrets_are_trimmed_so_a_pasted_newline_does_not_break_auth() {
        let s = store();
        s.set("xkiro", "acc-1", "  sk-abc\n").unwrap();
        assert_eq!(s.get("xkiro", "acc-1").unwrap().unwrap().expose(), "sk-abc");
    }

    #[test]
    fn delete_reports_whether_anything_was_removed() {
        let s = store();
        s.set("xkiro", "acc-1", "x").unwrap();
        assert!(s.delete("xkiro", "acc-1").unwrap());
        assert!(!s.delete("xkiro", "acc-1").unwrap());
    }

    #[test]
    fn status_reports_length_without_revealing_the_secret() {
        let s = store();
        s.set("xkiro", "acc-1", "sk-1234567890").unwrap();
        let status = s.status("xkiro", "acc-1");
        assert_eq!(status, CredentialStatus::Present { len: 13 });
        let rendered = status.label();
        assert!(!rendered.contains("1234567890"));
        assert!(rendered.contains("13"));
    }

    #[test]
    fn the_missing_credential_message_names_the_fix() {
        let msg = missing_credential_message("xkiro", "acc-1");
        assert!(msg.contains("lgw keys add xkiro acc-1"), "got: {msg}");
    }

    #[test]
    fn memory_backend_reports_itself_as_memory() {
        assert_eq!(store().backend_name(), "memory");
    }

    // ------------------------------------------------------------- the vault

    /// An ItemStore that keeps items in memory, so the vault's merge, fallback
    /// and caching can be tested without touching a real credential store.
    #[derive(Default)]
    struct FakeItems {
        items: Mutex<HashMap<String, String>>,
        reads: std::sync::atomic::AtomicUsize,
    }

    impl FakeItems {
        fn names(&self) -> Vec<String> {
            let mut names: Vec<String> =
                self.items.lock().expect("items").keys().cloned().collect();
            names.sort();
            names
        }
        /// Write a per-account item the way a pre-vault build did.
        fn put_legacy(&self, provider: &str, account: &str, secret: &str) {
            self.items
                .lock()
                .expect("items")
                .insert(entry_user(provider, account), secret.to_string());
        }
        fn reads(&self) -> usize {
            self.reads.load(std::sync::atomic::Ordering::SeqCst)
        }
        fn raw(&self, item: &str) -> Option<String> {
            self.items.lock().expect("items").get(item).cloned()
        }
    }

    impl ItemStore for FakeItems {
        fn read(&self, item: &str) -> Result<Option<String>> {
            self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(self.raw(item))
        }
        fn write(&self, item: &str, value: &str) -> Result<()> {
            self.items
                .lock()
                .expect("items")
                .insert(item.to_string(), value.to_string());
            Ok(())
        }
        fn delete(&self, item: &str) -> Result<bool> {
            Ok(self.items.lock().expect("items").remove(item).is_some())
        }
    }

    fn vault() -> VaultBackend<FakeItems> {
        VaultBackend::new(FakeItems::default())
    }

    fn pair(provider: &str, account: &str) -> (String, String) {
        (provider.to_string(), account.to_string())
    }

    /// One item, however many accounts.
    #[test]
    fn every_key_lives_in_a_single_item() {
        let v = vault();
        v.set("xkiro", "acc-1", "sk-one").unwrap();
        v.set("xkiro", "acc-2", "sk-two").unwrap();
        v.set("commandcode", "acc-1", "sk-three").unwrap();

        assert_eq!(
            v.items.names(),
            vec![VAULT_ITEM.to_string()],
            "three accounts, one credential-store item, one authorisation"
        );
        assert_eq!(v.get("xkiro", "acc-1").unwrap().unwrap().expose(), "sk-one");
        assert_eq!(
            v.get("commandcode", "acc-1").unwrap().unwrap().expose(),
            "sk-three"
        );
    }

    /// An install from before the vault keeps working until it migrates.
    #[test]
    fn a_per_account_item_is_still_read_until_it_is_migrated() {
        let v = vault();
        v.items.put_legacy("xkiro", "acc-1", "sk-legacy");

        assert_eq!(
            v.get("xkiro", "acc-1").unwrap().unwrap().expose(),
            "sk-legacy"
        );
        assert_eq!(
            v.origin("xkiro", "acc-1").unwrap(),
            CredentialOrigin::PerAccountItem
        );
    }

    /// Once migrated, the vault's copy is the one that is read.
    #[test]
    fn the_vault_wins_over_a_leftover_per_account_item() {
        let v = vault();
        v.items.put_legacy("xkiro", "acc-1", "sk-old");
        v.set("xkiro", "acc-1", "sk-new").unwrap();

        assert_eq!(v.get("xkiro", "acc-1").unwrap().unwrap().expose(), "sk-new");
        assert_eq!(v.origin("xkiro", "acc-1").unwrap(), CredentialOrigin::Vault);
    }

    /// Merging, then running it again.
    #[test]
    fn migration_folds_items_in_and_is_idempotent() {
        let v = vault();
        v.items.put_legacy("xkiro", "acc-1", "sk-one");
        v.items.put_legacy("xkiro", "acc-2", "sk-two");
        let pairs = [
            pair("xkiro", "acc-1"),
            pair("xkiro", "acc-2"),
            pair("xkiro", "acc-3"),
        ];

        let first = v.migrate(&pairs, false).unwrap();
        assert_eq!(first.merged, 2);
        assert_eq!(first.missing, 1, "acc-3 has nothing stored");
        assert_eq!(v.get("xkiro", "acc-2").unwrap().unwrap().expose(), "sk-two");

        let second = v.migrate(&pairs, false).unwrap();
        assert_eq!(second.merged, 0, "nothing left to move");
        assert_eq!(second.already, 2);
        assert_eq!(second.missing, 1);
    }

    /// Pruning is what removes the old items, and only after the vault has them.
    #[test]
    fn pruning_removes_per_account_items_only_once_the_vault_holds_them() {
        let v = vault();
        v.items.put_legacy("xkiro", "acc-1", "sk-one");
        let pairs = [pair("xkiro", "acc-1"), pair("xkiro", "acc-2")];

        let keep = v.migrate(&pairs, false).unwrap();
        assert_eq!(keep.merged, 1);
        assert_eq!(keep.pruned, 0, "without --prune nothing is deleted");
        assert_eq!(
            v.items.names(),
            vec![VAULT_ITEM.to_string(), "xkiro/acc-1".to_string()]
        );

        let drop = v.migrate(&pairs, true).unwrap();
        assert_eq!(drop.pruned, 1);
        assert_eq!(
            v.items.names(),
            vec![VAULT_ITEM.to_string()],
            "acc-2 had no item, and acc-1's is now redundant"
        );
        assert_eq!(v.get("xkiro", "acc-1").unwrap().unwrap().expose(), "sk-one");
    }

    /// An unreadable vault must not look like "no credentials".
    #[test]
    fn a_corrupt_vault_is_an_error_rather_than_a_missing_credential() {
        let v = vault();
        v.items.write(VAULT_ITEM, "this is not json").unwrap();

        let err = v.get("xkiro", "acc-1").unwrap_err().to_string();
        assert!(err.contains(VAULT_ITEM), "got: {err}");
        assert!(
            err.contains("missing"),
            "the message must warn against the wrong conclusion: {err}"
        );
    }

    /// Deleting has to clear both copies, or the fallback resurrects the key.
    #[test]
    fn removing_a_key_clears_both_copies() {
        let v = vault();
        v.items.put_legacy("xkiro", "acc-1", "sk-legacy");
        v.set("xkiro", "acc-1", "sk-current").unwrap();

        assert!(v.delete("xkiro", "acc-1").unwrap());
        assert!(v.get("xkiro", "acc-1").unwrap().is_none());
        assert_eq!(v.items.names(), vec![VAULT_ITEM.to_string()]);
        assert!(
            !v.delete("xkiro", "acc-1").unwrap(),
            "nothing left to remove"
        );
    }

    /// The item is read once per process, not once per account.
    #[test]
    fn the_vault_item_is_read_once_for_many_accounts() {
        let v = vault();
        v.set("xkiro", "acc-1", "sk-one").unwrap();
        v.set("xkiro", "acc-2", "sk-two").unwrap();
        let before = v.items.reads();

        for _ in 0..5 {
            v.get("xkiro", "acc-1").unwrap();
            v.get("xkiro", "acc-2").unwrap();
        }

        assert_eq!(
            v.items.reads(),
            before,
            "the parsed vault is cached; only a write invalidates it"
        );
    }
}
