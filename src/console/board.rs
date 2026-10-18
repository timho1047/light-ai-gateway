//! The Board: the pool as the operator reads it.
//!
//! Rendering is a pure function of the live Account snapshot and a timestamp, so
//! the page and the auto-refresh partial share one renderer and the tests need
//! no I/O. Money keeps the ledger's decimal precision; Covered and Metered are
//! separate columns and are never added together.

use super::copy::Lang;
use super::layout::esc;
use crate::domain::{Account, AccountOverview, AccountState, Gate, GateKind};
use rust_decimal::Decimal;
use std::fmt::Write;
use time::OffsetDateTime;

const WINDOWS: [GateKind; 3] = [GateKind::FiveHour, GateKind::Weekly, GateKind::Monthly];

/// Everything the Board renders besides the timestamp: the live accounts, the
/// ledger's vitals, the pool's in-flight count and the drifted learned prices.
pub struct BoardInput<'a> {
    pub accounts: &'a [Account],
    pub vitals: Option<&'a crate::store::BoardVitals>,
    pub in_flight: u32,
    pub drift: &'a [(
        String,
        String,
        crate::tier::TierKey,
        crate::meter::PriceDrift,
    )],
}

pub fn render(input: &BoardInput, at: OffsetDateTime, lang: Lang) -> String {
    let s = lang.strings();
    let accounts = input.accounts;
    let overviews: Vec<AccountOverview> = accounts.iter().map(Account::overview).collect();
    let mut out = String::new();
    let _ = write!(
        out,
        r#"<section class="section">
  <div class="section-head">
    <div class="eyebrow">{}</div>
    <h2 class="section-title">{}</h2>
    <p class="lede">{}</p>
    <div class="updated">{} <b data-updated></b></div>
  </div>
  <div class="board-grid">
    <div class="board-main">"#,
        esc(s.board_eyebrow),
        esc(s.board_title),
        lede(&overviews, lang),
        esc(s.updated),
    );
    table(&mut out, &overviews, at, lang);
    out.push_str("</div>");
    out.push_str(&rail(input, &overviews, lang));
    out.push_str("</div></section>");
    out
}

fn table(out: &mut String, accounts: &[AccountOverview], at: OffsetDateTime, lang: Lang) {
    let s = lang.strings();
    if accounts.is_empty() {
        let _ = write!(out, "<div class=\"empty\">{}</div>", esc(s.no_accounts));
    } else if accounts
        .iter()
        .all(|a| a.state == AccountState::Unknown && a.gates.is_empty())
    {
        let _ = write!(out, "<div class=\"empty\">{}</div>", esc(s.not_probed));
    } else {
        let _ = write!(
            out,
            r#"<table class="accounts">
  <thead><tr><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th></tr></thead>
  <tbody>"#,
            esc(s.col_account),
            esc(s.col_covered),
            esc(s.col_metered),
            esc(s.col_resets),
            esc(s.col_state),
        );
        for account in accounts {
            row(out, account, at, lang);
        }
        out.push_str("</tbody></table>");
    }
}

