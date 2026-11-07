//! Layout of the libva 2.22 encode structs this encoder writes.
//!
//! The fields are plain integers, so the same `#[repr(C)]` layout is what
//! libva uses on 64-bit Linux. Pointers appear only in [`VaCodedBufferSegment`],
//! which is 48 bytes on both LP64 and Windows x64.

#![allow(dead_code)]

pub(super) const PROFILE_BASELINE: i32 = 5;
pub(super) const PROFILE_MAIN: i32 = 6;
pub(super) const PROFILE_HIGH: i32 = 7;
pub(super) const PROFILE_CONSTRAINED_BASELINE: i32 = 13;
pub(super) const ENTRY_ENC_SLICE: i32 = 6;
pub(super) const ENTRY_ENC_SLICE_LP: i32 = 8;
pub(super) const ATTRIB_RT_FORMAT: i32 = 0;
pub(super) const ATTRIB_RATE_CONTROL: i32 = 5;
pub(super) const ATTRIB_PACKED_HEADERS: i32 = 10;
pub(super) const ATTRIB_QUALITY_RANGE: i32 = 21;
pub(super) const RT_YUV420: u32 = 1;
pub(super) const RC_CBR: u32 = 2;
pub(super) const RC_VBR: u32 = 4;
pub(super) const PACKED_SEQUENCE: u32 = 1;
pub(super) const PACKED_PICTURE: u32 = 2;
pub(super) const PACKED_SLICE: u32 = 4;
pub(super) const ATTRIB_NOT_SUPPORTED: u32 = 0x8000_0000;
pub(super) const INVALID_ID: u32 = 0xffff_ffff;
pub(super) const PROGRESSIVE: i32 = 1;
pub(super) const FOURCC_NV12: u32 = 0x3231_564E;
pub(super) const PICTURE_INVALID: u32 = 1;
pub(super) const PICTURE_SHORT_TERM: u32 = 8;
pub(super) const BUF_CODED: i32 = 21;
pub(super) const BUF_SEQUENCE: i32 = 22;
pub(super) const BUF_PICTURE: i32 = 23;
pub(super) const BUF_SLICE: i32 = 24;
pub(super) const BUF_PACKED_HEADER: i32 = 25;
pub(super) const BUF_PACKED_DATA: i32 = 26;
pub(super) const BUF_MISC: i32 = 27;
pub(super) const MISC_FRAME_RATE: i32 = 0;
pub(super) const MISC_RATE_CONTROL: i32 = 1;
pub(super) const MISC_HRD: i32 = 5;
pub(super) const MISC_QUALITY_LEVEL: i32 = 6;
pub(super) const PACKED_TYPE_SEQUENCE: u32 = 1;
pub(super) const PACKED_TYPE_PICTURE: u32 = 2;
pub(super) const PACKED_TYPE_SLICE: u32 = 4;
pub(super) const STATUS_FRAME_SIZE_OVERFLOW: u32 = 0x1000;
pub(super) const STATUS_BAD_BITSTREAM: u32 = 0x8000;

/// `chroma_format_idc = 1`, `frame_mbs_only_flag = 1`, `direct_8x8_inference_flag = 1`.
pub(super) fn sequence_fields() -> u32 {
    1 | (1 << 2) | (1 << 5)
}

/// Aspect ratio, timing, bitstream restriction, motion-vector length 16, fixed frame rate.
pub(super) fn vui_fields() -> u32 {
    1 | (1 << 1) | (1 << 2) | (16 << 3) | (16 << 8) | (1 << 13) | (1 << 15)
}

/// Reference picture, deblocking present, and the IDR bit when this picture is an IDR.
pub(super) fn picture_fields(idr: bool) -> u32 {
    u32::from(idr) | (1 << 1) | (1 << 10)
}

/// High profile: one reference, CABAC, 8×8 transform, deblocking, and the IDR bit.
///
/// `reference_pic_flag` is two bits wide in libva, so the reference value starts at bit 1.
/// CABAC is bit 3, the 8×8 transform is bit 8, and deblocking control is bit 9.
pub(super) fn picture_fields_high(idr: bool) -> u32 {
    u32::from(idr) | (1 << 1) | (1 << 3) | (1 << 8) | (1 << 9)
}

