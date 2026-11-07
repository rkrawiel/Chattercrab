//! The media engine task: turns [`MediaCommand`]s into actions on the audio devices, the
//! mixer, the screen publisher and the [`PeerManager`], and reports [`MediaEvent`]s.
//!
//! Runs as one Tokio task. Blocking work (opening devices, joining codec threads) goes
//! through `spawn_blocking`; codec work runs on dedicated threads owned by the audio,
//! mixer and screen modules.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use chattercrab_protocol::PeerId;
use ringbuf::HeapProd;
use ringbuf::traits::Producer;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};

use crate::audio::{AudioInput, AudioOutput, OpusFrame, ShareMix, SystemCapture};
use crate::clamp_input_gain;
use crate::mixer::{MixerControl, MixerHandle, PeerAudioControls, PeerSlot, RtpAudioPacket};
use crate::peer::{InternalEvent, PeerEvent, PeerManager, PeerSettings, SendTargets};
use crate::screen::{EncodedVideo, ScreenPublisher};
use crate::{MediaCommand, MediaConfig, MediaDiagnostics, MediaEvent, VideoFrameQueue};

/// Interval of [`MediaEvent::Levels`].
const LEVELS_INTERVAL: Duration = Duration::from_millis(100);
/// Opus frame duration.
const AUDIO_FRAME: Duration = Duration::from_millis(20);

/// Notifications from tasks spawned by the engine.
enum EngineNote {
    /// The local screen publisher stopped on its own (capture failure).
    LocalScreenEnded,
    /// A remote video track ended.
    RemoteVideoEnded(PeerId),
}

struct Engine {
    events: mpsc::Sender<MediaEvent>,
    video: VideoFrameQueue,
    peers: PeerManager,
    notes_tx: mpsc::Sender<EngineNote>,

    muted: Arc<AtomicBool>,
    denoise: Arc<AtomicBool>,
    /// Microphone gain as `f32` bits.
    gain: Arc<AtomicU32>,
    /// System audio mixed into the outgoing voice while a share includes it.
    share: Arc<ShareMix>,
    system_audio: Option<SystemCapture>,
    local_level: Arc<AtomicU32>,
    input_device: Option<String>,
    output_device: Option<String>,
    /// Synthetic input tone (diagnostics); replaces the microphone when set.
    test_tone: Option<f32>,
    input: Option<AudioInput>,
    output: Option<AudioOutput>,
    audio_pump: Option<JoinHandle<()>>,
    mixer: MixerHandle,

    peer_audio: HashMap<PeerId, PeerAudioControls>,
    volumes: HashMap<PeerId, f32>,
    readers: HashMap<PeerId, Vec<JoinHandle<()>>>,

    screen: Option<ScreenPublisher>,
    video_pump: Option<JoinHandle<()>>,
}

/// Runs the engine until `Shutdown` or until the command channel closes.
pub(crate) async fn run(
    config: MediaConfig,
    diagnostics: MediaDiagnostics,
    mut commands: mpsc::Receiver<MediaCommand>,
    events: mpsc::Sender<MediaEvent>,
    video: VideoFrameQueue,
) {
    let mixer = match MixerHandle::spawn(None) {
        Ok(m) => m,
        Err(e) => {
            let _ = events.send(MediaEvent::Error(e.to_string())).await;
            return;
        }
    };
    let (peers, mut internal) = PeerManager::new(PeerSettings::default());
    let (notes_tx, mut notes) = mpsc::channel(16);
    let mut engine = Engine {
        events,
        video,
        peers,
        notes_tx,
        muted: Arc::new(AtomicBool::new(false)),
        denoise: Arc::new(AtomicBool::new(config.noise_suppression)),
        gain: Arc::new(AtomicU32::new(
            clamp_input_gain(config.input_gain).to_bits(),
        )),
        share: ShareMix::new(),
        system_audio: None,
        local_level: Arc::new(AtomicU32::new(0)),
        input_device: config.input_device,
        output_device: config.output_device,
        test_tone: diagnostics.test_tone,
        input: None,
        output: None,
        audio_pump: None,
        mixer,
        peer_audio: HashMap::new(),
        volumes: HashMap::new(),
        readers: HashMap::new(),
        screen: None,
        video_pump: None,
    };
    engine.open_input().await;
    engine.open_output().await;

    let mut levels = tokio::time::interval(LEVELS_INTERVAL);
    levels.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            cmd = commands.recv() => match cmd {
                None | Some(MediaCommand::Shutdown) => break,
                Some(cmd) => engine.handle_command(cmd).await,
            },
            Some(ev) = internal.recv() => engine.handle_internal(ev).await,
            Some(note) = notes.recv() => engine.handle_note(note).await,
            _ = levels.tick() => engine.emit_levels(),
        }
        engine.forward_peer_events().await;
    }
    engine.shutdown().await;
}

