# chattercrab-media

Client-side media engine for Chattercrab: microphone capture and Opus encoding, adaptive
jitter buffers, a real-time mixer, screen capture with H.264, and WebRTC peer management
for a full-mesh call of up to 5 participants. The GUI talks to it only through
`MediaCommand` / `MediaEvent` channels (see `src/lib.rs` and `API_CHANGES.md`).

## Modules

| Module | Purpose |
|--------|---------|
| `audio` | cpal input/output streams, device enumeration, Opus encoder thread, resampling (`audio::resample`). |
| `denoise` | Optional microphone noise suppression applied on the encoder thread before Opus. |
| `jitter` | Adaptive jitter buffer per remote peer (40 to 120 ms), FEC/PLC decisions. |
| `mixer` | Real-time mixer thread: decode, per-peer volume, `i32` sum, soft-knee limiter, deafen. |
| `screen` | Screen capture + H.264 encode (`ScreenPublisher`); RTP H.264 receive + decode (`screen::receive`). |
| `peer` | `PeerManager`: one webrtc `PeerConnection` per peer, trickle ICE, perfect negotiation (`peer::negotiation`, a pure state machine). |
| (private) `engine` | The Tokio task behind `MediaEngine` that wires everything together. |

## Threading

```text
cpal input callback ──SPSC ring (f32, 500 ms)──▶ encoder thread ──OpusFrame (bounded 16)──▶ audio pump task ──▶ one track per peer
                                                 (resample → 48 kHz, denoise, Opus)
RTP reader task (per peer) ──SPSC ring (64 packets)──▶ mixer thread (RT priority) ──SPSC ring (f32, 200 ms)──▶ cpal output callback
                                                      (jitter, decode, volume, limiter, resample)
screen capture thread ──EncodedVideo (bounded 4)──▶ video pump task ──▶ one track per peer
RTP reader task ──access units (bounded 8)──▶ decoder thread ──RgbaFrame──▶ VideoFrameQueue (capacity 2, drop oldest)
engine task (Tokio): commands, PeerManager, events
```

* The cpal callbacks and the mixer loop do not allocate and do not lock. Peer slots
  (decoder, jitter buffer, ring) are built on the engine task and moved into the mixer.
* Every channel is bounded; real-time producers use `try_push` / `try_send` and count drops.
* The mixer runs whenever the playback ring holds less than 40 ms, so the output device
  clock drives it. Iterations longer than 20 ms are counted and logged (rate-limited).
* Noise suppression adds 10 ms of algorithmic delay to the microphone path while it is on.

## Loopback example

```sh
# Microphone → Opus → jitter buffer → mixer → speakers (use headphones)
cargo run -p chattercrab-media --example loopback -- --seconds 10
cargo run -p chattercrab-media --example loopback -- --seconds 10 --denoise
# Without a usable microphone: synthetic voice-like tone through the same codec path
cargo run -p chattercrab-media --example loopback -- --seconds 10 --tone
```

At the end it prints encoded bitrate, capture-to-encode latency (microphone mode), peak
playback level, the longest mixer iteration and over-budget count, and output underruns.
If the microphone opens but delivers no audio for 3 s the example exits with an
explanation instead of hanging.

### Measurements (development machine, Windows 11, release build)

| Metric | Result |
|--------|--------|
| Mixer iteration, 1 peer, including 48 → 96 kHz resampling | max 0.33 ms (budget 20 ms), 0 overruns in 10 s |
| Encoded bitrate, voiced synthetic signal | 30.5 kbit/s (measured at the previous 32 kbps target; configured target is now 96 kbps) |
| Output underruns during 10 s | 0 |
| Denoiser cost | about 80 µs per 20 ms frame (measured by the denoise module, release) |
| Capture-to-encode latency | not measured here: the test machine's microphone delivered no capture callbacks (also with plain cpal). The encoder records it per frame (`CaptureStats::last_latency_us` / `max_latency_us`) as the time since the last capture callback plus audio still queued; expected 10 to 22 ms with 10 ms device buffers. Run the loopback example on a machine with a working microphone to report it. |

## Two-process call test

`examples/signal_call.rs` is a headless call participant: it joins a room on a real
`chattercrab-signal`, runs the real `MediaEngine` with a synthetic tone instead of the
microphone (`MediaDiagnostics::test_tone`), and prints a `RESULT ... ok=true|false` line.
It needs no audio devices.

Automated (in-process server, separate participant processes; ignored by default):

```text
cargo test -p chattercrab-media --test two_process -- --ignored --nocapture
CHATTERCRAB_E2E_PEERS=3 CHATTERCRAB_E2E_SECONDS=20 cargo test -p chattercrab-media --test two_process -- --ignored --nocapture
```

Manual, three terminals:

