//! Microphone capture and the Opus encoder thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use tokio::sync::mpsc;

use super::resample::MonoResampler;
use super::share::ShareMix;
use super::{
    FRAME_CHANNEL_CAPACITY, FRAME_SAMPLES, MAX_OPUS_PACKET, OpusFrame, SAMPLE_RATE,
    apply_input_gain, device_name, find_input_device, new_opus_encoder, rms_f32,
};
use crate::MediaError;
use crate::denoise::{DENOISE_FRAME_LEN, Denoiser};

// The denoiser works on exactly one encoder frame.
const _: () = assert!(DENOISE_FRAME_LEN == FRAME_SAMPLES);

/// Capture ring capacity in milliseconds of audio.
const RING_MS: u32 = 500;
/// Interval between capture statistics log lines.
const STATS_LOG_INTERVAL: Duration = Duration::from_secs(10);

/// Live counters shared between the capture callback, the encoder thread and readers.
#[derive(Debug, Default)]
pub struct CaptureStats {
    /// Samples dropped because the capture ring was full.
    pub ring_overflow_samples: AtomicU64,
    /// Encoded frames dropped because the frame channel was full.
    pub frames_dropped: AtomicU64,
    /// Encoded frames sent.
    pub frames_sent: AtomicU64,
    /// Last capture-to-encode latency in microseconds.
    pub last_latency_us: AtomicU64,
    /// Maximum capture-to-encode latency in microseconds since start.
    pub max_latency_us: AtomicU64,
    /// Time of the last capture callback, nanoseconds since the stream epoch.
    last_callback_ns: AtomicU64,
}

/// A running microphone capture with its encoder thread.
///
/// Encoded 20 ms Opus frames arrive on [`AudioInput::frames`]. Dropping the value stops
/// the stream and joins the encoder thread.
pub struct AudioInput {
    /// Encoded frames, bounded at [`FRAME_CHANNEL_CAPACITY`].
    pub frames: mpsc::Receiver<OpusFrame>,
    /// Name of the opened device.
    pub device_name: String,
    /// Device sample rate.
    pub device_rate: u32,
    /// Shared counters.
    pub stats: Arc<CaptureStats>,
    stream: Option<cpal::Stream>,
    /// Synthetic tone thread standing in for the device callback (diagnostics only).
    generator: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    encoder: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for AudioInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioInput")
            .field("device_name", &self.device_name)
            .field("device_rate", &self.device_rate)
            .finish_non_exhaustive()
    }
}

impl AudioInput {
    /// Opens the input device (`None` = default) and starts capturing and encoding.
    ///
    /// `muted` replaces captured audio with silence (the stream keeps running and Opus
    /// DTX keeps packets tiny). `denoise` enables noise suppression on unmuted frames.
    /// `gain` is the microphone preamp as `f32` bits (`1.0` unity, `5.0` is 500%).
    /// `share` mixes system audio into each frame while a consumer is attached.
    /// `level` receives the RMS of each microphone frame as `f32` bits, after gain
    /// and before the system-audio mix.
    /// This call blocks while the device opens; call it from a blocking context.
    pub fn start(
        device: Option<&str>,
        muted: Arc<AtomicBool>,
        denoise: Arc<AtomicBool>,
        gain: Arc<AtomicU32>,
        share: Arc<ShareMix>,
        level: Arc<AtomicU32>,
    ) -> Result<Self, MediaError> {
        let device = find_input_device(device)?;
        let name = device_name(&device);
        let supported = device
            .default_input_config()
            .map_err(|e| MediaError::Audio(format!("{name}: {e}")))?;
        let config = supported.config();
        let rate = config.sample_rate;
        let channels = usize::from(config.channels.max(1));

        let ring = HeapRb::<f32>::new((rate * RING_MS / 1000) as usize);
        let (prod, cons) = ring.split();
        let stats = Arc::new(CaptureStats::default());
        let epoch = Instant::now();

        let err_name = name.clone();
        let on_error =
            move |e: cpal::Error| tracing::warn!(device = %err_name, "input stream error: {e}");
        let s = stats.clone();
        let stream = match supported.sample_format() {
            cpal::SampleFormat::F32 => {
                build::<f32>(&device, &config, channels, prod, s, epoch, on_error)
            }
            cpal::SampleFormat::I16 => {
                build::<i16>(&device, &config, channels, prod, s, epoch, on_error)
            }
            cpal::SampleFormat::I32 => {
                build::<i32>(&device, &config, channels, prod, s, epoch, on_error)
            }
            cpal::SampleFormat::U16 => {
                build::<u16>(&device, &config, channels, prod, s, epoch, on_error)
            }
            cpal::SampleFormat::U8 => {
                build::<u8>(&device, &config, channels, prod, s, epoch, on_error)
            }
            cpal::SampleFormat::I8 => {
                build::<i8>(&device, &config, channels, prod, s, epoch, on_error)
            }
            cpal::SampleFormat::F64 => {
                build::<f64>(&device, &config, channels, prod, s, epoch, on_error)
            }
            other => {
                return Err(MediaError::Audio(format!(
                    "{name}: unsupported sample format {other:?}"
                )));
            }
        }
        .map_err(|e| MediaError::Audio(format!("{name}: {e}")))?;
        stream
            .play()
            .map_err(|e| MediaError::Audio(format!("{name}: {e}")))?;

        tracing::info!(device = %name, rate, channels, "audio input started");
        let mut input = Self::spawn_encoder(
            cons,
            name,
            rate,
            Flags {
                muted,
                denoise,
                gain,
                share,
            },
            level,
            stats,
            epoch,
        )?;
        input.stream = Some(stream);
        Ok(input)
    }

