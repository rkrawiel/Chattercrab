//! Audio capture, encoding and playback.
//!
//! * [`AudioInput`]: cpal input stream, mono downmix in the callback, lock-free SPSC
//!   ring to a dedicated encoder thread that resamples to 48 kHz, applies microphone
//!   gain, optionally mixes system audio, cuts 20 ms frames and encodes them with Opus
//!   (VoIP, 96 kbps, DTX, in-band FEC, complexity 5).
//! * [`AudioOutput`]: cpal output stream fed from an SPSC ring of mono `f32` samples at
//!   the device rate; the matching [`OutputSink`] is handed to the mixer thread.
//! * Device enumeration and lookup by name.
//!
//! The cpal callbacks never allocate and never lock.

mod capture;
mod playback;
pub mod resample;
mod share;
mod system;

pub use capture::{AudioInput, CaptureStats};
pub use playback::{AudioOutput, OutputSink};
pub use share::ShareMix;
pub(crate) use system::SystemCapture;

use cpal::traits::{DeviceTrait, HostTrait};

use crate::MediaError;

/// Opus sample rate.
pub const SAMPLE_RATE: u32 = 48_000;
/// Samples per 20 ms frame at 48 kHz mono.
pub const FRAME_SAMPLES: usize = 960;
/// Largest Opus packet (RFC 6716).
pub const MAX_OPUS_PACKET: usize = crate::jitter::MAX_PACKET;
/// Target Opus bitrate.
pub const OPUS_BITRATE: i32 = 96_000;
/// Capacity of the encoded frame channel (16 x 20 ms = 320 ms).
pub const FRAME_CHANNEL_CAPACITY: usize = 16;

/// One encoded 20 ms Opus frame. Fixed-size so it can be passed through channels
/// without a heap allocation per frame.
#[derive(Clone)]
pub struct OpusFrame {
    data: [u8; MAX_OPUS_PACKET],
    len: u16,
    /// Monotonic frame counter assigned by the encoder.
    pub index: u64,
}

impl OpusFrame {
    /// Creates a frame from encoded bytes. Returns `None` if `bytes` is too long.
    pub fn new(bytes: &[u8], index: u64) -> Option<Self> {
        if bytes.len() > MAX_OPUS_PACKET {
            return None;
        }
        let mut data = [0u8; MAX_OPUS_PACKET];
        data[..bytes.len()].copy_from_slice(bytes);
        Some(Self {
            data,
            len: bytes.len() as u16,
            index,
        })
    }

    /// The encoded packet.
    pub fn bytes(&self) -> &[u8] {
        &self.data[..usize::from(self.len)]
    }
}

impl std::fmt::Debug for OpusFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpusFrame")
            .field("len", &self.len)
            .field("index", &self.index)
            .finish()
    }
}

/// Human-readable device name (cpal description name, falling back to `Display`).
pub(crate) fn device_name(device: &cpal::Device) -> String {
    device
        .description()
        .map(|d| d.name().to_owned())
        .unwrap_or_else(|_| device.to_string())
}

/// Names of the available input devices.
pub fn input_device_names() -> Result<Vec<String>, MediaError> {
    let host = cpal::default_host();
    let devices = host
        .input_devices()
        .map_err(|e| MediaError::Audio(e.to_string()))?;
    Ok(devices.map(|d| device_name(&d)).collect())
}

/// Names of the available output devices.
pub fn output_device_names() -> Result<Vec<String>, MediaError> {
    let host = cpal::default_host();
    let devices = host
        .output_devices()
        .map_err(|e| MediaError::Audio(e.to_string()))?;
    Ok(devices.map(|d| device_name(&d)).collect())
}

/// Finds an input device by name, or the default input when `name` is `None`.
pub(crate) fn find_input_device(name: Option<&str>) -> Result<cpal::Device, MediaError> {
    let host = cpal::default_host();
    match name {
        None => host
            .default_input_device()
            .ok_or_else(|| MediaError::DeviceNotFound("default input".into())),
        Some(wanted) => host
            .input_devices()
            .map_err(|e| MediaError::Audio(e.to_string()))?
            .find(|d| device_name(d) == wanted)
            .ok_or_else(|| MediaError::DeviceNotFound(wanted.to_owned())),
    }
}

/// Finds an output device by name, or the default output when `name` is `None`.
pub(crate) fn find_output_device(name: Option<&str>) -> Result<cpal::Device, MediaError> {
    let host = cpal::default_host();
    match name {
        None => host
            .default_output_device()
            .ok_or_else(|| MediaError::DeviceNotFound("default output".into())),
        Some(wanted) => host
            .output_devices()
            .map_err(|e| MediaError::Audio(e.to_string()))?
            .find(|d| device_name(d) == wanted)
            .ok_or_else(|| MediaError::DeviceNotFound(wanted.to_owned())),
    }
}

/// Lowest microphone gain. `0.0` replaces the microphone with silence.
pub const INPUT_GAIN_MIN: f32 = 0.0;
/// Highest microphone gain. `5.0` is 500% of the captured level.
pub const INPUT_GAIN_MAX: f32 = 5.0;

/// Clamps a microphone gain into [`INPUT_GAIN_MIN`]..=[`INPUT_GAIN_MAX`].
///
/// Non-finite values become `1.0` (unity, the captured level unchanged).
pub fn clamp_input_gain(gain: f32) -> f32 {
    if gain.is_finite() {
        gain.clamp(INPUT_GAIN_MIN, INPUT_GAIN_MAX)
    } else {
        1.0
    }
}

