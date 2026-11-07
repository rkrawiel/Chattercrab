//! iced application: state, messages, update and subscriptions (Elm architecture).
//!
//! `update` never blocks: every side effect is a [`UiCommand`] sent with `try_send`
//! to the backend running on its own Tokio runtime thread.

pub mod room_model;
mod selectable;
mod view;

use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use chattercrab_media::{RgbaFrame, ScreenContent, ScreenScale, ScreenSource, VideoFrameQueue};
use chattercrab_protocol::{DisplayName, PeerId, RoomId, RoomName, RoomSummary};
use iced::futures::stream;
use iced::widget::image;
use iced::{Subscription, Task, Theme, keyboard, window};
use iced_runtime::image::Allocation;
use tokio::sync::{Notify, mpsc};

use crate::config::{Config, SCREEN_FPS_RANGE, STANDARD_SCREEN_FPS_MAX, ThemeChoice};
use crate::net::{AppEvent, UiCommand};
use room_model::RoomModel;

/// Label for the "use the system default device" choice.
pub const SYSTEM_DEFAULT: &str = "System default";

/// Which screen is shown.
#[derive(Debug)]
pub enum Screen {
    /// Server, name and device selection.
    Connect {
        /// A connect attempt is in flight.
        connecting: bool,
    },
    /// Room list.
    Lobby(Lobby),
    /// Inside a room.
    Room(Box<RoomScreen>),
}

/// Lobby screen state.
#[derive(Debug, Default)]
pub struct Lobby {
    /// Rooms on the server.
    pub rooms: Vec<RoomSummary>,
    /// Name typed for a new room.
    pub new_room: String,
}

/// Screen-share source picker state.
#[derive(Debug, Clone)]
pub struct SharePicker {
    /// Available sources (empty until listed).
    pub sources: Vec<SourceOption>,
    /// Chosen source.
    pub selected: Option<SourceOption>,
    /// Chosen frame rate.
    pub fps: u32,
    /// 4K, 2K, 1440p, or 1080p cap.
    pub scale: ScreenScale,
    /// Mix other applications' audio into the outgoing voice.
    pub include_audio: bool,
    /// H.264 High, higher fixed bitrate, low delay.
    pub high_quality: bool,
    /// Desktop (crisp fonts) or game (less blur). Independent of size and frame rate.
    pub content: ScreenContent,
}

/// A [`ScreenSource`] with a display label, for pick lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceOption(pub ScreenSource);

impl std::fmt::Display for SourceOption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self.0.kind {
            chattercrab_media::ScreenSourceKind::Monitor => "Screen",
            chattercrab_media::ScreenSourceKind::Window => "Window",
        };
        write!(f, "{kind}: {}", self.0.name)
    }
}

fn clear_share_frame(room: &mut RoomScreen) {
    room.frame = None;
    room.frame_memory = None;
    room.pending_frame = None;
    room.frame_loading = false;
    room.inflight_upload = None;
}

/// Room screen state.
#[derive(Debug)]
pub struct RoomScreen {
    /// Room model fed by backend events.
    pub model: RoomModel,
    /// Chat input text.
    pub chat_input: String,
    /// Open share picker, if any.
    pub picker: Option<SharePicker>,
    /// Latest remote screen frame that has finished uploading to the GPU.
    pub frame: Option<image::Handle>,
    /// Holds the GPU copy of [`Self::frame`]. Dropping it before the next upload
    /// finishes is what makes the picture flash blank between frames.
    frame_memory: Option<Allocation>,
    /// A frame is uploading; the next decoded frame waits in [`Self::pending_frame`].
    frame_loading: bool,
    /// One frame that arrived while an upload was in progress.
    pending_frame: Option<RgbaFrame>,
    /// Identifies the upload currently in flight, so a late callback is ignored.
    inflight_upload: Option<u64>,
    /// Bumps each time a new upload starts.
    upload_id: u64,
    /// Bumps when a remote share starts, so the frame subscription restarts.
    share_epoch: u64,
}

/// Mutable UI state.
#[derive(Debug)]
pub struct UiState {
    /// Current screen.
    pub screen: Screen,
    /// Input device names.
    pub inputs: Vec<String>,
    /// Output device names.
    pub outputs: Vec<String>,
    /// Status line text.
    pub notice: Option<String>,
    /// Remote video frames, available once connected.
    pub video: Option<VideoFrameQueue>,
}

