# Decision Log

ADR-style record of choices made while building Chattercrab. Each entry: context,
decision, consequences. ADR-0xx are cross-cutting; the 100 to 500 series cover the
server, media engine, client, deployment and noise suppression.

## ADR-001: Repository root is the workspace root

* Context: the brief describes a `chattercrab/` workspace directory; the git repository is
  already named `Chattercrab`.
* Decision: the repository root is the `chattercrab/` workspace. No extra nesting.
* Consequences: `cargo` commands run from the repository root.

## ADR-002: `PeerId` is a UUID v4

* Context: the brief allows ULID or UUID v4.
* Decision: UUID v4 serialized lowercase hyphenated. Byte order of the UUID equals the
  string order of its lowercase hex form, so "lexicographically smaller" is unambiguous.
* Consequences: depends on `uuid` with `v4` and `serde`.

## ADR-003: Rooms are created over REST and deleted when empty

* Context: the brief requires `POST /rooms` with a room cap, and deletion when empty.
* Decision: joining a nonexistent room fails (`404` before upgrade, `room_not_found`
  after). A room is deleted the moment its last member leaves. Rooms that were created
  but never joined are evicted by the idle sweeper. Rooms with members are never
  idle-evicted, since a voice call produces no signaling traffic.
* Consequences: if a lone member's connection drops, the room disappears and the
  reconnect fails with `room_not_found`; the client returns to the lobby with a notice.

## ADR-004: Server-side heartbeat uses WebSocket Ping frames

* Context: "server pings every 20 s and closes after 2 missed pongs", while the message
  list also contains application `ping`/`pong`.
* Decision: the heartbeat uses protocol-level Ping/Pong frames, which every WebSocket
  client answers automatically. Application `ping`/`pong` stays available for clients
  that want an RTT probe.

## ADR-005: Slow consumers are kicked through a cancellation token

* Context: broadcasting happens under a room lock; the slow member's cleanup itself
  needs the room lock and broadcasts.
* Decision: broadcasters `try_send`; on `Full` they cancel the member's kick token. The
  member's own task performs cleanup after the broadcaster released the lock.
* Consequences: no re-entrancy, no lock held across socket I/O.

## ADR-006: Derived STUN URLs in `TurnCredentials`

* Context: webrtc gathers server-reflexive candidates only from `stun:` URLs, and the
  environment template only lists `turn:`/`turns:` URLs.
* Decision: for every `turn:` URL with UDP transport, the server adds `stun:host:port`.
* Consequences: peers behind ordinary NAT connect directly instead of always relaying.

Superseded by ADR-504.

## ADR-007: Native build tooling on the Windows development host

* Context: the development machine is Windows with MSVC. `opus` 0.4 builds libopus via
  `opusic-sys`, which needs CMake; cross builds need a C toolchain per target.
* Decision: install CMake and Zig from PyPI (`pip install --user cmake ziglang`) and
  `cargo-zigbuild`/`cargo-deny` with `cargo install`. No system-wide installers.
* Consequences: documented in the README build section.

## Signaling server (ADR-100 series)

### ADR-100: rustls with the `ring` provider, PEM parsing via `rustls-pki-types`

* Context: `axum-server`'s `tls-rustls` feature pulls in `aws-lc-rs`, which needs CMake
  and NASM and complicates the static musl cross build. `rustls-pemfile` is archived.
* Decision: use `axum-server` with `tls-rustls-no-provider`, build the `ServerConfig`
  with `rustls::crypto::ring::default_provider()`, and parse PEM with
  `rustls::pki_types::pem::PemObject`. ALPN is set to `http/1.1` only.
* Consequences: pure Rust plus `ring`'s C/asm, which `cargo zigbuild` handles. WebSocket
  upgrades always use HTTP/1.1.

### ADR-101: Writes race against the kick token and a 10 s timeout

* Context: a member whose socket stops draining blocks its own task inside a socket
  write, so it would never notice its kick token and never clean up.
* Decision: every socket write is raced against the member's kick token and bounded by
  a 10 s timeout. Final close frames are bounded by 1 s.
* Consequences: slow or stalled clients are always removed, and `peer_left` reaches the
  rest of the room.

### ADR-102: Direct replies bypass the outbound queue

* Context: replies to the sender (`pong`, `error`, `screen_granted`, `screen_denied`) are
  produced inside the connection task that owns the socket.
