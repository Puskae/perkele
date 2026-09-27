//! Strict parsing for the two date shapes that cross the API: `YYYY-MM-DD`
//! dates (chores, meal plan) and floating `YYYY-MM-DDTHH:MM:SSZ` timestamps
//! (calendar). Every validator that accepts one goes through here so the
//! client and server agree on exactly one format and one year range.
//!
//! Why the year bound: recurrence expansion iterates from the series start,
//! so a start in year 0001 makes every render / reminder scan walk ~2000
//! years of occurrences. 1900–2200 is far wider than any family calendar
//! needs while keeping that walk bounded.

use chrono::{Datelike as _, NaiveDate, NaiveDateTime};

/// Earliest year any stored date may have (inclusive).
pub const MIN_YEAR: i32 = 1900;
/// Latest year any stored date may have (inclusive).
pub const MAX_YEAR: i32 = 2200;

/// Byte-exact shape check: `d` in `pattern` means "an ASCII digit", any other
/// byte must match literally. chrono's `%Y`/`%m` alone would also accept
/// unpadded or signed numbers, which break lexical (string) date ordering.
fn has_shape(s: &str, pattern: &str) -> bool {
    s.len() == pattern.len()
        && s.bytes().zip(pattern.bytes()).all(|(c, p)| match p {
            b'd' => c.is_ascii_digit(),
            lit => c == lit,
        })
}

fn year_in_range(year: i32) -> bool {
    (MIN_YEAR..=MAX_YEAR).contains(&year)
}

/// Parse an exact `YYYY-MM-DD` date that is a real calendar day within
/// [`MIN_YEAR`]..=[`MAX_YEAR`].
pub fn parse_date(s: &str) -> Option<NaiveDate> {
    if !has_shape(s, "dddd-dd-dd") {
        return None;
    }
    let d = NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()?;
    // `bool::then_some(v)` = `if b { Some(v) } else { None }`.
    year_in_range(d.year()).then_some(d)
}

/// Parse an exact floating `YYYY-MM-DDTHH:MM:SSZ` timestamp (the one form
/// the calendar stores) within [`MIN_YEAR`]..=[`MAX_YEAR`].
pub fn parse_timestamp(s: &str) -> Option<NaiveDateTime> {
    if !has_shape(s, "dddd-dd-ddTdd:dd:ddZ") {
        return None;
    }
    let t = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%SZ").ok()?;
    year_in_range(t.year()).then_some(t)
}

pub fn validate_date(s: &str) -> Result<(), &'static str> {
    parse_date(s).map(|_| ()).ok_or("Virheellinen päivä.")
}

pub fn validate_timestamp(s: &str) -> Result<(), &'static str> {
    parse_timestamp(s).map(|_| ()).ok_or("Virheellinen aika.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_accept_real_days_in_range() {
        assert!(validate_date("2026-07-01").is_ok());
        assert!(validate_date("2024-02-29").is_ok()); // leap day
        assert!(validate_date("1900-01-01").is_ok());
        assert!(validate_date("2200-12-31").is_ok());
    }

    #[test]
    fn dates_reject_bad_shape_bad_day_and_out_of_range() {
        for bad in [
            "",
            "2026-7-01",
            "2026-07-1",
            "+026-07-01",
            "2026-07-01 ",
            "2026-13-01",
            "2026-02-30",
            "2025-02-29",
            "0001-01-01",
            "1899-12-31",
            "2201-01-01",
            "9999-12-31",
            "2026-07-01T00:00:00Z",
            "äää-07-01",
        ] {
            assert!(validate_date(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn timestamps_exact_format_and_range() {
        assert!(validate_timestamp("2026-07-01T10:30:00Z").is_ok());
        assert!(validate_timestamp("2026-07-01T00:00:00Z").is_ok());
        for bad in [
            "0001-01-01T00:00:00Z",
            "1899-12-31T23:59:59Z",
            "2201-01-01T00:00:00Z",
            "2026-07-01T10:30:00",
            "2026-07-01T10:30:00+02:00",
            "2026-07-01T10:30:00.000Z",
            "2026-07-01T10:30Z",
            "2026-07-01T25:00:00Z",
            "2026-02-30T10:00:00Z",
            ":00Z",
            "2026-07-01",
        ] {
            assert!(
                validate_timestamp(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }
}