    /// Starts a synthetic sine tone of `hz` (with a 3 Hz speech-like amplitude envelope)
    /// instead of a microphone (diagnostics).
    ///
    /// A generator thread writes 10 ms blocks of 48 kHz audio into the capture ring at
    /// real-time pace, exactly as a device callback would, so the encoder thread, mute,
    /// noise suppression and Opus run unchanged. No audio device is needed.
    pub fn start_tone(
        hz: f32,
        muted: Arc<AtomicBool>,
        denoise: Arc<AtomicBool>,
        gain: Arc<AtomicU32>,
        share: Arc<ShareMix>,
        level: Arc<AtomicU32>,
    ) -> Result<Self, MediaError> {
        if !(hz.is_finite() && hz > 0.0 && hz < SAMPLE_RATE as f32 / 2.0) {
            return Err(MediaError::Audio(format!(
                "invalid test tone frequency {hz}"
            )));
        }
        let ring = HeapRb::<f32>::new((SAMPLE_RATE * RING_MS / 1000) as usize);
        let (prod, cons) = ring.split();
        let stats = Arc::new(CaptureStats::default());
        let epoch = Instant::now();
        let name = format!("test tone {hz} Hz");
        let mut input = Self::spawn_encoder(
            cons,
            name.clone(),
            SAMPLE_RATE,
            Flags {
                muted,
                denoise,
                gain,
                share,
            },
            level,
            stats.clone(),
            epoch,
        )?;
        let stop = input.stop.clone();
        let generator = std::thread::Builder::new()
            .name("cc-audio-tone".into())
            .spawn(move || run_tone(hz, prod, stats, epoch, stop))
            .map_err(|e| MediaError::Audio(format!("spawn tone thread: {e}")))?;
        input.generator = Some(generator);
        tracing::info!(device = %name, "audio input started");
        Ok(input)
    }

    /// Spawns the encoder thread reading `cons` and assembles the [`AudioInput`].
    fn spawn_encoder(
        cons: HeapCons<f32>,
        name: String,
        rate: u32,
        flags: Flags,
        level: Arc<AtomicU32>,
        stats: Arc<CaptureStats>,
        epoch: Instant,
    ) -> Result<Self, MediaError> {
        let (tx, rx) = mpsc::channel(FRAME_CHANNEL_CAPACITY);
        let stop = Arc::new(AtomicBool::new(false));
        let worker = EncoderWorker::new(
            cons,
            rate,
            tx,
            flags,
            level,
            stats.clone(),
            epoch,
            stop.clone(),
        )?;
        let encoder = std::thread::Builder::new()
            .name("cc-audio-encoder".into())
            .spawn(move || worker.run())
            .map_err(|e| MediaError::Audio(format!("spawn encoder thread: {e}")))?;
        Ok(Self {
            frames: rx,
            device_name: name,
            device_rate: rate,
            stats,
            stream: None,
            generator: None,
            stop,
            encoder: Some(encoder),
        })
    }
}

