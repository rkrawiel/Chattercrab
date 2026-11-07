//! Real-time mixer.
//!
//! A dedicated thread (promoted to real-time priority where the OS allows) pulls one
//! 20 ms frame per remote peer from that peer's [`JitterBuffer`], decodes it with the
//! peer's Opus decoder (normal, FEC or PLC), applies the peer volume, sums into `i32`,
//! runs a soft-knee limiter, converts to `i16` and writes the frame to the playback
//! ring. Pacing follows the playback ring fill level, so the output device clock drives
//! the mixer. Without an output device the mixer paces itself on a 20 ms timer.
//!
//! Everything the loop touches is allocated before the loop: peer slots arrive fully
//! constructed through the control channel and the slot vector has fixed capacity.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use chattercrab_protocol::PeerId;
use ringbuf::traits::{Consumer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

use crate::MediaError;
use crate::audio::{FRAME_SAMPLES, OutputSink, new_opus_decoder, rms_i16};
use crate::jitter::{JitterBuffer, MAX_PACKET, Playout};

/// Maximum number of simultaneously mixed peers (design target is 4 remote peers).
pub const MAX_PEERS: usize = 8;
/// Packets buffered between an RTP reader task and the mixer for one peer.
pub const PEER_RING_PACKETS: usize = 64;
/// Playback ring fill level the mixer maintains, in milliseconds.
const TARGET_OUTPUT_MS: u32 = 40;
/// Frame period.
const FRAME_PERIOD: Duration = Duration::from_millis(20);
/// Minimum interval between "mixer overrun" warnings.
const WARN_INTERVAL: Duration = Duration::from_secs(5);
/// Fixed-point scale for per-peer gain (Q12).
const GAIN_ONE: i32 = 4096;
/// Soft-knee threshold: samples below this pass unchanged.
const KNEE: f32 = 24_576.0;
/// Limiter ceiling.
const CEILING: f32 = 32_767.0;

/// One received RTP audio payload, fixed-size so the reader-to-mixer ring never allocates.
#[derive(Clone, Copy)]
pub struct RtpAudioPacket {
    /// RTP sequence number.
    pub seq: u16,
    /// RTP timestamp (48 kHz).
    pub timestamp: u32,
    /// Arrival time.
    pub arrival: Instant,
    len: u16,
    data: [u8; MAX_PACKET],
}

impl RtpAudioPacket {
    /// Builds a packet; returns `None` if the payload is empty or too large.
    pub fn new(seq: u16, timestamp: u32, payload: &[u8], arrival: Instant) -> Option<Self> {
        if payload.is_empty() || payload.len() > MAX_PACKET {
            return None;
        }
        let mut data = [0u8; MAX_PACKET];
        data[..payload.len()].copy_from_slice(payload);
        Some(Self {
            seq,
            timestamp,
            arrival,
            len: payload.len() as u16,
            data,
        })
    }

    /// The Opus payload.
    pub fn payload(&self) -> &[u8] {
        &self.data[..usize::from(self.len)]
    }
}

/// Highest per-peer playback gain. `5.0` is 500%.
pub const PEER_VOLUME_MAX: f32 = 5.0;

/// Shared per-peer controls visible outside the mixer thread.
#[derive(Debug, Clone)]
pub struct PeerAudioControls {
    /// Linear gain as `f32` bits (0.0..=[`PEER_VOLUME_MAX`]).
    pub volume: Arc<AtomicU32>,
    /// Last frame RMS as `f32` bits.
    pub level: Arc<AtomicU32>,
}

impl PeerAudioControls {
    fn new() -> Self {
        Self {
            volume: Arc::new(AtomicU32::new(1.0f32.to_bits())),
            level: Arc::new(AtomicU32::new(0)),
        }
    }

    /// Sets the gain, clamped to 0.0..=[`PEER_VOLUME_MAX`].
    pub fn set_volume(&self, volume: f32) {
        let v = if volume.is_finite() {
            volume.clamp(0.0, PEER_VOLUME_MAX)
        } else {
            1.0
        };
        self.volume.store(v.to_bits(), Ordering::Relaxed);
    }

    /// Last measured RMS level.
    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }
}

