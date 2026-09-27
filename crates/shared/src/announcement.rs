//! Announcements board ("fridge door"): DTOs and validation shared by the
//! server (enforces) and the app (pre-checks).

use serde::{Deserialize, Serialize};

pub const BODY_MAX: usize = 2000;

/// One post as the API returns it. `author_name` is joined from users so the
/// client never needs a second lookup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Announcement {
    pub id: i64,
    pub body: String,
    pub pinned: bool,
    pub created_by: i64,
    pub author_name: String,
    pub created_at: String, // RFC3339 UTC
    pub updated_at: String,
}

/// Body for POST (create) and PUT (edit).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SaveAnnouncementRequest {
    pub body: String,
}

/// Body for PUT /api/announcements/{id}/pinned (admin only).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetPinnedRequest {
    pub pinned: bool,
}

/// Same convention as the auth validators: Finnish message, both sides use it.
pub fn validate_body(body: &str) -> Result<(), &'static str> {
    let len = body.trim().chars().count();
    if len == 0 || len > BODY_MAX {
        return Err("Ilmoituksen tulee olla 1–2000 merkkiä.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_bounds() {
        assert!(validate_body("Muistakaa mummon synttärit!").is_ok());
        assert!(validate_body("   ").is_err());
        assert!(validate_body(&"x".repeat(2001)).is_err());
        assert!(validate_body(&"x".repeat(2000)).is_ok());
    }
}