* Decision: they are written straight to the socket; broadcasts and relayed messages go
  through the bounded queue. `screen_granted` is written before the queued
  `screen_started`, matching PROTOCOL.md.

### ADR-103: Room-count reservation with compare-and-swap

* Context: `DashMap::len` is not atomic with insertion, so a check-then-insert could
  exceed `CHATTERCRAB_MAX_ROOMS` under concurrent `POST /rooms`.
* Decision: an `AtomicUsize` counter is reserved with a CAS loop before insertion and
  decremented only when `remove_if(Arc::ptr_eq)` actually removes the room.

### ADR-104: Shutdown deadline covers both HTTP and WebSocket tasks

* Context: upgraded WebSockets leave hyper's connection tracking, so `axum-server`'s
  graceful shutdown does not wait for them.
* Decision: each upgraded socket holds a `TaskTracker` token. After shutdown is requested
  one shared deadline (`shutdown_grace`, 10 s) bounds both the HTTP drain and the wait for
  WebSocket tasks, which close themselves with `1001` as soon as the shutdown token fires.
* Consequences: the process exits within 10 s of `SIGTERM` in all cases.

### ADR-105: Upgrades during shutdown are refused with 503

* Decision: `GET /ws/{room_id}` answers `503` (`internal`) once shutdown started, instead
  of accepting a socket only to close it immediately. Documented in PROTOCOL.md.

## Media engine (ADR-200 series)

### ADR-200: Mapping the spec's WebRTC names onto webrtc 0.21

* Context: the brief names the webrtc-rs 0.1x API (`RTCPeerConnection`, `on_track`,
  `SettingEngine`, ...). webrtc 0.21.1 is a rewrite as a thin async layer over the sans-IO
  `rtc` core, with a different API surface.
* Decision: use 0.21.1 (current stable) and map as follows.

| Spec | webrtc 0.21.1 |
|------|---------------|
| `RTCPeerConnection` | `PeerConnectionBuilder::build()` returning `impl PeerConnection`, stored as `Arc<dyn PeerConnection>` |
| callbacks (`on_track`, `on_ice_candidate`, `on_negotiation_needed`, state changes) | one `PeerConnectionEventHandler` trait object per connection (`peer::Handler`), which forwards into a bounded channel with `try_send` |
| `SettingEngine` | `SettingEngineBuilder` |
| ICE timeouts (disconnected 5 s, failed 15 s) | `SettingEngineBuilder::with_ice_timeouts(Some(5s), Some(15s), Some(2s keepalive))` |
| UDP and TCP candidates only | `with_network_types([Udp4, Tcp4])`, `with_multicast_dns_mode(Disabled)`, sockets from `with_udp_addrs(["0.0.0.0:0"])` and `with_tcp_addrs(["0.0.0.0:0"])` (passive ICE-TCP) |
| `RTCBundlePolicy::MaxBundle`, `RTCRtcpMuxPolicy::Require` | same enums via `RTCConfigurationBuilder::with_bundle_policy` / `with_rtcp_mux_policy` |
| ICE servers from `TurnCredentials` | one `RTCIceServer { urls, username, credential }` |
| `TrackLocalStaticSample` | same type; samples written with `sample_writer(ssrc, payload_type).write_sample(&Sample)` |
| reading remote RTP | `TrackRemote::poll()` yielding `TrackRemoteEvent::OnRtpPacket` |
| inbound PLI/FIR at the sender | a custom interceptor marks them `DeliverToApplication`; read with `TrackLocal::poll()` (ADR-204) |
| crypto | default `crypto-ring` provider (no aws-lc / cmake / nasm needed) |

* Consequences: `rtc` is a direct dependency (same version) to name `Sample`,
  `MediaStreamTrack` codings, the H.264 depacketizer and RTCP types.

### ADR-201: One local track per connection, encoded once

* Context: the brief asks for one Opus `TrackLocalStaticSample` shared by all connections.
  In 0.21 a local track stores a single binding context: adding the same track to a
  second connection rebinds it, so a shared track would only feed one peer.
* Decision: create one `TrackLocalStaticSample` per connection (fresh SSRC each), but
  encode only once. The audio pump wraps each encoded frame in `Bytes` (refcounted) and
  writes the same buffer to every connection's track. Same for the screen track.
* Consequences: identical CPU cost for encoding; packetization and SRTP happen per
  connection, which is inherent to a mesh anyway.

