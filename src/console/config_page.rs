//! The Configuration page: a draft editor over the file the gateway is running.
//!
//! Server and policy fields are inputs; provider, account, route and price rows
//! are still a report until their edit paths land. Every field carries a live or
//! restart-only marker, and the raw TOML view below shows the file with literal
//! secrets redacted. Validating a draft never writes: it checks the changed file
//! and shows the exact lines that would move.

use super::copy::{Lang, Strings};
use super::layout::esc;
use super::violations;
use crate::config::{AccountConfig, Config, QuotaAdapter};
use crate::config_doc::{DiffKind, DiffLine};
use std::fmt::Write;

pub struct ConfigView<'a> {
    pub config: &'a Config,
    pub path: &'a str,
    pub fingerprint: &'a str,
    pub last_applied: &'a str,
    pub raw: Option<&'a str>,
    pub lang: Lang,
}

pub fn render(view: &ConfigView) -> String {
    let s = view.lang.strings();
    let mut out = String::new();
    out.push_str(&header(view, s));
    let _ = write!(
        out,
        "<form id=\"config-form\" data-config-draft data-fingerprint=\"{}\">",
        esc(view.fingerprint)
    );

    out.push_str(&section(s.section_server, |body| {
        editable_text(
            body,
            &["server", "listen"],
            "server.listen",
            &view.config.server.listen,
            false,
            s,
        );
        editable_text(
            body,
            &["server", "db_path"],
            "server.db_path",
            &view.config.server.db_path,
            false,
            s,
        );
        let local_key = match &view.config.server.local_key {
            Some(secret) => secret.describe(),
            None => s.config_none.to_string(),
        };
        field(body, "server.local_key", &local_key, true, s);
    }));

    out.push_str(&section(s.section_policy, |body| {
        let p = &view.config.policy;
        editable_text(
            body,
            &["policy", "soft_threshold"],
            "policy.soft_threshold",
            &fnum(p.soft_threshold),
            true,
            s,
        );
        editable_bool(
            body,
            &["policy", "allow_metered"],
            "policy.allow_metered",
            p.allow_metered,
            true,
            s,
        );
        editable_text(
            body,
            &["policy", "poll_interval_secs"],
            "policy.poll_interval_secs",
            &p.poll_interval_secs.to_string(),
            true,
            s,
        );
        editable_text(
            body,
            &["policy", "poll_interval_active_secs"],
            "policy.poll_interval_active_secs",
            &p.poll_interval_active_secs.to_string(),
            true,
            s,
        );
        editable_text(
            body,
            &["policy", "poll_interval_idle_secs"],
            "policy.poll_interval_idle_secs",
            &p.poll_interval_idle_secs.to_string(),
            true,
            s,
        );
        editable_text(
            body,
            &["policy", "drift_threshold"],
            "policy.drift_threshold",
            &fnum(p.drift_threshold),
            true,
            s,
        );
        editable_text(
            body,
            &["policy", "hard_margin"],
            "policy.hard_margin",
            &fnum(p.hard_margin),
            true,
            s,
        );
        editable_text(
            body,
            &["policy", "max_retries"],
            "policy.max_retries",
            &p.max_retries.to_string(),
            true,
            s,
        );
        editable_text(
            body,
            &["policy", "connect_timeout_secs"],
            "policy.connect_timeout_secs",
            &p.connect_timeout_secs.to_string(),
            true,
            s,
        );
        editable_text(
            body,
            &["policy", "ttft_timeout_secs"],
            "policy.ttft_timeout_secs",
            &p.ttft_timeout_secs.to_string(),
            true,
            s,
        );
        editable_text(
            body,
            &["policy", "idle_timeout_secs"],
            "policy.idle_timeout_secs",
            &p.idle_timeout_secs.to_string(),
            true,
            s,
        );
        editable_bool(
            body,
            &["policy", "probe_before_switch"],
            "policy.probe_before_switch",
            p.probe_before_switch,
            true,
            s,
        );
        editable_text(
            body,
            &["policy", "max_body_bytes"],
            "policy.max_body_bytes",
            &p.max_body_bytes.to_string(),
            false,
            s,
        );
    }));

    out.push_str(&section(s.section_providers, |body| {
        if view.config.providers.is_empty() {
            let _ = write!(body, "<div class=\"empty\">{}</div>", esc(s.no_providers));
        }
        for (provider, cfg) in &view.config.providers {
            subhead(body, &format!("providers.{provider}"));
            field(body, "adapter", adapter_name(cfg.adapter), true, s);
            field(body, "base_url", &cfg.base_url, true, s);
            field(
                body,
                "probe_base_url",
                cfg.probe_base_url.as_deref().unwrap_or("—"),
                true,
                s,
            );
            field(body, "currency", &cfg.currency, true, s);
            field(
                body,
                "credit_multiplier",
                &cfg.credit_multiplier
                    .map(fnum)
                    .unwrap_or_else(|| "1".to_string()),
                true,
                s,
            );
            field(
                body,
                "allow_metered",
                &cfg.allow_metered
                    .map(flag)
                    .unwrap_or_else(|| s.follows_policy.to_string()),
                true,
                s,
            );
            for (id, account) in &cfg.accounts {
                subhead(body, &format!("providers.{provider}.accounts.{id}"));
                account_fields(body, account, &cfg.currency, s);
            }
        }
    }));

    out.push_str(&section(s.section_routes, |body| {
        if view.config.routes.is_empty() {
            let _ = write!(body, "<div class=\"empty\">{}</div>", esc(s.no_routes));
        } else {
            body.push_str(
                "<table class=\"table\"><thead><tr><th>routes.&lt;model&gt;</th><th>provider</th></tr></thead><tbody>",
            );
            for (model, provider) in &view.config.routes {
                let _ = write!(
                    body,
                    "<tr><td class=\"mono\">{}</td><td class=\"mono\">{}</td></tr>",
                    esc(model),
                    esc(provider)
                );
            }
            body.push_str("</tbody></table>");
        }
    }));

    out.push_str(&section(s.section_prices, |body| {
        if view.config.prices.is_empty() {
            let _ = write!(body, "<div class=\"empty\">{}</div>", esc(s.no_prices));
        }
        for (provider, models) in &view.config.prices {
            for (model, price) in models {
                subhead(body, &format!("prices.{provider}.{model}"));
                live(body, "input / M", &fnum(price.input), s);
                live(body, "output / M", &fnum(price.output), s);
                live(
                    body,
                    "cache read / M",
                    &price
                        .cache_read
                        .map(fnum)
                        .unwrap_or_else(|| "input / 10".to_string()),
                    s,
                );
                if !price.schedules.is_empty() {
                    subhead(body, s.config_schedules);
                }
                for (index, tier) in price.schedules.iter().enumerate() {
                    let name = tier
                        .name
                        .clone()
                        .unwrap_or_else(|| format!("#{}", index + 1));
                    let row = format!(
                        "{} · days {} · hours {} · tz {} · in {} · out {} · cache {}",
                        name,
                        tier.days,
                        tier.hours,
                        tier.tz.as_deref().unwrap_or("UTC"),
                        fnum(tier.input),
                        fnum(tier.output),
                        tier.cache_read
                            .map(fnum)
                            .unwrap_or_else(|| "input / 10".to_string()),
                    );
                    live(body, "schedule", &row, s);
                }
            }
        }
    }));

    out.push_str(&config_actions(view, s));
    out.push_str("</form>");

    if let Some(raw) = view.raw {
        out.push_str(&raw_section(raw, s));
    }
    out
}

