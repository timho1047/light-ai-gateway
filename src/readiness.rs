//! The startup handshake between lgw serve and the launcher.
//!
//! The address is an output of the process that binds it, not a rendering
//! detail of the log. When a launcher asks for a record, serve writes the bound
//! URL once; a foreground serve without the environment variable has no
//! filesystem side effects.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const READY_FILE_ENV: &str = "LGW_READY_FILE";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ready {
    pub url: String,
}

/// Write the bound URL for a waiting launcher. Best effort: a failed auxiliary
/// write must not stop the gateway from serving.
pub fn publish(url: &str) -> Option<Result<()>> {
    let path = std::env::var_os(READY_FILE_ENV).filter(|value| !value.is_empty())?;
    Some(write(Path::new(&path), url))
}

pub fn write(path: &Path, url: &str) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec(&Ready {
        url: url.to_string(),
    })?;
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .with_context(|| format!("setting mode on {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("installing {}", path.display()))?;
    Ok(())
}

pub fn read(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let ready: Ready = serde_json::from_slice(&bytes).ok()?;
    if ready.url.is_empty() {
        return None;
    }
    Some(ready.url.trim_end_matches('/').to_string())
}

pub fn path_from_env() -> Option<PathBuf> {
    std::env::var_os(READY_FILE_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_round_trips_and_trims_one_trailing_slash() {
        let dir = std::env::temp_dir().join(format!(
            "lgw-ready-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ready.json");
        write(&path, "http://127.0.0.1:8787/").expect("write");
        assert_eq!(read(&path), Some("http://127.0.0.1:8787".to_string()));
        assert!(read(&dir.join("missing")).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