/// UI messages.
#[derive(Debug, Clone)]
pub enum Message {
    /// Event from the backend.
    Backend(AppEvent),
    /// A decoded remote screen frame.
    VideoFrame(RgbaFrame),
    /// A remote frame is ready to draw. `memory` is the GPU copy when the
    /// renderer allocated one; without it the picture can flash between frames.
    FramePresented {
        /// Image to draw.
        handle: image::Handle,
        /// GPU allocation, kept alive while this frame is on screen.
        memory: Option<Allocation>,
        /// Matches [`RoomScreen`]'s in-flight upload. A stale callback is ignored.
        upload: u64,
    },
    /// Show or leave the fullscreen share view.
    ToggleShareFullscreen,
    /// Leave fullscreen (Escape, or the share ended).
    ExitShareFullscreen,
    /// Server URL edited.
    ServerUrlChanged(String),
    /// Basic auth user edited.
    AuthUserChanged(String),
    /// Shared password edited.
    PasswordChanged(String),
    /// Display name edited.
    DisplayNameChanged(String),
    /// Microphone chosen.
    InputSelected(String),
    /// Speakers chosen.
    OutputSelected(String),
    /// Dark theme toggled.
    DarkTheme(bool),
    /// Noise suppression toggled (connect screen or room bar).
    NoiseSuppression(bool),
    /// Microphone gain slider moved (`1.0` is 100%, `5.0` is 500%).
    InputGainChanged(f32),
    /// Microphone gain slider released; persist the value.
    InputGainReleased,
    /// Re-list audio devices.
    RefreshDevices,
    /// Connect button.
    Connect,
    /// Back to the connect screen.
    Disconnect,
    /// Reload the room list.
    RefreshRooms,
    /// New room name edited.
    NewRoomChanged(String),
    /// Create button.
    CreateRoom,
    /// Join a room.
    JoinRoom(RoomId, RoomName),
    /// Chat input edited.
    ChatChanged(String),
    /// Enter in the chat input.
    SendChat,
    /// Toggle microphone mute (button or Ctrl+M).
    ToggleMute,
    /// Toggle deafen (button or Ctrl+D).
    ToggleDeafen,
    /// Open the share picker.
    OpenSharePicker,
    /// Source picked.
    ShareSourceSelected(SourceOption),
    /// Frame rate picked.
    ShareFpsSelected(u32),
    /// Resolution picked.
    ShareScaleSelected(ScreenScale),
    /// Desktop or game mode picked.
    ShareContentSelected(ScreenContent),
    /// "Share audio from other apps" toggled.
    ShareAudioToggled(bool),
    /// High-quality screen share toggled.
    ShareHighQualityToggled(bool),
    /// Start sharing the picked source.
    StartShare,
    /// Close the picker.
    CancelShare,
    /// Stop sharing.
    StopShare,
    /// Leave the room.
    Leave,
    /// Volume slider moved.
    PeerVolumeChanged(PeerId, f32),
    /// Volume slider released.
    PeerVolumeCommitted(PeerId),
    /// Local mute of one peer toggled.
    PeerMuteToggled(PeerId, bool),
    /// Clear the status line.
    DismissNotice,
}

/// Which room the UI is waiting to open. A `welcome` that arrives after Leave
/// must not pull the user back in.
#[derive(Debug)]
enum PendingEnter {
    /// Create-and-join: the id arrives with [`AppEvent::Entering`].
    Created,
    /// Join clicked for this room.
    Room(RoomId),
}

/// The application: UI state, persisted config, and the command channel to the backend.
pub struct App {
    /// UI state.
    pub state: UiState,
    /// Persisted settings (also the connect form).
    pub config: Config,
    /// Commands to the backend.
    pub net_tx: mpsc::Sender<UiCommand>,
    bridge: EventBridge,
    config_path: Option<PathBuf>,
    /// Set by Create or Join, cleared once that room is shown or the user leaves.
    pending_enter: Option<PendingEnter>,
    /// The window is fullscreen for a remote share.
    share_fullscreen: bool,
    /// Paces remote frames so one upload finishes before the next frame is taken.
    video_gate: VideoGate,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App").field("state", &self.state).finish()
    }
}

/// Shared slot holding the backend event receiver until the subscription takes it.
pub type EventSlot = Arc<Mutex<Option<mpsc::Receiver<AppEvent>>>>;