/// What the form sends: one entry per changed leaf, the path split into TOML
/// keys so a model name containing a dot or slash is never parsed as structure.
fn data_path(path: &[&str]) -> String {
    let segments: Vec<String> = path
        .iter()
        .map(|segment| format!("\"{segment}\""))
        .collect();
    format!("[{}]", segments.join(","))
}

fn marker(live: bool, s: &Strings) -> String {
    let (class, text) = if live {
        ("live", s.config_live)
    } else {
        ("restart", s.config_restart)
    };
    format!("<span class=\"tag {}\">{}</span>", class, esc(text))
}

fn editable_text(
    out: &mut String,
    path: &[&str],
    label: &str,
    value: &str,
    live: bool,
    s: &Strings,
) {
    let _ = write!(
        out,
        r#"<div class="field"><span class="field-label mono">{}</span><input type="text" value="{}" data-path="{}" data-initial="{}" spellcheck="false" autocomplete="off"><span class="hint">{}</span></div>"#,
        esc(label),
        esc(value),
        esc(&data_path(path)),
        esc(value),
        marker(live, s),
    );
}

fn editable_bool(
    out: &mut String,
    path: &[&str],
    label: &str,
    value: bool,
    live: bool,
    s: &Strings,
) {
    let true_selected = if value { " selected" } else { "" };
    let false_selected = if value { "" } else { " selected" };
    let _ = write!(
        out,
        r#"<div class="field"><span class="field-label mono">{}</span><select data-path="{}" data-initial="{}"><option value="true"{true_selected}>true</option><option value="false"{false_selected}>false</option></select><span class="hint">{}</span></div>"#,
        esc(label),
        esc(&data_path(path)),
        if value { "true" } else { "false" },
        marker(live, s),
    );
}

