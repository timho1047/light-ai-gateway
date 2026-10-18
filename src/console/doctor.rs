//! The Doctor page: the same checks the terminal runs, with verdicts in the
//! page language and one bounded action to probe every account.

use super::copy::{Lang, Strings};
use super::layout::esc;
use crate::doctor::{DoctorReport, Verdict};
use std::fmt::Write;

pub fn render(report: &DoctorReport, lang: Lang) -> String {
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
        esc(s.doctor_eyebrow),
        esc(s.doctor_title),
        esc(s.doctor_lede),
    );

    if report.problems == 0 {
        let _ = write!(
            out,
            r#"<div class="banner ok"><b>{}</b></div>"#,
            esc(s.doctor_no_problems)
        );
    } else {
        let _ = write!(
            out,
            r#"<div class="banner error"><b>{} {}</b></div>"#,
            report.problems,
            esc(s.doctor_problems_found)
        );
    }

    let _ = write!(
        out,
        r#"<div class="prices-actions"><button class="btn btn-primary" type="button" data-probe-all>{}</button></div><div class="prices-result" data-doctor-result aria-live="polite"></div><p class="hint prices-note">{}</p>"#,
        esc(s.doctor_probe_all),
        esc(s.doctor_note),
    );

    let _ = write!(
        out,
        r#"<table class="table doctor-table"><thead><tr><th>{}</th><th>{}</th><th>{}</th></tr></thead><tbody>"#,
        esc(s.doctor_col_check),
        esc(s.doctor_col_verdict),
        esc(s.doctor_col_detail),
    );
    for check in &report.checks {
        let (class, label) = verdict_label(check.verdict, s);
        let _ = write!(
            out,
            r#"<tr><td class="mono">{}</td><td><span class="verdict {}">{}</span></td><td class="mono detail">{}</td></tr>"#,
            esc(&check.label),
            class,
            esc(label),
            esc(&check.detail),
        );
    }
    out.push_str("</tbody></table></section>");
    out
}

fn verdict_label(verdict: Verdict, s: &Strings) -> (&'static str, &'static str) {
    match verdict {
        Verdict::Ok => ("ok", s.doctor_ok),
        Verdict::Warn => ("warn", s.doctor_warn),
        Verdict::Fail => ("fail", s.doctor_fail),
        Verdict::Skip => ("skip", s.doctor_skip),
    }
}

fn empty_report(message: &str) -> DoctorReport {
    DoctorReport {
        checks: vec![crate::doctor::DoctorCheck {
            label: "doctor".to_string(),
            verdict: Verdict::Fail,
            detail: message.to_string(),
        }],
        problems: 1,
    }
}

pub fn unavailable(message: &str) -> DoctorReport {
    empty_report(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::DoctorCheck;

    fn report() -> DoctorReport {
        DoctorReport {
            checks: vec![
                DoctorCheck {
                    label: "providers".to_string(),
                    verdict: Verdict::Ok,
                    detail: "2 providers, 3 accounts, 2 routes".to_string(),
                },
                DoctorCheck {
                    label: "prices".to_string(),
                    verdict: Verdict::Fail,
                    detail: "no route sends z-ai/glm to xkiro".to_string(),
                },
                DoctorCheck {
                    label: "probes".to_string(),
                    verdict: Verdict::Skip,
                    detail: "mock/acc-1: no probe target".to_string(),
                },
                DoctorCheck {
                    label: "policy".to_string(),
                    verdict: Verdict::Warn,
                    detail: "allow_metered=true".to_string(),
                },
            ],
            problems: 1,
        }
    }

    #[test]
    fn every_check_and_failure_is_rendered_with_a_verdict() {
        let html = render(&report(), Lang::En);
        assert!(html.contains("providers"));
        assert!(html.contains("no route sends"));
        assert!(html.contains("FAIL"));
        assert!(html.contains("SKIP"));
        assert!(html.contains("WARN"));
        assert!(html.contains("1 problem(s) found"));
        assert!(html.contains("data-probe-all"));
    }

    #[test]
    fn both_languages_show_the_same_checks() {
        let en = render(&report(), Lang::En);
        let zh = render(&report(), Lang::Zh);
        for number in ["2 providers", "3 accounts", "allow_metered=true"] {
            assert!(en.contains(number), "EN missing {number}");
            assert!(zh.contains(number), "ZH missing {number}");
        }
        assert!(zh.contains("诊断"));
        assert!(zh.contains("失败"));
        assert!(zh.contains("跳过"));
    }

    #[test]
    fn a_clean_report_says_so_instead_of_showing_zeroes() {
        let clean = DoctorReport {
            checks: vec![DoctorCheck {
                label: "providers".to_string(),
                verdict: Verdict::Ok,
                detail: "one".to_string(),
            }],
            problems: 0,
        };
        let html = render(&clean, Lang::En);
        assert!(html.contains("No problems found"));
    }
}
