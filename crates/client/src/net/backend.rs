//! The backend task: owns the REST client, the media engine and the signaling
//! connection, and applies [`Effect`]s produced by the pure [`Session`].

use std::time::Duration;

use chattercrab_media::{MediaCommand, MediaConfig, MediaEngine, MediaEvent};
use chattercrab_protocol::{DisplayName, IceServers, RoomId, RoomName, RoomSummary};
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::backoff::Backoff;
use super::rest::Rest;
use super::session::{Effect, Session};
use super::signaling::{self, CloseReason, SignalEvent, SignalHandle};
use super::{AppEvent, UiCommand};

/// Capacity of the signaling task to backend channel.
const SIGNAL_EVENT_CAPACITY: usize = 256;
/// Capacity of the channel carrying finished REST requests back to the backend loop.
const REST_DONE_CAPACITY: usize = 16;

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

/// A connect request whose REST calls are in flight.
struct PendingConnect {
    request: u64,
    display_name: DisplayName,
    media: MediaConfig,
}

/// A REST request spawned off the backend loop finished. `request` ties the answer to
/// its request, so answers that arrive after Disconnect, Leave or a newer request are
/// dropped.
enum RestDone {
    /// Health check, room list and ICE servers of a connect request.
    Connect {
        request: u64,
        result: Result<(Rest, Vec<RoomSummary>, Option<IceServers>), String>,
    },
    /// Room creation.
    CreateRoom {
        request: u64,
        result: Result<RoomSummary, String>,
    },
    /// ICE servers fetched again because the connect-time request failed.
    Ice {
        request: u64,
        ice: Option<IceServers>,
    },
}

/// Backend state. Lives on the runtime thread.
pub struct Backend {
    commands: mpsc::Receiver<UiCommand>,
    events: mpsc::Sender<AppEvent>,
    signal_tx: mpsc::Sender<SignalEvent>,
    signal_rx: mpsc::Receiver<SignalEvent>,
    rest_done_tx: mpsc::Sender<RestDone>,
    rest_done_rx: mpsc::Receiver<RestDone>,
    rest: Option<Rest>,
    /// Request number of the connect that produced `rest`.
    rest_request: u64,
    /// ICE servers fetched on connect, reused by every join and reconnect.
    ice: Option<IceServers>,
    fetching_ice: bool,
    connecting: Option<PendingConnect>,
    /// Request number of the room creation in flight.
    creating: Option<u64>,
    last_request: u64,
    media: Option<MediaEngine>,
    display_name: Option<DisplayName>,
    room: Option<ActiveRoom>,
    next_generation: u64,
}

