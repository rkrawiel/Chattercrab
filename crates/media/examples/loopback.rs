//! Headless audio loopback: microphone -> Opus encoder -> jitter buffer/decoder/mixer ->
//! speakers, using the same components as a real call (minus the network).
//!
//! ```text
//! cargo run -p chattercrab-media --example loopback -- --seconds 10 [--denoise] [--tone]
//! ```
//!
//! `--tone` replaces the microphone with a synthetic voice-like tone that goes through the
//! same Opus encoder settings, which checks the decode/mix/playback half on machines
//! without a usable microphone.
//!
//! Prints capture-to-encode latency and mixer timing statistics at the end.

#![allow(missing_docs)]

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use chattercrab_media::audio::{
    AudioInput, AudioOutput, FRAME_SAMPLES, MAX_OPUS_PACKET, ShareMix, new_opus_encoder,
};
use chattercrab_media::mixer::{MixerControl, MixerHandle, PeerSlot, RtpAudioPacket};
use chattercrab_protocol::PeerId;
use ringbuf::traits::Producer;
use tokio::sync::mpsc::error::TryRecvError;

/// Fails the run when the microphone delivers nothing for this long.
const NO_AUDIO_TIMEOUT: Duration = Duration::from_secs(3);

struct Args {
    seconds: u64,
    denoise: bool,
    tone: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        seconds: 5,
        denoise: false,
        tone: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--seconds" => {
                let v = it.next().ok_or("--seconds needs a value")?;
                args.seconds = v
                    .parse()
                    .map_err(|_| format!("invalid --seconds value: {v}"))?;
            }
            "--denoise" => args.denoise = true,
            "--tone" => args.tone = true,
            "--help" | "-h" => {
                return Err("usage: loopback [--seconds N] [--denoise] [--tone]".into());
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(args)
}

/// Where encoded frames come from.
enum Source {
    Mic(Box<AudioInput>),
    Tone {
        encoder: opus::Encoder,
        n: u64,
        next_at: Instant,
    },
}

