//! Persistent client configuration (`chattercrab/config.toml` in the platform config
//! directory). Chat history is never stored.

use std::path::{Path, PathBuf};

use anyhow::Context;
use chattercrab_media::{ScreenContent, ScreenScale, clamp_input_gain};
use serde::{Deserialize, Serialize};

/// Default screen-share frame rate (paired with 1440p).
pub const DEFAULT_SCREEN_FPS: u32 = 60;
/// Allowed screen-share frame rates. 120 is only used with high quality.
pub const SCREEN_FPS_RANGE: std::ops::RangeInclusive<u32> = 5..=120;
/// Highest frame rate when high quality is off.
pub const STANDARD_SCREEN_FPS_MAX: u32 = 60;

/// Light or dark UI theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeChoice {
    /// Light theme.
    Light,
    /// Dark theme.
    #[default]
    Dark,
}

/// User settings persisted between runs.
///
/// `Debug` redacts [`Self::password`]. The password is stored in the TOML file in
/// plaintext because the operator keeps it in config.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Signaling server base URL. New installs use `https://chattercrab.qcodefactory.com`.
    pub server_url: String,
    /// HTTP Basic user sent when [`Self::password`] is non-empty.
    pub auth_user: String,
    /// Shared signaling password. Empty sends no `Authorization` header.
    pub password: String,
    /// Display name used when joining rooms.
    pub display_name: String,
    /// Preferred microphone; `None` uses the system default.
    pub input_device: Option<String>,
    /// Preferred speakers; `None` uses the system default.
    pub output_device: Option<String>,
    /// UI theme.
    pub theme: ThemeChoice,
    /// Screen-share frame rate (5, 10, 15, 20, 30, 60, or 120 with high quality).
    pub screen_fps: u32,
    /// Screen-share size cap: 4K, 2K, 1440p, or 1080p.
    pub screen_scale: ScreenScale,
    /// Microphone noise suppression (RNNoise) enabled.
    pub noise_suppression: bool,
    /// Microphone gain. `1.0` is 100%, `5.0` is 500%.
    #[serde(default = "default_input_gain")]
    pub input_gain: f32,
    /// Last choice of the "share audio from other apps" checkbox.
    pub share_audio: bool,
    /// Last choice of the high-quality screen share toggle.
    pub high_quality: bool,
    /// Desktop (crisp fonts) or game (less blur). Independent of size and frame rate.
    pub screen_content: ScreenContent,
}

fn default_input_gain() -> f32 {
    1.0
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server_url: "https://chattercrab.qcodefactory.com".to_owned(),
            auth_user: String::new(),
            password: String::new(),
            display_name: String::new(),
            input_device: None,
            output_device: None,
            theme: ThemeChoice::default(),
            screen_fps: DEFAULT_SCREEN_FPS,
            screen_scale: ScreenScale::P1440,
            noise_suppression: false,
            input_gain: default_input_gain(),
            share_audio: false,
            high_quality: false,
            screen_content: ScreenContent::Desktop,
        }
    }
}

impl Config {
    /// Default location: `<config dir>/chattercrab/config.toml`.
    pub fn default_path() -> Option<PathBuf> {
        directories::BaseDirs::new()
            .map(|dirs| dirs.config_dir().join("chattercrab").join("config.toml"))
    }

    /// Loads the configuration from `path`. A missing file yields the defaults.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let mut config: Self =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        config.screen_fps = config
            .screen_fps
            .clamp(*SCREEN_FPS_RANGE.start(), *SCREEN_FPS_RANGE.end());
        config.input_gain = clamp_input_gain(config.input_gain);
        // The previous default was full capture at 15 fps. That pair now means 1440p60.
        if config.screen_fps == 15 && legacy_screen_scale(&text) {
            config.screen_fps = DEFAULT_SCREEN_FPS;
        }
        Ok(config)
    }

    /// Writes the configuration to `path`, creating parent directories.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(self).context("serializing config")?;
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
    }
}

