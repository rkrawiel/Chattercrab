# chattercrab-media API changes

Changes to the public contract in `src/lib.rs` relative to the stub committed in
`e2cb69c` ("define media engine API contract with stub engine"). Rows above the ICE change are additive.

| Change | Kind | Notes |
| `MediaCommand::StartScreenShare.content: ScreenContent` | new field | Chosen before the share starts, independent of resolution and frame rate. `Desktop` tunes for crisp fonts (screen-content tools, two references, slower preset). `Game` tunes for moving images (motion tools, one reference, faster preset, and 60 Mbps at 1440p60 instead of 45 when high quality is on). Both rates still scale by pixels and frame rate, up to 120 Mbps. |
| `MediaCommand::StartScreenShare.high_quality: bool` | new field | High-quality share: H.264 High (`profile-level-id=640034`), fixed CBR from the content mode above, no B-frames, about two frames of rate-control delay, IDR only on request (at most one a second). Frame rate clamp is 5..=120 when set, otherwise 5..=60. Viewers must advertise the High payload type, which current clients do alongside constrained baseline. Linux VA-API selects the same High profile (CABAC, 8×8, two references for desktop and one for game) and falls back to Constrained Baseline, then OpenH264, when the driver has no High entrypoint. |
| `MediaConfig::input_gain: f32` | new field | Microphone preamp. `1.0` is unity, `5.0` is 500%. `Default` is `1.0` (a derived `Default` would have been `0.0` and silenced the mic). Applied on the encoder thread after noise suppression, hard-limited to [-1, 1]. |
| `MediaCommand::SetPeerVolume` | behavior | Playback gain clamp is 0.0..=5.0 (was 0.0..=2.0). `5.0` is 500%. |
| `MediaCommand::SetInputGain(f32)` | new variant | Live microphone gain. Clamped to 0.0..=5.0. Non-finite values become `1.0`. |
| `MediaCommand::StartScreenShare.include_audio: bool` | new field | When true, Windows mixes other applications' audio into the outgoing voice for the duration of the share. This process is excluded, so playback of the call is not sent back. Other platforms report that capture is unavailable and the video share still starts. |
| --------------------------------------------------------------- | ------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `MediaCommand::StartScreenShare.scale: ScreenScale` | new field | `1440p` fits inside 2560×1440 (default; older `full`, `half`, and `2160p` values load as this). `1080p` fits inside 1920×1080. Frames are never scaled up. Frame rate clamp is 5..=60. The share picker pairs 1440p with 60 fps and 1080p with 30 fps. Windows encodes with the GPU's H.264 encoder when one is registered. |
| `MediaConfig::noise_suppression: bool` | new field | Default `false`. Enable RNNoise-based noise suppression on the microphone input. `MediaConfig` still derives `Default`, so `MediaConfig { input_device, output_device, ..Default::default() }` keeps compiling; exhaustive struct literals need the new field. |
| `MediaCommand::SetNoiseSuppression(bool)` | new variant | Enable or disable microphone noise suppression live. Read by the encoder thread through an `AtomicBool`. |
| `MediaDiagnostics { test_tone: Option<f32> }` | new struct | Diagnostics only. `test_tone` replaces the microphone with a sine tone (3 Hz speech-like envelope) that goes through the normal encoder thread. Kept out of `MediaConfig` so the client's exhaustive `MediaConfig` literal keeps compiling. |
| `MediaEngine::start_with_diagnostics(config, diagnostics)` | new function | `start(config)` is `start_with_diagnostics(config, MediaDiagnostics::default())`. Used by `examples/signal_call.rs` and `tests/two_process.rs`. |
| `pub mod audio`, `jitter`, `mixer`, `peer`, `screen`, `denoise` | new modules | Building blocks used by the engine, the loopback example and tests. The client should keep using `MediaEngine`, `MediaCommand` and `MediaEvent` only. |
| `MediaCommand::Configure.ice` is `Option<IceServers>` | breaking | Was `Option<TurnCredentials>`. STUN servers from `GET /ice-servers` (urls only). `None` or empty `urls` means host candidates only. |

Behavior notes for the client (no type changes):

- `MediaEngine::start` no longer fails when no audio device exists. It returns `Ok`, and
  device problems arrive as `MediaEvent::Error` (the engine keeps running, for example
  receive-only without a microphone). It returns `Err` only when called outside a Tokio
  runtime.
- `MediaCommand::RemoveAllPeers` also stops a local screen share (emitting
  `LocalScreenStopped`), because the server releases the screen lock when the socket drops.
- `MediaEvent::Levels` is emitted only between `Configure` and `RemoveAllPeers`, and is
  dropped rather than queued when the event channel is full.
- A remote screen share is played up to one second behind the sender. The
  receiver holds encoded frames and shows them on the RTP clock, so a burst or a
  slow upload becomes delay instead of a dropped or late picture. Past one second
  it skips pictures until the buffer fits in that second again and keeps the
  spacing even. `VideoFrameQueue` no longer drops the oldest frame when it is full:
  `push` waits for a free slot and `try_push` returns the frame it could not store.
- Local audio and video are sent to a peer only after its connection reached
  `Connected` (and while it is transiently `Disconnected`), not during negotiation.
- `MediaEvent::RemoteScreenEnded` fires when the remote video track ends. The client
  should also hide the stream on the server's `screen_stopped`.
