//! REST client for the signaling server.

use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use base64::Engine;
use chattercrab_protocol::{
    ApiError, CreateRoomRequest, HealthResponse, IceServers, RoomId, RoomName, RoomSummary,
};
use reqwest::Url;

/// Timeout for every REST request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Thin wrapper around `reqwest` bound to one server.
///
/// `Debug` redacts the password.
#[derive(Clone)]
pub struct Rest {
    client: reqwest::Client,
    /// Base URL without a trailing slash, e.g. `https://chat.example.com:8443`.
    base: String,
    secure: bool,
    auth_user: String,
    password: String,
}

impl std::fmt::Debug for Rest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rest")
            .field("base", &self.base)
            .field("secure", &self.secure)
            .field("auth_user", &self.auth_user)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// Validates a user-entered server URL and returns `(base, secure)`.
pub fn normalize_server_url(input: &str) -> anyhow::Result<(String, bool)> {
    let trimmed = input.trim();
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_owned()
    } else {
        format!("https://{trimmed}")
    };
    let url = Url::parse(&with_scheme).with_context(|| format!("invalid server URL {input:?}"))?;
    let secure = match url.scheme() {
        "https" => true,
        "http" => false,
        other => bail!("unsupported URL scheme {other:?} (use https:// or http://)"),
    };
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("server URL has no host"))?;
    // `host_str` already brackets IPv6 literals.
    let base = match url.port() {
        Some(port) => format!("{}://{host}:{port}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    };
    Ok((base, secure))
}

/// `Authorization` header value for `user:password`, using standard base64.
pub(crate) fn basic_authorization_value(user: &str, password: &str) -> String {
    let token = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
    format!("Basic {token}")
}

impl Rest {
    /// Creates a client for `server_url` (scheme defaults to `https://`).
    ///
    /// An empty `password` sends no `Authorization` header. `auth_user` must not
    /// contain `:` because Basic splits the decoded token on the first colon.
    pub fn new(server_url: &str, auth_user: &str, password: &str) -> anyhow::Result<Self> {
        if auth_user.contains(':') {
            bail!("auth user must not contain ':'");
        }
        let (base, secure) = normalize_server_url(server_url)?;
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent(concat!("chattercrab/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            client,
            base,
            secure,
            auth_user: auth_user.to_owned(),
            password: password.to_owned(),
        })
    }

    /// Basic auth user stored for later REST and WebSocket calls.
    pub(crate) fn auth_user(&self) -> &str {
        &self.auth_user
    }

    /// Shared password. Empty means requests are unauthenticated.
    pub(crate) fn password(&self) -> &str {
        &self.password
    }

    fn authed(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.password.is_empty() {
            builder
        } else {
            builder.basic_auth(&self.auth_user, Some(&self.password))
        }
    }

    /// Base URL, e.g. `https://chat.example.com:8443`.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// WebSocket URL for a room.
    pub fn ws_url(&self, room: &RoomId) -> String {
        ws_url(&self.base, self.secure, room)
    }

    /// `GET /health`.
    pub async fn health(&self) -> anyhow::Result<HealthResponse> {
        let resp = self
            .authed(self.client.get(format!("{}/health", self.base)))
            .send()
            .await
            .with_context(|| format!("cannot reach {}", self.base))?;
        parse(resp).await
    }

    /// `GET /rooms`.
    pub async fn rooms(&self) -> anyhow::Result<Vec<RoomSummary>> {
        let resp = self
            .authed(self.client.get(format!("{}/rooms", self.base)))
            .send()
            .await
            .context("listing rooms")?;
        parse(resp).await
    }

    /// `POST /rooms`.
    pub async fn create_room(&self, name: RoomName) -> anyhow::Result<RoomSummary> {
        let resp = self
            .authed(
                self.client
                    .post(format!("{}/rooms", self.base))
                    .json(&CreateRoomRequest { name }),
            )
            .send()
            .await
            .context("creating room")?;
        parse(resp).await
    }

    /// `GET /ice-servers`.
    pub async fn ice_servers(&self) -> anyhow::Result<IceServers> {
        let resp = self
            .authed(self.client.get(format!("{}/ice-servers", self.base)))
            .send()
            .await
            .context("fetching ICE servers")?;
        parse(resp).await
    }
}

/// Builds `ws(s)://host:port/ws/{room}` from a normalized base URL.
fn ws_url(base: &str, secure: bool, room: &RoomId) -> String {
    let rest = base.split_once("://").map(|(_, rest)| rest).unwrap_or(base);
    let scheme = if secure { "wss" } else { "ws" };
    format!("{scheme}://{rest}/ws/{room}")
}

/// Decodes a JSON body, or turns an error response into a readable error.
async fn parse<T: serde::de::DeserializeOwned>(resp: reqwest::Response) -> anyhow::Result<T> {
    let status = resp.status();
    if status.is_success() {
        return resp.json::<T>().await.context("decoding server response");
    }
    let body = resp.text().await.unwrap_or_default();
    match serde_json::from_str::<ApiError>(&body) {
        Ok(api) => Err(anyhow!("{} ({})", api.message, status)),
        Err(_) => Err(anyhow!("server returned {status}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_urls() {
        assert_eq!(
            normalize_server_url("chat.example.com:8443").unwrap(),
            ("https://chat.example.com:8443".to_owned(), true)
        );
        assert_eq!(
            normalize_server_url(" http://127.0.0.1:8443/ ").unwrap(),
            ("http://127.0.0.1:8443".to_owned(), false)
        );
        assert_eq!(
            normalize_server_url("https://chat.example.com/some/path").unwrap(),
            ("https://chat.example.com".to_owned(), true)
        );
        assert_eq!(
            normalize_server_url("http://[::1]:8443").unwrap(),
            ("http://[::1]:8443".to_owned(), false)
        );
        assert!(normalize_server_url("ftp://x").is_err());
        assert!(normalize_server_url("").is_err());
    }

    fn rest(user: &str, password: &str) -> Rest {
        let _ = rustls::crypto::ring::default_provider().install_default();
        Rest::new("http://127.0.0.1:9", user, password).unwrap()
    }

    #[test]
    fn empty_password_sends_no_authorization_header() {
        let rest = rest("chattercrab", "");
        let req = rest
            .authed(rest.client.get("http://127.0.0.1:9/rooms"))
            .build()
            .unwrap();
        assert!(req.headers().get(reqwest::header::AUTHORIZATION).is_none());
        let rendered = format!("{rest:?}");
        assert!(!rendered.contains("s3cret"));
    }

    #[test]
    fn basic_header_matches_standard_base64() {
        let password = "s3cret:with:colon";
        let rest = rest("chattercrab", password);
        let req = rest
            .authed(rest.client.get("http://127.0.0.1:9/rooms"))
            .build()
            .unwrap();
        let header = req
            .headers()
            .get(reqwest::header::AUTHORIZATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(header, basic_authorization_value("chattercrab", password));
        let rendered = format!("{rest:?}");
        assert!(
            !rendered.contains(password),
            "password leaked in Debug: {rendered}"
        );
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }

    #[test]
    fn rejects_colon_in_auth_user() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let err = Rest::new("http://127.0.0.1:9", "a:b", "secret").unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("':'"), "{message}");
        assert!(!message.contains("secret"), "{message}");
    }

    #[test]
    fn builds_ws_urls() {
        let room = RoomId::new("abc123").unwrap();
        assert_eq!(
            ws_url("https://chat.example.com:8443", true, &room),
            "wss://chat.example.com:8443/ws/abc123"
        );
        assert_eq!(
            ws_url("http://127.0.0.1:8443", false, &room),
            "ws://127.0.0.1:8443/ws/abc123"
        );
    }
}