fn rail(input: &BoardInput, overviews: &[AccountOverview], lang: Lang) -> String {
    let s = lang.strings();
    let mut out = String::from("<aside class=\"rail\">");

    let inventory = inventory(overviews);
    if !inventory.is_empty() {
        let _ = write!(
            out,
            "<div class=\"rail-block\"><h3>{}</h3><div class=\"rail-rows\">",
            esc(s.inventory)
        );
        for (kind, used, cap) in inventory {
            let _ = write!(
                out,
                "<div class=\"rail-row\"><span>{}</span><b>{}</b></div>",
                esc(kind.label()),
                esc(&percent(used, cap))
            );
        }
        let _ = write!(
            out,
            "<div class=\"rail-row\"><span>{}</span><b>{}</b></div>",
            esc(s.inventory_pool),
            esc(&pool_percent(overviews))
        );
        out.push_str("</div></div>");
    }

    let _ = write!(
        out,
        "<div class=\"rail-block\"><h3>{}</h3><div class=\"rail-rows\">",
        esc(s.vitals)
    );
    let dash = "—".to_string();
    let requests = input
        .vitals
        .map(|v| v.requests_last_minute.to_string())
        .unwrap_or_else(|| dash.clone());
    let errors = input
        .vitals
        .map(|v| v.errors_last_hour.to_string())
        .unwrap_or_else(|| dash.clone());
    let p95 = input
        .vitals
        .and_then(|v| v.p95_ttft_ms)
        .map(|ms| {
            if ms >= 1000.0 {
                format!("{:.1}s", ms / 1000.0)
            } else {
                format!("{ms:.0}ms")
            }
        })
        .unwrap_or_else(|| dash.clone());
    let cache = input
        .vitals
        .and_then(|v| v.cache_hit)
        .map(percent_f64)
        .unwrap_or_else(|| dash.clone());
    rail_row(&mut out, s.requests_per_minute, &requests, false);
    rail_row(&mut out, s.in_flight, &input.in_flight.to_string(), false);
    rail_row(&mut out, s.p95_ttft, &p95, false);
    rail_row(
        &mut out,
        s.errors_1h,
        &errors,
        errors != "0" && errors != dash,
    );
    rail_row(&mut out, s.cache_hit, &cache, false);
    out.push_str("</div></div>");

    let _ = write!(
        out,
        "<div class=\"rail-block\"><h3>{}</h3><div class=\"rail-rows\">",
        esc(s.price_drift)
    );
    if input.drift.is_empty() {
        let _ = write!(
            out,
            "<div class=\"rail-row\"><span>{}</span></div>",
            esc(s.no_drift)
        );
    }
    for (provider, model, tier, drift) in input.drift.iter().take(4) {
        let _ = write!(
            out,
            "<div class=\"rail-row\"><span>{}/{} {}</span><b class=\"warn\">{:+.1}%</b></div>",
            esc(provider),
            esc(model),
            esc(tier.label()),
            drift.ratio * 100.0,
        );
    }
    out.push_str("</div></div>");

    let _ = write!(
        out,
        "<div class=\"rail-block\"><h3>{}</h3><button class=\"btn\" type=\"button\" data-refresh>{}</button></div>",
        esc(s.actions),
        esc(s.refresh)
    );
    out.push_str("</aside>");
    out
}

fn rail_row(out: &mut String, label: &str, value: &str, warn: bool) {
    let _ = write!(
        out,
        "<div class=\"rail-row\"><span>{}</span><b{}>{}</b></div>",
        esc(label),
        if warn { " class=\"warn\"" } else { "" },
        esc(value)
    );
}

fn percent_f64(ratio: f64) -> String {
    let rounded = (ratio * 1000.0).round() / 10.0;
    if (rounded - rounded.round()).abs() < f64::EPSILON {
        format!("{rounded:.0}%")
    } else {
        format!("{rounded:.1}%")
    }
}

fn lede(accounts: &[AccountOverview], lang: Lang) -> String {
    let stale = accounts
        .iter()
        .flat_map(|a| a.gates.iter())
        .filter(|g| g.stale)
        .count();
    let disabled = accounts
        .iter()
        .filter(|a| a.state == AccountState::Disabled)
        .count();
    let mut remaining: std::collections::BTreeMap<String, Decimal> =
        std::collections::BTreeMap::new();
    for account in accounts {
        for gate in &account.gates {
            if matches!(gate.kind, GateKind::Wallet | GateKind::PurchasedCredits) {
                *remaining
                    .entry(account.currency.clone())
                    .or_insert(Decimal::ZERO) += gate.remaining();
            }
        }
    }
    let metered = remaining
        .iter()
        .map(|(currency, amount)| money(*amount, currency))
        .collect::<Vec<_>>()
        .join(" · ");

    let pct = pool_percent(accounts);

    match lang {
        Lang::En => {
            let mut text = format!(
                "Covered allowance is <b>{pct}</b> consumed across <b>{}</b> accounts.",
                accounts.len()
            );
            if stale > 0 {
                let noun = if stale == 1 {
                    "reading is"
                } else {
                    "readings are"
                };
                let _ = write!(text, " <b>{stale}</b> {noun} stale.");
            }
            if disabled > 0 {
                let noun = if disabled == 1 {
                    "account is"
                } else {
                    "accounts are"
                };
                let _ = write!(text, " <b>{disabled}</b> {noun} disabled.");
            }
            if !metered.is_empty() {
                let _ = write!(
                    text,
                    " Metered balance remaining: <span class=\"metered\">{}</span>.",
                    esc(&metered)
                );
            }
            text
        }
        Lang::Zh => {
            let mut text = format!(
                "Covered 额度已用掉 <b>{pct}</b>，共 <b>{}</b> 个账号。",
                accounts.len()
            );
            if stale > 0 {
                let _ = write!(text, " 有 <b>{stale}</b> 条读数已 stale。");
            }
            if disabled > 0 {
                let _ = write!(text, " 有 <b>{disabled}</b> 个账号被停用。");
            }
            if !metered.is_empty() {
                let _ = write!(
                    text,
                    " Metered 余额还剩 <span class=\"metered\">{}</span>。",
                    esc(&metered)
                );
            }
            text
        }
    }
}