/// Multiplies `samples` by `gain` and hard-limits each sample to [-1, 1].
///
/// A gain of `1.0` leaves the samples untouched, so the default path does not
/// change existing audio.
pub fn apply_input_gain(samples: &mut [f32], gain: f32) {
    let gain = clamp_input_gain(gain);
    if (gain - 1.0).abs() <= f32::EPSILON {
        return;
    }
    for sample in samples {
        *sample = (*sample * gain).clamp(-1.0, 1.0);
    }
}

/// Root-mean-square level of an `f32` frame, 0.0..=1.0.
pub fn rms_f32(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f32 = samples.iter().map(|s| s * s).sum();
    (sum / samples.len() as f32).sqrt().min(1.0)
}

/// Root-mean-square level of an `i16` frame, 0.0..=1.0.
pub fn rms_i16(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples
        .iter()
        .map(|&s| {
            let v = f64::from(s) / 32768.0;
            v * v
        })
        .sum();
    ((sum / samples.len() as f64).sqrt() as f32).min(1.0)
}

/// Creates the Opus encoder with Chattercrab's voice settings.
pub fn new_opus_encoder() -> Result<opus::Encoder, MediaError> {
    let codec = |e: opus::Error| MediaError::Codec(e.to_string());
    let mut enc = opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip)
        .map_err(codec)?;
    enc.set_bitrate(opus::Bitrate::Bits(OPUS_BITRATE))
        .map_err(codec)?;
    enc.set_dtx(true).map_err(codec)?;
    enc.set_inband_fec(true).map_err(codec)?;
    // In-band FEC is only emitted when the encoder expects loss.
    enc.set_packet_loss_perc(10).map_err(codec)?;
    enc.set_complexity(5).map_err(codec)?;
    Ok(enc)
}

/// Creates a 48 kHz mono Opus decoder.
pub fn new_opus_decoder() -> Result<opus::Decoder, MediaError> {
    opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono)
        .map_err(|e| MediaError::Codec(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_bitrate_is_96_kbps() {
        assert_eq!(OPUS_BITRATE, 96_000);
        let mut enc = new_opus_encoder().unwrap();
        assert_eq!(enc.get_bitrate().unwrap(), opus::Bitrate::Bits(96_000));
    }

    #[test]
    fn opus_roundtrip_with_fec_and_plc() {
        let mut enc = new_opus_encoder().unwrap();
        let mut dec = new_opus_decoder().unwrap();
        let mut packets = Vec::new();
        for n in 0..10 {
            let frame: Vec<f32> = (0..FRAME_SAMPLES)
                .map(|i| ((n * FRAME_SAMPLES + i) as f32 * 0.05).sin() * 0.5)
                .collect();
            let mut out = [0u8; MAX_OPUS_PACKET];
            let len = enc.encode_float(&frame, &mut out).unwrap();
            packets.push(out[..len].to_vec());
        }
        let mut pcm = [0i16; FRAME_SAMPLES];
        assert_eq!(
            dec.decode(&packets[0], &mut pcm, false).unwrap(),
            FRAME_SAMPLES
        );
        assert!(rms_i16(&pcm) >= 0.0);
        // FEC recovery of frame 1 from packet 2.
        assert_eq!(
            dec.decode(&packets[2], &mut pcm, true).unwrap(),
            FRAME_SAMPLES
        );
        // PLC.
        assert_eq!(dec.decode(&[], &mut pcm, false).unwrap(), FRAME_SAMPLES);
    }

    #[test]
    fn silence_is_dtx_sized() {
        let mut enc = new_opus_encoder().unwrap();
        let zeros = [0f32; FRAME_SAMPLES];
        let mut out = [0u8; MAX_OPUS_PACKET];
        let mut last = 0;
        for _ in 0..30 {
            last = enc.encode_float(&zeros, &mut out).unwrap();
        }
        assert!(last <= 3, "silent frame was {last} bytes");
    }

    #[test]
    fn input_gain_is_unity_until_it_clips() {
        let mut samples = [0.2, -0.4, 0.5];
        apply_input_gain(&mut samples, 1.0);
        assert_eq!(samples, [0.2, -0.4, 0.5]);
        apply_input_gain(&mut samples, 5.0);
        assert!((samples[0] - 1.0).abs() < 1e-6);
        assert!((samples[1] + 1.0).abs() < 1e-6);
        assert!((samples[2] - 1.0).abs() < 1e-6);
        assert_eq!(clamp_input_gain(f32::NAN), 1.0);
        assert_eq!(clamp_input_gain(9.0), INPUT_GAIN_MAX);
        assert_eq!(clamp_input_gain(-1.0), INPUT_GAIN_MIN);
    }

    #[test]
    fn rms_levels() {
        assert_eq!(rms_f32(&[]), 0.0);
        assert!((rms_f32(&[0.5, -0.5]) - 0.5).abs() < 1e-6);
        assert!((rms_i16(&[i16::MIN, i16::MIN]) - 1.0).abs() < 1e-3);
    }

    #[test]
    fn opus_frame_bounds() {
        assert!(OpusFrame::new(&[1, 2, 3], 0).is_some());
        assert!(OpusFrame::new(&[0u8; MAX_OPUS_PACKET + 1], 0).is_none());
        assert_eq!(OpusFrame::new(&[9, 8], 4).unwrap().bytes(), &[9, 8]);
    }
}
