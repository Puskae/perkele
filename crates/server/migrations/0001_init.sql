-- Phase 1 foundation: families, members, sessions, invites.
--
-- Conventions used throughout PERKELE:
--   * Table names are plural ('users' not 'user') — 'user' is a reserved word
--     in Postgres, and we keep a Postgres migration on the table for later.
--   * Timestamps are TEXT in RFC 3339 / ISO 8601, always UTC. SQLite has no
--     native datetime type; storing ISO strings keeps them human-readable and
--     correctly sortable.
--   * Every tenant-owned row carries family_id. This is the multi-tenancy
--     backbone: queries are always scoped to the caller's family.

-- The top-level tenant. One row per household.
CREATE TABLE families (
    id         INTEGER PRIMARY KEY,
    name       TEXT NOT NULL,
    created_at TEXT NOT NULL
);

-- A member of one family. `username` is unique *within* a family, not
-- globally, so two different families can each have a "dad".
CREATE TABLE users (
    id           INTEGER PRIMARY KEY,
    family_id    INTEGER NOT NULL REFERENCES families (id),
    username     TEXT NOT NULL,
    display_name TEXT NOT NULL,
    email        TEXT, -- optional; used for password reset / notifications later
    pw_hash      TEXT NOT NULL,
    role         TEXT NOT NULL CHECK (role IN ('admin', 'member', 'kid')),
    created_at   TEXT NOT NULL,
    UNIQUE (family_id, username)
);

-- Opaque session tokens. We store only a hash of the token, never the token
-- itself — so a database leak cannot be replayed to hijack live sessions
-- (same reasoning as password hashing).
CREATE TABLE sessions (
    id         INTEGER PRIMARY KEY,
    user_id    INTEGER NOT NULL REFERENCES users (id),
    token_hash TEXT NOT NULL UNIQUE,
    expires_at TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE INDEX idx_sessions_user ON sessions (user_id);

-- Invite codes an admin generates to add a family member. Like sessions, we
-- store a hash of the code, not the code itself. `used_at` is NULL until the
-- code is redeemed (single use).
CREATE TABLE invites (
    id         INTEGER PRIMARY KEY,
    family_id  INTEGER NOT NULL REFERENCES families (id),
    code_hash  TEXT NOT NULL UNIQUE,
    role       TEXT NOT NULL CHECK (role IN ('admin', 'member', 'kid')),
    expires_at TEXT NOT NULL,
    used_at    TEXT,
    created_by INTEGER NOT NULL REFERENCES users (id),
    created_at TEXT NOT NULL
);

CREATE INDEX idx_invites_family ON invites (family_id);
