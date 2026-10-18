//! `lgw prices`: configured prices against the prices learned from real spend.

use super::PricesArgs;
use crate::ansi::Palette;
use crate::config::Config;
use crate::prices::PriceRow;
use crate::store;
use anyhow::{Result, bail};
use rust_decimal::Decimal;

/// Rebuild the price table from the config file plus whatever the ledger has
/// learned. The shared assembly owns the definition.
pub(super) fn load_price_table(config: &Config) -> crate::meter::PriceTable {
    crate::prices::load(config)
}

/// The drift table, shared by the usage and prices commands.
pub(super) fn render_price_rows(rows: &[PriceRow], pal: &Palette) -> String {
    use crate::table::{Align, Table};

    let mut out = String::new();
    let title = "DRIFT   observed spend against the configured schedule, USD per million tokens";
    out.push_str(&pal.header(title));
    out.push_str("\n\n");
    let mut table = Table::new(&[
        ("PROVIDER", Align::Left),
        ("MODEL", Align::Left),
        ("TIER", Align::Left),
        ("SOURCE", Align::Left),
        ("MIX", Align::Left),
        ("EXPECTED", Align::Right),
        ("ACTUAL", Align::Right),
        ("DRIFT", Align::Right),
        ("SAMPLES", Align::Right),
    ]);
    for r in rows {
        table.push(vec![
            r.provider.clone(),
            r.model.clone(),
            r.tier.clone(),
            r.source.clone(),
            r.mix.clone(),
            r.expected_per_million.clone(),
            r.actual_per_million.clone(),
            if r.significant {
                // The whole reading is coloured, not just the marker: this is
                // the row an operator came for.
                pal.red(&format!("{} !", r.drift))
            } else {
                r.drift.clone()
            },
            r.samples.to_string(),
        ]);
    }
    out.push_str(&table.render(pal));
    out.push('\n');
    for line in [
        "EXPECTED is your configured rates applied to the observed usage for that tier.",
        "ACTUAL is what the observed spend works out to over the same usage.",
        "MIX is your input / output / cache-read share of tokens, per tier.",
        "DRIFT is ACTUAL against EXPECTED; ! marks 20% or more, meaning the upstream is",
        "no longer charging what you configured - for that tier in particular.",
    ] {
        out.push_str(&pal.dim(line));
        out.push('\n');
    }
    out
}

/// One line of the rates-in-effect panel.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PricePanelRow {
    /// "peak", "default", "learned".
    label: String,
    /// When it applies, or a note for the learned row.
    when: String,
    /// The tier's multiplier against the default tier.
    ratio: String,
    input: String,
    output: String,
    cache_read: String,
}

/// USD per million tokens, prefixed. Built by pushing the sign so a format
/// string never has to start with a dollar sign followed by a brace.
fn usd(per_token: rust_decimal::Decimal) -> String {
    let v = f64_of(per_token) * 1_000_000.0;
    let mut out = String::new();
    out.push('$');
    out.push_str(&format!("{v:.4}"));
    out
}

/// A rate, already per million tokens, with a dollar sign and a readable scale.
///
/// The old name said "from_million" but nothing was converted: it just printed
/// three decimals whether or not the number had any, so $0.30 read as $0.300 and
/// $15 as $15.000.
fn usd_rate(per_million: f64) -> String {
    match Decimal::try_from(per_million) {
        Ok(rate) => format!("${}", crate::table::money(rate)),
        Err(_) => format!("${per_million:.3}"),
    }
}

/// A human reading of a configured window: "Mon-Fri 01:00-04:00, 06:00-10:00 UTC".
fn window_label(days: &str, hours: &str, tz: Option<&str>) -> String {
    let days = match days.trim().to_ascii_lowercase().as_str() {
        "mon-fri" => "Mon-Fri".to_string(),
        "sat,sun" | "sat-sun" => "Sat-Sun".to_string(),
        "*" => "Every day".to_string(),
        other => other.to_string(),
    };
    let hours = hours
        .split(',')
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(" ");
    let tz = tz.unwrap_or("UTC");
    format!("{days} {hours} {tz}")
}