fn config_actions(view: &ConfigView, s: &Strings) -> String {
    let url = format!("/console/api/config/validate?lang={}", view.lang.code());
    format!(
        r#"<div class="config-actions">
  <button class="btn btn-primary" type="button" data-config-validate data-validate-url="{}">{}</button>
  <span class="hint">{}</span>
</div>
<div class="config-result" data-config-result aria-live="polite"></div>"#,
        esc(&url),
        esc(s.config_validate),
        esc(s.config_validate_note),
    )
}

const CHANGED_ONLY: [DiffKind; 2] = [DiffKind::Removed, DiffKind::Added];

/// The panel after a valid draft: the exact lines that would move, and nothing
/// else. The page still writes nothing; the apply path is a later task.
pub fn validated(diff: &[DiffLine], lang: Lang) -> String {
    let s = lang.strings();
    let mut body = format!(
        "<div class=\"banner ok\"><b>{}</b></div>",
        esc(s.config_draft_ok)
    );
    let changed: Vec<&DiffLine> = diff
        .iter()
        .filter(|line| CHANGED_ONLY.contains(&line.kind))
        .collect();
    if changed.is_empty() {
        let _ = write!(
            body,
            "<div class=\"empty\">{}</div>",
            esc(s.config_no_changes)
        );
        return body;
    }
    let mut pre = String::new();
    for line in changed {
        let (sign, class) = match line.kind {
            DiffKind::Removed => ("-", "minus"),
            DiffKind::Added => ("+", "plus"),
            DiffKind::Same => continue,
        };
        let _ = writeln!(
            pre,
            "<span class=\"{class}\">{sign}{}</span>",
            esc(&line.text)
        );
    }
    let _ = write!(
        body,
        r#"<div class="diff"><div class="diff-head">{}</div><pre>{}</pre></div>"#,
        esc(s.config_diff_title),
        pre.trim_end(),
    );
    body
}

/// The panel after a rejected draft: the field, a page-language headline from
/// the stable code, and the validator's English sentence as technical detail.
pub fn rejected(field: &str, code: Option<&str>, message: &str, lang: Lang) -> String {
    let s = lang.strings();
    let headline = code
        .and_then(|code| violations::headline(lang, code))
        .unwrap_or(message);
    let detail = if code.is_some() && lang == Lang::Zh {
        format!("<div class=\"detail mono\">{}</div>", esc(message))
    } else {
        String::new()
    };
    format!(
        r#"<div class="banner error"><b>{}</b></div>
<ul class="error-list"><li><span class="field-label mono">{}</span><div>{}</div>{}</li></ul>"#,
        esc(s.config_draft_invalid),
        esc(field),
        esc(headline),
        detail,
    )
}

/// The draft was built against a file that has moved since.
pub fn stale(lang: Lang) -> String {
    let s = lang.strings();
    format!(
        "<div class=\"banner warn\"><b>{}</b></div>",
        esc(s.config_stale)
    )
}

/// A file that cannot be read or parsed; the draft never reached the validator.
pub fn failed(field: &str, message: &str, lang: Lang) -> String {
    let s = lang.strings();
    format!(
        r#"<div class="banner error"><b>{}</b></div>
<ul class="error-list"><li><span class="field-label mono">{}</span><div class="detail mono">{}</div></li></ul>"#,
        esc(s.config_draft_invalid),
        esc(field),
        esc(message),
    )
}

