//! WebRTC peer management: one peer connection per remote peer, full mesh.
//!
//! [`PeerManager`] owns every connection, runs perfect negotiation (see
//! [`negotiation`]) and exposes the local send tracks as [`SendTarget`] snapshots that the
//! audio and video pumps write into. It is driven by its owner (the media engine task):
//!
//! * signaling input via [`PeerManager::remote_offer`], [`PeerManager::remote_answer`],
//!   [`PeerManager::remote_ice`];
//! * connection callbacks arrive as [`InternalEvent`]s on the receiver returned by
//!   [`PeerManager::new`] and must be fed back through [`PeerManager::handle_internal`];
//! * outputs (signaling messages, state changes, remote tracks) are queued and collected
//!   with [`PeerManager::drain_events`].
//!
//! Every method runs to completion before the next is called, which is what makes the
//! negotiation logic safe without additional locking.

pub mod negotiation;
mod rtcp;

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use chattercrab_protocol::{IceServers, PeerId};
use rtc::ice::mdns::MulticastDnsMode;
use rtc::ice::network_type::NetworkType;
use rtc::interceptor::Slot;
use rtc::media::Sample;
use rtc::peer_connection::configuration::media_engine::{MIME_TYPE_H264, MIME_TYPE_OPUS};
use rtc::rtp_transceiver::rtp_sender::{
    RTCPFeedback, RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters,
    RTCRtpEncodingParameters, RtpCodecKind,
};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use webrtc::media_stream::MediaStreamTrack;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_local::{TrackLocal, TrackLocalEvent};
use webrtc::media_stream::track_remote::TrackRemote;
use webrtc::peer_connection::{
    MediaEngine as RtcMediaEngine, PeerConnection, PeerConnectionBuilder,
    PeerConnectionEventHandler, RTCBundlePolicy, RTCConfigurationBuilder, RTCIceCandidateInit,
    RTCIceServer, RTCPeerConnectionIceEvent, RTCPeerConnectionState, RTCRtcpMuxPolicy,
    RTCSessionDescription, Registry, SettingEngineBuilder, register_default_interceptors,
};
use webrtc::rtp_transceiver::RtpSender;

use self::negotiation::{Negotiator, OfferDecision};
use crate::{MediaError, OutgoingSignal, PeerConnectionState};

/// RTP payload type registered for Opus.
const OPUS_PAYLOAD_TYPE: u8 = 111;
/// RTP payload type registered for H.264 constrained baseline.
const H264_PAYLOAD_TYPE: u8 = 102;
/// RTP payload type for H.264 High, used by the high-quality share.
const H264_HIGH_PAYLOAD_TYPE: u8 = 104;
/// H.264 format parameters (constrained baseline, level 5.2, non-interleaved).
/// Level 5.2 covers 4K60. Smaller shares fit inside it.
const H264_FMTP: &str = "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e034";
/// H.264 High, level 5.2, non-interleaved. Level 5.2 covers 1440p120.
/// fmtp matching ignores the level byte, so a level 5.1 bitstream still pairs
/// with this. NACK, PLI, FIR and transport-cc are enabled; the shared encoder
/// stays at a fixed bitrate and does not follow per-viewer congestion.
const H264_HIGH_FMTP: &str =
    "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640034";
/// Upper bound advertised on a high-quality encoding, matching the encoder ceiling.
const H264_HIGH_MAX_BITRATE: u32 = 120_000_000;
/// Opus format parameters.
const OPUS_FMTP: &str = "minptime=10;useinbandfec=1";
/// ICE: no activity for this long marks the connection disconnected.
const ICE_DISCONNECTED_TIMEOUT: Duration = Duration::from_secs(5);
/// ICE: disconnected for this long marks it failed.
const ICE_FAILED_TIMEOUT: Duration = Duration::from_secs(15);
/// ICE keepalive interval.
const ICE_KEEPALIVE: Duration = Duration::from_secs(2);
/// ICE restarts attempted per peer after a failure (impolite side only).
const MAX_ICE_RESTARTS: u8 = 2;
/// Remote candidates buffered before the remote description is known.
const MAX_PENDING_ICE: usize = 64;
/// Capacity of the internal callback channel.
const INTERNAL_CHANNEL_CAPACITY: usize = 1024;

