//! Screen sharing: capture and H.264 encode (publisher) and H.264 decode (receiver).
//!
//! Publisher: a dedicated thread captures the selected monitor or window with `xcap`,
//! fits it inside 3840x2160, 2560x1440, or 1920x1080 without scaling up, and encodes H.264.
//! Windows uses the GPU's Media Foundation encoder when one is registered.
//! Linux uses VA-API when libva and a driver are installed. High quality is
//! H.264 High on both, with the same bitrate, GOP, and two-frame rate control.
//! Otherwise the publisher uses OpenH264. Encoded access units (Annex-B) leave
//! through a channel that holds one second of frames. A full channel blocks
//! capture instead of dropping pictures, so a viewer can play the stream late
//! and smooth rather than with gaps.
//!
//! Receiver: see [`receive`].

mod hw;
mod playout;
pub mod receive;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bytes::Bytes;
use fast_image_resize::images::{Image, ImageRef};
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
use openh264::OpenH264API;
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, Level, Profile,
    RateControlMode, UsageType,
};
use openh264::formats::{RgbaSliceU8, YUVBuffer};
use tokio::sync::mpsc;

use crate::{MediaError, ScreenContent, ScreenScale, ScreenSource, ScreenSourceKind};

/// Maximum encoded width (H.264 level 5.2 frame size).
pub const MAX_WIDTH: u32 = 4096;
/// Maximum encoded height (H.264 level 5.2 frame size).
pub const MAX_HEIGHT: u32 = 2304;
/// Default frame rate.
pub const DEFAULT_FPS: u32 = 15;
/// Allowed frame-rate range. 120 is available with high quality.
pub const FPS_RANGE: std::ops::RangeInclusive<u32> = 5..=120;
/// Highest frame rate of a standard (not high-quality) share.
pub const STANDARD_MAX_FPS: u32 = 60;
/// Periodic IDR distance in high quality. Far enough that a call never hits it;
/// a viewer PLI/FIR still inserts an IDR, and those requests are limited to one
/// a second across every viewer.
const HIGH_QUALITY_GOP: u32 = 65_535;
/// 1440p60 desktop target for H.264. The spec's HEVC text rate is 25–35 Mbps;
/// H.264 needs about 1.5× that, which lands in the 40–70 Mbps fallback band.
const HQ_TEXT_1440P60: u64 = 45_000_000;
/// 1440p60 game target for H.264. More bits per frame than the text rate, so a
/// moving image at the same resolution and frame rate is less blurry.
const HQ_GAME_1440P60: u64 = 60_000_000;
/// Floor for the rate-control target, so a small window is not starved.
const MIN_BITRATE_BPS: u64 = 2_000_000;
/// Ceiling for the rate-control target. High enough for 2160p60 on a one-to-one share.
const MAX_BITRATE_BPS: u64 = 80_000_000;
/// How often a slow high-quality encode is logged.
const KEYFRAME_RETRY: Duration = Duration::from_secs(1);
/// Longest presentation gap stamped onto one captured picture. A longer stall
/// shows up as the viewer rebuffering instead of one frozen timestamp.
const MAX_FRAME_GAP: Duration = Duration::from_secs(1);
/// Consecutive capture failures after which publishing stops.
const MAX_CAPTURE_FAILURES: u32 = 30;

/// One encoded access unit.
#[derive(Debug, Clone)]
pub struct EncodedVideo {
    /// Annex-B NAL units of one picture.
    pub data: Bytes,
    /// Presentation duration (1 / fps).
    pub duration: Duration,
    /// Whether this is an IDR picture.
    pub keyframe: bool,
}

