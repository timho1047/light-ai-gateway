//! The Requests page: recent requests with their attempt-level detail.
//!
//! The request assembly is the same one the terminal tail reads. Tickets that
//! needed a retry are expanded to one row per attempt so a failure that was
//! recovered from is still visible; request-level cost and tokens are printed
//! once, on the final attempt, rather than repeated as if each attempt cost the
//! whole request.

use super::copy::{Lang, Strings};
use super::layout::esc;
use crate::domain::{GateClass, RequestStatus};
use crate::store::{AttemptDetail, RequestHistory};
use std::fmt::Write;

pub const LIMITS: [u32; 4] = [25, 50, 100, 250];

#[derive(Debug, Clone, Copy)]
pub struct Filters {
    pub limit: u32,
    pub errors_only: bool,
    pub coverage: Option<GateClass>,
}

impl Filters {
    pub fn sanitise(limit: Option<u32>, errors_only: bool, coverage: Option<GateClass>) -> Filters {
        let limit = limit.filter(|limit| LIMITS.contains(limit)).unwrap_or(50);
        Filters {
            limit,
            errors_only,
            coverage,
        }
    }

    fn href(
        self,
        limit: u32,
        errors_only: bool,
        coverage: Option<GateClass>,
        lang: Lang,
    ) -> String {
        let mut url = format!("/requests?limit={limit}&lang={}", lang.code());
        if errors_only {
            url.push_str("&errors=1");
        }
        if let Some(coverage) = coverage {
            let _ = write!(url, "&coverage={}", coverage_key(coverage));
        }
        url
    }
}

pub fn render(history: Option<&[RequestHistory]>, filters: Filters, lang: Lang) -> String {
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
        esc(s.requests_eyebrow),
        esc(s.requests_title),
        esc(s.requests_lede),
        controls(filters, lang, s),
    );

    match history {
        None => out.push_str(&empty(s.requests_no_ledger)),
        Some([]) => out.push_str(&empty(s.requests_empty)),
        Some(rows) => {
            out.push_str(&table(rows, s));
            let _ = write!(
                out,
                r#"<p class="hint requests-note">{}</p>"#,
                esc(s.requests_note)
            );
        }
    }
    out.push_str("</section>");
    out
}

