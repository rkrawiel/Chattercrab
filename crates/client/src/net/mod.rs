//! Network and media runtime: REST, signaling WebSocket, reconnect, and the bridge to
//! the media engine. Runs on its own Tokio runtime thread; the UI talks to it only
//! through [`UiCommand`] and [`AppEvent`] channels.

pub mod backend;
pub mod backoff;
pub mod rest;
pub mod session;
pub mod signaling;

use chattercrab_media::{PeerConnectionState, ScreenSource, VideoFrameQueue};
use chattercrab_protocol::{
    ChatMessage, DisplayName, PeerId, PeerInfo, RoomId, RoomName, RoomSummary,
};

/// Capacity of the UI to backend command channel.
pub const UI_COMMAND_CAPACITY: usize = 64;
/// Capacity of the backend to UI event channel.
pub const APP_EVENT_CAPACITY: usize = 256;

/// Commands from the UI to the backend.
///
/// `Debug` redacts [`UiCommand::Connect`]'s password.
#[derive(Clone)]
pub enum UiCommand {
    /// Enumerate audio devices.
    ListDevices,
    /// Connect to a signaling server and open the audio devices.
    Connect {
        /// Server base URL (`https://host:port` or `http://host:port`).
        server_url: String,
        /// HTTP Basic user. Sent only when `password` is non-empty.
        auth_user: String,
        /// Shared password. Empty sends no `Authorization` header.
        password: String,
        /// Display name for joins.
        display_name: DisplayName,
        /// Microphone; `None` = system default.
        input_device: Option<String>,
        /// Speakers; `None` = system default.
        output_device: Option<String>,
        /// Microphone noise suppression.
        noise_suppression: bool,
        /// Microphone gain (`1.0` unity, `5.0` is 500%).
        input_gain: f32,
    },
    /// Forget the server and return to the connect screen.
    Disconnect,
    /// Fetch the room list.
    RefreshRooms,
    /// Create a room and join it.
    CreateRoom(RoomName),
    /// Join an existing room.
    JoinRoom {
        /// Room id.
        id: RoomId,
        /// Room name, for display.
        name: RoomName,
    },
    /// Leave the current room.
    LeaveRoom,
    /// Send a chat message to the room.
    SendChat(String),
    /// Mute or unmute the microphone.
    SetMuted(bool),
    /// Deafen or undeafen playback.
    SetDeafened(bool),
    /// Enable or disable microphone noise suppression.
    SetNoiseSuppression(bool),
    /// Microphone gain (`1.0` unity, `5.0` is 500%).
    SetInputGain(f32),
    /// Playback volume for one peer (0.0 to 5.0).
    SetPeerVolume {
        /// Remote peer.
        peer: PeerId,
        /// Linear gain.
        volume: f32,
    },
    /// Enumerate monitors and windows.
    ListScreenSources,
    /// Ask the server for the screen lock and publish `source` once granted.
    StartScreenShare {
        /// What to capture.
        source: ScreenSource,
        /// Frame rate.
        fps: u32,
        /// Original size or half size.
        scale: chattercrab_media::ScreenScale,
        /// Mix other applications' audio into the outgoing voice.
        include_audio: bool,
        /// H.264 High at a fixed high bitrate. Frame rate is chosen separately.
        high_quality: bool,
        /// Desktop or game. Independent of resolution and frame rate.
        content: chattercrab_media::ScreenContent,
    },
    /// Stop publishing the screen.
    StopScreenShare,
    /// Stop the backend.
    Shutdown,
}

