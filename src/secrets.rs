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

use crate::domain::ApiKey;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Service name under which entries are filed in the OS credential store.
pub const SERVICE: &str = "light-ai-gateway";

/// Reserved provider/account pair for the gateway's own local access token.
/// Register it with: ai-gw keys add local gateway
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
}

/// The composite key used inside the store.
pub fn entry_user(provider: &str, account: &str) -> String {
    format!("{provider}/{account}")
}

/// The message shown when an account has no credential. It names the exact
/// command that fixes the problem, because the alternative is an operator
/// restarting with an empty environment and guessing.
pub fn missing_credential_message(provider: &str, account: &str) -> String {
    format!(
        "no credential for {provider}/{account}. Run: ai-gw keys add {provider} {account}"
    )
}

/// The operating system credential store.
pub struct KeychainBackend {
    service: String,
}

impl Default for KeychainBackend {
    fn default() -> Self {
        KeychainBackend::new(SERVICE)
    }
}

impl KeychainBackend {
    pub fn new(service: impl Into<String>) -> Self {
        KeychainBackend {
            service: service.into(),
        }
    }

    fn entry(&self, provider: &str, account: &str) -> Result<keyring::Entry> {
        keyring::Entry::new(&self.service, &entry_user(provider, account))
            .context("opening the OS credential store entry")
    }
}

impl SecretBackend for KeychainBackend {
    fn name(&self) -> &'static str {
        "os-keychain"
    }

    fn set(&self, provider: &str, account: &str, secret: &str) -> Result<()> {
        self.entry(provider, account)?
            .set_password(secret)
            .context("storing the credential in the OS credential store")
    }

    fn get(&self, provider: &str, account: &str) -> Result<Option<ApiKey>> {
        match self.entry(provider, account)?.get_password() {
            Ok(secret) => Ok(Some(ApiKey::new(secret))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(anyhow::anyhow!("{e}")),
        }
    }

    fn delete(&self, provider: &str, account: &str) -> Result<bool> {
        match self.entry(provider, account)?.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(e) => Err(anyhow::anyhow!("{e}")),
        }
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
    /// AI_GW_CREDENTIAL_BACKEND=memory forces an ephemeral store, so dry runs and
    /// CI never touch the operator's real keychain. EVERY caller has to come
    /// through here. When only doctor and keys honoured the setting, "serve"
    /// read the real keychain anyway -- so the documented promise was false
    /// exactly where it mattered, and a dry run that believed it was isolated
    /// still raised authorisation prompts.
    pub fn from_env() -> Self {
        match std::env::var("AI_GW_CREDENTIAL_BACKEND").as_deref() {
            Ok("memory") => Self::memory(),
            _ => Self::keychain(),
        }
    }

    /// The real store, backed by the OS credential store.
    pub fn keychain() -> Self {
        CredentialStore {
            backend: Arc::new(KeychainBackend::default()),
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
        assert_eq!(s.get("deepseek", "acc-1").unwrap().unwrap().expose(), "three");
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
        assert!(msg.contains("ai-gw keys add xkiro acc-1"), "got: {msg}");
    }

    #[test]
    fn memory_backend_reports_itself_as_memory() {
        assert_eq!(store().backend_name(), "memory");
    }
}
