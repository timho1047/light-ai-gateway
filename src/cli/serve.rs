//! `lgw serve`: start the proxy.

use super::{ConfigOrigin, load_config};
use anyhow::Result;
use std::path::Path;

pub(super) async fn cmd_serve(path: &Path, origin: ConfigOrigin) -> Result<()> {
    let config = load_config(path)?;
    println!(
        "lgw: starting proxy from {} ({})",
        path.display(),
        origin.label()
    );
    crate::http::serve(config).await
}