impl Engine {
    async fn emit(&self, ev: MediaEvent) {
        let _ = self.events.send(ev).await;
    }

    async fn error(&self, msg: impl Into<String>) {
        let msg = msg.into();
        tracing::warn!("{msg}");
        self.emit(MediaEvent::Error(msg)).await;
    }

    async fn open_input(&mut self) {
        if let Some(pump) = self.audio_pump.take() {
            pump.abort();
        }
        if let Some(old) = self.input.take() {
            // Dropping joins the encoder thread.
            let _ = tokio::task::spawn_blocking(move || drop(old)).await;
        }
        let device = self.input_device.clone();
        let muted = self.muted.clone();
        let denoise = self.denoise.clone();
        let gain = self.gain.clone();
        let share = self.share.clone();
        let level = self.local_level.clone();
        let tone = self.test_tone;
        let opened = tokio::task::spawn_blocking(move || match tone {
            Some(hz) => AudioInput::start_tone(hz, muted, denoise, gain, share, level),
            None => AudioInput::start(device.as_deref(), muted, denoise, gain, share, level),
        })
        .await;
        match opened {
            Ok(Ok(mut input)) => {
                let (_, dummy) = mpsc::channel(1);
                let frames = std::mem::replace(&mut input.frames, dummy);
                self.audio_pump =
                    Some(tokio::spawn(audio_pump(frames, self.peers.audio_targets())));
                self.input = Some(input);
            }
            Ok(Err(e)) => self.error(format!("microphone unavailable: {e}")).await,
            Err(e) => self.error(format!("microphone task failed: {e}")).await,
        }
    }

    async fn open_output(&mut self) {
        let device = self.output_device.clone();
        let opened =
            tokio::task::spawn_blocking(move || AudioOutput::start(device.as_deref())).await;
        match opened {
            Ok(Ok((output, sink))) => {
                if !self
                    .mixer
                    .send(MixerControl::SetOutput(Some(Box::new(sink))))
                {
                    self.error("mixer did not accept the output device").await;
                }
                if let Some(old) = self.output.replace(output) {
                    let _ = tokio::task::spawn_blocking(move || drop(old)).await;
                }
            }
            Ok(Err(e)) => self.error(format!("speaker unavailable: {e}")).await,
            Err(e) => self.error(format!("speaker task failed: {e}")).await,
        }
    }

    async fn handle_command(&mut self, cmd: MediaCommand) {
        match cmd {
            MediaCommand::Configure { me, ice } => self.peers.configure(me, ice.as_ref()),
            MediaCommand::AddPeer { peer, initiate } => {
                if let Err(e) = self.peers.add_peer(peer, initiate).await {
                    self.error(format!("connect to {peer}: {e}")).await;
                }
            }
            MediaCommand::RemovePeer { peer } => self.remove_peer(peer).await,
            MediaCommand::RemoveAllPeers => {
                for peer in self.peers.peer_ids() {
                    self.remove_peer(peer).await;
                }
                self.peers.remove_all().await;
                if self.screen.is_some() {
                    self.stop_screen().await;
                }
            }
            MediaCommand::RemoteOffer { from, sdp } => {
                if let Err(e) = self.peers.remote_offer(from, sdp).await {
                    self.error(format!("offer from {from}: {e}")).await;
                }
            }
            MediaCommand::RemoteAnswer { from, sdp } => {
                if let Err(e) = self.peers.remote_answer(from, sdp).await {
                    self.error(format!("answer from {from}: {e}")).await;
                }
            }
            MediaCommand::RemoteIce {
                from,
                candidate,
                sdp_mid,
                sdp_mline_index,
            } => {
                self.peers
                    .remote_ice(from, candidate, sdp_mid, sdp_mline_index)
                    .await;
            }
            MediaCommand::SetMuted(m) => self.muted.store(m, Ordering::Relaxed),
            MediaCommand::SetDeafened(d) => self.mixer.deafened.store(d, Ordering::Relaxed),
            MediaCommand::SetNoiseSuppression(on) => self.denoise.store(on, Ordering::Relaxed),
            MediaCommand::SetInputGain(gain) => {
                self.gain
                    .store(clamp_input_gain(gain).to_bits(), Ordering::Relaxed);
            }
            MediaCommand::SetPeerVolume { peer, volume } => {
                self.volumes.insert(peer, volume);
                if let Some(c) = self.peer_audio.get(&peer) {
                    c.set_volume(volume);
                }
            }
            MediaCommand::SelectInput(name) => {
                self.input_device = name;
                self.open_input().await;
            }
            MediaCommand::SelectOutput(name) => {
                self.output_device = name;
                self.open_output().await;
            }
            MediaCommand::StartScreenShare {
                source,
                fps,
                scale,
                include_audio,
                high_quality,
                content,
            } => {
                self.start_screen(source, fps, scale, include_audio, high_quality, content)
                    .await;
            }
            MediaCommand::StopScreenShare => self.stop_screen().await,
            // Handled by the run loop.
            MediaCommand::Shutdown => {}
        }
    }