fn row(out: &mut String, account: &AccountOverview, at: OffsetDateTime, lang: Lang) {
    let s = lang.strings();
    let _ = write!(
        out,
        "<tr><td><div class=\"acct-name\">{}<small>{} · {}</small></div></td>",
        esc(&account.label),
        esc(&account.provider),
        esc(&account.currency),
    );

    // Covered: one mini bar per window, in the fixed order the CLI uses.
    out.push_str("<td><div class=\"windows\">");
    let mut any_covered = false;
    for kind in WINDOWS {
        let Some(gate) = account.gates.iter().find(|g| g.kind == kind) else {
            continue;
        };
        any_covered = true;
        let width = utilization(gate);
        let class = if width >= 80.0 { "hot" } else { "" };
        let _ = write!(
            out,
            r#"<div class="window"><b>{}</b><span>{} / {}</span><div class="track"><i class="{}" style="width:{:.1}%"></i></div></div>"#,
            esc(kind.label()),
            amount(gate.used),
            amount(gate.cap),
            class,
            width.clamp(0.0, 100.0),
        );
    }
    if !any_covered {
        let _ = write!(out, "<span class=\"muted\">—</span>");
    }
    out.push_str("</div></td>");

    // Metered: wallets and purchased credits, never folded into Covered.
    out.push_str("<td>");
    let mut any_metered = false;
    for gate in &account.gates {
        let label = match gate.kind {
            GateKind::Wallet => Some(s.wallet),
            GateKind::PurchasedCredits => Some(s.credits),
            _ => None,
        };
        let Some(label) = label else {
            continue;
        };
        any_metered = true;
        let stale = if gate.stale {
            "<span class=\"tag stale\">stale</span>"
        } else {
            ""
        };
        let _ = write!(
            out,
            "<div class=\"money metered\">{}{}<small>{}</small></div>",
            esc(&money(gate.remaining(), &account.currency)),
            stale,
            esc(label),
        );
    }
    if !any_metered {
        let _ = write!(out, "<span class=\"muted\">—</span>");
    }
    out.push_str("</td>");

    // Resets: window countdowns, plus a parked account's return time.
    out.push_str("<td>");
    let mut any_reset = false;
    for kind in WINDOWS {
        let Some(gate) = account.gates.iter().find(|g| g.kind == kind) else {
            continue;
        };
        let Some(resets_at) = gate.resets_at else {
            continue;
        };
        any_reset = true;
        let _ = write!(
            out,
            "<div class=\"reset\">{} <b>{}</b></div>",
            esc(kind.label()),
            esc(&countdown(at, resets_at)),
        );
    }
    if let Some(when) = account.available_at {
        any_reset = true;
        let _ = write!(
            out,
            "<div class=\"reset\">{} <b>{}</b></div>",
            esc(s.back),
            esc(&countdown(at, when)),
        );
    }
    if !any_reset {
        let _ = write!(out, "<span class=\"muted\">—</span>");
    }
    out.push_str("</td>");

    // State: the word, the stale tag and the operator-facing reason.
    let _ = write!(
        out,
        "<td><div class=\"state {}\">{}</div>",
        state_class(account.state),
        esc(account.state.label()),
    );
    if account.gates.iter().any(|g| g.stale) {
        let _ = write!(out, "<div class=\"tag stale\">{}</div>", esc(s.stale));
    }
    if let Some(error) = &account.last_error {
        let short = truncate(error, 64);
        let _ = write!(out, "<div class=\"tag danger\">{}</div>", esc(&short));
    }
    out.push_str("</td></tr>");
}

fn state_class(state: AccountState) -> &'static str {
    match state {
        AccountState::Active => "active",
        AccountState::Draining => "draining",
        AccountState::Parked => "parked",
        AccountState::Disabled => "disabled",
        AccountState::Unknown => "unknown",
    }
}

/// Covered inventory across the whole pool: sum(used) over sum(cap) for every
/// covered money window. One spent account beside a fresh one reads 50%, not
/// the "next account" figure.
fn pool_percent(accounts: &[AccountOverview]) -> String {
    let mut used = Decimal::ZERO;
    let mut cap = Decimal::ZERO;
    for (_, window_used, window_cap) in inventory(accounts) {
        used += window_used;
        cap += window_cap;
    }
    percent(used, cap)
}