### ADR-202: Negotiation model

* Context: perfect negotiation is required; the brief's "newcomer offers" flow also needs
  to work with renegotiation for screen share.
* Decision: `peer::negotiation::Negotiator` is a pure state machine (impolite = smaller
  `PeerId`). Offers are driven by `on_negotiation_needed`, which webrtc fires
  spec-compliantly when tracks are added or removed. The newcomer (`AddPeer { initiate:
  true }`) attaches its tracks immediately and therefore offers; the existing member
  (`initiate: false`) attaches nothing until the first remote offer is applied, then adds
  its tracks (reusing the offer's transceivers) before answering, so audio flows both ways
  after one round trip. Glare is resolved by ignore (impolite) or rollback (polite), with
  a queued re-offer after the answer.
* Consequences: verified by unit tests (glare in both delivery orders, renegotiation,
  200 randomized races) and by `tests/peer_loopback.rs`, where both sides start sharing
  simultaneously over real connections.

### ADR-203: ICE recovery and candidate scope

* Decision: on `Failed`, the impolite side calls `restart_ice()` (up to 2 attempts per
  peer; the counter resets on `Connected`). The polite side waits for the restart offer.
  Only IPv4 UDP/TCP host candidates are gathered locally; server-reflexive and relay
  candidates come from the configured STUN/TURN servers. mDNS candidates are disabled.
* Rationale: an ICE restart keeps DTLS and media state, avoiding the races of tearing down
  and recreating connections from both sides. IPv6 is left out to keep candidate counts
  small for a 5-peer mesh; Coturn covers IPv6-only paths if ever needed.

### ADR-204: Keyframe requests through an interceptor

* Context: in 0.21 inbound RTCP stops at the end of the interceptor chain unless marked
  for the application.
* Decision: `peer::rtcp::KeyframeRequestForwarder` (slot 14 000, after all built-ins)
  passes PLI and FIR packets to the application and drops other RTCP. A task per video
  sender polls the track and sets the shared keyframe flag, which the encoder thread reads
  every frame. Adding a new viewer also sets the flag.

### ADR-205: Real-time priority without dbus

* Decision: `audio_thread_priority` with default features off. On Windows it uses MMCSS;
  on Linux without the `dbus` feature it promotes the mixer thread directly with
  `SCHED_FIFO`, which needs an `RLIMIT_RTPRIO` grant (for example via
  `/etc/security/limits.d`). If promotion fails the mixer logs a warning and runs at
  normal priority.
* Rationale: avoids a `libdbus-1-dev` build dependency and keeps cross-compilation simple.
  rtkit support can be enabled later by turning the feature back on.

### ADR-206: Opus settings and DTX framing

* Decision: VoIP, mono 48 kHz, 32 kbps, DTX on, in-band FEC on, complexity 5, and
  `packet_loss_perc = 10`. Every 20 ms frame is sent, including 1 to 3 byte DTX frames.
* Rationale: libopus only embeds FEC data when it expects loss, so a non-zero loss
  percentage is required for FEC to have any effect. Sending DTX frames keeps the RTP
  timestamp and sequence advancing exactly 960 per packet, which keeps the jitter buffer
  simple; the cost is about 1.2 kbit/s during silence.

### ADR-207: Clocking, buffering and resampling

* Decision: capture ring 500 ms (device rate), playback ring 200 ms (device rate). The
  mixer produces a frame whenever the playback ring holds less than 40 ms, so the output
  device clock paces mixing. Without an output device it paces itself on a 20 ms timer.
  Rate conversion uses rubato's `Async` cubic polynomial resampler (no sinc filter), in
  fixed-input mode, on the encoder thread (device rate to 48 kHz) and on the mixer thread
  (48 kHz to device rate).
* Rationale: the polynomial resampler is the voice-call option rubato recommends and is
  allocation-free in `process_into_buffer`. Clock drift between peers is absorbed by the
  jitter buffer (PLC on underrun, frame drop when more than 60 ms above target).

### ADR-208: Jitter buffer

* Decision: 16 fixed slots keyed by sequence number, playout target 2 to 6 frames
  (40 to 120 ms) from `1 + ceil(3 * jitter / 20 ms)` using the RFC 3550 estimator, raised by
  one frame on each underrun and decayed after 5 s without loss. Missing frame: decode the
  next packet with FEC if present, otherwise PLC; after 5 concealed frames with an empty
  buffer, stop and re-buffer. Packets more than 120 ms ahead resynchronise the playout point.

### ADR-209: Screen capture threading and platform dependencies

* Decision: the capture source is opened on the capture thread because `xcap` handles are
  not `Send` on Windows; `ScreenPublisher::start` waits for the open result over a channel.
  OpenH264 is built from source by `openh264-sys2` (no nasm needed; assembly is skipped if
  nasm is absent).
* Consequences: on Linux, `xcap` links `libxcb`, `libpipewire-0.3`, `libdbus` (via zbus,
  pure Rust) and needs `libclang` at build time for the PipeWire bindings. Wayland capture
  goes through the desktop portal and is best effort; X11 is the supported path.

### ADR-210: Engine start never fails on missing devices

* Decision: `MediaEngine::start` returns `Ok` whenever a Tokio runtime is present. Devices
  open in the background; failures are reported as `MediaEvent::Error` and the engine stays
  usable (receive-only without a microphone; without a speaker the mixer keeps draining
  peer buffers on its own 20 ms clock and discards the output).
* Rationale: a missing or busy microphone should not prevent joining a room and chatting.

### ADR-211: Synthetic test tone and the two-process harness

* Context: the brief asks for a two-process call test against the real server. The
  development machine's microphone delivers no samples, and CI-like environments (WSL)
  have no audio devices at all.
* Decision: `MediaDiagnostics::test_tone` replaces the microphone with a sine tone
  produced by a generator thread that writes 10 ms blocks into the same capture ring at
  real-time pace, so the encoder thread, mute, noise suppression and Opus run unchanged.
  The tone has a 3 Hz amplitude envelope: a perfectly steady sine is classified as
  stationary background by Opus, which then switches to DTX (comfort noise) within seconds,
  so a peer joining later only ever received comfort noise from earlier members. The
  option lives in a separate `MediaDiagnostics` struct so `MediaConfig` stays
  source-compatible. `examples/signal_call.rs` is a headless participant mirroring the
  client's session logic, and `tests/two_process.rs` (ignored by default) runs an
  in-process server plus N participant processes and requires every participant to hear
  every other one for at least half the call.
* Consequences: end-to-end audio is verified without devices, on any OS.

### ADR-212: Waking the webrtc 0.21 driver explicitly; send only when connected

* Context: in webrtc 0.21.1, `add_track`, `remove_track` and `add_ice_candidate` (UDP)
  change the sans-IO core without notifying the background driver. The driver keeps
  sleeping on its previous timeout (up to a day), so `negotiationneeded` is never delivered
  and connectivity checks for trickled candidates never start. Until now the engine only
  worked because the audio pump wrote a microphone frame every 20 ms, which wakes the
  driver as a side effect. With a microphone that delivers no frames (or none at all) a
  call never connected, and every pre-connection write logged an SRTP warning.
* Decision: `PeerManager` wakes the driver explicitly by writing one Opus silence frame to
  that peer's audio sender after track changes and after remote candidates are added.
  Send targets are published only for peers whose connection is `Connected` (kept through
  a transient `Disconnected`), so media is no longer pushed into connections that cannot
  encrypt it yet. Regression test: `connects_without_any_media_written` in
  `tests/peer_loopback.rs` fails without the wake.
* Consequences: connection setup no longer depends on microphone activity. A wake before
  DTLS completes still logs one webrtc warning per wake (bounded by the number of
  candidates) instead of one per 20 ms. Remove the workaround once webrtc wakes its driver
  on these calls.

## Desktop client (ADR-300 series)

### ADR-300: Backend on its own runtime thread, pure session logic

* Context: `update()` must never block, and the UI must not touch webrtc or cpal.
* Decision: a dedicated OS thread runs a 2-worker Tokio runtime with one Backend task.
  The UI sends `UiCommand`s with `try_send` (bounded 64) and receives `AppEvent`s
  (bounded 256) through a single iced subscription that takes ownership of the
  receiver once. Protocol choreography lives in `net::session::Session`, a pure state
  machine returning effects, so it is unit tested without sockets.
* Consequences: if the backend queue is full a UI action is dropped with a warning
  instead of freezing the window.

### ADR-301: One signaling task per connection attempt, tagged by generation

* Context: reconnects can overlap with late events from a dead socket.
* Decision: each attempt spawns a task tagged with an increasing generation; the backend
  ignores events whose generation is not current. A 404 on the upgrade means the room is
  gone (back to the lobby); every other failure schedules a backoff retry
  (1, 2, 4, 8, 16, 30, 30 ... seconds). TURN credentials are refetched before each retry.

### ADR-302: ring as the only TLS crypto provider

* Context: `reqwest`'s `rustls` feature pulls in aws-lc-rs, which complicates cross
  builds; webrtc and the server already use ring.
* Decision: `reqwest` with `rustls-no-provider`, `tokio-tungstenite` with
  `rustls-tls-webpki-roots`, and `rustls` with `ring`. `main` installs the ring provider
  as the process default. `cargo tree -i aws-lc-rs` finds nothing.
* Consequences: HTTPS uses the OS trust store (rustls-platform-verifier); WSS uses the
  bundled webpki roots. Self-signed server certificates are not supported.

### ADR-303: Minimal iced features

* Decision: `default-features = false` with `tokio`, `image-without-codecs` (frames
  arrive as raw RGBA, so no image decoders), `wgpu`, `tiny-skia`, plus `x11` on Linux
  only. No `wayland`, `web-colors`, `linux-theme-detection`, `canvas`, `advanced`.
  Antialiasing (MSAA) is disabled because nothing is tessellated.

### ADR-304: `--software-render` and the Windows renderer default via environment

* Context: iced 0.14 selects its compositor only through `ICED_BACKEND`, and wgpu's
  backend through `WGPU_BACKEND`. There is no programmatic setting.
* Decision: `main` sets `ICED_BACKEND=tiny-skia` for `--software-render`; otherwise on
  Windows it sets `WGPU_BACKEND=dx12` unless the user already set it. Both happen before
  any thread starts, which is what makes `std::env::set_var` sound.
* Measured idle memory (release, connect screen, AMD Radeon 780M, Windows 11):
  DX12 82 MB private / 102 MB working set; Vulkan about 125 MB private; tiny-skia 15 MB
  private / 29 MB working set. Most of the GPU figure is driver allocations.
* Consequences: the 80 MB idle target holds with `--software-render` on this machine;
  with DX12 it is just above. Users can override the backend.

### ADR-305: Programmatic icon embedded via embed-resource

* Decision: `src/icon.rs` rasterizes an original crab silhouette (body ellipse, two open
  claws, eye stalks, three jointed legs per side) with 4x4 supersampling. `build.rs`
  includes the same file, writes a multi-size `.ico` (16 to 256 px, 32-bit BMP entries)
  to `OUT_DIR` and compiles a one-line `.rc` with `embed-resource`. A missing resource
  compiler is a warning, not an error.
* Consequences: verified that both the MSVC and the zig-built GNU executables contain
  the `.rsrc` section (305 KB). The 256 px BMP entry dominates; a PNG entry would be
  smaller but needs an encoder, so it was not worth a dependency.

### ADR-306: Room screen opens on `welcome` for created rooms

* Decision: "Join" opens the room screen immediately with a "Joining..." header. "Create
  and join" stays in the lobby until the server's `welcome` arrives, then opens the room.

### ADR-307: Noise suppression toggle

* Decision: `noise_suppression` (default `false`, `serde(default)` so old files load) is
  passed in `MediaConfig` at engine start and toggled live with
  `MediaCommand::SetNoiseSuppression`, from the connect screen or the room bottom bar.
  The config is saved on every change.

## Linux and deployment (ADR-400 series)

### ADR-400: WSL Ubuntu 24.04 with systemd as the Linux verification host

* Context: the development machine runs Windows; the brief requires Linux builds,
  `systemd-analyze verify` and Linux memory measurements.
* Decision: install `Ubuntu-24.04` with `wsl --install --no-launch`, enable systemd in
  `/etc/wsl.conf`, default user root. `deploy/wsl-verify.sh` clones the repository into
  the WSL filesystem (`~/chattercrab-verify`) because builds on `/mnt/<drive>` are slow.
  Windows git worktrees cannot be read by git inside WSL (their `.git` file holds a
  drive-letter path), so the script falls back to copying the tree with tar.
* Consequences: verification is reproducible with one command from Git Bash
  (`MSYS_NO_PATHCONV=1` is required, otherwise Git Bash rewrites `/mnt/...` paths).

### ADR-401: install.sh owns /etc/turnserver.conf, never the env file

* Context: the installer must be idempotent, keep the TURN secret identical in two files,
  and not destroy operator edits.
* Decision: the first run backs up the distribution `/etc/turnserver.conf` to
  `/etc/turnserver.conf.orig` and installs `deploy/turnserver.conf` with a
  `# Managed by Chattercrab install.sh` marker. Later runs only rewrite
  `static-auth-secret`, `realm` and `external-ip` in place. `/etc/chattercrab/env` is
  created once and afterwards only its `CHATTERCRAB_TURN_SECRET` is filled in when unset
  or still the placeholder. The secret is always copied from env into Coturn, so an
  operator-chosen secret also propagates.
* Consequences: rerunning the installer after editing the env file resyncs Coturn.
  Superseded for deployment by ADR-506.

### ADR-402: NAT detection

* Decision: discover the public IPv4 from HTTPS echo services (ipify, ifconfig.me,
  icanhazip), falling back to a STUN binding request with `turnutils_stunclient`.
  If that address is bound to a local interface there is no NAT and any `external-ip`
  line is removed; otherwise `external-ip=PUBLIC/PRIVATE` is set with the private source
  address of the default route. `--public-ip` overrides discovery; if discovery fails the
  setting is left unchanged with a warning.

### ADR-403: build-release.sh builder selection and static check

* Decision: the host's own target is always built with plain `cargo` (it links the host
  system libraries the Linux client needs); foreign targets use `cargo zigbuild`, then
  `cross`, then plain `cargo` with a warning. `CHATTERCRAB_BUILDER` forces a builder.
  On a non-Linux host the Linux client is skipped unless
  `CHATTERCRAB_FORCE_LINUX_CLIENT=1`, because it needs ALSA/X11 development libraries.
  Static linking is verified by parsing the ELF program headers with `od` and failing
  on a `PT_INTERP` header, which works without `file`/`ldd` (Git Bash); `file` and `ldd`
  output is printed when available. `--locked` is used when `Cargo.lock` exists.
