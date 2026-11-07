//! REST endpoint integration tests.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use std::num::NonZeroU32;
use std::time::Duration;

use chattercrab_protocol::{ApiError, ErrorCode, HealthResponse};
use chattercrab_server::config::BasicAuth;
use common::{start, start_with};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

#[tokio::test]
async fn health_reports_ok() {
    let server = start().await;
    let resp = server.http.get(server.url("/health")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: HealthResponse = resp.json().await.unwrap();
    assert_eq!(body.status, "ok");
    assert_eq!(body.version, env!("CARGO_PKG_VERSION"));
}

#[tokio::test]
async fn create_and_list_rooms() {
    let server = start().await;
    assert!(server.rooms().await.is_empty());
    let a = server.create_room("alpha").await;
    let b = server.create_room("  beta  ").await;
    assert_eq!(a.member_count, 0);
    assert_eq!(b.name.as_str(), "beta");
    assert_eq!(a.id.as_str().len(), 12);
    assert_ne!(a.id, b.id);
    let rooms = server.rooms().await;
    assert_eq!(rooms.len(), 2);
    assert_eq!(rooms[0].id, a.id, "sorted by created_at");
}

#[tokio::test]
async fn create_room_validation() {
    let server = start().await;
    let resp = server
        .http
        .post(server.url("/rooms"))
        .json(&serde_json::json!({ "name": "   " }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let err: ApiError = resp.json().await.unwrap();
    assert_eq!(err.code, ErrorCode::InvalidRoomName);

    let resp = server
        .http
        .post(server.url("/rooms"))
        .body("not json")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let err: ApiError = resp.json().await.unwrap();
    assert_eq!(err.code, ErrorCode::BadRequest);

    let resp = server
        .http
        .post(server.url("/rooms"))
        .body(vec![b'x'; 32 * 1024])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 413, "body limit");
}

#[tokio::test]
async fn max_rooms_enforced() {
    let server = start_with(|c| c.max_rooms = 2).await;
    server.create_room("one").await;
    server.create_room("two").await;
    let resp = server
        .http
        .post(server.url("/rooms"))
        .json(&serde_json::json!({ "name": "three" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);
    let err: ApiError = resp.json().await.unwrap();
    assert_eq!(err.code, ErrorCode::MaxRooms);
}

#[tokio::test]
async fn create_room_rate_limited() {
    let server = start_with(|c| {
        c.create_room_per_minute = NonZeroU32::new(1).unwrap();
        c.create_room_burst = NonZeroU32::new(2).unwrap();
    })
    .await;
    server.create_room("one").await;
    server.create_room("two").await;
    let resp = server
        .http
        .post(server.url("/rooms"))
        .json(&serde_json::json!({ "name": "three" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 429);
    let err: ApiError = resp.json().await.unwrap();
    assert_eq!(err.code, ErrorCode::RateLimited);
}

#[tokio::test]
async fn ice_servers_returns_configured_stun_url() {
    let server = start().await;
    let resp = server
        .http
        .get(server.url("/ice-servers"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body,
        serde_json::json!({ "urls": ["stun:stun.l.google.com:19302"] })
    );
    assert!(body.get("username").is_none());
    assert!(body.get("credential").is_none());

    let gone = server
        .http
        .get(server.url("/turn-credentials"))
        .send()
        .await
        .unwrap();
    assert_eq!(gone.status(), 404);
}

#[tokio::test]
async fn ice_servers_reflects_custom_stun_url() {
    let server = start_with(|c| {
        c.stun_url = "stun:stun.example.com:3478".into();
    })
    .await;
    let resp = server
        .http
        .get(server.url("/ice-servers"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body,
        serde_json::json!({ "urls": ["stun:stun.example.com:3478"] })
    );
}

#[tokio::test]
async fn ice_servers_rate_limited() {
    let server = start_with(|c| {
        c.ice_per_minute = NonZeroU32::new(1).unwrap();
    })
    .await;
    let first = server
        .http
        .get(server.url("/ice-servers"))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    let resp = server
        .http
        .get(server.url("/ice-servers"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 429);
    let err: ApiError = resp.json().await.unwrap();
    assert_eq!(err.code, ErrorCode::RateLimited);
    assert!(err.message.contains("ICE"));
}

#[tokio::test]
async fn ws_to_unknown_room_is_404() {
    let server = start().await;
    let err = tokio_tungstenite::connect_async(server.ws_url("doesnotexist"))
        .await
        .unwrap_err();
    match err {
        tokio_tungstenite::tungstenite::Error::Http(resp) => assert_eq!(resp.status(), 404),
        other => panic!("expected HTTP 404, got {other:?}"),
    }
}

#[tokio::test]
async fn idle_empty_rooms_are_evicted() {
    let server = start_with(|c| {
        c.room_idle_ttl = Duration::from_millis(200);
        c.sweep_interval = Duration::from_millis(50);
    })
    .await;
    let idle = server.create_room("idle").await;
    let busy = server.create_room("busy").await;
    let (_client, _, _) = server.join(busy.id.as_str(), "ada").await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let rooms = server.rooms().await;
    assert!(rooms.iter().all(|r| r.id != idle.id), "idle room evicted");
    let busy_now = rooms
        .iter()
        .find(|r| r.id == busy.id)
        .expect("busy room kept");
    assert_eq!(busy_now.member_count, 1);
}

const AUTH_USER: &str = "chattercrab";
const AUTH_PASSWORD: &str = "s3cret:with:colon";

fn enable_auth(config: &mut chattercrab_server::Config) {
    config.auth = Some(BasicAuth {
        user: AUTH_USER.into(),
        password: AUTH_PASSWORD.into(),
    });
}

async fn assert_unauthorized(resp: reqwest::Response) {
    assert_eq!(resp.status(), 401);
    let www = resp
        .headers()
        .get(reqwest::header::WWW_AUTHENTICATE)
        .expect("WWW-Authenticate")
        .to_str()
        .unwrap();
    assert_eq!(www, "Basic realm=\"chattercrab\"");
    let err: ApiError = resp.json().await.unwrap();
    assert_eq!(err.code, ErrorCode::Unauthorized);
    assert_eq!(err.message, "unauthorized");
}

#[tokio::test]
async fn health_is_open_when_password_is_set() {
    let server = start_with(enable_auth).await;
    let resp = server.http.get(server.url("/health")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: HealthResponse = resp.json().await.unwrap();
    assert_eq!(body.status, "ok");

    let options = server
        .http
        .request(reqwest::Method::OPTIONS, server.url("/rooms"))
        .header("origin", "http://localhost")
        .header("access-control-request-method", "GET")
        .header(
            "access-control-request-headers",
            "authorization, content-type",
        )
        .send()
        .await
        .unwrap();
    assert_ne!(options.status(), 401);
    let allow = options
        .headers()
        .get("access-control-allow-headers")
        .expect("CORS allow-headers")
        .to_str()
        .unwrap()
        .to_ascii_lowercase();
    assert!(allow.contains("authorization"), "{allow}");
    assert!(allow.contains("content-type"), "{allow}");
}

#[tokio::test]
async fn protected_routes_reject_missing_or_wrong_password() {
    let server = start_with(enable_auth).await;
    for url in ["/rooms", "/ice-servers"] {
        let missing = server.http.get(server.url(url)).send().await.unwrap();
        assert_unauthorized(missing).await;
        let wrong = server
            .http
            .get(server.url(url))
            .basic_auth(AUTH_USER, Some("wrong-password"))
            .send()
            .await
            .unwrap();
        assert_unauthorized(wrong).await;
    }
    let missing = server
        .http
        .post(server.url("/rooms"))
        .json(&serde_json::json!({ "name": "nope" }))
        .send()
        .await
        .unwrap();
    assert_unauthorized(missing).await;
    let wrong = server
        .http
        .post(server.url("/rooms"))
        .basic_auth("someone-else", Some(AUTH_PASSWORD))
        .json(&serde_json::json!({ "name": "nope" }))
        .send()
        .await
        .unwrap();
    assert_unauthorized(wrong).await;
    let bad = server
        .http
        .get(server.url("/rooms"))
        .header("authorization", "Basic !!!")
        .send()
        .await
        .unwrap();
    assert_unauthorized(bad).await;
}

#[tokio::test]
async fn protected_routes_accept_correct_basic_auth() {
    let server = start_with(enable_auth).await;
    let rooms = server
        .http
        .get(server.url("/rooms"))
        .basic_auth(AUTH_USER, Some(AUTH_PASSWORD))
        .send()
        .await
        .unwrap();
    assert_eq!(rooms.status(), 200);
    let list: Vec<serde_json::Value> = rooms.json().await.unwrap();
    assert!(list.is_empty());

    let ice = server
        .http
        .get(server.url("/ice-servers"))
        .basic_auth(AUTH_USER, Some(AUTH_PASSWORD))
        .send()
        .await
        .unwrap();
    assert_eq!(ice.status(), 200);

    let created = server
        .http
        .post(server.url("/rooms"))
        .basic_auth(AUTH_USER, Some(AUTH_PASSWORD))
        .json(&serde_json::json!({ "name": "alpha" }))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 201);
}

#[tokio::test]
async fn websocket_auth_is_checked_before_room_lookup() {
    let server = start_with(enable_auth).await;
    let unknown = server.ws_url("doesnotexist");
    assert_eq!(ws_status(&unknown, None).await, 401);

    let header = authorization_header(&server.http);
    assert_eq!(ws_status(&unknown, Some(&header)).await, 404);

    let created = server
        .http
        .post(server.url("/rooms"))
        .basic_auth(AUTH_USER, Some(AUTH_PASSWORD))
        .json(&serde_json::json!({ "name": "live" }))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 201);
    let room: serde_json::Value = created.json().await.unwrap();
    let id = room["id"].as_str().unwrap();
    let mut request = server.ws_url(id).into_client_request().unwrap();
    request
        .headers_mut()
        .insert("Authorization", header.parse().unwrap());
    let (_socket, response) = tokio_tungstenite::connect_async(request).await.unwrap();
    assert_eq!(response.status(), 101);
}

#[tokio::test]
async fn auth_failure_does_not_consume_create_rate_limit() {
    let server = start_with(|config| {
        enable_auth(config);
        config.create_room_per_minute = NonZeroU32::new(1).unwrap();
        config.create_room_burst = NonZeroU32::new(1).unwrap();
    })
    .await;
    let rejected = server
        .http
        .post(server.url("/rooms"))
        .json(&serde_json::json!({ "name": "nope" }))
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), 401);
    let created = server
        .http
        .post(server.url("/rooms"))
        .basic_auth(AUTH_USER, Some(AUTH_PASSWORD))
        .json(&serde_json::json!({ "name": "alpha" }))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 201);
}

fn authorization_header(http: &reqwest::Client) -> String {
    let req = http
        .get("http://127.0.0.1/")
        .basic_auth(AUTH_USER, Some(AUTH_PASSWORD))
        .build()
        .unwrap();
    req.headers()
        .get(reqwest::header::AUTHORIZATION)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned()
}

async fn ws_status(url: &str, authorization: Option<&str>) -> u16 {
    let mut request = url.into_client_request().unwrap();
    if let Some(value) = authorization {
        request
            .headers_mut()
            .insert("Authorization", value.parse().unwrap());
    }
    match tokio_tungstenite::connect_async(request).await {
        Ok(_) => panic!("expected the upgrade to be rejected, got a socket"),
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            if resp.status().as_u16() == 401 {
                let www = resp
                    .headers()
                    .get("www-authenticate")
                    .expect("WWW-Authenticate");
                assert_eq!(www.to_str().unwrap(), "Basic realm=\"chattercrab\"");
            }
            resp.status().as_u16()
        }
        Err(other) => panic!("expected an HTTP response, got {other:?}"),
    }
}