/// Everything the mixer needs for one remote peer. Built outside the real-time thread.
pub struct PeerSlot {
    peer: PeerId,
    packets: HeapCons<RtpAudioPacket>,
    jitter: JitterBuffer,
    decoder: opus::Decoder,
    pcm: [i16; FRAME_SAMPLES],
    controls: PeerAudioControls,
}

impl PeerSlot {
    /// Creates a slot and the producer the RTP reader pushes into.
    pub fn new(
        peer: PeerId,
    ) -> Result<(Box<Self>, HeapProd<RtpAudioPacket>, PeerAudioControls), MediaError> {
        let (prod, cons) = HeapRb::<RtpAudioPacket>::new(PEER_RING_PACKETS).split();
        let controls = PeerAudioControls::new();
        let slot = Box::new(Self {
            peer,
            packets: cons,
            jitter: JitterBuffer::new(),
            decoder: new_opus_decoder()?,
            pcm: [0; FRAME_SAMPLES],
            controls: controls.clone(),
        });
        Ok((slot, prod, controls))
    }

    /// Drains newly arrived packets and decodes the next 20 ms into `self.pcm`.
    fn produce_frame(&mut self) {
        while let Some(p) = self.packets.try_pop() {
            self.jitter.push(p.seq, p.timestamp, p.payload(), p.arrival);
        }
        let result = match self.jitter.pop() {
            Playout::Packet(data) => self.decoder.decode(data, &mut self.pcm, false),
            Playout::Fec(next) => self.decoder.decode(next, &mut self.pcm, true),
            Playout::Plc => self.decoder.decode(&[], &mut self.pcm, false),
            Playout::Silence => {
                self.pcm.fill(0);
                Ok(FRAME_SAMPLES)
            }
        };
        match result {
            Ok(n) if n < FRAME_SAMPLES => self.pcm[n..].fill(0),
            Ok(_) => {}
            Err(_) => self.pcm.fill(0),
        }
        self.controls
            .level
            .store(rms_i16(&self.pcm).to_bits(), Ordering::Relaxed);
    }
}

/// Soft-knee limiter: identity below [`KNEE`], smoothly compressed towards
/// [`CEILING`] above it. Never exceeds the `i16` range.
pub fn soft_limit(x: i32) -> i16 {
    let mag = x.unsigned_abs() as f32;
    if mag <= KNEE {
        return x as i16;
    }
    let range = CEILING - KNEE;
    let over = (mag - KNEE) / range;
    let y = KNEE + range * over.tanh();
    let y = y.min(CEILING) as i16;
    if x < 0 { -y } else { y }
}

/// Sums `inputs` (pcm, Q12 gain) into `acc`, then limits into `out`.
///
/// When `deafened` is set the output is silence regardless of the inputs.
pub fn mix_frames<'a>(
    inputs: impl Iterator<Item = (&'a [i16; FRAME_SAMPLES], i32)>,
    deafened: bool,
    acc: &mut [i32; FRAME_SAMPLES],
    out: &mut [i16; FRAME_SAMPLES],
) {
    acc.fill(0);
    if !deafened {
        for (pcm, gain) in inputs {
            if gain == 0 {
                continue;
            }
            for (a, &s) in acc.iter_mut().zip(pcm.iter()) {
                *a += (i32::from(s) * gain) >> 12;
            }
        }
    }
    for (o, &a) in out.iter_mut().zip(acc.iter()) {
        *o = soft_limit(a);
    }
}

fn gain_q12(controls: &PeerAudioControls) -> i32 {
    let v = f32::from_bits(controls.volume.load(Ordering::Relaxed));
    (v * GAIN_ONE as f32) as i32
}

/// Mixer state that does not depend on the thread or the output device. Exposed for
/// tests and the loopback example.
pub struct MixerCore {
    // Slots stay boxed: they are built (and allocated) outside the real-time thread and
    // moved in through the control channel as a pointer.
    #[allow(clippy::vec_box)]
    slots: Vec<Box<PeerSlot>>,
    acc: Box<[i32; FRAME_SAMPLES]>,
    out: Box<[i16; FRAME_SAMPLES]>,
    deafened: Arc<AtomicBool>,
}

