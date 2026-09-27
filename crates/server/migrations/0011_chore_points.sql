-- Chore gamification: per-chore point value, snapshotted onto each completion
-- at tick time so editing a chore never rewrites past standings.
-- (docs/superpowers/specs/2026-07-12-chore-gamification-design.md)
ALTER TABLE chores            ADD COLUMN points INTEGER NOT NULL DEFAULT 1;
ALTER TABLE chore_completions ADD COLUMN points INTEGER NOT NULL DEFAULT 1;