/// Wraps the backend event receiver so a subscription can take ownership of it once.
#[derive(Clone)]
struct EventBridge(EventSlot);

impl Hash for EventBridge {
    fn hash<H: Hasher>(&self, state: &mut H) {
        "chattercrab-backend-events".hash(state);
    }
}

/// Identity wrapper for the video subscription.
struct VideoSource {
    queue: VideoFrameQueue,
    gate: VideoGate,
    publisher: PeerId,
    epoch: u64,
}

impl Hash for VideoSource {
    fn hash<H: Hasher>(&self, state: &mut H) {
        "chattercrab-video".hash(state);
        self.publisher.hash(state);
        self.epoch.hash(state);
    }
}

/// Lets the UI take the next decoded frame only after the previous one is on screen.
#[derive(Clone)]
struct VideoGate {
    permits: Arc<AtomicUsize>,
    resume: Arc<Notify>,
}

impl App {
    /// Creates the app. The backend must already be running behind `net_tx`; its
    /// event receiver sits in `events` until the subscription takes it.
    pub fn new(
        config: Config,
        config_path: Option<PathBuf>,
        net_tx: mpsc::Sender<UiCommand>,
        events: EventSlot,
    ) -> Self {
        let app = Self {
            state: UiState {
                screen: Screen::Connect { connecting: false },
                inputs: Vec::new(),
                outputs: Vec::new(),
                notice: None,
                video: None,
            },
            config,
            net_tx,
            bridge: EventBridge(events),
            config_path,
            pending_enter: None,
            share_fullscreen: false,
            video_gate: VideoGate {
                permits: Arc::new(AtomicUsize::new(0)),
                resume: Arc::new(Notify::new()),
            },
        };
        app.send(UiCommand::ListDevices);
        app
    }

    /// Window title.
    pub fn title(&self) -> String {
        match &self.state.screen {
            Screen::Room(room) => format!("{} - Chattercrab", room.model.name),
            _ => "Chattercrab".to_owned(),
        }
    }

    /// Light or dark theme.
    pub fn theme(&self) -> Theme {
        match self.config.theme {
            ThemeChoice::Light => Theme::Light,
            ThemeChoice::Dark => Theme::Dark,
        }
    }

    fn send(&self, cmd: UiCommand) {
        if let Err(e) = self.net_tx.try_send(cmd) {
            tracing::warn!(error = %e, "backend command dropped");
        }
    }

    fn save_config(&mut self) {
        if let Some(path) = &self.config_path
            && let Err(e) = self.config.save(path)
        {
            self.state.notice = Some(format!("Could not save settings: {e:#}"));
        }
    }

    fn room_mut(&mut self) -> Option<&mut RoomScreen> {
        match &mut self.state.screen {
            Screen::Room(room) => Some(room),
            _ => None,
        }
    }

