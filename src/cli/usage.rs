//! `lgw usage`: aggregated spend and token usage.

use super::UsageArgs;
use super::open_ledger;
use super::prices::{load_price_table, price_rows, print_price_panel, render_price_rows};
use crate::ansi::Palette;
use crate::config::Config;
use crate::domain::{GroupBy, UsageRow};
use anyhow::{Result, anyhow, bail};
use rust_decimal::Decimal;
use std::collections::BTreeMap;
use time::OffsetDateTime;

pub(super) fn cmd_usage(config: &Config, args: &UsageArgs, pal: &Palette) -> Result<()> {
    let now = OffsetDateTime::now_utc();
    let since = parse_since(&args.since, now)?;
    let dims = parse_group_by(&args.group_by)?;
    let rows = open_ledger(&config.db_path())?.usage_by(since, &dims)?;

    if rows.is_empty() {
        println!(
            "no requests since {} in {}",
            since,
            config.db_path().display()
        );
        return Ok(());
    }
    print!("{}", render_usage(&rows, &dims, since, pal));

    // Rates in effect, then what each model is actually costing against them.
    // The panel comes first because the drift table is unreadable without it.
    let table = load_price_table(config);
    println!();
    print_price_panel(config, &table, pal);

    let drift_rows = price_rows(&table);
    if !drift_rows.is_empty() {
        println!();
        print!("{}", render_price_rows(&drift_rows, pal));
    }
    Ok(())
}

fn parse_group_by(spec: &str) -> Result<Vec<GroupBy>> {
    let mut out: Vec<GroupBy> = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let dim = GroupBy::parse(part).ok_or_else(|| {
            anyhow!("unknown --group-by value {part:?}; expected provider, model, account, or day")
        })?;
        if !out.contains(&dim) {
            out.push(dim);
        }
    }
    if out.is_empty() {
        out.push(GroupBy::Provider);
    }
    Ok(out)
}