* Consequences: verified on WSL Ubuntu 24.04 and on Windows Git Bash (musl server via
  zigbuild with zig from the PyPI `ziglang` package). `dist/server/` also contains the
  deploy assets so `install.sh` runs from the copied directory.

### ADR-404: Coturn drop-in

* Decision: `coturn-override.conf` raises `LimitNOFILE`, orders after
  `network-online.target`, restarts on failure, and sets
  `SupplementaryGroups=chattercrab` in addition to adding `turnserver` to the group
  (the drop-in takes effect even if group membership was changed while Coturn ran).

### ADR-405: Services are not started without a certificate

* Context: with `CHATTERCRAB_TLS_CERT` set and no file, the signaling server would fail
  and restart in a loop.
* Decision: `install.sh` installs and enables everything but skips the (re)start and
  prints the certbot commands when the configured certificate is missing.

### ADR-406: Certificate deploy hook writes atomically

* Decision: the certbot hook copies `fullchain.pem`/`privkey.pem` to temporary files in
  `/etc/chattercrab/tls/` (`root:chattercrab`, 0640) and renames them into place, then
  reloads `chattercrab-signal` (SIGHUP hot swap) only if active and restarts Coturn only
  if enabled. Without `RENEWED_LINEAGE` it uses the single lineage under
  `/etc/letsencrypt/live`, so it can also be run by hand.

