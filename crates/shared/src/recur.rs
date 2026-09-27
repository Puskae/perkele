//! Recurrence engine (Phase 5B) — the ONE place RRULE strings are produced,
//! parsed, validated and expanded. Runs in both the browser (rendering) and
//! the server (validation now; chore/reminder scanning later).
//!
//! Design:
//! - The UI edits a [`RecurrenceSpec`] — it never hand-writes RRULE grammar.
//! - Storage holds the RRULE string on the master event row.
//! - Time model is floating wall-clock `YYYY-MM-DDTHH:MM:SSZ`: we feed the
//!   strings to the `rrule` crate as UTC and format occurrences back the same
//!   way. No timezone conversion anywhere.

use chrono::NaiveDateTime;
use chrono::TimeZone as _; // trait import: brings `Tz::from_utc_datetime` into scope
use rrule::{Frequency, NWeekday, RRule, Tz, Unvalidated};

/// How often the event repeats. Maps 1:1 to RRULE `FREQ=`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freq {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

impl Freq {
    fn as_str(self) -> &'static str {
        match self {
            Freq::Daily => "DAILY",
            Freq::Weekly => "WEEKLY",
            Freq::Monthly => "MONTHLY",
            Freq::Yearly => "YEARLY",
        }
    }

    /// The rrule crate also knows HOURLY/MINUTELY/SECONDLY — sub-daily
    /// repeats make no sense for a family calendar, so they map to `None`.
    fn from_frequency(freq: Frequency) -> Option<Self> {
        match freq {
            Frequency::Daily => Some(Freq::Daily),
            Frequency::Weekly => Some(Freq::Weekly),
            Frequency::Monthly => Some(Freq::Monthly),
            Frequency::Yearly => Some(Freq::Yearly),
            _ => None,
        }
    }
}

/// Day-of-week for the weekly `BYDAY` picker. Our own enum (not chrono's)
/// so the UI layer doesn't need a chrono dependency to render checkboxes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Weekday {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

impl Weekday {
    /// RFC 5545 two-letter day code, as used in `BYDAY=MO,WE`.
    fn code(self) -> &'static str {
        match self {
            Weekday::Mon => "MO",
            Weekday::Tue => "TU",
            Weekday::Wed => "WE",
            Weekday::Thu => "TH",
            Weekday::Fri => "FR",
            Weekday::Sat => "SA",
            Weekday::Sun => "SU",
        }
    }

    fn from_chrono(day: chrono::Weekday) -> Self {
        match day {
            chrono::Weekday::Mon => Weekday::Mon,
            chrono::Weekday::Tue => Weekday::Tue,
            chrono::Weekday::Wed => Weekday::Wed,
            chrono::Weekday::Thu => Weekday::Thu,
            chrono::Weekday::Fri => Weekday::Fri,
            chrono::Weekday::Sat => Weekday::Sat,
            chrono::Weekday::Sun => Weekday::Sun,
        }
    }
}

/// When the series stops. `OnDate` carries a plain `YYYY-MM-DD` date (what a
/// date input produces); it becomes `UNTIL=<date>T235959Z` so the last day is
/// inclusive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum End {
    Never,
    OnDate(String),
    AfterCount(u32),
}

/// Everything the recurrence editor can express. This is deliberately a
/// subset of RRULE — patterns outside it (BYMONTHDAY, BYSETPOS, …) are
/// rejected by [`validate`] and round-trip to `None` in [`from_rrule`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecurrenceSpec {
    pub freq: Freq,
    /// "every N days/weeks/months/years"; 1 = every.
    pub interval: u32,
    /// Weekly only: which weekdays (RRULE `BYDAY`). Empty = the weekday of
    /// the event's start.
    pub byday: Vec<Weekday>,
    pub end: End,
}

