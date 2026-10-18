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
    /// Why the last external edit was refused, if it was.
    pub reload_error: Option<&'a crate::reload::ReloadError>,
    pub lang: Lang,
}

pub fn render(view: &ConfigView) -> String {
    let s = view.lang.strings();
    let mut out = String::new();
    out.push_str(&header(view, s));
    if let Some(error) = view.reload_error {
        out.push_str(&reload_banner(error, s));
    }
    let _ = write!(
        out,
        "<form id=\"config-form\" data-config-draft data-fingerprint=\"{}\" data-pending-label=\"{}\">",
        esc(view.fingerprint),
        esc(s.config_pending),
    );
    out.push_str(&config_shell(view, s));

    out.push_str(&panel("server", s.section_server, |body| {
        // Listener settings are read-only here: they take effect on the next
        // start, and an editable input would imply an Apply can move them.
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
        field(body, "server.local_key", &local_key, false, s);
    }));

    out.push_str(&panel("policy", s.section_policy, |body| {
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
        field(
            body,
            "policy.max_body_bytes",
            &p.max_body_bytes.to_string(),
            false,
            s,
        );
    }));

    out.push_str(&panel("providers", s.section_providers, |body| {
        if view.config.providers.is_empty() {
            let _ = write!(body, "<div class=\"empty\">{}</div>", esc(s.no_providers));
        }
        for (provider, cfg) in &view.config.providers {
            subhead(body, &format!("providers.{provider}"));
            let provider_path: [&str; 2] = ["providers", provider.as_str()];
            editable_text(
                body,
                &[provider_path[0], provider_path[1], "adapter"],
                "adapter",
                adapter_name(cfg.adapter),
                true,
                s,
            );
            editable_text(
                body,
                &[provider_path[0], provider_path[1], "base_url"],
                "base_url",
                &cfg.base_url,
                true,
                s,
            );
            editable_text(
                body,
                &[provider_path[0], provider_path[1], "probe_base_url"],
                "probe_base_url",
                cfg.probe_base_url.as_deref().unwrap_or(""),
                true,
                s,
            );
            editable_text(
                body,
                &[provider_path[0], provider_path[1], "currency"],
                "currency",
                &cfg.currency,
                true,
                s,
            );
            editable_text(
                body,
                &[provider_path[0], provider_path[1], "credit_multiplier"],
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
                account_fields(body, provider, id, account, &cfg.currency, s);
            }
            body.push_str(&add_account_form(provider, s));
        }
    }));

    out.push_str(&panel("routing", s.config_tab_routing, |body| {
        subhead(body, s.section_routes);
        if view.config.routes.is_empty() {
            let _ = write!(body, r#"<div class="empty">{}</div>"#, esc(s.no_routes));
        } else {
            body.push_str(
                r#"<table class="table"><thead><tr><th>routes.&lt;model&gt;</th><th>provider</th><th></th></tr></thead><tbody>"#,
            );
            for (model, provider) in &view.config.routes {
                let _ = write!(body, r#"<tr><td class="mono">{}</td><td>"#, esc(model));
                leaf_input(body, &["routes", model], provider);
                body.push_str("</td><td>");
                let payload = serde_json::json!({ "model": model }).to_string();
                remove_button(body, "remove_route", &payload, s);
                body.push_str("</td></tr>");
            }
            body.push_str("</tbody></table>");
        }
        let payload = serde_json::json!({}).to_string();
        add_form(
            body,
            "add_route",
            s.config_add_route,
            &payload,
            "model",
            &[("model", s.config_model), ("provider", s.config_provider)],
        );

        subhead(body, s.section_aliases);
        if view.config.provider_prefix_aliases.is_empty() {
            let _ = write!(body, r#"<div class="empty">{}</div>"#, esc(s.no_aliases));
        } else {
            body.push_str(
                r#"<table class="table"><thead><tr><th>alias</th><th>provider</th><th></th></tr></thead><tbody>"#,
            );
            for (alias, provider) in &view.config.provider_prefix_aliases {
                let _ = write!(body, r#"<tr><td class="mono">{}</td><td>"#, esc(alias));
                leaf_input(body, &["provider_prefix_aliases", alias], provider);
                body.push_str("</td><td>");
                let payload = serde_json::json!({ "alias": alias }).to_string();
                remove_button(body, "remove_alias", &payload, s);
                body.push_str("</td></tr>");
            }
            body.push_str("</tbody></table>");
        }
        let payload = serde_json::json!({}).to_string();
        add_form(
            body,
            "add_alias",
            s.config_add_alias,
            &payload,
            "alias",
            &[("alias", s.config_alias), ("provider", s.config_provider)],
        );
    }));

    out.push_str(&panel("prices", s.section_prices, |body| {
        if view.config.prices.is_empty() {
            let _ = write!(body, r#"<div class="empty">{}</div>"#, esc(s.no_prices));
        }
        for (provider, models) in &view.config.prices {
            for (model, price) in models {
                subhead(body, &format!("prices.{provider}.{model}"));
                editable_text(
                    body,
                    &["prices", provider, model, "input"],
                    s.config_input_m,
                    &fnum(price.input),
                    true,
                    s,
                );
                editable_text(
                    body,
                    &["prices", provider, model, "output"],
                    s.config_output_m,
                    &fnum(price.output),
                    true,
                    s,
                );
                editable_text(
                    body,
                    &["prices", provider, model, "cache_read"],
                    s.config_cache_m,
                    &price.cache_read.map(fnum).unwrap_or_default(),
                    true,
                    s,
                );
                let price_payload =
                    serde_json::json!({ "provider": provider, "model": model }).to_string();
                body.push_str(r#"<div class="account-actions">"#);
                remove_button(body, "remove_price", &price_payload, s);
                body.push_str("</div>");

                if price.schedules.is_empty() {
                    let _ = write!(
                        body,
                        r#"<div class="hint">{}</div>"#,
                        esc(s.config_no_schedules)
                    );
                }
                for (index, tier) in price.schedules.iter().enumerate() {
                    let index_text = index.to_string();
                    subhead(body, &format!("{} #{}", s.config_schedules, index + 1));
                    let name_path = [
                        "prices",
                        provider.as_str(),
                        model.as_str(),
                        "schedules",
                        index_text.as_str(),
                        "name",
                    ];
                    editable_text(
                        body,
                        &name_path,
                        s.config_schedule_name,
                        tier.name.as_deref().unwrap_or(""),
                        true,
                        s,
                    );
                    let days_path = [
                        "prices",
                        provider.as_str(),
                        model.as_str(),
                        "schedules",
                        index_text.as_str(),
                        "days",
                    ];
                    editable_text(body, &days_path, s.config_days, &tier.days, true, s);
                    let hours_path = [
                        "prices",
                        provider.as_str(),
                        model.as_str(),
                        "schedules",
                        index_text.as_str(),
                        "hours",
                    ];
                    editable_text(body, &hours_path, s.config_hours, &tier.hours, true, s);
                    let tz_path = [
                        "prices",
                        provider.as_str(),
                        model.as_str(),
                        "schedules",
                        index_text.as_str(),
                        "tz",
                    ];
                    editable_text(
                        body,
                        &tz_path,
                        s.config_tz,
                        tier.tz.as_deref().unwrap_or(""),
                        true,
                        s,
                    );
                    let input_path = [
                        "prices",
                        provider.as_str(),
                        model.as_str(),
                        "schedules",
                        index_text.as_str(),
                        "input",
                    ];
                    editable_text(
                        body,
                        &input_path,
                        s.config_input_m,
                        &fnum(tier.input),
                        true,
                        s,
                    );
                    let output_path = [
                        "prices",
                        provider.as_str(),
                        model.as_str(),
                        "schedules",
                        index_text.as_str(),
                        "output",
                    ];
                    editable_text(
                        body,
                        &output_path,
                        s.config_output_m,
                        &fnum(tier.output),
                        true,
                        s,
                    );
                    let cache_path = [
                        "prices",
                        provider.as_str(),
                        model.as_str(),
                        "schedules",
                        index_text.as_str(),
                        "cache_read",
                    ];
                    editable_text(
                        body,
                        &cache_path,
                        s.config_cache_m,
                        &tier.cache_read.map(fnum).unwrap_or_default(),
                        true,
                        s,
                    );
                    let schedule_payload = serde_json::json!({
                        "provider": provider,
                        "model": model,
                        "index": index
                    })
                    .to_string();
                    body.push_str(r#"<div class="account-actions">"#);
                    remove_button(body, "remove_schedule", &schedule_payload, s);
                    body.push_str("</div>");
                }
                let schedule_payload =
                    serde_json::json!({ "provider": provider, "model": model }).to_string();
                add_form(
                    body,
                    "add_schedule",
                    s.config_add_schedule,
                    &schedule_payload,
                    "days",
                    &[
                        ("name", s.config_schedule_name),
                        ("days", s.config_days),
                        ("hours", s.config_hours),
                        ("tz", s.config_tz),
                        ("input", s.config_input_m),
                        ("output", s.config_output_m),
                        ("cache_read", s.config_cache_m),
                    ],
                );
            }
            let price_payload = serde_json::json!({ "provider": provider }).to_string();
            add_form(
                body,
                "add_price",
                s.config_add_price,
                &price_payload,
                "model",
                &[
                    ("model", s.config_model),
                    ("input", s.config_input_m),
                    ("output", s.config_output_m),
                    ("cache_read", s.config_cache_m),
                ],
            );
        }
    }));

    if let Some(raw) = view.raw {
        out.push_str(&panel("raw", s.config_raw_title, |body| {
            let _ = write!(
                body,
                r#"<p class="lede">{}</p><pre class="raw-toml">{}</pre>"#,
                esc(s.config_rawnote),
                esc(raw),
            );
        }));
    }
    out.push_str(&config_actions(view, s));
    out.push_str("</div></div></form>");
    out
}

fn leaf_input(out: &mut String, path: &[&str], value: &str) {
    let _ = write!(
        out,
        r#"<input class="table-input" type="text" value="{}" data-path="{}" data-initial="{}" spellcheck="false" autocomplete="off">"#,
        esc(value),
        esc(&data_path(path)),
        esc(value),
    );
}

fn remove_button(out: &mut String, action: &str, payload: &str, s: &Strings) {
    let _ = write!(
        out,
        r#"<button class="btn btn-small danger" type="button" data-remove-action="{}" data-payload="{}" data-remove-label="{}" data-undo-label="{}">{}</button>"#,
        esc(action),
        esc(payload),
        esc(s.config_remove),
        esc(s.config_remove_undo_short),
        esc(s.config_remove),
    );
}

fn add_form(
    out: &mut String,
    kind: &str,
    title: &str,
    payload: &str,
    required: &str,
    fields: &[(&str, &str)],
) {
    let _ = write!(
        out,
        r#"<details class="add-account" data-add-form data-kind="{}" data-static="{}" data-required="{}"><summary>{}</summary><div class="add-account-grid">"#,
        esc(kind),
        esc(payload),
        esc(required),
        esc(title),
    );
    for (name, label) in fields {
        let _ = write!(
            out,
            r#"<label><span class="field-label mono">{}</span><input type="text" data-add-field="{}" autocomplete="off" spellcheck="false"></label>"#,
            esc(label),
            esc(name),
        );
    }
    out.push_str("</div></details>");
}

fn add_account_form(provider: &str, s: &Strings) -> String {
    format!(
        r#"<details class="add-account" data-add-account data-provider="{}">
  <summary>{}</summary>
  <div class="add-account-grid">
    <label><span class="field-label mono">{}</span><input type="text" data-add-field="id" autocomplete="off" spellcheck="false"></label>
    <label><span class="field-label mono">{}</span><input type="text" data-add-field="label" autocomplete="off"></label>
    <label><span class="field-label mono">{}</span><select data-add-field="source"><option value="keychain">{}</option><option value="env">{}</option><option value="file">{}</option></select></label>
    <label><span class="field-label mono">{}</span><input type="text" data-add-field="source_value" autocomplete="off" spellcheck="false"></label>
  </div>
  <p class="hint">{}</p>
</details>"#,
        esc(provider),
        esc(s.config_add_account),
        esc(s.config_account_id),
        esc(s.config_account_label),
        esc(s.config_key_source),
        esc(s.config_keychain),
        esc(s.config_key_env),
        esc(s.config_key_file),
        esc(s.config_key_value),
        esc(s.config_add_note),
    )
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
    let segment = format!("lang={}", view.lang.code());
    let validate_url = format!("/console/api/config/validate?{segment}");
    let apply_url = format!("/console/api/config/apply?{segment}");
    format!(
        r#"<div class="config-actions">
  <button class="btn" type="button" data-config-validate data-validate-url="{}">{}</button>
  <button class="btn btn-primary" type="button" data-config-apply data-apply-url="{}" data-confirm="{}">{}</button>
  <span class="hint">{} {}</span>
</div>
<div class="config-result" data-config-result aria-live="polite"></div>"#,
        esc(&validate_url),
        esc(s.config_validate),
        esc(&apply_url),
        esc(s.config_apply_confirm),
        esc(s.config_apply),
        esc(s.config_validate_note),
        esc(s.config_apply_note),
    )
}

const CHANGED_ONLY: [DiffKind; 2] = [DiffKind::Removed, DiffKind::Added];

/// The panel after a valid draft: the exact lines that would move, and nothing
/// else. The page still writes nothing; this is the validate half of apply.
pub fn validated(diff: &[DiffLine], lang: Lang) -> String {
    let s = lang.strings();
    let mut body = format!(
        "<div class=\"banner ok\"><b>{}</b></div>",
        esc(s.config_draft_ok)
    );
    body.push_str(&diff_block(diff, s.config_diff_title, s.config_no_changes));
    body
}

/// The panel after an apply: the change is live, and the form's fingerprint
/// moves with the file so the next validate is not refused as stale.
pub fn applied(diff: &[DiffLine], fingerprint: &str, reload: bool, lang: Lang) -> String {
    let s = lang.strings();
    let reload_attr = if reload { " data-reload=\"true\"" } else { "" };
    let mut body = format!(
        "<div data-applied=\"true\" data-fingerprint=\"{}\"{}><div class=\"banner ok\"><b>{}</b></div>",
        esc(fingerprint),
        reload_attr,
        esc(s.config_applied),
    );
    body.push_str(&diff_block(diff, s.config_diff_title, s.config_no_changes));
    body.push_str("</div>");
    body
}

/// A field that cannot change without a restart, named rather than applied.
pub fn restart_only(fields: &[String], lang: Lang) -> String {
    let s = lang.strings();
    refused(s.config_apply_failed, s.config_restart_only, fields)
}

/// A section whose apply path has not landed yet. Refused loudly instead of
/// being silently written and ignored.
pub fn not_live(fields: &[String], lang: Lang) -> String {
    let s = lang.strings();
    refused(s.config_apply_failed, s.config_not_live, fields)
}

fn refused(banner: &str, heading: &str, fields: &[String]) -> String {
    let mut items = String::new();
    for field in fields {
        let _ = write!(items, "<li><span class=\"mono\">{}</span></li>", esc(field));
    }
    format!(
        r#"<div class="banner error"><b>{}</b></div><p class="field-label mono">{}</p><ul class="error-list">{}</ul>"#,
        esc(banner),
        esc(heading),
        items,
    )
}

fn diff_block(diff: &[DiffLine], title: &str, empty: &str) -> String {
    let changed: Vec<&DiffLine> = diff
        .iter()
        .filter(|line| CHANGED_ONLY.contains(&line.kind))
        .collect();
    if changed.is_empty() {
        return format!("<div class=\"empty\">{}</div>", esc(empty));
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
    format!(
        r#"<div class="diff"><div class="diff-head">{}</div><pre>{}</pre></div>"#,
        esc(title),
        pre.trim_end(),
    )
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

fn reload_banner(error: &crate::reload::ReloadError, s: &Strings) -> String {
    format!(
        r#"<div class="banner error"><b>{}</b><div class="mono detail">{}</div><div class="hint">{}</div></div>"#,
        esc(s.config_external_error),
        esc(&error.message),
        esc(s.config_external_kept),
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

/// The section rail and the panel column. It is a plain list of hash links until
/// the script upgrades it into tabs, so every section stays reachable without
/// JavaScript (all panels are visible in that state).
fn config_shell(view: &ConfigView, s: &Strings) -> String {
    let mut tabs = String::new();
    let entries = [
        ("server", s.section_server),
        ("policy", s.section_policy),
        ("providers", s.section_providers),
        ("routing", s.config_tab_routing),
        ("prices", s.section_prices),
        ("raw", s.config_raw_title),
    ];
    for (key, title) in entries {
        if key == "raw" && view.raw.is_none() {
            continue;
        }
        let _ = write!(
            tabs,
            r##"<a class="config-tab" id="config-tab-{key}" href="#{key}" data-config-tab="{key}" aria-controls="config-panel-{key}"><span>{title}</span><span class="config-tab-count" data-tab-count="{key}"></span></a>"##,
            title = esc(title),
        );
    }
    format!(
        r#"<div class="config-layout"><nav class="config-rail" aria-label="{}"><div class="config-tabs" data-config-tabs>{tabs}</div><p class="config-rail-note hint" data-config-total aria-live="polite"></p></nav><div class="config-panels">"#,
        esc(s.config_tabs_label),
    )
}

fn panel(key: &str, title: &str, build: impl FnOnce(&mut String)) -> String {
    let mut body = String::new();
    build(&mut body);
    format!(
        r#"<section class="config-panel" id="config-panel-{key}" data-config-panel="{key}" aria-labelledby="config-tab-{key}" tabindex="-1"><h2 class="config-panel-title">{}</h2>{body}</section>"#,
        esc(title),
    )
}

fn subhead(out: &mut String, label: &str) {
    let _ = write!(out, "<h3 class=\"section-sub mono\">{}</h3>", esc(label));
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

fn account_fields(
    out: &mut String,
    provider: &str,
    id: &str,
    account: &AccountConfig,
    provider_currency: &str,
    s: &Strings,
) {
    let label = account.label.clone().unwrap_or_default();
    let source = account
        .key
        .as_ref()
        .map(|k| k.describe())
        .unwrap_or_else(|| "credential-store".to_string());
    let currency = account
        .currency
        .clone()
        .unwrap_or_else(|| provider_currency.to_string());
    let account_path: [&str; 4] = ["providers", provider, "accounts", id];
    let remove_json = data_path(&account_path);
    let _ = write!(
        out,
        r#"<div class="account-actions"><button class="btn btn-small danger" type="button" data-account-remove data-path="{}" data-remove-label="{}" data-undo-label="{}">{}</button></div>"#,
        esc(&remove_json),
        esc(s.config_remove_account),
        esc(s.config_remove_undo),
        esc(s.config_remove_account),
    );
    editable_text(
        out,
        &[
            account_path[0],
            account_path[1],
            account_path[2],
            account_path[3],
            "label",
        ],
        "label",
        &label,
        true,
        s,
    );
    field(out, "key source", &source, true, s);
    editable_text(
        out,
        &[
            account_path[0],
            account_path[1],
            account_path[2],
            account_path[3],
            "currency",
        ],
        "currency",
        &currency,
        true,
        s,
    );
    editable_text(
        out,
        &[
            account_path[0],
            account_path[1],
            account_path[2],
            account_path[3],
            "credit_multiplier",
        ],
        "credit_multiplier",
        &account
            .credit_multiplier
            .map(fnum)
            .unwrap_or_else(|| "1".to_string()),
        true,
        s,
    );
    editable_text(
        out,
        &[
            account_path[0],
            account_path[1],
            account_path[2],
            account_path[3],
            "concurrency_limit",
        ],
        "concurrency_limit",
        &account
            .concurrency_limit
            .map(|v| v.to_string())
            .unwrap_or_default(),
        true,
        s,
    );
    editable_text(
        out,
        &[
            account_path[0],
            account_path[1],
            account_path[2],
            account_path[3],
            "rpm_limit",
        ],
        "rpm_limit",
        &account.rpm_limit.map(|v| v.to_string()).unwrap_or_default(),
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
            reload_error: None,
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
    fn an_applied_fragment_carries_the_new_fingerprint() {
        let document = ConfigDocument::parse("[policy]\nsoft_threshold = 0.8\n").expect("doc");
        let edited = document
            .with_draft(&[DraftChange {
                path: vec!["policy".to_string(), "soft_threshold".to_string()],
                value: "0.75".to_string(),
            }])
            .expect("draft");
        let html = applied(
            &config_doc::diff(&document, &edited),
            "abc123",
            false,
            Lang::En,
        );
        assert!(html.contains("data-applied"));
        assert!(html.contains("data-fingerprint=\"abc123\""));
        assert!(html.contains("+soft_threshold = 0.75"));
    }

    #[test]
    fn a_restart_only_field_is_named_rather_than_written() {
        let html = restart_only(&["policy.max_body_bytes".to_string()], Lang::Zh);
        assert!(html.contains("policy.max_body_bytes"));
        assert!(html.contains("需要重启"));
    }

    #[test]
    fn a_section_without_a_live_path_is_refused_by_name() {
        let html = not_live(&["providers.mock.base_url".to_string()], Lang::En);
        assert!(html.contains("providers.mock.base_url"));
        assert!(html.contains("cannot be applied"));
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

#[cfg(test)]
mod tab_layout_tests {
    use super::*;
    use crate::config::sample_toml;

    fn view<'a>(config: &'a Config, raw: &'a str) -> ConfigView<'a> {
        ConfigView {
            config,
            path: "/home/op/.config/lgw/config.toml",
            fingerprint: "fnv64:feedface",
            last_applied: "just now",
            raw: Some(raw),
            reload_error: None,
            lang: Lang::En,
        }
    }

    #[test]
    fn the_form_is_split_into_linked_panels() {
        let config = Config::from_toml_str(sample_toml()).expect("sample");
        let html = render(&view(&config, "listen = \"127.0.0.1:8787\"\n"));
        for key in ["server", "policy", "providers", "routing", "prices", "raw"] {
            assert!(
                html.contains(&format!("data-config-tab=\"{key}\"")),
                "missing tab {key}"
            );
            assert!(
                html.contains(&format!("id=\"config-panel-{key}\"")),
                "missing panel {key}"
            );
        }
        // Routing owns both route and alias rows, in that order.
        let panel = html.find("id=\"config-panel-routing\"").expect("routing");
        let prices = html.find("id=\"config-panel-prices\"").expect("prices");
        let routes = html[panel..prices]
            .find("routes.&lt;model&gt;")
            .expect("routes");
        let aliases = html[panel..prices]
            .find("Provider aliases")
            .expect("aliases");
        assert!(
            routes < aliases,
            "routes come before aliases inside Routing"
        );

        // Progressive enhancement: the server never hides a panel itself.
        assert!(!html.contains("data-tabs-ready"));
        assert!(!html.contains("hidden"));
    }

    #[test]
    fn every_tab_is_named_in_the_page_language() {
        let config = Config::from_toml_str(sample_toml()).expect("sample");
        let mut zh = view(&config, "");
        zh.lang = Lang::Zh;
        let html = render(&zh);
        for title in [
            "服务",
            "策略",
            "Provider 与账号",
            "路由与别名",
            "价格",
            "原始 TOML",
        ] {
            assert!(html.contains(title), "missing {title}");
        }
    }

    #[test]
    fn a_page_without_raw_text_has_no_raw_tab() {
        let config = Config::from_toml_str(sample_toml()).expect("sample");
        let mut plain = view(&config, "");
        plain.raw = None;
        let html = render(&plain);
        assert!(!html.contains("data-config-tab=\"raw\""));
        assert!(!html.contains("config-panel-raw"));
    }
}