    /// Elm update.
    pub fn update(&mut self, message: Message) -> Task<Message> {
        let mut followup = Task::none();
        match message {
            Message::Backend(event) => {
                self.on_backend(event);
                if self.share_fullscreen && !self.remote_share_visible() {
                    followup = self.set_share_fullscreen(false);
                }
            }
            Message::VideoFrame(frame) => followup = self.queue_share_frame(frame),
            Message::FramePresented {
                handle,
                memory,
                upload,
            } => {
                followup = self.finish_share_frame(handle, memory, upload);
            }
            Message::ToggleShareFullscreen => {
                if self.remote_share_visible() {
                    followup = self.set_share_fullscreen(!self.share_fullscreen);
                }
            }
            Message::ExitShareFullscreen => {
                if self.share_fullscreen {
                    followup = self.set_share_fullscreen(false);
                }
            }
            Message::ServerUrlChanged(url) => self.config.server_url = url,
            Message::AuthUserChanged(user) => self.config.auth_user = user,
            Message::PasswordChanged(password) => self.config.password = password,
            Message::DisplayNameChanged(name) => self.config.display_name = name,
            Message::InputSelected(name) => {
                self.config.input_device = (name != SYSTEM_DEFAULT).then_some(name);
            }
            Message::OutputSelected(name) => {
                self.config.output_device = (name != SYSTEM_DEFAULT).then_some(name);
            }
            Message::DarkTheme(dark) => {
                self.config.theme = if dark {
                    ThemeChoice::Dark
                } else {
                    ThemeChoice::Light
                };
                self.save_config();
            }
            Message::NoiseSuppression(on) => {
                self.config.noise_suppression = on;
                self.save_config();
                // Applied live once the engine runs; otherwise passed on connect.
                if !matches!(self.state.screen, Screen::Connect { .. }) {
                    self.send(UiCommand::SetNoiseSuppression(on));
                }
            }
            Message::InputGainChanged(gain) => {
                let gain = chattercrab_media::clamp_input_gain(gain);
                self.config.input_gain = gain;
                if !matches!(self.state.screen, Screen::Connect { .. }) {
                    self.send(UiCommand::SetInputGain(gain));
                }
            }
            Message::InputGainReleased => self.save_config(),
            Message::RefreshDevices => self.send(UiCommand::ListDevices),
            Message::Connect => self.connect(),
            Message::Disconnect => {
                self.pending_enter = None;
                self.send(UiCommand::Disconnect);
                self.state.screen = Screen::Connect { connecting: false };
                if self.share_fullscreen {
                    followup = self.set_share_fullscreen(false);
                }
            }
            Message::RefreshRooms => self.send(UiCommand::RefreshRooms),
            Message::NewRoomChanged(name) => {
                if let Screen::Lobby(lobby) = &mut self.state.screen {
                    lobby.new_room = name;
                }
            }
            Message::CreateRoom => {
                if let Screen::Lobby(lobby) = &mut self.state.screen {
                    match RoomName::new(&lobby.new_room) {
                        Ok(name) => {
                            lobby.new_room.clear();
                            // The room screen opens on `Entering`, once the id is known.
                            self.pending_enter = Some(PendingEnter::Created);
                            self.send(UiCommand::CreateRoom(name));
                        }
                        Err(e) => self.state.notice = Some(e.to_string()),
                    }
                }
            }
            Message::JoinRoom(id, name) => {
                self.pending_enter = Some(PendingEnter::Room(id.clone()));
                self.enter_room(id.clone(), name.clone());
                self.send(UiCommand::JoinRoom { id, name });
            }
            Message::ChatChanged(text) => {
                if let Some(room) = self.room_mut() {
                    room.chat_input = text;
                }
            }
            Message::SendChat => {
                let text = self
                    .room_mut()
                    .map(|room| std::mem::take(&mut room.chat_input));
                if let Some(text) = text {
                    match chattercrab_protocol::validate_chat_text(&text) {
                        Ok(valid) => self.send(UiCommand::SendChat(valid.to_owned())),
                        Err(_) if text.trim().is_empty() => {}
                        Err(e) => {
                            if let Some(room) = self.room_mut() {
                                room.chat_input = text;
                            }
                            self.state.notice = Some(e.to_string());
                        }
                    }
                }
            }
            Message::ToggleMute => {
                if let Some(room) = self.room_mut() {
                    room.model.muted = !room.model.muted;
                    let muted = room.model.muted;
                    self.send(UiCommand::SetMuted(muted));
                }
            }
            Message::ToggleDeafen => {
                if let Some(room) = self.room_mut() {
                    room.model.deafened = !room.model.deafened;
                    let deaf = room.model.deafened;
                    self.send(UiCommand::SetDeafened(deaf));
                }
            }
            Message::OpenSharePicker => {
                let high_quality = self.config.high_quality;
                let max_fps = if high_quality {
                    *SCREEN_FPS_RANGE.end()
                } else {
                    STANDARD_SCREEN_FPS_MAX
                };
                let fps = self
                    .config
                    .screen_fps
                    .clamp(*SCREEN_FPS_RANGE.start(), max_fps);
                let scale = self.config.screen_scale;
                let include_audio = self.config.share_audio;
                let content = self.config.screen_content;
                if let Some(room) = self.room_mut() {
                    room.picker = Some(SharePicker {
                        sources: Vec::new(),
                        selected: None,
                        fps,
                        scale,
                        include_audio,
                        high_quality,
                        content,
                    });
                    self.send(UiCommand::ListScreenSources);
                }
            }
            Message::ShareSourceSelected(source) => {
                if let Some(picker) = self.room_mut().and_then(|r| r.picker.as_mut()) {
                    picker.selected = Some(source);
                }
            }
            Message::ShareFpsSelected(fps) => {
                if let Some(picker) = self.room_mut().and_then(|r| r.picker.as_mut()) {
                    let max_fps = if picker.high_quality {
                        *SCREEN_FPS_RANGE.end()
                    } else {
                        STANDARD_SCREEN_FPS_MAX
                    };
                    picker.fps = fps.clamp(*SCREEN_FPS_RANGE.start(), max_fps);
                }
            }
            Message::ShareScaleSelected(scale) => {
                if let Some(picker) = self.room_mut().and_then(|r| r.picker.as_mut()) {
                    picker.scale = scale;
                }
            }
            Message::ShareContentSelected(content) => {
                self.config.screen_content = content;
                self.save_config();
                if let Some(picker) = self.room_mut().and_then(|r| r.picker.as_mut()) {
                    picker.content = content;
                }
            }
            Message::ShareAudioToggled(on) => {
                self.config.share_audio = on;
                self.save_config();
                if let Some(picker) = self.room_mut().and_then(|r| r.picker.as_mut()) {
                    picker.include_audio = on;
                }
            }
            Message::ShareHighQualityToggled(on) => {
                self.config.high_quality = on;
                if !on && self.config.screen_fps > STANDARD_SCREEN_FPS_MAX {
                    self.config.screen_fps = STANDARD_SCREEN_FPS_MAX;
                }
                self.save_config();
                if let Some(picker) = self.room_mut().and_then(|r| r.picker.as_mut()) {
                    picker.high_quality = on;
                    if !on && picker.fps > STANDARD_SCREEN_FPS_MAX {
                        picker.fps = STANDARD_SCREEN_FPS_MAX;
                    }
                }
            }
            Message::StartShare => {
                let picked = self.room_mut().and_then(|r| r.picker.take()).and_then(|p| {
                    p.selected.map(|s| {
                        (
                            s.0,
                            p.fps,
                            p.scale,
                            p.include_audio,
                            p.high_quality,
                            p.content,
                        )
                    })
                });
                if let Some((source, fps, scale, include_audio, high_quality, content)) = picked {
                    let max_fps = if high_quality {
                        *SCREEN_FPS_RANGE.end()
                    } else {
                        STANDARD_SCREEN_FPS_MAX
                    };
                    let fps = fps.clamp(*SCREEN_FPS_RANGE.start(), max_fps);
                    self.config.screen_fps = fps;
                    self.config.screen_scale = scale;
                    self.config.share_audio = include_audio;
                    self.config.high_quality = high_quality;
                    self.config.screen_content = content;
                    self.save_config();
                    self.send(UiCommand::StartScreenShare {
                        source,
                        fps,
                        scale,
                        include_audio,
                        high_quality,
                        content,
                    });
                }
            }
            Message::CancelShare => {
                if let Some(room) = self.room_mut() {
                    room.picker = None;
                }
            }
            Message::StopShare => self.send(UiCommand::StopScreenShare),
            Message::Leave => {
                self.pending_enter = None;
                self.send(UiCommand::LeaveRoom);
                self.state.screen = Screen::Lobby(Lobby::default());
                if self.share_fullscreen {
                    followup = self.set_share_fullscreen(false);
                }
            }
            Message::PeerVolumeChanged(peer, volume) => {
                if let Some(m) = self.room_mut().and_then(|r| r.model.member_mut(peer)) {
                    m.volume = volume.clamp(0.0, chattercrab_media::PEER_VOLUME_MAX);
                }
            }
            Message::PeerVolumeCommitted(peer) => self.push_peer_volume(peer),
            Message::PeerMuteToggled(peer, muted) => {
                if let Some(m) = self.room_mut().and_then(|r| r.model.member_mut(peer)) {
                    m.muted = muted;
                }
                self.push_peer_volume(peer);
            }
            Message::DismissNotice => self.state.notice = None,
        }
        followup
    }

