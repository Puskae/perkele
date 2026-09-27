-- Per-name grocery favorites ("Suosikit"). Keyed by NAME like item_aisles,
-- not grocery_items.id: starring "maito" once marks every future "maito".
-- Family-wide; starts empty.
CREATE TABLE item_favorites (
    family_id INTEGER NOT NULL REFERENCES families(id),
    item_name TEXT    NOT NULL,  -- lowercased + trimmed at write time (in Rust)
    UNIQUE (family_id, item_name)
);
