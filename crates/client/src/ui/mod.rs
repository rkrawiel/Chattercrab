//! iced application: state, messages, update and subscriptions (Elm architecture).
//!
//! `update` never blocks: every side effect is a [`UiCommand`] sent with `try_send`
//! to the backend running on its own Tokio runtime thread.

mod links;
pub mod meter;
pub mod room_model;
mod selectable;
mod share_view;
mod style;
mod view;

use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

use chattercrab_media::{ScreenContent, ScreenScale, ScreenSource, VideoFrameQueue};
use chattercrab_protocol::{DisplayName, PeerId, RoomId, RoomName, RoomSummary};
use iced::futures::stream;
use iced::{Subscription, Task, Theme, keyboard, window};
use tokio::sync::mpsc;

use crate::config::{Config, ConfigWriter, SCREEN_FPS_RANGE, STANDARD_SCREEN_FPS_MAX, ThemeChoice};
use crate::net::{AppEvent, UiCommand};
use room_model::RoomModel;
use share_view::LatestFrame;

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
    room.share_frame.clear();
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
    /// The audio settings panel replaces the bottom bar. Never open together with `picker`.
    pub audio_panel: bool,
    /// Newest remote screen picture, filled by the video subscription and drawn
    /// by the share view. Pictures never pass through [`App::update`].
    pub share_frame: LatestFrame,
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
    /// The first picture of a share arrived. Only rebuilds the view, which then
    /// swaps the "waiting" placeholder for the share view.
    ShareFrameArrived,
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
    /// Switch between the light and dark theme.
    ToggleTheme,
    /// Noise suppression toggled (connect screen or room bar).
    NoiseSuppression(bool),
    /// Microphone gain slider moved (`1.0` is 100%, `5.0` is 500%).
    InputGainChanged(f32),
    /// Microphone gain slider released; persist the value.
    InputGainReleased,
    /// Re-list audio devices.
    RefreshDevices,
    /// Open the audio settings panel in a room.
    OpenAudioPanel,
    /// Close the audio settings panel.
    CloseAudioPanel,
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
    /// A link in the chat was clicked: open it in the browser.
    OpenLink(String),
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
    config_writer: Option<ConfigWriter>,
    /// Set by Create or Join, cleared once that room is shown or the user leaves.
    pending_enter: Option<PendingEnter>,
    /// The window is fullscreen for a remote share.
    share_fullscreen: bool,
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

/// Identity and data of the video subscription.
struct VideoSource {
    queue: VideoFrameQueue,
    latest: LatestFrame,
}

impl Hash for VideoSource {
    fn hash<H: Hasher>(&self, state: &mut H) {
        "chattercrab-video".hash(state);
        self.latest.id().hash(state);
    }
}

