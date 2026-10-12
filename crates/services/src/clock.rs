//! Wall-clock time for the bar.
//!
//! ## Why `chrono` and not a TZif parse
//!
//! A clock that shows UTC is a bug, and getting local time without a library
//! means reading `/etc/localtime` — a compiled TZif blob — and reimplementing
//! daylight-saving rules. `chrono` is already in the dependency graph (via
//! `zbus`), so this costs nothing new and gets the timezone right for
//! everyone.
//!
//! ## Formatting is here, not at the call site
//!
//! The bar needs "14:32", "14:32:07" and "Tue 14:32". Each of those is a
//! decision about the user's locale that belongs in one place with a test,
//! rather than in a format string three widgets deep.

use chrono::{DateTime, Datelike, Local, Timelike};

/// What the clock should show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// `14:32`
    Time,
    /// `14:32:07`
    TimeWithSeconds,
    /// `Tue 14:32`
    WeekdayTime,
}

impl Format {
    /// The string this produces for a given instant.
    ///
    /// 24-hour. The 12-hour form needs an AM/PM marker to be unambiguous, and
    /// a two-line bar cell is not worth it; the 24-hour form is what every
    /// other tiling shell's clock uses by default.
    pub fn render(&self, at: DateTime<Local>) -> String {
        match self {
            Format::Time => format!("{:02}:{:02}", at.hour(), at.minute()),
            Format::TimeWithSeconds => format!(
                "{:02}:{:02}:{:02}",
                at.hour(),
                at.minute(),
                at.second()
            ),
            Format::WeekdayTime => {
                // The first byte of the full name: "Monday" -> 'M'. Avoids
                // matching on a translated abbreviation table.
                let weekday = at.weekday();
                let letter = weekday
                    .to_string()
                    .chars()
                    .next()
                    .map(|c| c.to_ascii_uppercase())
                    .unwrap_or('?');
                format!("{letter} {:02}:{:02}", at.hour(), at.minute())
            }
        }
    }
}

/// Local time now.
pub fn now() -> DateTime<Local> {
    Local::now()
}

/// The bar's format, from `show_seconds`.
pub fn format(show_seconds: bool) -> Format {
    if show_seconds {
        Format::TimeWithSeconds
    } else {
        Format::Time
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(y: i32, m: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, m, d, h, mi, s).unwrap()
    }

    #[test]
    fn time_is_zero_padded_24_hour() {
        let f = Format::Time;
        assert_eq!(f.render(at(2026, 10, 12, 9, 5, 0)), "09:05");
        assert_eq!(f.render(at(2026, 10, 12, 14, 32, 7)), "14:32");
        assert_eq!(f.render(at(2026, 10, 12, 23, 59, 59)), "23:59");
    }

    /// Midnight reading "00:00" and not "24:00" is the whole reason this is
    /// zero-padded rather than trimmed.
    #[test]
    fn midnight_is_zero_not_twenty_four() {
        assert_eq!(Format::Time.render(at(2026, 10, 12, 0, 0, 0)), "00:00");
    }

    #[test]
    fn seconds_are_appended_when_asked_for() {
        assert_eq!(
            Format::TimeWithSeconds.render(at(2026, 10, 12, 14, 32, 7)),
            "14:32:07"
        );
    }

    #[test]
    fn weekday_is_a_single_letter() {
        // 2026-10-12 is a Monday.
        assert_eq!(Format::WeekdayTime.render(at(2026, 10, 12, 14, 32, 0)), "M 14:32");
    }

    #[test]
    fn show_seconds_selects_the_format() {
        assert_eq!(format(false), Format::Time);
        assert_eq!(format(true), Format::TimeWithSeconds);
    }
}
