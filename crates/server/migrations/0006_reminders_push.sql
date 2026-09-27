-- Phase 5C: event reminders + Web Push
-- (docs/superpowers/specs/2026-07-02-reminders-webpush-design.md).

-- Minutes before the occurrence start to remind; NULL = no reminder.
ALTER TABLE events ADD COLUMN reminder_minutes INTEGER;

-- One row per subscribed browser/device.
CREATE TABLE push_subscriptions (
    id         INTEGER PRIMARY KEY,
    user_id    INTEGER NOT NULL REFERENCES users(id),
    family_id  INTEGER NOT NULL REFERENCES families(id),
    endpoint   TEXT    NOT NULL UNIQUE,
    keys_json  TEXT    NOT NULL,          -- {"p256dh":"…","auth":"…"}
    created_at TEXT    NOT NULL
);

-- Dedup log: which occurrence reminders have been sent. Also implements the
-- "<30 min late or skip" rule across restarts.
CREATE TABLE reminder_sends (
    id         INTEGER PRIMARY KEY,
    family_id  INTEGER NOT NULL,
    event_uid  TEXT    NOT NULL,   -- master or one-off uid
    occ_start  TEXT    NOT NULL,   -- == starts_at for one-offs
    sent_at    TEXT    NOT NULL,
    UNIQUE (family_id, event_uid, occ_start)
);

-- Server-generated settings (VAPID keys). Key-value keeps it schema-free.
CREATE TABLE config (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
