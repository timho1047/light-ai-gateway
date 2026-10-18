//! The Prices page: the configured card against learned rates, with the scoped
//! forget action that puts the card back in force.

use super::copy::Lang;
use super::layout::esc;
use crate::prices::PriceRow;
use std::collections::BTreeMap;
use std::fmt::Write;

pub fn render(table: Option<&crate::meter::PriceTable>, lang: Lang) -> String {
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
        esc(s.prices_eyebrow),
        esc(s.prices_title),
        esc(s.prices_lede),
    );

    let Some(table) = table else {
        out.push_str(&empty(s.prices_no_ledger));
        out.push_str("</section>");
        return out;
    };
    let rows = crate::prices::rows(table);
    if rows.is_empty() {
        out.push_str(&empty(s.prices_empty));
        out.push_str("</section>");
        return out;
    }

    let _ = write!(
        out,
        r#"<div class="prices-actions"><button class="btn btn-primary" type="button" data-forget data-scope="all" data-confirm="{}">{}</button></div><div class="prices-result" data-price-result aria-live="polite"></div>"#,
        esc(s.prices_confirm),
        esc(s.prices_forget_all),
    );
    let _ = write!(
        out,
        r#"<p class="hint prices-note">{}</p>"#,
        esc(s.prices_learned_note)
    );

    let mut groups: BTreeMap<String, BTreeMap<String, Vec<&PriceRow>>> = BTreeMap::new();
    for row in &rows {
        groups
            .entry(row.provider.clone())
            .or_default()
            .entry(row.model.clone())
            .or_default()
            .push(row);
    }

    for (provider, models) in &groups {
        let provider_learned = models.values().flatten().any(|row| row.samples > 0);
        let _ = write!(
            out,
            r#"<div class="prices-group"><div class="prices-heading"><h3 class="mono">{}</h3>"#,
            esc(provider)
        );
        if provider_learned {
            let _ = write!(
                out,
                r#"<button class="btn btn-small danger" type="button" data-forget data-scope="provider" data-provider="{}" data-confirm="{}">{}</button>"#,
                esc(provider),
                esc(s.prices_confirm),
                esc(s.prices_forget_provider),
            );
        }
        out.push_str("</div>");

        for (model, model_rows) in models {
            let model_learned = model_rows.iter().any(|row| row.samples > 0);
            let _ = write!(
                out,
                r#"<div class="prices-heading model"><h4 class="mono">{}</h4>"#,
                esc(model)
            );
            if model_learned {
                let _ = write!(
                    out,
                    r#"<button class="btn btn-small danger" type="button" data-forget data-scope="model" data-provider="{}" data-model="{}" data-confirm="{}">{}</button>"#,
                    esc(provider),
                    esc(model),
                    esc(s.prices_confirm),
                    esc(s.prices_forget_model),
                );
            }
            out.push_str("</div>");

            let _ = write!(
                out,
                r#"<div class="table-wrap"><table class="table prices-table"><thead><tr><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th></tr></thead><tbody>"#,
                esc(s.prices_col_tier),
                esc(s.prices_col_source),
                esc(s.prices_col_mix),
                esc(s.prices_col_configured),
                esc(s.prices_col_learned),
                esc(s.prices_col_expected),
                esc(s.prices_col_actual),
                esc(s.prices_col_drift),
                esc(s.prices_col_samples),
            );
            for row in model_rows {
                let source_class = if row.source == "learned" {
                    " is-learned"
                } else {
                    ""
                };
                let drift_class = if row.significant {
                    " is-significant"
                } else {
                    ""
                };
                let drift = if row.significant {
                    format!("{} !", row.drift)
                } else {
                    row.drift.clone()
                };
                let configured = format!("{} / {}", row.configured_in, row.configured_out);
                let learned = if row.samples == 0 {
                    "—".to_string()
                } else {
                    format!("{} (cache {})", row.learned_rate, row.learned_cache)
                };
                let _ = write!(
                    out,
                    r#"<tr><td class="mono">{}</td><td><span class="price-source{}">{}</span></td><td class="mono">{}</td><td class="mono">{}</td><td class="mono">{}</td><td class="mono">{}</td><td class="mono">{}</td><td class="mono price-drift{}">{}</td><td class="mono">{}</td></tr>"#,
                    esc(&row.tier),
                    source_class,
                    esc(&row.source),
                    esc(&row.mix),
                    esc(&configured),
                    esc(&learned),
                    esc(&row.expected_per_million),
                    esc(&row.actual_per_million),
                    drift_class,
                    esc(&drift),
                    row.samples,
                );
            }
            out.push_str("</tbody></table></div>");
        }
        out.push_str("</div>");
    }

    let _ = write!(
        out,
        r#"<p class="hint prices-note">{}</p>"#,
        esc(s.prices_forget_note)
    );
    out.push_str("</section>");
    out
}

fn empty(message: &str) -> String {
    format!(r#"<div class="empty">{}</div>"#, esc(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::domain::TokenUsage;
    use crate::meter::PriceTable;
    use crate::tier::TierKey;
    use rust_decimal::Decimal;

    fn configured_table() -> PriceTable {
        let config = Config::from_toml_str(
            r#"
[providers.mock]
adapter = "none"
base_url = "http://127.0.0.1:1/v1"
[providers.mock.accounts.a1]
key = "sk-a1"
[prices.mock."mock/model"]
input = 0.3
output = 1.2
[routes]
"mock/model" = "mock"
"#,
        )
        .expect("config");
        let mut table = PriceTable::new();
        table.seed_from_config(&config.prices);
        table
    }

    fn learned_table() -> PriceTable {
        let mut table = configured_table();
        table.observe(
            "mock",
            "mock/model",
            &TierKey::Default,
            Decimal::from(2),
            &TokenUsage {
                prompt_tokens: 1_000_000,
                completion_tokens: 0,
                cached_tokens: 0,
                reasoning_tokens: 0,
                measured: true,
            },
        );
        table
    }

    #[test]
    fn the_page_shows_both_sides_and_the_drift() {
        let table = learned_table();
        let html = render(Some(&table), Lang::En);
        assert!(html.contains("learned"));
        assert!(html.contains("configured"));
        assert!(html.contains("$0.3"));
        assert!(html.contains("$1.2"));
        assert!(html.contains("$2"));
        assert!(html.contains("+"), "drift is signed");
        assert!(html.contains("!"));
        assert!(html.contains("data-forget"));
        assert!(html.contains("Forget model"));
    }

    #[test]
    fn forgetting_is_offered_but_not_forced() {
        let configured = configured_table();
        let html = render(Some(&configured), Lang::En);
        assert!(html.contains("data-forget"));
        assert!(!html.contains("Forget model"), "nothing is learned yet");
        assert!(html.contains("configured"));
    }

    #[test]
    fn empty_states_explain_themselves() {
        let missing = render(None, Lang::En);
        assert!(missing.contains("No ledger yet"));
        let empty = PriceTable::new();
        let none = render(Some(&empty), Lang::En);
        assert!(none.contains("No configured or learned prices yet"));
    }

    #[test]
    fn both_languages_show_the_same_numbers() {
        let table = learned_table();
        let en = render(Some(&table), Lang::En);
        let zh = render(Some(&table), Lang::Zh);
        for number in ["$0.3", "$1.2", "$2"] {
            assert!(en.contains(number), "EN missing {number}");
            assert!(zh.contains(number), "ZH missing {number}");
        }
        assert!(zh.contains("价格"));
        assert!(zh.contains("忘记该模型") || zh.contains("忘记全部学到的价格"));
    }
}
