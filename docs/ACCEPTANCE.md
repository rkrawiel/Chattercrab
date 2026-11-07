# Acceptance Checklist

Each requirement from the brief, mapped to evidence. Status values:

* **PASS**: verified as stated.
* **PARTIAL**: verified with a documented gap.
* **OPEN**: not verified on the development machines; reason given.

Environments used:

* **Windows**: Windows 11 Pro, MSVC host, Rust 1.99.1, AMD Radeon 780M iGPU.
* **WSL**: WSL Ubuntu 24.04 with systemd (`deploy/wsl-verify.sh`), also used as the Linux X11 desktop through WSLg.

Commit verified: `main` at the time of writing (see `git log`).

## 1. Quality bar

| Requirement | Status | Evidence |
|-------------|--------|----------|
| `cargo build --release --workspace` succeeds on Linux | PASS | `deploy/wsl-verify.sh` in WSL: step "cargo build --release --workspace", exit 0. |
| `cargo build --release -p chattercrab-client --target x86_64-pc-windows-gnu` succeeds | PASS | Ran literally in WSL with the mingw-w64 linker from `.cargo/config.toml`: 20,084,736 bytes. That binary needs `libstdc++-6.dll` next to it; the distributable comes from `cargo zigbuild` (19.7 MB) and imports only system DLLs plus the UCRT (ADR-408). The zigbuild path also verified from the Windows host. |
| `cargo clippy --workspace --all-targets -- -D warnings` clean | PASS | Clean on Windows and in WSL. Workspace lints in `Cargo.toml`: `missing_docs`, `clippy::unwrap_used`, `clippy::expect_used`, `dbg_macro`, `todo`. |
| `cargo fmt --check` clean | PASS | Windows and WSL. |
| `cargo deny check` clean | PASS | `advisories ok, bans ok, licenses ok, sources ok`. `deny.toml` has an explicit license allow-list, and six transitive "unmaintained" notices are ignored with written reasons; none is a vulnerability. |
| `cargo test --workspace` passes, including server integration tests | PASS | 134 passed and 1 ignored, on Windows and in WSL. By crate: protocol 17; server 31 (8 unit + 9 REST + 12 signaling + 2 TLS); media 57 (53 unit + 2 engine smoke + 2 peer loopback); client 29. The ignored test is the two-process call harness (section 5), run separately. |
| No `unwrap`/`expect` outside tests and startup | PASS | Enforced by `clippy::unwrap_used`/`expect_used` with `-D warnings`. `clippy.toml` allows them only inside tests. No crate-level `allow` overrides outside `tests/` and `examples/`. |
| Libraries use `thiserror`, binaries use `anyhow` | PASS | `chattercrab-protocol::ValidationError`, `chattercrab-media::MediaError`, `chattercrab-server` `CreateRoomError`/`JoinError`. `anyhow` in `crates/server/src/main.rs` and `crates/client/src/main.rs`. |
| Every public item has a doc comment | PASS | `missing_docs = "warn"` in workspace lints, denied by clippy `-D warnings`. |
| Root README has a section per crate | PASS | `README.md`, "Workspace" section. |
| No em dashes in code comments or docs | PASS | No U+2014 anywhere in the repository. |

## 2. Memory and latency

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Server idle RSS under 10 MB | PASS | Linux musl release, plain HTTP, 5 rooms: `VmRSS 2688 kB` (`/proc/<pid>/status`, measured by `deploy/wsl-verify.sh`). Windows release: 4.3 MB private. |
| Client idle under 80 MB | PARTIAL | Windows release, Connect screen, measured 8 s after launch via `Process.PrivateMemorySize64`: software renderer (`--software-render`, tiny-skia) 15.4 MB private / 31 MB working set; GPU renderer (DX12, the default) 86.5 MB private. The GPU figure is dominated by D3D12 driver allocations on the AMD 780M; Vulkan was 129 MB and GL 190 MB. The media engine in a 5-person call adds about 10 MB (headless participants, below). Linux under WSLg: software renderer 21 MB RSS; GPU 163 MB through WSL's Mesa D3D12 translation layer (not representative of native drivers). |
| No growth over a 1-hour soak with 5 peers | PARTIAL | Soak stopped at 41 minutes at the user's request; data below. |
| Audio capture-to-encode under 25 ms | PASS (synthetic input) | Measured in the encoder thread as the age of the newest sample when its Opus frame is finished (time since the last input callback plus audio still queued in the ring and accumulator); see `crates/media/src/audio/capture.rs`. Synthetic tone input during the soak: about 1.6 ms average and 2.9 to 13 ms max over 5 participants, apart from one machine-wide stall (below). The development machine's microphone delivers no samples to cpal, so the hardware input path is untested here. |
| Mixer frame budget 20 ms, warning when exceeded | PASS | `crates/media/src/mixer.rs` counts iterations over 20 ms and logs a rate-limited warning. Loopback example, release, `--tone`, output resampled 48 to 96 kHz over 8 channels: longest iteration 0.33 ms, 0 overruns, 0 underruns. |

