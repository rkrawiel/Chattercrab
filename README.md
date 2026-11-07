# Chattercrab

A minimalist peer-to-peer voice, text and screen-share application, written entirely in
Rust. Rooms hold up to 5 people connected in a full WebRTC mesh; a small signaling server
coordinates rooms and relays chat, but never touches audio or video. The server is still
a Rust binary. The VPS deploy is Docker Compose with Traefik. Media is direct WebRTC
with an external STUN URL, and there is no TURN relay. A shared password, set in the
server environment and the client config, can gate signaling. No JavaScript, no webviews.

- Voice: Opus 96 kbps with DTX and in-band FEC, adaptive jitter buffer, real-time mixer,
  per-peer volume, mute and deafen, optional RNNoise noise suppression.
- Text: room chat with the last 200 messages kept in server memory only.
- Screen share: one publisher per room (server-enforced), hardware H.264 at 1440p 60 fps, or 1080p 30 fps.
- Clients: Windows 10+ and Linux (X11).

| Document                                     | Contents                                                     |
| -------------------------------------------- | ------------------------------------------------------------ |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | Components, threading model, message flows, media pipeline   |
| [docs/PROTOCOL.md](docs/PROTOCOL.md)         | Every REST endpoint and WebSocket message with JSON examples |
| [docs/DEPLOY.md](docs/DEPLOY.md)             | VPS setup: Docker Compose, Traefik, DNS, firewall, upgrades  |
| [docs/DECISIONS.md](docs/DECISIONS.md)       | Decision log (ADR style)                                     |
| [docs/ACCEPTANCE.md](docs/ACCEPTANCE.md)     | Requirement-by-requirement evidence                          |

## Quick start (local, no TLS, no TURN)

```sh
# Terminal 1: signaling server on plain HTTP
CHATTERCRAB_BIND=127.0.0.1:8443 cargo run -p chattercrab-server --bin chattercrab-signal

# Terminal 2 and 3: two clients; enter http://127.0.0.1:8443 as the server URL
cargo run --release -p chattercrab-client
```

## Workspace

```text
crates/protocol   chattercrab-protocol   shared wire types, no I/O
crates/server     chattercrab-server     binary chattercrab-signal
crates/media      chattercrab-media      audio, video, WebRTC peer management
crates/client     chattercrab-client     binary chattercrab (iced desktop app)
deploy/           release build and Linux verification scripts
docs/             architecture, protocol, deployment, decisions, acceptance
```

### chattercrab-protocol

Serde types shared by the client and server: validated newtypes (`RoomId`, `PeerId`
(UUID v4), `DisplayName` (1 to 32 characters), `RoomName`), REST bodies (`RoomSummary`,
`CreateRoomRequest`, `IceServers`, `ApiError`) and the WebSocket enums `ClientMsg` and
`ServerMsg`, tagged with `"t"` in snake_case. No I/O; every variant has a JSON round-trip
test. Specification: [docs/PROTOCOL.md](docs/PROTOCOL.md).

### chattercrab-server

The signaling server `chattercrab-signal`: axum over rustls (ring), per-room locks in a
`DashMap`, bounded per-connection queues with slow-consumer eviction, WebSocket heartbeat,
5 second join timeout, screen-share arbitration, chat history, idle-room sweeper, per-IP
rate limits, one configured STUN URL via `GET /ice-servers`, TLS reload on `SIGHUP` and graceful
shutdown on `SIGTERM`. Configured entirely through `CHATTERCRAB_*` environment variables.
Builds as a fully static musl binary (about 3.9 MB) with about 3 MB idle RSS.
Details: [crates/server/README.md](crates/server/README.md).

### chattercrab-media

The client-side media engine, driven only through `MediaCommand` in and `MediaEvent` out
over bounded channels. Modules: `audio` (cpal capture, rubato resampling, 20 ms Opus
frames through a lock-free ring), `denoise` (RNNoise via nnnoiseless, about 0.4% of a
core), `jitter` (adaptive 40 to 120 ms, FEC and PLC), `mixer` (real-time thread, zero
allocations, soft-knee limiter), `screen` (xcap capture, OpenH264 encode/decode) and
`peer` (webrtc-rs peer connections with perfect negotiation).
Details: [crates/media/README.md](crates/media/README.md).

### chattercrab-client

The desktop app `chattercrab`, built with iced. Connect, Lobby and Room screens; member
list with speaking indicators and per-peer volume; chat; mute, deafen, noise suppression,
screen share; Ctrl+M and Ctrl+D shortcuts; automatic reconnect with backoff. The UI talks
to a backend running on its own Tokio runtime thread and never calls media APIs directly.
Config lives in `chattercrab/config.toml` under the platform config directory; chat is
never written to disk. `--software-render` uses the tiny-skia renderer for machines without
a usable GPU. Details: [crates/client/README.md](crates/client/README.md).

## Building

Requirements on every platform: Rust stable (pinned by `rust-toolchain.toml`), a C/C++
compiler and CMake (libopus and OpenH264 are compiled from source).

```sh
cargo build --release --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
cargo deny check
```

- **Linux (Debian/Ubuntu)**: see the apt package list in
  [docs/DEPLOY.md section 12](docs/DEPLOY.md#12-building-on-linux-client-prerequisites).
- **Windows**: Visual Studio Build Tools (C++ workload and Windows SDK). CMake can come
  from `pip install --user cmake`.
- **Release artifacts**: `deploy/build-release.sh` cross-compiles the static musl server
  and the Linux and Windows clients into `dist/` with `cargo zigbuild` and writes
  `dist/SHA256SUMS`.

Development on Windows used CMake, Zig and Ninja from PyPI (`pip install --user cmake
ziglang ninja`) and `cargo install --locked cargo-zigbuild cargo-deny`, plus WSL Ubuntu
24.04 for Linux verification (`deploy/wsl-verify.sh`).

## License

MIT. Third-party licenses are checked with `cargo deny check licenses` (see `deny.toml`).