    async fn handle_internal(&mut self, ev: InternalEvent) {
        self.peers.handle_internal(ev).await;
    }

    async fn handle_note(&mut self, note: EngineNote) {
        match note {
            EngineNote::LocalScreenEnded => {
                if self.screen.is_some() {
                    self.error("screen capture stopped").await;
                    self.stop_screen().await;
                }
            }
            EngineNote::RemoteVideoEnded(peer) => {
                self.emit(MediaEvent::RemoteScreenEnded { peer }).await;
            }
        }
    }

    async fn forward_peer_events(&mut self) {
        let events: Vec<PeerEvent> = self.peers.drain_events().collect();
        for ev in events {
            match ev {
                PeerEvent::Signal(signal) => self.emit(MediaEvent::Signal(signal)).await,
                PeerEvent::State { peer, state } => {
                    self.emit(MediaEvent::PeerState { peer, state }).await;
                }
                PeerEvent::RemoteAudio { peer, track } => {
                    self.attach_remote_audio(peer, track).await
                }
                PeerEvent::RemoteVideo { peer, track } => {
                    let queue = self.video.clone();
                    let notes = self.notes_tx.clone();
                    let reader = tokio::spawn(async move {
                        crate::screen::receive::run_receiver(track, queue).await;
                        let _ = notes.send(EngineNote::RemoteVideoEnded(peer)).await;
                    });
                    self.readers.entry(peer).or_default().push(reader);
                    self.emit(MediaEvent::RemoteScreenStarted { peer }).await;
                }
            }
        }
    }

    async fn attach_remote_audio(&mut self, peer: PeerId, track: Arc<dyn TrackRemote>) {
        let (slot, producer, controls) = match PeerSlot::new(peer) {
            Ok(parts) => parts,
            Err(e) => {
                self.error(format!("audio decoder for {peer}: {e}")).await;
                return;
            }
        };
        if let Some(v) = self.volumes.get(&peer) {
            controls.set_volume(*v);
        }
        if !self.mixer.send(MixerControl::AddPeer(slot)) {
            self.error("mixer queue full").await;
            return;
        }
        self.peer_audio.insert(peer, controls);
        let reader = tokio::spawn(audio_reader(track, producer));
        self.readers.entry(peer).or_default().push(reader);
    }

    async fn remove_peer(&mut self, peer: PeerId) {
        if let Some(tasks) = self.readers.remove(&peer) {
            for t in tasks {
                t.abort();
            }
        }
        self.peer_audio.remove(&peer);
        let _ = self.mixer.send(MixerControl::RemovePeer(peer));
        self.peers.remove_peer(peer).await;
    }

    async fn start_system_audio(&mut self) {
        self.stop_system_audio().await;
        let mix = self.share.clone();
        match tokio::task::spawn_blocking(move || SystemCapture::start(mix)).await {
            Ok(Ok(capture)) => self.system_audio = Some(capture),
            Ok(Err(e)) => self.error(format!("screen audio unavailable: {e}")).await,
            Err(e) => self.error(format!("screen audio task failed: {e}")).await,
        }
    }

    async fn stop_system_audio(&mut self) {
        if let Some(capture) = self.system_audio.take() {
            let _ = tokio::task::spawn_blocking(move || drop(capture)).await;
        }
    }

