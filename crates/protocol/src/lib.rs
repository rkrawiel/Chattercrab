//! Shared wire types for Chattercrab.
//!
//! This crate defines every REST body and WebSocket message exchanged between the
//! `chattercrab` client and the `chattercrab-signal` server, plus the validation rules
//! for user-supplied strings. It performs no I/O. See `docs/PROTOCOL.md` for the
//! human-readable specification.

mod ids;
mod rest;
mod ws;

pub use ids::{
    DisplayName, MAX_DISPLAY_NAME_CHARS, MAX_ROOM_ID_LEN, MAX_ROOM_NAME_CHARS, PeerId, RoomId,
    RoomName,
};
pub use rest::{ApiError, CreateRoomRequest, HealthResponse, IceServers, RoomSummary};
pub use ws::{ChatMessage, ClientMsg, PeerInfo, ServerMsg};

use serde::{Deserialize, Serialize};

/// Maximum chat message length in characters, after trimming.
pub const MAX_CHAT_CHARS: usize = 2000;

/// Maximum size of one WebSocket text frame in bytes.
pub const MAX_WS_MESSAGE_BYTES: usize = 64 * 1024;

/// Number of chat messages a room keeps in memory and replays in `welcome`.
pub const CHAT_HISTORY_LEN: usize = 200;

/// Milliseconds since the Unix epoch (UTC).
pub type UnixMillis = u64;

/// A user-supplied value failed validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    /// Room id is empty, too long, or contains characters outside `[A-Za-z0-9_-]`.
    #[error("room id must be 1-{MAX_ROOM_ID_LEN} characters of [A-Za-z0-9_-]")]
    RoomId,
    /// Peer id is not a UUID.
    #[error("peer id must be a UUID")]
    PeerId,
    /// Display name is empty, too long, or contains control characters.
    #[error(
        "display name must be 1-{MAX_DISPLAY_NAME_CHARS} characters without control characters"
    )]
    DisplayName,
    /// Room name is empty, too long, or contains control characters.
    #[error("room name must be 1-{MAX_ROOM_NAME_CHARS} characters without control characters")]
    RoomName,
    /// Chat text is empty or longer than [`MAX_CHAT_CHARS`].
    #[error("chat text must be 1-{MAX_CHAT_CHARS} characters")]
    ChatText,
}

/// Trims chat text and checks it is 1 to [`MAX_CHAT_CHARS`] characters.
///
/// Returns the trimmed text on success.
pub fn validate_chat_text(text: &str) -> Result<&str, ValidationError> {
    let trimmed = text.trim();
    let count = trimmed.chars().count();
    if count == 0 || count > MAX_CHAT_CHARS {
        return Err(ValidationError::ChatText);
    }
    Ok(trimmed)
}

/// Machine-readable error code used in WebSocket `error` messages and REST error bodies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Malformed JSON, unknown message type, or invalid field.
    BadRequest,
    /// Display name failed validation.
    InvalidDisplayName,
    /// Room name failed validation.
    InvalidRoomName,
    /// Chat text empty or longer than [`MAX_CHAT_CHARS`].
    ChatTooLong,
    /// No room with that id.
    RoomNotFound,
    /// The room already has the maximum number of members.
    RoomFull,
    /// The server already hosts the maximum number of rooms.
    MaxRooms,
    /// Too many requests from this client.
    RateLimited,
    /// Missing or incorrect HTTP Basic credentials.
    Unauthorized,
    /// No `join` arrived within the join timeout.
    JoinTimeout,
    /// A message other than `join` was sent before joining.
    NotJoined,
    /// A second `join` was sent on the same connection.
    AlreadyJoined,
    /// The addressed peer is not in the room.
    PeerNotFound,
    /// `screen_stop` from a peer that is not the screen publisher.
    NotScreenPublisher,
    /// Unexpected server error.
    Internal,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_text_limits() {
        assert_eq!(validate_chat_text("  hi  "), Ok("hi"));
        assert_eq!(validate_chat_text("   "), Err(ValidationError::ChatText));
        let max = "x".repeat(MAX_CHAT_CHARS);
        assert!(validate_chat_text(&max).is_ok());
        let over = "x".repeat(MAX_CHAT_CHARS + 1);
        assert_eq!(validate_chat_text(&over), Err(ValidationError::ChatText));
        // Characters, not bytes.
        let multibyte = "é".repeat(MAX_CHAT_CHARS);
        assert!(validate_chat_text(&multibyte).is_ok());
    }

    #[test]
    fn display_name_rules() {
        assert_eq!(DisplayName::new("  ada ").unwrap().as_str(), "ada");
        assert!(DisplayName::new("").is_err());
        assert!(DisplayName::new("   ").is_err());
        assert!(DisplayName::new(&"a".repeat(32)).is_ok());
        assert!(DisplayName::new(&"a".repeat(33)).is_err());
        assert!(DisplayName::new("bad\nname").is_err());
        assert!(DisplayName::new(&"ł".repeat(32)).is_ok());
    }

    #[test]
    fn display_name_rejected_on_deserialize() {
        assert!(serde_json::from_str::<DisplayName>("\"\"").is_err());
        let name: DisplayName = serde_json::from_str("\" grace \"").unwrap();
        assert_eq!(name.as_str(), "grace");
    }

    #[test]
    fn room_id_rules() {
        assert!(RoomId::new("k3v9q2m7x1ab").is_ok());
        assert!(RoomId::new("a_b-C").is_ok());
        assert!(RoomId::new("").is_err());
        assert!(RoomId::new("has space").is_err());
        assert!(RoomId::new("slash/").is_err());
        assert!(RoomId::new("x".repeat(65)).is_err());
    }

    #[test]
    fn room_name_rules() {
        assert_eq!(RoomName::new(" standup ").unwrap().as_str(), "standup");
        assert!(RoomName::new(&"r".repeat(48)).is_ok());
        assert!(RoomName::new(&"r".repeat(49)).is_err());
    }

    #[test]
    fn peer_id_order_matches_string_order() {
        for _ in 0..200 {
            let a = PeerId::random();
            let b = PeerId::random();
            assert_eq!(a.cmp(&b), a.to_string().cmp(&b.to_string()));
        }
    }

    #[test]
    fn peer_id_parses_and_serializes_lowercase() {
        let id: PeerId = "3F2B8C1E-9D4A-4F6E-8B7A-2C1D0E9F8A7B".parse().unwrap();
        assert_eq!(
            serde_json::to_string(&id).unwrap(),
            "\"3f2b8c1e-9d4a-4f6e-8b7a-2c1d0e9f8a7b\""
        );
        assert!("nope".parse::<PeerId>().is_err());
    }

    #[test]
    fn error_code_is_snake_case() {
        assert_eq!(
            serde_json::to_string(&ErrorCode::NotScreenPublisher).unwrap(),
            "\"not_screen_publisher\""
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::Unauthorized).unwrap(),
            "\"unauthorized\""
        );
    }
}
