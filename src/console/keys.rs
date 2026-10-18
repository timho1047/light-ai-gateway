//! The Keys page: where every credential comes from, and the three actions that
//! manage the credential store. A value never reaches this module: rows carry a
//! source and a status only.

use super::copy::Lang;
use super::layout::esc;
use crate::keys::KeysReport;
use std::fmt::Write;

pub fn render(report: Option<&KeysReport>, lang: Lang) -> String {
    let s = lang.strings();
    let mut out = String::new();
    let _ = write!(
        out,
        r#"<section class="section">
  <div class="section-head">
    <div class="eyebrow">{}</div>
    <h2 class="section-title">{}</h2>
    <p class="lede">{}</p>
  </div>"#,
        esc(s.keys_eyebrow),
        esc(s.keys_title),
        esc(s.keys_lede),
    );

    let Some(report) = report else {
        out.push_str(&empty(s.keys_empty));
        out.push_str("</section>");
        return out;
    };

    let _ = write!(
        out,
        r#"<p class="hint keys-backend mono">{}: {}{}</p>"#,
        esc(s.keys_backend),
        esc(&report.backend),
        if report.vaulted {
            format!(" · {}", esc(s.keys_vaulted))
        } else {
            String::new()
        },
    );

    if report.rows.is_empty() && report.local_token_chars.is_none() {
        out.push_str(&empty(s.keys_empty));
    } else {
        let _ = write!(
            out,
            r#"<table class="table keys-table"><thead><tr><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th></th></tr></thead><tbody>"#,
            esc(s.keys_col_provider),
            esc(s.keys_col_account),
            esc(s.keys_col_source),
            esc(s.keys_col_status),
        );
        for row in &report.rows {
            let status_class = if row.resolves {
                " is-ok"
            } else {
                " is-missing"
            };
            let _ = write!(
                out,
                r#"<tr><td class="mono">{}</td><td class="mono">{}</td><td class="mono">{}</td><td><span class="key-status{}">{}</span></td><td><div class="row-actions"><button class="btn btn-small" type="button" data-key-test data-provider="{}" data-account="{}">{}</button><button class="btn btn-small danger" type="button" data-key-remove data-provider="{}" data-account="{}" data-confirm="{}">{}</button></div></td></tr>"#,
                esc(&row.provider),
                esc(&row.account),
                esc(&row.source),
                status_class,
                esc(&row.status),
                esc(&row.provider),
                esc(&row.account),
                esc(s.keys_test),
                esc(&row.provider),
                esc(&row.account),
                esc(s.keys_remove_confirm),
                esc(s.keys_remove),
            );
        }
        if let Some(chars) = report.local_token_chars {
            let _ = write!(
                out,
                r#"<tr><td class="mono">{}</td><td class="mono">{}</td><td class="mono">credential-store</td><td><span class="key-status is-ok">{}</span></td><td></td></tr>"#,
                esc(crate::secrets::LOCAL_KEY_PROVIDER),
                esc(s.keys_local_account),
                esc(&s.keys_test_local.replacen("{}", &chars.to_string(), 1)),
            );
        }
        out.push_str("</tbody></table>");
    }

    let _ = write!(
        out,
        r#"<details class="add-account" data-key-add><summary>{}</summary><div class="add-account-grid"><label><span class="field-label mono">{}</span><input type="text" data-key-field="provider" autocomplete="off" spellcheck="false"></label><label><span class="field-label mono">{}</span><input type="text" data-key-field="account" autocomplete="off" spellcheck="false"></label><label><span class="field-label mono">{}</span><input type="password" data-key-field="secret" autocomplete="new-password"></label></div><div class="prices-actions"><button class="btn btn-primary" type="button" data-key-add-submit>{}</button></div></details><div class="prices-result" data-keys-result aria-live="polite"></div><p class="hint prices-note">{}</p></section>"#,
        esc(s.keys_add_title),
        esc(s.keys_provider),
        esc(s.keys_account),
        esc(s.keys_secret),
        esc(s.keys_store),
        esc(s.keys_note),
    );
    out
}

fn empty(message: &str) -> String {
    format!(r#"<div class="empty">{}</div>"#, esc(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::CredentialRow;

    fn report() -> KeysReport {
        KeysReport {
            backend: "keychain".to_string(),
            vaulted: true,
            rows: vec![
                CredentialRow {
                    provider: "mock".to_string(),
                    account: "a1".to_string(),
                    source: "credential-store (vaulted)".to_string(),
                    status: "ok".to_string(),
                    resolves: true,
                },
                CredentialRow {
                    provider: "mock".to_string(),
                    account: "a2".to_string(),
                    source: "inline-literal".to_string(),
                    status: "MISSING (no credential stored)".to_string(),
                    resolves: false,
                },
            ],
            local_token_chars: Some(64),
        }
    }

    #[test]
    fn the_page_lists_sources_and_statuses_but_never_a_value() {
        let html = render(Some(&report()), Lang::En);
        assert!(html.contains("mock"));
        assert!(html.contains("a1"));
        assert!(html.contains("credential-store"));
        assert!(html.contains("inline-literal"));
        assert!(html.contains("MISSING"));
        assert!(html.contains("token stored (64 chars)"));
        assert!(html.contains("data-key-add"));
    }

    #[test]
    fn both_languages_show_the_same_rows() {
        let en = render(Some(&report()), Lang::En);
        let zh = render(Some(&report()), Lang::Zh);
        for field in ["mock", "a1", "a2", "inline-literal", "64"] {
            assert!(en.contains(field), "EN missing {field}");
            assert!(zh.contains(field), "ZH missing {field}");
        }
        assert!(zh.contains("密钥"));
        assert!(zh.contains("来源"));
    }

    #[test]
    fn no_report_is_an_explanation_not_an_empty_table() {
        let html = render(None, Lang::En);
        assert!(html.contains("No credentials configured yet"));
        assert!(!html.contains("<table"));
    }
}