impl MixerCore {
    /// Creates a core with capacity for [`MAX_PEERS`] slots.
    pub fn new(deafened: Arc<AtomicBool>) -> Self {
        Self {
            slots: Vec::with_capacity(MAX_PEERS),
            acc: Box::new([0; FRAME_SAMPLES]),
            out: Box::new([0; FRAME_SAMPLES]),
            deafened,
        }
    }

    /// Adds (or replaces) a peer slot. Returns the slot back if the mixer is full.
    pub fn add(&mut self, slot: Box<PeerSlot>) -> Result<(), Box<PeerSlot>> {
        if let Some(existing) = self.slots.iter_mut().find(|s| s.peer == slot.peer) {
            *existing = slot;
            return Ok(());
        }
        if self.slots.len() >= MAX_PEERS {
            return Err(slot);
        }
        self.slots.push(slot);
        Ok(())
    }

    /// Removes a peer slot.
    pub fn remove(&mut self, peer: PeerId) {
        self.slots.retain(|s| s.peer != peer);
    }

    /// Number of active slots.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether no slots are active.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Produces the next mixed 20 ms frame.
    pub fn tick(&mut self) -> &[i16; FRAME_SAMPLES] {
        for slot in self.slots.iter_mut() {
            slot.produce_frame();
        }
        let deafened = self.deafened.load(Ordering::Relaxed);
        mix_frames(
            self.slots.iter().map(|s| (&s.pcm, gain_q12(&s.controls))),
            deafened,
            &mut self.acc,
            &mut self.out,
        );
        &self.out
    }
}

/// Commands for the mixer thread.
pub enum MixerControl {
    /// Start mixing a peer (replaces an existing slot for the same peer).
    AddPeer(Box<PeerSlot>),
    /// Stop mixing a peer.
    RemovePeer(PeerId),
    /// Replace the playback sink (`None` = no device, self-paced).
    SetOutput(Option<Box<OutputSink>>),
    /// Exit the thread.
    Shutdown,
}

/// Counters published by the mixer thread.
#[derive(Debug, Default)]
pub struct MixerStats {
    /// Frames mixed.
    pub frames: AtomicU64,
    /// Iterations that took longer than 20 ms.
    pub overruns: AtomicU64,
    /// Longest iteration in microseconds.
    pub max_tick_us: AtomicU64,
    /// Samples dropped because the playback ring was full.
    pub output_dropped: AtomicU64,
}

/// Handle to the running mixer thread.
pub struct MixerHandle {
    control: SyncSender<MixerControl>,
    /// Deafen flag read by the mixer every frame.
    pub deafened: Arc<AtomicBool>,
    /// Shared counters.
    pub stats: Arc<MixerStats>,
    thread: Option<JoinHandle<()>>,
}

impl MixerHandle {
    /// Spawns the mixer thread.
    pub fn spawn(output: Option<Box<OutputSink>>) -> Result<Self, MediaError> {
        let (tx, rx) = sync_channel(32);
        let deafened = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(MixerStats::default());
        let core = MixerCore::new(deafened.clone());
        let s = stats.clone();
        let thread = std::thread::Builder::new()
            .name("cc-mixer".into())
            .spawn(move || run(core, output, rx, s))
            .map_err(|e| MediaError::Audio(format!("spawn mixer thread: {e}")))?;
        Ok(Self {
            control: tx,
            deafened,
            stats,
            thread: Some(thread),
        })
    }

    /// Sends a control message without blocking. Returns false if the queue is full or
    /// the thread has exited.
    pub fn send(&self, msg: MixerControl) -> bool {
        self.control.try_send(msg).is_ok()
    }
}

