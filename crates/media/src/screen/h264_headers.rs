#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

//! Annex-B sequence, picture, and slice headers.
//!
//! VA-API drivers that advertise packed-header support (Intel iHD among them)
//! reject a picture unless the application supplies these NALs. Standard shares
//! match the Windows encoder: profile 66, constraint flags `0xE0`, CAVLC, no
//! B-frames, and level 5.1 or 5.2. High quality matches the Windows high-quality
//! share: profile 100, constraint flags `0x00`, CABAC, 8×8 transform, no
//! B-frames, and the same level.

use super::h264_level_idc;

/// One packed header. `bit_length` counts valid bits; a slice header may end
/// mid-byte, and the unused bits in the last byte are zero.
pub(super) struct PackedNal {
    pub bytes: Vec<u8>,
    pub bit_length: u32,
    pub has_emulation_bytes: bool,
}

/// Macroblock-aligned coded size and the H.264 crop offsets that bring it
/// back to the visible picture. Crop units are two luma samples for 4:2:0.
pub(super) struct FrameGrid {
    pub coded_w: u32,
    pub coded_h: u32,
    pub crop_right: u32,
    pub crop_bottom: u32,
}

impl FrameGrid {
    pub(super) fn cropping(&self) -> bool {
        self.crop_right != 0 || self.crop_bottom != 0
    }
}

pub(super) fn frame_grid(width: u32, height: u32) -> FrameGrid {
    let coded_w = width.div_ceil(16) * 16;
    let coded_h = height.div_ceil(16) * 16;
    FrameGrid {
        coded_w,
        coded_h,
        crop_right: (coded_w - width) / 2,
        crop_bottom: (coded_h - height) / 2,
    }
}

/// Constrained Baseline SPS. Level follows the visible size. One reference frame.
pub(super) fn sequence_header(width: u32, height: u32, fps: u32) -> PackedNal {
    sequence_header_for(width, height, fps, false, 1)
}

/// SPS NAL, including the Annex-B start code.
///
/// `high` selects profile 100 with 4:2:0 High-profile fields. `max_ref_frames`
/// is both `max_num_ref_frames` and `max_dec_frame_buffering`.
pub(super) fn sequence_header_for(
    width: u32,
    height: u32,
    fps: u32,
    high: bool,
    max_ref_frames: u32,
) -> PackedNal {
    let grid = frame_grid(width, height);
    let fps = fps.max(1);
    let refs = max_ref_frames.max(1);
    let mut bits = BitBuf::default();
    if high {
        bits.u(8, 100);
        bits.u(1, 0);
        bits.u(1, 0);
        bits.u(1, 0);
        bits.u(1, 0);
        bits.u(1, 0);
        bits.u(1, 0);
        bits.u(2, 0);
    } else {
        bits.u(8, 66);
        bits.u(1, 1);
        bits.u(1, 1);
        bits.u(1, 1);
        bits.u(1, 0);
        bits.u(1, 0);
        bits.u(1, 0);
        bits.u(2, 0);
    }
    bits.u(8, h264_level_idc(width, height, fps));
    bits.ue(0);
    if high {
        bits.ue(1);
        bits.ue(0);
        bits.ue(0);
        bits.u(1, 0);
        bits.u(1, 0);
    }
    bits.ue(0);
    bits.ue(0);
    bits.ue(0);
    bits.ue(refs);
    bits.u(1, 0);
    bits.ue(grid.coded_w / 16 - 1);
    bits.ue(grid.coded_h / 16 - 1);
    bits.u(1, 1);
    bits.u(1, 1);
    bits.u(1, u32::from(grid.cropping()));
    if grid.cropping() {
        bits.ue(0);
        bits.ue(grid.crop_right);
        bits.ue(0);
        bits.ue(grid.crop_bottom);
    }
    bits.u(1, 1);
    bits.u(1, 1);
    bits.u(8, 1);
    bits.u(1, 0);
    bits.u(1, 0);
    bits.u(1, 0);
    bits.u(1, 1);
    bits.u(32, 1);
    bits.u(32, fps * 2);
    bits.u(1, 1);
    bits.u(1, 0);
    bits.u(1, 0);
    bits.u(1, 0);
    bits.u(1, 1);
    bits.u(1, 1);
    bits.ue(0);
    bits.ue(0);
    bits.ue(16);
    bits.ue(16);
    bits.ue(0);
    bits.ue(refs);
    bits.rbsp_trailing();
    nal(0x67, &bits.finish(), true)
}

