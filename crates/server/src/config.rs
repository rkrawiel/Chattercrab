//! Server configuration loaded from `CHATTERCRAB_*` environment variables.

use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::time::Duration;

/// STUN URL used when `CHATTERCRAB_STUN_URL` is unset or empty.
pub const DEFAULT_STUN_URL: &str = "stun:stun.l.google.com:19302";

/// HTTP Basic credentials required when authentication is enabled.
///
/// `Debug` redacts [`Self::password`].
#[derive(Clone, PartialEq, Eq)]
pub struct BasicAuth {
    /// User (`CHATTERCRAB_AUTH_USER`). Never contains `:`.
    pub user: String,
    /// Shared password (`CHATTERCRAB_PASSWORD`). Never logged.
    pub password: String,
}

impl std::fmt::Debug for BasicAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BasicAuth")
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// Paths of the PEM certificate chain and private key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsPaths {
    /// Certificate chain (`fullchain.pem`).
    pub cert: PathBuf,
    /// Private key (`privkey.pem`).
    pub key: PathBuf,
}

/// Complete server configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Listen address (`CHATTERCRAB_BIND`).
    pub bind: SocketAddr,
    /// TLS files; `None` serves plain HTTP (local development).
    pub tls: Option<TlsPaths>,
    /// Single `stun:` URL handed to clients (`CHATTERCRAB_STUN_URL`).
    pub stun_url: String,
    /// Shared Basic credential. `None` leaves every route open.
    pub auth: Option<BasicAuth>,
    /// Maximum members per room (`CHATTERCRAB_MAX_ROOM_SIZE`).
    pub max_room_size: usize,
    /// Maximum number of rooms (`CHATTERCRAB_MAX_ROOMS`).
    pub max_rooms: usize,
    /// Empty rooms idle longer than this are evicted (`CHATTERCRAB_ROOM_IDLE_TTL_SECS`).
    pub room_idle_ttl: Duration,
    /// Time allowed between connect and `join`.
    pub join_timeout: Duration,
    /// Interval between WebSocket Ping frames.
    pub ping_interval: Duration,
    /// Interval of the idle-room sweeper.
    pub sweep_interval: Duration,
    /// Maximum time between a shutdown request and process exit.
    pub shutdown_grace: Duration,
    /// Sustained `POST /rooms` rate per IP, per minute.
    pub create_room_per_minute: NonZeroU32,
    /// `POST /rooms` burst per IP.
    pub create_room_burst: NonZeroU32,
    /// Sustained `GET /ice-servers` rate per IP, per minute.
    pub ice_per_minute: NonZeroU32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([0, 0, 0, 0], 8443)),
            tls: None,
            stun_url: DEFAULT_STUN_URL.to_owned(),
            auth: None,
            max_room_size: 5,
            max_rooms: 100,
            room_idle_ttl: Duration::from_secs(3600),
            join_timeout: Duration::from_secs(5),
            ping_interval: Duration::from_secs(20),
            sweep_interval: Duration::from_secs(60),
            shutdown_grace: Duration::from_secs(10),
            create_room_per_minute: NonZeroU32::MIN.saturating_add(9),
            create_room_burst: NonZeroU32::MIN.saturating_add(4),
            ice_per_minute: NonZeroU32::MIN.saturating_add(29),
        }
    }
}

/// A configuration value could not be used.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A variable holds an unparsable or out-of-range value.
    #[error("{var}={value:?} is invalid: {reason}")]
    Invalid {
        /// Variable name.
        var: &'static str,
        /// Offending value.
        value: String,
        /// Explanation.
        reason: &'static str,
    },
    /// Only one of the TLS certificate and key paths was set.
    #[error("CHATTERCRAB_TLS_CERT and CHATTERCRAB_TLS_KEY must be set together")]
    TlsIncomplete,
}