```text
# 1: server (plain HTTP, no TURN)
CHATTERCRAB_BIND=127.0.0.1:8443 cargo run -p chattercrab-server
# 2: first participant creates the room and prints "ROOM <id>"
cargo run -p chattercrab-media --example signal_call -- --server http://127.0.0.1:8443 --create demo --name a --tone 440 --seconds 30
# 3: second participant joins it
cargo run -p chattercrab-media --example signal_call -- --server http://127.0.0.1:8443 --room <id> --name b --tone 660 --seconds 25
```

Add `--play` to hear the other side on the default speaker. Each participant exits
non-zero unless every peer reached `Connected` and was heard above `--min-level`
(default 0.02) for at least `--min-heard-secs`.

## Noise suppression

Optional microphone noise suppression, toggled by the "Noise suppression" checkbox in
the client options (`MediaConfig::noise_suppression` /
`MediaCommand::SetNoiseSuppression`). Implemented in `denoise.rs` with RNNoise
(`nnnoiseless`, a pure-Rust port, BSD-3-Clause).

* What it does: removes steady and fluctuating background noise such as fans, hiss,
  hum-like rumble, traffic and keyboard noise, while keeping the speaker's voice.
  Measured attenuation: 19 to 53 dB on fan-like and rumble-like noise, about 37 dB on
  hiss after speech; a voiced test signal keeps its energy within about 1 dB.
* Where it runs: in the encoder thread, after resampling to 48 kHz and before Opus,
  on each 20 ms frame (two 10 ms RNNoise blocks). Skipped entirely when disabled or
  muted, so it costs nothing when off.
* Cost: about 80 microseconds per 20 ms frame in release builds (about 0.4% of one
  CPU core) and well under 100 KB of memory. `process` and `reset` never allocate.
* Latency: adds 10 ms (RNNoise's overlap-add delay). Switching it on is smoothed with a
  10 ms crossfade.
* Limitations:
  * It does not remove other voices: people talking in the background are kept.
  * Music and other tonal sounds may get slight artifacts or be partly suppressed.
  * Perfectly flat white noise is barely attenuated until speech has been heard; real
    background noise is not flat and is removed normally.
  * Switching it off drops 10 ms of audio once (the denoiser's delay is removed).

Measure it yourself with:

```bash
cargo test --release -p chattercrab-media denoise -- --nocapture
```

## Tests

```sh
cargo test -p chattercrab-media
```

* `jitter`: in-order, reorder, FEC when the next packet exists, PLC otherwise, late and
  duplicate drops, overflow resync, sequence wraparound, adaptive target, idle stop.
* `mixer`: summing, silence, limiter bounds and monotonicity, no wraparound on loud sums,
  volume scaling and clamping, deafen, real Opus decode through `MixerCore`.
* `peer::negotiation`: fake signaling between two negotiators; glare in both delivery
  orders, renegotiation, queued changes, 200 randomized race schedules.
* `tests/peer_loopback.rs`: two real `PeerManager`s on 127.0.0.1 exchange Opus RTP, then
  both start screen sharing at the same moment (renegotiation glare) and exchange H.264.
* `tests/engine_smoke.rs`: starts the real engine (devices optional) and shuts it down.
* `audio`, `screen`: Opus round trip with FEC/PLC, DTX size, H.264 encode/decode,
  resampler rate, scaling geometry.

## Build prerequisites

All platforms: Rust stable, a C/C++ compiler, and **CMake** (libopus is built from source
by `opusic-sys`). OpenH264 is compiled from source by `openh264-sys2`; `nasm` is optional
(assembly is skipped without it).

**Windows (MSVC):** Visual Studio Build Tools with the C++ workload. CMake can come from
`pip install --user cmake` if it is not on `PATH`.

**Cross-compiling to `x86_64-pc-windows-gnu` from Windows** works with
`cargo zigbuild` (Zig from `pip install --user ziglang`) plus Ninja as the CMake generator
for libopus: `pip install --user ninja`, then `CMAKE_GENERATOR=Ninja cargo zigbuild
-p chattercrab-media --target x86_64-pc-windows-gnu`. Verified: the loopback example built
this way links and runs.

**Linux (Debian/Ubuntu):**

```sh
sudo apt install build-essential cmake pkg-config clang libclang-dev \
    libasound2-dev \
    libxcb1-dev libxcb-randr0-dev libxcb-shm0-dev \
    libpipewire-0.3-dev libspa-0.2-dev libdbus-1-dev
```

`libasound2-dev` is for cpal (ALSA); the xcb and PipeWire packages are for `xcap`
(X11 capture; PipeWire/portal for Wayland, best effort). Real-time priority for the mixer
uses `SCHED_FIFO` and needs an `rtprio` limit, for example
`@audio - rtprio 20` in `/etc/security/limits.d/audio.conf`; without it the mixer runs at
normal priority and logs a warning.

Design decisions are recorded in `docs/DECISIONS.md` (ADR-200 and ADR-500 series).