impl RecurrenceSpec {
    /// Serialize to the RRULE content line we store, e.g.
    /// `FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE`. `INTERVAL=1` is omitted (it is
    /// the RFC default).
    pub fn to_rrule(&self) -> String {
        let mut parts = vec![format!("FREQ={}", self.freq.as_str())];
        if self.interval > 1 {
            parts.push(format!("INTERVAL={}", self.interval));
        }
        if !self.byday.is_empty() {
            let days: Vec<&str> = self.byday.iter().map(|d| d.code()).collect();
            parts.push(format!("BYDAY={}", days.join(",")));
        }
        match &self.end {
            End::Never => {}
            // "2027-06-30" -> "UNTIL=20270630T235959Z": end of that day, so
            // an occurrence ON the chosen date is still included.
            End::OnDate(date) => parts.push(format!("UNTIL={}T235959Z", date.replace('-', ""))),
            End::AfterCount(n) => parts.push(format!("COUNT={n}")),
        }
        parts.join(";")
    }

    /// Parse a stored RRULE back into the editable spec. Returns `None` for
    /// anything our editor can't represent (so the UI can fall back to a
    /// read-only "custom rule" label instead of silently mangling it).
    pub fn from_rrule(rrule: &str) -> Option<Self> {
        // Parse WITHOUT validating: `validate()` normalizes the rule against
        // a DTSTART (e.g. fills in BYDAY from the start's weekday), which
        // would break exact round-tripping. `RRule<Unvalidated>` is the raw,
        // as-written rule.
        let rule: RRule<Unvalidated> = rrule.parse().ok()?;
        let freq = Freq::from_frequency(rule.get_freq())?;

        // Anything the editor has no widget for makes the whole rule
        // unrepresentable — better to refuse than to silently drop parts.
        let unsupported = !rule.get_by_set_pos().is_empty()
            || !rule.get_by_month().is_empty()
            || !rule.get_by_month_day().is_empty()
            || !rule.get_by_year_day().is_empty()
            || !rule.get_by_week_no().is_empty()
            || !rule.get_by_hour().is_empty()
            || !rule.get_by_minute().is_empty()
            || !rule.get_by_second().is_empty()
            || rule.get_week_start() != chrono::Weekday::Mon;
        if unsupported {
            return None;
        }

        let mut byday = Vec::new();
        for day in rule.get_by_weekday() {
            match day {
                NWeekday::Every(w) => byday.push(Weekday::from_chrono(*w)),
                // "-1FR" (last Friday) is out of scope.
                NWeekday::Nth(..) => return None,
            }
        }
        // The weekday picker only exists for weekly repeats.
        if !byday.is_empty() && freq != Freq::Weekly {
            return None;
        }

        let end = match (rule.get_count(), rule.get_until()) {
            // RFC 5545 forbids COUNT and UNTIL together.
            (Some(_), Some(_)) => return None,
            (Some(n), None) => End::AfterCount(n),
            (None, Some(until)) => End::OnDate(until.format("%Y-%m-%d").to_string()),
            (None, None) => End::Never,
        };

        Some(RecurrenceSpec {
            freq,
            interval: u32::from(rule.get_interval()),
            byday,
            end,
        })
    }
}

/// Expand a master event's rule into occurrence start datetimes inside the
/// window `[window_start, window_end]` (inclusive), capped at `cap` results
/// to keep a bad rule from freezing the browser. All datetimes are floating
/// `YYYY-MM-DDTHH:MM:SSZ` strings. Invalid input yields an empty Vec — the
/// write path ([`validate`]) is where errors are surfaced.
pub fn expand(
    rrule: &str,
    dtstart: &str,
    window_start: &str,
    window_end: &str,
    cap: u16,
) -> Vec<String> {
    // let-else: bail out with an empty Vec on any malformed input.
    let (Some(start), Some(win_start), Some(win_end)) = (
        parse_floating(dtstart),
        parse_floating(window_start),
        parse_floating(window_end),
    ) else {
        return Vec::new();
    };
    let Ok(rule) = rrule.parse::<RRule<Unvalidated>>() else {
        return Vec::new();
    };
    // build() validates against DTSTART and yields an RRuleSet iterator.
    let Ok(set) = rule.build(start) else {
        return Vec::new();
    };
    // after/before are both INCLUSIVE bounds; all(cap) hard-limits the count.
    set.after(win_start)
        .before(win_end)
        .all(cap)
        .dates
        .iter()
        .map(format_floating)
        .collect()
}

