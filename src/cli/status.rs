//! `lgw status`: the covered-versus-metered account board.

use super::open_ledger;
use crate::ansi::Palette;
use crate::config::Config;
use crate::domain::{
    Account, AccountOverview, AccountState, Confidence, Gate, GateClass, GateKind,
};
use crate::router::Gateway;
use anyhow::Result;
use rust_decimal::Decimal;
use time::OffsetDateTime;

pub(super) async fn cmd_status(config: &Config, refresh: bool, pal: &Palette) -> Result<()> {
    let now = OffsetDateTime::now_utc();

    if refresh {
        let gateway = Gateway::new(config.clone()).await?;
        let rows: Vec<AccountOverview> = gateway.snapshot().iter().map(overview_of).collect();
        print!("{}", render_board(&rows, now, pal));
        let _ = gateway.shutdown();
        return Ok(());
    }

    let rows = open_ledger(&config.db_path())?.accounts()?;
    if rows.is_empty() {
        println!(
            "no accounts in the ledger yet at {}; run lgw status --refresh",
            config.db_path().display()
        );
        return Ok(());
    }
    print!("{}", render_board(&rows, now, pal));
    Ok(())
}

pub(super) fn overview_of(a: &Account) -> AccountOverview {
    AccountOverview {
        account_id: a.id.clone(),
        provider: a.provider.clone(),
        label: a.label.clone(),
        currency: a.currency.clone(),
        state: a.state,
        gates: a.gates.clone(),
        available_at: a.available_at,
        window_started_at: a.window_started_at,
        last_error: a.last_error.clone(),
    }
}

const BOARD_HEADERS: [(&str, crate::table::Align); 6] = [
    ("PROVIDER", crate::table::Align::Left),
    ("ACCOUNT", crate::table::Align::Left),
    ("COVERED", crate::table::Align::Left),
    ("METERED", crate::table::Align::Left),
    // Left, not right: the cell now holds several labelled countdowns, and
    // "which account resets first" is answered by reading those down one
    // column. Right-aligning the whole cell shifted each row's entries by its
    // own length, so they never lined up.
    ("RESETS", crate::table::Align::Left),
    ("STATE", crate::table::Align::Left),
];

/// Render the account board.
///
/// COVERED shows what has been consumed out of a prepaid allowance (used/cap).
/// METERED shows the money that is left, and says plainly when none of it has
/// been touched. The two are deliberately different shapes so that an operator
/// scanning the table cannot mistake one for the other.
///
/// All three are composite columns: several readings, each with its own label,
/// plus a unit. Their sub-columns are measured across the whole board (see
/// BoardWidths) rather than per cell, so a fraction, a window label and a
/// currency code line up down the page even though accounts hold different
/// numbers of windows.
pub(super) fn render_board(rows: &[AccountOverview], now: OffsetDateTime, pal: &Palette) -> String {
    // Measured widths, like every other table here. The board used to carry its
    // own [14, 18, 26, 26, 8], and an account whose state explanation ran long --
    // "DISABLED (insufficient_credit)" is exactly 32 -- pulled the columns after
    // it out of line.
    let covered: Vec<Option<Covered>> = rows.iter().map(|r| covered_of(r, pal)).collect();
    let metered: Vec<Option<Metered>> = rows.iter().map(|r| metered_of(r, pal)).collect();
    let resets: Vec<Option<Resets>> = rows.iter().map(|r| resets_of(r, now)).collect();
    let covered_refs: Vec<&Covered> = covered.iter().flatten().collect();
    let metered_refs: Vec<&Metered> = metered.iter().flatten().collect();
    let reset_refs: Vec<&Resets> = resets.iter().flatten().collect();
    let widths = BoardWidths::measure(&covered_refs, &metered_refs, &reset_refs);

    let mut table = crate::table::Table::new(&BOARD_HEADERS);
    for (i, r) in rows.iter().enumerate() {
        table.push(vec![
            r.provider.clone(),
            r.account_id.clone(),
            render_or_dash(covered[i].as_ref(), |c| widths.covered(c)),
            render_or_dash(metered[i].as_ref(), |m| widths.metered(m)),
            render_or_dash(resets[i].as_ref(), |x| widths.resets(x)),
            state_cell(r, pal),
        ]);
    }
    let mut out = table.render(pal);
    out.push('\n');
    // The legend explains two columns that look alike and are not, so it is set
    // quieter than the table it belongs to.
    out.push_str(&pal.dim(
        "covered = prepaid allowance consumed (used/cap, forfeited at reset)   \
         metered = real money remaining (spending it is permanent)",
    ));
    out.push('\n');
    out.push_str(&pal.dim(
        "back = when a parked account can serve again   \
         ~ = estimated reading   STALE = last probe failed, previous value carried over",
    ));
    out.push('\n');
    out
}