/// Lists capturable monitors and windows.
pub fn list_sources() -> Result<Vec<ScreenSource>, MediaError> {
    let mut out = Vec::new();
    let monitors = xcap::Monitor::all().map_err(|e| MediaError::Screen(e.to_string()))?;
    for m in monitors {
        let Ok(id) = m.id() else { continue };
        let name = m
            .friendly_name()
            .or_else(|_| m.name())
            .unwrap_or_else(|_| format!("Monitor {id}"));
        let dims = match (m.width(), m.height()) {
            (Ok(w), Ok(h)) => format!(" ({w}x{h})"),
            _ => String::new(),
        };
        out.push(ScreenSource {
            id,
            kind: ScreenSourceKind::Monitor,
            name: format!("Screen: {name}{dims}"),
        });
    }
    // Window enumeration is optional (it may be unsupported, e.g. on some Wayland setups).
    if let Ok(windows) = xcap::Window::all() {
        for w in windows {
            if w.is_minimized().unwrap_or(true) {
                continue;
            }
            let (Ok(id), Ok(title)) = (w.id(), w.title()) else {
                continue;
            };
            if title.trim().is_empty() {
                continue;
            }
            let app = w.app_name().unwrap_or_default();
            let name = if app.is_empty() || title.contains(&app) {
                title
            } else {
                format!("{app}: {title}")
            };
            out.push(ScreenSource {
                id,
                kind: ScreenSourceKind::Window,
                name: format!("Window: {name}"),
            });
        }
    }
    Ok(out)
}

/// Even output size for `scale`. Smaller than the cap is left unchanged.
pub fn encode_dimensions(src_w: u32, src_h: u32, scale: ScreenScale) -> (u32, u32) {
    let (max_w, max_h) = scale.max_dimensions();
    fit_dimensions(src_w, src_h, max_w.min(MAX_WIDTH), max_h.min(MAX_HEIGHT))
}

/// Rate-control target for a picture of `width` x `height` at `fps`.
///
/// About 0.15 bits per pixel per frame. Too low a target makes OpenH264 skip
/// pictures, which shows up as the share flashing between updates.
pub fn target_bitrate(width: u32, height: u32, fps: u32) -> u32 {
    let bps = u64::from(width) * u64::from(height) * u64::from(fps.max(1)) * 15 / 100;
    bps.clamp(MIN_BITRATE_BPS, MAX_BITRATE_BPS) as u32
}

/// Clamps `fps` to the range the chosen quality mode can encode.
pub fn clamp_fps(fps: u32, high_quality: bool) -> u32 {
    let max = if high_quality {
        *FPS_RANGE.end()
    } else {
        STANDARD_MAX_FPS
    };
    fps.clamp(*FPS_RANGE.start(), max)
}

/// High-quality CBR target, scaled from 1440p60 by pixel count and frame rate.
///
/// Desktop mode is 45 Mbps at 1440p60. Game mode is 60 Mbps at the same size
/// and rate. The ceiling is 120 Mbps.
pub fn high_quality_bitrate(width: u32, height: u32, fps: u32, game: bool) -> u32 {
    let fps = u64::from(fps.max(1));
    let reference = if game {
        HQ_GAME_1440P60
    } else {
        HQ_TEXT_1440P60
    };
    let pixels = u64::from(width) * u64::from(height);
    let ref_pixels = 2560 * 1440;
    let bps = reference.saturating_mul(pixels) / ref_pixels * fps / 60;
    bps.clamp(4_000_000, 120_000_000) as u32
}

/// `profile-level-id` the bitstream uses. Viewers advertise level 5.2 High
/// (`640034`); fmtp matching ignores the level byte, and the SPS carries the
/// real level. 1440p120 and 4K60 need level 5.2 (`640034`); slower shares fit
/// in level 5.1 (`640033`).
pub fn h264_profile_level_id(
    high_quality: bool,
    width: u32,
    height: u32,
    fps: u32,
) -> &'static str {
    if !high_quality {
        "42e034"
    } else if hw::h264_level_idc(width, height, fps) >= 52 {
        "640034"
    } else {
        "640033"
    }
}

/// Encoder operating point. One encode is fanned out to every viewer, and every
/// viewer decodes H.264, so high quality is H.264 High rather than HEVC or AV1.
pub fn encode_params(
    high_quality: bool,
    width: u32,
    height: u32,
    fps: u32,
    hardware: bool,
    game: bool,
) -> hw::EncodeParams {
    let fps = fps.max(1);
    let bitrate = if high_quality {
        high_quality_bitrate(width, height, fps, game)
    } else if hardware {
        hw::hardware_bitrate(width, height, fps)
    } else {
        target_bitrate(width, height, fps)
    };
    let gop_frames = if high_quality {
        HIGH_QUALITY_GOP
    } else {
        fps.saturating_mul(2).max(1)
    };
    // Two frames of rate-control delay. 1000 ms is the standard share's window.
    let rc_window_ms = if high_quality {
        (2_000 / fps).max(1)
    } else {
        1_000
    };
    hw::EncodeParams {
        bitrate,
        gop_frames,
        high_quality,
        game,
        rc_window_ms,
    }
}

