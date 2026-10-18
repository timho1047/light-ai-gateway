//! The Configuration page, rendered as a report.
//!
//! It shows the file the gateway is actually running, every known value, which
//! fields apply live and which need a restart, and a read-only view of the raw
//! document with literal secrets already redacted by the caller. This slice is
//! read-only: the editable draft and the apply pipeline land in later tasks.

use super::copy::{Lang, Strings};
use super::layout::esc;
use crate::config::{AccountConfig, Config, QuotaAdapter};
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

    out.push_str(&section(s.section_server, |body| {
        field(body, "server.listen", &view.config.server.listen, false, s);
        field(
            body,
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
        live(body, "policy.soft_threshold", &fnum(p.soft_threshold), s);
        live(body, "policy.allow_metered", &flag(p.allow_metered), s);
        live(
            body,
            "policy.poll_interval_secs",
            &p.poll_interval_secs.to_string(),
            s,
        );
        live(
            body,
            "policy.poll_interval_active_secs",
            &p.poll_interval_active_secs.to_string(),
            s,
        );
        live(
            body,
            "policy.poll_interval_idle_secs",
            &p.poll_interval_idle_secs.to_string(),
            s,
        );
        live(body, "policy.drift_threshold", &fnum(p.drift_threshold), s);
        live(body, "policy.hard_margin", &fnum(p.hard_margin), s);
        live(body, "policy.max_retries", &p.max_retries.to_string(), s);
        live(
            body,
            "policy.connect_timeout_secs",
            &p.connect_timeout_secs.to_string(),
            s,
        );
        live(
            body,
            "policy.ttft_timeout_secs",
            &p.ttft_timeout_secs.to_string(),
            s,
        );
        live(
            body,
            "policy.idle_timeout_secs",
            &p.idle_timeout_secs.to_string(),
            s,
        );
        live(
            body,
            "policy.probe_before_switch",
            &flag(p.probe_before_switch),
            s,
        );
        field(
            body,
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

    if let Some(raw) = view.raw {
        out.push_str(&raw_section(raw, s));
    }
    out
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