impl Backend {
    /// Creates a backend bound to the UI channels.
    pub fn new(commands: mpsc::Receiver<UiCommand>, events: mpsc::Sender<AppEvent>) -> Self {
        let (signal_tx, signal_rx) = mpsc::channel(SIGNAL_EVENT_CAPACITY);
        let (rest_done_tx, rest_done_rx) = mpsc::channel(REST_DONE_CAPACITY);
        Self {
            commands,
            events,
            signal_tx,
            signal_rx,
            rest_done_tx,
            rest_done_rx,
            rest: None,
            rest_request: 0,
            ice: None,
            fetching_ice: false,
            connecting: None,
            creating: None,
            last_request: 0,
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
                Some(done) = self.rest_done_rx.recv() => self.on_rest_done(done).await,
                () = sleep_until_opt(retry_at) => self.reconnect(),
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
                self.connecting = None;
                self.creating = None;
                self.ice = None;
                self.fetching_ice = false;
                self.leave_room().await;
                self.rest = None;
            }
            UiCommand::RefreshRooms => self.refresh_rooms(),
            UiCommand::CreateRoom(name) => {
                let Some(rest) = self.rest.clone() else {
                    return;
                };
                let request = self.next_request();
                self.creating = Some(request);
                let done = self.rest_done_tx.clone();
                tokio::spawn(async move {
                    let result = rest.create_room(name).await.map_err(|e| format!("{e:#}"));
                    let _ = done.send(RestDone::CreateRoom { request, result }).await;
                });
            }
            UiCommand::JoinRoom { id, name } => {
                self.creating = None;
                self.join(id, name).await;
            }
            UiCommand::LeaveRoom => {
                self.creating = None;
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
            // The engine reopens the device in a room and only records it otherwise.
            UiCommand::SelectInput(name) => self.media(MediaCommand::SelectInput(name)).await,
            UiCommand::SelectOutput(name) => self.media(MediaCommand::SelectOutput(name)).await,
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

    fn next_request(&mut self) -> u64 {
        self.last_request += 1;
        self.last_request
    }

    /// Starts the connect requests; [`Self::connected`] finishes once they succeed.
    async fn connect(
        &mut self,
        server_url: &str,
        auth_user: String,
        password: String,
        display_name: DisplayName,
        media: MediaConfig,
    ) {
        let rest = match Rest::new(server_url, &auth_user, &password) {
            Ok(rest) => rest,
            Err(e) => return self.ui(AppEvent::ConnectFailed(format!("{e:#}"))).await,
        };
        let request = self.next_request();
        self.connecting = Some(PendingConnect {
            request,
            display_name,
            media,
        });
        let done = self.rest_done_tx.clone();
        tokio::spawn(async move {
            let result = connect_requests(rest).await;
            let _ = done.send(RestDone::Connect { request, result }).await;
        });
    }

    async fn on_rest_done(&mut self, done: RestDone) {
        match done {
            RestDone::Connect { request, result } => {
                let Some(pending) = self.connecting.take_if(|p| p.request == request) else {
                    return;
                };
                match result {
                    Ok((rest, rooms, ice)) => self.connected(pending, rest, rooms, ice).await,
                    Err(e) => self.ui(AppEvent::ConnectFailed(e)).await,
                }
            }
            RestDone::CreateRoom { request, result } => {
                if self.creating != Some(request) {
                    return;
                }
                self.creating = None;
                match result {
                    Ok(room) => self.join(room.id, room.name).await,
                    Err(e) => self.ui(AppEvent::Notice(e)).await,
                }
            }
            RestDone::Ice { request, ice } => {
                if request != self.rest_request || self.rest.is_none() {
                    return;
                }
                self.fetching_ice = false;
                if let Some(ice) = ice {
                    if let Some(room) = self.room.as_mut() {
                        room.session.set_ice(Some(ice.clone()));
                    }
                    self.ice = Some(ice);
                }
            }
        }
    }

    async fn connected(
        &mut self,
        pending: PendingConnect,
        rest: Rest,
        rooms: Vec<RoomSummary>,
        ice: Option<IceServers>,
    ) {
        let PendingConnect {
            request,
            display_name,
            media: media_config,
        } = pending;
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
        self.rest_request = request;
        self.ice = ice;
        self.fetching_ice = false;
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

    /// The cached ICE servers. When the connect-time request failed, fetches them in
    /// the background for later joins.
    fn cached_ice(&mut self) -> Option<IceServers> {
        if self.ice.is_none()
            && !self.fetching_ice
            && let Some(rest) = self.rest.clone()
        {
            self.fetching_ice = true;
            let request = self.rest_request;
            let done = self.rest_done_tx.clone();
            tokio::spawn(async move {
                let ice = fetch_ice(&rest).await;
                let _ = done.send(RestDone::Ice { request, ice }).await;
            });
        }
        self.ice.clone()
    }

    async fn join(&mut self, id: RoomId, name: RoomName) {
        // Already in this room (create-and-join, then Join on the lobby row).
        // Leaving first would drop the only member and the server would delete the room.
        if let Some(room) = self.room.as_mut()
            && room.session.room_id() == &id
        {
            if let Some(joined) = room.session.rejoin_snapshot() {
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
        let ice = self.cached_ice();
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

    fn reconnect(&mut self) {
        let ice = self.cached_ice();
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
                    CloseReason::RoomNotFound | CloseReason::AuthFailed => {
                        let why = if reason == CloseReason::AuthFailed {
                            "Authentication failed."
                        } else {
                            "The room no longer exists."
                        };
                        self.room = None;
                        self.apply(effects).await;
                        self.ui(AppEvent::RoomClosed(why.into())).await;
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

/// Health check, then the room list and ICE servers in parallel.
async fn connect_requests(
    rest: Rest,
) -> Result<(Rest, Vec<RoomSummary>, Option<IceServers>), String> {
    rest.health().await.map_err(|e| format!("{e:#}"))?;
    let (rooms, ice) = tokio::join!(rest.rooms(), fetch_ice(&rest));
    let rooms = rooms.map_err(|e| format!("{e:#}"))?;
    Ok((rest, rooms, ice))
}

/// `None` (host candidates only) when the request fails.
async fn fetch_ice(rest: &Rest) -> Option<IceServers> {
    match rest.ice_servers().await {
        Ok(ice) => Some(ice),
        Err(e) => {
            tracing::warn!(error = %e, "ICE server request failed; using host candidates only");
            None
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
