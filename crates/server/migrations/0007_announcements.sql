-- Phase 6A: family announcements board ("fridge door").
-- (docs/superpowers/specs/2026-07-04-phase-6-chores-announcements-design.md)
CREATE TABLE announcements (
    id         INTEGER PRIMARY KEY,
    family_id  INTEGER NOT NULL REFERENCES families(id),
    body       TEXT    NOT NULL,          -- 1..2000 chars, validated in shared
    pinned     INTEGER NOT NULL DEFAULT 0,
    created_by INTEGER NOT NULL REFERENCES users(id),
    created_at TEXT    NOT NULL,          -- RFC3339 UTC
    updated_at TEXT    NOT NULL,
    deleted_at TEXT                       -- soft delete
);