/// True when this file still uses the old `full` or `half` size names.
fn legacy_screen_scale(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim();
        line.starts_with("screen_scale") && (line.contains("\"full\"") || line.contains("\"half\""))
    })
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("server_url", &self.server_url)
            .field("auth_user", &self.auth_user)
            .field("password", &"<redacted>")
            .field("display_name", &self.display_name)
            .field("input_device", &self.input_device)
            .field("output_device", &self.output_device)
            .field("theme", &self.theme)
            .field("screen_fps", &self.screen_fps)
            .field("screen_scale", &self.screen_scale)
            .field("noise_suppression", &self.noise_suppression)
            .field("input_gain", &self.input_gain)
            .field("share_audio", &self.share_audio)
            .field("high_quality", &self.high_quality)
            .field("screen_content", &self.screen_content)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chattercrab_media::ScreenScale;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "chattercrab-test-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("chattercrab").join("config.toml");
        let config = Config {
            server_url: "https://chat.example.com:8443".into(),
            auth_user: "ada".into(),
            password: "s3cret:with:colon".into(),
            display_name: "ada".into(),
            input_device: Some("USB Mic".into()),
            output_device: None,
            theme: ThemeChoice::Light,
            screen_fps: 60,
            screen_scale: ScreenScale::P1440,
            noise_suppression: true,
            input_gain: 2.5,
            share_audio: true,
            high_quality: true,
            screen_content: ScreenContent::Game,
        };
        config.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("s3cret:with:colon"),
            "password is stored in the config file"
        );
        assert_eq!(Config::load(&path).unwrap(), config);
        let rendered = format!("{config:?}");
        assert!(
            !rendered.contains("s3cret:with:colon"),
            "password leaked in Debug: {rendered}"
        );
        assert!(rendered.contains("<redacted>"), "{rendered}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_file_gives_defaults() {
        let dir = temp_dir("missing");
        assert_eq!(
            Config::load(&dir.join("nope.toml")).unwrap(),
            Config::default()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn partial_file_and_fps_clamp() {
        let dir = temp_dir("partial");
        let path = dir.join("config.toml");
        std::fs::write(&path, "display_name = \"grace\"\nscreen_fps = 240\n").unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.display_name, "grace");
        assert_eq!(config.screen_fps, 120);
        assert!(!config.high_quality);
        assert_eq!(config.screen_content, ScreenContent::Desktop);
        assert_eq!(config.screen_scale, ScreenScale::P1440);
        assert_eq!(config.server_url, Config::default().server_url);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn legacy_screen_scale_loads_as_1080p() {
        let dir = temp_dir("legacy-scale");
        let path = dir.join("config.toml");
        for (text, expected) in [
            ("screen_scale = \"full\"\n", ScreenScale::P1440),
            ("screen_scale = \"half\"\n", ScreenScale::P1440),
            ("screen_scale = \"1080p\"\n", ScreenScale::P1080),
            ("screen_scale = \"1440p\"\n", ScreenScale::P1440),
            ("screen_scale = \"2k\"\n", ScreenScale::P2K),
            ("screen_scale = \"4k\"\n", ScreenScale::P4K),
            ("screen_scale = \"2160p\"\n", ScreenScale::P1440),
        ] {
            std::fs::write(&path, text).unwrap();
            assert_eq!(
                Config::load(&path).unwrap().screen_scale,
                expected,
                "{text}"
            );
        }
        std::fs::write(&path, "screen_scale = \"full\"\nscreen_fps = 15\n").unwrap();
        let migrated = Config::load(&path).unwrap();
        assert_eq!(migrated.screen_scale, ScreenScale::P1440);
        assert_eq!(migrated.screen_fps, 60);
        std::fs::write(&path, "screen_scale = \"half\"\nscreen_fps = 20\n").unwrap();
        let custom = Config::load(&path).unwrap();
        assert_eq!(custom.screen_scale, ScreenScale::P1440);
        assert_eq!(custom.screen_fps, 20);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn noise_suppression_defaults_off_for_old_files() {
        let dir = temp_dir("old-file");
        let path = dir.join("config.toml");
        // A config written before the option existed.
        std::fs::write(
            &path,
            "server_url = \"https://chat.example.com:8443\"
display_name = \"ada\"
theme = \"dark\"
screen_fps = 15
",
        )
        .unwrap();
        let config = Config::load(&path).unwrap();
        assert!(!config.noise_suppression);
        assert!((config.input_gain - 1.0).abs() < f32::EPSILON);
        assert!(!config.share_audio);
        assert!(!config.high_quality);
        assert_eq!(config.screen_content, ScreenContent::Desktop);
        assert_eq!(config.display_name, "ada");
        assert_eq!(config.auth_user, "");
        assert!(config.password.is_empty());

        let enabled = Config {
            noise_suppression: true,
            ..config
        };
        enabled.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("noise_suppression = true"));
        assert!(Config::load(&path).unwrap().noise_suppression);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn old_file_without_auth_keys_uses_defaults() {
        let dir = temp_dir("old-auth");
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "server_url = \"https://chat.example.com:8443\"
display_name = \"ada\"
theme = \"dark\"
screen_fps = 15
noise_suppression = false
",
        )
        .unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.auth_user, "");
        assert!(config.password.is_empty());
        assert_eq!(config.display_name, "ada");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn default_path_ends_with_expected_components() {
        if let Some(path) = Config::default_path() {
            assert!(path.ends_with("chattercrab/config.toml"));
        }
    }
}