/// Socket and candidate settings for new peer connections.
#[derive(Debug, Clone)]
pub struct PeerSettings {
    /// Local UDP bind addresses (wildcards expand to every non-loopback interface).
    pub udp_addrs: Vec<String>,
    /// Local TCP bind addresses for passive ICE-TCP candidates.
    pub tcp_addrs: Vec<String>,
    /// Also gather loopback candidates (useful for same-host tests only).
    pub include_loopback: bool,
}

impl Default for PeerSettings {
    fn default() -> Self {
        Self {
            udp_addrs: vec!["0.0.0.0:0".into()],
            tcp_addrs: vec!["0.0.0.0:0".into()],
            include_loopback: false,
        }
    }
}

/// Output of the peer manager, collected with [`PeerManager::drain_events`].
pub enum PeerEvent {
    /// Send this to the signaling server.
    Signal(OutgoingSignal),
    /// Connection state of a peer changed.
    State {
        /// Remote peer.
        peer: PeerId,
        /// New state.
        state: PeerConnectionState,
    },
    /// A remote audio track started.
    RemoteAudio {
        /// Remote peer.
        peer: PeerId,
        /// The track; read RTP with `poll()`.
        track: Arc<dyn TrackRemote>,
    },
    /// A remote video (screen share) track started.
    RemoteVideo {
        /// Remote peer.
        peer: PeerId,
        /// The track; read RTP with `poll()`.
        track: Arc<dyn TrackRemote>,
    },
}

impl std::fmt::Debug for PeerEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Signal(s) => f.debug_tuple("Signal").field(s).finish(),
            Self::State { peer, state } => f
                .debug_struct("State")
                .field("peer", peer)
                .field("state", state)
                .finish(),
            Self::RemoteAudio { peer, .. } => {
                f.debug_struct("RemoteAudio").field("peer", peer).finish()
            }
            Self::RemoteVideo { peer, .. } => {
                f.debug_struct("RemoteVideo").field("peer", peer).finish()
            }
        }
    }
}

/// Callback from a peer connection, tagged with the connection generation so events
/// from a replaced connection are ignored.
pub struct InternalEvent {
    peer: PeerId,
    generation: u64,
    kind: InternalKind,
}

enum InternalKind {
    Candidate(RTCIceCandidateInit),
    State(RTCPeerConnectionState),
    Track(Arc<dyn TrackRemote>),
    NegotiationNeeded,
}

/// A local track bound to one peer connection, written by the audio or video pump.
pub struct SendTarget {
    /// Remote peer this track goes to.
    pub peer: PeerId,
    track: Arc<TrackLocalStaticSample>,
    sender: Arc<dyn RtpSender>,
    ssrc: u32,
    /// Negotiated payload type, `u16::MAX` until known.
    payload_type: AtomicU16,
}

impl SendTarget {
    /// Packetizes and sends one encoded sample. Fails (harmlessly) until negotiation has
    /// bound the track.
    pub async fn write(&self, data: Bytes, duration: Duration) -> Result<(), MediaError> {
        let pt = match self.payload_type.load(Ordering::Relaxed) {
            u16::MAX => {
                let params = self
                    .sender
                    .get_parameters()
                    .await
                    .map_err(|e| MediaError::WebRtc(e.to_string()))?;
                let Some(codec) = params.rtp_parameters.codecs.first() else {
                    return Err(MediaError::WebRtc("track not negotiated yet".into()));
                };
                self.payload_type
                    .store(u16::from(codec.payload_type), Ordering::Relaxed);
                codec.payload_type
            }
            pt => pt as u8,
        };
        let sample = Sample {
            data,
            duration,
            ..Sample::new(Instant::now())
        };
        let result = self
            .track
            .sample_writer(self.ssrc, pt)
            .write_sample(&sample)
            .await;
        if result.is_err() {
            // Re-resolve the payload type next time in case negotiation changed it.
            self.payload_type.store(u16::MAX, Ordering::Relaxed);
        }
        result.map_err(|e| MediaError::WebRtc(e.to_string()))
    }
}