fn header(view: &ConfigView, s: &Strings) -> String {
    format!(
        r#"<section class="section">
  <div class="section-head">
    <div class="eyebrow">{}</div>
    <h2 class="section-title">{}</h2>
    <p class="path-line"><b>{}</b> · {} <span class="mono">{}</span> · {} {}</p>
  </div>
</section>"#,
        esc(s.config_eyebrow),
        esc(s.config_title),
        esc(view.path),
        esc(s.config_fingerprint),
        esc(view.fingerprint),
        esc(s.config_last_applied),
        esc(view.last_applied),
    )
}

fn raw_section(raw: &str, s: &Strings) -> String {
    format!(
        r#"<section class="section">
  <div class="section-head">
    <h2 class="section-title">{}</h2>
    <p class="lede">{}</p>
  </div>
  <pre class="raw-toml">{}</pre>
</section>"#,
        esc(s.config_raw_title),
        esc(s.config_rawnote),
        esc(raw),
    )
}

fn section(title: &str, build: impl FnOnce(&mut String)) -> String {
    let mut body = String::new();
    build(&mut body);
    format!(
        "<section class=\"section\"><div class=\"section-head\"><h2 class=\"section-title\">{}</h2></div>{}</section>",
        esc(title),
        body
    )
}

fn subhead(out: &mut String, label: &str) {
    let _ = write!(out, "<h3 class=\"section-sub mono\">{}</h3>", esc(label));
}

fn live(out: &mut String, label: &str, value: &str, s: &Strings) {
    field(out, label, value, true, s);
}

fn field(out: &mut String, label: &str, value: &str, applies_live: bool, s: &Strings) {
    let (class, marker) = if applies_live {
        ("live", s.config_live)
    } else {
        ("restart", s.config_restart)
    };
    let _ = write!(
        out,
        r#"<div class="field"><span class="field-label mono">{}</span><span class="mono">{}</span><span class="hint"><span class="tag {}">{}</span></span></div>"#,
        esc(label),
        esc(value),
        class,
        esc(marker),
    );
}

fn account_fields(out: &mut String, account: &AccountConfig, provider_currency: &str, s: &Strings) {
    let label = account.label.clone().unwrap_or_else(|| "—".to_string());
    let source = account
        .key
        .as_ref()
        .map(|k| k.describe())
        .unwrap_or_else(|| "credential-store".to_string());
    let currency = account
        .currency
        .clone()
        .unwrap_or_else(|| provider_currency.to_string());
    field(out, "label", &label, true, s);
    field(out, "key source", &source, true, s);
    field(out, "currency", &currency, true, s);
    field(
        out,
        "credit_multiplier",
        &account
            .credit_multiplier
            .map(fnum)
            .unwrap_or_else(|| "1".to_string()),
        true,
        s,
    );
    field(
        out,
        "concurrency_limit",
        &account
            .concurrency_limit
            .map(|v| v.to_string())
            .unwrap_or_else(|| "—".to_string()),
        true,
        s,
    );
    field(
        out,
        "rpm_limit",
        &account
            .rpm_limit
            .map(|v| v.to_string())
            .unwrap_or_else(|| "—".to_string()),
        true,
        s,
    );
    field(
        out,
        "allow_metered",
        &account
            .allow_metered
            .map(flag)
            .unwrap_or_else(|| s.follows_provider.to_string()),
        true,
        s,
    );
}

fn adapter_name(adapter: QuotaAdapter) -> &'static str {
    match adapter {
        QuotaAdapter::Xkiro => "xkiro",
        QuotaAdapter::CommandCode => "command-code",
        QuotaAdapter::Deepseek => "deepseek",
        QuotaAdapter::Haoai => "haoai",
        QuotaAdapter::None => "none",
    }
}

fn fnum(v: f64) -> String {
    let text = format!("{v}");
    if text.ends_with(".0") {
        text.trim_end_matches(".0").to_string()
    } else {
        text
    }
}