/// The rates in effect, per configured model, plus the built-in fallback.
///
/// This is the panel that answers "what am I actually being charged, and what
/// would be used for a model I have not priced". The fallback row matters
/// because it is what the hard gate uses for an unconfigured model, and it is
/// deliberately far above real prices.
fn price_panel(
    config: &Config,
    table: &crate::meter::PriceTable,
) -> Vec<(String, Vec<PricePanelRow>)> {
    let mut out = Vec::new();

    for (provider, models) in &config.prices {
        for (model, price) in models {
            let label = format!("{provider}/{model}");
            let tiers = price.tiers();
            let default_input = tiers.first().map(|t| t.input).unwrap_or(0.0);
            let mut rows = Vec::new();

            // Windows first: they are the exception the reader is looking for.
            let parsed = price.schedule(provider, model).ok();
            for (i, s) in price.schedules.iter().enumerate() {
                let tier = &tiers[i + 1];
                let ratio = if default_input > 0.0 {
                    format!("x{:.2}", tier.input / default_input)
                } else {
                    "-".to_string()
                };
                let _ = parsed.as_ref();
                rows.push(PricePanelRow {
                    label: tier.name.clone(),
                    when: window_label(&s.days, &s.hours, s.tz.as_deref()),
                    ratio,
                    input: usd_rate(tier.input),
                    output: usd_rate(tier.output),
                    cache_read: usd_rate(tier.cache_read),
                });
            }

            let d = &tiers[0];
            rows.push(PricePanelRow {
                label: "default".to_string(),
                when: if price.schedules.is_empty() {
                    "no schedule configured".to_string()
                } else {
                    "every other hour".to_string()
                },
                ratio: "x1.00".to_string(),
                input: usd_rate(d.input),
                output: usd_rate(d.output),
                cache_read: usd_rate(d.cache_read),
            });

            // A learned rate for a model with no configured price is the only
            // number that exists for it, so show it in the same shape.
            for (p, m, tier, entry) in table.entries() {
                if p != provider.as_str() || m != model.as_str() {
                    continue;
                }
                let Some(learned) = entry.learned else {
                    continue;
                };
                let name = tier.label();
                rows.push(PricePanelRow {
                    label: format!("learned ({name})"),
                    when: "recovered from observed spend".to_string(),
                    ratio: "-".to_string(),
                    input: usd(learned.input_per_token),
                    output: usd(learned.output_per_token),
                    cache_read: usd(learned.cache_read_per_token),
                });
            }

            out.push((label, rows));
        }
    }

    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The built-in fallback, as one line, for models with no configured price.
fn fallback_row() -> PricePanelRow {
    use crate::meter::{
        CONSERVATIVE_COMPLETION_PER_MILLION as OUT, CONSERVATIVE_PROMPT_PER_MILLION as IN,
    };
    let input = IN as f64;
    let cache_read = input * crate::config::default_cache_read_ratio();
    PricePanelRow {
        label: "fallback".to_string(),
        when: "any model with no configured price".to_string(),
        ratio: "-".to_string(),
        input: usd_rate(input),
        output: usd_rate(OUT as f64),
        cache_read: usd_rate(cache_read),
    }
}

pub(super) fn print_price_panel(config: &Config, table: &crate::meter::PriceTable, pal: &Palette) {
    use crate::table::{Align, Table};

    /// The columns every panel shares, so a reader compares like with like across
    /// models instead of re-reading an inline "in ... out ..." label each time.
    const HEADERS: [(&str, Align); 6] = [
        ("RATE", Align::Left),
        ("WINDOW", Align::Left),
        ("RATIO", Align::Right),
        ("INPUT", Align::Right),
        ("OUTPUT", Align::Right),
        ("CACHE READ", Align::Right),
    ];

    let row_cells = |r: &PricePanelRow| {
        vec![
            r.label.clone(),
            r.when.clone(),
            r.ratio.clone(),
            r.input.clone(),
            r.output.clone(),
            r.cache_read.clone(),
        ]
    };

    // Rows sit under the model they belong to, so they are indented by two and
    // the header goes with them.
    let block = |rows: Vec<Vec<String>>| -> String {
        let mut t = Table::new(&HEADERS);
        for r in rows {
            t.push(r);
        }
        t.render(pal)
            .lines()
            .map(|l| format!("  {l}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let panels = price_panel(config, table);
    println!(
        "{}",
        pal.header("PRICES   rates in effect, USD per million tokens")
    );
    for (label, rows) in &panels {
        println!();
        // The model is a heading: the rows under it are its answer.
        println!("{}", pal.header(label));
        println!("{}", block(rows.iter().map(row_cells).collect()));
    }
    let f = fallback_row();
    println!();
    println!("{}", block(vec![row_cells(&f)]));
    println!(
        "{}",
        pal.dim("  deliberately high - under-estimating is what spends money")
    );
}

fn f64_of(d: Decimal) -> f64 {
    use rust_decimal::prelude::ToPrimitive;
    d.to_f64().unwrap_or(0.0)
}

/// Build the price report. The shared assembly owns the ordering and wording.
pub(super) fn price_rows(table: &crate::meter::PriceTable) -> Vec<PriceRow> {
    crate::prices::rows(table)
}

/// Forget learned prices and say what takes their place.
///
/// The learned table is only evidence about the model while the account is used
/// exclusively through this gateway: the provider meters spend per ACCOUNT, so
/// spend from an editor, another client, or the days before this gateway ran
/// lands in the same delta and gets divided by these tokens. The rate that comes
/// out is too high, the hard gate then over-estimates every request, and the
/// DRIFT row stays flagged -- a permanent warning hiding the real one. Forgetting
/// the row is the way back to the card.
fn forget_learned_prices(config: &Config, scope: &[String], pal: &Palette) -> Result<()> {
    let (provider, model) = match scope {
        [] => (None, None),
        [provider] => (Some(provider.as_str()), None),
        [provider, model] => (Some(provider.as_str()), Some(model.as_str())),
        _ => bail!("--forget takes at most a provider and a model"),
    };

    // Read what is there before deleting, so the command can show what it is
    // giving up and can tell "nothing matched" from "done".
    let table = load_price_table(config);
    let matching: Vec<crate::meter::PersistedPrice> = table
        .learned_rows()
        .into_iter()
        .filter(|row| provider.is_none_or(|p| row.provider == p))
        .filter(|row| model.is_none_or(|m| row.model == m))
        .collect();

    if matching.is_empty() {
        println!("no learned price matches that scope; nothing to forget");
        return Ok(());
    }

    // One RATE column, not an input/output pair: a learned price is a single
    // blended figure applied to both (see meter::PriceTable::observe), so the two
    // columns would always hold the same number and read like a bug.
    let mut rows = crate::table::Table::new(&[
        ("PROVIDER", crate::table::Align::Left),
        ("MODEL", crate::table::Align::Left),
        ("TIER", crate::table::Align::Right),
        ("RATE", crate::table::Align::Right),
        ("CACHE READ", crate::table::Align::Right),
        ("SAMPLES", crate::table::Align::Right),
    ]);
    for row in &matching {
        let per_million =
            |per_token: rust_decimal::Decimal| usd_rate(f64_of(per_token) * 1_000_000.0);
        rows.push(vec![
            row.provider.clone(),
            row.model.clone(),
            row.tier.label().to_string(),
            per_million(row.price.input_per_token),
            per_million(row.price.cache_read_per_token),
            row.price.samples.to_string(),
        ]);
    }
    println!("{}", pal.bold("FORGET   learned prices being dropped"));
    println!("{}", rows.render(pal));
    println!(
        "{}",
        pal.dim(
            "RATE is the blended rate learned from spend over weighted tokens, USD per million"
        )
    );

    let db = config.db_path();
    let handle = store::StoreHandle::spawn(&db)?;
    handle.forget_prices(provider, model);
    handle.flush_blocking()?;

    // What replaces them. A model with no configured price falls to the
    // conservative default, which is worth saying out loud: forgetting a row is
    // not free when nothing was written down instead.
    let mut unplaced: Vec<(String, String)> = Vec::new();
    for row in &matching {
        let configured = config
            .prices
            .get(&row.provider)
            .is_some_and(|models| models.contains_key(&row.model));
        if !configured && !unplaced.contains(&(row.provider.clone(), row.model.clone())) {
            unplaced.push((row.provider.clone(), row.model.clone()));
        }
    }

    println!();
    println!(
        "forgot {} learned row(s); the configured price is in effect again",
        matching.len()
    );
    for (provider, model) in &unplaced {
        println!(
            "  {} {provider}/{model} has no configured price: it is now priced at the \
             conservative default, which is deliberately far above real rates",
            pal.yellow("note:")
        );
    }
    println!(
        "{}",
        pal.dim(
            "note: a running lgw serve holds its own learned table and writes it back on the \
             next probe; restart it so this sticks"
        )
    );
    Ok(())
}

pub(super) fn cmd_prices(config: &Config, args: &PricesArgs, pal: &Palette) -> Result<()> {
    if let Some(scope) = &args.forget {
        return forget_learned_prices(config, scope, pal);
    }

    // Configured prices come from the config file; learned prices come from the
    // ledger, so this works whether or not a gateway is running.
    let table = load_price_table(config);
    print_price_panel(config, &table, pal);
    println!();

    let mut rows = price_rows(&table);
    if args.drift_only {
        rows.retain(|r| r.significant);
    }

    if rows.is_empty() {
        if args.drift_only {
            println!("no model has drifted materially from its configured price");
        } else {
            println!("no prices are configured or learned yet.");
            println!();
            println!(
                "Set a starting price in the config file, and the gateway learns the real one"
            );
            println!("from the spend difference between quota probes:");
            println!();
            println!("  [prices.xkiro.\"z-ai/glm-5.3-flash\"]");
            println!("  input = 0.30");
            println!("  output = 1.20");
            println!("  \"cache read\" = 0.03   # optional; defaults to input / 10");
        }
        return Ok(());
    }

    // One renderer for both commands, so they cannot disagree.
    print!("{}", render_price_rows(&rows, pal));

    let drifted = rows.iter().filter(|r| r.significant).count();
    if drifted > 0 {
        println!();
        println!("{drifted} model(s) have drifted; investigate before trusting the hard gate.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::{Cli, Command};
    use super::*;
    use crate::cli::test_helpers::dec;

    #[test]
    fn price_rows_show_configured_against_learned() {
        let rows = price_rows(&priced_table());
        assert_eq!(rows.len(), 2);

        let glm = rows
            .iter()
            .find(|r| r.model == "z-ai/glm-5.3-flash")
            .unwrap();
        assert_eq!(glm.provider, "xkiro");
        assert_eq!(
            glm.source, "learned",
            "an observed price outranks the expectation"
        );
        // 1M input tokens, nothing else: the mix says so.
        assert_eq!(glm.mix, "100/0/0%");
        // Configured $0.30/M input applied to that mix, against the $0.90/M the
        // observed spend actually works out to.
        // Trailing zeros are not precision: $0.3000 and $0.3 are the same money,
        // and the column is right-aligned so the decimal points still line up.
        assert_eq!(glm.expected_per_million, "$0.3");
        assert_eq!(glm.actual_per_million, "$0.9");
        assert_eq!(glm.drift, "+200.0%");
        assert!(glm.significant, "a tripling must be flagged");
        assert_eq!(glm.samples, 1);

        // No configured price: the actual figure is still useful, but there is
        // nothing to compare it against, so no drift claim is made.
        let ds = rows
            .iter()
            .find(|r| r.model == "deepseek/deepseek-v4.1-flash")
            .unwrap();
        assert_eq!(ds.expected_per_million, "-");
        assert_ne!(
            ds.actual_per_million, "-",
            "the observed cost is still shown"
        );
        assert_eq!(ds.drift, "-");
        assert!(!ds.significant);
    }

    #[test]
    fn drifted_rows_sort_first_so_they_are_seen() {
        let rows = price_rows(&priced_table());
        assert!(rows[0].significant, "the drifted model must come first");
    }

    #[test]
    fn a_configured_price_with_no_observation_is_shown_as_configured() {
        use crate::config::ModelPriceConfig;
        use std::collections::BTreeMap;
        let mut table = crate::meter::PriceTable::new();
        let mut cfg: BTreeMap<String, BTreeMap<String, ModelPriceConfig>> = BTreeMap::new();
        cfg.entry("xkiro".to_string()).or_default().insert(
            "m".to_string(),
            ModelPriceConfig {
                input: 1.0,
                output: 4.0,
                cache_read: None,
                schedules: Vec::new(),
            },
        );
        table.seed_from_config(&cfg);

        let rows = price_rows(&table);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source, "configured");
        // Nothing observed yet, so there is no mix and no per-million figure:
        // a dash rather than a claim computed from nothing.
        assert_eq!(rows[0].mix, "-");
        assert_eq!(rows[0].expected_per_million, "-");
        assert_eq!(rows[0].actual_per_million, "-");
        assert_eq!(rows[0].drift, "-");
        assert_eq!(rows[0].samples, 0);
    }

    /// The flagged row is the reason the drift table exists, so it is the row
    /// that gets the colour -- and only when it is flagged.
    #[test]
    fn colour_follows_the_drift_flag() {
        let row = |drift: &str, significant: bool| PriceRow {
            provider: "commandcode".into(),
            model: "deepseek/deepseek-v4.1-flash".into(),
            tier: "default".into(),
            source: "learned".into(),
            mix: "1/1/98%".into(),
            expected_per_million: "$0.008".into(),
            actual_per_million: "$0.0169".into(),
            drift: drift.to_string(),
            significant,
            samples: 24,
            configured_in: "-".into(),
            configured_out: "-".into(),
            configured_cache: "-".into(),
            learned_rate: "-".into(),
            learned_cache: "-".into(),
        };
        let rows = vec![row("+110.5%", true), row("-", false)];
        let out = render_price_rows(&rows, &Palette::on());

        assert!(out.contains("\x1b[31m+110.5% !\x1b[0m"), "{out:?}");
        assert!(
            !out.contains("\x1b[31m-\x1b[0m"),
            "a row that did not drift is not coloured: {out:?}"
        );
        assert!(
            out.starts_with("\x1b[1;36mDRIFT"),
            "the title is a heading: {out:?}"
        );
    }

    /// `--forget` takes zero, one or two values and nothing else.
    ///
    /// The scope is positional precisely so that "forget this model" cannot be
    /// confused with "forget everything", and clap accepting a third value would
    /// be silently ignored by the slow path.
    #[test]
    fn forget_parses_its_scope() {
        use clap::Parser;

        let parse = |args: &[&str]| {
            Cli::try_parse_from(std::iter::once("lgw").chain(args.iter().copied()))
                .map(|cli| match cli.command {
                    Command::Prices(p) => p.forget,
                    _ => panic!("expected the prices command"),
                })
                .map_err(|e| e.to_string())
        };

        assert_eq!(parse(&["prices"]).unwrap(), None);
        assert_eq!(parse(&["prices", "--forget"]).unwrap(), Some(Vec::new()));
        assert_eq!(
            parse(&["prices", "--forget", "xkiro"]).unwrap(),
            Some(vec!["xkiro".to_string()])
        );
        assert_eq!(
            parse(&["prices", "--forget", "xkiro", "z-ai/glm-5.3"]).unwrap(),
            Some(vec!["xkiro".to_string(), "z-ai/glm-5.3".to_string()])
        );
        assert!(
            parse(&["prices", "--forget", "a", "b", "c"]).is_err(),
            "a third value is not a scope"
        );
    }

    fn priced_table() -> crate::meter::PriceTable {
        use crate::config::ModelPriceConfig;
        use std::collections::BTreeMap;
        let mut table = crate::meter::PriceTable::new();
        let mut cfg: BTreeMap<String, BTreeMap<String, ModelPriceConfig>> = BTreeMap::new();
        cfg.entry("xkiro".to_string()).or_default().insert(
            "z-ai/glm-5.3-flash".to_string(),
            ModelPriceConfig {
                input: 0.30,
                output: 1.20,
                cache_read: None,
                schedules: Vec::new(),
            },
        );
        table.seed_from_config(&cfg);
        // Reality is 3x the configured prompt price: 0.90/M over 1M tokens.
        table.observe(
            "xkiro",
            "z-ai/glm-5.3-flash",
            &crate::tier::TierKey::Default,
            dec("0.9"),
            &crate::domain::TokenUsage {
                prompt_tokens: 1_000_000,
                ..Default::default()
            },
        );
        // A second model with no configured price: nothing to compare against.
        table.observe(
            "commandcode",
            "deepseek/deepseek-v4.1-flash",
            &crate::tier::TierKey::Default,
            dec("0.05"),
            &crate::domain::TokenUsage {
                prompt_tokens: 1_000,
                completion_tokens: 25,
                ..Default::default()
            },
        );
        table
    }
}