### ADR-407: Firewall ordering

* Decision: `firewall.sh` sets default deny incoming and allows SSH before `ufw --force
  enable`, so enabling never cuts the current SSH session. The signal port is a flag
  (`--signal-port`, default 8443) so the 443 variant needs no script edits. It has a
  `--dry-run` mode; it was not executed for real inside WSL.

### ADR-408: Distributable Windows client is built with cargo zigbuild

* Context: OpenH264 is C++. With the mingw-w64 toolchain, rustc links the C++ runtime
  requested by `openh264-sys2` as a dynamic library, so a plain
  `cargo build --target x86_64-pc-windows-gnu` imports `libstdc++-6.dll`. Adding
  `-C link-arg=-static` does not change that.
* Decision: `build-release.sh` produces the Windows client with `cargo zigbuild`, which
  links the C++ runtime statically; the binary imports only system DLLs and the UCRT
  (built into Windows 10+). The plain mingw build is supported for development and
  documented as needing `libstdc++-6.dll` alongside the executable.
* Consequences: no extra linker overrides that could break the zigbuild path.

## Noise suppression (ADR-500 series)

### ADR-500: RNNoise via `nnnoiseless` for optional microphone noise suppression

* Context: users asked for an optional "Noise suppression" checkbox that removes
  background noise (fans, hiss, rumble, keyboard) and keeps the speaker's voice,
  without much CPU. The project is Rust-only and prefers pure-Rust or vendored builds
  for cross compilation (musl server, Linux and windows-gnu clients).
