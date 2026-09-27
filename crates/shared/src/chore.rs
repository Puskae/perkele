//! Chores: DTOs, validation, and the single source of truth for "is this
//! chore due on date X and whose turn is it?" — shared by the app screen,
//! the /api/chores/today endpoint, and the reminder scanner, so the three
//! can never disagree.

use crate::recur;
use serde::{Deserialize, Serialize};

pub const TITLE_MAX: usize = 200;
/// Cap for recur::expand calls. Rotation needs the occurrence COUNT since
/// start, so the window can span years; 65535 daily occurrences ≈ 179 years.
const EXPAND_CAP: u16 = u16::MAX;

/// Turn-order length cap: far above any family's size.
pub const ROTATION_MAX: usize = 50;

pub const POINTS_MIN: i64 = 1;
pub const POINTS_MAX: i64 = 100;

/// serde fallback: rows/caches written before the points feature carry no
/// `points` key; they count as 1 point, same as the DB column default.
fn default_points() -> i64 {
    1
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chore {
    pub id: i64,
    pub title: String,
    pub rrule: String,
    pub start_date: String, // 'YYYY-MM-DD'
    pub assigned_user_id: Option<i64>,
    /// Turn order; validation forbids combining with assigned_user_id.
    pub rotation: Option<Vec<i64>>,
    pub remind_at: Option<String>, // local 'HH:MM'
    /// Admin-set reward for completing this chore (1–100).
    #[serde(default = "default_points")]
    pub points: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SaveChoreRequest {
    pub title: String,
    pub rrule: String,
    pub start_date: String,
    pub assigned_user_id: Option<i64>,
    pub rotation: Option<Vec<i64>>,
    pub remind_at: Option<String>,
    /// Admin-set reward for completing this chore (1–100). `None` means "not
    /// sent" — an old cached PWA bundle whose form predates this field won't
    /// include the key; serde's plain default then decodes it as `None`
    /// rather than as `1`, so the update handler can tell "not sent" apart
    /// from "explicitly set to 1" and keep the existing value instead of
    /// silently resetting it. On create, `None` still means the default (1).
    #[serde(default)]
    pub points: Option<i64>,
}

/// One row of the "today" list: a due chore with computed assignee and state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DueChore {
    pub chore_id: i64,
    pub title: String,
    pub date: String,
    /// None = whole family (anyone's job).
    pub assignee_id: Option<i64>,
    pub remind_at: Option<String>,
    /// Who completed it today; None = not done yet.
    pub done_by: Option<i64>,
    /// Admin-set reward for completing this chore (1–100).
    #[serde(default = "default_points")]
    pub points: i64,
}

/// Exact `YYYY-MM-DD`, a real day, year within the shared bounds (see
/// [`crate::dates`]) — an ancient start date would make every rotation
/// lookup expand decades of occurrences.
pub fn validate_date(d: &str) -> Result<(), &'static str> {
    crate::dates::validate_date(d)
}

pub fn validate_time_hhmm(t: &str) -> Result<(), &'static str> {
    let ok = t.len() == 5
        && t.as_bytes()[2] == b':'
        && t[..2].parse::<u8>().is_ok_and(|h| h < 24)
        && t[3..].parse::<u8>().is_ok_and(|m| m < 60);
    if ok {
        Ok(())
    } else {
        Err("Virheellinen kellonaika.")
    }
}

pub fn validate_chore(req: &SaveChoreRequest) -> Result<(), &'static str> {
    let len = req.title.trim().chars().count();
    if len == 0 || len > TITLE_MAX {
        return Err("Otsikon tulee olla 1–200 merkkiä.");
    }
    recur::validate(&req.rrule)?;
    validate_date(&req.start_date)?;
    if let Some(t) = &req.remind_at {
        validate_time_hhmm(t)?;
    }
    // `None` ("not sent") is valid — it means "keep current"/"use default",
    // decided later by the handler. Only a present value is range-checked.
    if let Some(p) = req.points
        && !(POINTS_MIN..=POINTS_MAX).contains(&p)
    {
        return Err("Pisteet 1–100.");
    }
    match (&req.assigned_user_id, &req.rotation) {
        (Some(_), Some(_)) => Err("Valitse joko vastuuhenkilö tai vuorottelu, ei molempia."),
        (_, Some(r)) if r.is_empty() => Err("Vuorottelu tarvitsee vähintään yhden jäsenen."),
        (_, Some(r)) if r.len() > ROTATION_MAX => Err("Vuorottelussa on liian monta jäsentä."),
        _ => Ok(()),
    }
}