impl App {
    /// Creates the app. The backend must already be running behind `net_tx`; its
    /// event receiver sits in `events` until the subscription takes it. Without a
    /// `config_writer` settings are not persisted.
    pub fn new(
        config: Config,
        config_writer: Option<ConfigWriter>,
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
            config_writer,
            pending_enter: None,
            share_fullscreen: false,
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
            ThemeChoice::Light => style::LIGHT.clone(),
            ThemeChoice::Dark => style::DARK.clone(),
        }
    }

    fn send(&self, cmd: UiCommand) {
        self.try_send(cmd);
    }

    /// Like [`Self::send`], but tells whether the backend got the command.
    fn try_send(&self, cmd: UiCommand) -> bool {
        match self.net_tx.try_send(cmd) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(error = %e, "backend command dropped");
                false
            }
        }
    }

    /// Queues the config for the background writer, which reports failures as notices.
    fn save_config(&self) {
        if let Some(writer) = &self.config_writer {
            writer.save(self.config.clone());
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
            Message::ShareFrameArrived => {}
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
                self.save_config();
                // Switched live once the engine runs; otherwise passed on connect.
                if !matches!(self.state.screen, Screen::Connect { .. }) {
                    self.send(UiCommand::SelectInput(self.config.input_device.clone()));
                }
            }
            Message::OutputSelected(name) => {
                self.config.output_device = (name != SYSTEM_DEFAULT).then_some(name);
                self.save_config();
                if !matches!(self.state.screen, Screen::Connect { .. }) {
                    self.send(UiCommand::SelectOutput(self.config.output_device.clone()));
                }
            }
            Message::ToggleTheme => {
                self.config.theme = match self.config.theme {
                    ThemeChoice::Dark => ThemeChoice::Light,
                    ThemeChoice::Light => ThemeChoice::Dark,
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
            Message::OpenAudioPanel => {
                if let Some(room) = self.room_mut() {
                    room.picker = None;
                    room.audio_panel = true;
                    self.send(UiCommand::ListDevices);
                }
            }
            Message::CloseAudioPanel => {
                if let Some(room) = self.room_mut() {
                    room.audio_panel = false;
                }
            }
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
            // The toggle only shows once the backend has the command, so the UI never
            // claims "muted" while the microphone is live.
            Message::ToggleMute => {
                if let Some(muted) = self.room_mut().map(|room| !room.model.muted) {
                    if self.try_send(UiCommand::SetMuted(muted)) {
                        if let Some(room) = self.room_mut() {
                            room.model.muted = muted;
                        }
                    } else {
                        self.state.notice = Some("Could not change mute. Try again.".into());
                    }
                }
            }
            Message::ToggleDeafen => {
                if let Some(deaf) = self.room_mut().map(|room| !room.model.deafened) {
                    if self.try_send(UiCommand::SetDeafened(deaf)) {
                        if let Some(room) = self.room_mut() {
                            room.model.deafened = deaf;
                        }
                    } else {
                        self.state.notice = Some("Could not change deafen. Try again.".into());
                    }
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
                    room.audio_panel = false;
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
            Message::OpenLink(url) => {
                if let Err(e) = links::open(&url) {
                    self.state.notice = Some(format!("Could not open the link: {e}"));
                }
            }
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
            audio_panel: false,
            share_frame: LatestFrame::default(),
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
                }
            }
            other => {
                if let Some(room) = self.room_mut() {
                    let before = room.model.screen_publisher;
                    room.model.apply(&other);
                    if room.model.screen_publisher != before || !room.model.remote_share_active() {
                        clear_share_frame(room);
                    }
                }
            }
        }
    }

    /// Backend events, keyboard shortcuts, and remote video while a share is shown.
    ///
    /// Video does not produce a message per picture: the stream moves the newest
    /// picture into [`RoomScreen::share_frame`], which keeps the decoder's queue
    /// drained even while the window is minimized and not redrawing, and yields
    /// only for the first picture after the slot was cleared.
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
            && room.model.remote_share_active()
        {
            subs.push(Subscription::run_with(
                VideoSource {
                    queue: video.clone(),
                    latest: room.share_frame.clone(),
                },
                |source| {
                    let queue = source.queue.clone();
                    let latest = source.latest.clone();
                    stream::unfold((), move |_| {
                        let queue = queue.clone();
                        let latest = latest.clone();
                        async move {
                            loop {
                                if latest.put(queue.next().await) {
                                    return Some((Message::ShareFrameArrived, ()));
                                }
                            }
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
        app_with_backend(8).0
    }

    /// The app and the backend end of its command channel.
    fn app_with_backend(capacity: usize) -> (App, mpsc::Receiver<UiCommand>) {
        let (tx, rx) = mpsc::channel(capacity);
        let app = App::new(Config::default(), None, tx, Arc::new(Mutex::new(None)));
        (app, rx)
    }

    fn muted(app: &App) -> (bool, bool) {
        match &app.state.screen {
            Screen::Room(room) => (room.model.muted, room.model.deafened),
            _ => panic!("expected the room"),
        }
    }

    #[test]
    fn mute_and_deafen_show_only_when_the_backend_takes_them() {
        // Room for ListDevices and one toggle.
        let (mut app, mut rx) = app_with_backend(2);
        app.enter_room(RoomId::new("room1").unwrap(), RoomName::new("1").unwrap());
        let _ = app.update(Message::ToggleMute);
        assert_eq!(muted(&app), (true, false));
        assert!(app.state.notice.is_none());
        // The queue is full now: the toggles are reverted and explained.
        let _ = app.update(Message::ToggleMute);
        let _ = app.update(Message::ToggleDeafen);
        assert_eq!(muted(&app), (true, false));
        assert!(app.state.notice.is_some());
        assert!(matches!(rx.try_recv(), Ok(UiCommand::ListDevices)));
        assert!(matches!(rx.try_recv(), Ok(UiCommand::SetMuted(true))));
        let _ = app.update(Message::ToggleDeafen);
        assert_eq!(muted(&app), (true, true));
        assert!(matches!(rx.try_recv(), Ok(UiCommand::SetDeafened(true))));
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

    fn in_room(app: &mut App) {
        app.enter_room(RoomId::new("room1").unwrap(), RoomName::new("1").unwrap());
    }

    #[test]
    fn selecting_a_device_in_a_room_switches_it_live() {
        let (mut app, mut rx) = app_with_backend(8);
        assert!(matches!(rx.try_recv(), Ok(UiCommand::ListDevices)));
        in_room(&mut app);
        let _ = app.update(Message::InputSelected("USB mic".into()));
        assert!(matches!(
            rx.try_recv(),
            Ok(UiCommand::SelectInput(Some(name))) if name == "USB mic"
        ));
        let _ = app.update(Message::OutputSelected("Headset".into()));
        assert!(matches!(
            rx.try_recv(),
            Ok(UiCommand::SelectOutput(Some(name))) if name == "Headset"
        ));
        assert_eq!(app.config.input_device.as_deref(), Some("USB mic"));
        assert_eq!(app.config.output_device.as_deref(), Some("Headset"));
        // The system default is sent as `None`.
        let _ = app.update(Message::InputSelected(SYSTEM_DEFAULT.into()));
        let _ = app.update(Message::OutputSelected(SYSTEM_DEFAULT.into()));
        assert!(matches!(rx.try_recv(), Ok(UiCommand::SelectInput(None))));
        assert!(matches!(rx.try_recv(), Ok(UiCommand::SelectOutput(None))));
        assert_eq!(app.config.input_device, None);
        assert_eq!(app.config.output_device, None);
    }

    #[test]
    fn selecting_a_device_on_the_connect_screen_only_changes_the_config() {
        let (mut app, mut rx) = app_with_backend(8);
        assert!(matches!(rx.try_recv(), Ok(UiCommand::ListDevices)));
        let _ = app.update(Message::InputSelected("USB mic".into()));
        let _ = app.update(Message::OutputSelected("Headset".into()));
        assert!(rx.try_recv().is_err());
        assert_eq!(app.config.input_device.as_deref(), Some("USB mic"));
        assert_eq!(app.config.output_device.as_deref(), Some("Headset"));
    }

    #[test]
    fn audio_panel_and_share_picker_are_exclusive() {
        let (mut app, mut rx) = app_with_backend(8);
        assert!(matches!(rx.try_recv(), Ok(UiCommand::ListDevices)));
        in_room(&mut app);
        let _ = app.update(Message::OpenAudioPanel);
        // Opening the panel refreshes the device lists.
        assert!(matches!(rx.try_recv(), Ok(UiCommand::ListDevices)));
        let _ = app.update(Message::OpenSharePicker);
        let room = app.room_mut().unwrap();
        assert!(!room.audio_panel);
        assert!(room.picker.is_some());
        let _ = app.update(Message::OpenAudioPanel);
        let room = app.room_mut().unwrap();
        assert!(room.audio_panel);
        assert!(room.picker.is_none());
        let _ = app.update(Message::CloseAudioPanel);
        assert!(!app.room_mut().unwrap().audio_panel);
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