fn controls(filters: Filters, lang: Lang, s: &Strings) -> String {
    let mut out = String::from(r#"<div class="usage-controls">"#);
    let _ = write!(
        out,
        r#"<div class="control-group"><span class="field-label mono">{}</span>"#,
        esc(s.requests_limit)
    );
    for limit in LIMITS {
        let active = if limit == filters.limit {
            " is-active"
        } else {
            ""
        };
        let _ = write!(
            out,
            r#"<a class="chip{}" href="{}">{}</a>"#,
            active,
            esc(&filters.href(limit, filters.errors_only, filters.coverage, lang)),
            limit,
        );
    }
    out.push_str("</div>");

    let _ = write!(
        out,
        r#"<div class="control-group"><span class="field-label mono">{}</span>"#,
        esc(s.requests_status_filter)
    );
    for errors_only in [false, true] {
        let active = if errors_only == filters.errors_only {
            " is-active"
        } else {
            ""
        };
        let label = if errors_only {
            s.requests_errors_only
        } else {
            s.requests_all
        };
        let _ = write!(
            out,
            r#"<a class="chip{}" href="{}">{}</a>"#,
            active,
            esc(&filters.href(filters.limit, errors_only, filters.coverage, lang)),
            esc(label),
        );
    }
    out.push_str("</div>");

    let _ = write!(
        out,
        r#"<div class="control-group"><span class="field-label mono">{}</span>"#,
        esc(s.requests_coverage_filter)
    );
    for coverage in [None, Some(GateClass::Covered), Some(GateClass::Metered)] {
        let active = if coverage == filters.coverage {
            " is-active"
        } else {
            ""
        };
        let label = match coverage {
            None => s.requests_all,
            Some(GateClass::Covered) => "covered",
            Some(GateClass::Metered) => "metered",
            Some(other) => coverage_key(other),
        };
        let _ = write!(
            out,
            r#"<a class="chip{}" href="{}">{}</a>"#,
            active,
            esc(&filters.href(filters.limit, filters.errors_only, coverage, lang)),
            esc(label),
        );
    }
    out.push_str("</div></div>");
    out
}

fn table(rows: &[RequestHistory], s: &Strings) -> String {
    let mut out = String::new();
    let _ = write!(
        out,
        r#"<table class="table requests-table"><thead><tr><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th></tr></thead><tbody>"#,
        esc(s.requests_col_time),
        esc(s.requests_col_status),
        esc(s.requests_col_attempt),
        esc(s.requests_col_provider),
        esc(s.requests_col_account),
        esc(s.requests_col_model),
        esc(s.requests_col_tokens),
        esc(s.requests_col_ttft),
        esc(s.requests_col_tps),
        esc(s.requests_col_coverage),
        esc(s.requests_col_cost),
    );
    for history in rows {
        let total = history.attempts_total();
        if history.attempts.is_empty() {
            row(&mut out, history, None, total, true, s);
        } else {
            let count = history.attempts.len();
            for (index, attempt) in history.attempts.iter().enumerate() {
                row(
                    &mut out,
                    history,
                    Some(attempt),
                    total.max(count as u32),
                    index + 1 == count,
                    s,
                );
            }
        }
    }
    out.push_str("</tbody></table>");
    out
}

fn row(
    out: &mut String,
    history: &RequestHistory,
    attempt: Option<&AttemptDetail>,
    total: u32,
    is_last: bool,
    s: &Strings,
) {
    let request = &history.request;
    let at = attempt.map(|a| a.started_at).unwrap_or(request.ts);
    let provider = attempt
        .map(|a| a.provider.as_str())
        .unwrap_or(&request.provider);
    let account = attempt
        .map(|a| a.account_id.as_str())
        .unwrap_or(&request.account_id);
    let attempt_no = attempt.map(|a| a.attempt_no).unwrap_or(1);

    let (failed, title) = attempt_status(history, attempt);
    let status_label = if is_last && request.status != RequestStatus::Ok {
        status_name(request.status)
    } else if failed {
        "error"
    } else {
        "ok"
    };
    let status_class = if status_label == "ok" { "" } else { " is-bad" };
    let title_attr = title
        .map(|title| format!(r#" title="{}""#, esc(&title)))
        .unwrap_or_default();

    let ttft = attempt
        .and_then(|a| a.ttft_ms)
        .map(ms)
        .or_else(|| {
            if attempt.is_none() {
                request.ttft_ms.map(ms)
            } else {
                None
            }
        })
        .unwrap_or_else(|| "—".to_string());
    let attempt_cell = if total > 1 {
        format!(
            r#"<span class="mono">{} / {}</span> <span class="badge">{}</span>"#,
            attempt_no,
            total,
            esc(s.requests_retried)
        )
    } else {
        format!("{attempt_no} / {total}")
    };
    let tokens = if is_last {
        format!(
            "{} / {} · {} cached",
            crate::table::tokens(request.input_tokens),
            crate::table::tokens(request.output_tokens),
            crate::table::tokens(request.cached_tokens)
        )
    } else {
        "—".to_string()
    };
    let tps = if is_last {
        request
            .tps
            .map(|tps| format!("{tps:.1}"))
            .unwrap_or_else(|| "—".to_string())
    } else {
        "—".to_string()
    };
    let coverage = coverage_name(request.coverage);
    let coverage_class = if request.coverage == GateClass::Metered {
        " is-metered"
    } else {
        ""
    };
    let cost = if is_last {
        crate::table::money_with(request.cost, &request.currency)
    } else {
        "—".to_string()
    };
    let model = truncate(&request.model, 32);

    let _ = write!(
        out,
        r#"<tr><td class="mono">{}</td><td><span class="request-status{}"{}>{}</span></td><td>{}</td><td class="mono">{}</td><td class="mono">{}</td><td class="mono" title="{}">{}</td><td class="mono">{}</td><td class="mono">{}</td><td class="mono">{}</td><td><span class="request-coverage{}">{}</span></td><td class="mono">{}</td></tr>"#,
        esc(&clock(at)),
        status_class,
        title_attr,
        esc(status_label),
        attempt_cell,
        esc(provider),
        esc(account),
        esc(&request.model),
        esc(&model),
        esc(&tokens),
        esc(&ttft),
        esc(&tps),
        coverage_class,
        esc(coverage),
        esc(&cost),
    );
}

/// The attempt's own outcome. The class is the useful detail when a retry
/// succeeded: the final row says ok, the earlier row says why.
fn attempt_status(
    history: &RequestHistory,
    attempt: Option<&AttemptDetail>,
) -> (bool, Option<String>) {
    let Some(attempt) = attempt else {
        return (history.request.status != RequestStatus::Ok, None);
    };
    let failed =
        attempt.error_class.is_some() || attempt.status_code.is_some_and(|code| code >= 400);
    let title = match (&attempt.error_class, attempt.status_code) {
        (Some(class), Some(code)) => Some(format!("{class} (HTTP {code})")),
        (Some(class), None) => Some(class.clone()),
        (None, Some(code)) => Some(format!("HTTP {code}")),
        (None, None) => None,
    };
    (failed, title)
}

fn status_name(status: RequestStatus) -> &'static str {
    match status {
        RequestStatus::Ok => "ok",
        RequestStatus::Error => "error",
        RequestStatus::Aborted => "aborted",
    }
}

fn coverage_name(coverage: GateClass) -> &'static str {
    coverage_key(coverage)
}

fn coverage_key(coverage: GateClass) -> &'static str {
    match coverage {
        GateClass::Covered => "covered",
        GateClass::Metered => "metered",
        GateClass::TokenAllowance => "token_allowance",
        GateClass::RateLimit => "rate_limit",
    }
}