fn inventory(accounts: &[AccountOverview]) -> Vec<(GateKind, Decimal, Decimal)> {
    WINDOWS
        .iter()
        .map(|kind| {
            let mut used = Decimal::ZERO;
            let mut cap = Decimal::ZERO;
            for account in accounts {
                for gate in &account.gates {
                    if gate.kind == *kind {
                        used += gate.used;
                        cap += gate.cap;
                    }
                }
            }
            (*kind, used, cap)
        })
        .filter(|(_, _, cap)| *cap > Decimal::ZERO)
        .collect()
}

fn utilization(gate: &Gate) -> f64 {
    gate.utilization_f64() * 100.0
}

fn percent(used: Decimal, cap: Decimal) -> String {
    if cap <= Decimal::ZERO {
        return "—".to_string();
    }
    let ratio = used / cap;
    let Ok(ratio) = ratio.to_string().parse::<f64>() else {
        return "—".to_string();
    };
    let rounded = (ratio * 1000.0).round() / 10.0;
    if (rounded - rounded.round()).abs() < f64::EPSILON {
        format!("{rounded:.0}%")
    } else {
        format!("{rounded:.1}%")
    }
}

/// Exact decimal text, trimmed; the dashboard never turns money into f64.
fn amount(value: Decimal) -> String {
    value.round_dp(2).normalize().to_string()
}

/// Money keeps at least two decimals so a balance reads as money, not a rate.
fn money_amount(value: Decimal) -> String {
    let text = value.round_dp(2).to_string();
    match text.split_once('.') {
        Some((_, frac)) if frac.len() >= 2 => text,
        Some((int, frac)) => format!("{int}.{frac}0"),
        None => format!("{text}.00"),
    }
}

fn money(value: Decimal, currency: &str) -> String {
    if currency.eq_ignore_ascii_case("USD") {
        format!("{}{}", "$", money_amount(value))
    } else {
        format!("{} {}", money_amount(value), currency)
    }
}

fn countdown(at: OffsetDateTime, when: OffsetDateTime) -> String {
    let seconds = (when - at).whole_seconds().max(0);
    let minutes = seconds / 60;
    if minutes < 1 {
        "<1m".to_string()
    } else if minutes < 60 {
        format!("{minutes}m")
    } else if minutes < 24 * 60 {
        format!("{}h {}m", minutes / 60, minutes % 60)
    } else {
        format!("{}d {}h", minutes / (24 * 60), (minutes % (24 * 60)) / 60)
    }
}

