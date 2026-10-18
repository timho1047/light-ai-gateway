//! The console's static assets: embedded in release, readable from disk while
//! developing.
//!
//! The install story is one self-contained binary, so a browser refresh must
//! never need a toolchain, a CDN or a directory next to the executable. Setting
//! LGW_UI_DIR serves the same names from that directory first, which is how CSS
//! and copy are iterated without a rebuild.

use std::path::{Component, Path};

pub struct Asset {
    pub content_type: &'static str,
    pub body: Body,
}

pub enum Body {
    Embedded(&'static [u8]),
    Owned(Vec<u8>),
}

/// Look up an asset by the path segment after the asset prefix.
///
/// A development override wins when LGW_UI_DIR is set and the file exists; the
/// embedded copy is the fallback, so a missing development file is not a 500.
pub fn get(path: &str) -> Option<Asset> {
    if let Some(dir) = std::env::var_os("LGW_UI_DIR")
        && let Some(asset) = from_disk(Path::new(&dir), path)
    {
        return Some(asset);
    }
    embedded(path)
}

fn embedded(path: &str) -> Option<Asset> {
    match path {
        "console.css" => Some(Asset {
            content_type: "text/css; charset=utf-8",
            body: Body::Embedded(include_bytes!("assets/console.css")),
        }),
        "console.js" => Some(Asset {
            content_type: "text/javascript; charset=utf-8",
            body: Body::Embedded(include_bytes!("assets/console.js")),
        }),
        "theme.js" => Some(Asset {
            content_type: "text/javascript; charset=utf-8",
            body: Body::Embedded(include_bytes!("assets/theme.js")),
        }),
        "favicon.svg" => Some(Asset {
            content_type: "image/svg+xml",
            body: Body::Embedded(include_bytes!("assets/favicon.svg")),
        }),
        _ => None,
    }
}

/// Read one flat asset name from a development directory.
///
/// Only the final path component is used and only a plain file name is
/// accepted, so a request can never walk out of the directory it named.
pub fn from_disk(dir: &Path, path: &str) -> Option<Asset> {
    let name = safe_name(path)?;
    let full = dir.join(name);
    let body = std::fs::read(full).ok()?;
    Some(Asset {
        content_type: content_type(path)?,
        body: Body::Owned(body),
    })
}

fn safe_name(path: &str) -> Option<&str> {
    let mut components = Path::new(path).components();
    match components.next()? {
        Component::Normal(name) => {
            if components.next().is_some() {
                return None;
            }
            name.to_str()
        }
        _ => None,
    }
}

fn content_type(path: &str) -> Option<&'static str> {
    let extension = path.rsplit('.').next()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shell_assets_are_embedded() {
        for path in ["console.css", "console.js", "theme.js", "favicon.svg"] {
            let asset = embedded(path).unwrap_or_else(|| panic!("{path} must be embedded"));
            assert!(!matches!(asset.body, Body::Owned(_)));
            assert!(
                asset.content_type.starts_with("text/") || asset.content_type == "image/svg+xml",
                "{path} has a browser-safe content type"
            );
        }
    }

    #[test]
    fn an_unknown_asset_is_not_found() {
        assert!(embedded("../etc/passwd").is_none());
        assert!(embedded("console.css/../../secret").is_none());
        assert!(embedded("").is_none());
        assert!(embedded("console.txt").is_none());
    }

    #[test]
    fn a_development_override_reads_only_flat_names() {
        let dir = std::env::temp_dir().join(format!("lgw-ui-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("console.css"), "body { color: red }").expect("write");

        let asset = from_disk(&dir, "console.css").expect("override file");
        assert_eq!(asset.content_type, "text/css; charset=utf-8");
        match asset.body {
            Body::Owned(bytes) => assert!(bytes.starts_with(b"body")),
            Body::Embedded(_) => panic!("the override must come from disk"),
        }

        assert!(from_disk(&dir, "../console.css").is_none());
        assert!(from_disk(&dir, "nested/console.css").is_none());
        assert!(from_disk(&dir, "missing.css").is_none());
        assert!(from_disk(&dir, "console.exe").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