impl Config {
    /// Loads the configuration from the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Loads the configuration through `lookup`, which maps a variable name to its
    /// value. Empty values count as unset. `CHATTERCRAB_AUTH_USER` is read only when
    /// `CHATTERCRAB_PASSWORD` is non-empty; an empty user is then an error, not a default.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let get = |name: &str| {
            lookup(name)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        let mut config = Self::default();

        if let Some(v) = get("CHATTERCRAB_BIND") {
            config.bind = v.parse().map_err(|_| ConfigError::Invalid {
                var: "CHATTERCRAB_BIND",
                value: v.clone(),
                reason: "expected host:port, e.g. 0.0.0.0:8443",
            })?;
        }

        config.tls = match (get("CHATTERCRAB_TLS_CERT"), get("CHATTERCRAB_TLS_KEY")) {
            (Some(cert), Some(key)) => Some(TlsPaths {
                cert: cert.into(),
                key: key.into(),
            }),
            (None, None) => None,
            _ => return Err(ConfigError::TlsIncomplete),
        };

        if let Some(v) = get("CHATTERCRAB_STUN_URL") {
            config.stun_url = parse_stun_url(&v)?;
        }

        config.auth = parse_auth(&lookup)?;

        if let Some(v) = get("CHATTERCRAB_MAX_ROOM_SIZE") {
            config.max_room_size = parse_positive("CHATTERCRAB_MAX_ROOM_SIZE", &v)?;
        }
        if let Some(v) = get("CHATTERCRAB_MAX_ROOMS") {
            config.max_rooms = parse_positive("CHATTERCRAB_MAX_ROOMS", &v)?;
        }
        if let Some(v) = get("CHATTERCRAB_ROOM_IDLE_TTL_SECS") {
            let secs = parse_positive("CHATTERCRAB_ROOM_IDLE_TTL_SECS", &v)?;
            config.room_idle_ttl = Duration::from_secs(secs as u64);
        }
        Ok(config)
    }
}

/// `None` when the password is unset or empty after trimming.
fn parse_auth(lookup: &impl Fn(&str) -> Option<String>) -> Result<Option<BasicAuth>, ConfigError> {
    let Some(raw_password) = lookup("CHATTERCRAB_PASSWORD") else {
        return Ok(None);
    };
    let password = raw_password.trim().to_owned();
    if password.is_empty() {
        return Ok(None);
    }
    let user = match lookup("CHATTERCRAB_AUTH_USER") {
        None => "chattercrab".to_owned(),
        Some(raw) => {
            let user = raw.trim().to_owned();
            if user.is_empty() {
                return Err(ConfigError::Invalid {
                    var: "CHATTERCRAB_AUTH_USER",
                    value: raw,
                    reason: "must not be empty",
                });
            }
            if user.contains(':') {
                return Err(ConfigError::Invalid {
                    var: "CHATTERCRAB_AUTH_USER",
                    value: user,
                    reason: "must not contain ':'",
                });
            }
            user
        }
    };
    Ok(Some(BasicAuth { user, password }))
}

fn parse_positive(var: &'static str, value: &str) -> Result<usize, ConfigError> {
    match value.parse::<usize>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(ConfigError::Invalid {
            var,
            value: value.to_owned(),
            reason: "expected a positive integer",
        }),
    }
}

/// Accepts exactly one `stun:host:port` or `stun:[ipv6]:port` URL.
fn parse_stun_url(value: &str) -> Result<String, ConfigError> {
    const VAR: &str = "CHATTERCRAB_STUN_URL";
    let invalid = |reason: &'static str| ConfigError::Invalid {
        var: VAR,
        value: value.to_owned(),
        reason,
    };

    if value.chars().any(char::is_whitespace) {
        return Err(invalid("must not contain whitespace"));
    }
    if value.contains(',') {
        return Err(invalid("expected exactly one URL"));
    }
    if value.contains('?') || value.contains('/') {
        return Err(invalid("must not contain a query or path"));
    }
    let Some(rest) = value.strip_prefix("stun:") else {
        return Err(invalid("expected a stun: URL"));
    };
    let Some((host, port)) = split_host_port(rest) else {
        return Err(invalid("expected host:port"));
    };
    if host.is_empty() {
        return Err(invalid("host is empty"));
    }
    match port.parse::<u16>() {
        Ok(port) if port >= 1 => Ok(value.to_owned()),
        _ => Err(invalid("port must be an integer from 1 to 65535")),
    }
}

