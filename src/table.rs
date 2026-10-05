//! Text tables, and the scale numbers are read at.
//!
//! A hard-coded width is a promise about the data that the data does not keep:
//! one model name a few characters longer than its column and every later column
//! slides right while the header stays where it was. A 30-digit cost does the
//! same thing to the usage table, and a nine-digit token count pushes three
//! columns out of line. Widths spelled out inside the format string --
//! [14, 18, 26, 26, 8] and friends -- cannot hold that promise.
//!
//! Widths here are measured from the content, so a table cannot go crooked. The
//! only remaining question is how wide the content is, which is what tokens() and
//! money() answer.

use crate::ansi::{Palette, visible_width};
use rust_decimal::Decimal;

/// Which side a column's padding goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

/// Gap between columns.
const SEPARATOR: &str = "  ";

/// A table whose columns are as wide as their widest cell.
#[derive(Debug, Default)]
pub struct Table {
    headers: Vec<String>,
    alignment: Vec<Align>,
    rows: Vec<Vec<String>>,
}

impl Table {
    pub fn new(headers: &[(&str, Align)]) -> Self {
        Table {
            headers: headers.iter().map(|(h, _)| (*h).to_string()).collect(),
            alignment: headers.iter().map(|(_, a)| *a).collect(),
            rows: Vec::new(),
        }
    }

    pub fn push(&mut self, cells: Vec<String>) {
        self.rows.push(cells);
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Render the header, then every row, with each column padded to the width
    /// its widest cell needs. Trailing whitespace is never emitted.
    ///
    /// The header carries the header style when the palette allows it, and a cell
    /// is measured by its *visible* width: one that carries a colour is measured
    /// without its escape codes, so styling a value can never push the columns
    /// apart.
    pub fn render(&self, pal: &Palette) -> String {
        let mut widths: Vec<usize> = self.headers.iter().map(|h| visible_width(h)).collect();
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                if i >= widths.len() {
                    break;
                }
                widths[i] = widths[i].max(visible_width(cell));
            }
        }

        let mut out = String::new();
        // Style the finished line rather than each cell: the padding then stays
        // out of the escape sequence, and the line is trimmed before it is
        // wrapped.
        out.push_str(&pal.header(&self.line(&self.headers, &widths)));
        out.push('\n');
        for row in &self.rows {
            out.push_str(&self.line(row, &widths));
            out.push('\n');
        }
        out
    }

    fn line(&self, cells: &[String], widths: &[usize]) -> String {
        let mut parts: Vec<String> = Vec::with_capacity(self.headers.len());
        for (i, width) in widths.iter().enumerate() {
            let cell = cells.get(i).map(String::as_str).unwrap_or("");
            let pad = width.saturating_sub(visible_width(cell));
            let blank = " ".repeat(pad);
            parts.push(match self.alignment.get(i) {
                Some(Align::Right) => format!("{blank}{cell}"),
                _ => format!("{cell}{blank}"),
            });
        }
        // Trim the padding that no later column needs.
        parts.join(SEPARATOR).trim_end().to_string()
    }
}

/// A token count at the scale it is read at.
///
/// Exact below a thousand, because a column reading "786" is worth more than one
/// reading "0.8K", and one decimal above it, which is as much as a table is read
/// at. Nine raw digits are both unreadable and wide enough to break the row.
pub fn tokens(n: u64) -> String {
    const THOUSAND: u64 = 1_000;
    const MILLION: u64 = 1_000_000;
    const BILLION: u64 = 1_000_000_000;

    let scaled = |value: u64, unit: u64| value as f64 / unit as f64;
    match n {
        0..THOUSAND => n.to_string(),
        THOUSAND..MILLION => format!("{:.1}K", scaled(n, THOUSAND)),
        MILLION..BILLION => format!("{:.1}M", scaled(n, MILLION)),
        _ => format!("{:.1}B", scaled(n, BILLION)),
    }
}

/// Money with as much precision as the number deserves.
///
/// One decimal above a dollar, which is what a total is read at -- the gateway
/// was printing 4.0157549641497491830835224971 USD, and everything past the first
/// decimal is noise. Below a dollar the same rule would print 0.0 for almost
/// every row, because the figures that matter here are per request and per million
/// tokens, so those get four decimals, and the genuinely tiny ones get six.
pub fn money(d: Decimal) -> String {
    // A hundredth of a cent. A single request can genuinely cost this little, and
    // rounding it away would print 0.0 for a row that spent money.
    const FINEST: Decimal = Decimal::from_parts(1, 0, 0, false, 4); // 0.0001

    let magnitude = d.abs();
    if magnitude >= Decimal::ONE {
        let rounded = d.round_dp(1).to_string();
        // round_dp does not keep a trailing zero on an integral value, and a money
        // column reading "15" beside one reading "4.0" looks like a change of unit.
        if rounded.contains('.') {
            rounded
        } else {
            format!("{rounded}.0")
        }
    } else if magnitude >= FINEST {
        d.round_dp(4).normalize().to_string()
    } else {
        // Eight places, not six: a per-request cost of 0.0000036 rounds to
        // 0.000004 at six, which is a ten percent error in a column that exists
        // to show exactly this kind of number.
        d.round_dp(8).normalize().to_string()
    }
}

