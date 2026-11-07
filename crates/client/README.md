# chattercrab-client

The Chattercrab desktop client (binary `chattercrab`) for Windows 10+ and Linux (X11).
A minimalist iced UI for peer-to-peer voice, text chat and screen sharing. The
signaling server only coordinates; audio and video flow directly between peers.

## Features

- **Connect screen**: server URL, user, password, display name, microphone and speaker selection,
  noise suppression, light/dark theme.
- **Lobby**: room list with member counts, create-and-join, join.
- **Room**:
  - member list with a speaking indicator (RMS above 0.02), per-peer volume
    (0 to 200 %) and local mute, and connection state (connecting, unstable, unreachable);
  - chat log with an input that sends on Enter (history lives in memory only and is
    never written to disk);
  - bottom bar: Mute, Deafen, Share screen / Stop sharing, Noise suppression, Leave;
  - while someone else shares, the chat shrinks to a side column and the stream is shown
    at its native aspect ratio. Only one person per room can share at a time; the button
    is disabled while the lock is taken.
- **Reconnect**: when the signaling connection drops, the client retries with
  exponential backoff (1, 2, 4, 8, 16, then every 30 seconds), rejoins the same room and
  rebuilds every peer connection from the new `welcome`. If the room no longer exists,
  it returns to the lobby with a notice.

## Keyboard shortcuts

| Keys   | Action                                                              |
| ------ | ------------------------------------------------------------------- |
| Ctrl+M | Mute / unmute microphone                                            |
| Ctrl+D | Deafen / undeafen                                                   |
| Enter  | Send chat message (also submits the connect form and room creation) |

## Command line

```text
chattercrab [--software-render] [--config <PATH>] [--help] [--version]
```

- `--software-render` uses the tiny-skia CPU renderer instead of wgpu. Use it on
  machines without a usable GPU, in VMs, or to minimize memory (about 15 MB private /
  30 MB working set at idle versus about 82 MB / 102 MB with DX12 on an AMD 780M).
  Internally it sets `ICED_BACKEND=tiny-skia`.
- On Windows the GPU renderer prefers DX12. Set `WGPU_BACKEND` (for example `vulkan`)
  to override it. If no GPU backend works, iced falls back to tiny-skia automatically.
- `RUST_LOG` controls logging (default `info`, with GPU and windowing crates at `warn`).
  Release builds on Windows have no console, so logs only show when started from a
  terminal with stderr redirected.

## Configuration

Stored as TOML in the platform config directory:

| Platform | Path                                                                                     |
| -------- | ---------------------------------------------------------------------------------------- |
| Windows  | `%APPDATA%\chattercrab\config.toml`                                                      |
| Linux    | `$XDG_CONFIG_HOME/chattercrab/config.toml` (usually `~/.config/chattercrab/config.toml`) |

```toml
server_url = "https://chat.example.com:8443"
auth_user = "chattercrab"                 # Basic user; default chattercrab
password = ""                             # empty sends no Authorization header
display_name = "ada"
input_device = "Microphone (USB Audio)"   # omit for the system default
output_device = "Speakers (Realtek)"      # omit for the system default
theme = "dark"                            # or "light"
screen_fps = 60                           # 5, 10, 15, 20, 30, 60, or 120 with high quality
screen_scale = "1440p"                    # or "1080p", "2k", "4k"
screen_content = "desktop"                # or "game"; independent of size and frame rate
high_quality = false                      # H.264 High, higher bitrate, low delay
noise_suppression = false                 # RNNoise on the microphone input
```

Every key is optional; missing keys take the defaults shown above (devices default to
the system default, `auth_user` defaults to `chattercrab`, `password` defaults to empty).
An empty password sends no `Authorization` header. When it is set, the client sends
HTTP Basic on every REST call and on the WebSocket upgrade. The password is stored in
this file in plaintext. Settings are saved on Connect, when the theme or noise suppression
is toggled, and when a share frame rate is chosen.

**Noise suppression** removes steady background noise (fans, keyboards, traffic) while
keeping voice, using RNNoise in the media engine. It can be set on the connect screen
and toggled live from the room's bottom bar.

## Architecture

```text
main thread                         runtime thread (Tokio, 2 workers)
-----------                         ---------------------------------
iced App { state, config, net_tx }  Backend task
  update(): try_send(UiCommand) --->  REST (reqwest + rustls/ring)
  Subscription <--- AppEvent -------  signaling task (tokio-tungstenite), reconnect timer
  Subscription <--- VideoFrameQueue   MediaEngine (chattercrab-media) commands/events
```

- `src/net/session.rs` is a pure state machine mapping server messages and media events
  to effects (media commands, UI events, outgoing signaling). It holds the protocol
  choreography: `welcome` -> `Configure` + `AddPeer{initiate: true}` per peer,
  `peer_joined` -> `AddPeer{initiate: false}`, offer/answer/ICE relay, screen-share lock.
- `src/net/backend.rs` owns the REST client, the media engine and the signaling task,
  and applies those effects. Stale connections are ignored by generation number.
- `src/ui/room_model.rs` reduces events into what the room screen displays.
- `src/icon.rs` draws the crab silhouette in code; `build.rs` encodes it as `.ico` and
  embeds it into Windows executables. The window icon uses the same generator.

## Building

```sh
cargo build --release -p chattercrab-client
```

### Windows (MSVC host)

Needs Visual Studio Build Tools (C/C++ and the Windows SDK, which provides `rc.exe` for
the icon) plus CMake on `PATH` for libopus (`pip install --user cmake`).

### Windows GNU target (cross build)

```sh
pip install --user ziglang cmake
cargo install --locked cargo-zigbuild
cargo zigbuild --release -p chattercrab-client --target x86_64-pc-windows-gnu
```

Verified on a Windows MSVC host: the binary imports only system DLLs and carries the
icon resource. The icon is compiled by whichever resource compiler `embed-resource`
finds: on a Windows host that is the SDK's `rc.exe`; on a Linux host it needs
`x86_64-w64-mingw32-windres` (package `binutils-mingw-w64-x86-64`) or `llvm-rc`. When
none is available the build prints a warning and the executable just has no embedded
icon (the window icon still works).

### Linux

X11 is required. Build dependencies on Debian/Ubuntu:

```sh
sudo apt install build-essential cmake pkg-config libasound2-dev \
  libx11-dev libxcursor-dev libxrandr-dev libxi-dev libxkbcommon-dev \
  libxcb1-dev libxcb-randr0-dev libxcb-shm0-dev libdbus-1-dev
```

The media engine may add requirements for screen capture; see its README.

## Tests

```sh
cargo test -p chattercrab-client
```

Covers the backoff schedule, the session state machine (welcome handling, signaling
relay, screen-share grant/deny/stop, disconnect teardown, fatal errors), the room model
(membership, bounded chat, screen state, levels, reconnect), config round-trips
(including files written before `noise_suppression` or the auth keys existed), URL normalization,
argument parsing and the icon encoder.