    fn remote_share_visible(&self) -> bool {
        matches!(&self.state.screen, Screen::Room(room) if room.model.remote_share_active())
    }

    fn set_share_fullscreen(&mut self, on: bool) -> Task<Message> {
        self.share_fullscreen = on;
        let mode = if on {
            window::Mode::Fullscreen
        } else {
            window::Mode::Windowed
        };
        window::oldest().then(move |id| match id {
            Some(id) => window::set_mode(id, mode),
            None => Task::none(),
        })
    }

    fn queue_share_frame(&mut self, frame: RgbaFrame) -> Task<Message> {
        let Some(room) = self.room_mut() else {
            self.release_video_frame();
            return Task::none();
        };
        if !room.model.remote_share_active() {
            self.release_video_frame();
            return Task::none();
        }
        if room.frame_loading {
            // The subscription only pulls the next frame after this upload
            // finishes. One waiting frame covers a callback that raced ahead.
            if room.pending_frame.is_none() {
                room.pending_frame = Some(frame);
            }
            return Task::none();
        }
        Self::upload_share_frame(room, frame)
    }

    fn finish_share_frame(
        &mut self,
        handle: image::Handle,
        memory: Option<Allocation>,
        upload: u64,
    ) -> Task<Message> {
        let Some(room) = self.room_mut() else {
            return Task::none();
        };
        if room.inflight_upload != Some(upload) {
            return Task::none();
        }
        room.inflight_upload = None;
        room.frame_loading = false;
        if !room.model.remote_share_active() {
            clear_share_frame(room);
            self.release_video_frame();
            return Task::none();
        }
        room.frame = Some(handle);
        room.frame_memory = memory;
        let Some(next) = room.pending_frame.take() else {
            self.release_video_frame();
            return Task::none();
        };
        Self::upload_share_frame(room, next)
    }

