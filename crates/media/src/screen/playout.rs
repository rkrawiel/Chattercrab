//! Screen-share playout clock and encoded-frame buffer.
//!
//! Pictures are shown on the RTP timestamp clock, up to one second behind the
//! newest frame. Arrival jitter and a slow viewer become delay. Frames are not
//! dropped to catch up until that second is used up, and they are never shown
//! in a burst.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// H.264 RTP clock rate.
pub(super) const RTP_CLOCK_HZ: u32 = 90_000;
/// A viewer may sit this far behind the newest picture.
pub(super) const MAX_PLAYOUT_DELAY: Duration = Duration::from_secs(1);
/// Delay before the first picture. Grows toward [`MAX_PLAYOUT_DELAY`] when
/// arrivals are uneven, and stays there so playback does not speed up again.
pub(super) const MIN_PLAYOUT_DELAY: Duration = Duration::from_millis(100);
/// Encoded access units kept when the decoder falls behind the cap. Past this
/// the buffer is discarded and the viewer waits for the next keyframe.
const MAX_BUFFERED_FRAMES: usize = 180;
const MAX_BUFFER_SPAN: Duration = Duration::from_millis(1200);

/// What to do with the oldest buffered access unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Step {
    /// Leave it queued until more media arrives or the preroll timer fires.
    Buffer,
    /// Show it at `at` (never earlier).
    Present {
        /// Wall-clock time to hand the picture to the UI.
        at: Instant,
    },
    /// Decode it so later pictures stay valid, and do not show it.
    DecodeOnly,
}

/// Maps RTP timestamps onto a steady display clock.
#[derive(Debug)]
pub(super) struct PlayoutClock {
    delay: Duration,
    origin_rtp: u32,
    origin_at: Instant,
    started: bool,
    frame_ticks: u32,
    frame_samples: u32,
    buffering_since: Option<Instant>,
    last_shown: Option<Instant>,
    last_rtp: Option<u32>,
    last_arrival: Option<Instant>,
    jitter_q4: u32,
    skipped: u64,
}

impl PlayoutClock {
    /// Clock that has not shown a frame yet.
    pub(super) fn new() -> Self {
        Self {
            delay: MIN_PLAYOUT_DELAY,
            origin_rtp: 0,
            origin_at: Instant::now(),
            started: false,
            frame_ticks: RTP_CLOCK_HZ / 30,
            frame_samples: 0,
            buffering_since: None,
            last_shown: None,
            last_rtp: None,
            last_arrival: None,
            jitter_q4: 0,
            skipped: 0,
        }
    }

    /// Forgets the current timeline after a gap or a new keyframe.
    pub(super) fn reset(&mut self) {
        let delay = self.delay;
        let frame_ticks = self.frame_ticks;
        let jitter_q4 = self.jitter_q4;
        *self = Self::new();
        self.delay = delay;
        self.frame_ticks = frame_ticks;
        self.jitter_q4 = jitter_q4;
    }

    /// Whether playout has left the preroll.
    pub(super) fn is_started(&self) -> bool {
        self.started
    }

    /// Estimated time between pictures.
    pub(super) fn frame_duration(&self) -> Duration {
        ticks_to_duration(self.frame_ticks.max(1)).max(Duration::from_millis(1))
    }

    /// Current preroll / cushion.
    pub(super) fn delay(&self) -> Duration {
        self.delay
    }

    /// Pictures skipped because the buffer was past [`MAX_PLAYOUT_DELAY`].
    pub(super) fn skipped(&self) -> u64 {
        self.skipped
    }

    /// Records one access unit as it arrives, before it is shown.
    pub(super) fn observe(&mut self, rtp_ts: u32, arrival: Instant) {
        if let Some(prev) = self.last_rtp {
            let delta = rtp_ts.wrapping_sub(prev);
            if delta > 0 && delta < RTP_CLOCK_HZ {
                self.frame_samples = self.frame_samples.saturating_add(1);
                self.frame_ticks = if self.frame_samples < 8 {
                    let samples = u64::from(self.frame_samples);
                    ((u64::from(self.frame_ticks) * (samples - 1) + u64::from(delta)) / samples)
                        as u32
                } else {
                    (self.frame_ticks * 7 + delta) / 8
                };
                self.note_jitter(prev, rtp_ts, arrival);
            }
        }
        self.last_rtp = Some(rtp_ts);
        self.last_arrival = Some(arrival);
        self.raise_delay(self.target_from_jitter());
    }

