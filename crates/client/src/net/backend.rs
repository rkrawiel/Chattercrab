//! The backend task: owns the REST client, the media engine and the signaling
//! connection, and applies [`Effect`]s produced by the pure [`Session`].

use std::time::Duration;

use chattercrab_media::{MediaCommand, MediaConfig, MediaEngine, MediaEvent};
use chattercrab_protocol::{DisplayName, RoomId, RoomName};
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::backoff::Backoff;
use super::rest::Rest;
use super::session::{Effect, Session};
use super::signaling::{self, CloseReason, SignalEvent, SignalHandle};
use super::{AppEvent, UiCommand};

/// Capacity of the signaling task to backend channel.
const SIGNAL_EVENT_CAPACITY: usize = 256;

/// Spawns a dedicated OS thread running a Tokio runtime with the backend task.
///
/// Returns the join handle; the thread ends when the command channel closes or
/// [`UiCommand::Shutdown`] arrives.
pub fn spawn_runtime_thread(
    commands: mpsc::Receiver<UiCommand>,
    events: mpsc::Sender<AppEvent>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("chattercrab-net".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("chattercrab-rt")
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!(error = %e, "cannot start network runtime");
                    return;
                }
            };
            runtime.block_on(Backend::new(commands, events).run());
            // Give webrtc and socket tasks a moment to close cleanly.
            runtime.shutdown_timeout(Duration::from_secs(2));
        })
}

/// A room we are in (or reconnecting to).
struct ActiveRoom {
    session: Session,
    backoff: Backoff,
    conn: Option<SignalHandle>,
    generation: u64,
    retry_at: Option<Instant>,
}

/// Backend state. Lives on the runtime thread.
pub struct Backend {
    commands: mpsc::Receiver<UiCommand>,
    events: mpsc::Sender<AppEvent>,
    signal_tx: mpsc::Sender<SignalEvent>,
    signal_rx: mpsc::Receiver<SignalEvent>,
    rest: Option<Rest>,
    media: Option<MediaEngine>,
    display_name: Option<DisplayName>,
    room: Option<ActiveRoom>,
    next_generation: u64,
}

impl Backend {
    /// Creates a backend bound to the UI channels.
    pub fn new(commands: mpsc::Receiver<UiCommand>, events: mpsc::Sender<AppEvent>) -> Self {
        let (signal_tx, signal_rx) = mpsc::channel(SIGNAL_EVENT_CAPACITY);
        Self {
            commands,
            events,
            signal_tx,
            signal_rx,
            rest: None,
            media: None,
            display_name: None,
            room: None,
            next_generation: 0,
        }
    }

    /// Runs until shutdown.
    pub async fn run(mut self) {
        loop {
            let retry_at = self.room.as_ref().and_then(|r| r.retry_at);
            tokio::select! {
                cmd = self.commands.recv() => match cmd {
                    None | Some(UiCommand::Shutdown) => break,
                    Some(cmd) => self.on_command(cmd).await,
                },
                event = next_media_event(&mut self.media) => match event {
                    Some(event) => self.on_media_event(event).await,
                    None => {
                        self.media = None;
                        self.ui(AppEvent::Notice("media engine stopped".into())).await;
                    }
                },
                Some(event) = self.signal_rx.recv() => self.on_signal_event(event).await,
                () = sleep_until_opt(retry_at) => self.reconnect().await,
            }
        }
        self.leave_room().await;
        if let Some(media) = self.media.take() {
            let _ = media.commands.send(MediaCommand::Shutdown).await;
        }
        tracing::info!("backend stopped");
    }

    async fn ui(&self, event: AppEvent) {
        if self.events.send(event).await.is_err() {
            tracing::debug!("UI event channel closed");
        }
    }

    async fn media(&self, cmd: MediaCommand) {
        if let Some(media) = &self.media
            && media.commands.send(cmd).await.is_err()
        {
            tracing::warn!("media engine command channel closed");
        }
    }

