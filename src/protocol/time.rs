//! Calendar date-times as recorded-video messages carry them.
//!
//! The device writes a time as separate year/month/day/hour/minute/second
//! fields with no time zone. Which zone it is in depends on where it came
//! from: the XML of the calendar, file search, seek and download messages is
//! in the device's local time, while the 32-byte info block at the start of
//! every playback carries the file's start and end in UTC. The type itself
//! does not know; the field or function that hands one out says which.

use serde::{Deserialize, Serialize};

/// A calendar date and time of day with no time zone attached.
///
/// As XML it is the `<year>`, `<month>`, `<day>`, `<hour>`, `<minute>` and
/// `<second>` elements, written without leading zeros. `month` and `day`
/// are 1-based. Missing elements read as 0. The derived ordering is
/// chronological, since the fields are compared from year down to second.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(default)]
pub struct CivilTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

impl CivilTime {
    pub const fn new(year: u16, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> Self {
        CivilTime { year, month, day, hour, minute, second }
    }

    /// Midnight at the start of the given day.
    pub const fn start_of_day(year: u16, month: u8, day: u8) -> Self {
        CivilTime::new(year, month, day, 0, 0, 0)
    }

    /// The last second (23:59:59) of the given day: the end a whole-day
    /// search or a month calendar asks for.
    pub const fn end_of_day(year: u16, month: u8, day: u8) -> Self {
        CivilTime::new(year, month, day, 23, 59, 59)
    }

    /// Number of days in `month` (1–12) of `year`; 0 for a month outside
    /// that range.
    pub fn days_in_month(year: u16, month: u8) -> u8 {
        match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if is_leap_year(year) => 29,
            2 => 28,
            _ => 0,
        }
    }

    /// Whether every field is within its calendar range (no leap seconds).
    pub fn is_valid(&self) -> bool {
        (1..=12).contains(&self.month)
            && self.day >= 1
            && self.day <= Self::days_in_month(self.year, self.month)
            && self.hour < 24
            && self.minute < 60
            && self.second < 60
    }

    /// Seconds since 1970-01-01 00:00:00 in the same (unnamed) zone as
    /// `self`. For a UTC time this is the Unix time; for a device-local time
    /// it is only good for differences and ordering.
    pub fn to_epoch_seconds(&self) -> i64 {
        let days = days_from_civil(i64::from(self.year), i64::from(self.month), i64::from(self.day));
        days * 86_400 + i64::from(self.hour) * 3_600 + i64::from(self.minute) * 60 + i64::from(self.second)
    }

    /// The inverse of [`CivilTime::to_epoch_seconds`]. Years outside
    /// 0..=65535 are clamped.
    pub fn from_epoch_seconds(seconds: i64) -> Self {
        let days = seconds.div_euclid(86_400);
        let in_day = seconds.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        CivilTime {
            year: year.clamp(0, i64::from(u16::MAX)) as u16,
            month,
            day,
            hour: (in_day / 3_600) as u8,
            minute: (in_day % 3_600 / 60) as u8,
            second: (in_day % 60) as u8,
        }
    }

    /// `self` moved by `seconds` (negative to go back), across day, month
    /// and year boundaries.
    pub fn add_seconds(&self, seconds: i64) -> Self {
        Self::from_epoch_seconds(self.to_epoch_seconds() + seconds)
    }
}

fn is_leap_year(year: u16) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Days from 1970-01-01 to the given proleptic Gregorian date. The year is
/// counted from March so that a leap day, if any, is the last day of the
/// counting year; 400-year cycles of 146097 days do the rest.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let cycle = year.div_euclid(400);
    let year_of_cycle = year - cycle * 400;
    let month_from_march = (month + 9) % 12;
    let day_of_year = (153 * month_from_march + 2) / 5 + day - 1;
    let day_of_cycle = year_of_cycle * 365 + year_of_cycle / 4 - year_of_cycle / 100 + day_of_year;
    // 719468 = days from 0000-03-01 to 1970-01-01.
    cycle * 146_097 + day_of_cycle - 719_468
}

/// The inverse of [`days_from_civil`]: (year, month, day).
fn civil_from_days(days: i64) -> (i64, u8, u8) {
    let days = days + 719_468;
    let cycle = days.div_euclid(146_097);
    let day_of_cycle = days - cycle * 146_097;
    let year_of_cycle =
        (day_of_cycle - day_of_cycle / 1_460 + day_of_cycle / 36_524 - day_of_cycle / 146_096) / 365;
    let day_of_year = day_of_cycle - (365 * year_of_cycle + year_of_cycle / 4 - year_of_cycle / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 { month_from_march + 3 } else { month_from_march - 9 };
    let year = year_of_cycle + cycle * 400 + i64::from(month <= 2);
    (year, month as u8, day as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_seconds_match_known_unix_times() {
        assert_eq!(CivilTime::new(1970, 1, 1, 0, 0, 0).to_epoch_seconds(), 0);
        assert_eq!(CivilTime::new(2000, 3, 1, 0, 0, 0).to_epoch_seconds(), 951_868_800);
        // The replay sequence number seen on the wire is a Unix time:
        // 1792348477 is 2026-10-18 18:34:37 UTC.
        assert_eq!(CivilTime::from_epoch_seconds(1_792_348_477), CivilTime::new(2026, 10, 18, 18, 34, 37));
        assert_eq!(CivilTime::new(2026, 10, 18, 18, 34, 37).to_epoch_seconds(), 1_792_348_477);
    }

    #[test]
    fn round_trips_across_leap_days_and_year_ends() {
        for t in [
            CivilTime::new(2024, 2, 29, 23, 59, 59),
            CivilTime::new(2026, 12, 31, 23, 59, 59),
            CivilTime::new(2100, 3, 1, 0, 0, 0),
            CivilTime::new(1999, 12, 31, 12, 0, 1),
        ] {
            assert_eq!(CivilTime::from_epoch_seconds(t.to_epoch_seconds()), t);
        }
        assert_eq!(
            CivilTime::new(2026, 12, 31, 23, 59, 59).add_seconds(1),
            CivilTime::new(2027, 1, 1, 0, 0, 0)
        );
        assert_eq!(CivilTime::new(2024, 3, 1, 0, 0, 0).add_seconds(-1), CivilTime::new(2024, 2, 29, 23, 59, 59));
    }

    #[test]
    fn validity_and_month_lengths() {
        assert!(CivilTime::new(2026, 10, 17, 9, 21, 7).is_valid());
        assert!(!CivilTime::default().is_valid());
        assert!(!CivilTime::new(2026, 2, 29, 0, 0, 0).is_valid());
        assert!(CivilTime::new(2028, 2, 29, 0, 0, 0).is_valid());
        assert!(!CivilTime::new(2026, 10, 17, 24, 0, 0).is_valid());
        assert_eq!(CivilTime::days_in_month(2026, 9), 30);
        assert_eq!(CivilTime::days_in_month(2026, 13), 0);
    }

    #[test]
    fn orders_chronologically() {
        let a = CivilTime::new(2026, 10, 17, 9, 21, 8);
        let b = CivilTime::new(2026, 10, 17, 10, 0, 0);
        let c = CivilTime::new(2026, 11, 1, 0, 0, 0);
        assert!(a < b && b < c);
    }
}