    fn upload_share_frame(room: &mut RoomScreen, frame: RgbaFrame) -> Task<Message> {
        room.frame_loading = true;
        room.upload_id = room.upload_id.wrapping_add(1);
        let upload = room.upload_id;
        room.inflight_upload = Some(upload);
        let handle = image::Handle::from_rgba(
            frame.width,
            frame.height,
            bytes::Bytes::from_owner(frame.pixels),
        );
        let shown = handle.clone();
        iced_runtime::image::allocate(handle).map(move |result| Message::FramePresented {
            memory: result.ok(),
            handle: shown.clone(),
            upload,
        })
    }

    /// The picture is on screen (or was discarded). The subscription may take the next frame.
    fn release_video_frame(&self) {
        self.video_gate.permits.fetch_add(1, Ordering::Release);
        self.video_gate.resume.notify_one();
    }

    fn push_peer_volume(&mut self, peer: PeerId) {
        let volume = self
            .room_mut()
            .and_then(|r| r.model.member_mut(peer))
            .map(|m| m.effective_volume());
        if let Some(volume) = volume {
            self.send(UiCommand::SetPeerVolume { peer, volume });
        }
    }

    fn connect(&mut self) {
        let name = match DisplayName::new(&self.config.display_name) {
            Ok(name) => name,
            Err(e) => {
                self.state.notice = Some(e.to_string());
                return;
            }
        };
        self.config.display_name = name.to_string();
        self.save_config();
        self.state.notice = None;
        self.state.screen = Screen::Connect { connecting: true };
        self.send(UiCommand::Connect {
            server_url: self.config.server_url.clone(),
            auth_user: self.config.auth_user.clone(),
            password: self.config.password.clone(),
            display_name: name,
            input_device: self.config.input_device.clone(),
            output_device: self.config.output_device.clone(),
            noise_suppression: self.config.noise_suppression,
            input_gain: self.config.input_gain,
        });
    }

    fn enter_room(&mut self, id: RoomId, name: RoomName) {
        self.state.notice = None;
        self.state.screen = Screen::Room(Box::new(RoomScreen {
            model: RoomModel::joining(id, name, self.config.display_name.clone()),
            chat_input: String::new(),
            picker: None,
            frame: None,
            frame_memory: None,
            frame_loading: false,
            pending_frame: None,
            inflight_upload: None,
            upload_id: 0,
            share_epoch: 0,
        }));
    }