    async fn on_command(&mut self, cmd: UiCommand) {
        match cmd {
            UiCommand::ListDevices => {
                let events = self.events.clone();
                tokio::spawn(async move {
                    let listed = tokio::task::spawn_blocking(|| {
                        (
                            chattercrab_media::list_input_devices(),
                            chattercrab_media::list_output_devices(),
                        )
                    })
                    .await;
                    let event = match listed {
                        Ok((Ok(inputs), Ok(outputs))) => AppEvent::Devices { inputs, outputs },
                        Ok((Err(e), _)) | Ok((_, Err(e))) => {
                            AppEvent::Notice(format!("cannot list audio devices: {e}"))
                        }
                        Err(e) => AppEvent::Notice(format!("cannot list audio devices: {e}")),
                    };
                    let _ = events.send(event).await;
                });
            }
            UiCommand::Connect {
                server_url,
                auth_user,
                password,
                display_name,
                input_device,
                output_device,
                noise_suppression,
                input_gain,
            } => {
                let media = MediaConfig {
                    input_device,
                    output_device,
                    noise_suppression,
                    input_gain,
                };
                self.connect(&server_url, auth_user, password, display_name, media)
                    .await;
            }
            UiCommand::Disconnect => {
                self.leave_room().await;
                self.rest = None;
            }
            UiCommand::RefreshRooms => self.refresh_rooms(),
            UiCommand::CreateRoom(name) => {
                let Some(rest) = self.rest.clone() else {
                    return;
                };
                match rest.create_room(name).await {
                    Ok(room) => self.join(room.id, room.name).await,
                    Err(e) => self.ui(AppEvent::Notice(format!("{e:#}"))).await,
                }
            }
            UiCommand::JoinRoom { id, name } => self.join(id, name).await,
            UiCommand::LeaveRoom => {
                self.leave_room().await;
                self.ui(AppEvent::Left).await;
                self.refresh_rooms();
            }
            UiCommand::SendChat(text) => {
                self.send_signal(chattercrab_protocol::ClientMsg::Chat { text });
            }
            UiCommand::SetMuted(muted) => self.media(MediaCommand::SetMuted(muted)).await,
            UiCommand::SetDeafened(deaf) => self.media(MediaCommand::SetDeafened(deaf)).await,
            UiCommand::SetNoiseSuppression(on) => {
                self.media(MediaCommand::SetNoiseSuppression(on)).await;
            }
            UiCommand::SetInputGain(gain) => {
                self.media(MediaCommand::SetInputGain(gain)).await;
            }
            UiCommand::SetPeerVolume { peer, volume } => {
                self.media(MediaCommand::SetPeerVolume { peer, volume })
                    .await;
            }
            UiCommand::ListScreenSources => {
                let events = self.events.clone();
                tokio::spawn(async move {
                    let listed =
                        tokio::task::spawn_blocking(chattercrab_media::list_screen_sources).await;
                    let event = match listed {
                        Ok(Ok(sources)) => AppEvent::ScreenSources(sources),
                        Ok(Err(e)) => AppEvent::Notice(format!("cannot list screens: {e}")),
                        Err(e) => AppEvent::Notice(format!("cannot list screens: {e}")),
                    };
                    let _ = events.send(event).await;
                });
            }
            UiCommand::StartScreenShare {
                source,
                fps,
                scale,
                include_audio,
                high_quality,
                content,
            } => {
                if let Some(room) = self.room.as_mut() {
                    let effects = room.session.request_share(
                        source,
                        fps,
                        scale,
                        include_audio,
                        high_quality,
                        content,
                    );
                    self.apply(effects).await;
                }
            }
            UiCommand::StopScreenShare => {
                if let Some(room) = self.room.as_mut() {
                    let effects = room.session.stop_share();
                    self.apply(effects).await;
                }
            }
            UiCommand::Shutdown => {}
        }
    }

    async fn connect(
        &mut self,
        server_url: &str,
        auth_user: String,
        password: String,
        display_name: DisplayName,
        media_config: MediaConfig,
    ) {
        let rest = match Rest::new(server_url, &auth_user, &password) {
            Ok(rest) => rest,
            Err(e) => return self.ui(AppEvent::ConnectFailed(format!("{e:#}"))).await,
        };
        if let Err(e) = rest.health().await {
            return self.ui(AppEvent::ConnectFailed(format!("{e:#}"))).await;
        }
        let rooms = match rest.rooms().await {
            Ok(rooms) => rooms,
            Err(e) => return self.ui(AppEvent::ConnectFailed(format!("{e:#}"))).await,
        };
        match &self.media {
            Some(_) => {
                let MediaConfig {
                    input_device,
                    output_device,
                    noise_suppression,
                    input_gain,
                } = media_config;
                self.media(MediaCommand::SelectInput(input_device)).await;
                self.media(MediaCommand::SelectOutput(output_device)).await;
                self.media(MediaCommand::SetNoiseSuppression(noise_suppression))
                    .await;
                self.media(MediaCommand::SetInputGain(input_gain)).await;
            }
            None => match MediaEngine::start(media_config) {
                Ok(engine) => self.media = Some(engine),
                Err(e) => {
                    return self
                        .ui(AppEvent::ConnectFailed(format!("cannot start audio: {e}")))
                        .await;
                }
            },
        }
        tracing::info!(server = rest.base(), "connected");
        self.rest = Some(rest);
        self.display_name = Some(display_name);
        let video = match &self.media {
            Some(media) => media.video.clone(),
            None => return,
        };
        self.ui(AppEvent::Connected { rooms, video }).await;
    }

    fn refresh_rooms(&self) {
        let Some(rest) = self.rest.clone() else {
            return;
        };
        let events = self.events.clone();
        tokio::spawn(async move {
            let event = match rest.rooms().await {
                Ok(rooms) => AppEvent::Rooms(rooms),
                Err(e) => AppEvent::Notice(format!("{e:#}")),
            };
            let _ = events.send(event).await;
        });
    }

    async fn ice_servers(&self) -> Option<chattercrab_protocol::IceServers> {
        let rest = self.rest.as_ref()?;
        match rest.ice_servers().await {
            Ok(ice) => Some(ice),
            Err(e) => {
                tracing::warn!(error = %e, "ICE server request failed; using host candidates only");
                None
            }
        }
    }

