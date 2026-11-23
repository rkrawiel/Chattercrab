//! Notification cues: short synthesized tones mixed into the playback.
//!
//! Each cue is a sequence of sine notes, each with a smooth attack, an exponential decay
//! and a short release to zero, so notes start and end without a click. The mixer renders
//! every cue once (48 kHz mono `i16`) when it is created, outside the real-time thread.

use std::f32::consts::TAU;

use crate::Cue;
use crate::audio::SAMPLE_RATE;

/// Peak amplitude as a fraction of full scale, quiet enough not to startle.
const PEAK: f32 = 0.25;
/// Fade-in at the start of every note.
const ATTACK_S: f32 = 0.005;
/// Fade-out to zero at the end of every note.
const RELEASE_S: f32 = 0.015;
/// Decay over one note: the amplitude falls to `exp(-DECAY)` by the end.
const DECAY: f32 = 1.2;

const E5: f32 = 659.26;
const A5: f32 = 880.0;
const E6: f32 = 1318.51;

/// `(frequency in Hz, duration in seconds)` per note; frequency 0 is a rest.
fn notes(cue: Cue) -> &'static [(f32, f32)] {
    match cue {
        Cue::Join => &[(E5, 0.09), (A5, 0.13)],
        Cue::Leave => &[(A5, 0.09), (E5, 0.13)],
        Cue::Message => &[(E6, 0.06), (0.0, 0.03), (E6, 0.06)],
    }
}

fn smoothstep(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

/// Gain at `t` seconds into a note of `len` seconds.
fn envelope(t: f32, len: f32) -> f32 {
    smoothstep(t / ATTACK_S) * smoothstep((len - t) / RELEASE_S) * (-DECAY * t / len).exp()
}

/// Renders `cue` as 48 kHz mono samples.
pub(crate) fn render(cue: Cue) -> Vec<i16> {
    let rate = SAMPLE_RATE as f32;
    let mut out = Vec::new();
    for &(freq, len) in notes(cue) {
        let n = (len * rate) as usize;
        out.extend((0..n).map(|i| {
            let t = i as f32 / rate;
            let s = (TAU * freq * t).sin() * envelope(t, len) * PEAK;
            (s * f32::from(i16::MAX)) as i16
        }));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const CUES: [Cue; 3] = [Cue::Join, Cue::Leave, Cue::Message];

    /// Power of `freq` in `samples` (Goertzel).
    fn power(samples: &[i16], freq: f32) -> f32 {
        let k = 2.0 * (TAU * freq / SAMPLE_RATE as f32).cos();
        let (mut s1, mut s2) = (0.0f32, 0.0f32);
        for &x in samples {
            let s0 = f32::from(x) + k * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        s1 * s1 + s2 * s2 - k * s1 * s2
    }

    /// First and last quarter of `cue`.
    fn ends(cue: Cue) -> (Vec<i16>, Vec<i16>) {
        let s = render(cue);
        let q = s.len() / 4;
        (s[..q].to_vec(), s[s.len() - q..].to_vec())
    }

    #[test]
    fn join_rises_and_leave_falls() {
        let (first, second) = ends(Cue::Join);
        assert!(power(&first, E5) > 10.0 * power(&first, A5));
        assert!(power(&second, A5) > 10.0 * power(&second, E5));
        let (first, second) = ends(Cue::Leave);
        assert!(power(&first, A5) > 10.0 * power(&first, E5));
        assert!(power(&second, E5) > 10.0 * power(&second, A5));
    }

    #[test]
    fn message_is_a_high_double_blip() {
        let s = render(Cue::Message);
        assert!(power(&s, E6) > 10.0 * power(&s, A5));
        assert!(power(&s, E6) > 10.0 * power(&s, E5));
        // A silent gap in the middle separates the two blips.
        let mid = s.len() / 2;
        assert!(s[mid - 100..mid + 100].iter().all(|&x| x == 0));
    }

    #[test]
    fn cues_are_short_bounded_and_click_free() {
        let full = f32::from(i16::MAX);
        for cue in CUES {
            let s = render(cue);
            let ms = s.len() * 1000 / SAMPLE_RATE as usize;
            assert!((100..=250).contains(&ms), "{cue:?} lasts {ms} ms");
            let peak = s.iter().map(|x| x.unsigned_abs()).max().unwrap();
            assert!(f32::from(peak) <= PEAK * full, "{cue:?} peak {peak}");
            assert!(f32::from(peak) >= 0.15 * full, "{cue:?} peak {peak}");
            // Starts and ends near zero.
            let edge = 0.01 * full;
            assert!(
                s[..10].iter().all(|&x| f32::from(x).abs() < edge),
                "{cue:?}"
            );
            assert!(
                s[s.len() - 10..].iter().all(|&x| f32::from(x).abs() < edge),
                "{cue:?}"
            );
        }
    }
}
