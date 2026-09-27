use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroceryItem {
    pub id: i64,
    pub name: String,
    pub qty: Option<String>,
    pub unit: Option<String>,
    pub category: Option<String>,
    pub checked: bool,
    pub added_by: i64,
    pub recipe_id: Option<i64>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddItemRequest {
    pub name: String,
    pub qty: Option<String>,
    pub unit: Option<String>,
    /// Optional section (e.g. "Apteekki") the item is filed under. Stored in
    /// the `category` column. `#[serde(default)]` keeps old cached PWA clients
    /// working: if the key is missing from the JSON entirely, serde uses
    /// `Default::default()` (None) instead of rejecting the request.
    #[serde(default)]
    pub section: Option<String>,
}

/// Body of `PUT /api/grocery/{id}` — edits an existing item's text fields.
///
/// `qty`/`unit` are `Option`: a missing key or `null` clears the field.
/// `#[serde(default)]` lets a minimal `{"name":"..."}` body parse (the field
/// falls back to `None`) instead of failing, matching how `AddItemRequest`
/// tolerates old clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditItemRequest {
    pub name: String,
    #[serde(default)]
    pub qty: Option<String>,
    #[serde(default)]
    pub unit: Option<String>,
}

/// Body of `PUT /api/grocery/{id}/check`.
///
/// Carries the *target* state ("set semantics") instead of flipping whatever
/// is there ("toggle semantics"). This makes the request idempotent: on a
/// flaky connection a duplicate or late-arriving request can't undo the
/// user's tap, it just re-states the same intent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetCheckedRequest {
    pub checked: bool,
}

/// Response from `PUT /api/grocery/{id}/check`.
///
/// Carries the sync seq the check-off wrote, so the client can raise its
/// "last applied seq" watermark immediately. Without it, a sync snapshot
/// fetched *before* the write landed could be applied *after* it and briefly
/// un-check the item the user just tapped (visible as flicker).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetCheckedResponse {
    pub seq: i64,
}

/// Response from `GET /api/grocery/sync`.
///
/// Returns the current live list and the server's latest sync sequence number.
/// The client stores `seq` in localStorage and compares it on reconnect to
/// decide whether re-fetching the list is needed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncResponse {
    pub items: Vec<GroceryItem>,
    /// Current MAX(seq) in the family's sync_log (0 if no mutations yet).
    pub seq: i64,
}

pub fn validate_section_name(name: &str) -> Result<(), &'static str> {
    let len = name.trim().chars().count();
    if len == 0 {
        return Err("Osion nimi ei voi olla tyhjä.");
    }
    if len > 40 {
        return Err("Osion nimi saa olla enintään 40 merkkiä.");
    }
    Ok(())
}

pub fn validate_grocery_item_name(name: &str) -> Result<(), &'static str> {
    let len = name.trim().chars().count();
    if len == 0 {
        return Err("Tuotteen nimi ei voi olla tyhjä.");
    }
    if len > 100 {
        return Err("Tuotteen nimi saa olla enintään 100 merkkiä.");
    }
    Ok(())
}

/// Max length of a quantity ("1,5") or unit ("rkl") string — shared by
/// grocery items and recipe ingredients (which become grocery items).
pub const QTY_UNIT_MAX: usize = 32;

/// Optional qty/unit length check for add, edit and recipe ingredients.
pub fn validate_qty_unit(qty: Option<&str>, unit: Option<&str>) -> Result<(), &'static str> {
    // `is_some_and` = "Some AND the closure says true"; None passes.
    let long = |s: Option<&str>| s.is_some_and(|s| s.chars().count() > QTY_UNIT_MAX);
    if long(qty) {
        return Err("Määrä saa olla enintään 32 merkkiä.");
    }
    if long(unit) {
        return Err("Yksikkö saa olla enintään 32 merkkiä.");
    }
    Ok(())
}

