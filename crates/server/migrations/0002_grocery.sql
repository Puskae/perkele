-- Phase 2: grocery list and sync log.
--
-- grocery_items: one implicit list per family (no "list" entity).
--   checked = 1 means the item is in the cart; deleted_at is a tombstone
--   used by "clear checked" and eventually by the Phase-3 offline sync.
-- sync_log: append-only event log for Phase-3 offline delta sync.
--   seq is per-family monotonic: each mutation appends
--   seq = COALESCE(MAX(seq), 0) + 1 for its family.
-- recipe_id is intentionally a plain INTEGER (no FK) because the recipes
--   table doesn't exist until Phase 4; the FK is added then.

CREATE TABLE grocery_items (
    id          INTEGER PRIMARY KEY,
    family_id   INTEGER NOT NULL REFERENCES families(id),
    name        TEXT    NOT NULL,
    qty         TEXT,
    unit        TEXT,
    category    TEXT,
    checked     INTEGER NOT NULL DEFAULT 0,
    added_by    INTEGER NOT NULL REFERENCES users(id),
    recipe_id   INTEGER,
    deleted_at  TEXT,
    created_at  TEXT    NOT NULL
);

CREATE INDEX idx_grocery_items_family ON grocery_items(family_id);

CREATE TABLE sync_log (
    family_id   INTEGER NOT NULL REFERENCES families(id),
    seq         INTEGER NOT NULL,
    entity      TEXT    NOT NULL,
    entity_id   INTEGER NOT NULL,
    op          TEXT    NOT NULL,
    updated_at  TEXT    NOT NULL,
    PRIMARY KEY (family_id, seq)
);

CREATE INDEX idx_sync_log_family_seq ON sync_log(family_id, seq);