/// Money with a currency suffix, so a column cannot be read as unitless.
pub fn money_with(d: Decimal, currency: &str) -> String {
    if currency.is_empty() {
        money(d)
    } else {
        format!("{} {currency}", money(d))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn t(rows: &[&[&str]], headers: &[(&str, Align)]) -> String {
        t_styled(rows, headers, Palette::off())
    }

    fn t_styled(rows: &[&[&str]], headers: &[(&str, Align)], pal: Palette) -> String {
        let mut table = Table::new(headers);
        for r in rows {
            table.push(r.iter().map(|c| (*c).to_string()).collect());
        }
        table.render(&pal)
    }

    /// The whole point: a cell wider than its header must not move the columns.
    #[test]
    fn a_wide_cell_widens_its_column_instead_of_pushing_the_rest() {
        let out = t(
            &[&["a", "1", "x"], &["a", "163931337", "x"]],
            &[("K", Align::Left), ("N", Align::Right), ("V", Align::Left)],
        );
        let lines: Vec<&str> = out.lines().collect();
        // The last column starts at the same offset on every line, header
        // included. This is exactly what the wide cell used to break.
        let third = lines[0].find('V').expect("header V");
        assert_eq!(lines[1].find('x'), Some(third), "row drifted: {out}");
        assert_eq!(lines[2].find('x'), Some(third), "wide row drifted: {out}");
    }

    /// Headers count towards the width too.
    #[test]
    fn a_header_wider_than_every_cell_sets_the_width() {
        let out = t(&[&["1"]], &[("REQUESTS", Align::Right)]);
        assert_eq!(out.lines().next().unwrap(), "REQUESTS");
        assert_eq!(out.lines().nth(1).unwrap(), "       1");
    }

    /// A row ending in a wide cell must not carry padding into the next line.
    #[test]
    fn trailing_padding_is_never_emitted() {
        let out = t(
            &[&["a", "b"], &["a", "a much longer value"]],
            &[("X", Align::Left), ("Y", Align::Left)],
        );
        for line in out.lines() {
            assert_eq!(line, line.trim_end(), "trailing space in {line:?}");
        }
    }

    /// A coloured cell is measured without its escapes.
    ///
    /// This is the whole reason colour is safe to add: if a table counted the
    /// escape codes, a styled value would be padded as if it were longer than it
    /// looks and every column after it would move.
    #[test]
    fn colour_does_not_change_a_column_position() {
        let pal = Palette::on();
        let mut table = Table::new(&[("K", Align::Left), ("V", Align::Left)]);
        table.push(vec!["a".to_string(), pal.green("ok")]);
        table.push(vec![
            "b".to_string(),
            pal.red("DISABLED (insufficient_credit)"),
        ]);
        let out = table.render(&pal);
        let lines: Vec<&str> = out.lines().collect();

        assert!(
            lines[0].starts_with("\x1b[1;36m"),
            "the header is a heading, not a table cell: {out:?}"
        );
        let visible_offset = |line: &str, needle: &str| {
            let at = line.find(needle).expect("the needle is on the line");
            visible_width(&line[..at])
        };
        assert_eq!(
            visible_offset(lines[1], "ok"),
            visible_offset(lines[2], "DISABLED"),
            "the value column moved: {out:?}"
        );
    }

    /// Ragged rows are a bug in the caller, not a panic in the renderer.
    #[test]
    fn a_short_row_pads_rather_than_panicking() {
        let out = t(&[&["only-one"]], &[("A", Align::Left), ("B", Align::Left)]);
        assert!(out.contains("only-one"));
    }

    #[test]
    fn tokens_are_exact_below_a_thousand_and_scaled_above() {
        assert_eq!(tokens(0), "0");
        assert_eq!(tokens(786), "786");
        assert_eq!(tokens(999), "999");
        assert_eq!(tokens(1_000), "1.0K");
        assert_eq!(tokens(921_417), "921.4K");
        assert_eq!(tokens(163_931_337), "163.9M");
        assert_eq!(tokens(162_019_968), "162.0M");
        assert_eq!(tokens(2_500_000_000), "2.5B");
    }

    /// The case that started this: a 30-digit cost.
    #[test]
    fn a_total_is_read_at_one_decimal() {
        let d = Decimal::from_str("4.0157549641497491830835224971").unwrap();
        assert_eq!(money(d), "4.0");
        assert_eq!(money(Decimal::from(15)), "15.0");
        assert_eq!(money(Decimal::from_str("1234.567").unwrap()), "1234.6");
    }

    /// Below a dollar the one-decimal rule would erase the number, so it does not
    /// apply there.
    #[test]
    fn small_amounts_keep_enough_precision_to_mean_something() {
        assert_eq!(money(Decimal::from_str("0.4595").unwrap()), "0.4595");
        assert_eq!(money(Decimal::from_str("0.008").unwrap()), "0.008");
        assert_eq!(money(Decimal::from_str("0.0000324").unwrap()), "0.0000324");
        assert_eq!(money(Decimal::from_str("0.0000036").unwrap()), "0.0000036");
        assert_ne!(money(Decimal::from_str("0.0000324").unwrap()), "0.0");
    }

    #[test]
    fn a_currency_suffix_is_carried_along() {
        assert_eq!(
            money_with(Decimal::from_str("4.0157").unwrap(), "USD"),
            "4.0 USD"
        );
        assert_eq!(money_with(Decimal::from(3), ""), "3.0");
    }
}