### Soak run (41 minutes, Windows release builds)

Setup:

* One `chattercrab-signal` on 127.0.0.1 and five `signal_call` participants in one room, each with its own tone (300 to 740 Hz).
* That is a full mesh with 4 peer connections per participant.
* Private memory sampled every minute with `Get-Process`.

Values in MB (private bytes):

| Process | Minute 0 | Minute 5 | Minute 41 | Max | Working set at 41 |
|---------|---------:|---------:|----------:|----:|------------------:|
| server | 4.39 | 4.30 | 4.30 | 4.39 | 11.24 |
| p0 | 9.81 | 9.82 | 9.84 | 9.89 | 23.30 |
| p1 | 9.94 | 9.93 | 9.93 | 9.99 | 23.22 |
| p2 | 10.20 | 10.16 | 10.22 | 10.22 | 23.52 |
| p3 | 10.30 | 10.29 | 10.27 | 10.34 | 23.59 |
| p4 | 9.86 | 9.86 | 9.88 | 9.91 | 23.34 |

* **Memory:** no growth; every process stayed within 0.1 MB of its minute-5 value.
* **Drops:** zero dropped encoded frames and zero capture-ring overflows in every 10-second stats interval (1205 intervals).
* **Latency spike:** at 12:25 UTC all five processes recorded a single capture-to-encode maximum of 89 to 102 ms, at the moment a full WSL release build started on the same machine. Interval averages before and after stayed at about 1.6 ms. This was host CPU starvation, not a pipeline fault.
* **Client UI not covered:** the 41-minute soak used headless participants running the real `MediaEngine`, not the iced UI.

The 90-second rehearsal of the same setup ended with every participant `peers_connected=4 ok=true`, each peer heard for 85.5 to 86.9 s.

## 3. Protocol crate

| Requirement | Status | Evidence |
|-------------|--------|----------|
| `RoomId`, `PeerId` (UUID v4), `DisplayName` (validated, 1 to 32 chars, trimmed) | PASS | `crates/protocol/src/ids.rs`; tests `display_name_rules`, `room_id_rules`, `peer_id_order_matches_string_order`. |
| REST models `RoomSummary`, `CreateRoomRequest`, `IceServers` | PASS | `crates/protocol/src/rest.rs`. `IceServers` is `{ "urls": ["stun:…"] }` with no username or credential (ADR-504). |
| WS enums, `#[serde(tag = "t", rename_all = "snake_case")]`, all listed variants | PASS | `crates/protocol/src/ws.rs`. `Chat` text limit enforced by `validate_chat_text` (2000 characters). |
| Round-trip tests for every variant | PASS | `every_client_msg_roundtrips`, `every_server_msg_roundtrips`, `tag_field_and_snake_case_names`, `exact_wire_format`. |

