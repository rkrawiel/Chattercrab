//! Headless call participant for end-to-end tests: joins a room on a real
//! `chattercrab-signal` server, runs the real [`MediaEngine`] with a synthetic tone
//! instead of the microphone, and reports whether audio from the other peers arrived.
//!
//! ```text
//! cargo run -p chattercrab-media --example signal_call -- \
//!     --server http://127.0.0.1:8443 --create e2e --name a --tone 440 --seconds 10
//! ```
//!
//! The last line of output is machine-readable:
//!
//! ```text
//! RESULT name=a peers_seen=1 peers_connected=1 min_remote_level=0.2100 min_heard_secs=9.1 max_local_level=0.2121 ok=true
//! ```
//!
//! The process exits non-zero unless at least `--expect-peers` peers reached
//! `Connected`, every connected peer's audio level exceeded `--min-level`, and every
//! connected peer was heard above that level for at least `--min-heard-secs` in total
//! (levels arrive every 100 ms; a peak alone would hide audio that stops mid-call).
//! The signaling choreography mirrors `crates/client/src/net/session.rs`.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use anyhow::{Context, bail};
use chattercrab_media::{
    MediaCommand, MediaConfig, MediaDiagnostics, MediaEngine, MediaEvent, OutgoingSignal,
    PeerConnectionState,
};
use chattercrab_protocol::{
    ClientMsg, CreateRoomRequest, DisplayName, ErrorCode, IceServers, PeerId, RoomName,
    RoomSummary, ServerMsg,
};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

/// Output device name that never exists, so the engine runs without playback (the mixer
/// then paces itself on a timer). `--play` uses the default speaker instead.
const NO_OUTPUT_DEVICE: &str = "chattercrab-signal-call-no-output";

struct Args {
    server: String,
    room: Option<String>,
    create: Option<String>,
    name: String,
    tone: f32,
    seconds: u64,
    expect_peers: usize,
    min_level: f32,
    min_heard_secs: f32,
    play: bool,
}

const USAGE: &str = "usage: signal_call --server URL (--room ID | --create NAME) [--name NAME] \
[--tone HZ] [--seconds N] [--expect-peers N] [--min-level F] [--min-heard-secs F] [--play]";

fn parse_args() -> anyhow::Result<Args> {
    let mut args = Args {
        server: "http://127.0.0.1:8443".into(),
        room: None,
        create: None,
        name: "e2e".into(),
        tone: 440.0,
        seconds: 10,
        expect_peers: 1,
        min_level: 0.02,
        min_heard_secs: 0.0,
        play: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || {
            it.next()
                .with_context(|| format!("{flag} needs a value\n{USAGE}"))
        };
        match flag.as_str() {
            "--server" => args.server = value()?,
            "--room" => args.room = Some(value()?),
            "--create" => args.create = Some(value()?),
            "--name" => args.name = value()?,
            "--tone" => args.tone = value()?.parse()?,
            "--seconds" => args.seconds = value()?.parse()?,
            "--expect-peers" => args.expect_peers = value()?.parse()?,
            "--min-level" => args.min_level = value()?.parse()?,
            "--min-heard-secs" => args.min_heard_secs = value()?.parse()?,
            "--play" => args.play = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => bail!("unknown argument {other}\n{USAGE}"),
        }
    }
    if args.room.is_none() && args.create.is_none() {
        bail!("--room or --create is required\n{USAGE}");
    }
    Ok(args)
}

/// What happened during the call.
#[derive(Default)]
struct Report {
    peers_seen: BTreeSet<PeerId>,
    connected: BTreeSet<PeerId>,
    max_level: BTreeMap<PeerId, f32>,
    /// Number of level reports in which the peer was above `--min-level`.
    heard_reports: BTreeMap<PeerId, u32>,
    max_local_level: f32,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = parse_args()?;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    // reqwest and tokio-tungstenite use rustls without a built-in provider.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let base = args.server.trim_end_matches('/').to_owned();
    let http = reqwest::Client::new();
    let room = match (&args.room, &args.create) {
        (Some(room), _) => room.clone(),
        (None, Some(name)) => {
            let req = CreateRoomRequest {
                name: RoomName::new(name)?,
            };
            let summary: RoomSummary = http
                .post(format!("{base}/rooms"))
                .json(&req)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            println!("ROOM {}", summary.id);
            summary.id.to_string()
        }
        (None, None) => unreachable!("checked in parse_args"),
    };

    let ice: IceServers = http
        .get(format!("{base}/ice-servers"))
        .send()
        .await
        .context("fetching ICE servers")?
        .error_for_status()
        .context("ICE servers request failed")?
        .json()
        .await
        .context("decoding ICE servers")?;