fn truncate(text: &str, width: usize) -> String {
    let mut short: String = text.chars().take(width).collect();
    if text.chars().count() > width {
        short.push('…');
    }
    short
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::gate;
    use crate::domain::ApiKey;

    fn ts(epoch: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(epoch).expect("timestamp")
    }

    fn board(accounts: &[Account], at: OffsetDateTime, lang: Lang) -> String {
        render(
            &BoardInput {
                accounts,
                vitals: None,
                in_flight: 0,
                drift: &[],
            },
            at,
            lang,
        )
    }

    fn account(
        provider: &str,
        id: &str,
        label: &str,
        state: AccountState,
        gates: Vec<Gate>,
    ) -> Account {
        Account {
            id: id.to_string(),
            provider: provider.to_string(),
            label: label.to_string(),
            base_url: "https://api.example.test/v1".to_string(),
            api_key: ApiKey::new("sk-test-DO-NOT-LEAK"),
            currency: "USD".to_string(),
            gates,
            state,
            available_at: None,
            window_started_at: None,
            credit_multiplier: Decimal::ONE,
            rpm_limit: None,
            concurrency_limit: None,
            allow_metered: None,
            last_probe_at: Some(ts(1_000_000)),
            last_error: None,
        }
    }

    fn covered_account() -> Account {
        account(
            "xkiro",
            "acc-1",
            "primary",
            AccountState::Active,
            vec![
                gate(
                    GateKind::FiveHour,
                    Decimal::from(20),
                    Decimal::from(10),
                    None,
                ),
                gate(
                    GateKind::Weekly,
                    Decimal::from(80),
                    Decimal::from(20),
                    Some(ts(1_000_000 + 3 * 3600 + 12 * 60)),
                ),
                gate(
                    GateKind::Wallet,
                    Decimal::new(1240, 2),
                    Decimal::new(500, 2),
                    None,
                ),
            ],
        )
    }

    #[test]
    fn the_board_renders_an_account_state_and_its_windows() {
        let at = ts(1_000_000);
        let html = board(&[covered_account()], at, Lang::En);
        assert!(html.contains("primary"));
        assert!(html.contains("xkiro"));
        assert!(html.contains("ACTIVE"));
        assert!(html.contains("10 / 20"));
        assert!(html.contains("3h 12m"));
        assert!(html.contains("$7.40"), "wallet remaining is metered money");
        assert!(
            html.contains("30%"),
            "inventory across every covered window"
        );
    }

    #[test]
    fn the_rail_reports_inventory_vitals_and_drift() {
        let vitals = crate::store::BoardVitals {
            requests_last_minute: 7,
            errors_last_hour: 1,
            p95_ttft_ms: Some(1200.0),
            cache_hit: Some(0.78),
        };
        let drift = vec![(
            "xkiro".to_string(),
            "z-ai/glm-5.3-flash".to_string(),
            crate::tier::TierKey::Default,
            crate::meter::PriceDrift {
                expected: Decimal::from(25),
                observed: Decimal::from(90),
                expected_per_million: Decimal::from(25),
                actual_per_million: Decimal::from(90),
                total_tokens: 12_000,
                ratio: 2.523,
                samples: 12,
                significant: true,
            },
        )];
        let accounts = [covered_account()];
        let html = render(
            &BoardInput {
                accounts: &accounts,
                vitals: Some(&vitals),
                in_flight: 3,
                drift: &drift,
            },
            ts(1_000_000),
            Lang::En,
        );
        assert!(html.contains("Vitals"));
        assert!(html.contains("requests / min"));
        assert!(html.contains("1.2s"));
        assert!(html.contains("78%"));
        assert!(html.contains("+252.3%"));
        assert!(html.contains("xkiro/z-ai/glm-5.3-flash"));
        assert!(html.contains("data-refresh"));
        assert!(html.contains("Inventory"));
    }

    #[test]
    fn pool_inventory_is_inventory_not_the_next_account() {
        let mut spent = covered_account();
        spent.id = "spent".to_string();
        spent.label = "spent".to_string();
        spent.gates = vec![gate(
            GateKind::FiveHour,
            Decimal::from(20),
            Decimal::from(20),
            None,
        )];
        let mut fresh = covered_account();
        fresh.id = "fresh".to_string();
        fresh.label = "fresh".to_string();
        fresh.gates = vec![gate(
            GateKind::FiveHour,
            Decimal::from(20),
            Decimal::ZERO,
            None,
        )];
        let html = board(&[spent, fresh], ts(1_000_000), Lang::En);
        assert!(
            html.contains("50%"),
            "one spent account beside a fresh one is 50%"
        );
    }

    #[test]
    fn a_stale_reading_is_never_rendered_as_zero() {
        let mut account = covered_account();
        for gate in &mut account.gates {
            gate.stale = true;
        }
        let html = board(&[account], ts(1_000_000), Lang::En);
        assert!(html.contains("stale"));
        assert!(html.contains("10 / 20"));
        assert!(!html.contains(">0 / 20<"));
    }

    #[test]
    fn the_api_key_never_reaches_the_page() {
        let html = board(&[covered_account()], ts(1_000_000), Lang::En);
        assert!(!html.contains("sk-test"));
        assert!(!html.contains("api_key"));
    }

    #[test]
    fn covered_and_metered_are_not_folded_together() {
        let html = board(&[covered_account()], ts(1_000_000), Lang::En);
        assert!(html.contains("10 / 20"));
        assert!(html.contains("$7.40"));
        assert!(!html.contains("$17.40"), "covered is not added to metered");
    }

    #[test]
    fn a_never_probed_pool_says_so_instead_of_showing_empty_rows() {
        let mut account = covered_account();
        account.gates.clear();
        account.last_probe_at = None;
        account.state = AccountState::Unknown;
        let html = board(&[account], ts(1_000_000), Lang::En);
        assert!(html.contains("probed"));
        assert!(!html.contains("<table"));
    }

    #[test]
    fn the_chinese_board_speaks_chinese() {
        let html = board(&[covered_account()], ts(1_000_000), Lang::Zh);
        assert!(html.contains("账号与额度"));
        assert!(html.contains("个账号"));
    }

    #[test]
    fn a_malicious_label_is_escaped() {
        let mut account = covered_account();
        account.label = "<img src=x onerror=alert(1)>".to_string();
        let html = board(&[account], ts(1_000_000), Lang::En);
        assert!(!html.contains("<img src=x"));
        assert!(html.contains("&lt;img"));
    }
}
