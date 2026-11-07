//! HTTP Basic authentication. A missing password leaves the server open.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use chattercrab_protocol::{ApiError, ErrorCode};

use crate::config::BasicAuth;
use crate::state::AppState;

/// Axum middleware. When [`crate::config::Config::auth`] is `None`, every request
/// continues. Otherwise `GET /health` and `OPTIONS` stay open and every other route
/// needs `Authorization: Basic`.
pub(crate) async fn require_basic_auth(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    let allowed = match state.config.auth.as_ref() {
        None => true,
        Some(_) if is_public(&req) => true,
        Some(expected) => credentials_ok(expected, req.headers()),
    };
    if allowed {
        next.run(req).await
    } else {
        unauthorized()
    }
}

/// `GET /health` stays open for process health checks. `OPTIONS` is the CORS preflight.
fn is_public(req: &Request) -> bool {
    req.method() == Method::OPTIONS
        || (req.method() == Method::GET && req.uri().path() == "/health")
}

fn credentials_ok(expected: &BasicAuth, headers: &HeaderMap) -> bool {
    let Some(presented) = headers.get(header::AUTHORIZATION) else {
        return false;
    };
    let Ok(text) = presented.to_str() else {
        return false;
    };
    let Some((user, password)) = decode_basic(text) else {
        return false;
    };
    secrets_match(expected, &user, &password)
}

/// Splits `Basic <base64(user:password)>`. The password may contain colons.
fn decode_basic(value: &str) -> Option<(String, String)> {
    let value = value.trim();
    let (scheme, token) = value.split_once(char::is_whitespace)?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let token = token.trim();
    if token.is_empty() || token.split_whitespace().nth(1).is_some() {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(token)
        .ok()?;
    let text = std::str::from_utf8(&bytes).ok()?;
    let (user, password) = text.split_once(':')?;
    Some((user.to_owned(), password.to_owned()))
}

/// Compares the user and the password without returning on the first mismatch.
fn secrets_match(expected: &BasicAuth, user: &str, password: &str) -> bool {
    let user_diff = constant_time_diff(expected.user.as_bytes(), user.as_bytes());
    let password_diff = constant_time_diff(expected.password.as_bytes(), password.as_bytes());
    (user_diff | password_diff) == 0
}

/// Zero when `a` and `b` are identical. Walks the longer slice even when the lengths differ.
fn constant_time_diff(a: &[u8], b: &[u8]) -> usize {
    let n = a.len().max(b.len());
    let mut diff = a.len() ^ b.len();
    for i in 0..n {
        let left = a.get(i).copied().unwrap_or(0);
        let right = b.get(i).copied().unwrap_or(0);
        diff |= usize::from(left ^ right);
    }
    diff
}

fn unauthorized() -> Response {
    let body = Json(ApiError {
        code: ErrorCode::Unauthorized,
        message: "unauthorized".to_owned(),
    });
    let mut response = (StatusCode::UNAUTHORIZED, body).into_response();
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"chattercrab\""),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expected() -> BasicAuth {
        BasicAuth {
            user: "chattercrab".into(),
            password: "s3cret:with:colon".into(),
        }
    }

    fn headers(value: &str) -> HeaderMap {
        let mut map = HeaderMap::new();
        map.insert(header::AUTHORIZATION, value.parse().unwrap());
        map
    }

    fn basic(user: &str, password: &str) -> String {
        let token = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
        format!("Basic {token}")
    }

    #[test]
    fn accepts_exact_credentials() {
        let header = basic("chattercrab", "s3cret:with:colon");
        assert!(credentials_ok(&expected(), &headers(&header)));
        let token = header.strip_prefix("Basic ").unwrap();
        assert!(credentials_ok(
            &expected(),
            &headers(&format!("basic {token}"))
        ));
    }

    #[test]
    fn rejects_missing_malformed_and_wrong_secrets() {
        assert!(!credentials_ok(&expected(), &HeaderMap::new()));
        assert!(!credentials_ok(&expected(), &headers("Bearer abc")));
        assert!(!credentials_ok(&expected(), &headers("Basic !!!")));
        assert!(!credentials_ok(&expected(), &headers("Basic")));
        let bad_utf8 = base64::engine::general_purpose::STANDARD.encode([0xff, 0xfe]);
        assert!(!credentials_ok(
            &expected(),
            &headers(&format!("Basic {bad_utf8}"))
        ));
        assert!(!credentials_ok(
            &expected(),
            &headers(&basic("chattercrac", "s3cret:with:colon"))
        ));
        assert!(!credentials_ok(
            &expected(),
            &headers(&basic("chattercrab", "s3cret:with:colo"))
        ));
        assert!(!credentials_ok(
            &expected(),
            &headers(&basic("chattercrab", "s3cret:with:colonX"))
        ));
        assert!(!credentials_ok(
            &expected(),
            &headers(&basic("other", "nope"))
        ));
    }

    #[test]
    fn length_mismatch_is_not_equal() {
        assert_eq!(constant_time_diff(b"", b""), 0);
        assert_ne!(constant_time_diff(b"secret", b"secretX"), 0);
        assert_ne!(constant_time_diff(b"secret", b"secre"), 0);
        // A trailing zero must not compare equal to a shorter slice.
        assert_ne!(constant_time_diff(&[1, 0], &[1]), 0);
        assert_eq!(constant_time_diff(b"same", b"same"), 0);
    }
}