    fn on_backend(&mut self, event: AppEvent) {
        match event {
            AppEvent::Devices { inputs, outputs } => {
                self.state.inputs = inputs;
                self.state.outputs = outputs;
            }
            AppEvent::Connected { rooms, video } => {
                self.state.video = Some(video);
                self.state.screen = Screen::Lobby(Lobby {
                    rooms,
                    new_room: String::new(),
                });
            }
            AppEvent::ConnectFailed(why) => {
                self.state.notice = Some(why);
                self.state.screen = Screen::Connect { connecting: false };
            }
            AppEvent::Rooms(rooms) => {
                if let Screen::Lobby(lobby) = &mut self.state.screen {
                    lobby.rooms = rooms;
                }
            }
            AppEvent::RoomClosed(why) => {
                self.state.notice = Some(why);
                self.state.screen = Screen::Lobby(Lobby::default());
            }
            AppEvent::Left => {
                if !matches!(self.state.screen, Screen::Lobby(_)) {
                    self.state.screen = Screen::Lobby(Lobby::default());
                }
            }
            AppEvent::ScreenSources(sources) => {
                if let Some(picker) = self.room_mut().and_then(|r| r.picker.as_mut()) {
                    picker.sources = sources.into_iter().map(SourceOption).collect();
                    picker.selected = picker.sources.first().cloned();
                }
            }
            AppEvent::ScreenDenied(reason) => {
                self.state.notice = Some(format!("Screen share denied: {reason}"));
            }
            AppEvent::Notice(text) => self.state.notice = Some(text),
            AppEvent::Entering { id, name } => {
                let follow = match &self.pending_enter {
                    Some(PendingEnter::Created) => true,
                    Some(PendingEnter::Room(expected)) => *expected == id,
                    None => false,
                };
                if follow {
                    self.pending_enter = None;
                    let same = self.room_mut().is_some_and(|room| room.model.id == id);
                    if !same {
                        self.enter_room(id, name);
                    }
                }
            }
            AppEvent::Joined(joined) => {
                // Join opens the room immediately; Create opens it on `Entering`.
                // A welcome that was already in flight when the user left must not
                // reopen the room.
                if let Some(room) = self.room_mut()
                    && room.model.id == joined.id
                {
                    room.model.apply_joined(joined);
                    clear_share_frame(room);
                    if room.model.remote_share_active() {
                        room.share_epoch = room.share_epoch.wrapping_add(1);
                    }
                }
            }
            other => {
                if let Some(room) = self.room_mut() {
                    let before = room.model.screen_publisher;
                    room.model.apply(&other);
                    if room.model.screen_publisher != before && room.model.remote_share_active() {
                        room.share_epoch = room.share_epoch.wrapping_add(1);
                    }
                    if !room.model.remote_share_active() {
                        clear_share_frame(room);
                    }
                }
            }
        }
    }

