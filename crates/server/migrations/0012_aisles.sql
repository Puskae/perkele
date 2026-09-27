-- Aisles ("hyllyt"): the family's store aisles in walking order, plus a
-- learned item-name -> aisle mapping used to sort the main grocery list.
-- The mapping is keyed by NAME, not grocery_items.id: assigning "maito" once
-- teaches every future "maito". grocery_items is untouched — its `category`
-- column goes back to meaning ONLY "store section" (Apteekki etc.).

CREATE TABLE grocery_aisles (
    id        INTEGER PRIMARY KEY,
    family_id INTEGER NOT NULL REFERENCES families(id),
    name      TEXT    NOT NULL,
    position  INTEGER NOT NULL,
    UNIQUE (family_id, name)
);

CREATE TABLE item_aisles (
    family_id INTEGER NOT NULL REFERENCES families(id),
    item_name TEXT    NOT NULL,  -- lowercased + trimmed at write time (in Rust)
    aisle_id  INTEGER NOT NULL REFERENCES grocery_aisles(id) ON DELETE CASCADE,
    -- also serves as the family_id lookup index (UNIQUE gives SQLite an
    -- index on (family_id, item_name); its prefix covers family_id alone).
    UNIQUE (family_id, item_name)
);

-- Bootstrap 1/3: create each family's aisles from the distinct ingredient
-- categories it already has (the 20 seed recipes carry kasvikset/maito/...).
-- Only LIVE recipes contribute — soft-deleted ones leave their
-- recipe_ingredients rows behind, and those shouldn't seed an aisle.
-- Alphabetical initial order; the user reorders in the editor.
INSERT INTO grocery_aisles (family_id, name, position)
SELECT family_id,
       category,
       ROW_NUMBER() OVER (PARTITION BY family_id ORDER BY category)
FROM (
    SELECT DISTINCT ri.family_id, TRIM(ri.category) AS category
    FROM recipe_ingredients ri
    JOIN recipes r ON r.id = ri.recipe_id AND r.deleted_at IS NULL
    WHERE ri.category IS NOT NULL AND TRIM(ri.category) <> ''
);

-- Bootstrap 2/3: seed the item map from ingredient name -> category pairs.
-- Only LIVE recipes contribute (see bootstrap 1/3). OR IGNORE: if the same
-- name maps to two categories across recipes, the first wins (rare; user
-- can reassign). NOTE: SQLite LOWER() folds ASCII only — seed ingredient
-- names are already lowercase, and any non-ASCII misses self-heal the
-- first time the user assigns the item by hand.
INSERT OR IGNORE INTO item_aisles (family_id, item_name, aisle_id)
SELECT ri.family_id, LOWER(TRIM(ri.name)), ga.id
FROM recipe_ingredients ri
JOIN recipes r ON r.id = ri.recipe_id AND r.deleted_at IS NULL
JOIN grocery_aisles ga
  ON ga.family_id = ri.family_id AND ga.name = TRIM(ri.category)
WHERE ri.category IS NOT NULL AND TRIM(ri.category) <> '';

-- Bootstrap 3/3: phantom-section cleanup. Grocery items whose section name
-- matches one of the family's new aisles were created by the old recipe
-- import leaking ingredient categories into `category`; return them to the
-- sectionless main list (their aisle knowledge now lives in item_aisles).
-- Tombstoned (soft-deleted) items are left untouched.
UPDATE grocery_items
SET category = NULL
WHERE category IS NOT NULL
  AND deleted_at IS NULL
  AND EXISTS (
      SELECT 1 FROM grocery_aisles ga
      WHERE ga.family_id = grocery_items.family_id
        AND ga.name = grocery_items.category
  );
