//! Perhechatti (family group chat): DTOs and validation shared by the
//! server (enforces) and the app (pre-checks).

use serde::{Deserialize, Serialize};

pub const BODY_MAX: usize = 2000;
/// Default page size for GET /api/chat/messages.
pub const PAGE_SIZE: i64 = 50;
/// The fixed reaction set; anything else is rejected on both sides.
pub const REACTION_EMOJI: [&str; 5] = ["👍", "😂", "❤️", "😮", "👎"];

/// One user's reaction on one message. The client groups these into
/// `emoji × count` chips and highlights its own.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reaction {
    pub user_id: i64,
    pub emoji: String,
}

/// One message as the API returns it. `author_name` is joined from users so
/// the client never needs a second lookup (same as announcements).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: i64,
    pub body: String,
    pub author_id: i64,
    pub author_name: String,
    pub created_at: String, // RFC3339 UTC instant
    pub reactions: Vec<Reaction>,
}

/// One page of chat. The page is the NEWEST slice of history before
/// `?before=`, but `messages` inside it is ascending by id (oldest first)
/// so the client renders top-to-bottom without re-sorting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatPage {
    pub messages: Vec<ChatMessage>,
    /// True when older messages exist beyond this page.
    pub has_more: bool,
    /// Newest non-deleted message id in the family (0 = empty room).
    pub latest_id: i64,
    /// The caller's read marker (0 = never read anything).
    pub last_read_id: i64,
    /// Non-deleted messages newer than the marker — feeds the nav badge.
    pub unread_count: i64,
}

/// Body for POST /api/chat/messages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SendMessageRequest {
    pub body: String,
}

/// Body for PUT /api/chat/reactions — toggles the caller's reaction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToggleReactionRequest {
    pub message_id: i64,
    pub emoji: String,
}

/// Body for PUT /api/chat/read — the marker only ever moves forward.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MarkReadRequest {
    pub last_read_id: i64,
}

/// Same convention as the other validators: Finnish message, both sides use it.
pub fn validate_body(body: &str) -> Result<(), &'static str> {
    let len = body.trim().chars().count();
    if len == 0 || len > BODY_MAX {
        return Err("Viestin tulee olla 1–2000 merkkiä.");
    }
    Ok(())
}

pub fn validate_emoji(emoji: &str) -> Result<(), &'static str> {
    if !REACTION_EMOJI.contains(&emoji) {
        return Err("Tuntematon reaktio.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_bounds() {
        assert!(validate_body("Kuka söi viimeisen jäätelön?!").is_ok());
        assert!(validate_body("   ").is_err());
        assert!(validate_body(&"x".repeat(2001)).is_err());
        assert!(validate_body(&"x".repeat(2000)).is_ok());
    }

    #[test]
    fn emoji_set_is_closed() {
        for e in REACTION_EMOJI {
            assert!(validate_emoji(e).is_ok());
        }
        assert!(validate_emoji("🦄").is_err());
        assert!(validate_emoji("").is_err());
    }
}