/// Constrained Baseline PPS. CAVLC, one reference, deblocking, initial QP 26.
pub(super) fn picture_header() -> PackedNal {
    picture_header_for(false)
}

/// PPS NAL. High profile uses CABAC and the 8×8 transform. Initial QP is 26.
pub(super) fn picture_header_for(high: bool) -> PackedNal {
    let mut bits = BitBuf::default();
    bits.ue(0);
    bits.ue(0);
    bits.u(1, u32::from(high));
    bits.u(1, 0);
    bits.ue(0);
    bits.ue(0);
    bits.ue(0);
    bits.u(1, 0);
    bits.u(2, 0);
    bits.se(0);
    bits.se(0);
    bits.se(0);
    bits.u(1, 1);
    bits.u(1, 0);
    bits.u(1, 0);
    if high {
        bits.u(1, 1);
        bits.u(1, 0);
        bits.se(0);
    }
    bits.rbsp_trailing();
    nal(0x68, &bits.finish(), true)
}

/// Constrained Baseline slice header. One reference, CAVLC.
pub(super) fn slice_header(idr: bool, frame_num: u32, poc: u32, idr_pic_id: u32) -> PackedNal {
    slice_header_for(idr, frame_num, poc, idr_pic_id, false, 1)
}

/// Slice header only: start code, NAL byte, and `slice_header()`, without
/// slice data or RBSP trailing bits. The driver appends the coded slice.
///
/// `cabac` inserts `cabac_init_idc` for P slices. `active_refs` overrides the
/// PPS default of one reference when it is any other count. The driver inserts
/// CABAC alignment; those bits are not part of this header.
pub(super) fn slice_header_for(
    idr: bool,
    frame_num: u32,
    poc: u32,
    idr_pic_id: u32,
    cabac: bool,
    active_refs: u32,
) -> PackedNal {
    let mut bits = BitBuf::default();
    for byte in [0, 0, 0, 1] {
        bits.u(8, byte);
    }
    bits.u(8, if idr { 0x65 } else { 0x61 });
    bits.ue(0);
    bits.ue(if idr { 2 } else { 0 });
    bits.ue(0);
    bits.u(4, frame_num & 15);
    if idr {
        bits.ue(idr_pic_id);
    }
    bits.u(4, poc & 15);
    if idr {
        bits.u(1, 0);
        bits.u(1, 0);
    } else {
        let override_refs = active_refs != 1;
        bits.u(1, u32::from(override_refs));
        if override_refs {
            bits.ue(active_refs.saturating_sub(1));
        }
        bits.u(1, 0);
        bits.u(1, 0);
        if cabac {
            bits.ue(0);
        }
    }
    bits.se(0);
    bits.ue(0);
    bits.se(0);
    bits.se(0);
    PackedNal {
        bit_length: bits.len,
        bytes: bits.bytes,
        has_emulation_bytes: false,
    }
}

fn nal(header: u8, rbsp: &[u8], emulation: bool) -> PackedNal {
    let body = if emulation {
        prevent_emulation(rbsp)
    } else {
        rbsp.to_vec()
    };
    let mut bytes = Vec::with_capacity(5 + body.len());
    bytes.extend_from_slice(&[0, 0, 0, 1, header]);
    bytes.extend_from_slice(&body);
    let bit_length = (bytes.len() * 8) as u32;
    PackedNal {
        bytes,
        bit_length,
        has_emulation_bytes: emulation,
    }
}

fn prevent_emulation(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + 8);
    let mut zeros = 0u8;
    for &byte in rbsp {
        if zeros >= 2 && byte <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    out
}

#[derive(Default)]
struct BitBuf {
    bytes: Vec<u8>,
    len: u32,
}

impl BitBuf {
    fn u(&mut self, n: u32, value: u32) {
        if n == 0 {
            return;
        }
        debug_assert!(n <= 32);
        let value = if n == 32 {
            value
        } else {
            value & ((1u32 << n) - 1)
        };
        for bit in (0..n).rev() {
            let index = (self.len / 8) as usize;
            if self.bytes.len() == index {
                self.bytes.push(0);
            }
            if (value >> bit) & 1 == 1 {
                let shift = 7 - (self.len % 8);
                self.bytes[index] |= 1 << shift;
            }
            self.len += 1;
        }
    }