fn clock(at: time::OffsetDateTime) -> String {
    const STAMP: &[time::format_description::FormatItem<'_>] =
        time::macros::format_description!("[month]-[day] [hour]:[minute]:[second]");
    at.format(STAMP).unwrap_or_else(|_| "--:--:--".to_string())
}

fn ms(value: u64) -> String {
    if value >= 1_000 {
        format!("{:.1}s", value as f64 / 1000.0)
    } else {
        format!("{value}ms")
    }
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let head: String = text.chars().take(width.saturating_sub(1)).collect();
    format!("{head}…")
}

fn empty(message: &str) -> String {
    format!(r#"<div class="empty">{}</div>"#, esc(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::RequestSummary;
    use crate::domain::{AccountId, ProviderId};
    use rust_decimal::Decimal;

    fn request(id: &str, retries: u32) -> RequestSummary {
        RequestSummary {
            ts: time::OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(),
            request_id: id.to_string(),
            provider: "xkiro".to_string(),
            account_id: "acc-1".to_string(),
            model: "z-ai/glm-5.3-flash".to_string(),
            input_tokens: 1_200,
            output_tokens: 300,
            cached_tokens: 1_024,
            ttft_ms: Some(240),
            tps: Some(45.2),
            coverage: GateClass::Covered,
            status: RequestStatus::Ok,
            cost: "0.25".parse::<Decimal>().unwrap(),
            currency: "USD".to_string(),
            retries,
        }
    }

    fn attempt(no: u32, status: Option<u16>, class: Option<&str>) -> AttemptDetail {
        AttemptDetail {
            attempt_no: no,
            account_id: AccountId::from("acc-1"),
            provider: ProviderId::from("xkiro"),
            started_at: time::OffsetDateTime::from_unix_timestamp(1_800_000_000 + i64::from(no))
                .unwrap(),
            ttft_ms: if class.is_some() { None } else { Some(240) },
            status_code: status,
            error_class: class.map(str::to_string),
            retry_after_ms: None,
            billed: class.is_none(),
        }
    }

    fn sample() -> Vec<RequestHistory> {
        let mut metered = request("r2", 0);
        metered.coverage = GateClass::Metered;
        metered.status = RequestStatus::Error;
        metered.cost = "0.5".parse::<Decimal>().unwrap();
        metered.model = "deepseek-chat".to_string();
        vec![
            RequestHistory {
                request: request("r1", 1),
                attempts: vec![
                    attempt(1, Some(429), Some("quota_exhausted")),
                    attempt(2, Some(200), None),
                ],
            },
            RequestHistory {
                request: metered,
                attempts: vec![attempt(1, Some(402), Some("quota_exhausted"))],
            },
        ]
    }

    #[test]
    fn every_attempt_gets_a_row_and_a_successful_retry_still_shows_the_failure() {
        let html = render(
            Some(&sample()),
            Filters::sanitise(None, false, None),
            Lang::En,
        );
        assert!(html.contains("quota_exhausted"));
        assert!(html.contains("2 / 2"));
        assert!(html.contains("retried"));
        assert!(html.contains("HTTP 402"));
        assert!(html.contains("error"));
        assert!(
            html.contains("1.2K / 300"),
            "the request owns its token count"
        );
        assert!(html.contains("0.25 USD"));
        assert!(html.contains("0.5 USD"));
        assert!(html.contains("metered"));
    }

    #[test]
    fn cost_and_tokens_are_printed_once_per_request() {
        let html = render(
            Some(&sample()),
            Filters::sanitise(None, false, None),
            Lang::En,
        );
        assert_eq!(html.matches("0.25 USD").count(), 1);
        assert_eq!(
            html.matches("1.2K / 300").count(),
            2,
            "once per request, not per attempt"
        );
        assert!(
            html.contains("—"),
            "earlier attempt fields are explicitly absent"
        );
    }

    #[test]
    fn filters_are_links_and_keep_each_other() {
        let html = render(
            Some(&sample()),
            Filters::sanitise(Some(100), true, Some(GateClass::Metered)),
            Lang::En,
        );
        assert!(html.contains("/requests?limit=25&amp;lang=en&amp;errors=1&amp;coverage=metered"));
        assert!(html.contains("is-active"));
        assert!(html.contains("errors only"));
    }

    #[test]
    fn empty_states_explain_themselves() {
        let missing = render(None, Filters::sanitise(None, false, None), Lang::En);
        assert!(missing.contains("No ledger yet"));
        let empty = render(Some(&[]), Filters::sanitise(None, false, None), Lang::En);
        assert!(empty.contains("No requests recorded yet"));
        assert!(!empty.contains("<table"));
    }

    #[test]
    fn both_languages_render_the_same_rows() {
        let en = render(
            Some(&sample()),
            Filters::sanitise(None, false, None),
            Lang::En,
        );
        let zh = render(
            Some(&sample()),
            Filters::sanitise(None, false, None),
            Lang::Zh,
        );
        for number in ["0.25 USD", "0.5 USD", "1.2K / 300", "240ms", "45.2"] {
            assert!(en.contains(number), "EN missing {number}");
            assert!(zh.contains(number), "ZH missing {number}");
        }
        assert!(zh.contains("请求"));
        assert!(zh.contains("已重试"));
    }
}
