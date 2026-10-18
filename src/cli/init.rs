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
    // 0600 from the moment it exists. The template carries no secret, but it is
    // the file people then put credentials-adjacent settings in, and a config
    // written world-readable is world-readable for the rest of its life.
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("writing {}", path.display()))?;
        use std::io::Write as _;
        file.write_all(crate::config::sample_toml().as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
        // --force over an existing file keeps that file's mode, so make it
        // explicit rather than inheriting whatever the old file had.
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    std::fs::write(path, crate::config::sample_toml())
        .with_context(|| format!("writing {}", path.display()))?;
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
