//! The Usage page: the ledger's spend and traffic, read-only.
//!
//! Covered and Metered spend are two colours of money and are never added
//! together. Rows are per grouping key and per currency; a currency with no
//! spend in the window is omitted rather than drawn as zero. Latency comes from
//! the 30-day timing history only, and the page says so whenever the selected
//! range is longer.

use super::copy::{Lang, Strings};
use super::layout::esc;
use crate::domain::GroupBy;
use crate::store::UsageReport;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use time::OffsetDateTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    Day,
    Week,
    Month,
    Quarter,
    All,
}

impl Range {
    pub fn parse(value: Option<&str>) -> Range {
        match value {
            Some("24h") => Range::Day,
            Some("30d") => Range::Month,
            Some("90d") => Range::Quarter,
            Some("all") => Range::All,
            _ => Range::Week,
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Range::Day => "24h",
            Range::Week => "7d",
            Range::Month => "30d",
            Range::Quarter => "90d",
            Range::All => "all",
        }
    }

    pub fn since(self, now: OffsetDateTime) -> OffsetDateTime {
        match self {
            Range::Day => now - time::Duration::hours(24),
            Range::Week => now - time::Duration::days(7),
            Range::Month => now - time::Duration::days(30),
            Range::Quarter => now - time::Duration::days(90),
            Range::All => OffsetDateTime::from_unix_timestamp(0).unwrap_or(now),
        }
    }

    fn label(self, s: &Strings) -> &'static str {
        match self {
            Range::Day => s.usage_range_24h,
            Range::Week => s.usage_range_7d,
            Range::Month => s.usage_range_30d,
            Range::Quarter => s.usage_range_90d,
            Range::All => s.usage_range_all,
        }
    }

    fn all() -> [Range; 5] {
        [
            Range::Day,
            Range::Week,
            Range::Month,
            Range::Quarter,
            Range::All,
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Provider,
    Model,
    Account,
    Day,
}

impl Group {
    pub fn parse(value: Option<&str>) -> Group {
        match value {
            Some("model") => Group::Model,
            Some("account") => Group::Account,
            Some("day") => Group::Day,
            _ => Group::Provider,
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Group::Provider => "provider",
            Group::Model => "model",
            Group::Account => "account",
            Group::Day => "day",
        }
    }

    pub fn group_by(self) -> GroupBy {
        match self {
            Group::Provider => GroupBy::Provider,
            Group::Model => GroupBy::Model,
            Group::Account => GroupBy::Account,
            Group::Day => GroupBy::Day,
        }
    }

    fn label(self, s: &Strings) -> &'static str {
        match self {
            Group::Provider => s.usage_group_provider,
            Group::Model => s.usage_group_model,
            Group::Account => s.usage_group_account,
            Group::Day => s.usage_group_day,
        }
    }

    fn all() -> [Group; 4] {
        [Group::Provider, Group::Model, Group::Account, Group::Day]
    }
}

pub fn render(report: Option<&UsageReport>, range: Range, group: Group, lang: Lang) -> String {
    let s = lang.strings();
    let mut out = String::new();
    let _ = write!(
        out,
        r#"<section class="section">
  <div class="section-head">
    <div class="eyebrow">{}</div>
    <h2 class="section-title">{}</h2>
    <p class="lede">{}</p>
  </div>
{}"#,
        esc(s.usage_eyebrow),
        esc(s.usage_title),
        esc(s.usage_lede),
        controls(range, group, lang, s),
    );

    match report {
        None => out.push_str(&empty(s.usage_no_ledger)),
        Some(report) if report.rows.is_empty() => {
            out.push_str(&empty(s.usage_empty));
            out.push_str(&latency(report, s));
        }
        Some(report) => {
            out.push_str(&summary(report, s));
            out.push_str(&table(report, s));
            out.push_str(&chart(report, s));
            out.push_str(&spend_chart(report, s));
            out.push_str(&latency(report, s));
        }
    }

    let _ = write!(
        out,
        r#"<p class="hint usage-note">{}</p>
</section>"#,
        esc(s.usage_live_note)
    );
    out
}