fn parse_since(spec: &str, now: OffsetDateTime) -> Result<OffsetDateTime> {
    let spec = spec.trim();
    if spec.is_empty() {
        bail!("--since must not be empty");
    }

    if let Some(offset) = parse_relative(spec) {
        return Ok(now - offset);
    }
    if let Ok(t) = OffsetDateTime::parse(spec, &time::format_description::well_known::Rfc3339) {
        return Ok(t);
    }
    const DATE: &[time::format_description::FormatItem<'_>] =
        time::macros::format_description!("[year]-[month]-[day]");
    if let Ok(date) = time::Date::parse(spec, DATE) {
        return Ok(date.midnight().assume_utc());
    }

    bail!(
        "could not parse --since {spec:?}; use 7d, 24h, 90m, 2w, an RFC3339 timestamp, or YYYY-MM-DD"
    )
}

/// Parse "7d" / "24h" / "90m" / "45s" / "2w" into a duration.
fn parse_relative(spec: &str) -> Option<time::Duration> {
    // Split on the last CHARACTER, not the last byte: split_at panics when the
    // index lands inside a multi-byte character, so "--since 1天" crashed the
    // whole CLI with a char-boundary panic instead of the tidy parse error five
    // lines up. A Chinese or accented unit is an easy thing for a human to type.
    let (index, unit) = spec.char_indices().next_back()?;
    let digits = &spec[..index];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: i64 = digits.parse().ok()?;
    let secs = match unit {
        's' => n,
        'm' => n.checked_mul(60)?,
        'h' => n.checked_mul(3_600)?,
        'd' => n.checked_mul(86_400)?,
        'w' => n.checked_mul(604_800)?,
        _ => return None,
    };
    Some(time::Duration::seconds(secs))
}

/// Cached tokens with the hit rate beside them.
///
/// The raw count says how much was read from cache; the rate says whether the
/// cache is working at all, and it is the one to watch when a cost looks wrong
/// (a cached token is billed at a fraction of a fresh one).
fn cached_cell(row: &UsageRow) -> String {
    let tokens = crate::table::tokens(row.cached_tokens);
    if row.input_tokens == 0 {
        return tokens;
    }
    let hit = row.cached_tokens as f64 / row.input_tokens as f64;
    format!("{tokens} ({:.0}%)", hit * 100.0)
}

fn render_usage(
    rows: &[UsageRow],
    dims: &[GroupBy],
    since: OffsetDateTime,
    pal: &Palette,
) -> String {
    let header = dims
        .iter()
        .map(|d| d.column_header())
        .collect::<Vec<_>>()
        .join("+");

    // One key can legitimately appear once per currency, and the two must not
    // be added together. Suffix the key in exactly that case
    // so the two rows do not look like an accidental duplicate.
    let mut keys_per_currency: BTreeMap<&str, std::collections::BTreeSet<&str>> = BTreeMap::new();
    for r in rows {
        keys_per_currency
            .entry(r.key.as_str())
            .or_default()
            .insert(r.currency.as_str());
    }
    let display_key = |r: &UsageRow| -> String {
        if keys_per_currency
            .get(r.key.as_str())
            .is_some_and(|c| c.len() > 1)
        {
            format!("{} [{}]", r.key, r.currency)
        } else {
            r.key.clone()
        }
    };

    let mut out = String::new();
    // The title is what a reader scans for when several reports are on screen
    // at once, so it is the bold one.
    let title = format!(
        "USAGE since {} (grouped by {})",
        stamp(since),
        dims.iter()
            .map(|d| d.column_header().to_ascii_lowercase())
            .collect::<Vec<_>>()
            .join(", ")
    );
    out.push_str(&pal.header(&title));
    out.push_str("\n\n");

    // Widths are measured, and the counts are scaled, so a nine-digit token count
    // cannot push the cost column out of line the way it used to.
    let mut table = crate::table::Table::new(&[
        (&header, crate::table::Align::Left),
        ("REQUESTS", crate::table::Align::Right),
        ("IN", crate::table::Align::Right),
        ("OUT", crate::table::Align::Right),
        ("CACHED", crate::table::Align::Right),
        ("COST", crate::table::Align::Right),
    ]);
    for r in rows {
        table.push(vec![
            display_key(r),
            r.requests.to_string(),
            crate::table::tokens(r.input_tokens),
            crate::table::tokens(r.output_tokens),
            cached_cell(r),
            crate::table::money_with(r.cost, &r.currency),
        ]);
    }
    out.push_str(&table.render(pal));

    // Totals are per currency. Adding CNY to USD would be meaningless.
    let mut totals: BTreeMap<String, (u64, u64, u64, u64, Decimal)> = BTreeMap::new();
    for r in rows {
        let t = totals.entry(r.currency.clone()).or_default();
        t.0 += r.requests;
        t.1 += r.input_tokens;
        t.2 += r.output_tokens;
        t.3 += r.cached_tokens;
        t.4 += r.cost;
    }
    if rows.len() > 1 && !totals.is_empty() {
        out.push('\n');
        for (currency, (requests, input, output, cached, cost)) in totals {
            let total = format!(
                "TOTAL {currency}: {requests} requests, in {}, out {}, cached {}, cost {} {currency}",
                crate::table::tokens(input),
                crate::table::tokens(output),
                crate::table::tokens(cached),
                crate::table::money(cost),
            );
            out.push_str(&pal.header(&total));
            out.push('\n');
        }
    }
    out
}

/// UTC stamp for headers: "2026-09-04 14:18:13Z".
fn stamp(t: OffsetDateTime) -> String {
    const STAMP: &[time::format_description::FormatItem<'_>] =
        time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second]Z");
    t.format(STAMP).unwrap_or_else(|_| t.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_helpers::{dec, now};

    #[test]
    fn group_by_parses_a_comma_separated_list() {
        assert_eq!(parse_group_by("provider").unwrap(), vec![GroupBy::Provider]);
        assert_eq!(
            parse_group_by("provider,model,account").unwrap(),
            vec![GroupBy::Provider, GroupBy::Model, GroupBy::Account]
        );
        // Whitespace and repeated dimensions are tolerated.
        assert_eq!(
            parse_group_by(" model , model ").unwrap(),
            vec![GroupBy::Model]
        );
        assert_eq!(parse_group_by("").unwrap(), vec![GroupBy::Provider]);
        assert!(parse_group_by("provider,session").is_err());
    }

    #[test]
    fn since_accepts_durations_timestamps_and_dates() {
        let t = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        assert_eq!(parse_since("7d", t).unwrap(), t - time::Duration::days(7));
        assert_eq!(
            parse_since("24h", t).unwrap(),
            t - time::Duration::hours(24)
        );
        assert_eq!(
            parse_since("90m", t).unwrap(),
            t - time::Duration::minutes(90)
        );
        assert_eq!(parse_since("2w", t).unwrap(), t - time::Duration::weeks(2));

        assert_eq!(
            parse_since("2023-11-14T22:13:20Z", t).unwrap(),
            t,
            "an RFC3339 timestamp is used as-is"
        );
        assert_eq!(
            parse_since("2023-11-14", t).unwrap().unix_timestamp(),
            1_699_920_000
        );

        assert!(parse_since("", t).is_err());
        assert!(parse_since("soon", t).is_err());
        assert!(parse_since("7", t).is_err(), "a bare number has no unit");
    }

    #[test]
    fn usage_totals_are_kept_per_currency() {
        let rows = vec![
            UsageRow {
                key: "xkiro".into(),
                requests: 2,
                input_tokens: 200,
                output_tokens: 40,
                cached_tokens: 80,
                cost: dec("0.75"),
                currency: "USD".into(),
            },
            UsageRow {
                key: "deepseek".into(),
                requests: 1,
                input_tokens: 10,
                output_tokens: 5,
                cached_tokens: 0,
                cost: dec("0.5"),
                currency: "CNY".into(),
            },
        ];
        let text = render_usage(&rows, &[GroupBy::Provider], now(), &Palette::off());
        assert!(text.contains("USAGE since"));
        assert!(text.contains("PROVIDER"));
        assert!(text.contains("0.75 USD"));
        assert!(text.contains("0.5 CNY"));
        assert!(text.contains("TOTAL USD: 2 requests"));
        assert!(text.contains("TOTAL CNY: 1 requests"));
        assert!(
            !text.contains("1.25"),
            "USD and CNY must never be added together: {text}"
        );
        assert!(text.contains("2023-11-14 22:13:20Z"), "got: {text}");
    }

    #[test]
    fn a_key_billed_in_two_currencies_is_disambiguated() {
        let rows = vec![
            UsageRow {
                key: "m".into(),
                requests: 2,
                input_tokens: 2_100,
                output_tokens: 420,
                cached_tokens: 1_024,
                cost: dec("0.0053"),
                currency: "USD".into(),
            },
            UsageRow {
                key: "m".into(),
                requests: 1,
                input_tokens: 10,
                output_tokens: 5,
                cached_tokens: 0,
                cost: dec("0.000356"),
                currency: "CNY".into(),
            },
        ];
        let text = render_usage(&rows, &[GroupBy::Model], now(), &Palette::off());
        assert!(text.contains("m [USD]"), "got: {text}");
        assert!(text.contains("m [CNY]"), "got: {text}");

        // A key seen in one currency only keeps its bare name.
        let single = vec![UsageRow {
            key: "m".into(),
            requests: 1,
            input_tokens: 1,
            output_tokens: 1,
            cached_tokens: 0,
            cost: dec("0.1"),
            currency: "USD".into(),
        }];
        assert!(
            !render_usage(&single, &[GroupBy::Model], now(), &Palette::off()).contains("m [USD]")
        );
    }

    /// A multi-byte unit is a parse error, not a panic.
    ///
    /// The parser split at spec.len() - 1, a BYTE index, so "--since 1天" aborted
    /// the process with "byte index 3 is not a char boundary" instead of the tidy
    /// message the function already had. A Chinese unit is an easy thing to type.
    #[test]
    fn a_non_ascii_since_unit_is_refused_instead_of_panicking() {
        assert!(parse_relative("1天").is_none());
        assert!(parse_relative("7天前").is_none());
        assert!(parse_relative("é").is_none());
        assert!(parse_relative("").is_none());
        // And the ASCII spellings still work.
        assert_eq!(parse_relative("7d"), Some(time::Duration::days(7)));
        assert_eq!(parse_relative("90m"), Some(time::Duration::minutes(90)));
    }
}