* Decision: use `nnnoiseless` 0.5.2 (BSD-3-Clause), a pure-Rust port of Xiph's RNNoise,
  with `default-features = false` (no CLI, no `dasp`). It is wrapped in
  `chattercrab_media::denoise::Denoiser`, which processes one 20 ms, 48 kHz mono frame
  in place, between resampling and Opus encoding in the encoder thread.
* Why RNNoise:
  * Pure Rust, no C or C++ toolchain, so it cross-builds wherever the rest of the
    workspace does.
  * Native 48 kHz with 10 ms blocks: one 20 ms Opus frame is exactly two blocks, so no
    resampling or buffering is needed.
  * Low cost: about 80 microseconds per 20 ms frame in release builds on the
    development machine (about 0.4% of one core), and a model of about 88 KB of
    static weights compiled into the binary.
  * A trained speech/noise model: measured 19 to 53 dB of attenuation on fan-like and
    rumble-like noise, about 37 dB on hiss after speech, while a voiced test signal
    keeps its energy within about 1 dB.
* Alternatives rejected:
  * WebRTC Audio Processing Module (noise suppression, AGC, AEC): better feature set,
    but a large C++ build (Meson or vendored sources), which complicates the musl and
    windows-gnu cross builds and breaks the pure-Rust preference.
  * Spectral subtraction or Wiener filtering written by hand: no dependency, but
    noticeably lower quality, with "musical noise" artifacts and poor handling of
    non-stationary noise such as keyboard clicks.