/// Every occurrence date from start through `date` (inclusive). The length
/// of this list IS the deterministic rotation index.
fn occurrences_through(chore: &Chore, date: &str) -> Vec<String> {
    recur::expand(
        &chore.rrule,
        &format!("{}T00:00:00Z", chore.start_date),
        &format!("{}T00:00:00Z", chore.start_date),
        &format!("{date}T23:59:59Z"),
        EXPAND_CAP,
    )
}

pub fn is_due_on(chore: &Chore, date: &str) -> bool {
    // Single-day window; cap 2 because we only care about "any occurrence".
    !recur::expand(
        &chore.rrule,
        &format!("{}T00:00:00Z", chore.start_date),
        &format!("{date}T00:00:00Z"),
        &format!("{date}T23:59:59Z"),
        2,
    )
    .is_empty()
}

/// Whose turn on `date`? Deterministic by date: the Nth occurrence since
/// start goes to rotation[(N-1) % len] — skipped days don't shift the
/// future schedule. Falls back to the fixed assignee; None = whole family.
pub fn assignee_on(chore: &Chore, date: &str) -> Option<i64> {
    if let Some(rot) = chore.rotation.as_ref().filter(|r| !r.is_empty()) {
        let n = occurrences_through(chore, date).len();
        if n == 0 {
            return None; // before the chore's first occurrence
        }
        return Some(rot[(n - 1) % rot.len()]);
    }
    chore.assigned_user_id
}

// --- stats DTOs (GET /api/chores/stats) ------------------------------------

/// One leaderboard row: what a member earned inside the requested window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemberTotal {
    pub user_id: i64,
    pub points: i64,
    pub completions: i64,
}

/// Generic (member, number) pair: a completion count in `ChoreBreakdown.by`,
/// a points sum in `WeekBucket.points`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemberCount {
    pub user_id: i64,
    pub count: i64,
}

/// One chore's window summary: how often it was due vs actually done, and by
/// whom. `due` comes from the shared recurrence expansion — the same code the
/// today-list uses — so due/done can never disagree with the app.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChoreBreakdown {
    pub chore_id: i64,
    pub title: String,
    pub due: i64,
    pub done: i64,
    pub by: Vec<MemberCount>,
}

