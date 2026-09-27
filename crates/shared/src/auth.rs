//! Auth & family DTOs shared by the server and the app.
//!
//! This module is compiled into the WASM frontend, so it must stay free of
//! server-only dependencies (no sqlx, no argon2). It holds the request/response
//! shapes and the validation rules — the latter so the client can give instant
//! feedback while the server still enforces them authoritatively.

use serde::{Deserialize, Serialize};

/// A member's permission level within their family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Full control: manage members, invites, everything.
    Admin,
    /// A regular adult member.
    Member,
    /// Limited access for children.
    Kid,
}

impl Role {
    /// The canonical lowercase string stored in the database `role` column.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Member => "member",
            Role::Kid => "kid",
        }
    }

    /// Parse the value read back from the database. Returns `None` for anything
    /// the CHECK constraint shouldn't have allowed in the first place.
    pub fn from_db(s: &str) -> Option<Self> {
        match s {
            "admin" => Some(Role::Admin),
            "member" => Some(Role::Member),
            "kid" => Some(Role::Kid),
            _ => None,
        }
    }

    pub fn is_admin(self) -> bool {
        matches!(self, Role::Admin)
    }
}

/// `GET /api/setup/status` — does the server need first-run setup?
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupStatus {
    pub needs_setup: bool,
}

/// `POST /api/setup` — create the first family and its admin. Only succeeds when
/// no family exists yet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupRequest {
    /// The one-time setup token the server prints to its log at startup. Proves
    /// the caller can read the server's logs, i.e. is whoever runs it — so a
    /// stranger who finds a fresh install first can't claim it.
    /// `#[serde(default)]`: a missing field becomes `""` (and is rejected by the
    /// server with 403) rather than failing JSON parsing with a vaguer 422.
    #[serde(default)]
    pub setup_token: String,
    pub family_name: String,
    pub username: String,
    pub display_name: String,
    pub password: String,
}

/// `POST /api/auth/login`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

/// A member as seen by the client (never includes the password hash).
/// Also the body of `GET /api/me`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserView {
    pub id: i64,
    pub family_id: i64,
    pub username: String,
    pub display_name: String,
    pub role: Role,
}

/// `POST /api/family/invites` (admin only) — mint an invite code for a new
/// member who will join with the given role.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateInviteRequest {
    pub role: Role,
}

/// Response to creating an invite. The plaintext `code` is shown to the admin
/// exactly once — only its hash is stored — so they must hand it over now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteResponse {
    pub code: String,
    /// RFC 3339 timestamp (UTC) after which the code stops working.
    pub expires_at: String,
}

/// `POST /api/auth/redeem` — a new member joins by entering an invite code and
/// choosing their own credentials. The role comes from the invite, not the
/// request, so a redeemer can't grant themselves admin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedeemRequest {
    pub code: String,
    pub username: String,
    pub display_name: String,
    pub password: String,
}

/// Minimum password length, shared so client and server agree.
pub const MIN_PASSWORD_LEN: usize = 8;
/// Maximum password length. Argon2's cost grows with input size, so an
/// unbounded password is a cheap way to make the server burn CPU. 256 chars
/// is far beyond any passphrase a person types or a manager generates.
pub const MAX_PASSWORD_LEN: usize = 256;

/// Usernames: 2–32 chars, lowercase letters / digits / `-` / `_`.
/// Kept deliberately strict so they're easy to type and unambiguous.
pub fn validate_username(username: &str) -> Result<(), &'static str> {
    let len = username.chars().count();
    if !(2..=32).contains(&len) {
        return Err("Käyttäjänimen tulee olla 2–32 merkkiä.");
    }
    if !username
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        return Err(
            "Käyttäjänimi voi sisältää vain pieniä kirjaimia, numeroita sekä merkkejä - ja _.",
        );
    }
    Ok(())
}

/// Coerce arbitrary text into a valid username: lowercase, keeping only the
/// allowed characters. Pairs with [`validate_username`] — the client calls this
/// on each keystroke so a member can't submit, say, a capitalised name ("Matti"
/// becomes "matti") and hit a confusing validation error.
pub fn normalize_username(raw: &str) -> String {
    raw.chars()
        .map(|c| c.to_ascii_lowercase())
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-' || *c == '_')
        .collect()
}

pub fn validate_password(password: &str) -> Result<(), &'static str> {
    let len = password.chars().count();
    if len < MIN_PASSWORD_LEN {
        return Err("Salasanan tulee olla vähintään 8 merkkiä.");
    }
    if len > MAX_PASSWORD_LEN {
        return Err("Salasana saa olla enintään 256 merkkiä.");
    }
    Ok(())
}

/// A free-text human name (display name or family name): 1–64 chars after
/// trimming surrounding whitespace.
pub fn validate_name(name: &str) -> Result<(), &'static str> {
    let len = name.trim().chars().count();
    if len == 0 || len > 64 {
        return Err("Nimen tulee olla 1–64 merkkiä.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_db_string_round_trips() {
        for role in [Role::Admin, Role::Member, Role::Kid] {
            assert_eq!(Role::from_db(role.as_str()), Some(role));
        }
        assert_eq!(Role::from_db("president"), None);
    }

    #[test]
    fn usernames_enforce_charset_and_length() {
        assert!(validate_username("mikko").is_ok());
        assert!(validate_username("pikku-matti_2").is_ok());
        assert!(validate_username("a").is_err()); // too short
        assert!(validate_username("Mikko").is_err()); // uppercase
        assert!(validate_username("mikko virtanen").is_err()); // space
    }

    #[test]
    fn passwords_enforce_minimum_length() {
        assert!(validate_password("hunter2!").is_ok());
        assert!(validate_password("short").is_err());
    }

    #[test]
    fn passwords_enforce_maximum_length() {
        assert!(validate_password(&"x".repeat(MAX_PASSWORD_LEN)).is_ok());
        assert!(validate_password(&"x".repeat(MAX_PASSWORD_LEN + 1)).is_err());
    }

    #[test]
    fn normalize_username_yields_a_valid_username() {
        assert_eq!(normalize_username("Matti"), "matti");
        assert_eq!(normalize_username("Pikku Matti!"), "pikkumatti");
        assert_eq!(normalize_username("mikko_2"), "mikko_2");
        // Normalizing non-empty letter/digit input always passes validation.
        assert!(validate_username(&normalize_username("Matti")).is_ok());
    }

    #[test]
    fn names_reject_empty_and_overlong() {
        assert!(validate_name("Virtanen Family").is_ok());
        assert!(validate_name("   ").is_err());
        assert!(validate_name(&"x".repeat(65)).is_err());
    }
}