* Consequences:
  * Enabling suppression adds 10 ms of latency (RNNoise overlap-add delay). Capture to
    encode stays within the 25 ms budget: one 20 ms frame of buffering plus the
    denoiser's sub-millisecond processing; the 10 ms is algorithmic delay in the
    signal, not processing time.
  * Switching on repeats 10 ms of audio. The first block after a start or reset is
    passed through dry and the second is an equal-power crossfade into the denoised
    signal, so the splice is smooth.
  * Flat white noise with no speech ever heard is only attenuated by 1 to 2 dB
    (broadband flat noise looks partly speech-like to the network). Real background
    noise is colored and is removed strongly, as is white noise once speech has been
    present.
  * RNNoise does not separate voices: other people talking in the room are kept.

### ADR-501: Allocation-free reset by swapping a preallocated state

* Context: `Denoiser::reset` is called on the audio encoder thread when suppression is
  switched on (and should be called after any gap in the frame sequence, such as
  unmuting). `nnnoiseless::DenoiseState` holds heap buffers, has no in-place reset, and
  cloning it allocates. FFT plans are cached per thread on first use.
* Decision: `Denoiser::new` allocates an active state and a fresh spare state, and runs
  one silent block through a throwaway state to fill the calling thread's FFT plan
  caches. The first `reset` swaps the spare in (exact reset); the used state moves to
  the spare slot instead of being freed. Later resets keep the current state and only
  restart the dry-then-crossfade handling: the discarded first block hides the stale
  overlap-add tail, and the recurrent state re-adapts within about 100 ms.
* Consequences: `process` and `reset` never allocate or free when the denoiser was
  constructed on the calling thread. This was verified with a counting global
  allocator over 200 frames and 4 resets (zero allocation or free calls). Memory is
  two RNNoise states, well under 100 KB. Rebuilding the audio stream creates a new
  denoiser and restores exact resets.

### ADR-502: dtln-rs evaluated and not adopted

* Context: the user suggested DataDog's `dtln-rs` (<https://github.com/DataDog/dtln-rs>),
  a Rust wrapper around the DTLN (Dual-signal Transformation LSTM Network) noise
  suppression models, as an alternative to RNNoise.