/// Snapshot of send targets, swapped atomically when connections change.
pub type SendTargets = Arc<[Arc<SendTarget>]>;

struct LocalSender {
    target: Arc<SendTarget>,
    rtcp_task: Option<JoinHandle<()>>,
}

impl Drop for LocalSender {
    fn drop(&mut self) {
        if let Some(t) = self.rtcp_task.take() {
            t.abort();
        }
    }
}

struct PeerEntry {
    generation: u64,
    pc: Arc<dyn PeerConnection>,
    neg: Negotiator,
    /// Tracks may be attached: true for the initiator from the start, for the
    /// responder once the first remote offer has been applied.
    ready: bool,
    remote_set: bool,
    pending_ice: Vec<RTCIceCandidateInit>,
    audio: Option<LocalSender>,
    video: Option<LocalSender>,
    restarts: u8,
    /// DTLS-SRTP is up. Send targets are only published for connected peers, so the
    /// audio pump never writes RTP before the SRTP context exists.
    connected: bool,
}

struct Handler {
    peer: PeerId,
    generation: u64,
    tx: mpsc::Sender<InternalEvent>,
}

impl Handler {
    fn emit(&self, kind: InternalKind) {
        let ev = InternalEvent {
            peer: self.peer,
            generation: self.generation,
            kind,
        };
        if self.tx.try_send(ev).is_err() {
            tracing::warn!(peer = %self.peer, "peer event queue full, event dropped");
        }
    }
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_negotiation_needed(&self) {
        self.emit(InternalKind::NegotiationNeeded);
    }

    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        match event.candidate.to_json() {
            Ok(init) => self.emit(InternalKind::Candidate(init)),
            Err(e) => tracing::debug!("skip unserializable candidate: {e}"),
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        self.emit(InternalKind::State(state));
    }

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        self.emit(InternalKind::Track(track));
    }
}

/// Owns all peer connections. See the module documentation.
pub struct PeerManager {
    settings: PeerSettings,
    me: Option<PeerId>,
    ice_servers: Vec<RTCIceServer>,
    peers: HashMap<PeerId, PeerEntry>,
    next_generation: u64,
    internal_tx: mpsc::Sender<InternalEvent>,
    events: VecDeque<PeerEvent>,
    screen_active: bool,
    screen_high_quality: bool,
    screen_fps: u32,
    audio_targets: watch::Sender<SendTargets>,
    video_targets: watch::Sender<SendTargets>,
    keyframe_requested: Arc<AtomicBool>,
}

fn empty_targets() -> SendTargets {
    Arc::from(Vec::<Arc<SendTarget>>::new())
}

fn map_state(state: RTCPeerConnectionState) -> PeerConnectionState {
    match state {
        RTCPeerConnectionState::Connected => PeerConnectionState::Connected,
        RTCPeerConnectionState::Disconnected => PeerConnectionState::Disconnected,
        RTCPeerConnectionState::Failed => PeerConnectionState::Failed,
        RTCPeerConnectionState::Closed => PeerConnectionState::Closed,
        _ => PeerConnectionState::Connecting,
    }
}

fn webrtc_err(e: impl std::fmt::Display) -> MediaError {
    MediaError::WebRtc(e.to_string())
}

fn opus_codec() -> RTCRtpCodec {
    RTCRtpCodec {
        mime_type: MIME_TYPE_OPUS.to_owned(),
        clock_rate: 48_000,
        channels: 2,
        sdp_fmtp_line: OPUS_FMTP.to_owned(),
        rtcp_feedback: vec![],
    }
}

fn h264_codec(fmtp: &str) -> RTCRtpCodec {
    let fb = |typ: &str, parameter: &str| RTCPFeedback {
        typ: typ.to_owned(),
        parameter: parameter.to_owned(),
    };
    RTCRtpCodec {
        mime_type: MIME_TYPE_H264.to_owned(),
        clock_rate: 90_000,
        channels: 0,
        sdp_fmtp_line: fmtp.to_owned(),
        rtcp_feedback: vec![
            fb("nack", ""),
            fb("nack", "pli"),
            fb("ccm", "fir"),
            fb("transport-cc", ""),
        ],
    }
}

