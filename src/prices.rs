//! The price report: configured prices against what spend actually taught the
//! ledger. Shared by the terminal report and the console page so the two cannot
//! disagree about a rate.

use crate::config::Config;
use crate::store::{self, Ledger};
use rust_decimal::Decimal;

/// One line of the price report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriceRow {
    pub provider: String,
    pub model: String,
    /// The window's own name, or "default" for the model's plain price.
    pub tier: String,
    /// "learned" or "configured": which rate is in force.
    pub source: String,
    /// Input / output / cache-read share of the tokens actually used.
    pub mix: String,
    /// Configured prices applied to that mix, per million tokens.
    pub expected_per_million: String,
    /// Observed spend over that same mix, per million tokens.
    pub actual_per_million: String,
    /// Cost drift against the configured price, or a dash when not comparable.
    pub drift: String,
    pub significant: bool,
    pub samples: u64,
    /// The configured rates themselves, for a page that shows both sides.
    pub configured_in: String,
    pub configured_out: String,
    pub configured_cache: String,
    pub learned_rate: String,
    pub learned_cache: String,
}

/// Rebuild the table from the config file plus whatever the ledger has learned.
///
/// Every command and page that reports on prices uses this, so they cannot
/// drift apart on where a rate came from.
pub fn load(config: &Config) -> crate::meter::PriceTable {
    let mut table = crate::meter::PriceTable::new();
    table.seed_from_config(&config.prices);
    let db = config.db_path();
    if store::db_exists(&db)
        && let Ok(ledger) = Ledger::open(&db)
        && let Ok(rows) = ledger.prices()
    {
        table.load_learned(rows);
    }
    table
}

/// Build the price report. Pure, so it can be tested without a terminal.
pub fn rows(table: &crate::meter::PriceTable) -> Vec<PriceRow> {
    let mut rows: Vec<PriceRow> = table
        .entries()
        .filter_map(|(provider, model, tier, entry)| {
            let source = entry.source()?.label().to_string();

            // The mix is what makes a per-million figure mean anything: the same
            // prices produce very different per-million costs on different mixes.
            let mix = match entry.mix_percentages() {
                Some((i, o, c)) => format!("{i:.0}/{o:.0}/{c:.0}%"),
                None => "-".to_string(),
            };
            let drift = table.drift(provider, model, tier);
            let expected_per_million = drift
                .as_ref()
                .map(|d| format!("${}", crate::table::money(d.expected_per_million)))
                .unwrap_or_else(|| "-".to_string());
            let actual_per_million = table
                .actual_per_million(provider, model, tier)
                .map(|(v, _)| format!("${}", crate::table::money(v)))
                .unwrap_or_else(|| "-".to_string());

            let samples = entry.learned.map(|p| p.samples).unwrap_or(0);
            let usd =
                |per_token: Decimal| format!("${}", crate::table::money(per_million(per_token)));
            let configured_in = entry
                .configured
                .map(|p| usd(p.input_per_token))
                .unwrap_or_else(|| "-".to_string());
            let configured_out = entry
                .configured
                .map(|p| usd(p.output_per_token))
                .unwrap_or_else(|| "-".to_string());
            let configured_cache = entry
                .configured
                .map(|p| usd(p.cache_read_per_token))
                .unwrap_or_else(|| "-".to_string());
            let learned_rate = entry
                .learned
                .map(|p| usd(p.input_per_token))
                .unwrap_or_else(|| "-".to_string());
            let learned_cache = entry
                .learned
                .map(|p| usd(p.cache_read_per_token))
                .unwrap_or_else(|| "-".to_string());

            Some(PriceRow {
                provider: provider.to_string(),
                model: model.to_string(),
                tier: tier.label().to_string(),
                source,
                mix,
                expected_per_million,
                actual_per_million,
                drift: match drift {
                    Some(d) => format!("{:+.1}%", d.ratio * 100.0),
                    None => "-".to_string(),
                },
                significant: drift.is_some_and(|d| d.significant),
                samples,
                configured_in,
                configured_out,
                configured_cache,
                learned_rate,
                learned_cache,
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        b.significant
            .cmp(&a.significant)
            .then_with(|| a.provider.cmp(&b.provider))
            .then_with(|| a.model.cmp(&b.model))
    });
    rows
}

fn per_million(per_token: Decimal) -> Decimal {
    per_token * Decimal::from(1_000_000u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config::from_toml_str(
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
        .expect("config")
    }

    #[test]
    fn a_learned_rate_outranks_the_configured_one_and_the_page_can_show_both() {
        let config = config();
        let mut table = crate::meter::PriceTable::new();
        table.seed_from_config(&config.prices);
        let learned = crate::meter::ModelPrice {
            input_per_token: "0.0000009".parse().unwrap(),
            output_per_token: "0.0000009".parse().unwrap(),
            cache_read_per_token: "0.00000009".parse().unwrap(),
            samples: 4,
        };
        table.set(
            "mock",
            "mock/model",
            &crate::tier::TierKey::Default,
            learned,
        );
        let rows = rows(&table);
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.source, "learned");
        assert_eq!(row.samples, 4);
        assert_eq!(row.configured_in, "$0.3");
        assert_eq!(row.configured_out, "$1.2");
        assert_eq!(row.learned_rate, "$0.9");
    }
}