/// An account with nothing to show in a composite column gets a dash rather than
/// a blank: a blank cell reads as "nobody looked", a dash as "there is nothing
/// there to look at".
fn render_or_dash<T>(cell: Option<&T>, render: impl Fn(&T) -> String) -> String {
    match cell {
        Some(cell) => render(cell),
        None => "-".to_string(),
    }
}

/// One window of prepaid allowance: its label, what has been consumed, and its cap.
struct Window {
    label: String,
    used: String,
    cap: String,
}

/// The covered column of one account, before the board's shared widths are known.
struct Covered {
    windows: Vec<Window>,
    currency: String,
    marks: String,
}

impl Covered {
    /// The windows on one line, with each sub-column padded to the shared widths.
    ///
    /// The cap is padded on the right, so the slashes of two accounts sit in the
    /// same column and "which of these is closer to its limit" is answered by
    /// reading down instead of by counting characters.
    fn windows_text(&self, w: &BoardWidths) -> String {
        let (lw, uw, cw) = (w.window, w.used, w.cap);
        self.windows
            .iter()
            .map(|win| {
                format!(
                    "{label:<lw$} {used:>uw$}/{cap:<cw$}",
                    label = win.label,
                    used = win.used,
                    cap = win.cap
                )
            })
            .collect::<Vec<_>>()
            .join("  ")
    }
}

/// One money pot: what it is, what is left, and what has happened to it.
struct Pot {
    label: String,
    left: String,
    note: String,
}

/// The metered column of one account, before the board's shared widths are known.
struct Metered {
    pots: Vec<Pot>,
    currency: String,
    marks: String,
}

/// The widths of the sub-columns inside the three composite cells, measured
/// across every row of the board.
///
/// Without this each account brought its own widths: a "1320.1/1320" pushed that
/// row's unit a column right of everyone else's, and two fractions could not be
/// compared by scanning one vertical line. Measuring the sub-columns the way the
/// outer table measures its own columns is what makes the board a table rather
/// than a stack of formatted strings.
#[derive(Clone, Copy, Default)]
struct BoardWidths {
    window: usize,
    used: usize,
    cap: usize,
    pot: usize,
    left: usize,
    /// Width of the window part of the widest row. Rows carry different numbers
    /// of windows, so the unit is another column that has to be lined up rather
    /// than simply appended.
    windows_part: usize,
    /// Width of the window label inside the RESETS cell.
    ///
    /// Only the label is padded. The countdown that follows is left alone: its
    /// own length is the reading, and padding it only pushes the digits that
    /// matter further apart.
    deadline_label: usize,
}

impl BoardWidths {
    fn measure(covered: &[&Covered], metered: &[&Metered], resets: &[&Resets]) -> Self {
        let mut w = BoardWidths::default();
        for c in covered {
            for win in &c.windows {
                w.window = w.window.max(win.label.chars().count());
                w.used = w.used.max(win.used.chars().count());
                w.cap = w.cap.max(win.cap.chars().count());
            }
        }
        for m in metered {
            for p in &m.pots {
                w.pot = w.pot.max(p.label.chars().count());
                w.left = w.left.max(p.left.chars().count());
            }
        }
        for r in resets {
            for e in &r.entries {
                w.deadline_label = w.deadline_label.max(e.label.chars().count());
            }
        }
        // A second pass, because the width of the window part depends on the
        // widths just measured.
        for c in covered {
            w.windows_part = w.windows_part.max(c.windows_text(&w).chars().count());
        }
        w
    }

    /// The RESETS cell: every countdown on its own, label first, so two accounts
    /// offering the same window can be compared by reading down one column.
    fn resets(&self, r: &Resets) -> String {
        let lw = self.deadline_label;
        r.entries
            .iter()
            .map(|e| format!("{label:<lw$} {when}", label = e.label, when = e.when))
            .collect::<Vec<_>>()
            .join("  ")
    }

