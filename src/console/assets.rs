//! The console's static assets, embedded in the binary.
//!
//! The install story is one self-contained binary, so a browser refresh must
//! never need a toolchain, a CDN or a directory next to the executable. A
//! development override in the HTTP layer serves these from disk instead.

pub struct Asset {
    pub content_type: &'static str,
    pub body: &'static [u8],
}

/// Look up an asset by the path segment after the asset prefix.
pub fn get(path: &str) -> Option<Asset> {
    match path {
        "console.css" => Some(Asset {
            content_type: "text/css; charset=utf-8",
            body: include_bytes!("assets/console.css"),
        }),
        "console.js" => Some(Asset {
            content_type: "text/javascript; charset=utf-8",
            body: include_bytes!("assets/console.js"),
        }),
        "theme.js" => Some(Asset {
            content_type: "text/javascript; charset=utf-8",
            body: include_bytes!("assets/theme.js"),
        }),
        "favicon.svg" => Some(Asset {
            content_type: "image/svg+xml",
            body: include_bytes!("assets/favicon.svg"),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shell_assets_are_embedded() {
        for path in ["console.css", "console.js", "theme.js", "favicon.svg"] {
            let asset = get(path).unwrap_or_else(|| panic!("{path} must be embedded"));
            assert!(!asset.body.is_empty(), "{path} must not be empty");
            assert!(
                asset.content_type.starts_with("text/") || asset.content_type == "image/svg+xml",
                "{path} has a browser-safe content type"
            );
        }
    }

    #[test]
    fn an_unknown_asset_is_not_found() {
        assert!(get("../etc/passwd").is_none());
        assert!(get("console.css/../../secret").is_none());
        assert!(get("").is_none());
    }
}