* Findings:
  * It does not build for our targets. `build.rs` is a no-op on Windows, so TensorFlow
    Lite is never linked, and there is no Linux branch. Prebuilt TensorFlow Lite is
    provided only for macOS and wasm32.
  * It depends on the TensorFlow Lite C++ runtime through `cxx`, plus CMake and Conan
    off macOS. That violates the Rust-only, pure-Rust or vendored build constraint and
    complicates the musl and windows-gnu cross builds. It also pulls in `neon`
    (Node.js bindings).
  * It processes 16 kHz audio with a 512-sample window and a 128-sample hop (32 ms
    window, 8 ms hop). That would require a 48 kHz to 16 kHz to 48 kHz resampling round
    trip, discard everything above 8 kHz (fullband Opus voice gets duller), add more
    latency than the 25 ms capture-to-encode budget allows, and run two LSTM models
    every 8 ms, which costs several times RNNoise's CPU.
  * It is not published on crates.io, has 8 commits (last on 2025-03-21), and its
    README is partly templated.
* Decision: stay with RNNoise via `nnnoiseless` (ADR-500).
* Possible follow-up: if higher suppression quality is ever needed, run the DTLN
  `.tflite` or ONNX models through a pure-Rust inference engine (for example `tract`)
  behind the same `Denoiser` interface. The interface (one 20 ms, 48 kHz frame in
  place, plus `reset`) is deliberately engine-agnostic so that swap would not touch
  the encoder thread or the client.

### ADR-503: Microphone Opus bitrate is 96 kbps

Context: voice was encoded at 32 kbps (ADR-206). The operator wants higher mic quality.
Decision: `OPUS_BITRATE` is 96_000. Application stays VoIP, mono 48 kHz, 20 ms, DTX, in-band FEC, complexity 5.
Consequences: each upstream audio stream is about 96 kbps. A full mesh of N peers costs `(N - 1) x 96 kbps` upstream for audio. Screen share is unchanged. ADR-206 remains the historical record of the original settings; this ADR supersedes only the bitrate.

### ADR-504: External STUN URL, no TURN

Context: ADR-006 derived `stun:` URLs from the operator's Coturn `turn:` URLs and the server minted TURN REST credentials. The operator does not want a TURN relay.
Decision: `CHATTERCRAB_STUN_URL` (default `stun:stun.l.google.com:19302`) is the only ICE server. `GET /ice-servers` returns `{ "urls": [that url] }`. `turn:` and `turns:` URLs are rejected. TURN REST, HMAC-SHA1 credentials, and `ErrorCode::TurnUnavailable` are removed.
Consequences: peers behind symmetric NAT may fail to connect. The signaling server no longer depends on `hmac` or `sha1`. ADR-505 brings `base64` back only to decode Basic credentials. ADR-006 is historical and superseded. Coturn deploy files are removed by ADR-506.

### ADR-505: Shared password via HTTP Basic

Context: the signaling server was open to anyone who could reach it. The operator wants a password, as part of config, enough to stop strangers connecting.
Decision: `CHATTERCRAB_PASSWORD` (empty means off) and `CHATTERCRAB_AUTH_USER` (default `chattercrab`). When the password is set, all routes except `GET /health` and CORS preflight require HTTP Basic. The client stores `auth_user` and `password` in `config.toml` and sends them on REST and on the WebSocket upgrade. Comparison is constant-time. Failures are a single `401` / `unauthorized`.
Consequences: the password sits in the server environment and in the client config file in plaintext. `GET /health` does not reveal rooms. Peers still exchange media directly; the password only guards signaling.

### ADR-506: Docker Compose and Traefik, no TURN

Context: deployment was a static binary plus Coturn under systemd (ADR-401 and the surrounding deploy ADRs). The operator wants a Docker image on a VPS, Compose, and Traefik, and does not want TURN.
Decision: Dockerfile builds chattercrab-signal and serves plain HTTP on 8080 as user chattercrab. docker-compose.yml runs Traefik v3 (ports 80 and 443, Let's Encrypt HTTP-01) and the server with no published port. CHATTERCRAB_PASSWORD is required by the compose file. CHATTERCRAB_STUN_URL defaults to stun:stun.l.google.com:19302. Coturn, the systemd unit, and install.sh are removed.
Consequences: TLS and certificates are Traefik's job. The Rust server does not see client certificates. Media is not relayed. The password is in the compose environment (visible to anyone who can inspect the container). ADR-401's installer is no longer the supported path.