    fn covered(&self, c: &Covered) -> String {
        let mut out = c.windows_text(self);
        // The unit is a column of its own: two spaces past the widest row's
        // windows, so it lands in the same place on every line.
        let gap = 2 + self.windows_part.saturating_sub(out.chars().count());
        out.push_str(&" ".repeat(gap));
        out.push_str(&c.currency);
        out.push_str(&c.marks);
        out
    }

    fn metered(&self, m: &Metered) -> String {
        let (pw, lw) = (self.pot, self.left);
        let pots = m
            .pots
            .iter()
            .map(|p| {
                format!(
                    "{label:<pw$} {left:>lw$} {currency} ({note})",
                    label = p.label,
                    left = p.left,
                    currency = m.currency,
                    note = p.note
                )
            })
            .collect::<Vec<_>>()
            .join("  ");
        format!("{pots}{}", m.marks)
    }
}

/// The covered column: the money-denominated allowances this account still has.
///
/// Only money-denominated gates appear. A gate counted in tokens is a limit on
/// a different thing -- the free-token allowance gates free models, not the paid
/// traffic this board is about -- and printing its raw number next to a currency
/// code claimed that 30000000 tokens were 30000000 dollars. It is hidden rather
/// than converted because an operator paying for every request has no use for
/// it, and an unused allowance nobody spends is not worth a column.
///
/// Every gate carries its own label. Without one the column is a row of bare
/// fractions whose meaning depends on knowing the order, which is exactly how an
/// allowance gets mistaken for a monthly cap.
fn covered_of(r: &AccountOverview, pal: &Palette) -> Option<Covered> {
    let mut gates: Vec<&Gate> = r
        .gates
        .iter()
        .filter(|g| g.class() == GateClass::Covered)
        .collect();
    if gates.is_empty() {
        return None;
    }
    gates.sort_by_key(|g| gate_rank(g.kind));
    let windows = gates
        .iter()
        .map(|g| Window {
            label: g.kind.label().to_string(),
            used: board_amount(g.used),
            cap: board_amount(g.cap),
        })
        .collect();
    Some(Covered {
        windows,
        currency: r.currency.clone(),
        marks: markers(&gates, pal),
    })
}

fn metered_of(r: &AccountOverview, pal: &Palette) -> Option<Metered> {
    let mut gates: Vec<&Gate> = r
        .gates
        .iter()
        .filter(|g| g.class() == GateClass::Metered)
        .collect();
    if gates.is_empty() {
        return None;
    }
    gates.sort_by_key(|g| gate_rank(g.kind));
    let pots = gates
        .iter()
        .map(|g| Pot {
            label: g.kind.label().to_string(),
            left: board_amount(g.remaining()),
            // Spelled out rather than left to the reader: a balance that has
            // never been touched is the strongest evidence the account is whole,
            // and "0 spent" is a number to decode.
            note: if g.used <= Decimal::ZERO {
                "unused".to_string()
            } else {
                format!("{} spent", board_amount(g.used))
            },
        })
        .collect();
    Some(Metered {
        pots,
        currency: r.currency.clone(),
        marks: markers(&gates, pal),
    })
}

/// The two cells as the board would render them for one account on its own.
///
/// The board itself measures across every row, so these single-row forms exist
/// only for the tests that assert on one cell at a time.
#[cfg(test)]
fn covered_cell(r: &AccountOverview, pal: &Palette) -> String {
    match covered_of(r, pal) {
        Some(c) => BoardWidths::measure(&[&c], &[], &[]).covered(&c),
        None => "-".to_string(),
    }
}

#[cfg(test)]
fn metered_cell(r: &AccountOverview, pal: &Palette) -> String {
    match metered_of(r, pal) {
        Some(m) => BoardWidths::measure(&[], &[&m], &[]).metered(&m),
        None => "-".to_string(),
    }
}

