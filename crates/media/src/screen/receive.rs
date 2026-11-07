//! Receiving side of screen share: RTP to H.264 access units to RGBA frames.
//!
//! An RTP reader task depacketizes the remote H.264 track into access units (one per
//! marker bit), drops pictures after loss until the next IDR, and asks the publisher for
//! a keyframe with PLI (at most every 500 ms). Access units wait in a playout buffer of
//! up to one second. The decoder thread shows them on the RTP clock, so a burst of
//! packets becomes a steady picture instead of a stutter. A viewer that falls further
//! than that second behind skips pictures (still decoding them, so the stream stays
//! valid) until it is inside the window again.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use openh264::OpenH264API;
use openh264::decoder::{Decoder, DecoderConfig};
use openh264::formats::YUVSource;
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtc::rtp::codec::h264::H264Packet;
use rtc::rtp::packetizer::Depacketizer;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};

use super::playout::{AuBuffer, MAX_PLAYOUT_DELAY, PushError, SharedClock, Step, TimedAu, Wait};
use crate::{MediaError, RgbaFrame, VideoFrameQueue};

/// Minimum interval between PLIs.
const PLI_INTERVAL: Duration = Duration::from_millis(500);
/// Upper bound for one access unit; larger ones are dropped.
const MAX_AU_BYTES: usize = 8 * 1024 * 1024;