/// Body of the test-tone thread: 10 ms blocks at real-time pace, no allocation.
fn run_tone(
    hz: f32,
    mut prod: HeapProd<f32>,
    stats: Arc<CaptureStats>,
    epoch: Instant,
    stop: Arc<AtomicBool>,
) {
    const BLOCK: usize = (SAMPLE_RATE / 100) as usize;
    const BLOCK_PERIOD: Duration = Duration::from_millis(10);
    // A perfectly steady sine looks like stationary background to Opus, which then
    // switches to DTX (comfort noise) after a few seconds. A 3 Hz syllable-rate
    // amplitude envelope keeps the encoder in active mode like real speech does.
    const ENVELOPE_HZ: f64 = 3.0;
    let tau = std::f64::consts::TAU;
    let step = tau * f64::from(hz) / f64::from(SAMPLE_RATE);
    let env_step = tau * ENVELOPE_HZ / f64::from(SAMPLE_RATE);
    let mut phase = 0.0f64;
    let mut env_phase = 0.0f64;
    let mut block = [0f32; BLOCK];
    let mut deadline = Instant::now();
    while !stop.load(Ordering::Acquire) {
        for s in &mut block {
            let envelope = 0.55 + 0.45 * env_phase.sin();
            *s = (phase.sin() * 0.3 * envelope) as f32;
            phase = (phase + step) % tau;
            env_phase = (env_phase + env_step) % tau;
        }
        let pushed = prod.push_slice(&block);
        if pushed < BLOCK {
            stats
                .ring_overflow_samples
                .fetch_add((BLOCK - pushed) as u64, Ordering::Relaxed);
        }
        stats
            .last_callback_ns
            .store(epoch.elapsed().as_nanos() as u64, Ordering::Release);
        deadline += BLOCK_PERIOD;
        let now = Instant::now();
        if deadline > now {
            std::thread::sleep(deadline - now);
        } else if now - deadline > BLOCK_PERIOD * 10 {
            // Fell far behind (suspended process): resynchronise instead of bursting.
            deadline = now;
        }
    }
}

impl Drop for AudioInput {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        drop(self.stream.take());
        if let Some(handle) = self.generator.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.encoder.take() {
            let _ = handle.join();
        }
    }
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    mut prod: HeapProd<f32>,
    stats: Arc<CaptureStats>,
    epoch: Instant,
    on_error: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    let scale = 1.0 / channels as f32;
    device.build_input_stream::<T, _, _>(
        *config,
        move |data: &[T], _info| {
            // Real-time context: no allocation, no locks.
            let mut chunk = [0f32; 256];
            let mut n = 0;
            let mut dropped = 0usize;
            for frame in data.chunks_exact(channels) {
                let mut sum = 0.0f32;
                for s in frame {
                    sum += s.to_sample::<f32>();
                }
                chunk[n] = sum * scale;
                n += 1;
                if n == chunk.len() {
                    dropped += n - prod.push_slice(&chunk);
                    n = 0;
                }
            }
            if n > 0 {
                dropped += n - prod.push_slice(&chunk[..n]);
            }
            if dropped > 0 {
                stats
                    .ring_overflow_samples
                    .fetch_add(dropped as u64, Ordering::Relaxed);
            }
            stats
                .last_callback_ns
                .store(epoch.elapsed().as_nanos() as u64, Ordering::Release);
        },
        on_error,
        None,
    )
}

/// Live switches read by the encoder thread every frame.
struct Flags {
    muted: Arc<AtomicBool>,
    denoise: Arc<AtomicBool>,
    /// Microphone gain as `f32` bits.
    gain: Arc<AtomicU32>,
    /// System audio mixed in after the microphone is processed.
    share: Arc<ShareMix>,
}

/// State owned by the encoder thread.
struct EncoderWorker {
    cons: HeapCons<f32>,
    device_rate: u32,
    resampler: Option<MonoResampler>,
    encoder: opus::Encoder,
    tx: mpsc::Sender<OpusFrame>,
    flags: Flags,
    level: Arc<AtomicU32>,
    stats: Arc<CaptureStats>,
    epoch: Instant,
    stop: Arc<AtomicBool>,
}