/// Parse our floating `YYYY-MM-DDTHH:MM:SSZ` string. The trailing `Z` is
/// matched as a literal — we *declare* the value UTC rather than convert it,
/// which is what "floating wall-clock" means.
fn parse_floating(s: &str) -> Option<chrono::DateTime<Tz>> {
    let naive = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%SZ").ok()?;
    Some(Tz::UTC.from_utc_datetime(&naive))
}

/// Format an occurrence back to the exact same floating string shape.
fn format_floating(dt: &chrono::DateTime<Tz>) -> String {
    dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Accept exactly the rules our system can render and edit: parseable by the
/// engine AND representable as a [`RecurrenceSpec`]. Client pre-checks,
/// server enforces (same Finnish message convention as other validators).
/// End datetime for one occurrence: `occ_start` + the master's duration
/// (`master_end - master_start`). Falls back to `occ_start` itself on
/// malformed input so a bad row renders as a zero-length event instead of
/// panicking mid-view.
pub fn occurrence_end(master_start: &str, master_end: &str, occ_start: &str) -> String {
    let (Some(start), Some(end), Some(occ)) = (
        parse_floating(master_start),
        parse_floating(master_end),
        parse_floating(occ_start),
    ) else {
        return occ_start.to_owned();
    };
    // chrono: DateTime - DateTime = Duration. The `+` operator PANICS on
    // overflow, so add with `checked_add_signed` (returns Option) instead.
    match occ.checked_add_signed(end - start) {
        Some(t) => format_floating(&t),
        None => occ_start.to_owned(),
    }
}

/// Shift a floating datetime string by `minutes` (negative = earlier).
/// `None` on malformed input OR when the shift leaves chrono's range — the
/// offset can be user-controlled (event reminders), so this must never panic.
pub fn shift_minutes(dt: &str, minutes: i64) -> Option<String> {
    let parsed = parse_floating(dt)?;
    // `Duration::minutes` / `DateTime + Duration` both panic when out of
    // range; the `try_`/`checked_` variants return `None` instead, and `?`
    // propagates that `None` straight out of the function.
    let delta = chrono::Duration::try_minutes(minutes)?;
    Some(format_floating(&parsed.checked_add_signed(delta)?))
}

/// Trim a rule for a series split ("tästä eteenpäin"): the returned rule ends
/// the day BEFORE `occ_start`'s date, so the tapped occurrence and everything
/// after it leave the old series. Any COUNT end is replaced by the UNTIL (our
/// model has at most one occurrence per day, so a whole-day cut is exact).
/// `None` if the rule or datetime is outside our subset.
pub fn trim_until(rrule: &str, occ_start: &str) -> Option<String> {
    let mut spec = RecurrenceSpec::from_rrule(rrule)?;
    let occ_date = chrono::NaiveDate::parse_from_str(occ_start.get(..10)?, "%Y-%m-%d").ok()?;
    let last_day = occ_date.pred_opt()?; // the day before, calendar-aware
    spec.end = End::OnDate(last_day.format("%Y-%m-%d").to_string());
    Some(spec.to_rrule())
}

pub fn validate(rrule: &str) -> Result<(), &'static str> {
    const MSG: &str = "Toistosääntö ei kelpaa.";
    // Representable in our editor?
    let Some(_) = RecurrenceSpec::from_rrule(rrule) else {
        return Err(MSG);
    };
    // And accepted by the engine itself (RFC 5545 checks need a DTSTART, so
    // validate against an arbitrary fixed one).
    let rule: RRule<Unvalidated> = rrule.parse().map_err(|_| MSG)?;
    let dtstart = parse_floating("2026-01-01T12:00:00Z").expect("fixed datetime is valid");
    rule.build(dtstart).map(|_| ()).map_err(|_| MSG)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- spec -> RRULE string ------------------------------------------------

    #[test]
    fn daily_default_interval_omits_interval() {
        let spec = RecurrenceSpec {
            freq: Freq::Daily,
            interval: 1,
            byday: vec![],
            end: End::Never,
        };
        assert_eq!(spec.to_rrule(), "FREQ=DAILY");
    }

    #[test]
    fn weekly_with_interval_and_byday() {
        let spec = RecurrenceSpec {
            freq: Freq::Weekly,
            interval: 2,
            byday: vec![Weekday::Mon, Weekday::Wed],
            end: End::Never,
        };
        assert_eq!(spec.to_rrule(), "FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE");
    }

    #[test]
    fn count_end_serializes() {
        let spec = RecurrenceSpec {
            freq: Freq::Daily,
            interval: 1,
            byday: vec![],
            end: End::AfterCount(5),
        };
        assert_eq!(spec.to_rrule(), "FREQ=DAILY;COUNT=5");
    }

    #[test]
    fn on_date_end_becomes_inclusive_until() {
        let spec = RecurrenceSpec {
            freq: Freq::Yearly,
            interval: 1,
            byday: vec![],
            end: End::OnDate("2027-06-30".into()),
        };
        assert_eq!(spec.to_rrule(), "FREQ=YEARLY;UNTIL=20270630T235959Z");
    }

    // -- RRULE string -> spec (round-trip) ------------------------------------

    #[test]
    fn round_trip_weekly() {
        let spec = RecurrenceSpec {
            freq: Freq::Weekly,
            interval: 2,
            byday: vec![Weekday::Mon, Weekday::Wed],
            end: End::Never,
        };
        assert_eq!(RecurrenceSpec::from_rrule(&spec.to_rrule()), Some(spec));
    }

    #[test]
    fn round_trip_count_and_until() {
        let count = RecurrenceSpec {
            freq: Freq::Monthly,
            interval: 3,
            byday: vec![],
            end: End::AfterCount(12),
        };
        assert_eq!(RecurrenceSpec::from_rrule(&count.to_rrule()), Some(count));

        let until = RecurrenceSpec {
            freq: Freq::Daily,
            interval: 1,
            byday: vec![],
            end: End::OnDate("2026-08-31".into()),
        };
        assert_eq!(RecurrenceSpec::from_rrule(&until.to_rrule()), Some(until));
    }

    #[test]
    fn unsupported_rules_come_back_as_none() {
        // Valid RRULEs our editor deliberately can't express (YAGNI scope).
        assert_eq!(
            RecurrenceSpec::from_rrule("FREQ=MONTHLY;BYMONTHDAY=15"),
            None
        );
        assert_eq!(RecurrenceSpec::from_rrule("FREQ=MONTHLY;BYDAY=-1FR"), None);
        // And plain garbage.
        assert_eq!(RecurrenceSpec::from_rrule("no such rule"), None);
    }

    // -- expansion -------------------------------------------------------------

    #[test]
    fn daily_count_expands_from_dtstart() {
        let occs = expand(
            "FREQ=DAILY;COUNT=3",
            "2026-07-06T18:00:00Z",
            "2026-07-01T00:00:00Z",
            "2026-07-31T23:59:59Z",
            100,
        );
        assert_eq!(
            occs,
            vec![
                "2026-07-06T18:00:00Z",
                "2026-07-07T18:00:00Z",
                "2026-07-08T18:00:00Z",
            ]
        );
    }

    #[test]
    fn weekly_byday_hits_both_days() {
        // 2026-07-06 is a Monday.
        let occs = expand(
            "FREQ=WEEKLY;BYDAY=MO,WE",
            "2026-07-06T18:00:00Z",
            "2026-07-06T00:00:00Z",
            "2026-07-19T23:59:59Z",
            100,
        );
        assert_eq!(
            occs,
            vec![
                "2026-07-06T18:00:00Z",
                "2026-07-08T18:00:00Z",
                "2026-07-13T18:00:00Z",
                "2026-07-15T18:00:00Z",
            ]
        );
    }

    #[test]
    fn biweekly_interval_skips_alternate_weeks() {
        let occs = expand(
            "FREQ=WEEKLY;INTERVAL=2;BYDAY=MO",
            "2026-07-06T18:00:00Z",
            "2026-07-01T00:00:00Z",
            "2026-07-31T23:59:59Z",
            100,
        );
        assert_eq!(occs, vec!["2026-07-06T18:00:00Z", "2026-07-20T18:00:00Z"]);
    }

    #[test]
    fn until_stops_the_series() {
        let occs = expand(
            "FREQ=DAILY;UNTIL=20260708T235959Z",
            "2026-07-06T18:00:00Z",
            "2026-07-01T00:00:00Z",
            "2026-07-31T23:59:59Z",
            100,
        );
        assert_eq!(occs.len(), 3); // 6th, 7th, 8th
    }

    #[test]
    fn window_excludes_occurrences_outside_it() {
        // Series started before the window: only in-window dates come back.
        let occs = expand(
            "FREQ=DAILY",
            "2026-07-06T18:00:00Z",
            "2026-07-10T00:00:00Z",
            "2026-07-12T23:59:59Z",
            100,
        );
        assert_eq!(
            occs,
            vec![
                "2026-07-10T18:00:00Z",
                "2026-07-11T18:00:00Z",
                "2026-07-12T18:00:00Z",
            ]
        );
    }

    #[test]
    fn cap_stops_runaway_expansion() {
        let occs = expand(
            "FREQ=DAILY",
            "2026-07-06T18:00:00Z",
            "2026-01-01T00:00:00Z",
            "2030-12-31T23:59:59Z",
            10,
        );
        assert_eq!(occs.len(), 10);
    }

    #[test]
    fn invalid_input_expands_to_empty() {
        assert!(
            expand(
                "FREQ=BOGUS",
                "2026-07-06T18:00:00Z",
                "2026-07-01T00:00:00Z",
                "2026-07-31T23:59:59Z",
                100
            )
            .is_empty()
        );
        assert!(
            expand(
                "FREQ=DAILY",
                "not a datetime",
                "2026-07-01T00:00:00Z",
                "2026-07-31T23:59:59Z",
                100
            )
            .is_empty()
        );
    }

    // -- fire-time math ------------------------------------------------------------

    #[test]
    fn shift_minutes_moves_time() {
        assert_eq!(
            shift_minutes("2026-07-06T18:00:00Z", -30),
            Some("2026-07-06T17:30:00Z".to_owned())
        );
        // Crossing midnight backwards changes the date.
        assert_eq!(
            shift_minutes("2026-07-06T00:10:00Z", -30),
            Some("2026-07-05T23:40:00Z".to_owned())
        );
        // A whole day forward (the 1 vrk reminder offset).
        assert_eq!(
            shift_minutes("2026-07-06T18:00:00Z", 1440),
            Some("2026-07-07T18:00:00Z".to_owned())
        );
        assert_eq!(shift_minutes("garbage", 5), None);
    }

    // -- series split (trim) -----------------------------------------------------

    #[test]
    fn trim_until_ends_series_the_day_before() {
        assert_eq!(
            trim_until("FREQ=WEEKLY;BYDAY=MO", "2026-07-20T18:00:00Z"),
            Some("FREQ=WEEKLY;BYDAY=MO;UNTIL=20260719T235959Z".to_owned())
        );
        // Month boundary: the day before 1.8. is 31.7.
        assert_eq!(
            trim_until("FREQ=DAILY", "2026-08-01T09:00:00Z"),
            Some("FREQ=DAILY;UNTIL=20260731T235959Z".to_owned())
        );
    }

    #[test]
    fn trim_until_replaces_count_and_old_until() {
        // RFC 5545 forbids COUNT+UNTIL together, so COUNT must be dropped.
        assert_eq!(
            trim_until("FREQ=DAILY;COUNT=30", "2026-07-20T18:00:00Z"),
            Some("FREQ=DAILY;UNTIL=20260719T235959Z".to_owned())
        );
        assert_eq!(
            trim_until("FREQ=DAILY;UNTIL=20261231T235959Z", "2026-07-20T18:00:00Z"),
            Some("FREQ=DAILY;UNTIL=20260719T235959Z".to_owned())
        );
    }

    #[test]
    fn trim_until_rejects_bad_input() {
        assert_eq!(trim_until("FREQ=BOGUS", "2026-07-20T18:00:00Z"), None);
        assert_eq!(trim_until("FREQ=DAILY", "not a datetime"), None);
    }

    // -- validation --------------------------------------------------------------

    // -- occurrence end times ----------------------------------------------------

    #[test]
    fn occurrence_end_preserves_duration() {
        // 90-minute timed event.
        assert_eq!(
            occurrence_end(
                "2026-07-06T18:00:00Z",
                "2026-07-06T19:30:00Z",
                "2026-07-13T18:00:00Z"
            ),
            "2026-07-13T19:30:00Z"
        );
        // Multi-day all-day span keeps its length (Fri–Sun = 2 days).
        assert_eq!(
            occurrence_end(
                "2026-07-03T00:00:00Z",
                "2026-07-05T00:00:00Z",
                "2026-07-10T00:00:00Z"
            ),
            "2026-07-12T00:00:00Z"
        );
    }

    #[test]
    fn occurrence_end_falls_back_on_bad_input() {
        assert_eq!(
            occurrence_end("bad", "worse", "2026-07-13T18:00:00Z"),
            "2026-07-13T18:00:00Z"
        );
    }

    /// Security finding: a member-supplied reminder offset reached chrono's
    /// panicking `Duration::minutes` / `DateTime + Duration`, which killed
    /// the reminder scheduler. Out-of-range shifts must be `None`, never a panic.
    #[test]
    fn shift_minutes_out_of_range_is_none_not_panic() {
        let t = "2026-07-06T18:00:00Z";
        assert_eq!(
            shift_minutes(t, 30).as_deref(),
            Some("2026-07-06T18:30:00Z")
        );
        assert_eq!(
            shift_minutes(t, -30).as_deref(),
            Some("2026-07-06T17:30:00Z")
        );
        assert_eq!(shift_minutes(t, 200_000_000_000_000), None); // TimeDelta overflow
        assert_eq!(shift_minutes(t, -100_000_000_000_000), None);
        assert_eq!(shift_minutes(t, i64::MIN), None);
        assert_eq!(shift_minutes(t, i64::MAX), None);
        // In TimeDelta range, but past chrono's max DateTime (~262 000 years).
        assert_eq!(shift_minutes(t, 200_000_000_000), None);
    }

    #[test]
    fn validate_accepts_editor_rules() {
        assert!(validate("FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE").is_ok());
        assert!(validate("FREQ=DAILY;COUNT=5").is_ok());
        assert!(validate("FREQ=YEARLY;UNTIL=20270630T235959Z").is_ok());
    }

    #[test]
    fn validate_rejects_garbage_and_unsupported() {
        assert!(validate("FREQ=BOGUS").is_err());
        assert!(validate("").is_err());
        // Parseable but outside our editor's subset — reject at write time so
        // every stored rule is one the UI can render and edit.
        assert!(validate("FREQ=MONTHLY;BYMONTHDAY=15").is_err());
    }
}