/// Whether an Annex-B buffer contains an IDR slice (NAL type 5).
pub fn contains_idr(au: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            if au[i + 3] & 0x1F == 5 {
                return true;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

/// Creates an OpenH264 decoder.
pub fn new_decoder() -> Result<Decoder, MediaError> {
    Decoder::with_api_config(OpenH264API::from_source(), DecoderConfig::new())
        .map_err(|e| MediaError::Codec(format!("openh264 decoder: {e}")))
}

/// Decodes one access unit; returns `Ok(None)` when the decoder needs more data.
pub fn decode_to_rgba(decoder: &mut Decoder, au: &[u8]) -> Result<Option<RgbaFrame>, MediaError> {
    let Some(yuv) = decoder
        .decode(au)
        .map_err(|e| MediaError::Codec(format!("h264 decode: {e}")))?
    else {
        return Ok(None);
    };
    let (w, h) = yuv.dimensions();
    let mut pixels = vec![0u8; w * h * 4];
    yuv.write_rgba8(&mut pixels);
    Ok(Some(RgbaFrame {
        width: w as u32,
        height: h as u32,
        pixels: Arc::from(pixels),
    }))
}

fn decode_discard(decoder: &mut Decoder, au: &[u8]) -> Result<(), MediaError> {
    decoder
        .decode(au)
        .map(|_| ())
        .map_err(|e| MediaError::Codec(format!("h264 decode: {e}")))
}

/// Reads `track` until it ends, decoding into `queue`. Returns when the track ends.
pub async fn run_receiver(track: Arc<dyn TrackRemote>, queue: VideoFrameQueue) {
    queue.clear();
    let buffer = AuBuffer::new();
    let clock = SharedClock::new();
    let want_keyframe = Arc::new(AtomicBool::new(true));
    let decoder_buffer = Arc::clone(&buffer);
    let decoder_clock = clock.clone();
    let decoder_flag = Arc::clone(&want_keyframe);
    let decoder = std::thread::Builder::new()
        .name("cc-screen-decoder".into())
        .spawn(move || decode_loop(decoder_buffer, decoder_clock, queue, decoder_flag));
    if let Err(e) = decoder {
        tracing::warn!("spawn decoder thread: {e}");
        return;
    }
    read_loop(track, &buffer, &clock, &want_keyframe).await;
    buffer.close();
}

async fn read_loop(
    track: Arc<dyn TrackRemote>,
    buffer: &AuBuffer,
    clock: &SharedClock,
    want_keyframe: &AtomicBool,
) {
    let mut depacketizer = H264Packet::default();
    let mut au: Vec<u8> = Vec::with_capacity(256 * 1024);
    let mut au_ts: Option<u32> = None;
    let mut last_seq: Option<u16> = None;
    let mut last_pli: Option<Instant> = None;
    let mut waiting_for_idr = true;
    let mut corrupt = false;

    while let Some(event) = track.poll().await {
        let pkt = match event {
            TrackRemoteEvent::OnRtpPacket(pkt) => pkt,
            TrackRemoteEvent::OnEnded => break,
            _ => continue,
        };
        let seq = pkt.header.sequence_number;
        if let Some(prev) = last_seq
            && seq != prev.wrapping_add(1)
        {
            // Loss: the current picture is unusable and references will be broken.
            corrupt = true;
            waiting_for_idr = true;
        }
        last_seq = Some(seq);
        if au_ts.is_none() {
            au_ts = Some(pkt.header.timestamp);
        }

        match depacketizer.depacketize(&pkt.payload) {
            Ok(bytes) => {
                if au.len() + bytes.len() <= MAX_AU_BYTES {
                    au.extend_from_slice(&bytes);
                } else {
                    corrupt = true;
                }
            }
            Err(_) => corrupt = true,
        }

        if pkt.header.marker {
            let timestamp = au_ts.take().unwrap_or(pkt.header.timestamp);
            let complete = !corrupt && !au.is_empty();
            let keyframe = complete && contains_idr(&au);
            if complete && waiting_for_idr && keyframe {
                waiting_for_idr = false;
            }
            if complete && !waiting_for_idr {
                let arrival = Instant::now();
                clock.observe(timestamp, arrival);
                let timed = TimedAu {
                    data: std::mem::replace(&mut au, Vec::with_capacity(64 * 1024)),
                    rtp_ts: timestamp,
                    keyframe,
                };
                match buffer.push(timed) {
                    Ok(()) => {}
                    Err(PushError::Overflow) => {
                        tracing::debug!("screen playout buffer overflowed; waiting for a keyframe");
                        waiting_for_idr = true;
                    }
                    Err(PushError::Closed) => return,
                }
            }
            au.clear();
            au_ts = None;
            corrupt = false;
        }

        if (waiting_for_idr || want_keyframe.load(Ordering::Relaxed))
            && last_pli.is_none_or(|t| t.elapsed() >= PLI_INTERVAL)
        {
            last_pli = Some(Instant::now());
            want_keyframe.store(false, Ordering::Relaxed);
            let pli = PictureLossIndication {
                sender_ssrc: 0,
                media_ssrc: pkt.header.ssrc,
            };
            let _ = track.write_rtcp(vec![Box::new(pli)]).await;
        }
    }
}

fn decode_loop(
    buffer: Arc<AuBuffer>,
    clock: SharedClock,
    queue: VideoFrameQueue,
    want_keyframe: Arc<AtomicBool>,
) {
    let mut decoder = match new_decoder() {
        Ok(decoder) => decoder,
        Err(e) => {
            tracing::warn!("{e}");
            return;
        }
    };
    let mut generation = 0u64;
    let mut need_idr = false;
    let mut logged_start = false;
    let mut empty_since: Option<Instant> = None;
    loop {
        if buffer.take_overflow() {
            need_idr = true;
            want_keyframe.store(true, Ordering::Relaxed);
            clock.with_mut(super::playout::PlayoutClock::underrun);
            decoder = match new_decoder() {
                Ok(decoder) => decoder,
                Err(e) => {
                    tracing::warn!("{e}");
                    return;
                }
            };
        }

        let timeout = clock.with_mut(|clock| clock.frame_duration());
        let peeked = match buffer.wait_generation(generation, timeout) {
            Wait::Closed => return,
            Wait::Overflow => continue,
            Wait::Timeout { queued: None } => {
                let (started, cushion) =
                    clock.with_mut(|clock| (clock.is_started(), clock.delay()));
                if started {
                    let since = empty_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= cushion {
                        empty_since = None;
                        clock.with_mut(super::playout::PlayoutClock::underrun);
                        tracing::debug!("screen playout underrun; buffering again");
                    }
                }
                continue;
            }
            Wait::Timeout { queued: Some(peek) } | Wait::Ready(peek) => {
                empty_since = None;
                peek
            }
        };
        generation = peeked.generation;

        if need_idr && !peeked.keyframe {
            let _ = buffer.pop();
            continue;
        }
        if need_idr {
            need_idr = false;
            clock.with_mut(super::playout::PlayoutClock::reset);
            decoder = match new_decoder() {
                Ok(decoder) => decoder,
                Err(e) => {
                    tracing::warn!("{e}");
                    return;
                }
            };
        }

        let decision =
            clock.with_mut(|clock| clock.step(peeked.oldest, peeked.newest, Instant::now()));
        match decision {
            Step::Buffer => continue,
            Step::DecodeOnly => {
                let skipped = clock.with_mut(|clock| clock.skipped());
                if skipped == 1 || skipped.is_multiple_of(60) {
                    tracing::debug!(skipped, "screen playout skipped a frame to stay within 1s");
                }
                if let Some(au) = buffer.pop()
                    && decode_discard(&mut decoder, &au.data).is_err()
                {
                    need_idr = true;
                    want_keyframe.store(true, Ordering::Relaxed);
                }
            }
            Step::Present { at } => {
                if !logged_start {
                    logged_start = true;
                    let delay = clock.with_mut(|clock| clock.delay());
                    tracing::debug!(delay_ms = delay.as_millis(), "screen playout started");
                }
                let Some(au) = buffer.pop() else {
                    continue;
                };
                if !present_frame(
                    &buffer,
                    &clock,
                    &queue,
                    &mut decoder,
                    &want_keyframe,
                    au,
                    at,
                ) {
                    if buffer.is_closed() {
                        return;
                    }
                    need_idr = true;
                }
            }
        }
    }
}

/// Decodes `au` and hands it to the UI at `at`.
/// Returns false when the picture could not be decoded or the receiver stopped.
fn present_frame(
    buffer: &AuBuffer,
    clock: &SharedClock,
    queue: &VideoFrameQueue,
    decoder: &mut Decoder,
    want_keyframe: &AtomicBool,
    au: TimedAu,
    at: Instant,
) -> bool {
    let frame_len = clock.with_mut(|clock| clock.frame_duration());
    let lead = decode_lead(frame_len);
    let decode_at = at.checked_sub(lead).unwrap_or(at);
    if !buffer.sleep_until(decode_at) {
        return false;
    }
    let frame = match decode_to_rgba(decoder, &au.data) {
        Ok(Some(frame)) => frame,
        Ok(None) => return true,
        Err(e) => {
            tracing::debug!("{e}");
            want_keyframe.store(true, Ordering::Relaxed);
            return false;
        }
    };
    if !buffer.sleep_until(at) {
        return false;
    }
    match enqueue_frame(queue, frame, buffer, at) {
        Enqueue::Queued(shown) => {
            // A frame that is only a little late stays on the RTP grid. Stamping
            // the actual time every frame would add that lateness again and
            // the delay would grow until we started skipping.
            let late = shown.saturating_duration_since(at);
            let stamp = if late > frame_len / 2 { shown } else { at };
            clock.with_mut(|clock| clock.did_show(stamp));
            true
        }
        Enqueue::Skipped => {
            // The UI is still busy and we are already a second behind. Keep the
            // picture it has; the next step skips until the buffer fits again.
            clock.with_mut(|clock| clock.did_show(Instant::now()));
            true
        }
        Enqueue::Closed => false,
    }
}

fn decode_lead(frame: Duration) -> Duration {
    (frame * 2).clamp(Duration::from_millis(8), Duration::from_millis(50))
}

enum Enqueue {
    Queued(Instant),
    Skipped,
    Closed,
}

fn enqueue_frame(
    queue: &VideoFrameQueue,
    frame: RgbaFrame,
    buffer: &AuBuffer,
    planned: Instant,
) -> Enqueue {
    let mut frame = Some(frame);
    let deadline = planned + MAX_PLAYOUT_DELAY;
    loop {
        if buffer.is_closed() {
            return Enqueue::Closed;
        }
        let Some(pending) = frame.take() else {
            return Enqueue::Skipped;
        };
        match queue.try_push(pending) {
            Ok(()) => return Enqueue::Queued(Instant::now()),
            Err(returned) => {
                frame = Some(returned);
                if buffer.span() > MAX_PLAYOUT_DELAY || Instant::now() >= deadline {
                    return Enqueue::Skipped;
                }
                let pause = Instant::now() + Duration::from_millis(4);
                if !buffer.sleep_until(pause) {
                    return Enqueue::Closed;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idr_detection() {
        assert!(contains_idr(&[0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x65, 9]));
        assert!(!contains_idr(&[0, 0, 0, 1, 0x41, 1, 2, 3]));
        assert!(!contains_idr(&[]));
    }

    #[test]
    fn decode_lead_stays_inside_one_frame_budget() {
        assert_eq!(
            decode_lead(Duration::from_millis(16)),
            Duration::from_millis(32)
        );
        assert_eq!(
            decode_lead(Duration::from_millis(8)),
            Duration::from_millis(16)
        );
        assert_eq!(
            decode_lead(Duration::from_millis(100)),
            Duration::from_millis(50)
        );
    }
}