fn new_local_track(
    kind: RtpCodecKind,
    high_quality: bool,
    fps: u32,
) -> Result<(Arc<TrackLocalStaticSample>, u32), MediaError> {
    let ssrc: u32 = rand::random();
    let (codec, track_id) = match kind {
        RtpCodecKind::Video if high_quality => (h264_codec(H264_HIGH_FMTP), "screen"),
        RtpCodecKind::Video => (h264_codec(H264_FMTP), "screen"),
        _ => (opus_codec(), "audio"),
    };
    let track = MediaStreamTrack::new(
        "chattercrab".to_owned(),
        format!("{track_id}-{ssrc:08x}"),
        track_id.to_owned(),
        kind,
        vec![RTCRtpEncodingParameters {
            rtp_coding_parameters: RTCRtpCodingParameters {
                ssrc: Some(ssrc),
                ..Default::default()
            },
            codec,
            max_bitrate: if high_quality {
                H264_HIGH_MAX_BITRATE
            } else {
                0
            },
            max_framerate: high_quality.then_some(f64::from(fps.max(1))),
            ..Default::default()
        }],
    );
    let track = TrackLocalStaticSample::new(Instant::now(), track).map_err(webrtc_err)?;
    Ok((Arc::new(track), ssrc))
}

impl PeerManager {
    /// Creates a manager. The returned receiver carries connection callbacks that must
    /// be passed to [`PeerManager::handle_internal`].
    pub fn new(settings: PeerSettings) -> (Self, mpsc::Receiver<InternalEvent>) {
        let (tx, rx) = mpsc::channel(INTERNAL_CHANNEL_CAPACITY);
        let (audio_targets, _) = watch::channel(empty_targets());
        let (video_targets, _) = watch::channel(empty_targets());
        (
            Self {
                settings,
                me: None,
                ice_servers: Vec::new(),
                peers: HashMap::new(),
                next_generation: 0,
                internal_tx: tx,
                events: VecDeque::new(),
                screen_active: false,
                screen_high_quality: false,
                screen_fps: 0,
                audio_targets,
                video_targets,
                keyframe_requested: Arc::new(AtomicBool::new(false)),
            },
            rx,
        )
    }

    /// Sets our peer id and the ICE servers for connections created afterwards.
    pub fn configure(&mut self, me: PeerId, ice: Option<&IceServers>) {
        self.me = Some(me);
        self.ice_servers = ice
            .filter(|c| !c.urls.is_empty())
            .map(|c| {
                vec![RTCIceServer {
                    urls: c.urls.clone(),
                    username: String::new(),
                    credential: String::new(),
                }]
            })
            .unwrap_or_default();
    }

    /// Our peer id, if configured.
    pub fn me(&self) -> Option<PeerId> {
        self.me
    }

    /// Ids of all current peers.
    pub fn peer_ids(&self) -> Vec<PeerId> {
        self.peers.keys().copied().collect()
    }

    /// Audio send targets; the audio pump writes every encoded frame to each of them.
    pub fn audio_targets(&self) -> watch::Receiver<SendTargets> {
        self.audio_targets.subscribe()
    }

    /// Video send targets for the screen publisher.
    pub fn video_targets(&self) -> watch::Receiver<SendTargets> {
        self.video_targets.subscribe()
    }

    /// Flag set when any peer asks for a keyframe (PLI/FIR) or a new viewer joins.
    pub fn keyframe_flag(&self) -> Arc<AtomicBool> {
        self.keyframe_requested.clone()
    }