/// A stale reading dominates an estimated one: it is the reading most likely to
/// be wrong, and it must never be silently rendered as a fresh number.
fn markers(gates: &[&Gate], pal: &Palette) -> String {
    if gates.iter().any(|g| g.stale) {
        // A stale reading is the one most likely to be wrong, so it is the
        // loudest thing on the row.
        pal.red(" STALE")
    } else if gates.iter().any(|g| g.confidence == Confidence::Estimated) {
        // Estimated is not wrong, only weaker evidence: yellow, not red.
        pal.yellow(" ~EST")
    } else {
        String::new()
    }
}

/// One countdown on the board: which allowance it belongs to, and how far off.
struct Reset {
    label: String,
    when: String,
}

/// The RESETS column of one account, before the board's shared widths are known.
struct Resets {
    entries: Vec<Reset>,
}

/// The RESETS column: when each allowance comes back.
///
/// Every window gets its own labelled countdown rather than only the soonest
/// one. The five-hour and the weekly window answer the same question at very
/// different distances, and that pair is the whole difference between "spent
/// until this evening" and "spent until Monday" -- which is the reason the
/// weekly reading is probed at all. Showing only the minimum hid the second.
fn resets_of(r: &AccountOverview, now: OffsetDateTime) -> Option<Resets> {
    let mut windows: Vec<&Gate> = r
        .gates
        .iter()
        .filter(|g| g.kind.is_window() && g.resets_at.is_some())
        .collect();
    if windows.is_empty() && r.available_at.is_none() {
        return None;
    }
    windows.sort_by_key(|g| gate_rank(g.kind));

    let mut entries: Vec<Reset> = Vec::new();
    // The account's own return time is only news when no window already says it.
    // A parked account is usually parked ON a window, and the same countdown
    // printed twice under two names reads as two different facts.
    if let Some(at) = r.available_at
        && !windows.iter().any(|g| {
            g.resets_at
                .is_some_and(|t| (t - at).abs() <= time::Duration::seconds(60))
        })
    {
        entries.push(Reset {
            label: "back".to_string(),
            when: format_countdown(now, at),
        });
    }
    for g in windows {
        if let Some(at) = g.resets_at {
            entries.push(Reset {
                label: g.kind.label().to_string(),
                when: format_countdown(now, at),
            });
        }
    }
    if entries.is_empty() {
        return None;
    }
    Some(Resets { entries })
}

/// The RESETS cell as the board would render it for one account on its own.
#[cfg(test)]
fn resets_cell(r: &AccountOverview, now: OffsetDateTime) -> String {
    match resets_of(r, now) {
        Some(x) => BoardWidths::measure(&[], &[], &[&x]).resets(&x),
        None => "-".to_string(),
    }
}

fn format_countdown(now: OffsetDateTime, at: OffsetDateTime) -> String {
    let delta = at - now;
    if delta.is_negative() {
        return "now".to_string();
    }
    let secs = delta.whole_seconds().max(0) as u64;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3_600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h{}m", secs / 3_600, (secs % 3_600) / 60)
    } else {
        format!("{}d{}h", secs / 86_400, (secs % 86_400) / 3_600)
    }
}

fn state_cell(r: &AccountOverview, pal: &Palette) -> String {
    // Only ACTIVE needs no action, so it is the only state that is green; a
    // state that a reader has to do something about is never the quiet one.
    let mut s = match r.state {
        AccountState::Active => pal.green(r.state.label()),
        AccountState::Draining => pal.yellow(r.state.label()),
        AccountState::Parked | AccountState::Disabled => pal.red(r.state.label()),
        AccountState::Unknown => pal.dim(r.state.label()),
    };

    // A terminal state explains itself first; otherwise describe the shape of
    // what was read. Only one parenthetical is shown so the column stays short.
    let diagnosis = match r.state {
        AccountState::Disabled | AccountState::Parked => r.last_error.clone(),
        _ => None,
    };
    let has_covered = r.gates.iter().any(|g| g.class() == GateClass::Covered);
    let has_metered = r.gates.iter().any(|g| g.class() == GateClass::Metered);
    let shape = if !has_covered && has_metered {
        // DeepSeek is the archetype: no window, no "80%", just a balance line.
        Some("metered only".to_string())
    } else if !has_covered && !has_metered {
        // A missing reading must never look like a funded account.
        Some("no reading".to_string())
    } else {
        None
    };

    if let Some(detail) = diagnosis.or(shape) {
        s.push_str(&format!(" ({detail})"));
    }
    s
}

