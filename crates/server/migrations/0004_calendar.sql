-- Phase 5A: calendar events and their attendees.
--
-- Calendar events. Timed and all-day events share the same RFC3339 UTC columns;
-- the all_day flag tells the client how to interpret/render them:
--   timed:   starts_at/ends_at are real UTC instants; client renders local time.
--   all-day: normalized to midnight UTC. Single-day has starts_at == ends_at at
--            T00:00:00Z; a multi-day span stores the INCLUSIVE last day. The
--            client renders the date portion directly (no TZ conversion), so an
--            all-day event never drifts to an adjacent day.
-- uid is client-generated (crypto.randomUUID) and is the client-facing identity:
--   all routes and the local cache key on it, never the integer id. This is what
--   lets an offline-created event be edited/deleted before it ever reaches the
--   server. The integer id stays as the internal rowid + sync_log.entity_id.
CREATE TABLE events (
    id          INTEGER PRIMARY KEY,
    family_id   INTEGER NOT NULL REFERENCES families(id),
    uid         TEXT    NOT NULL,
    title       TEXT    NOT NULL,
    all_day     INTEGER NOT NULL DEFAULT 0,
    starts_at   TEXT    NOT NULL,   -- RFC3339 UTC
    ends_at     TEXT    NOT NULL,   -- RFC3339 UTC; >= starts_at
    location    TEXT,
    notes       TEXT,
    created_by  INTEGER NOT NULL REFERENCES users(id),
    created_at  TEXT    NOT NULL,
    updated_at  TEXT    NOT NULL,
    deleted_at  TEXT,
    UNIQUE (family_id, uid)
);

CREATE INDEX idx_events_family_start ON events(family_id, starts_at);

CREATE TABLE event_attendees (
    event_id  INTEGER NOT NULL REFERENCES events(id),
    user_id   INTEGER NOT NULL REFERENCES users(id),
    family_id INTEGER NOT NULL REFERENCES families(id),  -- denormalized for scoping
    PRIMARY KEY (event_id, user_id)
);
