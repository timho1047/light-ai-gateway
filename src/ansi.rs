//! ANSI colour, and the rules that keep it out of the way.
//!
//! Colour is a third channel next to position and wording, and it is used here
//! only for the few things an operator has to notice: an account that is not
//! active, a reading that is stale, a price that has drifted, a request that
//! spent real money. It never carries information on its own -- the same words
//! are printed with the colours off -- so a piped report, a log file, and a
//! terminal where the palette is unreadable all still say the same thing.
//!
//! Three rules keep it from turning into noise:
//!
//!   - it is decided once, from --color, NO_COLOR / CLICOLOR_FORCE and whether
//!     stdout is a terminal, so `lgw status | grep x` produces exactly the bytes
//!     it produced before colour existed;
//!   - a table measures the *visible* width of a cell, never the escape codes,
//!     so colouring a cell cannot push a column out of line;
//!   - the palette is handed to the renderers rather than read from a global, so
//!     a test renders the same table both ways and asserts on both.
//!
//! The palette is the 16-colour set every terminal has, and each style is a
//! separate escape with a reset after it: nesting styles into one sequence is
//! how a missing reset leaks colour into whatever is printed next.

use std::io::IsTerminal;

/// Whether the output is coloured at all.
///
/// A zero-sized switch rather than a set of booleans per colour: the choice is
/// one decision made at startup, and every renderer either has a palette or
/// does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    enabled: bool,
}

impl Palette {
    /// Print the plain text, byte for byte.
    pub const fn off() -> Self {
        Palette { enabled: false }
    }

    /// Colour unconditionally: what `--color always` means, and what the tests
    /// that assert on the escape codes use.
    pub const fn on() -> Self {
        Palette { enabled: true }
    }

    /// Decide from the environment and the destination.
    pub fn auto() -> Self {
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        let force = std::env::var_os("CLICOLOR_FORCE").is_some_and(|v| v != "0");
        let term = std::env::var("TERM").ok();
        Palette::decide(
            no_color,
            force,
            term.as_deref(),
            std::io::stdout().is_terminal(),
        )
    }

    /// The decision, as a function of its inputs, so every branch is tested
    /// without mutating the environment of a running test binary.
    ///
    /// CLICOLOR_FORCE wins over NO_COLOR: a terminal that cannot be detected
    /// (`docker run -t`, a CI that captures the stream, an editor's run panel)
    /// is exactly the case the force variable exists for, and asking for colour
    /// is more specific than asking for none.
    fn decide(no_color: bool, force: bool, term: Option<&str>, tty: bool) -> Self {
        if force {
            return Palette::on();
        }
        if no_color || term == Some("dumb") {
            return Palette::off();
        }
        Palette { enabled: tty }
    }

    pub const fn enabled(self) -> bool {
        self.enabled
    }

    fn paint(self, code: &str, text: &str) -> String {
        if self.enabled {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    /// A heading: a table's header row, the title of a section, a total.
    ///
    /// Cyan, because the other three colours mean something specific -- green is
    /// working, yellow is weakening, red is wrong -- and a heading must never be
    /// read as one of those. Bold as well, so the heading still looks like one
    /// for a reader who cannot see the colour.
    pub fn header(self, text: &str) -> String {
        self.paint("1;36", text)
    }

    /// Emphasis without colour: a line that must not be mistaken for its
    /// neighbours, in a report where colour is already spoken for.
    pub fn bold(self, text: &str) -> String {
        self.paint("1", text)
    }

    /// Legends and footnotes: present, but not part of the table.
    pub fn dim(self, text: &str) -> String {
        self.paint("2", text)
    }

    /// Money being spent, a problem found, a reading that is wrong.
    pub fn red(self, text: &str) -> String {
        self.paint("31", text)
    }

    /// Working as intended.
    pub fn green(self, text: &str) -> String {
        self.paint("32", text)
    }

    /// Working, but not for much longer, or a reading that is only estimated.
    pub fn yellow(self, text: &str) -> String {
        self.paint("33", text)
    }
}

/// The text as a terminal shows it, with the escape sequences removed.
///
/// Tests compare a coloured render against a plain one through this: if the
/// stripped text is not exactly what the palette-free run produced, then colour
/// is carrying information, which is the one thing it must never do.
pub fn strip(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// The width a terminal gives the text: escape sequences take no columns.
///
/// Every table in this program pads by this rather than by `chars().count()`, so
/// a cell can be coloured without becoming wider than it looks.
pub fn visible_width(text: &str) -> usize {
    let mut width = 0;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            // A CSI sequence ends at the first byte in @..~ .
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
            continue;
        }
        width += 1;
    }
    width
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_a_palette_the_text_is_returned_unchanged() {
        let p = Palette::off();
        assert_eq!(p.header("TOTAL"), "TOTAL");
        assert_eq!(p.bold("TOTAL"), "TOTAL");
        assert_eq!(p.red("DISABLED"), "DISABLED");
        assert_eq!(p.dim("covered = ..."), "covered = ...");
    }

    #[test]
    fn every_style_opens_and_closes() {
        let p = Palette::on();
        for styled in [
            p.header("x"),
            p.bold("x"),
            p.dim("x"),
            p.red("x"),
            p.green("x"),
            p.yellow("x"),
        ] {
            assert_eq!(visible_width(&styled), 1, "{styled:?}");
            assert!(styled.starts_with("\x1b["), "{styled:?}");
            assert!(styled.ends_with("\x1b[0m"), "{styled:?}");
        }
    }

    /// The reason visible_width exists: the escapes must not count as columns.
    #[test]
    fn escapes_are_not_columns() {
        let p = Palette::on();
        assert_eq!(visible_width("ok"), 2);
        assert_eq!(visible_width(&p.green("ok")), 2);
        assert_eq!(
            visible_width(&format!("{}{}", p.bold("HEADER"), p.dim("  legend"))),
            14
        );
        assert_eq!(visible_width(""), 0);
    }

    /// Stripping is how a test asks "is this the same report, only coloured?".
    #[test]
    fn stripping_leaves_the_text_the_reader_sees() {
        let p = Palette::on();
        assert_eq!(strip(&p.red("DISABLED")), "DISABLED");
        assert_eq!(
            strip(&format!("{}{}", p.bold("A  B"), p.dim(" C"))),
            "A  B C"
        );
        assert_eq!(strip("plain"), "plain");
    }

    #[test]
    fn the_decision_is_the_environment_it_was_given() {
        assert_eq!(
            Palette::decide(false, false, Some("xterm-256color"), true),
            Palette::on()
        );
        assert_eq!(
            Palette::decide(false, false, Some("xterm-256color"), false),
            Palette::off()
        );
        assert_eq!(
            Palette::decide(true, false, Some("xterm"), true),
            Palette::off()
        );
        assert_eq!(
            Palette::decide(false, false, Some("dumb"), true),
            Palette::off()
        );
        // FORCE wins: it is how a captured stream is told to keep its colours.
        assert_eq!(
            Palette::decide(true, true, Some("dumb"), false),
            Palette::on()
        );
    }
}
