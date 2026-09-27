-- Phase 5B: recurrence (RFC 5545 RRULE), per
-- docs/superpowers/specs/2026-07-01-calendar-recurrence-design.md.
--
-- A repeating event is ONE events row carrying an RRULE ("the master"), plus
-- small patch records for exceptions:
--   * event_exdates rows skip single occurrences (delete-this-one, and the
--     removal half of edit-this-one),
--   * override rows are normal events rows that REPLACE one occurrence,
--     linked back via series_uid + recurrence_id.
-- Occurrences themselves are never stored — both client and server expand the
-- rule on demand via shared::recur.

-- NULL = one-off event; all existing rows are untouched.
ALTER TABLE events ADD COLUMN rrule TEXT;

-- Override rows only (both NULL on masters and one-offs):
ALTER TABLE events ADD COLUMN series_uid TEXT;     -- the master's uid
ALTER TABLE events ADD COLUMN recurrence_id TEXT;  -- occurrence start datetime being replaced

-- "All overrides of this series" lookup (edit-all / delete-all cascades).
CREATE INDEX idx_events_family_series ON events(family_id, series_uid);

CREATE TABLE event_exdates (
    id         INTEGER PRIMARY KEY,
    family_id  INTEGER NOT NULL REFERENCES families(id),
    series_uid TEXT    NOT NULL,   -- master uid (not a FK: masters key on uid)
    occ_start  TEXT    NOT NULL,   -- occurrence start datetime being skipped
    UNIQUE (family_id, series_uid, occ_start)
);