impl Drop for MixerHandle {
    fn drop(&mut self) {
        let _ = self.control.try_send(MixerControl::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(
    mut core: MixerCore,
    mut output: Option<Box<OutputSink>>,
    rx: Receiver<MixerControl>,
    stats: Arc<MixerStats>,
) {
    let _rt = match audio_thread_priority::promote_current_thread_to_real_time(
        FRAME_SAMPLES as u32,
        crate::audio::SAMPLE_RATE,
    ) {
        Ok(handle) => Some(handle),
        Err(e) => {
            tracing::warn!("mixer thread runs without real-time priority: {e}");
            None
        }
    };
    let mut last_warn: Option<Instant> = None;
    let mut next_deadline = Instant::now() + FRAME_PERIOD;

    loop {
        loop {
            match rx.try_recv() {
                Ok(MixerControl::AddPeer(slot)) => {
                    if core.add(slot).is_err() {
                        tracing::warn!("mixer full, peer not mixed");
                    }
                }
                Ok(MixerControl::RemovePeer(peer)) => core.remove(peer),
                Ok(MixerControl::SetOutput(sink)) => output = sink,
                Ok(MixerControl::Shutdown) | Err(TryRecvError::Disconnected) => return,
                Err(TryRecvError::Empty) => break,
            }
        }

        let due = match output.as_ref() {
            Some(sink) => sink.buffered_ms() < TARGET_OUTPUT_MS,
            None => Instant::now() >= next_deadline,
        };
        if !due {
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }

        let started = Instant::now();
        let frame = core.tick();
        if let Some(sink) = output.as_mut() {
            let dropped = sink.write_frame(frame);
            if dropped > 0 {
                stats
                    .output_dropped
                    .fetch_add(dropped as u64, Ordering::Relaxed);
            }
        } else {
            next_deadline += FRAME_PERIOD;
            if next_deadline < started {
                next_deadline = started + FRAME_PERIOD;
            }
        }
        let elapsed = started.elapsed();
        stats.frames.fetch_add(1, Ordering::Relaxed);
        let us = elapsed.as_micros() as u64;
        stats.max_tick_us.fetch_max(us, Ordering::Relaxed);
        if elapsed > FRAME_PERIOD {
            stats.overruns.fetch_add(1, Ordering::Relaxed);
            if last_warn.is_none_or(|t| t.elapsed() >= WARN_INTERVAL) {
                last_warn = Some(Instant::now());
                tracing::warn!(elapsed_us = us, "mixer frame exceeded the 20 ms budget");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{MAX_OPUS_PACKET, new_opus_encoder};
    use ringbuf::traits::Producer;

    fn frame(v: i16) -> [i16; FRAME_SAMPLES] {
        [v; FRAME_SAMPLES]
    }

    #[test]
    fn sums_inputs() {
        let a = frame(1000);
        let b = frame(-300);
        let c = frame(2000);
        let mut acc = [0; FRAME_SAMPLES];
        let mut out = [0; FRAME_SAMPLES];
        mix_frames(
            [(&a, GAIN_ONE), (&b, GAIN_ONE), (&c, GAIN_ONE)].into_iter(),
            false,
            &mut acc,
            &mut out,
        );
        assert!(out.iter().all(|&s| s == 2700));
    }

    #[test]
    fn silence_in_silence_out() {
        let z = frame(0);
        let mut acc = [0; FRAME_SAMPLES];
        let mut out = [1; FRAME_SAMPLES];
        mix_frames(
            [(&z, GAIN_ONE), (&z, GAIN_ONE)].into_iter(),
            false,
            &mut acc,
            &mut out,
        );
        assert!(out.iter().all(|&s| s == 0));
        mix_frames(std::iter::empty(), false, &mut acc, &mut out);
        assert!(out.iter().all(|&s| s == 0));
    }

    #[test]
    fn limiter_bounds_and_preserves_quiet_signals() {
        assert_eq!(soft_limit(0), 0);
        assert_eq!(soft_limit(12_000), 12_000);
        assert_eq!(soft_limit(-12_000), -12_000);
        assert_eq!(soft_limit(24_576), 24_576);
        let loud = soft_limit(4 * 32_767);
        assert!(loud > 30_000, "{loud}");
        assert!(soft_limit(-4 * 32_767) < -30_000);
        // Monotonic above the knee.
        let mut prev = soft_limit(24_000);
        for x in (24_100..200_000).step_by(997) {
            let y = soft_limit(x);
            assert!(y >= prev, "not monotonic at {x}");
            prev = y;
        }
    }

    #[test]
    fn loud_sum_is_limited_not_wrapped() {
        let a = frame(30_000);
        let mut acc = [0; FRAME_SAMPLES];
        let mut out = [0; FRAME_SAMPLES];
        mix_frames([(&a, GAIN_ONE); 4].into_iter(), false, &mut acc, &mut out);
        assert!(out.iter().all(|&s| s > 30_000));
        let n = frame(-30_000);
        mix_frames([(&n, GAIN_ONE); 4].into_iter(), false, &mut acc, &mut out);
        assert!(out.iter().all(|&s| s < -30_000));
    }

    #[test]
    fn volume_scales_and_mutes() {
        let a = frame(1000);
        let mut acc = [0; FRAME_SAMPLES];
        let mut out = [0; FRAME_SAMPLES];
        mix_frames([(&a, GAIN_ONE / 2)].into_iter(), false, &mut acc, &mut out);
        assert!(out.iter().all(|&s| s == 500));
        mix_frames([(&a, GAIN_ONE * 2)].into_iter(), false, &mut acc, &mut out);
        assert!(out.iter().all(|&s| s == 2000));
        mix_frames([(&a, 0)].into_iter(), false, &mut acc, &mut out);
        assert!(out.iter().all(|&s| s == 0));
    }

    #[test]
    fn deafen_outputs_silence() {
        let a = frame(5000);
        let mut acc = [0; FRAME_SAMPLES];
        let mut out = [0; FRAME_SAMPLES];
        mix_frames([(&a, GAIN_ONE)].into_iter(), true, &mut acc, &mut out);
        assert!(out.iter().all(|&s| s == 0));
    }

    #[test]
    fn controls_clamp_volume() {
        let c = PeerAudioControls::new();
        c.set_volume(5.0);
        assert_eq!(gain_q12(&c), 5 * GAIN_ONE);
        c.set_volume(9.0);
        assert_eq!(gain_q12(&c), 5 * GAIN_ONE);
        c.set_volume(-1.0);
        assert_eq!(gain_q12(&c), 0);
        c.set_volume(f32::NAN);
        assert_eq!(gain_q12(&c), GAIN_ONE);
    }

    #[test]
    fn core_decodes_and_mixes_real_opus() {
        let deaf = Arc::new(AtomicBool::new(false));
        let mut core = MixerCore::new(deaf.clone());
        let peer = PeerId::random();
        let (slot, mut prod, controls) = PeerSlot::new(peer).unwrap();
        core.add(slot).map_err(|_| ()).unwrap();

        let mut enc = new_opus_encoder().unwrap();
        let t0 = Instant::now();
        let mut energy = 0.0f32;
        let mut peak_level = 0.0f32;
        for n in 0..50u16 {
            let pcm: Vec<f32> = (0..FRAME_SAMPLES)
                .map(|i| ((usize::from(n) * FRAME_SAMPLES + i) as f32 * 0.03).sin() * 0.4)
                .collect();
            let mut buf = [0u8; MAX_OPUS_PACKET];
            let len = enc.encode_float(&pcm, &mut buf).unwrap();
            let at = t0 + Duration::from_millis(u64::from(n) * 20);
            prod.try_push(RtpAudioPacket::new(n, u32::from(n) * 960, &buf[..len], at).unwrap())
                .map_err(|_| ())
                .unwrap();
            let out = core.tick();
            energy += rms_i16(out);
            peak_level = peak_level.max(controls.level());
        }
        assert!(energy > 1.0, "mixed energy {energy}");
        // A steady sine eventually triggers Opus DTX, so check the peak level.
        assert!(peak_level > 0.2, "peak level {peak_level}");

        deaf.store(true, Ordering::Relaxed);
        assert!(core.tick().iter().all(|&s| s == 0));

        core.remove(peer);
        assert!(core.is_empty());
    }

    #[test]
    fn core_rejects_more_than_max_peers_and_replaces_duplicates() {
        let mut core = MixerCore::new(Arc::new(AtomicBool::new(false)));
        let first = PeerId::random();
        let (slot, _p, _c) = PeerSlot::new(first).unwrap();
        assert!(core.add(slot).is_ok());
        let (slot, _p2, _c2) = PeerSlot::new(first).unwrap();
        assert!(core.add(slot).is_ok());
        assert_eq!(core.len(), 1);
        for _ in 1..MAX_PEERS {
            let (slot, _p, _c) = PeerSlot::new(PeerId::random()).unwrap();
            assert!(core.add(slot).is_ok());
        }
        let (slot, _p, _c) = PeerSlot::new(PeerId::random()).unwrap();
        assert!(core.add(slot).is_err());
    }
}
