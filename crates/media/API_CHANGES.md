# chattercrab-media API changes

Changes to the public contract in `src/lib.rs` relative to the stub committed in
`e2cb69c` ("define media engine API contract with stub engine"). Rows above the ICE change are additive.

| Change | Kind | Notes |
| `MediaCommand::PlayCue(Cue)` + `Cue { Join, Leave, Message }` | new variant, new enum | Plays a short synthesized notification sound through the selected output device. The mixer adds it after the deafen gate, so it plays while deafened. Dropped (not queued) when no output is open, that is outside a room. A new cue replaces one still playing. `MediaConfig` is unchanged. |
| `Nv12Frame.seq: u64` + `Nv12Frame::next_seq()` | new field | Unique increasing picture number set by every producer. Plane buffers may be pooled and reuse an address, so consumers must detect a new picture by `seq`, not by `Arc` pointer. |
| `RgbaFrame` replaced by `Nv12Frame` | breaking | The screen queue now carries NV12 planes. The client draws them with a shader instead of uploading RGBA through the image widget. |
| `ScreenPublisher.bitrate_bps` | new field | Encoder target for the share that just started. |
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
- Audio devices are open only while in a room. `MediaCommand::Configure` opens the
  microphone and speaker (a repeated `Configure` keeps them open), and
  `MediaCommand::RemoveAllPeers` and `Shutdown` close them. Device errors arrive as
  `MediaEvent::Error` after `Configure`, not after `MediaEngine::start`. `SelectInput` and
  `SelectOutput` outside a room only record the device for the next `Configure`.
- `MediaEvent::Levels` is emitted only between `Configure` and `RemoveAllPeers`, and is
  dropped rather than queued when the event channel is full.
- A remote screen share is played up to one second behind the sender. The
  receiver holds encoded frames and shows them on the RTP clock, so a burst or a
  slow upload becomes delay instead of a dropped or late picture. Past one second
  it skips pictures until the buffer fits in that second again and keeps the
  spacing even. `VideoFrameQueue` no longer drops the oldest frame when it is full:
  `push` waits for a free slot and `try_push` returns the frame it could not store.
  The delay starts at 50 ms (at least three frames) and grows with jitter and
  underruns. After 5 s without an underrun or increase it decays back toward the
  jitter target by skipping (still decoding) one picture at most every 250 ms.
- A screen share encodes only captures that changed: on Windows a new Windows
  Graphics Capture frame (cursor moves included), otherwise a screenshot that is
  not byte-identical to the last one. A keyframe request always encodes, and an
  idle share still sends a picture about once a second. Each picture is an
  `EncodedVideo` with one frame period as `duration`. Time that produced no
  picture (unchanged, dropped, or rate-control-skipped captures, and ticks late
  by half a period or more) goes out just before the next picture as an
  `EncodedVideo` with empty `data` and the skipped time as `duration`;
  `video_pump` writes it as an empty sample, which sends no packet and only
  advances the RTP timestamp. So screen RTP timestamps follow wall time, and a
  viewer sees gaps of up to about one second between pictures. A gap is not an
  underrun: a picture is late only if it arrives after its RTP due time. The
  standard two-second IDR period counts encoded pictures, so an idle share gets
  periodic IDRs much less often; a joining viewer's keyframe request covers that.
- Screen-share underruns are counted only when a picture arrives more than one
  frame after its RTP due time. A jump in the RTP timestamps (the publisher skipped
  unchanged pictures) is not an underrun, does not raise the delay, and does not
  change the frame-interval estimate: once one interval is known, steps over 2.5×
  the estimate are ignored for it. An empty buffer alone no longer rebuffers.
- Decoded `Nv12Frame` planes come from a per-decoder pool and are reused once every
  clone of a frame is dropped, so a consumer should not hold old frames (it keeps
  their buffers out of the pool). Frames are cropped to the visible picture (the
  Windows hardware decoder reports 1920×1088 for 1080p; frames are 1920×1080).
  Chroma is `ceil(width / 2)` UV pairs by `ceil(height / 2)` rows, and `uv_stride`
  is the bytes actually written per chroma row (equal to `width` for even widths).
- Every screen-share encoder (Media Foundation, D3D11 video processor, VA-API,
  OpenH264) produces BT.709 limited-range YCbCr and signals it in the SPS VUI
  where the encoder allows, matching the viewer's BT.709 shader.
- Local audio and video are sent to a peer only after its connection reached
  `Connected` (and while it is transiently `Disconnected`), not during negotiation.
- `MediaEvent::RemoteScreenEnded` fires when the remote video track ends. The client
  should also hide the stream on the server's `screen_stopped`.
- Opus DTX frames (libopus "do not transmit" frames, at most `audio::DTX_MAX_BYTES` = 2
  bytes) are not sent. While a peer is silent or muted only the comfort-noise update
  (about one packet per 400 ms) goes out. Receivers see an RTP timestamp gap with
  contiguous sequence numbers (the gap is written as an empty sample, so
  `SendTarget::write` accepts empty data). The jitter buffer conceals the gap and starts
  the next talk spurt at its playout target; only a packet that arrives after its media
  time raises the target. Builds before this change see a gap as a late stream: they
  play it as concealment, and speech after a short pause may lose its first frames.
- The voice jitter buffer resynchronises only on a jump of 14 frames or more (was 6),
  and trims standing excess delay by at most one frame every 0.5 s (preferring DTX
  packets from older senders) once it is 40 ms above the target.
- In-band FEC follows the receivers: the expected packet loss given to Opus is the worst
  RTCP receiver-report `fraction_lost` over the connected peers, smoothed (immediate
  rise, 3 s decay), 0 % on a clean link and at most 20 %. It was a fixed 10 %. Bitrate
  (96 kbps) and complexity (5) are unchanged. Building blocks: `AudioInput.packet_loss`,
  `SendTarget::fraction_lost()`.
- The mixer keeps the playback ring at 25 ms (was 40 ms), or the device callback period
  plus 5 ms when that is longer (`OutputSink::callback_ms()`).