/// Largest even size that fits within `max_w` x `max_h`, preserving aspect ratio.
pub fn fit_dimensions(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if w == 0 || h == 0 {
        return (2, 2);
    }
    let scale = (f64::from(max_w) / f64::from(w))
        .min(f64::from(max_h) / f64::from(h))
        .min(1.0);
    let even = |v: f64| ((v.round() as u32) & !1).max(2);
    (even(f64::from(w) * scale), even(f64::from(h) * scale))
}

enum Target {
    Monitor(xcap::Monitor),
    Window(xcap::Window),
}

impl Target {
    fn open(source: &ScreenSource) -> Result<Self, MediaError> {
        let not_found = || MediaError::Screen(format!("source not found: {}", source.name));
        match source.kind {
            ScreenSourceKind::Monitor => xcap::Monitor::all()
                .map_err(|e| MediaError::Screen(e.to_string()))?
                .into_iter()
                .find(|m| m.id().ok() == Some(source.id))
                .map(Target::Monitor)
                .ok_or_else(not_found),
            ScreenSourceKind::Window => xcap::Window::all()
                .map_err(|e| MediaError::Screen(e.to_string()))?
                .into_iter()
                .find(|w| w.id().ok() == Some(source.id))
                .map(Target::Window)
                .ok_or_else(not_found),
        }
    }

    fn capture(&self) -> Result<xcap::image::RgbaImage, MediaError> {
        match self {
            Target::Monitor(m) => m.capture_image(),
            Target::Window(w) => w.capture_image(),
        }
        .map_err(|e| MediaError::Screen(e.to_string()))
    }
}

/// A running screen publisher. Dropping it stops the capture thread.
pub struct ScreenPublisher {
    /// Encoded access units, bounded.
    pub frames: mpsc::Receiver<EncodedVideo>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for ScreenPublisher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScreenPublisher").finish_non_exhaustive()
    }
}

impl ScreenPublisher {
    /// Opens `source` and starts capturing at `fps` and `scale`.
    /// `fps` is clamped to 5..=60, or 5..=120 when `high_quality` is set.
    /// `content` selects desktop or game tuning and does not change `fps` or `scale`.
    /// `keyframe` is polled every frame; when set, the next frame is an IDR. Blocks
    /// until the source is opened.
    pub fn start(
        source: &ScreenSource,
        fps: u32,
        scale: ScreenScale,
        keyframe: Arc<AtomicBool>,
        high_quality: bool,
        content: ScreenContent,
    ) -> Result<Self, MediaError> {
        let fps = clamp_fps(fps, high_quality);
        let game = content.is_game();
        // One second of encoded pictures. `blocking_send` waits when this is full
        // instead of dropping a frame (a drop is what makes the share stutter).
        let (tx, rx) = mpsc::channel(fps.max(1) as usize);
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<(), MediaError>>(1);
        let source_owned = source.clone();
        let thread_stop = stop.clone();
        // Capture handles are not `Send` on every platform, so the source is opened on the
        // capture thread itself and the result reported back.
        let thread = std::thread::Builder::new()
            .name("cc-screen-capture".into())
            .spawn(move || {
                let opened = Target::open(&source_owned).and_then(|target| {
                    let first = target.capture()?;
                    let (width, height) = (first.width(), first.height());
                    let (tw, th) = encode_dimensions(width, height, scale);
                    let encoder = ScreenEncoder::open(fps, tw, th, high_quality, game)?;
                    Ok((target, first, encoder, (tw, th)))
                });
                match opened {
                    Ok((target, first, encoder, encoded)) => {
                        let _ = ready_tx.send(Ok(()));
                        CaptureWorker {
                            target,
                            fps,
                            scale,
                            high_quality,
                            game,
                            encoder,
                            encoded,
                            keyframe,
                            last_slow_encode: None,
                            tx,
                            stop: thread_stop,
                        }
                        .run(first);
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                }
            })
            .map_err(|e| MediaError::Screen(format!("spawn capture thread: {e}")))?;
        let publisher = Self {
            frames: rx,
            stop,
            thread: Some(thread),
        };
        match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => {
                tracing::info!(source = %source.name, fps, high_quality, game, "screen publisher started");
                Ok(publisher)
            }
            Ok(Err(e)) => Err(e),
            Err(_) => {
                publisher.stop.store(true, Ordering::Release);
                Err(MediaError::Screen(
                    "screen source did not open in time".into(),
                ))
            }
        }
    }
}