    /// Takes all queued output events.
    pub fn drain_events(&mut self) -> impl Iterator<Item = PeerEvent> + '_ {
        self.events.drain(..)
    }

    async fn build_connection(
        &mut self,
        peer: PeerId,
    ) -> Result<(Arc<dyn PeerConnection>, u64), MediaError> {
        let generation = self.next_generation;
        self.next_generation += 1;

        let mut media = RtcMediaEngine::default();
        media
            .register_codec(
                RTCRtpCodecParameters {
                    rtp_codec: opus_codec(),
                    payload_type: OPUS_PAYLOAD_TYPE,
                },
                RtpCodecKind::Audio,
            )
            .map_err(webrtc_err)?;
        media
            .register_codec(
                RTCRtpCodecParameters {
                    rtp_codec: h264_codec(H264_FMTP),
                    payload_type: H264_PAYLOAD_TYPE,
                },
                RtpCodecKind::Video,
            )
            .map_err(webrtc_err)?;
        media
            .register_codec(
                RTCRtpCodecParameters {
                    rtp_codec: h264_codec(H264_HIGH_FMTP),
                    payload_type: H264_HIGH_PAYLOAD_TYPE,
                },
                RtpCodecKind::Video,
            )
            .map_err(webrtc_err)?;
        let registry = register_default_interceptors(Registry::new(), &mut media)
            .map_err(webrtc_err)?
            .with(
                Slot::from(rtcp::RTCP_FORWARDER_SLOT),
                rtcp::KeyframeRequestForwarder::default(),
            );

        let config = RTCConfigurationBuilder::new()
            .with_ice_servers(self.ice_servers.clone())
            .with_bundle_policy(RTCBundlePolicy::MaxBundle)
            .with_rtcp_mux_policy(RTCRtcpMuxPolicy::Require)
            .build();
        let mut network_types = Vec::new();
        if !self.settings.udp_addrs.is_empty() {
            network_types.push(NetworkType::Udp4);
        }
        if !self.settings.tcp_addrs.is_empty() {
            network_types.push(NetworkType::Tcp4);
        }
        let settings = SettingEngineBuilder::new()
            .with_ice_timeouts(
                Some(ICE_DISCONNECTED_TIMEOUT),
                Some(ICE_FAILED_TIMEOUT),
                Some(ICE_KEEPALIVE),
            )
            .with_network_types(network_types)
            .with_multicast_dns_mode(MulticastDnsMode::Disabled)
            .with_include_loopback_candidate(self.settings.include_loopback)
            .build();

        let handler = Arc::new(Handler {
            peer,
            generation,
            tx: self.internal_tx.clone(),
        });
        let pc = PeerConnectionBuilder::new()
            .with_configuration(config)
            .with_media_engine(media)
            .with_setting_engine(settings)
            .with_interceptor_registry(registry)
            .with_handler(handler)
            .with_udp_addrs(self.settings.udp_addrs.clone())
            .with_tcp_addrs(self.settings.tcp_addrs.clone())
            .build()
            .await
            .map_err(webrtc_err)?;
        Ok((Arc::new(pc), generation))
    }

    /// Creates a connection to `peer`. With `initiate` the local tracks are attached
    /// immediately, which makes the connection send the first offer; otherwise it waits
    /// for the remote offer.
    pub async fn add_peer(&mut self, peer: PeerId, initiate: bool) -> Result<(), MediaError> {
        let Some(me) = self.me else {
            return Err(MediaError::WebRtc("add_peer before configure".into()));
        };
        if self.peers.contains_key(&peer) {
            self.remove_peer(peer).await;
        }
        let (pc, generation) = self.build_connection(peer).await?;
        self.peers.insert(
            peer,
            PeerEntry {
                generation,
                pc,
                neg: Negotiator::new(me, peer),
                ready: initiate,
                remote_set: false,
                pending_ice: Vec::new(),
                audio: None,
                video: None,
                restarts: 0,
                connected: false,
            },
        );
        self.events.push_back(PeerEvent::State {
            peer,
            state: PeerConnectionState::Connecting,
        });
        if initiate {
            self.sync_tracks(peer).await;
        }
        Ok(())
    }

    /// Closes and forgets the connection to `peer`.
    pub async fn remove_peer(&mut self, peer: PeerId) {
        if let Some(entry) = self.peers.remove(&peer) {
            let _ = entry.pc.close().await;
            self.events.push_back(PeerEvent::State {
                peer,
                state: PeerConnectionState::Closed,
            });
            self.publish_targets();
        }
    }

    /// Closes every connection and forgets our peer id.
    pub async fn remove_all(&mut self) {
        let peers: Vec<PeerId> = self.peers.keys().copied().collect();
        for peer in peers {
            self.remove_peer(peer).await;
        }
        self.me = None;
    }

    /// Starts or stops sending the screen-share track to every peer.
    ///
    /// `high_quality` selects H.264 High at the fixed high bitrate. `fps` is the
    /// encoding's max frame rate. Both are stored when the share starts.
    pub async fn set_screen_active(&mut self, active: bool, high_quality: bool, fps: u32) {
        if active {
            self.screen_high_quality = high_quality;
            self.screen_fps = fps;
        }
        if self.screen_active == active {
            return;
        }
        self.screen_active = active;
        let peers: Vec<PeerId> = self.peers.keys().copied().collect();
        for peer in peers {
            self.sync_tracks(peer).await;
        }
        if active {
            self.keyframe_requested.store(true, Ordering::Relaxed);
        }
    }

    /// Attaches or detaches local tracks so the connection matches the desired state.
    /// Track changes trigger `negotiationneeded`, which drives the offer.
    async fn sync_tracks(&mut self, peer: PeerId) {
        let screen_active = self.screen_active;
        let high_quality = self.screen_high_quality;
        let fps = self.screen_fps;
        let keyframe = self.keyframe_requested.clone();
        let Some(entry) = self.peers.get_mut(&peer) else {
            return;
        };
        if !entry.ready {
            return;
        }
        let mut changed = false;
        if entry.audio.is_none() {
            match attach_track(peer, &entry.pc, RtpCodecKind::Audio, None, false, 0).await {
                Ok(s) => {
                    entry.audio = Some(s);
                    changed = true;
                }
                Err(e) => tracing::warn!(%peer, "attach audio track: {e}"),
            }
        }
        if screen_active && entry.video.is_none() {
            match attach_track(
                peer,
                &entry.pc,
                RtpCodecKind::Video,
                Some(keyframe.clone()),
                high_quality,
                fps,
            )
            .await
            {
                Ok(s) => {
                    entry.video = Some(s);
                    keyframe.store(true, Ordering::Relaxed);
                    changed = true;
                }
                Err(e) => tracing::warn!(%peer, "attach video track: {e}"),
            }
        }
        if !screen_active && let Some(video) = entry.video.take() {
            if let Err(e) = entry.pc.remove_track(&video.target.sender).await {
                tracing::warn!(%peer, "remove video track: {e}");
            }
            changed = true;
        }
        if changed {
            self.publish_targets();
            // Track changes queue `negotiationneeded` in the core, which is only
            // delivered once the driver runs.
            self.wake_driver(peer).await;
        }
    }

    fn publish_targets(&self) {
        let audio: Vec<Arc<SendTarget>> = self
            .peers
            .values()
            .filter(|e| e.connected)
            .filter_map(|e| e.audio.as_ref().map(|s| s.target.clone()))
            .collect();
        let video: Vec<Arc<SendTarget>> = self
            .peers
            .values()
            .filter(|e| e.connected)
            .filter_map(|e| e.video.as_ref().map(|s| s.target.clone()))
            .collect();
        self.audio_targets.send_replace(Arc::from(audio));
        self.video_targets.send_replace(Arc::from(video));
    }

    async fn make_offer(&mut self, peer: PeerId) {
        let Some(entry) = self.peers.get_mut(&peer) else {
            return;
        };
        let result = async {
            let offer = entry.pc.create_offer(None).await.map_err(webrtc_err)?;
            entry
                .pc
                .set_local_description(offer.clone())
                .await
                .map_err(webrtc_err)?;
            Ok::<_, MediaError>(offer.sdp)
        }
        .await;
        match result {
            Ok(sdp) => {
                tracing::debug!(%peer, "sending offer");
                self.events
                    .push_back(PeerEvent::Signal(OutgoingSignal::Offer { to: peer, sdp }));
            }
            Err(e) => {
                tracing::warn!(%peer, "create offer: {e}");
                entry.neg.offer_failed();
            }
        }
    }

    /// Handles a remote offer (creating the connection if this is the first contact).
    pub async fn remote_offer(&mut self, from: PeerId, sdp: String) -> Result<(), MediaError> {
        tracing::debug!(peer = %from, "received offer");
        if !self.peers.contains_key(&from) {
            self.add_peer(from, false).await?;
        }
        let Some(entry) = self.peers.get_mut(&from) else {
            return Ok(());
        };
        let rollback = match entry.neg.remote_offer() {
            OfferDecision::Ignore => {
                tracing::debug!(peer = %from, "ignoring colliding offer (impolite)");
                return Ok(());
            }
            OfferDecision::Accept { rollback } => rollback,
        };
        if rollback {
            let rb = RTCSessionDescription::rollback(None).map_err(webrtc_err)?;
            entry
                .pc
                .set_local_description(rb)
                .await
                .map_err(webrtc_err)?;
        }
        let offer = RTCSessionDescription::offer(sdp).map_err(webrtc_err)?;
        entry
            .pc
            .set_remote_description(offer)
            .await
            .map_err(webrtc_err)?;
        entry.remote_set = true;
        entry.ready = true;
        let pending = std::mem::take(&mut entry.pending_ice);
        for c in pending {
            if let Err(e) = entry.pc.add_ice_candidate(c).await {
                tracing::debug!(peer = %from, "buffered candidate rejected: {e}");
            }
        }
        // Attach our tracks to the transceivers the offer created before answering, so
        // the answer already sends our audio.
        self.sync_tracks(from).await;
        self.wake_driver(from).await;

        let Some(entry) = self.peers.get_mut(&from) else {
            return Ok(());
        };
        let answer = entry.pc.create_answer(None).await.map_err(webrtc_err)?;
        entry
            .pc
            .set_local_description(answer.clone())
            .await
            .map_err(webrtc_err)?;
        self.events
            .push_back(PeerEvent::Signal(OutgoingSignal::Answer {
                to: from,
                sdp: answer.sdp,
            }));
        if entry.neg.answer_sent() {
            self.make_offer(from).await;
        }
        Ok(())
    }

    /// Handles a remote answer.
    pub async fn remote_answer(&mut self, from: PeerId, sdp: String) -> Result<(), MediaError> {
        tracing::debug!(peer = %from, "received answer");
        let Some(entry) = self.peers.get_mut(&from) else {
            return Ok(());
        };
        if !entry.neg.remote_answer() {
            tracing::debug!(peer = %from, "dropping unexpected answer");
            return Ok(());
        }
        let answer = RTCSessionDescription::answer(sdp).map_err(webrtc_err)?;
        let applied = entry
            .pc
            .set_remote_description(answer)
            .await
            .map_err(webrtc_err);
        let again = entry.neg.answer_applied();
        applied?;
        entry.remote_set = true;
        let pending = std::mem::take(&mut entry.pending_ice);
        for c in pending {
            if let Err(e) = entry.pc.add_ice_candidate(c).await {
                tracing::debug!(peer = %from, "buffered candidate rejected: {e}");
            }
        }
        self.wake_driver(from).await;
        if again {
            self.make_offer(from).await;
        }
        Ok(())
    }

    /// Handles a remote trickle ICE candidate.
    pub async fn remote_ice(
        &mut self,
        from: PeerId,
        candidate: String,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
    ) {
        if candidate.is_empty() {
            return; // end-of-candidates
        }
        let Some(entry) = self.peers.get_mut(&from) else {
            return;
        };
        let init = RTCIceCandidateInit {
            candidate,
            sdp_mid,
            sdp_mline_index,
            ..Default::default()
        };
        if !entry.remote_set {
            if entry.pending_ice.len() < MAX_PENDING_ICE {
                entry.pending_ice.push(init);
            }
            return;
        }
        if let Err(e) = entry.pc.add_ice_candidate(init).await
            && !entry.neg.is_ignoring_offer()
        {
            tracing::debug!(peer = %from, "remote candidate rejected: {e}");
        }
        tracing::debug!(peer = %from, "remote candidate added");
        self.wake_driver(from).await;
    }

    /// Wakes the connection's background driver after a state change made through the
    /// API.
    ///
    /// Workaround for webrtc 0.21.1: `add_ice_candidate` (UDP), `add_track` and
    /// `remove_track` change the core without notifying the driver, which then keeps
    /// sleeping on its previous timeout: connectivity checks for new candidate pairs
    /// never start and `negotiationneeded` is never delivered. Any sample write wakes
    /// it, so one Opus
    /// silence frame is written to this peer's audio sender. Before DTLS completes the
    /// packet is dropped by webrtc (one log line); afterwards it is a valid 20 ms silence
    /// frame. See ADR-212.
    async fn wake_driver(&self, peer: PeerId) {
        /// TOC byte for CELT fullband 20 ms followed by a minimal silent payload.
        static OPUS_SILENCE: [u8; 3] = [0xF8, 0xFF, 0xFE];
        let Some(target) = self
            .peers
            .get(&peer)
            .and_then(|e| e.audio.as_ref())
            .map(|a| a.target.clone())
        else {
            return;
        };
        let _ = target
            .write(Bytes::from_static(&OPUS_SILENCE), Duration::from_millis(20))
            .await;
    }

    /// Processes one connection callback.
    pub async fn handle_internal(&mut self, ev: InternalEvent) {
        let peer = ev.peer;
        let Some(entry) = self.peers.get_mut(&peer) else {
            return;
        };
        if entry.generation != ev.generation {
            return;
        }
        match ev.kind {
            InternalKind::Candidate(init) => {
                tracing::debug!(%peer, candidate = %init.candidate, "local candidate");
                self.events
                    .push_back(PeerEvent::Signal(OutgoingSignal::Ice {
                        to: peer,
                        candidate: init.candidate,
                        sdp_mid: init.sdp_mid,
                        sdp_mline_index: init.sdp_mline_index,
                    }));
            }
            InternalKind::State(state) => {
                tracing::info!(%peer, ?state, "peer connection state");
                if state == RTCPeerConnectionState::Connected {
                    entry.restarts = 0;
                }
                if state == RTCPeerConnectionState::Failed
                    && !entry.neg.is_polite()
                    && entry.restarts < MAX_ICE_RESTARTS
                {
                    entry.restarts += 1;
                    tracing::info!(%peer, attempt = entry.restarts, "restarting ICE");
                    if let Err(e) = entry.pc.restart_ice().await {
                        tracing::warn!(%peer, "ICE restart: {e}");
                    }
                }
                // Keep sending through a transient `Disconnected` (ICE may recover);
                // stop on `Failed`/`Closed` and before the first `Connected`.
                let connected = state == RTCPeerConnectionState::Connected
                    || (entry.connected && state == RTCPeerConnectionState::Disconnected);
                let targets_changed = connected != entry.connected;
                entry.connected = connected;
                self.events.push_back(PeerEvent::State {
                    peer,
                    state: map_state(state),
                });
                if targets_changed {
                    self.publish_targets();
                }
            }
            InternalKind::Track(track) => {
                let ev = match track.kind().await {
                    RtpCodecKind::Video => PeerEvent::RemoteVideo { peer, track },
                    _ => PeerEvent::RemoteAudio { peer, track },
                };
                self.events.push_back(ev);
            }
            InternalKind::NegotiationNeeded => {
                tracing::debug!(%peer, "negotiation needed");
                if entry.neg.negotiation_needed() {
                    self.make_offer(peer).await;
                }
            }
        }
    }
}