    /// Chooses what to do with the oldest queued picture at `now`.
    ///
    /// `oldest` and `newest` are RTP timestamps of the frames still buffered.
    pub(super) fn step(&mut self, oldest: u32, newest: u32, now: Instant) -> Step {
        let delta = newest.wrapping_sub(oldest);
        if delta > RTP_CLOCK_HZ * 2 {
            self.restart(oldest, now);
            return Step::Present { at: now };
        }
        let span = ticks_to_duration(delta);
        if !self.started {
            let since = *self.buffering_since.get_or_insert(now);
            let ready =
                span >= self.preroll() || now.saturating_duration_since(since) >= self.preroll();
            if !ready {
                return Step::Buffer;
            }
            self.restart(oldest, now);
            return Step::Present { at: now };
        }
        if delta > duration_to_ticks(MAX_PLAYOUT_DELAY) {
            self.skipped = self.skipped.saturating_add(1);
            return Step::DecodeOnly;
        }
        let rtp_due = self
            .origin_at
            .checked_add(ticks_to_duration(oldest.wrapping_sub(self.origin_rtp)))
            .unwrap_or(now);
        let earliest = self
            .last_shown
            .map(|shown| shown + self.frame_duration())
            .unwrap_or(now);
        let at = if rtp_due > earliest {
            rtp_due
        } else {
            earliest
        };
        if at > now {
            Step::Present { at }
        } else {
            Step::Present { at: now }
        }
    }

    /// The picture was handed to the UI at `at`. The next one is not shown
    /// sooner than one frame after this.
    pub(super) fn did_show(&mut self, at: Instant) {
        self.last_shown = Some(at);
    }

    /// The buffer ran dry. The next pictures wait for [`Self::preroll`] again.
    pub(super) fn underrun(&mut self) {
        let extra = self.frame_duration().max(Duration::from_millis(20));
        self.delay = (self.delay + extra).min(MAX_PLAYOUT_DELAY);
        self.started = false;
        self.buffering_since = None;
        self.last_shown = None;
    }

    fn preroll(&self) -> Duration {
        let three_frames = self.frame_duration() * 3;
        self.delay
            .max(three_frames)
            .clamp(MIN_PLAYOUT_DELAY, MAX_PLAYOUT_DELAY)
    }

    fn restart(&mut self, rtp: u32, now: Instant) {
        self.started = true;
        self.origin_rtp = rtp;
        self.origin_at = now;
        self.buffering_since = None;
        self.last_shown = None;
    }

    fn target_from_jitter(&self) -> Duration {
        let jitter = Duration::from_secs_f64(f64::from(self.jitter_ms()) / 1000.0);
        let target = jitter * 3 + self.frame_duration() * 2;
        target.clamp(MIN_PLAYOUT_DELAY, MAX_PLAYOUT_DELAY)
    }

    fn jitter_ms(&self) -> f32 {
        (self.jitter_q4 as f32 / 16.0) * 1000.0 / (RTP_CLOCK_HZ as f32)
    }

    fn note_jitter(&mut self, prev_rtp: u32, rtp: u32, arrival: Instant) {
        let Some(prev_arrival) = self.last_arrival else {
            return;
        };
        let rtp_delta = i64::from(rtp.wrapping_sub(prev_rtp));
        let arrival_ticks = arrival
            .saturating_duration_since(prev_arrival)
            .as_secs_f64()
            * f64::from(RTP_CLOCK_HZ);
        let d = i64::from(
            u32::try_from(
                (arrival_ticks as i64 - rtp_delta)
                    .unsigned_abs()
                    .min(u64::from(RTP_CLOCK_HZ)),
            )
            .unwrap_or(RTP_CLOCK_HZ),
        );
        let j = i64::from(self.jitter_q4);
        let next = (j + (d * 16 - j) / 16).max(0);
        self.jitter_q4 = u32::try_from(next).unwrap_or(u32::MAX);
    }