    fn ue(&mut self, value: u32) {
        let code = u64::from(value) + 1;
        let bits = 64 - code.leading_zeros();
        debug_assert!(bits <= 32);
        self.u(bits - 1, 0);
        self.u(bits, code as u32);
    }

    fn se(&mut self, value: i32) {
        // 0, 1, -1, 2, -2 map to exp-golomb codes 0, 1, 2, 3, 4.
        let code = if value > 0 {
            (u32::try_from(value).unwrap_or(u32::MAX) << 1).saturating_sub(1)
        } else {
            value.unsigned_abs() << 1
        };
        self.ue(code);
    }

    fn rbsp_trailing(&mut self) {
        self.u(1, 1);
        let remainder = self.len % 8;
        if remainder != 0 {
            self.u(8 - remainder, 0);
        }
    }

    fn finish(self) -> Vec<u8> {
        debug_assert!(self.len.is_multiple_of(8));
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_crops_only_the_macroblock_padding() {
        let exact = frame_grid(16, 16);
        assert_eq!((exact.coded_w, exact.coded_h), (16, 16));
        assert!(!exact.cropping());
        let padded = frame_grid(800, 600);
        assert_eq!((padded.coded_w, padded.coded_h), (800, 608));
        assert_eq!((padded.crop_right, padded.crop_bottom), (0, 4));
    }

    #[test]
    fn sequence_and_picture_headers_match_constrained_baseline() {
        let sps_16 = sequence_header(16, 16, 30);
        assert_eq!(
            sps_16.bytes,
            [
                0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0xe0, 0x33, 0xf4, 0xf6, 0x02, 0x20, 0x00, 0x00,
                0x03, 0x00, 0x20, 0x00, 0x00, 0x07, 0x91, 0xe1, 0x10, 0x8d, 0x40,
            ]
        );
        assert!(sps_16.has_emulation_bytes);
        assert_eq!(sps_16.bit_length, sps_16.bytes.len() as u32 * 8);

        let sps_crop = sequence_header(800, 600, 60);
        assert_eq!(
            sps_crop.bytes,
            [
                0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0xe0, 0x33, 0xf4, 0x06, 0x40, 0x9b, 0xf2, 0xe0,
                0x22, 0x00, 0x00, 0x03, 0x00, 0x02, 0x00, 0x00, 0x03, 0x00, 0xf1, 0x1e, 0x11, 0x08,
                0xd4,
            ]
        );

        let sps_4k = sequence_header(3840, 2160, 60);
        assert_eq!(sps_4k.bytes[5..8], [0x42, 0xe0, 0x34]);
        assert_eq!(
            sps_4k.bytes,
            [
                0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0xe0, 0x34, 0xf4, 0x01, 0xe0, 0x02, 0x1f, 0x60,
                0x22, 0x00, 0x00, 0x03, 0x00, 0x02, 0x00, 0x00, 0x03, 0x00, 0xf1, 0x1e, 0x11, 0x08,
                0xd4,
            ]
        );

        let pps = picture_header();
        assert_eq!(pps.bytes, [0x00, 0x00, 0x00, 0x01, 0x68, 0xce, 0x3c, 0x80]);
    }

    #[test]
    fn slice_headers_stop_on_the_last_header_bit() {
        let idr = slice_header(true, 0, 0, 0);
        assert_eq!(idr.bytes, [0x00, 0x00, 0x00, 0x01, 0x65, 0xb8, 0x40, 0xf0]);
        assert_eq!(idr.bit_length, 60);
        assert!(!idr.has_emulation_bytes);

        let p = slice_header(false, 1, 2, 0);
        assert_eq!(p.bytes, [0x00, 0x00, 0x00, 0x01, 0x61, 0xe2, 0x43, 0xc0]);
        assert_eq!(p.bit_length, 58);
    }

    #[test]
    fn high_profile_headers_match_the_windows_operating_point() {
        let desktop = sequence_header_for(16, 16, 30, true, 2);
        assert_eq!(desktop.bytes[4] & 0x1f, 7);
        assert_eq!(desktop.bytes[5..8], [0x64, 0x00, 0x33]);
        let parsed = parse_sps(&desktop.bytes);
        assert_eq!(parsed.profile_idc, 100);
        assert_eq!(parsed.constraint, 0);
        assert_eq!(parsed.level_idc, 51);
        assert_eq!(parsed.chroma_format_idc, Some(1));
        assert_eq!(parsed.max_num_ref_frames, 2);
        assert_eq!(parsed.max_dec_frame_buffering, 2);
        assert!(parsed.frame_mbs_only);
        assert!(parsed.direct_8x8);

        let game = sequence_header_for(2560, 1440, 120, true, 1);
        assert_eq!(game.bytes[5..8], [0x64, 0x00, 0x34]);
        assert_eq!(parse_sps(&game.bytes).max_num_ref_frames, 1);
        assert_eq!(parse_sps(&game.bytes).level_idc, 52);

        let uhd = sequence_header_for(3840, 2160, 60, true, 2);
        assert_eq!(uhd.bytes[5..8], [0x64, 0x00, 0x34]);
        let cropped = parse_sps(&sequence_header_for(800, 600, 60, true, 2).bytes);
        assert!(cropped.cropping);
        assert_eq!(cropped.crop_bottom, 4);

        let pps = parse_pps(&picture_header_for(true).bytes);
        assert!(pps.cabac);
        assert!(pps.transform_8x8);
        assert!(pps.deblocking);
        assert!(!pps.redundant);
        let baseline = parse_pps(&picture_header().bytes);
        assert!(!baseline.cabac);
        assert!(!baseline.transform_8x8);

        let idr = slice_header_for(true, 0, 0, 0, true, 1);
        assert_eq!(idr.bytes, slice_header(true, 0, 0, 0).bytes);
        assert_eq!(idr.bit_length, 60);

        let one_ref = slice_header_for(false, 1, 2, 0, true, 1);
        assert!(one_ref.bit_length > slice_header(false, 1, 2, 0).bit_length);
        assert_eq!(
            parse_slice(&one_ref.bytes, one_ref.bit_length, true).cabac_init_idc,
            Some(0)
        );
        assert_eq!(
            parse_slice(&one_ref.bytes, one_ref.bit_length, true).active_refs,
            1
        );

        let two_ref = slice_header_for(false, 1, 2, 0, true, 2);
        assert_eq!(
            parse_slice(&two_ref.bytes, two_ref.bit_length, true).active_refs,
            2
        );
        assert_eq!(
            parse_slice(&slice_header(false, 1, 2, 0).bytes, 58, false).active_refs,
            1
        );
    }

    struct Sps {
        profile_idc: u32,
        constraint: u32,
        level_idc: u32,
        chroma_format_idc: Option<u32>,
        max_num_ref_frames: u32,
        frame_mbs_only: bool,
        direct_8x8: bool,
        cropping: bool,
        crop_bottom: u32,
        max_dec_frame_buffering: u32,
    }

    struct Pps {
        cabac: bool,
        deblocking: bool,
        redundant: bool,
        transform_8x8: bool,
    }

    struct Slice {
        active_refs: u32,
        cabac_init_idc: Option<u32>,
    }

    struct Reader<'a> {
        bytes: &'a [u8],
        bit: usize,
    }

