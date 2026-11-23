# Notification sounds (ADR-600 series)

### ADR-600: Join, leave and message cues synthesized in the media mixer

* Context: users want to hear when someone joins or leaves the room and when a chat
  message arrives, Discord-style: a rising sound for a join, the falling mirror for a
  leave, and a distinct sound for a message. The cues must play on the speaker the user
  selected and must still play while deafened.
* Decision:
  * The media engine plays cues: `MediaCommand::PlayCue(Cue)` with
    `Cue { Join, Leave, Message }`. The engine forwards it to the mixer thread, which
    adds the cue to the 20 ms frame after the deafen gate and before the soft-knee
    limiter. Cues therefore use the selected output device, its resampler and its
    clock, with no second output stream in the client.
  * Tones are synthesized in code (no asset files): sine notes at 48 kHz mono with a
    5 ms smoothstep attack, an exponential decay and a 15 ms smoothstep release, peak
    0.25 of full scale. Join is E5 (659 Hz, 90 ms) then A5 (880 Hz, 130 ms). Leave is
    A5 (90 ms) then E5 (130 ms). Message is a double blip at E6 (1319 Hz): 60 ms, 30 ms
    rest, 60 ms. All three are rendered once when the mixer is created, so the
    real-time thread never allocates for them.
  * A new cue replaces one still playing. A cue sent while no output is open is dropped,
    and closing the output stops a cue in progress.
  * The client's session state machine decides when to play: one join cue on `welcome`
    (ordered after `Configure`, which opens the output), a join cue per `peer_joined`
    and a leave cue per `peer_left` for a known peer, and a message cue for `chat`
    from another peer. Our own echoed message and the history delivered in `welcome`
    are silent. Leaving the room plays nothing, because the output closes.
* Consequences:
  * A reconnect plays a single join cue, because `welcome` carries the peer list rather
    than one `peer_joined` per peer.
  * Cues are not affected by peer volumes and have no volume or on/off setting of their
    own. Adding one would be a `MediaCommand` and a config field, not a mixer change.
  * Two events within about 200 ms (for example a join and a message) play only the
    second cue.