    fn raise_delay(&mut self, target: Duration) {
        let target = target.clamp(MIN_PLAYOUT_DELAY, MAX_PLAYOUT_DELAY);
        let slack = self.frame_duration() / 2;
        if target <= self.delay.saturating_add(slack) {
            return;
        }
        let add = target - self.delay;
        self.delay = target;
        if self.started {
            self.origin_at += add;
        }
    }
}

/// Shared clock the RTP reader and the decoder thread both touch.
#[derive(Clone)]
pub(super) struct SharedClock {
    inner: Arc<Mutex<PlayoutClock>>,
}

impl SharedClock {
    pub(super) fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(PlayoutClock::new())),
        }
    }

    pub(super) fn observe(&self, rtp_ts: u32, arrival: Instant) {
        self.lock().observe(rtp_ts, arrival);
    }

    pub(super) fn with_mut<R>(&self, f: impl FnOnce(&mut PlayoutClock) -> R) -> R {
        f(&mut self.lock())
    }

    fn lock(&self) -> MutexGuard<'_, PlayoutClock> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// One encoded picture waiting to be decoded.
pub(super) struct TimedAu {
    pub data: Vec<u8>,
    pub rtp_ts: u32,
    pub keyframe: bool,
}

/// Why [`AuBuffer::push`] did not keep the access unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PushError {
    /// The buffer was past the cap and has been cleared. Wait for an IDR.
    Overflow,
    /// The receiver has stopped.
    Closed,
}

struct AuState {
    frames: VecDeque<TimedAu>,
    closed: bool,
    overflow: bool,
    /// Bumps on every push, pop, clear, or close so a waiter can tell that the
    /// queue changed even when the length happens to match.
    generation: u64,
}

/// Bounded queue of encoded access units between the RTP reader and the decoder.
pub(super) struct AuBuffer {
    state: Mutex<AuState>,
    cv: Condvar,
}

/// A look at the oldest and newest queued pictures.
#[derive(Debug, Clone, Copy)]
pub(super) struct Peek {
    pub oldest: u32,
    pub newest: u32,
    pub generation: u64,
    pub keyframe: bool,
}

/// Result of waiting for the encoded queue.
#[derive(Debug)]
pub(super) enum Wait {
    /// [`AuBuffer::close`] was called.
    Closed,
    /// The queue was discarded. The decoder must wait for a keyframe.
    Overflow,
    /// Nothing new arrived before the timeout. `queued` is set when pictures
    /// are still sitting there (the preroll timer ran out).
    Timeout {
        /// Pictures still queued, if any.
        queued: Option<Peek>,
    },
    /// A picture is waiting that the caller has not looked at yet.
    Ready(Peek),
}