fn flag(v: bool) -> String {
    if v {
        "true".to_string()
    } else {
        "false".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::sample_toml;
    use crate::config_doc::{self, ConfigDocument, DraftChange};

    fn view<'a>(config: &'a Config, raw: &'a str) -> ConfigView<'a> {
        ConfigView {
            config,
            path: "/home/op/.light-ai-gateway/config.toml",
            fingerprint: "fnv64:deadbeef",
            last_applied: "2m ago",
            raw: Some(raw),
            lang: Lang::En,
        }
    }

    fn sample() -> Config {
        Config::from_toml_str(sample_toml()).expect("sample config")
    }

    #[test]
    fn a_rejected_draft_names_the_field_and_speaks_the_page_language() {
        let html = rejected(
            "policy.soft_threshold",
            Some("soft_threshold_out_of_range"),
            "policy.soft_threshold must be within 0.0..=1.0, got 1.5",
            Lang::Zh,
        );
        assert!(html.contains("policy.soft_threshold"));
        assert!(html.contains("0.0 到 1.0"));
        assert!(
            html.contains("must be within 0.0..=1.0"),
            "English detail is kept"
        );
        let english = rejected(
            "policy.soft_threshold",
            Some("soft_threshold_out_of_range"),
            "bad",
            Lang::En,
        );
        assert!(english.contains("bad"));
    }

    #[test]
    fn a_valid_draft_shows_only_the_moved_lines() {
        let document = ConfigDocument::parse("[policy]\nsoft_threshold = 0.8\n").expect("doc");
        let edited = document
            .with_draft(&[DraftChange {
                path: vec!["policy".to_string(), "soft_threshold".to_string()],
                value: "0.75".to_string(),
            }])
            .expect("draft");
        let html = validated(&config_doc::diff(&document, &edited), Lang::En);
        assert!(html.contains("-soft_threshold = 0.8"));
        assert!(html.contains("+soft_threshold = 0.75"));
        assert!(html.contains("Pending change"));
        assert!(
            !html.contains("[policy]"),
            "unchanged lines are not repeated"
        );

        let same = validated(&config_doc::diff(&document, &document), Lang::En);
        assert!(same.contains("No line would change."));
    }

    #[test]
    fn a_stale_draft_asks_for_a_reload() {
        let html = stale(Lang::En);
        assert!(html.contains("changed after this page loaded"));
    }

    #[test]
    fn every_section_and_the_restart_markers_are_rendered() {
        let config = sample();
        let html = render(&view(&config, "listen = \"127.0.0.1:8787\""));
        assert!(html.contains("Configuration"));
        assert!(html.contains("server.listen"));
        assert!(html.contains("server.db_path"));
        assert!(html.contains("policy.soft_threshold"));
        assert!(html.contains("Providers &amp; accounts"));
        assert!(html.contains("routes."));
        assert!(html.contains("Prices"));
        assert!(
            html.contains("restart required"),
            "listen and db_path need a restart"
        );
        assert!(html.contains("applies live"));
    }

    #[test]
    fn the_raw_view_is_escaped() {
        let config = sample();
        let html = render(&view(&config, "<script>alert(1)</script>"));
        assert!(!html.contains("<script>alert(1)</script>"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn a_literal_key_is_named_but_never_printed() {
        let toml = r#"
[server]
listen = "127.0.0.1:8787"

[providers.xkiro]
adapter = "xkiro"
base_url = "https://api.xkiro.com/v1"

[providers.xkiro.accounts.acc-1]
key = "sk-live-SECRET"
"#;
        let config = Config::from_toml_str(toml).expect("config");
        let html = render(&view(&config, "redacted"));
        assert!(html.contains("inline-literal"));
        assert!(!html.contains("sk-live-SECRET"));
    }

    #[test]
    fn the_chinese_page_is_chinese() {
        let config = sample();
        let mut page = view(&config, "");
        page.lang = Lang::Zh;
        let html = render(&page);
        assert!(html.contains("配置"));
        assert!(html.contains("立即生效"));
    }

    #[test]
    fn a_malicious_route_and_label_are_escaped() {
        let toml = r#"
[server]
listen = "127.0.0.1:8787"

[providers.xkiro]
adapter = "xkiro"
base_url = "https://api.xkiro.com/v1"

[providers.xkiro.accounts.acc-1]
label = "<img src=x onerror=alert(1)>"

[routes]
"glm/<x>" = "xkiro"
"#;
        let config = Config::from_toml_str(toml).expect("config");
        let html = render(&view(&config, ""));
        assert!(!html.contains("<img src=x"));
        assert!(!html.contains("glm/<x>"));
        assert!(html.contains("&lt;img"));
    }
}
