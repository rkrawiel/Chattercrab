//! System-audio samples mixed into the microphone frame while screen sharing.

use std::sync::{Arc, Mutex};

use ringbuf::HeapCons;
use ringbuf::traits::{Consumer, Observer};

use super::{FRAME_SAMPLES, SAMPLE_RATE};

/// How much captured system audio may sit ahead of the encoder before the oldest
/// samples are dropped. Keeps a shared video's sound from drifting behind the picture.
const MAX_LAG_SAMPLES: usize = (SAMPLE_RATE as usize) / 10;

/// Consumer end of the system-audio ring. The encoder thread mixes whatever is
/// available into each microphone frame. Empty (or detached) leaves the frame unchanged.
pub struct ShareMix {
    cons: Mutex<Option<HeapCons<f32>>>,
}

impl ShareMix {
    /// No system audio until [`ShareMix::attach`].
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            cons: Mutex::new(None),
        })
    }

    /// Replaces the current consumer. Samples already queued on the previous one are dropped.
    pub fn attach(&self, cons: HeapCons<f32>) {
        *self.cons.lock().unwrap_or_else(|e| e.into_inner()) = Some(cons);
    }

    /// Stops mixing. Safe to call when nothing is attached.
    pub fn detach(&self) {
        *self.cons.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Adds queued system-audio samples onto `frame` and clamps the sum to [-1, 1].
    ///
    /// A shortfall mixes as silence for the missing tail, so the microphone frame is
    /// never delayed. A backlog longer than 100 ms is skipped so the mix stays live.
    pub fn mix_into(&self, frame: &mut [f32]) {
        let mut guard = self.cons.lock().unwrap_or_else(|e| e.into_inner());
        let Some(cons) = guard.as_mut() else {
            return;
        };
        let need = frame.len();
        if need == 0 || need > FRAME_SAMPLES {
            return;
        }
        let have = cons.occupied_len();
        if have == 0 {
            return;
        }
        if have > need + MAX_LAG_SAMPLES {
            let _ = cons.skip(have - need);
        }
        let mut extra = [0f32; FRAME_SAMPLES];
        let n = cons.pop_slice(&mut extra[..need]);
        for i in 0..n {
            frame[i] = (frame[i] + extra[i]).clamp(-1.0, 1.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ringbuf::HeapRb;
    use ringbuf::traits::{Producer, Split};

    fn attached(samples: &[f32]) -> Arc<ShareMix> {
        let ring = HeapRb::<f32>::new(samples.len().max(8) + MAX_LAG_SAMPLES);
        let (mut prod, cons) = ring.split();
        assert_eq!(prod.push_slice(samples), samples.len());
        let mix = ShareMix::new();
        mix.attach(cons);
        mix
    }

    #[test]
    fn mixes_and_clamps() {
        let mix = attached(&[0.75, 0.75]);
        let mut frame = [0.5, 0.5, 0.25];
        mix.mix_into(&mut frame);
        assert!((frame[0] - 1.0).abs() < 1e-6);
        assert!((frame[1] - 1.0).abs() < 1e-6);
        // The third sample had no system audio behind it.
        assert!((frame[2] - 0.25).abs() < 1e-6);
    }

    #[test]
    fn detached_mix_is_a_no_op() {
        let mix = ShareMix::new();
        let mut frame = [0.2, -0.2];
        mix.mix_into(&mut frame);
        assert_eq!(frame, [0.2, -0.2]);
    }

    #[test]
    fn drops_backlog_past_the_lag_limit() {
        let stale = MAX_LAG_SAMPLES + 4;
        let mut samples = vec![0.0; stale];
        samples.push(0.5);
        let mix = attached(&samples);
        let mut frame = [0.0];
        mix.mix_into(&mut frame);
        assert!((frame[0] - 0.5).abs() < 1e-6, "mixed {}", frame[0]);
    }
}
