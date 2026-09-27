//! Aisle ("hylly") types: the family's store aisles in walking order and the
//! learned item-name → aisle mapping used to sort the main grocery list.
//! The mapping is keyed by item NAME (lowercased), not item id — assigning
//! "maito" once teaches the app where every future "maito" belongs.

use serde::{Deserialize, Serialize};

/// One aisle. `position` is the family's walking order (1-based).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Aisle {
    pub id: i64,
    pub name: String,
    pub position: i64,
}

/// One learned mapping row: lowercased item name → aisle id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemAisle {
    pub item_name: String,
    pub aisle_id: i64,
}

/// Response of GET /api/aisles: aisles in position order + the full map +
/// the family's starred item names (lowercased).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AislesResponse {
    pub aisles: Vec<Aisle>,
    pub map: Vec<ItemAisle>,
    /// Lowercased names the family has starred. `#[serde(default)]` keeps old
    /// cached PWA payloads (which lack the key) parsing — they read as empty.
    #[serde(default)]
    pub favorites: Vec<String>,
}

/// Body for POST /api/aisles (create) and PUT /api/aisles/{id} (rename).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaveAisleRequest {
    pub name: String,
}

/// Body for PUT /api/aisles/order: the FULL id list in the new order.
/// Sending the whole list makes the request idempotent — a duplicate or
/// late-arriving reorder restates the same total order instead of applying
/// a relative move twice.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReorderAislesRequest {
    pub ids: Vec<i64>,
}

/// Body for PUT /api/aisles/map. `aisle_id: None` clears the mapping
/// (item returns to "Lajittelematon").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetItemAisleRequest {
    pub item_name: String,
    pub aisle_id: Option<i64>,
}

/// Body for PUT /api/aisles/favorite. `starred=false` removes the mark.
/// Per-name like the aisle map: starring "maito" applies to every "maito".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetFavoriteRequest {
    pub item_name: String,
    pub starred: bool,
}

pub fn validate_aisle_name(name: &str) -> Result<(), &'static str> {
    let len = name.trim().chars().count();
    if len == 0 {
        return Err("Hyllyn nimi ei voi olla tyhjä.");
    }
    if len > 40 {
        return Err("Hyllyn nimi saa olla enintään 40 merkkiä.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_aisle_names_pass() {
        assert!(validate_aisle_name("Maito").is_ok());
        assert!(validate_aisle_name("Hedelmät & vihannekset").is_ok());
        assert!(validate_aisle_name(&"x".repeat(40)).is_ok());
    }

    #[test]
    fn empty_aisle_name_is_rejected() {
        assert!(validate_aisle_name("").is_err());
        assert!(validate_aisle_name("   ").is_err());
    }

    #[test]
    fn overlong_aisle_name_is_rejected() {
        assert!(validate_aisle_name(&"x".repeat(41)).is_err());
    }

    #[test]
    fn aisles_response_serde_round_trip() {
        let resp = AislesResponse {
            aisles: vec![Aisle {
                id: 1,
                name: "Maito".into(),
                position: 1,
            }],
            map: vec![ItemAisle {
                item_name: "maito".into(),
                aisle_id: 1,
            }],
            favorites: vec![],
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: AislesResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(resp, back);
    }

    /// `aisle_id: null` in JSON must deserialize to None (the "clear" form).
    #[test]
    fn set_item_aisle_null_clears() {
        let req: SetItemAisleRequest =
            serde_json::from_str(r#"{"item_name":"maito","aisle_id":null}"#).unwrap();
        assert_eq!(req.aisle_id, None);
    }

    #[test]
    fn aisles_response_without_favorites_defaults_empty() {
        // Old cached PWA payloads have no `favorites` key — must default to [].
        let resp: AislesResponse = serde_json::from_str(r#"{"aisles":[],"map":[]}"#).unwrap();
        assert!(resp.favorites.is_empty());
    }

    #[test]
    fn set_favorite_request_round_trip() {
        let req = SetFavoriteRequest {
            item_name: "maito".into(),
            starred: true,
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: SetFavoriteRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.item_name, "maito");
        assert!(back.starred);
    }
}
