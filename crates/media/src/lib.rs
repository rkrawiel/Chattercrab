//! Client-side media engine for Chattercrab.
//!
//! The GUI never touches WebRTC or audio devices directly. It starts a [`MediaEngine`],
//! sends [`MediaCommand`]s and reacts to [`MediaEvent`]s, all over bounded
//! `tokio::sync::mpsc` channels. Decoded remote screen-share frames are delivered
//! through a [`VideoFrameQueue`]. The queue keeps the frames it already has when it
//! is full; the screen receiver plays them on a clock up to one second behind the
//! sender so a jittery share stays smooth.
//!
//! The types in this file are the public API contract shared by the media engine and
//! the client (additive changes are recorded in `API_CHANGES.md`). The building blocks
//! are public modules too: [`audio`], [`jitter`], [`mixer`], [`screen`], [`peer`] and
//! [`denoise`].
//!
//! Threading: the engine is one Tokio task; capture callbacks, the Opus encoder, the
//! mixer (real-time priority), screen capture/encode and H.264 decode each run on their
//! own OS thread. See `crates/media/README.md`.

pub mod audio;
pub mod denoise;
mod engine;
pub mod jitter;
pub mod mixer;
pub mod peer;
pub mod screen;
mod video_queue;

pub use audio::{INPUT_GAIN_MAX, INPUT_GAIN_MIN, clamp_input_gain};
pub use mixer::PEER_VOLUME_MAX;
pub use video_queue::{RgbaFrame, VideoFrameQueue};

use chattercrab_protocol::{IceServers, PeerId};
use tokio::sync::mpsc;

/// Capacity of the command channel into the engine.
pub const COMMAND_CHANNEL_CAPACITY: usize = 64;
/// Capacity of the event channel out of the engine.
pub const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Errors returned by the media engine API.
#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    /// No audio device with the requested name exists.
    #[error("audio device not found: {0}")]
    DeviceNotFound(String),
    /// The audio backend failed.
    #[error("audio backend error: {0}")]
    Audio(String),
    /// Screen capture failed.
    #[error("screen capture error: {0}")]
    Screen(String),
    /// WebRTC failure.
    #[error("webrtc error: {0}")]
    WebRtc(String),
    /// Codec initialisation or coding failure.
    #[error("codec error: {0}")]
    Codec(String),
    /// The engine has shut down.
    #[error("media engine is not running")]
    Stopped,
}

/// Initial engine configuration.
#[derive(Debug, Clone)]
pub struct MediaConfig {
    /// Input device name; `None` selects the system default.
    pub input_device: Option<String>,
    /// Output device name; `None` selects the system default.
    pub output_device: Option<String>,
    /// Enable RNNoise-based noise suppression on the microphone input.
    pub noise_suppression: bool,
    /// Microphone gain. `1.0` is unity, `5.0` is 500%. Clamped when applied.
    pub input_gain: f32,
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            input_device: None,
            output_device: None,
            noise_suppression: false,
            input_gain: 1.0,
        }
    }
}

/// Diagnostic engine options, separate from [`MediaConfig`] so that adding them does not
/// break code that builds `MediaConfig` with a struct literal.
#[derive(Debug, Clone, Default)]
pub struct MediaDiagnostics {
    /// Diagnostics: replace the microphone with a sine tone of this frequency in Hz.
    /// Used by tests and the two-process harness. The tone goes through the same encoder
    /// thread (20 ms frames, mute, noise suppression, Opus) as microphone audio.
    pub test_tone: Option<f32>,
}

/// What kind of surface a [`ScreenSource`] captures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenSourceKind {
    /// A whole monitor.
    Monitor,
    /// A single window.
    Window,
}

/// Resolution cap applied before encoding. The picture is never scaled up.
///
/// Older configs used `full`, `half`, and `2160p`. Those load as [`Self::P1440`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenScale {
    /// Fit inside 1920×1080.
    #[serde(rename = "1080p")]
    P1080,
    /// Fit inside 2560×1440. Default.
    #[default]
    #[serde(rename = "1440p", alias = "2160p", alias = "full", alias = "half")]
    P1440,
    /// Fit inside 2560×1440, the 2K name for the same cap as [`Self::P1440`].
    #[serde(rename = "2k")]
    P2K,
    /// Fit inside 3840×2160.
    #[serde(rename = "4k")]
    P4K,
}

