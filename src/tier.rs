//! Time-window price tiers.
//!
//! FILE OWNER: Lead.
//!
//! Some providers charge different rates by time of day: Command Code bills
//! deepseek-v4.1-flash at double the rate Mon-Fri 01:00-04:00 and 06:00-10:00
//! UTC, and the off-peak rate at every other hour. A gateway that assumes one
//! price for a model is then wrong by a factor of two for part of every day,
//! and wrong in the direction that spends money when it underestimates.
//!
//! Everything here is a pure function of a UTC instant, so the boundaries are
//! testable without a clock. The boundaries are where the bugs are: a range is
//! half-open, the weekday is evaluated in the schedule's own timezone, and a
//! range that wraps past midnight belongs to the day it STARTED on.

use anyhow::{Context, Result, bail};
use time::{OffsetDateTime, UtcOffset, Weekday};

/// A set of weekdays, as a bitmask with Monday as the low bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Days(u8);

impl Days {
    pub const fn all() -> Self {
        Days(0b111_1111)
    }

    pub fn contains(self, day: Weekday) -> bool {
        let bit = match day {
            Weekday::Monday => 0,
            Weekday::Tuesday => 1,
            Weekday::Wednesday => 2,
            Weekday::Thursday => 3,
            Weekday::Friday => 4,
            Weekday::Saturday => 5,
            Weekday::Sunday => 6,
        };
        self.0 & (1 << bit) != 0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// A half-open interval of the day, in minutes since local midnight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HourRange {
    pub start_min: u16,
    pub end_min: u16,
}

impl HourRange {
    /// A range that crosses midnight on its way to the end.
    pub fn wraps(&self) -> bool {
        self.start_min > self.end_min
    }

    /// Is this minute inside, counting from the day the range started?
    fn contains(&self, minute_of_day: u16, day_offset_minutes: i32) -> bool {
        let m = i32::from(minute_of_day) + day_offset_minutes;
        m >= i32::from(self.start_min) && m < i32::from(self.end_min)
    }
}

/// One priced tier with the window it applies to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tier {
    pub name: String,
    pub days: Days,
    pub hours: Vec<HourRange>,
    /// Offset from UTC in minutes. 0 is UTC.
    pub offset_minutes: i32,
}

/// The tier set for one model. The default price applies whenever no window
/// here matches.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Schedule {
    pub tiers: Vec<Tier>,
}

impl Schedule {
    pub fn is_empty(&self) -> bool {
        self.tiers.is_empty()
    }

    /// Which tier prices a request starting at this instant.
    ///
    /// Returns the index into tiers, or None for the default price. Tiers are
    /// checked in order so the first match wins, which makes overlapping
    /// windows behave predictably instead of arbitrarily.
    pub fn tier_index_at(&self, at: OffsetDateTime) -> Option<usize> {
        for (i, tier) in self.tiers.iter().enumerate() {
            let offset = UtcOffset::from_whole_seconds(tier.offset_minutes * 60).ok()?;
            let local = at.to_offset(offset);
            let minute_of_day = u16::from(local.hour()) * 60 + u16::from(local.minute());

            for range in &tier.hours {
                if !range.wraps() {
                    if tier.days.contains(local.weekday()) && range.contains(minute_of_day, 0) {
                        return Some(i);
                    }
                    continue;
                }
                if minute_of_day >= range.start_min && tier.days.contains(local.weekday()) {
                    return Some(i);
                }
                if minute_of_day < range.end_min && tier.days.contains(local.weekday().previous()) {
                    return Some(i);
                }
            }
        }
        None
    }
}

/// Parse "mon-fri", "sat,sun", "mon,wed,fri" or "*".
pub fn parse_days(input: &str) -> Result<Days> {
    let text = input.trim();
    if text.is_empty() {
        bail!("days must not be empty");
    }
    if text == "*" {
        return Ok(Days::all());
    }

    let mut phrase = text.to_ascii_lowercase().replace(' ', "");
    if phrase.contains("mon-fri") {
        phrase = phrase.replace("mon-fri", "mon,tue,wed,thu,fri");
    }
    if phrase.contains("sat-sun") {
        phrase = phrase.replace("sat-sun", "sat,sun");
    }

    let mut mask = 0u8;
    for part in phrase.split(',') {
        if part.is_empty() {
            continue;
        }
        let bit = match part {
            "mon" | "monday" => 0,
            "tue" | "tues" | "tuesday" => 1,
            "wed" | "wednesday" => 2,
            "thu" | "thur" | "thurs" | "thursday" => 3,
            "fri" | "friday" => 4,
            "sat" | "saturday" => 5,
            "sun" | "sunday" => 6,
            other => bail!("unknown day {other:?}; use mon,tue,wed,thu,fri,sat,sun or *"),
        };
        mask |= 1 << bit;
    }
    let days = Days(mask);
    if days.is_empty() {
        bail!("days {text:?} matched no weekdays");
    }
    Ok(days)
}