## 4. Signaling server

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Stack: tokio, axum `ws`, axum-server rustls, tower-http (cors, trace, limit), dashmap, tracing JSON | PASS | `crates/server/Cargo.toml`. axum-server uses `tls-rustls-no-provider` with ring instead of aws-lc (ADR-100). No `hmac` or `sha1` (ADR-504). `base64` decodes HTTP Basic credentials (ADR-505). |
| Env config with the listed variables and defaults | PASS | `crates/server/src/config.rs`; container overrides in `docker-compose.yml` and `.env.example`. |
| `Arc<AppState>` with `DashMap<RoomId, Room>`, per-room tokio Mutex, no global lock | PASS | `crates/server/src/state.rs` (`Arc<Room>` values, `Mutex<RoomInner>`). |
| Routes `/health`, `/rooms` GET and POST (rate limited, max rooms), `/ice-servers`, `/ws/{room_id}` | PASS | `crates/server/src/http.rs`; tests `health_reports_ok`, `create_and_list_rooms`, `create_room_rate_limited`, `max_rooms_enforced`, `ice_servers_returns_configured_stun_url`, `ice_servers_reflects_custom_stun_url`, `ice_servers_rate_limited`, `ws_to_unknown_room_is_404`. `GET /turn-credentials` is not served. |
| `GET /ice-servers` returns the configured STUN URL and no credential | PASS | Default `stun:stun.l.google.com:19302` (`CHATTERCRAB_STUN_URL`). Response is `{ "urls": ["<that url>"] }` with no username or credential. `turn:` and `turns:` URLs are rejected at config load. No TURN relay. |
| Optional shared Basic auth | PASS | `CHATTERCRAB_PASSWORD` empty or unset leaves the server open. When set, `GET /health` and CORS `OPTIONS` stay open and every other route requires `Authorization: Basic` (`401` / `unauthorized`, `WWW-Authenticate: Basic realm="chattercrab"`). Comparison is constant-time. The client stores `auth_user` and `password` in `config.toml` (ADR-505). |
| Join within 5 s | PASS | `join_timeout_closes_socket`. |
| Room size enforced | PASS | `room_full_rejected`. |
| Outbound queue 256, slow consumer dropped | PASS | `OUTBOUND_QUEUE_CAPACITY = 256`; `slow_consumer_is_kicked`. |
| Ping every 20 s, close after 2 missed pongs | PASS | `config.ping_interval`; `heartbeat_drops_unresponsive_peer`. |
| Disconnect cleanup, screen lock release, room deleted when empty | PASS | `room_deleted_when_last_member_leaves`, `screen_lock_semantics`. |
| Screen arbitration | PASS | `screen_lock_semantics` covers grant, deny, stop, and release on publisher disconnect. |
| Idle sweeper every 60 s | PASS | `idle_empty_rooms_are_evicted`; only empty rooms are evicted (ADR-003). |
| TLS reload on SIGHUP without dropping connections | PASS | `serves_https_and_reloads_certificate`: after a reload the new certificate is served and the old one is not, and a broken key file keeps the current certificate. Existing connections are untouched by design, because `RustlsConfig::reload_from_config` swaps the config only for new handshakes; there is no dedicated test for that part. SIGHUP handler in `crates/server/src/main.rs`. |
| Graceful SIGTERM within 10 s | PASS | `graceful_shutdown_sends_going_away`; Linux release binary exited 4 ms after SIGTERM. |
| Integration tests with tokio-tungstenite on an ephemeral port | PASS | `crates/server/tests/` (29 tests). |
| Static musl binary | PASS | `file`: "statically linked"; `ldd`: "not a dynamic executable"; 3,891,000 bytes. |

## 5. Media crate

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Modules `audio`, `screen`, `peer`, `mixer`, `jitter` | PASS | `crates/media/src/`, plus `denoise`. |
| cpal capture, rubato to 48 kHz, 20 ms frames via SPSC ring, Opus VoIP 96 kbps, DTX, FEC, complexity 5 | PASS | `crates/media/src/audio/capture.rs` (ADR-206, ADR-207, ADR-503). |
| Device enumeration and selection by name; mute sends silence | PASS | `list_input_devices`/`list_output_devices`; `MediaCommand::SetMuted`. |
| Adaptive jitter buffer, target 40 ms, max 120 ms, FEC or PLC | PASS | `crates/media/src/jitter.rs`; 12 tests including reorder, FEC, PLC, late, overflow and wraparound. |
| Mixer on an RT-priority thread, i32 sum, soft-knee limiter, no allocation in the loop, per-peer volume 0 to 2, deafen | PASS | `crates/media/src/mixer.rs`; tests cover summing, silence, limiter bounds, volume, deafen and real Opus decode. |
| Screen: xcap, fit 1280x720, 15 fps (5 to 30), OpenH264 baseline about 1.5 Mbps, IDR every 2 s and on PLI/FIR; decoder to a drop-oldest queue of 2 | PARTIAL | Implemented in `crates/media/src/screen/` and `video_queue.rs`. H.264 exchange over real peer connections, with simultaneous renegotiation, is verified by `tests/peer_loopback.rs` on a synthetic source. Real desktop capture between two GUI clients was not exercised end to end. |
| PeerManager: perfect negotiation, shared Opus track, renegotiated screen track, trickle ICE, MaxBundle, rtcp-mux, ICE timeouts 5/15 s, UDP+TCP only | PASS (deviation) | `crates/media/src/peer/`. webrtc 0.21 binds a local track to one connection, so there is one track per connection and audio is encoded once (ADR-200, ADR-201). |
| Message-based API (`MediaCommand` / `MediaEvent`, bounded channels) | PASS | `crates/media/src/lib.rs`. |
| Tests: mixer, jitter, negotiation with fake signaling | PASS | 53 unit tests; negotiation includes glare in both orders, renegotiation and 200 randomized race schedules. |
| `cargo run -p chattercrab-media --example loopback` | PASS (synthetic input) | Runs with `--tone`: 30.5 kbit/s. In microphone mode it exits after 3 s with an explanation, because this machine's microphone delivers no samples. |
| Two-process local test against the server without TURN | PASS | `cargo test -p chattercrab-media --test two_process -- --ignored --nocapture`. Windows: both peers `ok=true`, heard 9.2 to 9.3 s of 10. WSL: both `ok=true`, heard 9.7 s. A 3-peer run passed on both. |
| Optional noise suppression (added on request) | PASS | `crates/media/src/denoise.rs` (RNNoise via nnnoiseless). 52.5 dB less colored noise, 1.1 dB loss on a voiced signal, about 80 µs per 20 ms frame (0.4% of a core), zero allocations per frame (ADR-500 to ADR-502). |