fn controls(range: Range, group: Group, lang: Lang, s: &Strings) -> String {
    let mut out = String::from(r#"<div class="usage-controls">"#);
    let _ = write!(
        out,
        r#"<div class="control-group"><span class="field-label mono">{}</span>"#,
        esc(s.usage_range)
    );
    for candidate in Range::all() {
        let active = if candidate == range { " is-active" } else { "" };
        let _ = write!(
            out,
            r#"<a class="chip{}" href="/usage?range={}&group={}&lang={}">{}</a>"#,
            active,
            candidate.key(),
            group.key(),
            lang.code(),
            esc(candidate.label(s)),
        );
    }
    out.push_str("</div>");
    let _ = write!(
        out,
        r#"<div class="control-group"><span class="field-label mono">{}</span>"#,
        esc(s.usage_group)
    );
    for candidate in Group::all() {
        let active = if candidate == group { " is-active" } else { "" };
        let _ = write!(
            out,
            r#"<a class="chip{}" href="/usage?range={}&group={}&lang={}">{}</a>"#,
            active,
            range.key(),
            candidate.key(),
            lang.code(),
            esc(candidate.label(s)),
        );
    }
    out.push_str("</div></div>");
    out
}

fn summary(report: &UsageReport, s: &Strings) -> String {
    let covered = currency_totals(
        report
            .rows
            .iter()
            .map(|r| (r.currency.as_str(), r.covered_cost)),
    );
    let metered = currency_totals(
        report
            .rows
            .iter()
            .map(|r| (r.currency.as_str(), r.metered_cost)),
    );
    let requests: u64 = report.rows.iter().map(|r| r.requests).sum();
    let failures: u64 = report.rows.iter().map(|r| r.failed).sum();
    let input: u64 = report.rows.iter().map(|r| r.input_tokens).sum();
    let output: u64 = report.rows.iter().map(|r| r.output_tokens).sum();
    let cached: u64 = report.rows.iter().map(|r| r.cached_tokens).sum();
    let hit = if input == 0 {
        "—".to_string()
    } else {
        format!("{:.0}%", cached as f64 / input as f64 * 100.0)
    };

    let mut out = String::from(r#"<div class="usage-cards">"#);
    out.push_str(&money_card(s.usage_covered, &covered));
    out.push_str(&money_card(s.usage_metered, &metered));
    let _ = write!(
        out,
        r#"<div class="card stat"><div class="eyebrow">{}</div><div class="stat-big">{}</div><div class="stat-sub">{} {}</div></div>"#,
        esc(s.usage_requests),
        requests,
        failures,
        esc(s.usage_failures),
    );
    let _ = write!(
        out,
        r#"<div class="card stat"><div class="eyebrow">{}</div><div class="stat-big">{} / {}</div><div class="stat-sub">{} {}, {} {}</div></div>"#,
        esc(s.usage_tokens),
        crate::table::tokens(input),
        crate::table::tokens(output),
        esc(s.usage_cache_hit),
        hit,
        crate::table::tokens(cached),
        esc(s.usage_col_cache),
    );
    out.push_str("</div>");
    out
}

fn money_card(title: &str, totals: &BTreeMap<String, Decimal>) -> String {
    let mut rows = String::new();
    for (currency, amount) in totals {
        if *amount == Decimal::ZERO {
            continue;
        }
        let _ = write!(
            rows,
            r#"<div class="stat-row"><span class="mono">{}</span><b>{}</b></div>"#,
            esc(currency),
            esc(&crate::table::money(*amount)),
        );
    }
    if rows.is_empty() {
        rows.push_str(r#"<div class="stat-row"><span class="mono">—</span></div>"#);
    }
    format!(
        r#"<div class="card stat"><div class="eyebrow">{}</div>{}</div>"#,
        esc(title),
        rows
    )
}

fn currency_totals<'a>(
    items: impl Iterator<Item = (&'a str, Decimal)>,
) -> BTreeMap<String, Decimal> {
    let mut totals: BTreeMap<String, Decimal> = BTreeMap::new();
    for (currency, amount) in items {
        *totals.entry(currency.to_string()).or_default() += amount;
    }
    if totals.len() > 1 {
        totals.retain(|_, amount| *amount != Decimal::ZERO);
    }
    totals
}

fn table(report: &UsageReport, s: &Strings) -> String {
    let mut keys_per_currency: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for row in &report.rows {
        keys_per_currency
            .entry(row.key.as_str())
            .or_default()
            .insert(row.currency.as_str());
    }

    let mut out = String::new();
    let _ = write!(
        out,
        r#"<div class="table-wrap"><table class="table usage-table"><thead><tr><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th></tr></thead><tbody>"#,
        esc(s.usage_col_key),
        esc(s.usage_col_requests),
        esc(s.usage_col_failures),
        esc(s.usage_col_covered),
        esc(s.usage_col_metered),
        esc(s.usage_col_tokens),
        esc(s.usage_col_cache),
    );
    for row in &report.rows {
        let label = if keys_per_currency
            .get(row.key.as_str())
            .is_some_and(|currencies| currencies.len() > 1)
        {
            format!("{} [{}]", row.key, row.currency)
        } else {
            row.key.clone()
        };
        let covered = if row.covered_requests == 0 {
            "—".to_string()
        } else {
            crate::table::money_with(row.covered_cost, &row.currency)
        };
        let metered = if row.metered_requests == 0 {
            "—".to_string()
        } else {
            crate::table::money_with(row.metered_cost, &row.currency)
        };
        let cache = row
            .cache_hit()
            .map(|hit| format!("{:.0}%", hit * 100.0))
            .unwrap_or_else(|| "—".to_string());
        let _ = write!(
            out,
            r#"<tr><td class="mono">{}</td><td>{}</td><td>{}</td><td class="mono">{}</td><td class="mono">{}</td><td class="mono">{} / {}</td><td>{}</td></tr>"#,
            esc(&label),
            row.requests,
            row.failed,
            esc(&covered),
            esc(&metered),
            crate::table::tokens(row.input_tokens),
            crate::table::tokens(row.output_tokens),
            esc(&cache),
        );
    }
    out.push_str("</tbody></table></div>");
    out
}

fn chart(report: &UsageReport, s: &Strings) -> String {
    let mut per_day: BTreeMap<String, u64> = BTreeMap::new();
    for row in &report.daily {
        *per_day.entry(row.key.clone()).or_default() += row.requests;
    }
    if per_day.is_empty() {
        return String::new();
    }
    let max = per_day.values().copied().max().unwrap_or(0).max(1);
    let total: u64 = per_day.values().sum();
    let (busiest_day, busiest) = per_day
        .iter()
        .max_by_key(|(_, count)| **count)
        .map(|(day, count)| (day.clone(), *count))
        .unwrap_or_default();
    let summary = format!(
        "{}: {} · {}: {} · {}: {} ({})",
        s.usage_chart_days,
        per_day.len(),
        s.usage_chart_total,
        total,
        s.usage_chart_busiest,
        busiest_day,
        busiest
    );
    let mut bars = String::new();
    for (day, count) in &per_day {
        let height = (*count as f64 / max as f64 * 100.0).round().max(2.0);
        let _ = write!(
            bars,
            r#"<div class="bar" style="height:{height:.0}%" title="{day}: {count}"></div>"#
        );
    }
    format!(
        r#"<div class="usage-chart card"><div class="eyebrow">{}</div><div class="bars" role="img" aria-label="{}">{}</div><p class="chart-summary">{}</p><p class="hint">{}</p></div>"#,
        esc(s.usage_chart_title),
        esc(&summary),
        bars,
        esc(&summary),
        esc(s.usage_chart_note),
    )
}

/// Covered and Metered spend over time. Currencies cannot be added together, so
/// the chart shows the currency with the most spend and says which one it is.
fn spend_chart(report: &UsageReport, s: &Strings) -> String {
    // Per currency, per day: [covered, metered].
    let mut coverage: BTreeMap<&str, (Decimal, Decimal)> = BTreeMap::new();
    let mut per_day: BTreeMap<&str, BTreeMap<String, (Decimal, Decimal)>> = BTreeMap::new();
    for row in &report.daily {
        let totals = coverage.entry(row.currency.as_str()).or_default();
        totals.0 += row.covered_cost;
        totals.1 += row.metered_cost;
        let days = per_day.entry(row.currency.as_str()).or_default();
        let day = days.entry(row.key.clone()).or_default();
        day.0 += row.covered_cost;
        day.1 += row.metered_cost;
    }
    let Some((currency, _)) = coverage
        .iter()
        .max_by_key(|(_, (covered, metered))| *covered + *metered)
    else {
        return String::new();
    };
    let days = &per_day[currency];
    let max = days
        .values()
        .map(|(covered, metered)| *covered + *metered)
        .max()
        .unwrap_or(Decimal::ZERO);
    if max <= Decimal::ZERO {
        return String::new();
    }
    let (covered_total, metered_total) = coverage[currency];
    let summary = format!(
        "{}: {} · {}: {} · {}",
        s.usage_covered,
        crate::table::money_with(covered_total, currency),
        s.usage_metered,
        crate::table::money_with(metered_total, currency),
        esc(currency),
    );
    let mut bars = String::new();
    for (day, (covered, metered)) in days {
        let total = *covered + *metered;
        let height = (total / max * Decimal::from(100))
            .to_f64()
            .unwrap_or(0.0)
            .max(2.0);
        let covered_share = if total > Decimal::ZERO {
            (covered / total * Decimal::from(100))
                .to_f64()
                .unwrap_or(0.0)
        } else {
            0.0
        };
        let _ = write!(
            bars,
            r#"<div class="bar spend" style="height:{height:.0}%" title="{day}: {c} covered / {m} metered"><i class="covered" style="height:{covered_share:.0}%"></i><i class="metered"></i></div>"#,
            c = crate::table::money_with(*covered, currency),
            m = crate::table::money_with(*metered, currency),
        );
    }
    format!(
        r#"<div class="usage-chart card"><div class="eyebrow">{}</div><div class="bars" role="img" aria-label="{}">{}</div><p class="chart-summary">{}</p><p class="hint">{}</p></div>"#,
        esc(s.usage_spend_chart_title),
        esc(&summary),
        bars,
        esc(&summary),
        esc(s.usage_spend_note),
    )
}

fn latency(report: &UsageReport, s: &Strings) -> String {
    let p50 = report
        .latency
        .p50_ttft_ms
        .map(ms)
        .unwrap_or_else(|| "—".to_string());
    let p95 = report
        .latency
        .p95_ttft_ms
        .map(ms)
        .unwrap_or_else(|| "—".to_string());
    let samples = report.latency.samples;
    let mut note = s.usage_latency_note.to_string();
    if report.latency_truncated {
        note.push(' ');
        note.push_str(s.usage_latency_capped);
    }
    let values = if samples == 0 {
        format!(
            r#"<div class="stat-sub">{}</div>"#,
            esc(s.usage_latency_empty)
        )
    } else {
        format!(
            r#"<div class="latency-values"><div><span class="field-label mono">{}</span><b>{}</b></div><div><span class="field-label mono">{}</span><b>{}</b></div><div class="stat-sub">{} {}</div></div>"#,
            esc(s.usage_p50),
            esc(&p50),
            esc(s.usage_p95),
            esc(&p95),
            samples,
            esc(s.usage_samples),
        )
    };
    format!(
        r#"<div class="usage-latency card"><div class="eyebrow">{}</div>{}<p class="hint">{}</p></div>"#,
        esc(s.usage_latency),
        values,
        esc(&note),
    )
}

fn ms(value: u64) -> String {
    if value >= 1_000 {
        format!("{:.1}s", value as f64 / 1000.0)
    } else {
        format!("{value}ms")
    }
}

fn empty(message: &str) -> String {
    format!(r#"<div class="empty">{}</div>"#, esc(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::UsageFullRow;

    fn row(key: &str, currency: &str, covered: &str, metered: &str) -> UsageFullRow {
        UsageFullRow {
            key: key.to_string(),
            currency: currency.to_string(),
            requests: 3,
            failed: 1,
            covered_requests: if covered == "0" { 0 } else { 2 },
            metered_requests: if metered == "0" { 0 } else { 1 },
            input_tokens: 1_200,
            output_tokens: 300,
            cached_tokens: 600,
            covered_cost: covered.parse().unwrap(),
            metered_cost: metered.parse().unwrap(),
            other_cost: Decimal::ZERO,
        }
    }

    fn report() -> UsageReport {
        UsageReport {
            rows: vec![
                row("xkiro", "USD", "0.75", "0.50"),
                row("deepseek", "CNY", "0.00", "0.25"),
            ],
            daily: vec![
                UsageFullRow {
                    key: "2026-10-17".to_string(),
                    currency: "USD".to_string(),
                    requests: 2,
                    covered_cost: "0.2".parse().unwrap(),
                    metered_cost: "0.1".parse().unwrap(),
                    ..UsageFullRow::default()
                },
                UsageFullRow {
                    key: "2026-10-18".to_string(),
                    currency: "USD".to_string(),
                    requests: 4,
                    covered_cost: "0.3".parse().unwrap(),
                    metered_cost: "0.2".parse().unwrap(),
                    ..UsageFullRow::default()
                },
            ],
            latency: crate::store::LatencySummary {
                p50_ttft_ms: Some(120),
                p95_ttft_ms: Some(1_400),
                samples: 42,
            },
            latency_since: None,
            latency_truncated: true,
        }
    }

    #[test]
    fn covered_and_metered_are_separate_and_never_added_together() {
        let html = render(Some(&report()), Range::Week, Group::Provider, Lang::En);
        assert!(html.contains("Covered spend"));
        assert!(html.contains("Metered spend"));
        assert!(html.contains("0.75"));
        assert!(html.contains("0.5"));
        assert!(html.contains("0.25"));
        assert!(
            !html.contains("1.25"),
            "covered and metered must not be summed"
        );
        assert!(html.contains("2026-10-18"));
        assert!(html.contains("Daily spend"));
        assert!(html.contains("0.5 USD"), "covered total");
        assert!(html.contains("0.3 USD"), "metered total");
    }

    #[test]
    fn latency_says_why_it_stops_at_thirty_days() {
        let html = render(Some(&report()), Range::Quarter, Group::Model, Lang::En);
        assert!(html.contains("last 30 days"));
        assert!(html.contains("p95 TTFT"));
        assert!(html.contains("1.4s"));
        assert!(html.contains("42"));
    }

    #[test]
    fn both_languages_show_the_same_numbers() {
        let en = render(Some(&report()), Range::Week, Group::Provider, Lang::En);
        let zh = render(Some(&report()), Range::Week, Group::Provider, Lang::Zh);
        for number in ["0.75", "0.5", "0.25", "1.2K", "42"] {
            assert!(en.contains(number), "EN missing {number}");
            assert!(zh.contains(number), "ZH missing {number}");
        }
        assert!(zh.contains("缓冲") || zh.contains("延迟") || zh.contains("Covered 消费"));
    }

    #[test]
    fn an_empty_report_is_omitted_not_rendered_as_zero() {
        let empty = UsageReport::default();
        let html = render(Some(&empty), Range::Day, Group::Provider, Lang::En);
        assert!(html.contains("No requests in this range"));
        assert!(!html.contains("Covered spend"));
        let missing = render(None, Range::Day, Group::Provider, Lang::En);
        assert!(missing.contains("No ledger yet"));
    }
}