/// Creates a local track, adds it to `pc`, and (for video) starts a task that watches
/// for keyframe requests from the remote peer.
async fn attach_track(
    peer: PeerId,
    pc: &Arc<dyn PeerConnection>,
    kind: RtpCodecKind,
    keyframe: Option<Arc<AtomicBool>>,
    high_quality: bool,
    fps: u32,
) -> Result<LocalSender, MediaError> {
    let (track, ssrc) = new_local_track(kind, high_quality, fps)?;
    let sender = pc
        .add_track(track.clone() as Arc<dyn TrackLocal>)
        .await
        .map_err(webrtc_err)?;
    let rtcp_task = keyframe.map(|flag| {
        let watched = track.clone();
        tokio::spawn(async move {
            while let Some(event) = watched.poll().await {
                if let TrackLocalEvent::OnRtcpPacket(packets) = event
                    && packets
                        .iter()
                        .any(|p| rtcp::is_keyframe_request(p.as_ref()))
                {
                    flag.store(true, Ordering::Relaxed);
                }
            }
        })
    });
    Ok(LocalSender {
        target: Arc::new(SendTarget {
            peer,
            track,
            sender,
            ssrc,
            payload_type: AtomicU16::new(u16::MAX),
        }),
        rtcp_task,
    })
}