## 6. Client

| Requirement | Status | Evidence |
|-------------|--------|----------|
| iced Elm architecture, one Subscription bridging the backend channel, own Tokio runtime thread, no blocking in `update` | PASS | `crates/client/src/ui/mod.rs`, `crates/client/src/net/backend.rs` (ADR-300). |
| Connect, Lobby, Room screens as specified, including screen-share layout | PASS (manual) | `crates/client/src/ui/view.rs`. Window launch verified on Windows (GPU and software) and on Linux X11 (WSLg). Connect screen screenshot taken during development. |
| Config at `chattercrab/config.toml` in the platform config dir; chat not written to disk | PASS | `crates/client/src/config.rs`; round-trip tests. |
| Reconnect with exponential backoff 1 to 30 s | PASS | `crates/client/src/net/backoff.rs`; `schedule_doubles_then_caps`. |
| Ctrl+M, Ctrl+D, Enter | PASS | Keyboard subscription in `crates/client/src/ui/mod.rs`. |
| `--software-render` (tiny-skia) | PASS | `crates/client/src/main.rs`; measured above. |
| Windows subsystem in release, generated crab icon embedded, no downloaded assets | PASS | `#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]`; `crates/client/src/icon.rs` and `build.rs` (ADR-305). |
| Noise suppression checkbox in options | PASS | Connect screen and room bottom bar; persisted as `noise_suppression` (ADR-307). |

## 7. Deploy assets

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Docker Compose files at the repo root | PASS (files only) | `Dockerfile`, `.dockerignore`, `docker-compose.yml`, `.env.example`. The image was not built in this environment: `docker` is not installed, and Docker was not installed to run `docker compose config` or `docker build`. |
| Coturn installer and systemd unit removed | PASS | Deleted `deploy/install.sh`, `deploy/firewall.sh`, `deploy/certbot-deploy-hook.sh`, `deploy/coturn-override.conf`, `deploy/turnserver.conf`, `deploy/chattercrab-signal.service`, and `deploy/chattercrab.env.example`. `deploy/build-release.sh` no longer copies them. `deploy/wsl-verify.sh` no longer runs `systemd-analyze`. |
| `build-release.sh`: targets, zigbuild/cross/cargo, `dist/` layout, SHA256SUMS, sizes, static check | PASS (previous run) | Full run in WSL before this deploy change: server 3,891,000 B (static), Linux client 22,495,200 B, Windows client 19,658,240 B, `dist/SHA256SUMS` written. This phase did not re-run the release build. The script still fails when the server binary is not static, and it no longer copies Coturn or systemd assets. |
| Scripts lint | PASS | `bash -n` on `deploy/build-release.sh` and `deploy/wsl-verify.sh` after the Coturn removal. ShellCheck was not re-run here. |
| `docs/DEPLOY.md` | PASS | Rewritten for Compose and Traefik: DNS, Docker Engine, firewall 22/80/443, `.env`, `docker compose up -d --build`, client URL on port 443, health and 401 checks, logs, upgrade, external STUN, troubleshooting. No Coturn installer. |

## 8. Known gaps and follow-ups

1. **GPU idle memory:** the client renderer is 6.5 MB over the 80 MB target on the test iGPU (DX12 driver overhead). `--software-render` is at 15 MB. Possible fixes: default to tiny-skia when no screen share is shown, or lower wgpu limits once iced exposes them.
2. **Soak length:** the soak ran 41 of the 60 minutes. Rerun with `SOAK_SECS=3600` using the procedure in section 2 if a full hour is required.
3. **Hardware microphone latency:** not verified on real hardware here, because the development machine's microphone produces no samples. Run the loopback example on a machine with a working microphone; it prints capture-to-encode latency.
4. **Screen share end to end:** test real desktop capture between two GUI clients manually.
5. **Transceivers:** each screen share start/stop leaves an inactive transceiver on every connection, renegotiated in each later offer.
6. **IPv6:** only IPv4 host candidates are gathered.
7. **Self-signed servers:** the client does not accept self-signed server certificates.
8. **Wayland:** screen capture under Wayland/PipeWire is a documented follow-up; X11 is required.
9. **Linux real-time priority:** needs an `rtprio` limit; without it the mixer runs at normal priority and logs a warning.