impl Drop for ScreenPublisher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn openh264_level(high_quality: bool, width: u32, height: u32, fps: u32) -> Level {
    if high_quality && hw::h264_level_idc(width, height, fps) < 52 {
        Level::Level_5_1
    } else {
        Level::Level_5_2
    }
}

fn new_encoder(
    fps: u32,
    width: u32,
    height: u32,
    params: hw::EncodeParams,
) -> Result<Encoder, MediaError> {
    let profile = if params.high_quality {
        Profile::High
    } else {
        Profile::Baseline
    };
    let complexity = if !params.high_quality {
        Complexity::Low
    } else if params.game {
        Complexity::Medium
    } else {
        Complexity::High
    };
    // Game mode is motion. Desktop mode is text and UI.
    let usage = if params.game {
        UsageType::CameraVideoRealTime
    } else {
        UsageType::ScreenContentRealTime
    };
    // OpenH264 only honors the bitrate target when frame skip is enabled, and it
    // has no spatial AQ or background detection for screen content. The high
    // target is large enough that a normal desktop rarely skips a picture.
    // Game mode (camera usage) keeps adaptive quantization and a long GOP.
    let config = EncoderConfig::new()
        .bitrate(BitRate::from_bps(params.bitrate))
        .max_frame_rate(FrameRate::from_hz(fps as f32))
        .usage_type(usage)
        .rate_control_mode(RateControlMode::Bitrate)
        .profile(profile)
        .level(openh264_level(params.high_quality, width, height, fps))
        .complexity(complexity)
        .intra_frame_period(IntraFramePeriod::from_num_frames(params.gop_frames.max(1)))
        .scene_change_detect(!params.game)
        .adaptive_quantization(params.game)
        .background_detection(false)
        .skip_frames(true);
    Encoder::with_api_config(OpenH264API::from_source(), config)
        .map_err(|e| MediaError::Codec(format!("openh264 encoder: {e}")))
}

/// Picture encoder. Hardware H.264 when the machine has one, otherwise OpenH264.
enum ScreenEncoder {
    Hardware(Box<hw::HwEncoder>),
    Software(Box<SoftwareEncoder>),
}

struct SoftwareEncoder {
    encoder: Encoder,
    yuv: YUVBuffer,
    width: usize,
    height: usize,
}

impl SoftwareEncoder {
    fn open(
        fps: u32,
        width: u32,
        height: u32,
        high_quality: bool,
        game: bool,
    ) -> Result<Self, MediaError> {
        let params = encode_params(high_quality, width, height, fps, false, game);
        Ok(Self {
            encoder: new_encoder(fps, width, height, params)?,
            yuv: YUVBuffer::new(width as usize, height as usize),
            width: width as usize,
            height: height as usize,
        })
    }

    fn encode(
        &mut self,
        rgba: &[u8],
        force_idr: bool,
        elapsed_ms: u64,
    ) -> Result<Vec<hw::AccessUnit>, MediaError> {
        if force_idr {
            self.encoder.force_intra_frame();
        }
        self.yuv
            .read_rgba8(RgbaSliceU8::new(rgba, (self.width, self.height)));
        let ts = openh264::Timestamp::from_millis(elapsed_ms);
        let bs = self
            .encoder
            .encode_at(&self.yuv, ts)
            .map_err(|e| MediaError::Codec(format!("h264 encode: {e}")))?;
        let mut data = Vec::new();
        bs.write_vec(&mut data);
        if data.is_empty() {
            return Ok(Vec::new());
        }
        let keyframe = matches!(bs.frame_type(), openh264::encoder::FrameType::IDR);
        Ok(vec![hw::AccessUnit { data, keyframe }])
    }
}

impl ScreenEncoder {
    fn open(
        fps: u32,
        width: u32,
        height: u32,
        high_quality: bool,
        game: bool,
    ) -> Result<Self, MediaError> {
        let params = encode_params(high_quality, width, height, fps, true, game);
        match hw::HwEncoder::open(width, height, fps, params) {
            Ok(encoder) => Ok(Self::Hardware(Box::new(encoder))),
            Err(error) => {
                tracing::warn!(%error, "hardware h264 unavailable, using openh264");
                Ok(Self::Software(Box::new(SoftwareEncoder::open(
                    fps,
                    width,
                    height,
                    high_quality,
                    game,
                )?)))
            }
        }
    }

