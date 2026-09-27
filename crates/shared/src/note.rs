//! Muistiot (notes): DTOs, validation, and the pure text↔blocks converter.
//!
//! A topic is one document of ordered blocks: free-text lines and checklist
//! items mixed. The whole note is EDITED as plain text with `- [ ]` / `- [x]`
//! markers and STORED as one row per block, so checkbox taps can be per-row
//! idempotent updates (safe under flaky networks and two simultaneous users).
//! `parse_note_text`/`render_note_text` are exact inverses for any state that
//! can come out of the database, which is what makes the edit round-trip safe:
//! checked state rides the `[x]` markers through the textarea and back.

use serde::{Deserialize, Serialize};

pub const TITLE_MAX: usize = 200;
pub const BLOCKS_MAX: usize = 500;
pub const BLOCK_CONTENT_MAX: usize = 1000;

/// What one block is. Serialized lowercase ("text"/"check") — the same
/// strings the DB column stores, see `as_str`/`from_db`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BlockKind {
    Text,
    Check,
}

impl BlockKind {
    /// DB representation (the CHECK constraint in migration 0010 matches).
    pub fn as_str(self) -> &'static str {
        match self {
            BlockKind::Text => "text",
            BlockKind::Check => "check",
        }
    }

    /// Parse the DB string. Unknown values (impossible under the CHECK
    /// constraint) degrade to Text rather than erroring.
    pub fn from_db(s: &str) -> Self {
        if s == "check" {
            BlockKind::Check
        } else {
            BlockKind::Text
        }
    }
}

/// One stored block, as the API returns it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoteBlock {
    pub id: i64,
    pub kind: BlockKind,
    pub content: String,
    pub checked: bool,
}

/// One block in a save request — no id, the server assigns fresh ones on
/// every replace-all edit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlockDraft {
    pub kind: BlockKind,
    pub content: String,
    pub checked: bool,
}

/// Topic-list row: counts drive the "3/7" progress label.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoteTopicSummary {
    pub id: i64,
    pub title: String,
    pub check_done: i64,
    pub check_total: i64,
    pub updated_at: String, // RFC3339 UTC
}

/// Full topic as GET/PUT return it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoteTopic {
    pub id: i64,
    pub title: String,
    pub created_by: i64,
    pub updated_at: String,
    pub blocks: Vec<NoteBlock>,
}

/// Body for POST /api/notes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateTopicRequest {
    pub title: String,
}

/// Body for PUT /api/notes/{id} — replaces title AND all blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SaveTopicRequest {
    pub title: String,
    pub blocks: Vec<BlockDraft>,
}

/// Body for PUT /api/notes/{id}/blocks/{block_id}/checked. SET semantics
/// (not toggle) so a duplicate or late request can't undo a tap — same
/// reasoning as grocery's SetCheckedRequest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetBlockCheckedRequest {
    pub checked: bool,
}

pub fn validate_topic_title(title: &str) -> Result<(), &'static str> {
    let len = title.trim().chars().count();
    if len == 0 || len > TITLE_MAX {
        return Err("Otsikon tulee olla 1–200 merkkiä.");
    }
    Ok(())
}

/// Bounds only — content is free-form by design.
pub fn validate_blocks(blocks: &[BlockDraft]) -> Result<(), &'static str> {
    if blocks.len() > BLOCKS_MAX {
        return Err("Muistiossa on liian monta riviä.");
    }
    if blocks
        .iter()
        .any(|b| b.content.chars().count() > BLOCK_CONTENT_MAX)
    {
        return Err("Rivi on liian pitkä.");
    }
    Ok(())
}

/// Plain text → blocks. `- [ ] x` / `- [x] x` (case-insensitive x) become
/// check blocks; every other line — blank lines included — is a text block.
/// A text line that ITSELF starts with the marker can't be stored: it would
/// parse as a check block right here, so stored text never needs escaping.
///
/// Trailing-newline normalization: `split('\n')` (NOT `.lines()`, which would
/// eat every trailing newline silently) turns a textarea's final "\n" into one
/// empty last segment; exactly that one is popped. Deliberate blank lines
/// anywhere else survive, and `render_note_text` (plain `join("\n")`) is the
/// exact inverse for anything the DB can hold — a trailing empty text block is
/// unreachable through this parser, so the pair round-trips.
pub fn parse_note_text(text: &str) -> Vec<BlockDraft> {
    let mut drafts: Vec<BlockDraft> = text
        .split('\n')
        .map(|line| {
            let check = |checked: bool, rest: &str| BlockDraft {
                kind: BlockKind::Check,
                // One separator space belongs to the marker, not the content.
                content: rest.strip_prefix(' ').unwrap_or(rest).to_owned(),
                checked,
            };
            if let Some(rest) = line.strip_prefix("- [ ]") {
                check(false, rest)
            } else if let Some(rest) = line
                .strip_prefix("- [x]")
                .or_else(|| line.strip_prefix("- [X]"))
            {
                check(true, rest)
            } else {
                BlockDraft {
                    kind: BlockKind::Text,
                    content: line.to_owned(),
                    checked: false,
                }
            }
        })
        .collect();
    // A textarea ends with one dangling newline; split gives it as a final
    // empty segment. Drop exactly one — deliberate blank lines survive
    // because only the LAST segment is popped.
    if drafts
        .last()
        .is_some_and(|d| d.kind == BlockKind::Text && d.content.is_empty())
    {
        drafts.pop();
    }
    drafts
}

