//! Segmented microphone level meter ("lights" that fill left to right with loudness).

use iced::widget::{container, row, space};
use iced::{Element, Length, Theme};

use super::Message;
use super::style::{self, MeterZone};

/// Number of segments.
pub const SEGMENTS: usize = 16;
/// Level at the empty end of the scale, in dBFS. 0 dBFS is the full end.
///
/// On this scale speech (RMS about 0.03 to 0.2, -30 to -14 dBFS) fills half to three
/// quarters of the meter, and the speaking threshold sits a little below half.
const FLOOR_DB: f32 = -60.0;
/// Segments from this index on are yellow.
const LOUD_FROM: usize = 11;
/// Segments from this index on are red.
const HOT_FROM: usize = 14;

/// Microphone RMS (0.0..=1.0) as a fraction of the meter on a dB scale.
pub fn fraction(rms: f32) -> f32 {
    if rms.is_nan() || rms <= 0.0 {
        return 0.0;
    }
    let db = 20.0 * rms.log10();
    ((db - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0)
}

/// How many segments light up for `rms`.
pub fn lit_segments(rms: f32) -> usize {
    (fraction(rms) * SEGMENTS as f32).round() as usize
}

/// Color band of segment `index` (0 is the left end).
fn zone(index: usize) -> MeterZone {
    if index >= HOT_FROM {
        MeterZone::Hot
    } else if index >= LOUD_FROM {
        MeterZone::Loud
    } else {
        MeterZone::Normal
    }
}

/// A row of [`SEGMENTS`] rounded segments filling the available width.
pub fn level_meter(rms: f32, height: f32, gap: u32) -> Element<'static, Message> {
    let lit = lit_segments(rms);
    row((0..SEGMENTS).map(|i| {
        let zone = zone(i);
        let on = i < lit;
        container(space())
            .width(Length::Fill)
            .height(height)
            .style(move |theme: &Theme| style::meter_segment(theme, zone, on))
            .into()
    }))
    .spacing(gap)
    .width(Length::Fill)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::room_model::SPEAKING_THRESHOLD;

    #[test]
    fn silence_and_garbage_show_nothing() {
        assert_eq!(lit_segments(0.0), 0);
        assert_eq!(lit_segments(-0.5), 0);
        assert_eq!(lit_segments(f32::NAN), 0);
        // -80 dBFS is below the floor.
        assert_eq!(lit_segments(1e-4), 0);
    }

    #[test]
    fn full_scale_lights_everything() {
        assert_eq!(fraction(1.0), 1.0);
        assert_eq!(lit_segments(1.0), SEGMENTS);
        // Gain can push the level past full scale.
        assert_eq!(lit_segments(3.0), SEGMENTS);
    }

    #[test]
    fn scale_is_logarithmic() {
        // -30 dBFS is the middle of a -60..0 scale.
        assert!((fraction(10_f32.powf(-1.5)) - 0.5).abs() < 1e-4);
        // Every 10x in amplitude is the same step.
        let step = fraction(0.1) - fraction(0.01);
        assert!((fraction(1.0) - fraction(0.1) - step).abs() < 1e-4);
    }

    #[test]
    fn speech_fills_about_half_to_three_quarters() {
        assert!((7..=9).contains(&lit_segments(0.03)));
        assert!((12..=13).contains(&lit_segments(0.2)));
        // Normal speech stays green; only loud peaks reach yellow.
        assert_eq!(zone(lit_segments(0.05) - 1), MeterZone::Normal);
        // The speaking ring appears a little below half.
        let at_threshold = lit_segments(SPEAKING_THRESHOLD);
        assert!((6..=8).contains(&at_threshold));
    }

    #[test]
    fn colors_go_green_yellow_red() {
        assert_eq!(zone(0), MeterZone::Normal);
        assert_eq!(zone(LOUD_FROM - 1), MeterZone::Normal);
        assert_eq!(zone(LOUD_FROM), MeterZone::Loud);
        assert_eq!(zone(HOT_FROM), MeterZone::Hot);
        assert_eq!(zone(SEGMENTS - 1), MeterZone::Hot);
    }
}