fn gate_rank(kind: GateKind) -> u8 {
    match kind {
        GateKind::FiveHour => 0,
        GateKind::Weekly => 1,
        GateKind::Monthly => 2,
        GateKind::FreeTokens => 3,
        GateKind::Wallet => 4,
        GateKind::PurchasedCredits => 5,
        GateKind::Rpm => 6,
    }
}

/// An amount on the account board, rounded to ONE decimal place.
///
/// The ledger is Decimal arithmetic, so a running balance carries fifteen digits
/// nobody wants to read: 1.113194698/14 is a wall of digits wrapped around the
/// one that matters.
///
/// Two departures from a plain one-decimal rounding, both to avoid printing
/// something untrue:
///   - a small but REAL balance would round to 0 and read as untouched, so
///     anything under 0.05 keeps enough digits to stay visible;
///   - an exact integer keeps no decimal point, so 368 does not become 368.0.
///
/// Full precision stays available in the usage and prices reports; the board is
/// meant to be read at a glance.
pub(super) fn board_amount(d: Decimal) -> String {
    use rust_decimal::prelude::ToPrimitive;
    let v = d.to_f64().unwrap_or(0.0);
    if v == 0.0 {
        return "0".to_string();
    }

    let trim = |s: &str| -> String {
        let mut s = s.to_string();
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
        if s.is_empty() || s == "-" || s == "0" || s == "-0" {
            "0".to_string()
        } else {
            s
        }
    };

    if v.abs() < 0.05 {
        // Three decimals is enough to show that something was spent.
        let three = trim(&format!("{v:.3}"));
        if three != "0" {
            return three;
        }
        let six = trim(&format!("{v:.6}"));
        if six != "0" {
            return six;
        }
        // Non-zero in the ledger, below what six decimals can show.
        return if v < 0.0 {
            "<-0.000001".to_string()
        } else {
            "<0.000001".to_string()
        };
    }

    trim(&format!("{v:.1}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_helpers::{dec, gate, now, overview};

    #[test]
    fn board_amounts_round_to_one_decimal_without_lying() {
        // The values that prompted this: fifteen digits wrapped around one.
        assert_eq!(board_amount(dec("1.113194698")), "1.1");
        assert_eq!(board_amount(dec("4.175475087")), "4.2");
        assert_eq!(board_amount(dec("4.182168809")), "4.2");
        assert_eq!(board_amount(dec("65.824524913")), "65.8");
        assert_eq!(board_amount(dec("1320.065041")), "1320.1");
        assert_eq!(board_amount(dec("32.042344")), "32");

        // An exact integer keeps no decimal point.
        assert_eq!(board_amount(dec("368")), "368");
        assert_eq!(board_amount(dec("30000000")), "30000000");
        assert_eq!(board_amount(Decimal::ZERO), "0");

        // A small but real balance must never read as untouched: that is the
        // difference between "this account has not been used" and "it has been
        // used a little", and the board is where that judgement gets made.
        assert_eq!(board_amount(dec("0.001221")), "0.001");
        assert_eq!(board_amount(dec("0.0004")), "0.0004");
        assert_ne!(board_amount(dec("0.0000001")), "0");
        assert_eq!(board_amount(dec("-0.0000001")), "<-0.000001");

        // Money is not rounded away in the cost columns either. board_amount is
        // for the board; the report columns go through table::money, which keeps a
        // per-request cost like this one intact.
        assert_eq!(crate::table::money(dec("0.0000036")), "0.0000036");
    }

    #[test]
    fn board_keeps_covered_and_metered_visually_distinct() {
        let t = now();
        let acct = overview(
            "commandcode",
            "cc-1",
            "USD",
            AccountState::Draining,
            vec![
                gate(
                    GateKind::FiveHour,
                    "100",
                    "40",
                    Some(4_200),
                    Confidence::Exact,
                    false,
                    t,
                ),
                gate(
                    GateKind::Weekly,
                    "500",
                    "25",
                    None,
                    Confidence::Exact,
                    false,
                    t,
                ),
                gate(
                    GateKind::PurchasedCredits,
                    "12",
                    "0",
                    None,
                    Confidence::Exact,
                    false,
                    t,
                ),
            ],
        );

        let covered = covered_cell(&acct, &Palette::off());
        let metered = metered_cell(&acct, &Palette::off());
        assert_eq!(
            covered, "5h 40/100  7d 25/500  USD",
            "covered shows used/cap and its unit"
        );
        assert_eq!(
            metered, "credits 12 USD (unused)",
            "metered shows the money left and says it is untouched"
        );
        assert_ne!(covered, metered);

        let board = render_board(&[acct], t, &Palette::off());
        assert!(board.contains("COVERED"));
        assert!(board.contains("METERED"));
        assert!(board.contains("5h 40/100  7d 25/500  USD"));
        assert!(board.contains("credits 12 USD (unused)"));
        assert!(board.contains("DRAINING"));
        assert!(
            board.contains("metered = real money remaining"),
            "the legend must explain the two columns: {board}"
        );
    }

    /// The composite cells are measured across the whole board, not per row.
    ///
    /// Two accounts with different numbers of windows and wildly different
    /// fractions must still agree on where the slashes and the unit sit: that is
    /// the difference between a table of allowances and two lines of text that
    /// happen to be stacked.
    #[test]
    fn board_lines_up_the_sub_columns_of_every_row() {
        let t = now();
        let short = overview(
            "xkiro",
            "xk-1",
            "USD",
            AccountState::Active,
            vec![
                gate(
                    GateKind::FiveHour,
                    "20",
                    "0",
                    None,
                    Confidence::Exact,
                    false,
                    t,
                ),
                gate(
                    GateKind::Weekly,
                    "132",
                    "0.001",
                    None,
                    Confidence::Exact,
                    false,
                    t,
                ),
                gate(
                    GateKind::PurchasedCredits,
                    "4",
                    "0",
                    None,
                    Confidence::Exact,
                    false,
                    t,
                ),
            ],
        );
        let long = overview(
            "deepseek",
            "ds-1",
            "USD",
            AccountState::Active,
            vec![
                gate(
                    GateKind::FiveHour,
                    "200",
                    "0",
                    None,
                    Confidence::Exact,
                    false,
                    t,
                ),
                gate(
                    GateKind::Weekly,
                    "1320",
                    "1320.1",
                    None,
                    Confidence::Exact,
                    false,
                    t,
                ),
                gate(
                    GateKind::Wallet,
                    "700",
                    "16.05",
                    None,
                    Confidence::Exact,
                    false,
                    t,
                ),
            ],
        );

        let board = render_board(&[short, long], t, &Palette::off());
        let rows: Vec<&str> = board
            .lines()
            .filter(|l| l.starts_with("xkiro") || l.starts_with("deepseek"))
            .collect();
        assert_eq!(rows.len(), 2, "{board}");

        let slashes = |l: &str| l.match_indices('/').map(|(i, _)| i).collect::<Vec<_>>();
        assert_eq!(
            slashes(rows[0]),
            slashes(rows[1]),
            "the fractions drifted apart: {board}"
        );
        let unit = |l: &str| l.find("USD").expect("every row carries its unit");
        assert_eq!(
            unit(rows[0]),
            unit(rows[1]),
            "the unit column drifted: {board}"
        );
        let note = |l: &str| l.find('(').expect("the note follows the money");
        assert_eq!(
            note(rows[0]),
            note(rows[1]),
            "the pot labels are not the same width: {board}"
        );
        assert!(rows[0].contains("credits"), "the pot is labelled: {board}");
        assert!(rows[1].contains("wallet"), "the pot is labelled: {board}");
    }

    /// Colour changes what the output looks like, never what it says.
    ///
    /// Strip the escapes and the coloured board is exactly the plain one. That is
    /// what makes it safe to colour a cell in a table that is also read through a
    /// pipe, and it is why the palette is threaded through the renderers instead
    /// of being read from a global.
    #[test]
    fn colour_changes_the_look_and_nothing_else() {
        let t = now();
        let rows = vec![
            overview(
                "xkiro",
                "xk-1",
                "USD",
                AccountState::Draining,
                vec![gate(
                    GateKind::FiveHour,
                    "200",
                    "178",
                    Some(600),
                    Confidence::Estimated,
                    false,
                    t,
                )],
            ),
            overview(
                "xkiro",
                "xk-2",
                "USD",
                AccountState::Disabled,
                vec![gate(
                    GateKind::FiveHour,
                    "20",
                    "0",
                    None,
                    Confidence::Exact,
                    false,
                    t,
                )],
            ),
        ];
        let colored = render_board(&rows, t, &Palette::on());
        let plain = render_board(&rows, t, &Palette::off());

        assert!(colored.contains("\x1b[33mDRAINING\x1b[0m"), "{colored:?}");
        assert!(colored.contains("\x1b[31mDISABLED\x1b[0m"), "{colored:?}");
        assert!(colored.contains("\x1b[33m ~EST\x1b[0m"), "{colored:?}");
        assert!(!plain.contains('\x1b'), "no palette, no escapes: {plain:?}");
        assert_eq!(
            crate::ansi::strip(&colored),
            plain,
            "colour changed the text"
        );
    }

    /// The board must not print a free-token allowance.
    ///
    /// It is counted in tokens rather than money, so it has no place sitting
    /// under a currency code, and an operator who never calls a free model has
    /// no use for the number. The gate is still probed and stored -- this board
    /// simply does not show it.
    #[test]
    fn board_hides_the_free_token_allowance() {
        let t = now();
        let acct = overview(
            "xkiro",
            "xk-1",
            "USD",
            AccountState::Active,
            vec![
                gate(
                    GateKind::FiveHour,
                    "20",
                    "0.5",
                    None,
                    Confidence::Exact,
                    false,
                    t,
                ),
                gate(
                    GateKind::FreeTokens,
                    "30000000",
                    "368",
                    None,
                    Confidence::Exact,
                    false,
                    t,
                ),
            ],
        );

        let covered = covered_cell(&acct, &Palette::off());
        assert_eq!(covered, "5h 0.5/20  USD");
        assert!(!covered.contains("free"), "no free allowance: {covered}");
        assert!(!covered.contains("tok"), "no token count: {covered}");
    }

    /// A free allowance on its own is not a covered reading at all.
    #[test]
    fn a_free_allowance_alone_leaves_the_covered_column_blank() {
        let t = now();
        let acct = overview(
            "xkiro",
            "xk-2",
            "USD",
            AccountState::Active,
            vec![gate(
                GateKind::FreeTokens,
                "30000000",
                "0",
                None,
                Confidence::Exact,
                false,
                t,
            )],
        );
        assert_eq!(covered_cell(&acct, &Palette::off()), "-");
    }

    #[test]
    fn board_shows_metered_only_accounts_without_a_covered_reading() {
        let t = now();
        let acct = overview(
            "deepseek",
            "ds-1",
            "USD",
            AccountState::Active,
            vec![gate(
                GateKind::Wallet,
                "12.34",
                "0",
                None,
                Confidence::Exact,
                false,
                t,
            )],
        );
        assert_eq!(
            covered_cell(&acct, &Palette::off()),
            "-",
            "covered must be blank, not 0/0"
        );
        assert_eq!(
            metered_cell(&acct, &Palette::off()),
            "wallet 12.3 USD (unused)"
        );
        assert!(state_cell(&acct, &Palette::off()).contains("metered only"));
        assert_eq!(resets_cell(&acct, t), "-", "a wallet has no reset");
    }

    #[test]
    fn board_reports_spent_metered_money_rather_than_claiming_it_is_unused() {
        let t = now();
        let acct = overview(
            "xkiro",
            "xk-1",
            "USD",
            AccountState::Active,
            vec![gate(
                GateKind::Wallet,
                "700",
                "16.05",
                None,
                Confidence::Exact,
                false,
                t,
            )],
        );
        assert_eq!(
            metered_cell(&acct, &Palette::off()),
            "wallet 684 USD (16.1 spent)"
        );
    }

    #[test]
    fn board_marks_stale_and_estimated_readings() {
        let t = now();
        let estimated = overview(
            "xkiro",
            "xk-1",
            "USD",
            AccountState::Active,
            vec![gate(
                GateKind::FiveHour,
                "200",
                "178",
                Some(600),
                Confidence::Estimated,
                false,
                t,
            )],
        );
        assert!(covered_cell(&estimated, &Palette::off()).ends_with(" ~EST"));

        let stale = overview(
            "xkiro",
            "xk-2",
            "USD",
            AccountState::Active,
            vec![gate(
                GateKind::FiveHour,
                "200",
                "178",
                Some(600),
                Confidence::Estimated,
                true,
                t,
            )],
        );
        let cell = covered_cell(&stale, &Palette::off());
        assert!(cell.ends_with(" STALE"), "got {cell}");
        assert!(
            !cell.contains("~EST"),
            "a stale reading is not presented as merely estimated"
        );
        let board = render_board(&[stale], t, &Palette::off());
        assert!(
            board
                .lines()
                .any(|l| l.starts_with("xkiro") && l.contains("STALE")),
            "the stale row itself must carry the marker: {board}"
        );
    }

    #[test]
    fn board_prints_a_reset_countdown() {
        let t = now();
        let acct = overview(
            "xkiro",
            "xk-1",
            "USD",
            AccountState::Active,
            vec![
                gate(
                    GateKind::FiveHour,
                    "200",
                    "178",
                    Some(4_200),
                    Confidence::Exact,
                    false,
                    t,
                ),
                gate(
                    GateKind::Weekly,
                    "1000",
                    "100",
                    Some(90_000),
                    Confidence::Exact,
                    false,
                    t,
                ),
            ],
        );
        // Every window is shown, each under its own label: the 5h window is the
        // one an operator waits out tonight, the 7d one is the one that decides
        // whether the account is usable this week.
        assert_eq!(resets_cell(&acct, t), "5h 1h10m  7d 1d1h");
        let board = render_board(&[acct], t, &Palette::off());
        assert!(board.contains("5h 1h10m"), "{board}");
        assert!(board.contains("7d 1d1h"), "{board}");
    }

    /// The account's own return time is shown as "back" only when no window
    /// already covers it.
    ///
    /// A parked account is parked on a window, and that window's countdown is
    /// already on the row: printing the same deadline twice under two names
    /// would read as two different facts.
    #[test]
    fn the_account_return_time_is_not_repeated_when_a_window_already_says_it() {
        let t = now();
        let gates = vec![gate(
            GateKind::FiveHour,
            "200",
            "200",
            Some(4_200),
            Confidence::Exact,
            false,
            t,
        )];
        let mut acct = overview("xkiro", "xk-1", "USD", AccountState::Parked, gates);
        acct.available_at = Some(t + time::Duration::seconds(4_200));
        assert_eq!(resets_cell(&acct, t), "5h 1h10m");

        // A return time no window accounts for is its own entry: it is the only
        // thing on the row that says when the account can serve again.
        acct.available_at = Some(t + time::Duration::seconds(600));
        // "back" widens the label column, so every label in the cell lines up
        // under it -- that column alignment is the point of measuring it.
        assert_eq!(resets_cell(&acct, t), "back 10m  5h   1h10m");

        // No window at all: the return time stands alone.
        acct.gates.clear();
        assert_eq!(resets_cell(&acct, t), "back 10m");
    }

    #[test]
    fn countdown_formatting_is_readable_at_every_scale() {
        let t = now();
        assert_eq!(format_countdown(t, t + time::Duration::seconds(45)), "45s");
        assert_eq!(format_countdown(t, t + time::Duration::seconds(600)), "10m");
        assert_eq!(
            format_countdown(t, t + time::Duration::seconds(4_200)),
            "1h10m"
        );
        assert_eq!(
            format_countdown(t, t + time::Duration::seconds(93_600)),
            "1d2h"
        );
        assert_eq!(format_countdown(t, t - time::Duration::seconds(5)), "now");
    }

    #[test]
    fn account_with_no_gates_is_marked_as_having_no_reading() {
        let acct = overview("xkiro", "xk-1", "USD", AccountState::Unknown, vec![]);
        assert_eq!(covered_cell(&acct, &Palette::off()), "-");
        assert_eq!(metered_cell(&acct, &Palette::off()), "-");
        assert!(state_cell(&acct, &Palette::off()).contains("no reading"));
        assert!(!state_cell(&acct, &Palette::off()).contains("metered only"));
    }

    #[test]
    fn disabled_account_shows_its_reason() {
        let mut acct = overview("xkiro", "xk-1", "USD", AccountState::Disabled, vec![]);
        acct.last_error = Some("auth".to_string());
        assert_eq!(state_cell(&acct, &Palette::off()), "DISABLED (auth)");
    }
}
