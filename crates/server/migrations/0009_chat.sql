-- Perhechatti: one chat room per family.
-- (docs/superpowers/specs/2026-07-04-group-chat-design.md)
CREATE TABLE messages (
    id         INTEGER PRIMARY KEY,
    family_id  INTEGER NOT NULL REFERENCES families(id),
    author_id  INTEGER NOT NULL REFERENCES users(id),
    body       TEXT    NOT NULL,          -- 1..2000 chars, validated in shared
    created_at TEXT    NOT NULL,          -- RFC3339 UTC instant
    deleted_at TEXT                       -- soft delete
);
CREATE INDEX idx_messages_family_id ON messages(family_id, id);

-- One row per (message, user, emoji); reacting again with the same emoji
-- toggles the row away. Emoji set is validated in shared.
CREATE TABLE message_reactions (
    id         INTEGER PRIMARY KEY,
    message_id INTEGER NOT NULL REFERENCES messages(id),
    user_id    INTEGER NOT NULL REFERENCES users(id),
    emoji      TEXT    NOT NULL,
    UNIQUE(message_id, user_id, emoji)
);

-- Per-user read marker; only ever moves forward. Drives the unread badge.
CREATE TABLE chat_reads (
    user_id      INTEGER PRIMARY KEY REFERENCES users(id),
    family_id    INTEGER NOT NULL REFERENCES families(id),
    last_read_id INTEGER NOT NULL DEFAULT 0
);