/// Splits `host:port` or `[ipv6]:port`. An unbracketed address with extra colons is rejected.
fn split_host_port(rest: &str) -> Option<(&str, &str)> {
    if let Some(inner) = rest.strip_prefix('[') {
        let (host, after) = inner.split_once(']')?;
        let port = after.strip_prefix(':')?;
        if port.contains(':') || host.contains('[') || host.contains(']') {
            return None;
        }
        Some((host, port))
    } else {
        let (host, port) = rest.rsplit_once(':')?;
        if host.contains(':') || host.contains('[') || host.contains(']') {
            return None;
        }
        Some((host, port))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn load(vars: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::from_lookup(|k| map.get(k).cloned())
    }

    #[test]
    fn defaults_when_empty() {
        let c = load(&[]).unwrap();
        assert_eq!(c.bind, "0.0.0.0:8443".parse().unwrap());
        assert!(c.tls.is_none());
        assert_eq!(c.stun_url, DEFAULT_STUN_URL);
        assert!(c.auth.is_none());
        assert_eq!(c.max_room_size, 5);
        assert_eq!(c.max_rooms, 100);
        assert_eq!(c.room_idle_ttl, Duration::from_secs(3600));
        assert_eq!(c.create_room_per_minute.get(), 10);
        assert_eq!(c.create_room_burst.get(), 5);
        assert_eq!(c.ice_per_minute.get(), 30);
        assert_eq!(
            load(&[("CHATTERCRAB_STUN_URL", "")]).unwrap().stun_url,
            DEFAULT_STUN_URL
        );
        assert_eq!(
            load(&[("CHATTERCRAB_STUN_URL", "   ")]).unwrap().stun_url,
            DEFAULT_STUN_URL
        );
    }

    #[test]
    fn parses_full_template() {
        let c = load(&[
            ("CHATTERCRAB_BIND", "127.0.0.1:9000"),
            ("CHATTERCRAB_TLS_CERT", "/etc/chattercrab/tls/fullchain.pem"),
            ("CHATTERCRAB_TLS_KEY", "/etc/chattercrab/tls/privkey.pem"),
            ("CHATTERCRAB_STUN_URL", "stun:stun.example.com:3478"),
            ("CHATTERCRAB_MAX_ROOM_SIZE", "4"),
            ("CHATTERCRAB_MAX_ROOMS", "7"),
            ("CHATTERCRAB_ROOM_IDLE_TTL_SECS", "60"),
        ])
        .unwrap();
        assert_eq!(c.bind.port(), 9000);
        assert!(c.tls.is_some());
        assert_eq!(c.stun_url, "stun:stun.example.com:3478");
        assert_eq!(c.max_room_size, 4);
        assert_eq!(c.max_rooms, 7);
        assert_eq!(c.room_idle_ttl, Duration::from_secs(60));
    }

    #[test]
    fn accepts_custom_stun_url() {
        let c = load(&[("CHATTERCRAB_STUN_URL", "stun:stun.example.com:3478")]).unwrap();
        assert_eq!(c.stun_url, "stun:stun.example.com:3478");
        let v6 = load(&[("CHATTERCRAB_STUN_URL", "stun:[2001:db8::1]:19302")]).unwrap();
        assert_eq!(v6.stun_url, "stun:[2001:db8::1]:19302");
    }

    #[test]
    fn rejects_bad_stun_urls() {
        for value in [
            "turn:chat.example.com:3478",
            "turns:chat.example.com:5349",
            "stuns:stun.example.com:5349",
            "http://stun.example.com:19302",
            "https://stun.example.com:19302",
            "stun:stun.l.google.com",
            "stun:a.example:19302,stun:b.example:19302",
            "stun::19302",
            "stun:[]:19302",
            "stun:stun.example.com:19302?transport=udp",
            "stun:stun.example.com:19302/path",
        ] {
            match load(&[("CHATTERCRAB_STUN_URL", value)]) {
                Err(ConfigError::Invalid {
                    var, value: got, ..
                }) => {
                    assert_eq!(var, "CHATTERCRAB_STUN_URL");
                    assert_eq!(got, value);
                }
                other => panic!("expected invalid STUN URL for {value:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn password_unset_or_blank_disables_auth() {
        assert!(load(&[]).unwrap().auth.is_none());
        assert!(
            load(&[("CHATTERCRAB_PASSWORD", "")])
                .unwrap()
                .auth
                .is_none()
        );
        assert!(
            load(&[("CHATTERCRAB_PASSWORD", "   ")])
                .unwrap()
                .auth
                .is_none()
        );
        // The user is ignored when authentication is off, even if it would be invalid.
        assert!(
            load(&[
                ("CHATTERCRAB_PASSWORD", "  "),
                ("CHATTERCRAB_AUTH_USER", "a:b"),
            ])
            .unwrap()
            .auth
            .is_none()
        );
    }

    #[test]
    fn password_defaults_user_and_trims() {
        let auth = load(&[("CHATTERCRAB_PASSWORD", "  secret  ")])
            .unwrap()
            .auth
            .unwrap();
        assert_eq!(auth.user, "chattercrab");
        assert_eq!(auth.password, "secret");
    }

    #[test]
    fn custom_auth_user() {
        let auth = load(&[
            ("CHATTERCRAB_PASSWORD", "secret"),
            ("CHATTERCRAB_AUTH_USER", "  ada  "),
        ])
        .unwrap()
        .auth
        .unwrap();
        assert_eq!(auth.user, "ada");
        assert_eq!(auth.password, "secret");
    }

    #[test]
    fn rejects_user_with_colon() {
        match load(&[
            ("CHATTERCRAB_PASSWORD", "secret"),
            ("CHATTERCRAB_AUTH_USER", "a:b"),
        ]) {
            Err(ConfigError::Invalid { var, value, .. }) => {
                assert_eq!(var, "CHATTERCRAB_AUTH_USER");
                assert_eq!(value, "a:b");
            }
            other => panic!("expected invalid user, got {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_user_when_password_set() {
        for user in ["", "   "] {
            match load(&[
                ("CHATTERCRAB_PASSWORD", "secret"),
                ("CHATTERCRAB_AUTH_USER", user),
            ]) {
                Err(ConfigError::Invalid { var, .. }) => {
                    assert_eq!(var, "CHATTERCRAB_AUTH_USER");
                }
                other => panic!("expected empty user error for {user:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn debug_redacts_password() {
        let password = "super-secret-value";
        let c = load(&[
            ("CHATTERCRAB_PASSWORD", password),
            ("CHATTERCRAB_AUTH_USER", "ada"),
        ])
        .unwrap();
        let rendered = format!("{c:?}");
        assert!(
            !rendered.contains(password),
            "password leaked in Debug: {rendered}"
        );
        assert!(rendered.contains("<redacted>"), "{rendered}");
        assert!(rendered.contains("ada"), "{rendered}");
        let auth = c.auth.unwrap();
        let auth_rendered = format!("{auth:?}");
        assert!(!auth_rendered.contains(password), "{auth_rendered}");
        assert!(auth_rendered.contains("<redacted>"));
    }

    #[test]
    fn rejects_bad_values() {
        assert!(load(&[("CHATTERCRAB_BIND", "nope")]).is_err());
        assert!(load(&[("CHATTERCRAB_MAX_ROOMS", "0")]).is_err());
        assert!(load(&[("CHATTERCRAB_MAX_ROOM_SIZE", "-1")]).is_err());
        assert!(matches!(
            load(&[("CHATTERCRAB_TLS_CERT", "/x")]),
            Err(ConfigError::TlsIncomplete)
        ));
    }
}