impl std::fmt::Debug for UiCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connect {
                server_url,
                auth_user,
                password: _,
                display_name,
                input_device,
                output_device,
                noise_suppression,
                input_gain,
            } => f
                .debug_struct("UiCommand::Connect")
                .field("server_url", server_url)
                .field("auth_user", auth_user)
                .field("password", &"<redacted>")
                .field("display_name", display_name)
                .field("input_device", input_device)
                .field("output_device", output_device)
                .field("noise_suppression", noise_suppression)
                .field("input_gain", input_gain)
                .finish(),
            Self::ListDevices => f.write_str("UiCommand::ListDevices"),
            Self::Disconnect => f.write_str("UiCommand::Disconnect"),
            Self::RefreshRooms => f.write_str("UiCommand::RefreshRooms"),
            Self::CreateRoom(name) => f.debug_tuple("UiCommand::CreateRoom").field(name).finish(),
            Self::JoinRoom { id, name } => f
                .debug_struct("UiCommand::JoinRoom")
                .field("id", id)
                .field("name", name)
                .finish(),
            Self::LeaveRoom => f.write_str("UiCommand::LeaveRoom"),
            Self::SendChat(text) => f.debug_tuple("UiCommand::SendChat").field(text).finish(),
            Self::SetMuted(muted) => f.debug_tuple("UiCommand::SetMuted").field(muted).finish(),
            Self::SetDeafened(deaf) => f.debug_tuple("UiCommand::SetDeafened").field(deaf).finish(),
            Self::SetNoiseSuppression(on) => f
                .debug_tuple("UiCommand::SetNoiseSuppression")
                .field(on)
                .finish(),
            Self::SetInputGain(gain) => f
                .debug_tuple("UiCommand::SetInputGain")
                .field(gain)
                .finish(),
            Self::SetPeerVolume { peer, volume } => f
                .debug_struct("UiCommand::SetPeerVolume")
                .field("peer", peer)
                .field("volume", volume)
                .finish(),
            Self::ListScreenSources => f.write_str("UiCommand::ListScreenSources"),
            Self::StartScreenShare {
                source,
                fps,
                scale,
                include_audio,
                high_quality,
                content,
            } => f
                .debug_struct("UiCommand::StartScreenShare")
                .field("source", source)
                .field("fps", fps)
                .field("scale", scale)
                .field("include_audio", include_audio)
                .field("high_quality", high_quality)
                .field("content", content)
                .finish(),
            Self::StopScreenShare => f.write_str("UiCommand::StopScreenShare"),
            Self::Shutdown => f.write_str("UiCommand::Shutdown"),
        }
    }
}

/// State delivered by `welcome`.
#[derive(Debug, Clone)]
pub struct JoinedRoom {
    /// Room id.
    pub id: RoomId,
    /// Room name.
    pub name: RoomName,
    /// Our peer id.
    pub me: PeerId,
    /// Our display name.
    pub display_name: DisplayName,
    /// Other members.
    pub peers: Vec<PeerInfo>,
    /// Chat history, oldest first.
    pub history: Vec<ChatMessage>,
    /// Current screen publisher.
    pub screen_publisher: Option<PeerId>,
}

/// Events from the backend to the UI.
#[derive(Debug, Clone)]
pub enum AppEvent {
    /// Available audio devices.
    Devices {
        /// Input device names.
        inputs: Vec<String>,
        /// Output device names.
        outputs: Vec<String>,
    },
    /// Connected to the server; media engine running.
    Connected {
        /// Initial room list.
        rooms: Vec<RoomSummary>,
        /// Decoded remote screen-share frames.
        video: VideoFrameQueue,
    },
    /// Connecting failed.
    ConnectFailed(String),
    /// Updated room list.
    Rooms(Vec<RoomSummary>),
    /// The backend started joining this room. Shown before `welcome` arrives.
    Entering {
        /// Room id.
        id: RoomId,
        /// Room name.
        name: RoomName,
    },
    /// Joined (or re-joined after a reconnect) a room.
    Joined(JoinedRoom),
    /// Signaling dropped; retrying after a delay.
    Reconnecting {
        /// Retry number, starting at 1.
        attempt: u32,
        /// Delay before the retry.
        delay_secs: u64,
    },
    /// The room is gone or refused us; back to the lobby.
    RoomClosed(String),
    /// We left the room on request.
    Left,
    /// A member joined.
    PeerJoined(PeerInfo),
    /// A member left.
    PeerLeft(PeerId),
    /// A chat message (including our own echo).
    Chat(ChatMessage),
    /// A member started sharing.
    ScreenStarted(PeerId),
    /// The screen share ended.
    ScreenStopped,
    /// Our screen request was refused.
    ScreenDenied(String),
    /// Whether we are publishing our screen.
    LocalShare(bool),
    /// Capturable monitors and windows.
    ScreenSources(Vec<ScreenSource>),
    /// Connection state to one peer.
    PeerState {
        /// Remote peer.
        peer: PeerId,
        /// New state.
        state: PeerConnectionState,
    },
    /// Audio levels.
    Levels {
        /// Local microphone level.
        local: f32,
        /// Per remote peer.
        peers: Vec<(PeerId, f32)>,
    },
    /// A message to show in the status line.
    Notice(String),
}