    fn encode(
        &mut self,
        rgba: &[u8],
        force_idr: bool,
        elapsed_ms: u64,
    ) -> Result<Vec<hw::AccessUnit>, MediaError> {
        match self {
            Self::Hardware(encoder) => encoder.encode(rgba, force_idr).map_err(MediaError::Codec),
            Self::Software(encoder) => encoder.encode(rgba, force_idr, elapsed_ms),
        }
    }
}

struct CaptureWorker {
    target: Target,
    fps: u32,
    scale: ScreenScale,
    high_quality: bool,
    game: bool,
    encoder: ScreenEncoder,
    encoded: (u32, u32),
    keyframe: Arc<AtomicBool>,
    last_slow_encode: Option<Instant>,
    tx: mpsc::Sender<EncodedVideo>,
    stop: Arc<AtomicBool>,
}

impl CaptureWorker {
    fn run(mut self, first: xcap::image::RgbaImage) {
        let period = Duration::from_secs_f64(1.0 / f64::from(self.fps));
        let started = Instant::now();
        let mut resizer = Resizer::new();
        let options = ResizeOptions::new()
            .resize_alg(ResizeAlg::Convolution(FilterType::Bilinear))
            .use_alpha(false);
        let mut scaled: Option<Image<'static>> = None;
        let mut failures = 0u32;
        let mut pending = Some(first);
        let mut next = Instant::now();
        let mut last_capture = Instant::now();

        while !self.stop.load(Ordering::Acquire) {
            let image = match pending.take().map_or_else(|| self.target.capture(), Ok) {
                Ok(img) => {
                    failures = 0;
                    img
                }
                Err(e) => {
                    failures += 1;
                    if failures >= MAX_CAPTURE_FAILURES {
                        tracing::warn!("screen capture keeps failing, stopping: {e}");
                        return;
                    }
                    std::thread::sleep(period);
                    continue;
                }
            };
            let (sw, sh) = image.dimensions();
            let (tw, th) = encode_dimensions(sw, sh, self.scale);
            if (tw, th) != self.encoded {
                match ScreenEncoder::open(self.fps, tw, th, self.high_quality, self.game) {
                    Ok(encoder) => {
                        self.encoder = encoder;
                        self.encoded = (tw, th);
                    }
                    Err(e) => {
                        tracing::warn!("h264 encoder: {e}");
                        continue;
                    }
                }
            }
            let rgba: &[u8] = if (tw, th) == (sw, sh) {
                image.as_raw()
            } else {
                let dst = match scaled.as_mut() {
                    Some(img) if img.width() == tw && img.height() == th => img,
                    _ => scaled.insert(Image::new(tw, th, PixelType::U8x4)),
                };
                let src = match ImageRef::new(sw, sh, image.as_raw(), PixelType::U8x4) {
                    Ok(src) => src,
                    Err(e) => {
                        tracing::warn!("screen frame layout: {e}");
                        continue;
                    }
                };
                if let Err(e) = resizer.resize(&src, dst, &options) {
                    tracing::warn!("screen resize: {e}");
                    continue;
                }
                dst.buffer()
            };
            let force_idr = self.keyframe.swap(false, Ordering::Relaxed);
            let elapsed_ms = started.elapsed().as_millis() as u64;
            let encode_started = Instant::now();
            match self.encoder.encode(rgba, force_idr, elapsed_ms) {
                Ok(units) => {
                    if self.high_quality {
                        let took = encode_started.elapsed();
                        let overdue = self
                            .last_slow_encode
                            .is_none_or(|t| t.elapsed() >= KEYFRAME_RETRY);
                        if took > period && overdue {
                            tracing::warn!(
                                ?took,
                                ?period,
                                "high quality encode missed the frame budget"
                            );
                            self.last_slow_encode = Some(Instant::now());
                        }
                    }
                    let captured_at = Instant::now();
                    let gap = captured_at.saturating_duration_since(last_capture);
                    last_capture = captured_at;
                    // Stamp the real gap so a slow encode holds the previous picture
                    // instead of making the next frames bunch up on the viewer.
                    let stretched = gap.clamp(period, MAX_FRAME_GAP);
                    let mut stamped_gap = false;
                    for unit in units {
                        if unit.data.is_empty() {
                            continue;
                        }
                        let duration = if stamped_gap {
                            period
                        } else {
                            stamped_gap = true;
                            stretched
                        };
                        let frame = EncodedVideo {
                            data: Bytes::from(unit.data),
                            duration,
                            keyframe: unit.keyframe,
                        };
                        // Blocks once a second of frames is queued. Shutdown drops
                        // the receiver, which unblocks this send.
                        if self.tx.blocking_send(frame).is_err() {
                            return;
                        }
                    }
                }
                Err(e) => tracing::warn!("h264 encode: {e}"),
            }

            next += period;
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            } else {
                next = now;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ScreenScale;

    #[test]
    fn fit_keeps_aspect_and_even_sizes() {
        assert_eq!(fit_dimensions(1920, 1080, 1280, 720), (1280, 720));
        assert_eq!(fit_dimensions(3840, 2160, 1280, 720), (1280, 720));
        assert_eq!(fit_dimensions(1280, 1024, 1280, 720), (900, 720));
        assert_eq!(fit_dimensions(800, 600, 1280, 720), (800, 600));
        assert_eq!(fit_dimensions(801, 601, 1280, 720), (800, 600));
        assert_eq!(fit_dimensions(0, 10, 1280, 720), (2, 2));
        let (w, h) = fit_dimensions(5120, 1440, 1280, 720);
        assert!(w <= 1280 && h <= 720 && w % 2 == 0 && h % 2 == 0);
    }

    #[test]
    fn scale_keeps_aspect_and_caps_the_picture() {
        assert_eq!(
            encode_dimensions(1920, 1080, ScreenScale::P1080),
            (1920, 1080)
        );
        assert_eq!(
            encode_dimensions(3840, 2160, ScreenScale::P1080),
            (1920, 1080)
        );
        assert_eq!(
            encode_dimensions(3840, 2160, ScreenScale::P1440),
            (2560, 1440)
        );
        assert_eq!(
            encode_dimensions(1920, 1080, ScreenScale::P1440),
            (1920, 1080)
        );
        assert_eq!(encode_dimensions(801, 601, ScreenScale::P1080), (800, 600));
        assert_eq!(
            encode_dimensions(7680, 4320, ScreenScale::P1440),
            (2560, 1440)
        );
        assert_eq!(
            encode_dimensions(3840, 2160, ScreenScale::P2K),
            (2560, 1440)
        );
        assert_eq!(
            encode_dimensions(3840, 2160, ScreenScale::P4K),
            (3840, 2160)
        );
        assert_eq!(
            encode_dimensions(1920, 1080, ScreenScale::P4K),
            (1920, 1080)
        );
        assert_eq!(
            encode_dimensions(7680, 4320, ScreenScale::P4K),
            (3840, 2160)
        );
        assert_eq!(target_bitrate(1920, 1080, 30), 9_331_200);
        assert_eq!(target_bitrate(3840, 2160, 60), 74_649_600);
        assert_eq!(target_bitrate(7680, 4320, 60), MAX_BITRATE_BPS as u32);
        assert_eq!(target_bitrate(320, 240, 15), MIN_BITRATE_BPS as u32);
    }

    #[test]
    fn high_quality_bitrate_matches_the_1440p_presets() {
        assert_eq!(high_quality_bitrate(2560, 1440, 60, false), 45_000_000);
        assert_eq!(high_quality_bitrate(2560, 1440, 30, false), 22_500_000);
        assert_eq!(high_quality_bitrate(2560, 1440, 120, false), 90_000_000);
        assert_eq!(high_quality_bitrate(2560, 1440, 60, true), 60_000_000);
        assert_eq!(high_quality_bitrate(2560, 1440, 120, true), 120_000_000);
        assert_eq!(high_quality_bitrate(1920, 1080, 60, false), 25_312_500);
        assert_eq!(high_quality_bitrate(1920, 1080, 60, true), 33_750_000);
        assert_eq!(high_quality_bitrate(1920, 1080, 120, true), 67_500_000);
        assert_eq!(clamp_fps(120, false), 60);
        assert_eq!(clamp_fps(120, true), 120);
        assert_eq!(clamp_fps(4, true), 5);
        assert_eq!(h264_profile_level_id(false, 2560, 1440, 60), "42e034");
        assert_eq!(h264_profile_level_id(true, 2560, 1440, 60), "640033");
        assert_eq!(h264_profile_level_id(true, 2560, 1440, 120), "640034");
        assert_eq!(h264_profile_level_id(true, 3840, 2160, 60), "640034");
        let desktop = encode_params(true, 2560, 1440, 60, false, false);
        assert!(desktop.high_quality);
        assert!(!desktop.game);
        assert_eq!(desktop.bitrate, 45_000_000);
        assert_eq!(desktop.gop_frames, HIGH_QUALITY_GOP);
        assert_eq!(desktop.rc_window_ms, 2_000 / 60);
        let desktop_fast = encode_params(true, 2560, 1440, 120, false, false);
        assert!(!desktop_fast.game);
        assert_eq!(desktop_fast.bitrate, 90_000_000);
        let game = encode_params(true, 2560, 1440, 60, true, true);
        assert!(game.game);
        assert_eq!(game.bitrate, 60_000_000);
        assert_eq!(game.rc_window_ms, 2_000 / 60);
        let game_fast = encode_params(true, 2560, 1440, 120, true, true);
        assert_eq!(game_fast.bitrate, 120_000_000);
        assert_eq!(game_fast.rc_window_ms, 2_000 / 120);
        let standard = encode_params(false, 2560, 1440, 60, true, true);
        assert!(!standard.high_quality);
        assert!(standard.game);
        assert_eq!(standard.gop_frames, 120);
        assert_eq!(standard.bitrate, hw::hardware_bitrate(2560, 1440, 60));
    }

    #[test]
    fn encoder_produces_idr_then_decodes() {
        let params = encode_params(false, 320, 240, 15, false, false);
        let mut enc = new_encoder(15, 320, 240, params).unwrap();
        let (w, h) = (320usize, 240usize);
        let mut rgba = vec![0u8; w * h * 4];
        for (i, px) in rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            *px = [(i % 255) as u8, ((i / w) % 255) as u8, 128, 255];
        }
        let mut yuv = YUVBuffer::new(w, h);
        yuv.read_rgba8(RgbaSliceU8::new(&rgba, (w, h)));
        let bs = enc.encode(&yuv).unwrap();
        assert!(matches!(bs.frame_type(), openh264::encoder::FrameType::IDR));
        let au = bs.to_vec();
        assert!(receive::contains_idr(&au));

        let mut dec = receive::new_decoder().unwrap();
        let frame = receive::decode_to_rgba(&mut dec, &au).unwrap().unwrap();
        assert_eq!((frame.width, frame.height), (320, 240));
        assert_eq!(frame.pixels.len(), 320 * 240 * 4);
    }

    fn sps_profile_idc(au: &[u8]) -> Option<u8> {
        let mut i = 0;
        while i + 4 < au.len() {
            let (header, next) = if au[i..].starts_with(&[0, 0, 0, 1]) {
                (i + 4, i + 5)
            } else if au[i..].starts_with(&[0, 0, 1]) {
                (i + 3, i + 4)
            } else {
                i += 1;
                continue;
            };
            if au[header] & 0x1F == 7 {
                return au.get(next).copied();
            }
            i = next;
        }
        None
    }

    #[test]
    fn high_quality_encoder_uses_high_profile_and_decodes() {
        let params = encode_params(true, 320, 240, 60, false, false);
        let mut enc = new_encoder(60, 320, 240, params).unwrap();
        let (w, h) = (320usize, 240usize);
        let mut rgba = vec![0u8; w * h * 4];
        for (i, px) in rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            *px = [(i % 255) as u8, 32, 200, 255];
        }
        let mut yuv = YUVBuffer::new(w, h);
        yuv.read_rgba8(RgbaSliceU8::new(&rgba, (w, h)));
        let bs = enc.encode(&yuv).unwrap();
        assert!(matches!(bs.frame_type(), openh264::encoder::FrameType::IDR));
        let au = bs.to_vec();
        assert_eq!(sps_profile_idc(&au), Some(100), "expected High profile");
        let mut dec = receive::new_decoder().unwrap();
        let frame = receive::decode_to_rgba(&mut dec, &au).unwrap().unwrap();
        assert_eq!((frame.width, frame.height), (320, 240));
    }
}