/// `disable_frame_skip`, plus a rate-control reset on the first picture.
pub(super) fn rate_control_flags(reset: bool) -> u32 {
    u32::from(reset) | (1 << 1)
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct VaConfigAttrib {
    pub type_: i32,
    pub value: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct VaPictureH264 {
    pub picture_id: u32,
    pub frame_idx: u32,
    pub flags: u32,
    pub top_field_order_cnt: i32,
    pub bottom_field_order_cnt: i32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(super) struct VaEncSequenceParameterBufferH264 {
    pub seq_parameter_set_id: u8,
    pub level_idc: u8,
    pub intra_period: u32,
    pub intra_idr_period: u32,
    pub ip_period: u32,
    pub bits_per_second: u32,
    pub max_num_ref_frames: u32,
    pub picture_width_in_mbs: u16,
    pub picture_height_in_mbs: u16,
    pub seq_fields: u32,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub num_ref_frames_in_pic_order_cnt_cycle: u8,
    pub offset_for_non_ref_pic: i32,
    pub offset_for_top_to_bottom_field: i32,
    pub offset_for_ref_frame: [i32; 256],
    pub frame_cropping_flag: u8,
    pub frame_crop_left_offset: u32,
    pub frame_crop_right_offset: u32,
    pub frame_crop_top_offset: u32,
    pub frame_crop_bottom_offset: u32,
    pub vui_parameters_present_flag: u8,
    pub vui_fields: u32,
    pub aspect_ratio_idc: u8,
    pub sar_width: u32,
    pub sar_height: u32,
    pub num_units_in_tick: u32,
    pub time_scale: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct VaEncPictureParameterBufferH264 {
    pub curr_pic: VaPictureH264,
    pub reference_frames: [VaPictureH264; 16],
    pub coded_buf: u32,
    pub pic_parameter_set_id: u8,
    pub seq_parameter_set_id: u8,
    pub last_picture: u8,
    pub frame_num: u16,
    pub pic_init_qp: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub chroma_qp_index_offset: i8,
    pub second_chroma_qp_index_offset: i8,
    pub pic_fields: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct VaEncSliceParameterBufferH264 {
    pub macroblock_address: u32,
    pub num_macroblocks: u32,
    pub macroblock_info: u32,
    pub slice_type: u8,
    pub pic_parameter_set_id: u8,
    pub idr_pic_id: u16,
    pub pic_order_cnt_lsb: u16,
    pub delta_pic_order_cnt_bottom: i32,
    pub delta_pic_order_cnt: [i32; 2],
    pub direct_spatial_mv_pred_flag: u8,
    pub num_ref_idx_active_override_flag: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub ref_pic_list0: [VaPictureH264; 32],
    pub ref_pic_list1: [VaPictureH264; 32],
    pub luma_log2_weight_denom: u8,
    pub chroma_log2_weight_denom: u8,
    pub luma_weight_l0_flag: u8,
    pub luma_weight_l0: [i16; 32],
    pub luma_offset_l0: [i16; 32],
    pub chroma_weight_l0_flag: u8,
    pub chroma_weight_l0: [[i16; 2]; 32],
    pub chroma_offset_l0: [[i16; 2]; 32],
    pub luma_weight_l1_flag: u8,
    pub luma_weight_l1: [i16; 32],
    pub luma_offset_l1: [i16; 32],
    pub chroma_weight_l1_flag: u8,
    pub chroma_weight_l1: [[i16; 2]; 32],
    pub chroma_offset_l1: [[i16; 2]; 32],
    pub cabac_init_idc: u8,
    pub slice_qp_delta: i8,
    pub disable_deblocking_filter_idc: u8,
    pub slice_alpha_c0_offset_div2: i8,
    pub slice_beta_offset_div2: i8,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct VaEncPackedHeaderParameterBuffer {
    pub type_: u32,
    pub bit_length: u32,
    pub has_emulation_bytes: u8,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct VaEncMiscParameterRateControl {
    pub bits_per_second: u32,
    pub target_percentage: u32,
    pub window_size: u32,
    pub initial_qp: u32,
    pub min_qp: u32,
    pub basic_unit_size: u32,
    pub rc_flags: u32,
    pub icq_quality_factor: u32,
    pub max_qp: u32,
    pub quality_factor: u32,
    pub target_frame_size: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct VaEncMiscParameterFrameRate {
    pub framerate: u32,
    pub framerate_flags: u32,
    pub va_reserved: [u32; 4],
}

/// HRD / VBV sizes in bits. `va_reserved` matches libva's `VA_PADDING_LOW`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct VaEncMiscParameterHrd {
    pub initial_buffer_fullness: u32,
    pub buffer_size: u32,
    pub va_reserved: [u32; 4],
}

/// Driver quality preset. 1 is the highest quality; 0 means the driver default.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct VaEncMiscParameterQualityLevel {
    pub quality_level: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct VaImageFormat {
    pub fourcc: u32,
    pub byte_order: u32,
    pub bits_per_pixel: u32,
    pub depth: u32,
    pub red_mask: u32,
    pub green_mask: u32,
    pub blue_mask: u32,
    pub alpha_mask: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct VaImage {
    pub image_id: u32,
    pub format: VaImageFormat,
    pub buf: u32,
    pub width: u16,
    pub height: u16,
    pub data_size: u32,
    pub num_planes: u32,
    pub pitches: [u32; 3],
    pub offsets: [u32; 3],
    pub num_palette_entries: i32,
    pub entry_bytes: i32,
    pub component_order: [i8; 4],
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(super) struct VaCodedBufferSegment {
    pub size: u32,
    pub bit_offset: u32,
    pub status: u32,
    pub reserved: u32,
    pub buf: *mut std::ffi::c_void,
    pub next: *mut std::ffi::c_void,
    pub va_reserved: [u32; 4],
}

#[cfg(test)]
mod tests {
    use std::mem::{offset_of, size_of};

    use super::*;

    #[test]
    fn h264_encode_structs_match_libva_2_22() {
        assert_eq!(size_of::<VaConfigAttrib>(), 8);
        assert_eq!(size_of::<VaPictureH264>(), 36);
        assert_eq!(size_of::<VaEncPackedHeaderParameterBuffer>(), 28);
        assert_eq!(
            offset_of!(VaEncPackedHeaderParameterBuffer, va_reserved),
            12
        );
        assert_eq!(size_of::<VaEncMiscParameterRateControl>(), 60);
        assert_eq!(size_of::<VaEncMiscParameterFrameRate>(), 24);
        assert_eq!(size_of::<VaEncMiscParameterHrd>(), 24);
        assert_eq!(size_of::<VaEncMiscParameterQualityLevel>(), 20);
        assert_eq!(size_of::<VaImageFormat>(), 48);
        assert_eq!(size_of::<VaImage>(), 120);

        assert_eq!(size_of::<VaEncSequenceParameterBufferH264>(), 1132);
        assert_eq!(
            offset_of!(VaEncSequenceParameterBufferH264, intra_period),
            4
        );
        assert_eq!(
            offset_of!(VaEncSequenceParameterBufferH264, picture_width_in_mbs),
            24
        );
        assert_eq!(offset_of!(VaEncSequenceParameterBufferH264, seq_fields), 28);
        assert_eq!(
            offset_of!(VaEncSequenceParameterBufferH264, offset_for_ref_frame),
            44
        );
        assert_eq!(
            offset_of!(VaEncSequenceParameterBufferH264, frame_cropping_flag),
            1068
        );
        assert_eq!(
            offset_of!(VaEncSequenceParameterBufferH264, va_reserved),
            1116
        );

        assert_eq!(size_of::<VaEncPictureParameterBufferH264>(), 648);
        assert_eq!(offset_of!(VaEncPictureParameterBufferH264, frame_num), 620);
        assert_eq!(offset_of!(VaEncPictureParameterBufferH264, pic_fields), 628);

        assert_eq!(size_of::<VaEncSliceParameterBufferH264>(), 3140);
        assert_eq!(offset_of!(VaEncSliceParameterBufferH264, slice_type), 12);
        assert_eq!(offset_of!(VaEncSliceParameterBufferH264, ref_pic_list0), 36);
        assert_eq!(
            offset_of!(VaEncSliceParameterBufferH264, ref_pic_list1),
            1188
        );
        assert_eq!(
            offset_of!(VaEncSliceParameterBufferH264, luma_log2_weight_denom),
            2340
        );
        assert_eq!(
            offset_of!(VaEncSliceParameterBufferH264, luma_weight_l0),
            2344
        );
        assert_eq!(offset_of!(VaEncSliceParameterBufferH264, va_reserved), 3124);
    }

    #[test]
    fn coded_segment_matches_64_bit_libva() {
        assert_eq!(size_of::<usize>(), 8);
        assert_eq!(size_of::<VaCodedBufferSegment>(), 48);
        assert_eq!(offset_of!(VaCodedBufferSegment, buf), 16);
        assert_eq!(offset_of!(VaCodedBufferSegment, next), 24);
    }

    #[test]
    fn field_bits_match_the_constrained_baseline_headers() {
        assert_eq!(sequence_fields(), 37);
        assert_eq!(vui_fields(), 45_191);
        assert_eq!(picture_fields(true), 1027);
        assert_eq!(picture_fields(false), 1026);
        assert_eq!(picture_fields_high(true), 779);
        assert_eq!(picture_fields_high(false), 778);
        assert_eq!(rate_control_flags(false), 2);
        assert_eq!(rate_control_flags(true), 3);
    }
}