/// Blocks → plain text for the editor textarea. Exact inverse of
/// `parse_note_text` for anything the DB can hold.
pub fn render_note_text(blocks: &[NoteBlock]) -> String {
    blocks
        .iter()
        .map(|b| match b.kind {
            BlockKind::Check if b.checked => format!("- [x] {}", b.content),
            BlockKind::Check => format!("- [ ] {}", b.content),
            BlockKind::Text => b.content.clone(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(kind: BlockKind, content: &str, checked: bool) -> BlockDraft {
        BlockDraft {
            kind,
            content: content.to_owned(),
            checked,
        }
    }

    #[test]
    fn parse_mixes_text_and_checks() {
        let drafts = parse_note_text(
            "Muista pappi!\n- [ ] tilaa puvut\n- [x] varaa kampaaja\n\nBudjetti 2000e",
        );
        assert_eq!(
            drafts,
            vec![
                draft(BlockKind::Text, "Muista pappi!", false),
                draft(BlockKind::Check, "tilaa puvut", false),
                draft(BlockKind::Check, "varaa kampaaja", true),
                draft(BlockKind::Text, "", false),
                draft(BlockKind::Text, "Budjetti 2000e", false),
            ]
        );
    }

    #[test]
    fn parse_is_case_insensitive_on_x_and_keeps_plain_dashes_as_text() {
        let drafts = parse_note_text("- [X] iso x\n- ei checkbox\n-[ ] ei välilyöntiä");
        assert_eq!(drafts[0], draft(BlockKind::Check, "iso x", true));
        assert_eq!(drafts[1].kind, BlockKind::Text); // "- " alone is prose
        assert_eq!(drafts[2].kind, BlockKind::Text); // malformed marker stays text
    }

    #[test]
    fn render_then_parse_round_trips() {
        // Middle blank lines are real content and must survive the trip.
        let blocks = vec![
            NoteBlock {
                id: 1,
                kind: BlockKind::Text,
                content: "Otsikkorivi".into(),
                checked: false,
            },
            NoteBlock {
                id: 2,
                kind: BlockKind::Text,
                content: "".into(),
                checked: false,
            },
            NoteBlock {
                id: 3,
                kind: BlockKind::Check,
                content: "tehty juttu".into(),
                checked: true,
            },
            NoteBlock {
                id: 4,
                kind: BlockKind::Check,
                content: "tekemättä".into(),
                checked: false,
            },
        ];
        let text = render_note_text(&blocks);
        assert_eq!(text, "Otsikkorivi\n\n- [x] tehty juttu\n- [ ] tekemättä");
        let reparsed = parse_note_text(&text);
        let expected: Vec<BlockDraft> = blocks
            .iter()
            .map(|b| BlockDraft {
                kind: b.kind,
                content: b.content.clone(),
                checked: b.checked,
            })
            .collect();
        assert_eq!(reparsed, expected);
    }

    #[test]
    fn parse_drops_exactly_one_trailing_textarea_newline() {
        // A textarea's final "\n" is not content — but only ONE is dropped,
        // so a deliberate trailing blank line ("a\n\n") still yields a blank.
        assert_eq!(
            parse_note_text("a\n"),
            vec![draft(BlockKind::Text, "a", false)]
        );
        assert_eq!(
            parse_note_text("a\n\n"),
            vec![
                draft(BlockKind::Text, "a", false),
                draft(BlockKind::Text, "", false)
            ]
        );
    }

    #[test]
    fn empty_check_content_round_trips() {
        let text = render_note_text(&[NoteBlock {
            id: 1,
            kind: BlockKind::Check,
            content: "".into(),
            checked: false,
        }]);
        assert_eq!(
            parse_note_text(&text),
            vec![draft(BlockKind::Check, "", false)]
        );
    }

    #[test]
    fn title_bounds() {
        assert!(validate_topic_title("MENS jobs before wedding").is_ok());
        assert!(validate_topic_title("   ").is_err());
        assert!(validate_topic_title(&"x".repeat(201)).is_err());
        assert!(validate_topic_title(&"x".repeat(200)).is_ok());
    }

    #[test]
    fn block_bounds() {
        let ok = vec![draft(BlockKind::Text, "jee", false)];
        assert!(validate_blocks(&ok).is_ok());
        let too_many: Vec<BlockDraft> = (0..501)
            .map(|_| draft(BlockKind::Text, "r", false))
            .collect();
        assert!(validate_blocks(&too_many).is_err());
        let too_long = vec![draft(BlockKind::Text, &"x".repeat(1001), false)];
        assert!(validate_blocks(&too_long).is_err());
    }
}