/// Points per member for one Monday-started week (the trend chart's bars).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WeekBucket {
    pub week_start: String, // 'YYYY-MM-DD', always a Monday
    pub points: Vec<MemberCount>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChoreStats {
    /// Sorted by points DESC (ties: user_id) — the leaderboard order.
    pub totals: Vec<MemberTotal>,
    pub per_chore: Vec<ChoreBreakdown>,
    /// Exactly 12 buckets, oldest first, empty weeks included so chart
    /// columns line up without client-side gap math.
    pub trend: Vec<WeekBucket>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chore(rrule: &str, start: &str, rotation: Option<Vec<i64>>) -> Chore {
        Chore {
            id: 1,
            title: "Tiskit".into(),
            rrule: rrule.into(),
            start_date: start.into(),
            assigned_user_id: None,
            rotation,
            remind_at: None,
            points: 1, // Chore.points stays plain i64 — read-side default.
        }
    }

    #[test]
    fn weekly_chore_is_due_only_on_its_weekday() {
        // 2026-01-05 is a Monday.
        let c = chore("FREQ=WEEKLY;BYDAY=MO", "2026-01-05", None);
        assert!(is_due_on(&c, "2026-01-05"));
        assert!(!is_due_on(&c, "2026-01-06"));
        assert!(is_due_on(&c, "2026-01-12"));
        // Before the start date: never due.
        assert!(!is_due_on(&c, "2025-12-29"));
    }

    #[test]
    fn daily_rotation_wraps_deterministically() {
        let c = chore("FREQ=DAILY", "2026-01-05", Some(vec![10, 20, 30]));
        assert_eq!(assignee_on(&c, "2026-01-05"), Some(10)); // occurrence 1
        assert_eq!(assignee_on(&c, "2026-01-06"), Some(20));
        assert_eq!(assignee_on(&c, "2026-01-07"), Some(30));
        assert_eq!(assignee_on(&c, "2026-01-08"), Some(10)); // wrapped
    }

    #[test]
    fn weekly_rotation_counts_occurrences_not_days() {
        let c = chore("FREQ=WEEKLY;BYDAY=MO", "2026-01-05", Some(vec![1, 2]));
        assert_eq!(assignee_on(&c, "2026-01-05"), Some(1));
        assert_eq!(assignee_on(&c, "2026-01-12"), Some(2));
        assert_eq!(assignee_on(&c, "2026-01-19"), Some(1));
    }

    #[test]
    fn fixed_assignee_and_family_fallbacks() {
        let mut c = chore("FREQ=DAILY", "2026-01-05", None);
        assert_eq!(assignee_on(&c, "2026-01-06"), None); // whole family
        c.assigned_user_id = Some(7);
        assert_eq!(assignee_on(&c, "2026-01-06"), Some(7));
        // Rotation before start: no turn yet.
        let r = chore("FREQ=DAILY", "2026-01-05", Some(vec![1]));
        assert_eq!(assignee_on(&r, "2025-01-01"), None);
    }

    #[test]
    fn validate_chore_rejects_out_of_range_points() {
        let mut req = SaveChoreRequest {
            title: "Tiskit".into(),
            rrule: "FREQ=DAILY".into(),
            start_date: "2026-01-05".into(),
            assigned_user_id: None,
            rotation: None,
            remind_at: None,
            points: Some(1),
        };
        assert!(validate_chore(&req).is_ok());
        req.points = Some(0);
        assert_eq!(validate_chore(&req), Err("Pisteet 1–100."));
        req.points = Some(101);
        assert_eq!(validate_chore(&req), Err("Pisteet 1–100."));
        req.points = Some(100);
        assert!(validate_chore(&req).is_ok());
        // "Not sent" (old client) is valid — the handler decides the value.
        req.points = None;
        assert!(validate_chore(&req).is_ok());
    }

    #[test]
    fn points_default_to_one_when_missing_from_json() {
        // Old localStorage caches and old clients don't send `points`; serde's
        // default keeps them deserializable instead of erroring the whole list.
        let d: DueChore = serde_json::from_str(
            r#"{"chore_id":1,"title":"Tiskit","date":"2026-01-05",
                "assignee_id":null,"remind_at":null,"done_by":null}"#,
        )
        .unwrap();
        assert_eq!(d.points, 1);
    }

    #[test]
    fn validate_chore_rejects_bad_combos() {
        let mut req = SaveChoreRequest {
            title: "Tiskit".into(),
            rrule: "FREQ=DAILY".into(),
            start_date: "2026-01-05".into(),
            assigned_user_id: None,
            rotation: None,
            remind_at: None,
            points: Some(1),
        };
        assert!(validate_chore(&req).is_ok());
        req.assigned_user_id = Some(1);
        req.rotation = Some(vec![2]);
        assert!(validate_chore(&req).is_err()); // both set
        req.assigned_user_id = None;
        req.rotation = Some(vec![]);
        assert!(validate_chore(&req).is_err()); // empty rotation
        req.rotation = None;
        req.remind_at = Some("25:00".into());
        assert!(validate_chore(&req).is_err()); // bad time
        req.remind_at = Some("17:00".into());
        req.start_date = "05.01.2026".into();
        assert!(validate_chore(&req).is_err()); // bad date
    }

    #[test]
    fn validate_chore_bounds_start_date_and_rotation() {
        let mut req = SaveChoreRequest {
            title: "Tiskit".into(),
            rrule: "FREQ=DAILY".into(),
            start_date: "0001-01-01".into(),
            assigned_user_id: None,
            rotation: None,
            remind_at: None,
            points: Some(1),
        };
        assert!(validate_chore(&req).is_err()); // pre-1900: unbounded expansion
        req.start_date = "2026-02-30".into();
        assert!(validate_chore(&req).is_err()); // shape ok, not a real day
        req.start_date = "2026-01-05".into();
        req.rotation = Some((1..=ROTATION_MAX as i64).collect());
        assert!(validate_chore(&req).is_ok());
        req.rotation = Some((1..=ROTATION_MAX as i64 + 1).collect());
        assert!(validate_chore(&req).is_err());
    }
}
