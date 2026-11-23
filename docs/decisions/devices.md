# Decisions: live audio devices and the microphone meter

Same format as `docs/DECISIONS.md`. The 620 series covers switching audio devices in a
room and the microphone level meter.

### ADR-620: Audio devices switch live from an audio panel in the room

* Context: the microphone and speakers could only be chosen on the connect screen.
  Changing them meant disconnecting, although the media engine already reopens a device
  on `MediaCommand::SelectInput` / `SelectOutput` while in a room and only records it
  otherwise. The room bottom bar has no space for two more pick lists.
* Decision: `UiCommand::SelectInput(Option<String>)` and `SelectOutput` are forwarded
  unchanged to the engine. Outside the connect screen a pick list choice sends them at
  once (`None` for "System default"), like noise suppression; on the connect screen the
  choice only goes into the config passed with Connect. The config is saved on every
  choice. An "Audio" button on the bottom bar opens an audio settings panel that replaces
  the bar, the same way the share picker does: Microphone and Speakers pick lists, mic
  volume, the mic level meter, noise suppression, "Refresh devices" and Close. Opening the
  panel re-lists devices. The panel and the share picker are never open together.
* Consequences: a device chosen in the lobby is recorded by the engine and used when the
  next room opens. A saved device that is missing from the listed devices is shown as
  "<name> (not found)", on the connect screen as well; it is not offered as an option, so
  the user picks another device or the system default. Whether the engine falls back when
  the device cannot be opened is unchanged.

### ADR-621: Segmented microphone level meter on a -60..0 dBFS scale

* Context: `MediaEvent::Levels` already delivers our microphone RMS (after gain, 0.0 while
  muted) every 100 ms, but only the avatar's speaking ring used it. A linear RMS bar
  barely moves for speech (RMS about 0.03 to 0.2).
* Decision: 16 rounded segments in a row, lit from the left. The RMS is mapped to dBFS
  and placed on a -60..0 dBFS scale (`ui::meter::fraction`), rounded to whole segments.
  Speech fills about half to three quarters; the speaking threshold (0.02, -34 dBFS)
  lights 7 segments. Segments 1 to 11 are green, 12 to 14 yellow, 15 and 16 red; unlit
  segments use the slider rail colour. The meter is drawn from plain containers, no
  canvas. It shows in the audio panel and, 5 px high, under the mic volume slider in the
  bottom bar. It is empty while muted, without waiting for the next level event.
* Consequences: no change to the media engine or the level interval; the meter updates at
  10 Hz with no peak hold or smoothing. The speaking threshold is not marked on the meter.
