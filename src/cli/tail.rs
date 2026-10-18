//! `lgw tail`: the most recent requests, newest first.

use super::open_ledger;
use crate::ansi::Palette;
use crate::config::Config;
use crate::domain::{GateClass, RequestStatus, RequestSummary};
use anyhow::Result;
use time::OffsetDateTime;

pub(super) fn cmd_tail(config: &Config, limit: u32, pal: &Palette) -> Result<()> {
    let rows = open_ledger(&config.db_path())?.tail(limit)?;
    if rows.is_empty() {
        println!("no requests recorded yet in {}", config.db_path().display());
        return Ok(());
    }
    print!("{}", render_tail(&rows, pal));
    Ok(())
}

fn render_tail(rows: &[RequestSummary], pal: &Palette) -> String {
    use crate::table::{Align, Table, tokens};

    let mut out = String::new();

    // MODEL is truncated rather than measured: a model name can be sixty
    // characters, and one long row would otherwise stretch every column of the
    // table to fit it. Everything else is measured.
    const MODEL_WIDTH: usize = 28;

    let mut table = Table::new(&[
        ("TIME", Align::Left),
        ("STATUS", Align::Left),
        ("PROVIDER", Align::Left),
        ("ACCOUNT", Align::Left),
        ("MODEL", Align::Left),
        ("IN", Align::Right),
        ("OUT", Align::Right),
        ("CACHED", Align::Right),
        ("TTFT", Align::Right),
        ("TPS", Align::Right),
        ("COVERAGE", Align::Left),
    ]);
    for r in rows {
        let ttft = r
            .ttft_ms
            .map(|ms| format!("{ms}ms"))
            .unwrap_or_else(|| "-".to_string());
        let tps = r
            .tps
            .map(|t| format!("{t:.1}"))
            .unwrap_or_else(|| "-".to_string());
        // ok is the expected case and stays plain; anything else is the reason
        // someone opened this table, so it is the coloured thing.
        let status = match r.status {
            RequestStatus::Ok => status_label(r.status).to_string(),
            _ => pal.red(status_label(r.status)),
        };
        // Spending metered money is not an error, but it is permanent, and this
        // column is where an operator watches it happen.
        let coverage = match r.coverage {
            GateClass::Metered => pal.yellow(coverage_label(r.coverage)),
            _ => coverage_label(r.coverage).to_string(),
        };
        table.push(vec![
            clock(r.ts),
            status,
            r.provider.clone(),
            r.account_id.clone(),
            truncate(&r.model, MODEL_WIDTH),
            tokens(r.input_tokens),
            tokens(r.output_tokens),
            tokens(r.cached_tokens),
            ttft,
            tps,
            coverage,
        ]);
    }
    out.push_str(&table.render(pal));
    out
}

fn clock(t: OffsetDateTime) -> String {
    const HMS: &[time::format_description::FormatItem<'_>] =
        time::macros::format_description!("[hour]:[minute]:[second]");
    t.time()
        .format(HMS)
        .unwrap_or_else(|_| "--:--:--".to_string())
}

fn status_label(s: RequestStatus) -> &'static str {
    match s {
        RequestStatus::Ok => "ok",
        RequestStatus::Error => "error",
        RequestStatus::Aborted => "aborted",
    }
}

fn coverage_label(c: GateClass) -> &'static str {
    match c {
        GateClass::Covered => "covered",
        GateClass::Metered => "metered",
        GateClass::TokenAllowance => "token_allowance",
        GateClass::RateLimit => "rate_limit",
    }
}

fn truncate(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    let head: String = s.chars().take(width.saturating_sub(1)).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_helpers::now;

    /// In the tail, ok and covered are the expected case. Only the request that
    /// went wrong, and the one that spent real money, are coloured.
    #[test]
    fn colour_marks_the_request_that_went_wrong_or_spent_money() {
        let summary = |status: RequestStatus, coverage: GateClass| RequestSummary {
            ts: now(),
            request_id: "r1".into(),
            provider: "xkiro".into(),
            account_id: "xk-1".into(),
            model: "deepseek/deepseek-v3.2".into(),
            input_tokens: 1_200,
            output_tokens: 300,
            cached_tokens: 1_024,
            ttft_ms: Some(850),
            tps: Some(45.25),
            coverage,
            status,
            cost: rust_decimal::Decimal::ZERO,
            currency: "USD".into(),
            retries: 0,
        };
        let bad = render_tail(
            &[summary(RequestStatus::Error, GateClass::Metered)],
            &Palette::on(),
        );
        assert!(bad.contains("\x1b[31merror\x1b[0m"), "{bad:?}");
        assert!(bad.contains("\x1b[33mmetered\x1b[0m"), "{bad:?}");

        let good = render_tail(
            &[summary(RequestStatus::Ok, GateClass::Covered)],
            &Palette::on(),
        );
        assert!(!good.contains("\x1b[31m"), "nothing went wrong: {good:?}");
        assert!(!good.contains("\x1b[33m"), "nothing was metered: {good:?}");
    }

    #[test]
    fn tail_renders_one_row_per_request() {
        let rows = vec![RequestSummary {
            ts: now(),
            request_id: "r1".into(),
            provider: "xkiro".into(),
            account_id: "xk-1".into(),
            model: "deepseek/deepseek-v3.2".into(),
            input_tokens: 1200,
            output_tokens: 300,
            cached_tokens: 1024,
            ttft_ms: Some(850),
            tps: Some(45.25),
            coverage: GateClass::Covered,
            status: RequestStatus::Ok,
            cost: rust_decimal::Decimal::ZERO,
            currency: "USD".into(),
            retries: 0,
        }];
        let text = render_tail(&rows, &Palette::off());
        assert!(text.contains("TIME"));
        assert!(text.contains("COVERAGE"));
        assert!(text.contains("xkiro"));
        assert!(text.contains("850ms"));
        assert!(text.contains("45.2"));
        assert!(text.contains("covered"));
    }
}
