-- Admin-only "who changed what, when" over the seven shared-data domains.
-- Separate from sync_log by design: audit is accountability, not offline sync.
-- We store the structured triple (op, entity, label) — NOT a frozen Finnish
-- sentence — so all phrasing lives in one frontend formatter. `label` captures
-- the entity's display name at write time so a deleted item stays readable.
CREATE TABLE audit_log (
    id             INTEGER PRIMARY KEY,
    family_id      INTEGER NOT NULL REFERENCES families(id),
    actor_user_id  INTEGER NOT NULL REFERENCES users(id),
    entity         TEXT    NOT NULL,   -- see ENTITY_KINDS in shared/src/audit.rs
    entity_id      INTEGER,            -- NULL for bulk ops (one row, not N)
    op             TEXT    NOT NULL,   -- 'create' | 'update' | 'delete'
    label          TEXT    NOT NULL,   -- entity display name captured at write time
    created_at     TEXT    NOT NULL    -- RFC3339 UTC
);

-- Serves the retention prune (created_at range scan) and family-scoped reads.
CREATE INDEX idx_audit_log_family_created ON audit_log(family_id, created_at DESC);
