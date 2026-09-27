-- Phase 6B: chores — definitions, done-tracking, reminder dedup.
-- (docs/superpowers/specs/2026-07-04-phase-6-chores-announcements-design.md)
CREATE TABLE chores (
    id               INTEGER PRIMARY KEY,
    family_id        INTEGER NOT NULL REFERENCES families(id),
    title            TEXT    NOT NULL,   -- 1..200 chars
    rrule            TEXT    NOT NULL,   -- e.g. FREQ=DAILY, FREQ=WEEKLY;BYDAY=MO,TH
    start_date       TEXT    NOT NULL,   -- 'YYYY-MM-DD' local date (dtstart)
    assigned_user_id INTEGER REFERENCES users(id),  -- fixed assignee …
    rotation_json    TEXT,               -- … OR JSON array of user ids in turn order
    remind_at        TEXT,               -- optional local 'HH:MM'
    created_by       INTEGER NOT NULL REFERENCES users(id),
    created_at       TEXT    NOT NULL,
    updated_at       TEXT    NOT NULL,
    deleted_at       TEXT
);

-- Lapse model: due today + no row here = not done. Yesterday is never asked.
CREATE TABLE chore_completions (
    id           INTEGER PRIMARY KEY,
    chore_id     INTEGER NOT NULL REFERENCES chores(id),
    date         TEXT    NOT NULL,       -- 'YYYY-MM-DD'
    completed_by INTEGER NOT NULL REFERENCES users(id),
    completed_at TEXT    NOT NULL,
    UNIQUE (chore_id, date)
);

-- Insert-to-claim dedup for reminder pushes, same trick as reminder_sends.
CREATE TABLE chore_reminder_sends (
    chore_id INTEGER NOT NULL,
    date     TEXT    NOT NULL,
    sent_at  TEXT    NOT NULL,
    PRIMARY KEY (chore_id, date)
);