impl EncoderWorker {
    #[allow(clippy::too_many_arguments)]
    fn new(
        cons: HeapCons<f32>,
        device_rate: u32,
        tx: mpsc::Sender<OpusFrame>,
        flags: Flags,
        level: Arc<AtomicU32>,
        stats: Arc<CaptureStats>,
        epoch: Instant,
        stop: Arc<AtomicBool>,
    ) -> Result<Self, MediaError> {
        let resampler = if device_rate == SAMPLE_RATE {
            None
        } else {
            // 10 ms input chunks at the device rate.
            Some(MonoResampler::new(
                device_rate,
                SAMPLE_RATE,
                (device_rate / 100) as usize,
            )?)
        };
        Ok(Self {
            cons,
            device_rate,
            resampler,
            encoder: new_opus_encoder()?,
            tx,
            flags,
            level,
            stats,
            epoch,
            stop,
        })
    }

    fn run(mut self) {
        let chunk = self
            .resampler
            .as_ref()
            .map_or(FRAME_SAMPLES / 2, MonoResampler::chunk);
        let max_out = self
            .resampler
            .as_ref()
            .map_or(chunk, MonoResampler::max_output);
        // All buffers are allocated once, here.
        let mut input = vec![0f32; chunk];
        let mut acc: Vec<f32> = Vec::with_capacity(FRAME_SAMPLES + max_out);
        let mut frame_buf = [0f32; DENOISE_FRAME_LEN];
        // Built here, on the encoder thread: the denoiser warms thread-local state in
        // `new`, so constructing it elsewhere would allocate on the first `process`.
        let mut denoiser = Denoiser::new();
        let mut denoise_was_on = false;
        let mut was_muted = false;
        let mut packet = [0u8; MAX_OPUS_PACKET];
        let mut index = 0u64;
        let mut latency_sum_us = 0u64;
        let mut latency_count = 0u64;
        let mut last_log = Instant::now();

        while !self.stop.load(Ordering::Acquire) {
            if self.cons.occupied_len() < chunk {
                std::thread::sleep(Duration::from_millis(2));
                continue;
            }
            let got = self.cons.pop_slice(&mut input);
            debug_assert_eq!(got, chunk);
            match self.resampler.as_mut() {
                Some(r) => match r.process(&input) {
                    Ok(out) => acc.extend_from_slice(out),
                    Err(e) => {
                        tracing::warn!("capture resampler: {e}");
                        continue;
                    }
                },
                None => acc.extend_from_slice(&input),
            }

            while acc.len() >= FRAME_SAMPLES {
                let muted = self.flags.muted.load(Ordering::Relaxed);
                let denoise_on = self.flags.denoise.load(Ordering::Relaxed);
                let gain = f32::from_bits(self.flags.gain.load(Ordering::Relaxed));
                // Reset when suppression switches on, and on unmute while it is on, so the
                // denoiser's 10 ms of internal history never replays stale audio.
                if denoise_on && (!denoise_was_on || (was_muted && !muted)) {
                    denoiser.reset();
                }
                denoise_was_on = denoise_on;
                was_muted = muted;
                if muted {
                    frame_buf.fill(0.0);
                } else {
                    frame_buf.copy_from_slice(&acc[..FRAME_SAMPLES]);
                    // Suppression runs on the captured level. Gain comes after, so a quiet
                    // mic is boosted without first amplifying the noise the denoiser removes.
                    if denoise_on {
                        denoiser.process(&mut frame_buf);
                    }
                    apply_input_gain(&mut frame_buf, gain);
                }
                let rms = if muted { 0.0 } else { rms_f32(&frame_buf) };
                self.level.store(rms.to_bits(), Ordering::Relaxed);
                // Mute silences the microphone only. Shared application audio still goes out.
                self.flags.share.mix_into(&mut frame_buf);
                match self.encoder.encode_float(&frame_buf, &mut packet) {
                    Ok(len) => {
                        if let Some(f) = OpusFrame::new(&packet[..len], index) {
                            index += 1;
                            match self.tx.try_send(f) {
                                Ok(()) => {
                                    self.stats.frames_sent.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(mpsc::error::TrySendError::Full(_)) => {
                                    self.stats.frames_dropped.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(mpsc::error::TrySendError::Closed(_)) => return,
                            }
                        }
                    }
                    Err(e) => tracing::warn!("opus encode: {e}"),
                }
                acc.drain(..FRAME_SAMPLES);

                // Capture-to-encode latency: age of the newest sample in this frame at the
                // moment encoding finished. That is the time since the last callback plus
                // the audio still queued behind this frame (ring + accumulator).
                let now_ns = self.epoch.elapsed().as_nanos() as u64;
                let cb_ns = self.stats.last_callback_ns.load(Ordering::Acquire);
                let queued_us = (self.cons.occupied_len() as u64 * 1_000_000)
                    / u64::from(self.device_rate)
                    + (acc.len() as u64 * 1_000_000) / u64::from(SAMPLE_RATE);
                let latency_us = now_ns.saturating_sub(cb_ns) / 1000 + queued_us;
                self.stats
                    .last_latency_us
                    .store(latency_us, Ordering::Relaxed);
                self.stats
                    .max_latency_us
                    .fetch_max(latency_us, Ordering::Relaxed);
                latency_sum_us += latency_us;
                latency_count += 1;
            }

            if last_log.elapsed() >= STATS_LOG_INTERVAL && latency_count > 0 {
                tracing::debug!(
                    avg_latency_ms = latency_sum_us as f64 / latency_count as f64 / 1000.0,
                    max_latency_ms =
                        self.stats.max_latency_us.load(Ordering::Relaxed) as f64 / 1000.0,
                    frames_sent = self.stats.frames_sent.load(Ordering::Relaxed),
                    frames_dropped = self.stats.frames_dropped.load(Ordering::Relaxed),
                    ring_overflow = self.stats.ring_overflow_samples.load(Ordering::Relaxed),
                    "audio capture stats"
                );
                latency_sum_us = 0;
                latency_count = 0;
                last_log = Instant::now();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Controls {
        muted: Arc<AtomicBool>,
        denoise: Arc<AtomicBool>,
        gain: Arc<AtomicU32>,
        share: Arc<ShareMix>,
        level: Arc<AtomicU32>,
    }

    fn controls() -> Controls {
        Controls {
            muted: Arc::new(AtomicBool::new(false)),
            denoise: Arc::new(AtomicBool::new(false)),
            gain: Arc::new(AtomicU32::new(1.0f32.to_bits())),
            share: ShareMix::new(),
            level: Arc::new(AtomicU32::new(0)),
        }
    }

    #[test]
    fn tone_produces_paced_opus_frames_without_a_device() {
        let c = controls();
        let mut input =
            AudioInput::start_tone(440.0, c.muted, c.denoise, c.gain, c.share, c.level.clone())
                .unwrap();
        assert_eq!(input.device_rate, SAMPLE_RATE);
        std::thread::sleep(Duration::from_millis(500));
        let mut frames = 0;
        while let Ok(f) = input.frames.try_recv() {
            assert!(!f.bytes().is_empty());
            frames += 1;
        }
        // 500 ms of real-time audio is 25 frames; allow for scheduling jitter.
        assert!((15..=40).contains(&frames), "got {frames} frames");
        assert!(f32::from_bits(c.level.load(Ordering::Relaxed)) > 0.05);
    }

    #[test]
    fn muted_tone_reports_zero_level() {
        let c = controls();
        c.muted.store(true, Ordering::Relaxed);
        let _input =
            AudioInput::start_tone(440.0, c.muted, c.denoise, c.gain, c.share, c.level.clone())
                .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(f32::from_bits(c.level.load(Ordering::Relaxed)), 0.0);
    }

    #[test]
    fn zero_gain_reports_zero_level() {
        let c = controls();
        c.gain.store(0.0f32.to_bits(), Ordering::Relaxed);
        let _input =
            AudioInput::start_tone(440.0, c.muted, c.denoise, c.gain, c.share, c.level.clone())
                .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(f32::from_bits(c.level.load(Ordering::Relaxed)), 0.0);
    }

    #[test]
    fn rejects_invalid_tone_frequency() {
        for hz in [0.0, -1.0, 24_000.0, f32::NAN] {
            let c = controls();
            assert!(
                AudioInput::start_tone(hz, c.muted, c.denoise, c.gain, c.share, c.level).is_err()
            );
        }
    }
}