impl Source {
    /// Returns the next encoded frame, waiting for it. `Err` means no audio arrives.
    fn next(&mut self, buf: &mut [u8; MAX_OPUS_PACKET]) -> Result<usize, String> {
        match self {
            Source::Mic(input) => {
                let waiting = Instant::now();
                loop {
                    match input.frames.try_recv() {
                        Ok(frame) => {
                            let bytes = frame.bytes();
                            buf[..bytes.len()].copy_from_slice(bytes);
                            return Ok(bytes.len());
                        }
                        Err(TryRecvError::Disconnected) => return Err("encoder stopped".into()),
                        Err(TryRecvError::Empty) => {
                            if waiting.elapsed() > NO_AUDIO_TIMEOUT {
                                return Err(format!(
                                    "the microphone delivered no audio for {} s; check that it is not \
                                     muted or held exclusively by another application and that the OS \
                                     allows desktop apps to use it (or run with --tone)",
                                    NO_AUDIO_TIMEOUT.as_secs()
                                ));
                            }
                            std::thread::sleep(Duration::from_millis(2));
                        }
                    }
                }
            }
            Source::Tone {
                encoder,
                n,
                next_at,
            } => {
                let now = Instant::now();
                if *next_at > now {
                    std::thread::sleep(*next_at - now);
                }
                *next_at += Duration::from_millis(20);
                // 180 Hz fundamental with harmonics and a 3 Hz syllable-like envelope.
                let mut pcm = [0f32; FRAME_SAMPLES];
                for (i, s) in pcm.iter_mut().enumerate() {
                    let t = (*n as usize * FRAME_SAMPLES + i) as f32 / 48_000.0;
                    let env = 0.5 + 0.5 * (std::f32::consts::TAU * 3.0 * t).sin();
                    let w = std::f32::consts::TAU * 180.0 * t;
                    *s = 0.3 * env * (w.sin() + 0.5 * (2.0 * w).sin() + 0.25 * (3.0 * w).sin());
                }
                *n += 1;
                encoder
                    .encode_float(&pcm, buf)
                    .map_err(|e| format!("opus: {e}"))
            }
        }
    }
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("loopback: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    let level = Arc::new(AtomicU32::new(0));
    let mut source = if args.tone {
        Source::Tone {
            encoder: new_opus_encoder().map_err(|e| e.to_string())?,
            n: 0,
            next_at: Instant::now(),
        }
    } else {
        let muted = Arc::new(AtomicBool::new(false));
        let denoise = Arc::new(AtomicBool::new(args.denoise));
        let gain = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let input = AudioInput::start(None, muted, denoise, gain, ShareMix::new(), level.clone())
            .map_err(|e| format!("cannot open the microphone: {e} (or run with --tone)"))?;
        Source::Mic(Box::new(input))
    };
    let (output, sink) =
        AudioOutput::start(None).map_err(|e| format!("cannot open the speakers: {e}"))?;
    let mixer = MixerHandle::spawn(Some(Box::new(sink))).map_err(|e| e.to_string())?;
    let (slot, mut ring, controls) = PeerSlot::new(PeerId::random()).map_err(|e| e.to_string())?;
    if !mixer.send(MixerControl::AddPeer(slot)) {
        return Err("mixer did not accept the stream".into());
    }

    let input_desc = match &source {
        Source::Mic(i) => format!("{} ({} Hz)", i.device_name, i.device_rate),
        Source::Tone { .. } => "synthetic tone".to_owned(),
    };
    println!(
        "loopback: {input_desc} -> {} ({} Hz), {} s, denoise {}",
        output.device_name,
        output.device_rate,
        args.seconds,
        if args.denoise { "on" } else { "off" }
    );
    if !args.tone {
        println!("speak into the microphone; use headphones to avoid feedback");
    }

    let deadline = Instant::now() + Duration::from_secs(args.seconds);
    let mut buf = [0u8; MAX_OPUS_PACKET];
    let mut seq: u16 = 0;
    let mut frames = 0u64;
    let mut bytes = 0u64;
    let mut latency_sum_us = 0u64;
    let mut peak_playback = 0.0f32;
    let mut last_report = Instant::now();
    while Instant::now() < deadline {
        let len = source.next(&mut buf)?;
        frames += 1;
        bytes += len as u64;
        if let Source::Mic(input) = &source {
            latency_sum_us += input.stats.last_latency_us.load(Ordering::Relaxed);
        }
        if let Some(p) = RtpAudioPacket::new(seq, u32::from(seq) * 960, &buf[..len], Instant::now())
        {
            let _ = ring.try_push(p);
        }
        seq = seq.wrapping_add(1);
        peak_playback = peak_playback.max(controls.level());
        if last_report.elapsed() >= Duration::from_secs(1) {
            last_report = Instant::now();
            println!(
                "  mic level {:.3}  playback level {:.3}",
                f32::from_bits(level.load(Ordering::Relaxed)),
                controls.level()
            );
        }
    }

    let stats = &mixer.stats;
    println!("--- results ---");
    println!(
        "frames encoded:            {frames} ({:.1} kbit/s average)",
        bytes as f64 * 8.0 / (frames.max(1) as f64 * 20.0)
    );
    if let Source::Mic(input) = &source {
        println!(
            "capture-to-encode latency: avg {:.2} ms, max {:.2} ms",
            latency_sum_us as f64 / frames.max(1) as f64 / 1000.0,
            input.stats.max_latency_us.load(Ordering::Relaxed) as f64 / 1000.0
        );
        println!(
            "dropped: {} encoded frames, {} capture samples",
            input.stats.frames_dropped.load(Ordering::Relaxed),
            input.stats.ring_overflow_samples.load(Ordering::Relaxed),
        );
    }
    println!("peak playback level:       {peak_playback:.3}");
    println!(
        "mixer: {} frames, max iteration {:.3} ms, {} over the 20 ms budget",
        stats.frames.load(Ordering::Relaxed),
        stats.max_tick_us.load(Ordering::Relaxed) as f64 / 1000.0,
        stats.overruns.load(Ordering::Relaxed)
    );
    println!(
        "output underrun samples:   {}",
        output.underruns.load(Ordering::Relaxed)
    );
    drop(mixer);
    drop(source);
    drop(output);
    Ok(())
}