/// Sort key for Finnish alphabetical order.
///
/// Rust's default `str` ordering compares Unicode code points, which puts
/// å (U+00E5), ä (U+00E4), ö (U+00F6) *after* `z` but in the wrong order
/// among themselves (ä before å). Finnish order is `… x y z å ä ö`. We
/// lowercase, then remap those three letters to sequences starting with `{`
/// — the code point right after `z` — so they sort last and in the right
/// order. Every other char keeps its lowercased self.
pub fn finnish_sort_key(name: &str) -> String {
    let mut key = String::with_capacity(name.len());
    for ch in name.to_lowercase().chars() {
        match ch {
            'å' => key.push_str("{1"),
            'ä' => key.push_str("{2"),
            'ö' => key.push_str("{3"),
            other => key.push(other),
        }
    }
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_names_pass() {
        assert!(validate_grocery_item_name("milk").is_ok());
        assert!(validate_grocery_item_name("Oat milk (1L)").is_ok());
        assert!(validate_grocery_item_name(&"x".repeat(100)).is_ok());
    }

    #[test]
    fn empty_name_is_rejected() {
        assert!(validate_grocery_item_name("").is_err());
        assert!(validate_grocery_item_name("   ").is_err());
    }

    #[test]
    fn overlong_name_is_rejected() {
        assert!(validate_grocery_item_name(&"x".repeat(101)).is_err());
    }

    #[test]
    fn valid_section_names_pass() {
        assert!(validate_section_name("Apteekki").is_ok());
        assert!(validate_section_name(&"x".repeat(40)).is_ok());
    }

    #[test]
    fn empty_section_name_is_rejected() {
        assert!(validate_section_name("").is_err());
        assert!(validate_section_name("   ").is_err());
    }

    #[test]
    fn overlong_section_name_is_rejected() {
        assert!(validate_section_name(&"x".repeat(41)).is_err());
    }

    /// Old cached PWA clients send AddItemRequest without a `section` key at
    /// all — the field must default to None instead of failing to parse.
    #[test]
    fn add_item_request_without_section_deserializes() {
        let req: AddItemRequest =
            serde_json::from_str(r#"{"name":"milk","qty":null,"unit":null}"#).unwrap();
        assert_eq!(req.section, None);
    }

    #[test]
    fn grocery_item_serde_round_trip() {
        let item = GroceryItem {
            id: 1,
            name: "milk".into(),
            qty: Some("2".into()),
            unit: Some("L".into()),
            category: None,
            checked: false,
            added_by: 1,
            recipe_id: None,
            created_at: "2026-01-01T00:00:00Z".into(),
        };
        let json = serde_json::to_string(&item).unwrap();
        let back: GroceryItem = serde_json::from_str(&json).unwrap();
        assert_eq!(item, back);
    }

    #[test]
    fn edit_item_request_round_trip() {
        let req = EditItemRequest {
            name: "Kevytmaito".into(),
            qty: Some("2".into()),
            unit: Some("L".into()),
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: EditItemRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "Kevytmaito");
        assert_eq!(back.qty.as_deref(), Some("2"));
        assert_eq!(back.unit.as_deref(), Some("L"));
    }

    #[test]
    fn edit_item_request_missing_qty_and_unit_is_none() {
        // A body with just a name must parse, leaving qty/unit as None.
        let req: EditItemRequest = serde_json::from_str(r#"{"name":"Maito"}"#).unwrap();
        assert_eq!(req.name, "Maito");
        assert_eq!(req.qty, None);
        assert_eq!(req.unit, None);
    }

    #[test]
    fn finnish_sort_key_orders_a_o_after_z() {
        let mut names = ["öljy", "Apple", "ähky", "åke", "zebra", "banaani"];
        names.sort_by_key(|s| finnish_sort_key(s));
        assert_eq!(names, ["Apple", "banaani", "zebra", "åke", "ähky", "öljy"]);
    }

    #[test]
    fn qty_unit_limits() {
        assert!(validate_qty_unit(None, None).is_ok());
        assert!(validate_qty_unit(Some("1,5"), Some("rkl")).is_ok());
        let max = "x".repeat(QTY_UNIT_MAX);
        let over = "x".repeat(QTY_UNIT_MAX + 1);
        assert!(validate_qty_unit(Some(&max), Some(&max)).is_ok());
        assert!(validate_qty_unit(Some(&over), None).is_err());
        assert!(validate_qty_unit(None, Some(&over)).is_err());
    }
}
