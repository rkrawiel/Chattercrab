//! REST request and response bodies.

use serde::{Deserialize, Serialize};

use crate::{ErrorCode, RoomId, RoomName, UnixMillis};

/// Body of `GET /health`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthResponse {
    /// Always `"ok"` when the server answers.
    pub status: String,
    /// Server version (crate version).
    pub version: String,
}

/// One entry of `GET /rooms`, also the body of a successful `POST /rooms`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomSummary {
    /// Room identifier, used in `/ws/{room_id}`.
    pub id: RoomId,
    /// Human-readable name.
    pub name: RoomName,
    /// Number of currently joined members.
    pub member_count: usize,
    /// Creation time.
    pub created_at: UnixMillis,
}

/// Body of `POST /rooms`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateRoomRequest {
    /// Name of the room to create.
    pub name: RoomName,
}

/// Body of `GET /ice-servers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IceServers {
    /// ICE server URLs. Chattercrab sends exactly one `stun:` URL and no TURN URLs.
    pub urls: Vec<String>,
}

/// Error body returned by REST endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    /// Machine-readable code.
    pub code: ErrorCode,
    /// Human-readable description.
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<T>(value: &T)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_string(value).unwrap();
        let back: T = serde_json::from_str(&json).unwrap();
        assert_eq!(&back, value, "roundtrip of {json}");
    }

    #[test]
    fn rest_models_roundtrip() {
        roundtrip(&HealthResponse {
            status: "ok".into(),
            version: "0.1.0".into(),
        });
        roundtrip(&RoomSummary {
            id: RoomId::new("k3v9q2m7x1ab").unwrap(),
            name: RoomName::new("standup").unwrap(),
            member_count: 3,
            created_at: 1_793_875_200_000,
        });
        roundtrip(&CreateRoomRequest {
            name: RoomName::new("standup").unwrap(),
        });
        roundtrip(&IceServers {
            urls: vec!["stun:stun.l.google.com:19302".into()],
        });
        roundtrip(&ApiError {
            code: ErrorCode::RoomFull,
            message: "room is full".into(),
        });
    }

    #[test]
    fn create_room_rejects_invalid_name() {
        assert!(serde_json::from_str::<CreateRoomRequest>(r#"{"name":"  "}"#).is_err());
    }

    #[test]
    fn room_summary_wire_shape() {
        let s = RoomSummary {
            id: RoomId::new("abc").unwrap(),
            name: RoomName::new("n").unwrap(),
            member_count: 0,
            created_at: 5,
        };
        assert_eq!(
            serde_json::to_string(&s).unwrap(),
            r#"{"id":"abc","name":"n","member_count":0,"created_at":5}"#
        );
    }
}