    async fn join(&mut self, id: RoomId, name: RoomName) {
        // Already in this room (create-and-join, then Join on the lobby row).
        // Leaving first would drop the only member and the server would delete the room.
        if let Some(room) = &self.room
            && room.session.room_id() == &id
        {
            if let Some(joined) = room.session.joined_snapshot() {
                self.ui(AppEvent::Joined(joined)).await;
            }
            return;
        }
        let Some(display_name) = self.display_name.clone() else {
            return;
        };
        self.ui(AppEvent::Entering {
            id: id.clone(),
            name: name.clone(),
        })
        .await;
        self.leave_room().await;
        let ice = self.ice_servers().await;
        self.room = Some(ActiveRoom {
            session: Session::new(id, name, display_name, ice),
            backoff: Backoff::new(),
            conn: None,
            generation: 0,
            retry_at: None,
        });
        self.open_connection();
    }

    fn open_connection(&mut self) {
        let (url, display_name, auth_user, password) = {
            let Some(rest) = &self.rest else { return };
            let Some(room) = &self.room else { return };
            (
                rest.ws_url(room.session.room_id()),
                room.session.display_name().clone(),
                rest.auth_user().to_owned(),
                rest.password().to_owned(),
            )
        };
        let Some(room) = self.room.as_mut() else {
            return;
        };
        self.next_generation += 1;
        room.generation = self.next_generation;
        room.retry_at = None;
        room.conn = Some(signaling::spawn(
            room.generation,
            url,
            display_name,
            auth_user,
            password,
            self.signal_tx.clone(),
        ));
    }

    async fn reconnect(&mut self) {
        let ice = self.ice_servers().await;
        if let Some(room) = self.room.as_mut() {
            room.session.set_ice(ice);
        }
        self.open_connection();
    }

    async fn leave_room(&mut self) {
        if let Some(mut room) = self.room.take() {
            room.conn = None;
            let effects = room.session.on_disconnected();
            self.apply(effects).await;
        }
    }

    fn send_signal(&self, msg: chattercrab_protocol::ClientMsg) {
        match self.room.as_ref().and_then(|r| r.conn.as_ref()) {
            Some(conn) => {
                if !conn.send(msg) {
                    tracing::warn!("signaling queue full; message dropped");
                }
            }
            None => tracing::debug!("not connected; signaling message dropped"),
        }
    }

    async fn on_media_event(&mut self, event: MediaEvent) {
        let effects = match self.room.as_mut() {
            Some(room) => room.session.on_media_event(event),
            None => return,
        };
        self.apply(effects).await;
    }

    async fn on_signal_event(&mut self, event: SignalEvent) {
        let current = self.room.as_ref().map(|r| r.generation);
        match event {
            SignalEvent::Msg(generation, msg) => {
                if current != Some(generation) {
                    return;
                }
                let Some(room) = self.room.as_mut() else {
                    return;
                };
                if matches!(msg, chattercrab_protocol::ServerMsg::Welcome { .. }) {
                    room.backoff.reset();
                }
                let effects = room.session.on_server_msg(msg);
                self.apply(effects).await;
            }
            SignalEvent::Closed { generation, reason } => {
                if current != Some(generation) {
                    return;
                }
                let Some(room) = self.room.as_mut() else {
                    return;
                };
                room.conn = None;
                let effects = room.session.on_disconnected();
                match reason {
                    CloseReason::RoomNotFound => {
                        self.room = None;
                        self.apply(effects).await;
                        self.ui(AppEvent::RoomClosed("The room no longer exists.".into()))
                            .await;
                        self.refresh_rooms();
                    }
                    CloseReason::Failed(why) => {
                        tracing::warn!(reason = %why, "signaling connection lost");
                        let delay = room.backoff.next_delay();
                        let attempt = room.backoff.attempt();
                        room.retry_at = Some(Instant::now() + delay);
                        self.apply(effects).await;
                        self.ui(AppEvent::Reconnecting {
                            attempt,
                            delay_secs: delay.as_secs(),
                        })
                        .await;
                    }
                }
            }
        }
    }

    async fn apply(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Media(cmd) => self.media(cmd).await,
                Effect::Ui(event) => self.ui(event).await,
                Effect::Send(msg) => self.send_signal(msg),
                Effect::Fatal(message) => {
                    // Leave without re-running this loop on the same session.
                    if let Some(mut room) = self.room.take() {
                        room.conn = None;
                        for cmd in room.session.on_disconnected() {
                            if let Effect::Media(cmd) = cmd {
                                self.media(cmd).await;
                            }
                        }
                    }
                    self.ui(AppEvent::RoomClosed(message)).await;
                    self.refresh_rooms();
                    return;
                }
            }
        }
    }
}

async fn next_media_event(media: &mut Option<MediaEngine>) -> Option<MediaEvent> {
    match media {
        Some(media) => media.events.recv().await,
        None => std::future::pending().await,
    }
}

async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}