    /// Backend events, keyboard shortcuts, and remote video frames while a share is shown.
    pub fn subscription(&self) -> Subscription<Message> {
        let backend = Subscription::run_with(self.bridge.clone(), |bridge| {
            let rx = bridge.0.lock().ok().and_then(|mut slot| slot.take());
            stream::unfold(rx, |rx| async move {
                let mut rx = rx?;
                let event = rx.recv().await?;
                Some((Message::Backend(event), Some(rx)))
            })
        });
        let keys = iced::event::listen_with(|event, _status, _window| match event {
            iced::Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) => {
                match key.as_ref() {
                    keyboard::Key::Named(keyboard::key::Named::Escape) => {
                        Some(Message::ExitShareFullscreen)
                    }
                    keyboard::Key::Character("m") if modifiers.command() => {
                        Some(Message::ToggleMute)
                    }
                    keyboard::Key::Character("d") if modifiers.command() => {
                        Some(Message::ToggleDeafen)
                    }
                    _ => None,
                }
            }
            _ => None,
        });
        let mut subs = vec![backend, keys];
        if let (Screen::Room(room), Some(video)) = (&self.state.screen, &self.state.video)
            && let Some(publisher) = room.model.screen_publisher
            && room.model.remote_share_active()
        {
            let gate = self.video_gate.clone();
            subs.push(Subscription::run_with(
                VideoSource {
                    queue: video.clone(),
                    gate,
                    publisher,
                    epoch: room.share_epoch,
                },
                |source| {
                    // The first frame is taken immediately. Each later frame waits
                    // until the previous upload has been presented, so frames are
                    // not dropped or drawn in a burst while the GPU is busy.
                    source.gate.permits.store(0, Ordering::Release);
                    let queue = source.queue.clone();
                    let gate = source.gate.clone();
                    stream::unfold(false, move |wait| {
                        let queue = queue.clone();
                        let gate = gate.clone();
                        async move {
                            if wait {
                                loop {
                                    let notified = gate.resume.notified();
                                    if gate.permits.swap(0, Ordering::AcqRel) > 0 {
                                        break;
                                    }
                                    notified.await;
                                }
                            }
                            let frame = queue.next().await;
                            Some((Message::VideoFrame(frame), true))
                        }
                    })
                },
            ));
        }
        Subscription::batch(subs)
    }

    /// Elm view.
    pub fn view(&self) -> iced::Element<'_, Message> {
        view::view(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::JoinedRoom;

    fn app() -> App {
        let (tx, _rx) = mpsc::channel(8);
        App::new(Config::default(), None, tx, Arc::new(Mutex::new(None)))
    }

    fn peer() -> PeerId {
        "00000000-0000-4000-8000-000000000001".parse().unwrap()
    }

    fn entering(id: &str) -> AppEvent {
        AppEvent::Entering {
            id: RoomId::new(id).unwrap(),
            name: RoomName::new("1").unwrap(),
        }
    }

    fn joined(id: &str) -> AppEvent {
        AppEvent::Joined(JoinedRoom {
            id: RoomId::new(id).unwrap(),
            name: RoomName::new("1").unwrap(),
            me: peer(),
            display_name: DisplayName::new("ada").unwrap(),
            peers: Vec::new(),
            history: Vec::new(),
            screen_publisher: None,
        })
    }

    fn room_id(app: &App) -> Option<&str> {
        match &app.state.screen {
            Screen::Room(room) => Some(room.model.id.as_str()),
            _ => None,
        }
    }

    #[test]
    fn create_opens_the_room_when_the_backend_enters_it() {
        let mut app = app();
        app.state.screen = Screen::Lobby(Lobby::default());
        app.pending_enter = Some(PendingEnter::Created);
        app.on_backend(entering("room1"));
        assert_eq!(room_id(&app), Some("room1"));
        assert!(app.pending_enter.is_none());
        app.on_backend(joined("room1"));
        assert!(matches!(
            &app.state.screen,
            Screen::Room(room) if room.model.me == Some(peer())
        ));
    }

    #[test]
    fn welcome_after_leave_does_not_reopen_the_room() {
        let mut app = app();
        app.state.screen = Screen::Lobby(Lobby::default());
        app.pending_enter = Some(PendingEnter::Created);
        let _ = app.update(Message::Leave);
        app.on_backend(entering("room1"));
        app.on_backend(joined("room1"));
        assert!(matches!(app.state.screen, Screen::Lobby(_)));
    }

    #[test]
    fn share_mode_does_not_change_resolution_or_frame_rate() {
        let mut app = app();
        app.state.screen = Screen::Lobby(Lobby::default());
        app.pending_enter = Some(PendingEnter::Created);
        app.on_backend(entering("room1"));
        app.on_backend(joined("room1"));
        app.config.high_quality = true;
        app.config.screen_fps = 120;
        app.config.screen_scale = ScreenScale::P1440;
        let _ = app.update(Message::OpenSharePicker);
        let _ = app.update(Message::ShareFpsSelected(120));
        let _ = app.update(Message::ShareScaleSelected(ScreenScale::P1080));
        let _ = app.update(Message::ShareContentSelected(ScreenContent::Game));
        let picker = match &app.state.screen {
            Screen::Room(room) => room.picker.as_ref().unwrap(),
            _ => panic!("expected the room"),
        };
        assert_eq!(picker.fps, 120);
        assert_eq!(picker.scale, ScreenScale::P1080);
        assert_eq!(picker.content, ScreenContent::Game);
        let _ = app.update(Message::ShareContentSelected(ScreenContent::Desktop));
        let _ = app.update(Message::ShareScaleSelected(ScreenScale::P4K));
        let picker = match &app.state.screen {
            Screen::Room(room) => room.picker.as_ref().unwrap(),
            _ => panic!("expected the room"),
        };
        assert_eq!(picker.fps, 120);
        assert_eq!(picker.scale, ScreenScale::P4K);
        assert_eq!(picker.content, ScreenContent::Desktop);
    }

    #[test]
    fn entering_a_different_room_is_ignored() {
        let mut app = app();
        app.state.screen = Screen::Lobby(Lobby::default());
        app.pending_enter = Some(PendingEnter::Room(RoomId::new("room2").unwrap()));
        app.on_backend(entering("room1"));
        assert!(matches!(app.state.screen, Screen::Lobby(_)));
        assert!(app.pending_enter.is_some());
    }
}