    let ws_base = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        bail!("--server must start with http:// or https://");
    };
    let (ws, _) = tokio_tungstenite::connect_async(format!("{ws_base}/ws/{room}"))
        .await
        .context("websocket connect")?;
    let (mut sink, mut stream) = ws.split();

    let config = MediaConfig {
        input_device: None,
        output_device: (!args.play).then(|| NO_OUTPUT_DEVICE.to_owned()),
        noise_suppression: false,
        input_gain: 1.0,
    };
    let diagnostics = MediaDiagnostics {
        test_tone: Some(args.tone),
    };
    let mut engine = MediaEngine::start_with_diagnostics(config, diagnostics)?;

    let join = ClientMsg::Join {
        display_name: DisplayName::new(&args.name)?,
    };
    sink.send(Message::Text(serde_json::to_string(&join)?.into()))
        .await?;

    let mut report = Report::default();
    let deadline = tokio::time::sleep(Duration::from_secs(args.seconds));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            frame = stream.next() => {
                let text = match frame {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Close(_))) | None => bail!("server closed the connection"),
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => bail!("websocket error: {e}"),
                };
                let msg: ServerMsg = serde_json::from_str(&text)?;
                for cmd in on_server_msg(msg, Some(&ice), &mut report)? {
                    engine.commands.send(cmd).await?;
                }
            }
            Some(event) = engine.events.recv() => {
                if let Some(msg) = on_media_event(event, &mut report, args.min_level) {
                    sink.send(Message::Text(serde_json::to_string(&msg)?.into())).await?;
                }
            }
        }
    }

    let _ = engine.commands.send(MediaCommand::RemoveAllPeers).await;
    let _ = engine.commands.send(MediaCommand::Shutdown).await;
    let _ = sink.send(Message::Close(None)).await;
    // Let the engine close peer connections and join its threads.
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        while engine.events.recv().await.is_some() {}
    })
    .await;

    let min_remote = report
        .connected
        .iter()
        .map(|p| report.max_level.get(p).copied().unwrap_or(0.0))
        .fold(f32::INFINITY, f32::min);
    let min_remote = if min_remote.is_finite() {
        min_remote
    } else {
        0.0
    };
    // `Levels` is emitted every 100 ms.
    let min_heard_secs = report
        .connected
        .iter()
        .map(|p| report.heard_reports.get(p).copied().unwrap_or(0) as f32 / 10.0)
        .fold(f32::INFINITY, f32::min);
    let min_heard_secs = if min_heard_secs.is_finite() {
        min_heard_secs
    } else {
        0.0
    };
    let ok = report.connected.len() >= args.expect_peers
        && min_remote > args.min_level
        && min_heard_secs >= args.min_heard_secs;
    println!(
        "RESULT name={} peers_seen={} peers_connected={} min_remote_level={:.4} min_heard_secs={:.1} max_local_level={:.4} ok={}",
        args.name,
        report.peers_seen.len(),
        report.connected.len(),
        min_remote,
        min_heard_secs,
        report.max_local_level,
        ok
    );
    std::process::exit(if ok { 0 } else { 1 });
}

/// Server message to media commands, as in the client's session logic.
fn on_server_msg(
    msg: ServerMsg,
    ice: Option<&IceServers>,
    report: &mut Report,
) -> anyhow::Result<Vec<MediaCommand>> {
    Ok(match msg {
        ServerMsg::Welcome { you, peers, .. } => {
            println!("JOINED me={you} peers={}", peers.len());
            let mut cmds = vec![MediaCommand::Configure {
                me: you,
                ice: ice.cloned(),
            }];
            // The newcomer offers to everyone already present.
            for p in peers {
                report.peers_seen.insert(p.id);
                cmds.push(MediaCommand::AddPeer {
                    peer: p.id,
                    initiate: true,
                });
            }
            cmds
        }
        ServerMsg::PeerJoined { peer } => {
            report.peers_seen.insert(peer.id);
            vec![MediaCommand::AddPeer {
                peer: peer.id,
                initiate: false,
            }]
        }
        ServerMsg::PeerLeft { peer_id } => vec![MediaCommand::RemovePeer { peer: peer_id }],
        ServerMsg::Offer { from, sdp } => vec![MediaCommand::RemoteOffer { from, sdp }],
        ServerMsg::Answer { from, sdp } => vec![MediaCommand::RemoteAnswer { from, sdp }],
        ServerMsg::Ice {
            from,
            candidate,
            sdp_mid,
            sdp_mline_index,
        } => vec![MediaCommand::RemoteIce {
            from,
            candidate,
            sdp_mid,
            sdp_mline_index,
        }],
        ServerMsg::Error { code, message } => match code {
            ErrorCode::RoomNotFound
            | ErrorCode::RoomFull
            | ErrorCode::InvalidDisplayName
            | ErrorCode::JoinTimeout
            | ErrorCode::NotJoined => bail!("server error {code:?}: {message}"),
            _ => {
                eprintln!("server notice {code:?}: {message}");
                Vec::new()
            }
        },
        _ => Vec::new(),
    })
}

/// Media event to an outgoing signaling message, recording connectivity and levels.
fn on_media_event(event: MediaEvent, report: &mut Report, min_level: f32) -> Option<ClientMsg> {
    match event {
        MediaEvent::Signal(signal) => Some(match signal {
            OutgoingSignal::Offer { to, sdp } => ClientMsg::Offer { to, sdp },
            OutgoingSignal::Answer { to, sdp } => ClientMsg::Answer { to, sdp },
            OutgoingSignal::Ice {
                to,
                candidate,
                sdp_mid,
                sdp_mline_index,
            } => ClientMsg::Ice {
                to,
                candidate,
                sdp_mid,
                sdp_mline_index,
            },
        }),
        MediaEvent::PeerState { peer, state } => {
            println!("PEER {peer} {state:?}");
            if state == PeerConnectionState::Connected {
                report.connected.insert(peer);
            }
            None
        }
        MediaEvent::Levels { local, peers } => {
            report.max_local_level = report.max_local_level.max(local);
            for (peer, level) in peers {
                let max = report.max_level.entry(peer).or_insert(0.0);
                *max = max.max(level);
                if level > min_level {
                    *report.heard_reports.entry(peer).or_insert(0) += 1;
                }
            }
            None
        }
        MediaEvent::Error(e) => {
            eprintln!("media: {e}");
            None
        }
        _ => None,
    }
}