impl AuBuffer {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(AuState {
                frames: VecDeque::new(),
                closed: false,
                overflow: false,
                generation: 0,
            }),
            cv: Condvar::new(),
        })
    }

    pub(super) fn push(&self, au: TimedAu) -> Result<(), PushError> {
        let mut guard = self.lock();
        if guard.closed {
            return Err(PushError::Closed);
        }
        guard.frames.push_back(au);
        guard.generation = guard.generation.wrapping_add(1);
        let overflow =
            guard.frames.len() > MAX_BUFFERED_FRAMES || span_of(&guard.frames) > MAX_BUFFER_SPAN;
        if overflow {
            guard.frames.clear();
            guard.overflow = true;
            guard.generation = guard.generation.wrapping_add(1);
        }
        self.cv.notify_all();
        if overflow {
            Err(PushError::Overflow)
        } else {
            Ok(())
        }
    }

    pub(super) fn close(&self) {
        let mut guard = self.lock();
        guard.closed = true;
        guard.generation = guard.generation.wrapping_add(1);
        self.cv.notify_all();
    }

    pub(super) fn is_closed(&self) -> bool {
        self.lock().closed
    }

    /// Takes and clears the overflow flag.
    pub(super) fn take_overflow(&self) -> bool {
        let mut guard = self.lock();
        let overflow = guard.overflow;
        guard.overflow = false;
        overflow
    }

    pub(super) fn pop(&self) -> Option<TimedAu> {
        let mut guard = self.lock();
        let au = guard.frames.pop_front()?;
        guard.generation = guard.generation.wrapping_add(1);
        self.cv.notify_all();
        Some(au)
    }

    /// Timestamp span from the oldest queued picture to the newest.
    pub(super) fn span(&self) -> Duration {
        span_of(&self.lock().frames)
    }

    /// Waits until `generation` changes, the buffer closes, or `timeout` elapses.
    pub(super) fn wait_generation(&self, generation: u64, timeout: Duration) -> Wait {
        let mut guard = self.lock();
        let deadline = Instant::now() + timeout;
        loop {
            if guard.closed {
                return Wait::Closed;
            }
            if guard.overflow {
                return Wait::Overflow;
            }
            if guard.generation != generation
                && !guard.frames.is_empty()
                && let Some(peek) = peek_of(&guard)
            {
                return Wait::Ready(peek);
            }
            let now = Instant::now();
            if now >= deadline {
                return Wait::Timeout {
                    queued: peek_of(&guard),
                };
            }
            let wait = deadline.saturating_duration_since(now);
            let waited = self.cv.wait_timeout(guard, wait);
            let (next, status) = match waited {
                Ok(pair) => pair,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard = next;
            if status.timed_out() && !guard.closed && !guard.overflow {
                let changed = guard.generation != generation && !guard.frames.is_empty();
                if !changed {
                    return Wait::Timeout {
                        queued: peek_of(&guard),
                    };
                }
            }
        }
    }

    /// Sleeps until `deadline` or until the buffer is closed.
    /// Returns false when the receiver has stopped.
    pub(super) fn sleep_until(&self, deadline: Instant) -> bool {
        let mut guard = self.lock();
        loop {
            if guard.closed {
                return false;
            }
            let now = Instant::now();
            if now >= deadline {
                return true;
            }
            let waited = self
                .cv
                .wait_timeout(guard, deadline.saturating_duration_since(now));
            guard = match waited {
                Ok((guard, _)) => guard,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }

    fn lock(&self) -> MutexGuard<'_, AuState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn peek_of(state: &AuState) -> Option<Peek> {
    let first = state.frames.front()?;
    let last = state.frames.back()?;
    Some(Peek {
        oldest: first.rtp_ts,
        newest: last.rtp_ts,
        generation: state.generation,
        keyframe: first.keyframe,
    })
}

fn span_of(frames: &VecDeque<TimedAu>) -> Duration {
    let (Some(first), Some(last)) = (frames.front(), frames.back()) else {
        return Duration::ZERO;
    };
    ticks_to_duration(last.rtp_ts.wrapping_sub(first.rtp_ts))
}

pub(super) fn ticks_to_duration(ticks: u32) -> Duration {
    Duration::from_nanos(u64::from(ticks) * 1_000_000_000 / u64::from(RTP_CLOCK_HZ))
}

fn duration_to_ticks(duration: Duration) -> u32 {
    let ticks = duration.as_secs_f64() * f64::from(RTP_CLOCK_HZ);
    if ticks <= 1.0 {
        1
    } else if ticks >= f64::from(u32::MAX) {
        u32::MAX
    } else {
        ticks.round() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn preroll_holds_then_shows_the_first_frame_immediately() {
        let mut clock = PlayoutClock::new();
        clock.frame_ticks = 1_500;
        let t0 = Instant::now();
        assert_eq!(clock.step(0, 0, t0), Step::Buffer);
        let newest = 6 * 1_500;
        match clock.step(0, newest, t0) {
            Step::Present { at } => assert_eq!(at, t0),
            other => panic!("expected the first frame, got {other:?}"),
        }
        assert!(clock.is_started());
    }

    #[test]
    fn preroll_starts_when_the_wall_timer_expires() {
        let mut clock = PlayoutClock::new();
        let t0 = Instant::now();
        assert_eq!(clock.step(0, 0, t0), Step::Buffer);
        match clock.step(0, 0, at(t0, 100)) {
            Step::Present { at: shown } => assert_eq!(shown, at(t0, 100)),
            other => panic!("expected start after the timer, got {other:?}"),
        }
    }

    #[test]
    fn steady_frames_stay_one_interval_apart() {
        let mut clock = PlayoutClock::new();
        let ticks = 1_500u32;
        clock.frame_ticks = ticks;
        let t0 = Instant::now();
        let newest = 8 * ticks;
        let Step::Present { at } = clock.step(0, newest, t0) else {
            panic!("preroll should start");
        };
        clock.did_show(at);
        let mut now = at;
        let mut shown = vec![now];
        for n in 1..8u32 {
            let rtp = n * ticks;
            match clock.step(rtp, newest, now) {
                Step::Present { at } => {
                    now = at;
                    clock.did_show(at);
                    shown.push(at);
                }
                other => panic!("frame {n}: {other:?}"),
            }
        }
        let expected = ticks_to_duration(ticks);
        for pair in shown.windows(2) {
            let gap = pair[1].saturating_duration_since(pair[0]);
            let drift = gap.abs_diff(expected);
            assert!(
                drift < Duration::from_millis(2),
                "gap {gap:?}, expected {expected:?}"
            );
        }
    }

    #[test]
    fn a_late_frame_does_not_make_the_next_one_immediate() {
        let mut clock = PlayoutClock::new();
        clock.frame_ticks = 1_500;
        clock.started = true;
        let t0 = Instant::now();
        clock.origin_rtp = 0;
        clock.origin_at = t0;
        clock.last_shown = Some(t0);
        let late = at(t0, 40);
        clock.did_show(late);
        match clock.step(3_000, 3_000, late) {
            Step::Present { at } => {
                let gap = at.saturating_duration_since(late);
                assert!(
                    gap >= Duration::from_millis(15),
                    "next frame followed after only {gap:?}"
                );
            }
            other => panic!("expected a timed frame, got {other:?}"),
        }
    }

    #[test]
    fn a_buffer_deeper_than_one_second_skips_until_it_fits() {
        let mut clock = PlayoutClock::new();
        clock.frame_ticks = 1_500;
        clock.started = true;
        let t0 = Instant::now();
        clock.origin_rtp = 0;
        clock.origin_at = t0;
        clock.last_shown = Some(t0);
        let newest = 119 * 1_500;
        let now = at(t0, 50);
        let mut oldest_i = 0u32;
        loop {
            let oldest = oldest_i * 1_500;
            match clock.step(oldest, newest, now) {
                Step::DecodeOnly => oldest_i += 1,
                Step::Present { .. } => break,
                Step::Buffer => panic!("should not rebuffer while frames are queued"),
            }
            assert!(oldest_i < 120, "skipped every frame");
        }
        let remain = newest - oldest_i * 1_500;
        assert!(remain <= 90_000, "still {remain} ticks behind");
        assert!(clock.skipped() > 0);
    }

    #[test]
    fn underrun_waits_for_a_deeper_preroll() {
        let mut clock = PlayoutClock::new();
        clock.frame_ticks = 1_500;
        let t0 = Instant::now();
        let _ = clock.step(0, 6 * 1_500, t0);
        assert!(clock.is_started());
        let before = clock.delay();
        clock.underrun();
        assert!(!clock.is_started());
        assert!(clock.delay() > before);
        assert_eq!(clock.step(0, 0, t0), Step::Buffer);
    }

    #[test]
    fn overflow_clears_the_encoded_buffer() {
        let buffer = AuBuffer::new();
        let mut overflowed = false;
        for n in 0..200u32 {
            let result = buffer.push(TimedAu {
                data: vec![n as u8],
                rtp_ts: n * 1_500,
                keyframe: n == 0,
            });
            if result == Err(PushError::Overflow) {
                overflowed = true;
                break;
            }
        }
        assert!(overflowed);
        assert!(buffer.pop().is_none());
        assert!(buffer.take_overflow());
        assert!(!buffer.take_overflow());
    }

    #[test]
    fn close_wakes_a_waiter() {
        let buffer = AuBuffer::new();
        let waiting = Arc::clone(&buffer);
        let thread = std::thread::spawn(move || waiting.wait_generation(0, Duration::from_secs(5)));
        buffer.close();
        match thread.join() {
            Ok(Wait::Closed) => {}
            Ok(other) => panic!("waiter returned {other:?}"),
            Err(_) => panic!("waiter thread panicked"),
        }
    }
}
