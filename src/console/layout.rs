//! The page shell: head, masthead, navigation, language and theme controls,
//! the footer and the confirm dialog every page shares.

use super::copy::{Lang, Strings};
use std::fmt::Write;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    Board,
    Usage,
    Requests,
    Prices,
    Config,
    Keys,
    Doctor,
}

impl Nav {
    fn path(self) -> &'static str {
        match self {
            Nav::Board => "/",
            Nav::Usage => "/usage",
            Nav::Requests => "/requests",
            Nav::Prices => "/prices",
            Nav::Config => "/config",
            Nav::Keys => "/keys",
            Nav::Doctor => "/doctor",
        }
    }

    fn label(self, s: &Strings) -> &'static str {
        match self {
            Nav::Board => s.nav_board,
            Nav::Usage => s.nav_usage,
            Nav::Requests => s.nav_requests,
            Nav::Prices => s.nav_prices,
            Nav::Config => s.nav_config,
            Nav::Keys => s.nav_keys,
            Nav::Doctor => s.nav_doctor,
        }
    }

    fn all() -> [Nav; 7] {
        [
            Nav::Board,
            Nav::Usage,
            Nav::Requests,
            Nav::Prices,
            Nav::Config,
            Nav::Keys,
            Nav::Doctor,
        ]
    }
}

/// Escape text that came from configuration, the ledger or a provider.
pub fn esc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

fn href_with_lang(path: &str, lang: Lang) -> String {
    match lang {
        Lang::En => path.to_string(),
        Lang::Zh => format!("{path}?lang=zh"),
    }
}

/// Render a full page around the body a page module produced.
pub fn render(
    lang: Lang,
    active: Nav,
    title: &str,
    listen: &str,
    config_path: &str,
    body: &str,
) -> String {
    let s = lang.strings();
    let version = env!("CARGO_PKG_VERSION");

    let mut nav = String::new();
    for item in Nav::all() {
        let current = if item == active {
            " aria-current=\"page\""
        } else {
            ""
        };
        let _ = write!(
            nav,
            "<a href=\"{}\"{current}>{}</a>",
            href_with_lang(item.path(), lang),
            esc(item.label(s))
        );
    }

    let (lang_target, lang_label) = match lang {
        Lang::En => ("zh", "简体中文"),
        Lang::Zh => ("en", "English"),
    };
    let lang_href = match lang {
        Lang::En => format!("{}?lang=zh", active.path()),
        Lang::Zh => active.path().to_string(),
    };

    format!(
        r#"<!doctype html>
<html lang="{lang_code}" data-theme="light">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} · {suffix}</title>
<link rel="icon" href="/console/assets/favicon.svg" type="image/svg+xml">
<link rel="stylesheet" href="/console/assets/console.css">
<script src="/console/assets/theme.js"></script>
</head>
<body data-nav="{nav_name}">
<header class="site-header">
  <div class="kicker">{kicker}</div>
  <div class="mast-row">
    <h1 class="mast-title">{title}</h1>
    <div class="mast-meta"><span class="live-dot"></span><b>{live}</b> · {listen}<br>lgw {version}</div>
  </div>
</header>
<nav class="site-nav" aria-label="Console">
  {nav}
  <span class="spacer"></span>
  <div class="nav-tools">
    <button class="btn btn-sm theme-toggle" type="button" data-theme-toggle aria-pressed="false">
      <span class="theme-dark-label">{to_night}</span><span class="theme-light-label">{to_day}</span>
    </button>
    <a class="btn btn-sm lang-switch" href="{lang_href}" data-lang="{lang_target}">{lang_label}</a>
  </div>
</nav>
<main class="page">
{body}
</main>
<footer class="site-footer">
  <span>lgw {version} · {loopback}</span>
  <span>{config_label} {config_path}</span>
</footer>
<dialog class="confirm" data-confirm-dialog>
  <div class="confirm-body">
    <h2>{confirm_title}</h2>
    <p data-confirm-message></p>
  </div>
  <div class="confirm-actions">
    <button class="btn" type="button" data-confirm-cancel>{confirm_cancel}</button>
    <button class="btn btn-primary" type="button" data-confirm-accept>{confirm_accept}</button>
  </div>
</dialog>
<script src="/console/assets/console.js" defer></script>
</body>
</html>"#,
        nav_name = match active {
            Nav::Board => "board",
            Nav::Usage => "usage",
            Nav::Requests => "requests",
            Nav::Prices => "prices",
            Nav::Config => "config",
            Nav::Keys => "keys",
            Nav::Doctor => "doctor",
        },
        lang_code = lang.code(),
        title = esc(title),
        suffix = esc(s.title_suffix),
        kicker = esc(s.kicker),
        live = esc(s.live),
        listen = esc(listen),
        nav = nav,
        to_night = esc(s.theme_to_night),
        to_day = esc(s.theme_to_day),
        lang_href = esc(&lang_href),
        lang_target = lang_target,
        lang_label = lang_label,
        loopback = esc(s.footer_loopback),
        config_label = esc(s.footer_config),
        config_path = esc(config_path),
        version = version,
        confirm_title = esc(s.confirm_title),
        confirm_cancel = esc(s.confirm_cancel),
        confirm_accept = esc(s.confirm_accept),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(lang: Lang) -> String {
        render(
            lang,
            Nav::Board,
            "Board",
            "127.0.0.1:8787",
            "/home/op/.light-ai-gateway/config.toml",
            "<p>body</p>",
        )
    }

    #[test]
    fn the_shell_carries_the_theme_language_and_asset_hooks() {
        let html = page(Lang::En);
        assert!(html.contains("data-theme-toggle"));
        assert!(html.contains("/console/assets/console.css"));
        assert!(html.contains("/console/assets/console.js"));
        assert!(html.contains("/console/assets/theme.js"));
        assert!(html.contains("data-lang=\"zh\""));
        assert!(html.contains("aria-current=\"page\""));
        assert!(html.contains("<html lang=\"en\""));
    }

    #[test]
    fn chinese_pages_link_back_to_english() {
        let html = page(Lang::Zh);
        assert!(html.contains("<html lang=\"zh\""));
        assert!(html.contains("data-lang=\"en\""));
        assert!(html.contains("English"));
    }

    #[test]
    fn dynamic_text_is_escaped() {
        let html = render(
            Lang::En,
            Nav::Board,
            "<script>alert(1)</script>",
            "127.0.0.1:8787",
            "/tmp/a<b>.toml",
            "",
        );
        assert!(!html.contains("<script>alert(1)</script>"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("/tmp/a&lt;b&gt;.toml"));
    }
}