impl ScreenScale {
    /// Maximum encoded width and height. Smaller captures stay at their own size.
    pub fn max_dimensions(self) -> (u32, u32) {
        match self {
            Self::P1080 => (1920, 1080),
            Self::P1440 | Self::P2K => (2560, 1440),
            Self::P4K => (3840, 2160),
        }
    }
}

impl std::fmt::Display for ScreenScale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::P1080 => "1080p",
            Self::P1440 => "1440p",
            Self::P2K => "2K",
            Self::P4K => "4K",
        })
    }
}

/// What the share is showing. The user picks this before the share starts.
/// Resolution and frame rate stay independent of the choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenContent {
    /// Crisp text and UI: screen-content tools, two references in high quality.
    #[default]
    Desktop,
    /// Moving images: motion tools and, in high quality, more bits per frame.
    Game,
}

impl ScreenContent {
    /// Whether this choice uses the game operating point.
    pub fn is_game(self) -> bool {
        matches!(self, Self::Game)
    }
}

impl std::fmt::Display for ScreenContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Desktop => "Desktop",
            Self::Game => "Game",
        })
    }
}

/// A capturable monitor or window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenSource {
    /// Backend-specific identifier, stable for the lifetime of the source.
    pub id: u32,
    /// Monitor or window.
    pub kind: ScreenSourceKind,
    /// Human-readable label for the UI.
    pub name: String,
}

/// State of the connection to one remote peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerConnectionState {
    /// Negotiating or gathering candidates.
    Connecting,
    /// Media is flowing.
    Connected,
    /// Temporarily lost connectivity; ICE may recover.
    Disconnected,
    /// ICE failed.
    Failed,
    /// Closed locally or remotely.
    Closed,
}

/// Signaling message the engine wants delivered to a remote peer via the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutgoingSignal {
    /// SDP offer.
    Offer {
        /// Recipient.
        to: PeerId,
        /// Session description.
        sdp: String,
    },
    /// SDP answer.
    Answer {
        /// Recipient.
        to: PeerId,
        /// Session description.
        sdp: String,
    },
    /// Trickle ICE candidate; empty `candidate` means end-of-candidates.
    Ice {
        /// Recipient.
        to: PeerId,
        /// Candidate attribute.
        candidate: String,
        /// Media stream identification tag.
        sdp_mid: Option<String>,
        /// Index of the m-line.
        sdp_mline_index: Option<u16>,
    },
}

/// Commands accepted by the engine.
#[derive(Debug, Clone)]
pub enum MediaCommand {
    /// Set the local peer id (from `welcome`) and the ICE servers to use for every
    /// peer connection created afterwards.
    Configure {
        /// Our own peer id, used for perfect-negotiation politeness.
        me: PeerId,
        /// STUN servers from `GET /ice-servers`. `None` or empty urls means host candidates only.
        ice: Option<IceServers>,
    },
    /// Create a peer connection to `peer`. If `initiate` is true the engine sends the
    /// first offer (the newcomer does this for every peer listed in `welcome`);
    /// otherwise it waits for the remote offer (peers learned from `peer_joined`).
    AddPeer {
        /// Remote peer.
        peer: PeerId,
        /// Whether we send the initial offer.
        initiate: bool,
    },
    /// Close and forget the connection to `peer`.
    RemovePeer {
        /// Remote peer.
        peer: PeerId,
    },
    /// Close every peer connection (leave room or signaling reconnect). Audio devices
    /// stay open.
    RemoveAllPeers,
    /// Remote SDP offer relayed by the server.
    RemoteOffer {
        /// Sender.
        from: PeerId,
        /// Session description.
        sdp: String,
    },
    /// Remote SDP answer relayed by the server.
    RemoteAnswer {
        /// Sender.
        from: PeerId,
        /// Session description.
        sdp: String,
    },
    /// Remote ICE candidate relayed by the server.
    RemoteIce {
        /// Sender.
        from: PeerId,
        /// Candidate attribute; empty means end-of-candidates.
        candidate: String,
        /// Media stream identification tag.
        sdp_mid: Option<String>,
        /// Index of the m-line.
        sdp_mline_index: Option<u16>,
    },
    /// Mute or unmute the microphone (the stream keeps running and sends silence).
    SetMuted(bool),
    /// Deafen or undeafen playback.
    SetDeafened(bool),
    /// Enable or disable microphone noise suppression.
    SetNoiseSuppression(bool),
    /// Microphone gain, clamped to 0.0..=5.0 (`1.0` unity, `5.0` is 500%).
    SetInputGain(f32),
    /// Per-peer playback volume, clamped to 0.0..=5.0.
    SetPeerVolume {
        /// Remote peer.
        peer: PeerId,
        /// Linear gain.
        volume: f32,
    },
    /// Switch the input device (`None` = system default).
    SelectInput(Option<String>),
    /// Switch the output device (`None` = system default).
    SelectOutput(Option<String>),
    /// Start publishing a screen share. Only send after the server granted the lock.
    StartScreenShare {
        /// What to capture.
        source: ScreenSource,
        /// Target frame rate, clamped to 5..=60, or 5..=120 when `high_quality`.
        fps: u32,
        /// Fit inside 4K, 2K, 1440p, or 1080p. Smaller captures are not scaled up.
        scale: ScreenScale,
        /// Mix other applications' audio into the outgoing voice for this share.
        include_audio: bool,
        /// H.264 High at a fixed high bitrate, with IDR frames only when a
        /// viewer asks, at most once a second. Frame rate stays 5..=120.
        high_quality: bool,
        /// Desktop (crisp fonts) or game (less blur on moving images).
        /// Independent of `scale` and `fps`.
        content: ScreenContent,
    },
    /// Stop publishing the screen share.
    StopScreenShare,
    /// Stop everything and close the event channel.
    Shutdown,
}