    impl<'a> Reader<'a> {
        fn rbsp(nal: &'a [u8]) -> Vec<u8> {
            let start = if nal.starts_with(&[0, 0, 0, 1]) { 5 } else { 4 };
            let mut out = Vec::new();
            let mut zeros = 0u8;
            for &byte in &nal[start..] {
                if zeros >= 2 && byte == 3 {
                    zeros = 0;
                    continue;
                }
                out.push(byte);
                zeros = if byte == 0 { zeros + 1 } else { 0 };
            }
            out
        }

        fn new(bytes: &'a [u8]) -> Self {
            Self { bytes, bit: 0 }
        }

        fn u(&mut self, n: usize) -> u32 {
            let mut value = 0u32;
            for _ in 0..n {
                let index = self.bit / 8;
                let shift = 7 - (self.bit % 8);
                let bit = u32::from((self.bytes[index] >> shift) & 1);
                value = (value << 1) | bit;
                self.bit += 1;
            }
            value
        }

        fn ue(&mut self) -> u32 {
            let mut zeros = 0u32;
            while self.u(1) == 0 {
                zeros += 1;
            }
            if zeros == 0 {
                0
            } else {
                ((1u32 << zeros) | self.u(zeros as usize)) - 1
            }
        }

        fn se(&mut self) -> i32 {
            let code = self.ue();
            if code & 1 == 0 {
                -((code / 2) as i32)
            } else {
                ((code / 2) + 1) as i32
            }
        }
    }

