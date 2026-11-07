//! HTTP routes.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, Path, State, WebSocketUpgrade};
use axum::http::{Method, StatusCode, header};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use chattercrab_protocol::{
    ApiError, ErrorCode, HealthResponse, IceServers, MAX_WS_MESSAGE_BYTES, RoomId, RoomName,
    RoomSummary,
};
use serde::Deserialize;
use tower_http::cors::{Any, CorsLayer};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;

use crate::auth;
use crate::state::{AppState, CreateRoomError};
use crate::ws;

/// Maximum REST request body size.
pub const MAX_BODY_BYTES: usize = 16 * 1024;

/// Builds the application router.
pub fn router(state: Arc<AppState>) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST])
        .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION]);
    // CORS is outside auth so a preflight OPTIONS is answered before credentials are required.
    Router::new()
        .route("/health", get(health))
        .route("/rooms", get(list_rooms).post(create_room))
        .route("/ice-servers", get(ice_servers))
        .route("/ws/{room_id}", get(ws_upgrade))
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        .layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            auth::require_basic_auth,
        ))
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Builds a JSON error response.
fn api_error(status: StatusCode, code: ErrorCode, message: impl Into<String>) -> Response {
    let body = ApiError {
        code,
        message: message.into(),
    };
    (status, Json(body)).into_response()
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
    })
}

async fn list_rooms(State(state): State<Arc<AppState>>) -> Json<Vec<RoomSummary>> {
    Json(state.list_rooms())
}

/// Loose shape of the create body, so an invalid name maps to `invalid_room_name`.
#[derive(Deserialize)]
struct RawCreateRoom {
    name: String,
}

async fn create_room(
    State(state): State<Arc<AppState>>,
    ConnectInfo(remote): ConnectInfo<SocketAddr>,
    body: Bytes,
) -> Response {
    if !state.allow_create_room(remote.ip()) {
        return api_error(
            StatusCode::TOO_MANY_REQUESTS,
            ErrorCode::RateLimited,
            "too many rooms created, try again later",
        );
    }
    let raw: RawCreateRoom = match serde_json::from_slice(&body) {
        Ok(raw) => raw,
        Err(err) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                ErrorCode::BadRequest,
                err.to_string(),
            );
        }
    };
    let name = match RoomName::new(&raw.name) {
        Ok(name) => name,
        Err(err) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidRoomName,
                err.to_string(),
            );
        }
    };
    match state.create_room(name) {
        Ok(room) => {
            tracing::info!(room = %room.id, name = %room.name, %remote, "room created");
            (StatusCode::CREATED, Json(room.summary())).into_response()
        }
        Err(CreateRoomError::MaxRooms) => api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::MaxRooms,
            format!("server already hosts {} rooms", state.config.max_rooms),
        ),
    }
}

async fn ice_servers(
    State(state): State<Arc<AppState>>,
    ConnectInfo(remote): ConnectInfo<SocketAddr>,
) -> Response {
    if !state.allow_ice_servers(remote.ip()) {
        return api_error(
            StatusCode::TOO_MANY_REQUESTS,
            ErrorCode::RateLimited,
            "too many ICE server requests, try again later",
        );
    }
    Json(IceServers {
        urls: vec![state.config.stun_url.clone()],
    })
    .into_response()
}

async fn ws_upgrade(
    State(state): State<Arc<AppState>>,
    ConnectInfo(remote): ConnectInfo<SocketAddr>,
    Path(room_id): Path<String>,
    upgrade: WebSocketUpgrade,
) -> Response {
    let room = RoomId::new(room_id).ok().and_then(|id| state.room(&id));
    let Some(room) = room else {
        return api_error(
            StatusCode::NOT_FOUND,
            ErrorCode::RoomNotFound,
            "room not found",
        );
    };
    if state.shutdown.is_cancelled() {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Internal,
            "server shutting down",
        );
    }
    let token = state.tracker.token();
    upgrade
        .max_message_size(MAX_WS_MESSAGE_BYTES)
        .max_frame_size(MAX_WS_MESSAGE_BYTES)
        .on_upgrade(move |socket| async move {
            let _token = token;
            ws::serve_connection(state, room, socket, remote).await;
        })
}