/// Events emitted by the engine.
#[derive(Debug, Clone)]
pub enum MediaEvent {
    /// Deliver this message to the server.
    Signal(OutgoingSignal),
    /// Connection state of one peer changed.
    PeerState {
        /// Remote peer.
        peer: PeerId,
        /// New state.
        state: PeerConnectionState,
    },
    /// Audio levels (RMS, 0.0..=1.0), emitted about every 100 ms while in a room.
    Levels {
        /// Local microphone level (0.0 while muted).
        local: f32,
        /// Per remote peer.
        peers: Vec<(PeerId, f32)>,
    },
    /// A remote screen share started arriving; frames follow on the [`VideoFrameQueue`].
    RemoteScreenStarted {
        /// Publisher.
        peer: PeerId,
    },
    /// The remote screen share track ended.
    RemoteScreenEnded {
        /// Publisher.
        peer: PeerId,
    },
    /// Local screen publishing started successfully.
    LocalScreenStarted,
    /// Local screen publishing stopped (by command or by error).
    LocalScreenStopped,
    /// A non-fatal error to show in the UI.
    Error(String),
}

/// Handle to a running media engine.
#[derive(Debug)]
pub struct MediaEngine {
    /// Command sender.
    pub commands: mpsc::Sender<MediaCommand>,
    /// Event receiver.
    pub events: mpsc::Receiver<MediaEvent>,
    /// Decoded remote screen-share frames.
    pub video: VideoFrameQueue,
}

impl MediaEngine {
    /// Starts the engine on the current Tokio runtime. The engine opens the audio devices
    /// in the background; failures are reported as [`MediaEvent::Error`] and the engine
    /// keeps running (for example without a microphone).
    ///
    /// Must be called from within a Tokio runtime.
    pub fn start(config: MediaConfig) -> Result<Self, MediaError> {
        Self::start_with_diagnostics(config, MediaDiagnostics::default())
    }

    /// Like [`MediaEngine::start`], with diagnostic options such as a synthetic test tone
    /// instead of the microphone.
    pub fn start_with_diagnostics(
        config: MediaConfig,
        diagnostics: MediaDiagnostics,
    ) -> Result<Self, MediaError> {
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| MediaError::Audio("MediaEngine::start needs a Tokio runtime".into()))?;
        let (cmd_tx, cmd_rx) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel::<MediaEvent>(EVENT_CHANNEL_CAPACITY);
        let video = VideoFrameQueue::new();
        handle.spawn(engine::run(
            config,
            diagnostics,
            cmd_rx,
            event_tx,
            video.clone(),
        ));
        Ok(Self {
            commands: cmd_tx,
            events: event_rx,
            video,
        })
    }
}

/// Names of the available audio input devices.
pub fn list_input_devices() -> Result<Vec<String>, MediaError> {
    audio::input_device_names()
}

/// Names of the available audio output devices.
pub fn list_output_devices() -> Result<Vec<String>, MediaError> {
    audio::output_device_names()
}

/// Monitors and windows that can be shared.
pub fn list_screen_sources() -> Result<Vec<ScreenSource>, MediaError> {
    screen::list_sources()
}
