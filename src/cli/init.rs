//! `lgw init`: write an example configuration file.

use anyhow::{Context, Result, bail};
use std::path::Path;

pub(super) fn cmd_init(path: &Path, force: bool) -> Result<()> {
    if path.exists() && !force {
        bail!(
            "{} already exists; pass --force to overwrite it",
            path.display()
        );
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    // One shared writer: 0600 from the moment the file exists, and a rename
    // instead of a truncate so a crash cannot leave half a configuration.
    crate::config_doc::write_atomic(path, crate::config::sample_toml())?;
    println!("wrote example config to {}", path.display());
    println!("next: set the credentials it references, then run lgw doctor");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn init_writes_the_sample_and_refuses_to_clobber() {
        let dir = std::env::temp_dir().join(format!(
            "lgw-cli-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        cmd_init(&path, false).unwrap();
        assert!(path.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the configuration file must be private");
        }
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            crate::config::sample_toml()
        );
        // The written sample must be loadable as-is (no live credentials are
        // needed to parse it, only to build accounts).
        Config::load(&path).expect("the sample config must parse");

        let err = cmd_init(&path, false).unwrap_err().to_string();
        assert!(err.contains("--force"), "got: {err}");

        std::fs::write(&path, "clobbered").unwrap();
        cmd_init(&path, true).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            crate::config::sample_toml()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