/// Parse "01:00-04:00,06:00-10:00".
pub fn parse_hours(input: &str) -> Result<Vec<HourRange>> {
    let mut out = Vec::new();
    for part in input.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (start, end) = part
            .split_once('-')
            .with_context(|| format!("hour range {part:?} must look like 01:00-04:00"))?;
        let start_min = parse_clock(start).with_context(|| format!("in {part:?}"))?;
        let end_min = parse_clock(end).with_context(|| format!("in {part:?}"))?;
        if start_min == end_min {
            bail!("hour range {part:?} is empty; start and end are the same");
        }
        out.push(HourRange { start_min, end_min });
    }
    if out.is_empty() {
        bail!("hours {input:?} contained no ranges");
    }
    Ok(out)
}

fn parse_clock(text: &str) -> Result<u16> {
    let text = text.trim();
    let (h, m) = text
        .split_once(':')
        .with_context(|| format!("time {text:?} must look like HH:MM"))?;
    let hour: u16 = h.trim().parse().context("hour must be a number")?;
    let minute: u16 = m.trim().parse().context("minute must be a number")?;
    if hour > 23 || minute > 59 {
        bail!("time {text:?} is out of range");
    }
    Ok(hour * 60 + minute)
}

/// Parse a timezone: "UTC", "+08:00", "-05:00", or a bare offset in hours.
///
/// Deliberately limited to UTC and fixed offsets. Providers state their windows
/// this way, and supporting named zones would mean carrying a timezone database
/// for a value that shifts twice a year. A named zone is rejected with a message
/// saying what to use instead, rather than silently mispricing.
pub fn parse_tz(input: &str) -> Result<i32> {
    let text = input.trim();
    if text.eq_ignore_ascii_case("utc") || text.eq_ignore_ascii_case("z") || text == "+00:00" {
        return Ok(0);
    }
    let (sign, rest) = match text.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, text.strip_prefix('+').unwrap_or(text)),
    };
    let (h, m) = match rest.split_once(':') {
        Some((h, m)) => (h, m),
        None => (rest, "0"),
    };
    let hours: i32 = h.trim().parse().with_context(|| {
        format!("timezone {input:?} is not UTC, a fixed offset like +08:00, or a bare offset")
    })?;
    let minutes: i32 = m
        .trim()
        .parse()
        .context("timezone minutes must be a number")?;
    if hours > 23 || minutes > 59 {
        bail!("timezone {input:?} is out of range");
    }
    Ok(sign * (hours * 60 + minutes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    /// The real Command Code window: peak Mon-Fri 01:00-04:00 and 06:00-10:00 UTC.
    fn commandcode() -> Schedule {
        Schedule {
            tiers: vec![Tier {
                name: "peak".into(),
                days: parse_days("mon-fri").unwrap(),
                hours: parse_hours("01:00-04:00,06:00-10:00").unwrap(),
                offset_minutes: 0,
            }],
        }
    }

    fn peak_at(s: &Schedule, at: OffsetDateTime) -> bool {
        s.tier_index_at(at).is_some()
    }

    #[test]
    fn the_real_command_code_window_is_applied_hour_by_hour() {
        let s = commandcode();
        let expected: [(u8, bool); 24] = [
            (0, false),
            (1, true),
            (2, true),
            (3, true),
            (4, false),
            (5, false),
            (6, true),
            (7, true),
            (8, true),
            (9, true),
            (10, false),
            (11, false),
            (12, false),
            (13, false),
            (14, false),
            (15, false),
            (16, false),
            (17, false),
            (18, false),
            (19, false),
            (20, false),
            (21, false),
            (22, false),
            (23, false),
        ];
        for (hour, want) in expected {
            let at = datetime!(2026-09-09 0:00 UTC) + time::Duration::hours(i64::from(hour));
            assert_eq!(peak_at(&s, at), want, "hour {hour:02}:00 UTC");
        }
    }

    /// The boundary is the bug-prone part. A range is half-open: 01:00 is peak
    /// and 04:00 is not, so no hour is counted twice and none is missed.
    #[test]
    fn range_endpoints_are_half_open() {
        let s = commandcode();
        let base = datetime!(2026-09-09 0:00 UTC);
        assert!(!peak_at(&s, base + time::Duration::minutes(59)), "00:59");
        assert!(
            peak_at(&s, base + time::Duration::hours(1)),
            "01:00 exactly"
        );
        assert!(
            peak_at(
                &s,
                base + time::Duration::hours(3) + time::Duration::minutes(59)
            ),
            "03:59"
        );
        assert!(
            !peak_at(&s, base + time::Duration::hours(4)),
            "04:00 exactly"
        );
    }

    #[test]
    fn weekends_are_never_peak() {
        let s = commandcode();
        for day in [12u8, 13u8] {
            let at = datetime!(2026-09-01 0:00 UTC)
                + time::Duration::days(i64::from(day) - 1)
                + time::Duration::hours(2);
            assert!(!peak_at(&s, at), "day {day} at 02:00 UTC must be off-peak");
        }
    }

    /// The window is stated in UTC and must be evaluated in UTC, not in the
    /// machine's local zone. 02:00 UTC is peak even for a UTC+8 operator, whose
    /// local clock reads 10:00.
    #[test]
    fn the_schedule_is_evaluated_in_its_own_timezone() {
        let s = commandcode();
        assert!(peak_at(&s, datetime!(2026-09-09 2:00 UTC)));
    }

    #[test]
    fn a_fixed_offset_shifts_the_window() {
        let s = Schedule {
            tiers: vec![Tier {
                name: "peak".into(),
                days: parse_days("mon-fri").unwrap(),
                hours: parse_hours("09:00-12:00").unwrap(),
                offset_minutes: 8 * 60,
            }],
        };
        assert!(peak_at(&s, datetime!(2026-09-09 1:00 UTC)));
        assert!(!peak_at(&s, datetime!(2026-09-09 0:59 UTC)));
    }

    /// A range crossing midnight belongs to the day it STARTED on, so a Friday
    /// 23:00-02:00 window still covers Saturday 01:00, while Sunday night does
    /// not belong to a Monday window.
    #[test]
    fn a_wrapping_range_belongs_to_the_day_it_started() {
        let s = Schedule {
            tiers: vec![Tier {
                name: "night".into(),
                days: parse_days("fri").unwrap(),
                hours: parse_hours("23:00-02:00").unwrap(),
                offset_minutes: 0,
            }],
        };
        assert!(peak_at(&s, datetime!(2026-09-11 23:30 UTC)), "Friday 23:30");
        assert!(
            peak_at(&s, datetime!(2026-09-12 1:00 UTC)),
            "Saturday 01:00"
        );
        assert!(
            !peak_at(&s, datetime!(2026-09-12 2:00 UTC)),
            "Saturday 02:00"
        );
        assert!(!peak_at(&s, datetime!(2026-09-13 1:00 UTC)), "Sunday 01:00");
    }

    #[test]
    fn the_default_tier_is_used_when_nothing_matches() {
        let empty = Schedule::default();
        assert_eq!(empty.tier_index_at(datetime!(2026-09-09 2:00 UTC)), None);
        assert!(empty.is_empty());
    }

    #[test]
    fn the_first_matching_tier_wins() {
        let s = Schedule {
            tiers: vec![
                Tier {
                    name: "specific".into(),
                    days: parse_days("wed").unwrap(),
                    hours: parse_hours("01:00-02:00").unwrap(),
                    offset_minutes: 0,
                },
                Tier {
                    name: "broad".into(),
                    days: parse_days("mon-fri").unwrap(),
                    hours: parse_hours("01:00-04:00").unwrap(),
                    offset_minutes: 0,
                },
            ],
        };
        assert_eq!(s.tier_index_at(datetime!(2026-09-09 1:30 UTC)), Some(0));
        assert_eq!(s.tier_index_at(datetime!(2026-09-09 3:00 UTC)), Some(1));
    }

    #[test]
    fn days_parse_the_way_a_pricing_page_writes_them() {
        assert!(parse_days("mon-fri").unwrap().contains(Weekday::Monday));
        assert!(parse_days("mon-fri").unwrap().contains(Weekday::Friday));
        assert!(!parse_days("mon-fri").unwrap().contains(Weekday::Saturday));
        assert_eq!(parse_days("*").unwrap(), Days::all());
        assert!(parse_days("sat,sun").unwrap().contains(Weekday::Sunday));
        assert!(
            parse_days("WEDNESDAY")
                .unwrap()
                .contains(Weekday::Wednesday)
        );
    }

    #[test]
    fn a_malformed_schedule_is_refused_rather_than_mispriced() {
        assert!(parse_days("").is_err());
        assert!(parse_days("funday").is_err());
        assert!(parse_hours("").is_err());
        assert!(parse_hours("01:00").is_err());
        assert!(parse_hours("01:00-01:00").is_err(), "an empty window");
        assert!(parse_hours("25:00-26:00").is_err());
        assert!(
            parse_tz("Asia/Shanghai").is_err(),
            "named zones are rejected"
        );
        assert!(parse_tz("+99:00").is_err());
    }

    #[test]
    fn timezones_parse_to_whole_minute_offsets() {
        assert_eq!(parse_tz("UTC").unwrap(), 0);
        assert_eq!(parse_tz("utc").unwrap(), 0);
        assert_eq!(parse_tz("+08:00").unwrap(), 480);
        assert_eq!(parse_tz("-05:00").unwrap(), -300);
        assert_eq!(parse_tz("8").unwrap(), 480);
    }
}
