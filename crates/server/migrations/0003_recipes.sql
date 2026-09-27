-- Phase 4: recipes, structured ingredients, and the dinner-per-day meal plan.
--
-- recipes: one header row per recipe (soft-deleted via deleted_at).
-- recipe_ingredients: structured rows (qty/unit/name/category). family_id is
--   denormalized here so the grocery-import query can scope by family without a
--   join, honoring the "every domain row has family_id" rule. position keeps
--   the displayed ingredient order stable.
-- meal_plan_entries: dinner-only — one entry per (family_id, date). recipe_id
--   XOR free_text: a planned dinner is either a chosen recipe or typed text.
--
-- Note on grocery_items.recipe_id (from migration 0002): despite the comment
-- there, no foreign key is added to it now. SQLite can't ALTER TABLE ADD
-- CONSTRAINT, and rebuilding grocery_items isn't worth it — the column is
-- provenance only (which recipe an imported item came from), not a referential
-- guarantee. We don't modify the applied 0002 migration; migrations are
-- immutable once shipped, so the clarification lives here instead.

CREATE TABLE recipes (
    id           INTEGER PRIMARY KEY,
    family_id    INTEGER NOT NULL REFERENCES families(id),
    title        TEXT    NOT NULL,
    instructions TEXT,
    servings     INTEGER,
    prep_min     INTEGER,
    cook_min     INTEGER,
    source       TEXT,
    created_by   INTEGER NOT NULL REFERENCES users(id),
    created_at   TEXT    NOT NULL,
    updated_at   TEXT    NOT NULL,
    deleted_at   TEXT
);

CREATE INDEX idx_recipes_family ON recipes(family_id);

CREATE TABLE recipe_ingredients (
    id         INTEGER PRIMARY KEY,
    recipe_id  INTEGER NOT NULL REFERENCES recipes(id),
    family_id  INTEGER NOT NULL REFERENCES families(id),
    name       TEXT    NOT NULL,
    qty        TEXT,
    unit       TEXT,
    category   TEXT,
    position   INTEGER NOT NULL
);

CREATE INDEX idx_recipe_ingredients_recipe ON recipe_ingredients(recipe_id);

CREATE TABLE meal_plan_entries (
    id          INTEGER PRIMARY KEY,
    family_id   INTEGER NOT NULL REFERENCES families(id),
    date        TEXT    NOT NULL,
    recipe_id   INTEGER REFERENCES recipes(id),
    free_text   TEXT,
    created_by  INTEGER NOT NULL REFERENCES users(id),
    created_at  TEXT    NOT NULL,
    UNIQUE (family_id, date)
);