    async fn start_screen(
        &mut self,
        source: crate::ScreenSource,
        fps: u32,
        scale: crate::ScreenScale,
        include_audio: bool,
        high_quality: bool,
        content: crate::ScreenContent,
    ) {
        if self.screen.is_some() {
            self.stop_screen().await;
        }
        if include_audio {
            self.start_system_audio().await;
        }
        let fps = crate::screen::clamp_fps(fps, high_quality);
        let keyframe = self.peers.keyframe_flag();
        let started = tokio::task::spawn_blocking(move || {
            ScreenPublisher::start(&source, fps, scale, keyframe, high_quality, content)
        })
        .await;
        match started {
            Ok(Ok(mut publisher)) => {
                let (_, dummy) = mpsc::channel(1);
                let frames = std::mem::replace(&mut publisher.frames, dummy);
                self.screen = Some(publisher);
                self.peers.set_screen_active(true, high_quality, fps).await;
                let notes = self.notes_tx.clone();
                let targets = self.peers.video_targets();
                self.video_pump = Some(tokio::spawn(async move {
                    video_pump(frames, targets).await;
                    let _ = notes.send(EngineNote::LocalScreenEnded).await;
                }));
                self.emit(MediaEvent::LocalScreenStarted).await;
            }
            Ok(Err(e)) => {
                self.stop_system_audio().await;
                self.error(format!("screen share failed: {e}")).await;
                self.emit(MediaEvent::LocalScreenStopped).await;
            }
            Err(e) => {
                self.stop_system_audio().await;
                self.error(format!("screen share task failed: {e}")).await;
                self.emit(MediaEvent::LocalScreenStopped).await;
            }
        }
    }

    async fn stop_screen(&mut self) {
        self.stop_system_audio().await;
        if let Some(pump) = self.video_pump.take() {
            pump.abort();
        }
        if let Some(publisher) = self.screen.take() {
            let _ = tokio::task::spawn_blocking(move || drop(publisher)).await;
            self.peers.set_screen_active(false, false, 0).await;
            self.emit(MediaEvent::LocalScreenStopped).await;
        }
    }

    fn emit_levels(&self) {
        if self.peers.me().is_none() {
            return;
        }
        let local = if self.muted.load(Ordering::Relaxed) {
            0.0
        } else {
            f32::from_bits(self.local_level.load(Ordering::Relaxed))
        };
        let peers = self
            .peer_audio
            .iter()
            .map(|(id, c)| (*id, c.level()))
            .collect();
        // Levels are advisory: drop them rather than wait if the UI is behind.
        let _ = self.events.try_send(MediaEvent::Levels { local, peers });
    }

    async fn shutdown(mut self) {
        for peer in self.peers.peer_ids() {
            self.remove_peer(peer).await;
        }
        self.peers.remove_all().await;
        self.stop_screen().await;
        if let Some(p) = self.audio_pump.take() {
            p.abort();
        }
        let input = self.input.take();
        let output = self.output.take();
        let mixer = self.mixer;
        let _ = tokio::task::spawn_blocking(move || {
            drop(input);
            drop(mixer);
            drop(output);
        })
        .await;
        tracing::info!("media engine stopped");
    }
}

/// Writes every encoded audio frame to all audio send targets.
async fn audio_pump(
    mut frames: mpsc::Receiver<OpusFrame>,
    mut targets: watch::Receiver<SendTargets>,
) {
    while let Some(frame) = frames.recv().await {
        let snapshot = targets.borrow_and_update().clone();
        if snapshot.is_empty() {
            continue;
        }
        let data = Bytes::copy_from_slice(frame.bytes());
        for t in snapshot.iter() {
            let _ = t.write(data.clone(), AUDIO_FRAME).await;
        }
    }
}

/// Writes every encoded screen frame to all video send targets.
async fn video_pump(
    mut frames: mpsc::Receiver<EncodedVideo>,
    mut targets: watch::Receiver<SendTargets>,
) {
    while let Some(frame) = frames.recv().await {
        let snapshot = targets.borrow_and_update().clone();
        for t in snapshot.iter() {
            let _ = t.write(frame.data.clone(), frame.duration).await;
        }
    }
}

/// Moves RTP payloads of one remote audio track into the mixer's ring for that peer.
async fn audio_reader(track: Arc<dyn TrackRemote>, mut producer: HeapProd<RtpAudioPacket>) {
    let mut dropped = 0u64;
    while let Some(ev) = track.poll().await {
        match ev {
            TrackRemoteEvent::OnRtpPacket(pkt) => {
                let Some(p) = RtpAudioPacket::new(
                    pkt.header.sequence_number,
                    pkt.header.timestamp,
                    &pkt.payload,
                    Instant::now(),
                ) else {
                    continue;
                };
                if producer.try_push(p).is_err() {
                    dropped += 1;
                    if dropped.is_power_of_two() {
                        tracing::debug!(dropped, "mixer ring full, audio packet dropped");
                    }
                }
            }
            TrackRemoteEvent::OnEnded => break,
            _ => {}
        }
    }
}
