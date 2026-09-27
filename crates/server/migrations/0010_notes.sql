-- Muistiot: topics of mixed free-text + checklist blocks.
-- (docs/superpowers/specs/2026-07-12-home-hub-and-notes-design.md)
CREATE TABLE note_topics (
    id         INTEGER PRIMARY KEY,
    family_id  INTEGER NOT NULL REFERENCES families(id),
    title      TEXT    NOT NULL,          -- 1..200 chars, validated in shared
    created_by INTEGER NOT NULL REFERENCES users(id),
    created_at TEXT    NOT NULL,          -- RFC3339 UTC
    updated_at TEXT    NOT NULL,
    deleted_at TEXT                       -- soft delete (topics only)
);

-- Blocks are hard-replaced wholesale on every full edit (recipe_ingredients
-- pattern), so they carry no soft-delete; the topic row is the recoverable
-- unit. family_id is denormalized so the checkbox UPDATE can scope by family
-- without a JOIN.
CREATE TABLE note_blocks (
    id        INTEGER PRIMARY KEY,
    topic_id  INTEGER NOT NULL REFERENCES note_topics(id),
    family_id INTEGER NOT NULL REFERENCES families(id),
    kind      TEXT    NOT NULL CHECK (kind IN ('text', 'check')),
    content   TEXT    NOT NULL,
    checked   INTEGER NOT NULL DEFAULT 0,
    position  INTEGER NOT NULL
);
CREATE INDEX idx_note_blocks_topic ON note_blocks(topic_id, position);