    fn parse_sps(nal: &[u8]) -> Sps {
        let rbsp = Reader::rbsp(nal);
        let mut r = Reader::new(&rbsp);
        let profile_idc = r.u(8);
        let constraint = r.u(8);
        let level_idc = r.u(8);
        let _sps_id = r.ue();
        let chroma_format_idc = if profile_idc == 100 {
            let chroma = r.ue();
            let _luma = r.ue();
            let _chroma_depth = r.ue();
            let _bypass = r.u(1);
            let _scaling = r.u(1);
            Some(chroma)
        } else {
            None
        };
        let _log2_frame = r.ue();
        let poc_type = r.ue();
        if poc_type == 0 {
            let _log2_poc = r.ue();
        }
        let max_num_ref_frames = r.ue();
        let _gaps = r.u(1);
        let _width = r.ue();
        let _height = r.ue();
        let frame_mbs_only = r.u(1) == 1;
        let direct_8x8 = r.u(1) == 1;
        let cropping = r.u(1) == 1;
        let mut crop_bottom = 0;
        if cropping {
            let _left = r.ue();
            let _right = r.ue();
            let _top = r.ue();
            crop_bottom = r.ue();
        }
        let vui = r.u(1) == 1;
        assert!(vui);
        let aspect = r.u(1) == 1;
        if aspect {
            let _idc = r.u(8);
        }
        let _overscan = r.u(1);
        let _video_signal = r.u(1);
        let _chroma_loc = r.u(1);
        let timing = r.u(1) == 1;
        assert!(timing);
        let _tick = r.u(32);
        let _scale = r.u(32);
        let _fixed = r.u(1);
        let _nal_hrd = r.u(1);
        let _vcl_hrd = r.u(1);
        let _pic_struct = r.u(1);
        let restriction = r.u(1) == 1;
        assert!(restriction);
        let _motion = r.u(1);
        let _max_bytes = r.ue();
        let _max_bits = r.ue();
        let _mv_x = r.ue();
        let _mv_y = r.ue();
        let _reorder = r.ue();
        let max_dec_frame_buffering = r.ue();
        Sps {
            profile_idc,
            constraint,
            level_idc,
            chroma_format_idc,
            max_num_ref_frames,
            frame_mbs_only,
            direct_8x8,
            cropping,
            crop_bottom,
            max_dec_frame_buffering,
        }
    }

    fn parse_pps(nal: &[u8]) -> Pps {
        let rbsp = Reader::rbsp(nal);
        let mut r = Reader::new(&rbsp);
        let _pps = r.ue();
        let _sps = r.ue();
        let cabac = r.u(1) == 1;
        let _bottom = r.u(1);
        let _groups = r.ue();
        let _l0 = r.ue();
        let _l1 = r.ue();
        let _weighted = r.u(1);
        let _bipred = r.u(2);
        let _qp = r.se();
        let _qs = r.se();
        let _chroma = r.se();
        let deblocking = r.u(1) == 1;
        let _intra = r.u(1);
        let redundant = r.u(1) == 1;
        let transform_8x8 = cabac && r.u(1) == 1;
        Pps {
            cabac,
            deblocking,
            redundant,
            transform_8x8,
        }
    }

    fn parse_slice(nal: &[u8], bit_length: u32, cabac: bool) -> Slice {
        let mut r = Reader::new(nal);
        let _start = r.u(32);
        let nal_header = r.u(8);
        let idr = nal_header & 0x1f == 5;
        let _first = r.ue();
        let _slice_type = r.ue();
        let _pps = r.ue();
        let _frame_num = r.u(4);
        if idr {
            let _idr_id = r.ue();
        }
        let _poc = r.u(4);
        let active_refs = if idr {
            let _no_output = r.u(1);
            let _long_term = r.u(1);
            0
        } else {
            let override_refs = r.u(1) == 1;
            let refs = if override_refs { r.ue() + 1 } else { 1 };
            let _mod = r.u(1);
            let _marking = r.u(1);
            refs
        };
        let cabac_init_idc = if cabac && !idr { Some(r.ue()) } else { None };
        let _qp = r.se();
        let _deblock = r.ue();
        let _alpha = r.se();
        let _beta = r.se();
        assert_eq!(r.bit as u32, bit_length);
        Slice {
            active_refs,
            cabac_init_idc,
        }
    }
}
