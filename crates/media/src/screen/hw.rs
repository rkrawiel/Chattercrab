//! Hardware H.264 encode.
//!
//! Windows uses the GPU's Media Foundation encoder (NVIDIA NVENC, Intel Quick
//! Sync, or AMD AMF, whichever the driver registered). Linux uses VA-API on a
//! DRM render node, which covers Intel and AMD directly and NVIDIA through
//! `nvidia-vaapi-driver`. libva is loaded at runtime, so the build does not
//! link it. Both paths emit Annex-B H.264 with no B-frames, which the OpenH264
//! decoder can read. The standard operating point is Constrained Baseline,
//! 1440p at 60 fps and 12 Mbps. High quality requests H.264 High at a higher
//! fixed bitrate, and 4K60 or 1440p120 uses level 5.2. Linux VA-API uses that
//! same profile, GOP, and two-frame rate-control window, with two reference
//! frames for desktop and one for game. The user picks that mode. Resolution
//! and frame rate do not.
//!
//! The Windows encoder is handed a Direct3D 11 device on the adapter that
//! registered it. Without that device, `SetOutputType` fails with
//! `MF_E_UNSUPPORTED_D3D_TYPE` and the capture thread falls back to OpenH264.
//!
//! If no hardware encoder can be opened, the capture thread uses OpenH264.

/// One encoded picture, Annex-B.
pub struct AccessUnit {
    /// NAL units of one picture, including SPS and PPS on a keyframe.
    pub data: Vec<u8>,
    /// Whether this picture is an IDR.
    pub keyframe: bool,
}

/// Rate control for one encoder open.
///
/// Standard shares keep the existing bitrate and a two-second GOP. High quality
/// follows the low-latency operating point: CBR at the spec bitrate, a GOP long
/// enough that IDRs happen only on demand, no B-frames, and a one-to-two-frame
/// rate-control window.
#[derive(Debug, Clone, Copy)]
pub struct EncodeParams {
    /// CBR target, bits per second.
    pub bitrate: u32,
    /// Frames between periodic IDRs. High quality uses a very large value.
    pub gop_frames: u32,
    /// H.264 High profile, higher bitrate, IDR only on demand.
    pub high_quality: bool,
    /// Game content: one reference and a faster preset, so moving images stay
    /// less blurry. Desktop content keeps two references and a slower preset
    /// for crisp text.
    pub game: bool,
    /// Rate-control window. About two frames when [`Self::high_quality`].
    pub rc_window_ms: u32,
}

/// Length-prefixed NALs (AVCC) become Annex-B start codes. Annex-B input is copied.
pub fn to_annex_b(src: &[u8]) -> Vec<u8> {
    if src.is_empty() || has_start_code(src) {
        return src.to_vec();
    }
    let mut out = Vec::with_capacity(src.len() + 64);
    let mut i = 0;
    while i + 4 <= src.len() {
        let len = u32::from_be_bytes([src[i], src[i + 1], src[i + 2], src[i + 3]]) as usize;
        i += 4;
        if len == 0 || i + len > src.len() {
            return src.to_vec();
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&src[i..i + len]);
        i += len;
    }
    if i != src.len() || out.is_empty() {
        src.to_vec()
    } else {
        out
    }
}

fn has_start_code(src: &[u8]) -> bool {
    src.starts_with(&[0, 0, 0, 1]) || src.starts_with(&[0, 0, 1])
}

/// Bitrate for the hardware encoder. 12 Mbps at 2560×1440 and 60 fps, scaled
/// by pixel rate. Hardware H.264 holds screen text at this rate; a higher
/// target would only spend bandwidth.
pub fn hardware_bitrate(width: u32, height: u32, fps: u32) -> u32 {
    const REF_BPS: u64 = 12_000_000;
    const REF_PIXELS: u64 = 2560 * 1440 * 60;
    let pixels = u64::from(width) * u64::from(height) * u64::from(fps.max(1));
    let bps = REF_BPS.saturating_mul(pixels) / REF_PIXELS;
    bps.clamp(4_000_000, 20_000_000) as u32
}

/// H.264 `level_idc` for this picture. 51 when it fits in level 5.1, otherwise 52.
/// Level 5.2 is what 3840×2160 at 60 fps requires; 1440p60 still fits in 5.1.
pub fn h264_level_idc(width: u32, height: u32, fps: u32) -> u32 {
    let mbs = u64::from(width.div_ceil(16)) * u64::from(height.div_ceil(16));
    let mbps = mbs.saturating_mul(u64::from(fps.max(1)));
    if mbs <= 36_864 && mbps <= 983_040 {
        51
    } else {
        52
    }
}

#[path = "h264_headers.rs"]
mod h264_headers;
#[path = "va_abi.rs"]
mod va_abi;

/// BT.601 limited-range RGBA to tightly packed NV12.
fn rgba_to_nv12(src: &[u8], width: usize, height: usize, y_plane: &mut [u8], uv_plane: &mut [u8]) {
    for y in 0..height {
        let src_row = &src[y * width * 4..(y + 1) * width * 4];
        let y_row = &mut y_plane[y * width..(y + 1) * width];
        for (x, y_px) in y_row.iter_mut().enumerate() {
            let i = x * 4;
            *y_px = luma(src_row[i], src_row[i + 1], src_row[i + 2]);
        }
    }
    let mut uv_i = 0;
    for y in (0..height).step_by(2) {
        for x in (0..width).step_by(2) {
            let mut r = 0i32;
            let mut g = 0i32;
            let mut b = 0i32;
            for dy in 0..2 {
                for dx in 0..2 {
                    let i = ((y + dy) * width + (x + dx)) * 4;
                    r += i32::from(src[i]);
                    g += i32::from(src[i + 1]);
                    b += i32::from(src[i + 2]);
                }
            }
            uv_plane[uv_i] = chroma(-38, -74, 112, r / 4, g / 4, b / 4);
            uv_plane[uv_i + 1] = chroma(112, -94, -18, r / 4, g / 4, b / 4);
            uv_i += 2;
        }
    }
}

fn luma(r: u8, g: u8, b: u8) -> u8 {
    let y = (66 * i32::from(r) + 129 * i32::from(g) + 25 * i32::from(b) + 128) >> 8;
    (y + 16).clamp(0, 255) as u8
}

fn chroma(cr: i32, cg: i32, cb: i32, r: i32, g: i32, b: i32) -> u8 {
    let c = (cr * r + cg * g + cb * b + 128) >> 8;
    (c + 128).clamp(0, 255) as u8
}

#[cfg(windows)]
#[allow(unsafe_op_in_unsafe_fn)]
mod platform {
    use std::mem::ManuallyDrop;
    use std::ptr;
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    use windows::Win32::Foundation::{HMODULE, VARIANT_TRUE};
    use windows::Win32::Graphics::Direct3D::{
        D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1,
        D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
    };
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
        D3D11CreateDevice, ID3D11Device, ID3D11Multithread,
    };
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, DXGI_ADAPTER_DESC1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIAdapter1,
        IDXGIDevice, IDXGIFactory1,
    };
    use windows::Win32::Media::MediaFoundation::{
        CODECAPI_AVEncCommonBufferSize, CODECAPI_AVEncCommonLowLatency,
        CODECAPI_AVEncCommonMaxBitRate, CODECAPI_AVEncCommonMeanBitRate,
        CODECAPI_AVEncCommonQualityVsSpeed, CODECAPI_AVEncCommonRateControlMode,
        CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize,
        CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVEncVideoMaxNumRefFrame,
        CODECAPI_AVLowLatencyMode, CODECAPI_AVScenarioInfo, ICodecAPI, IMFActivate,
        IMFDXGIDeviceManager, IMFMediaEventGenerator, IMFSample, IMFTransform, MEError,
        METransformHaveOutput, METransformNeedInput, MF_E_NO_EVENTS_AVAILABLE,
        MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_EVENT_FLAG_NO_WAIT,
        MF_MT_AVG_BITRATE, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
        MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG2_LEVEL, MF_MT_MPEG2_PROFILE,
        MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK,
        MF_VERSION, MFCreateDXGIDeviceManager, MFCreateMediaType, MFCreateMemoryBuffer,
        MFCreateSample, MFMediaType_Video, MFSTARTUP_FULL, MFSampleExtension_CleanPoint, MFStartup,
        MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_ADAPTER_LUID, MFT_ENUM_FLAG, MFT_ENUM_FLAG_HARDWARE,
        MFT_ENUM_FLAG_SORTANDFILTER, MFT_FRIENDLY_NAME_Attribute, MFT_MESSAGE_COMMAND_FLUSH,
        MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM,
        MFT_MESSAGE_NOTIFY_END_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
        MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES,
        MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoFormat_H264,
        MFVideoFormat_NV12, MFVideoInterlace_Progressive, eAVEncCommonRateControlMode_CBR,
        eAVEncH264VProfile_Base, eAVEncH264VProfile_ConstrainedBase, eAVEncH264VProfile_High,
        eAVScenarioInfo_DisplayRemoting, eAVScenarioInfo_LiveStreaming,
    };
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
    use windows::Win32::System::Variant::{VARIANT, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4};
    use windows::core::Interface;

    use super::{AccessUnit, EncodeParams, to_annex_b};
    use crate::screen::receive::contains_idr;

    /// How long to wait for the encoder to accept a frame.
    const NEED_INPUT_TIMEOUT: Duration = Duration::from_millis(500);
    /// How long to wait for that frame's output before returning. A picture that
    /// is still inside the encoder comes out on the next call.
    const OUTPUT_GRACE: Duration = Duration::from_millis(4);

    /// GPU H.264 encoder. Touched only from the capture thread that created it.
    pub struct HwEncoder {
        width: u32,
        height: u32,
        fps: u32,
        transform: IMFTransform,
        // Held so the MFT's DXGI device outlives clearing the manager in Drop.
        device: ID3D11Device,
        device_manager: IMFDXGIDeviceManager,
        event_gen: Option<IMFMediaEventGenerator>,
        codec_api: Option<ICodecAPI>,
        output_provides_samples: bool,
        output_buffer_size: u32,
        input_stream_id: u32,
        output_stream_id: u32,
        need_input_credits: u32,
        frames_submitted: u64,
    }

    impl HwEncoder {
        /// Opens a Media Foundation encoder for even `width` x `height` frames at `fps`.
        pub fn open(
            width: u32,
            height: u32,
            fps: u32,
            params: EncodeParams,
        ) -> Result<Self, String> {
            if width < 2 || height < 2 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
                return Err(format!(
                    "hardware encoder needs even dimensions, got {width}x{height}"
                ));
            }
            let fps = fps.max(1);
            ensure_mf()?;
            unsafe { Self::start(width, height, fps, params) }
        }

        /// Encodes one tightly packed RGBA frame. `force_idr` starts a new GOP.
        pub fn encode(&mut self, rgba: &[u8], force_idr: bool) -> Result<Vec<AccessUnit>, String> {
            let expected = (self.width as usize) * (self.height as usize) * 4;
            if rgba.len() != expected {
                return Err(format!(
                    "frame is {} bytes, expected {expected}",
                    rgba.len()
                ));
            }
            let sample = unsafe { self.input_sample(rgba)? };
            if let Some(api) = &self.codec_api {
                // Some drivers leave this set, so every later picture becomes an IDR.
                let force = u32::from(force_idr || self.frames_submitted == 0);
                set_codec(api, &CODECAPI_AVEncVideoForceKeyFrame, variant_u32(force));
            }
            let transform = self.transform.clone();
            let mut units = Vec::new();
            if let Some(events) = self.event_gen.clone() {
                self.encode_async(&transform, &events, sample, &mut units)?;
            } else {
                self.encode_sync(&transform, sample, &mut units)?;
            }
            self.frames_submitted += 1;
            Ok(units)
        }

        unsafe fn start(
            width: u32,
            height: u32,
            fps: u32,
            params: EncodeParams,
        ) -> Result<Self, String> {
            let activates =
                enumerate().map_err(|e| format!("enumerating hardware encoders: {e}"))?;
            if activates.is_empty() {
                return Err("no hardware h264 encoder".into());
            }
            let mut errors = Vec::new();
            for activate in &activates {
                let name = friendly_name(activate).unwrap_or_else(|| "hardware h264".to_owned());
                match open_activated(activate, &name, width, height, fps, params) {
                    Ok(encoder) => return Ok(encoder),
                    Err(error) => {
                        tracing::debug!(encoder = %name, %error, "skipping hardware encoder");
                        errors.push(format!("{name}: {error}"));
                    }
                }
            }
            Err(errors.join("; "))
        }

        unsafe fn input_sample(&self, rgba: &[u8]) -> Result<IMFSample, String> {
            let w = self.width as usize;
            let h = self.height as usize;
            let nv12_len = w * h * 3 / 2;
            let buffer =
                MFCreateMemoryBuffer(nv12_len as u32).map_err(|e| format!("input buffer: {e}"))?;
            let mut data_ptr: *mut u8 = ptr::null_mut();
            buffer
                .Lock(&mut data_ptr, None, None)
                .map_err(|e| format!("locking input buffer: {e}"))?;
            {
                let dst = std::slice::from_raw_parts_mut(data_ptr, nv12_len);
                let (y_plane, uv_plane) = dst.split_at_mut(w * h);
                super::rgba_to_nv12(rgba, w, h, y_plane, uv_plane);
            }
            buffer
                .Unlock()
                .map_err(|e| format!("unlocking input buffer: {e}"))?;
            buffer
                .SetCurrentLength(nv12_len as u32)
                .map_err(|e| format!("input length: {e}"))?;
            let sample = MFCreateSample().map_err(|e| format!("input sample: {e}"))?;
            sample
                .AddBuffer(&buffer)
                .map_err(|e| format!("attaching input buffer: {e}"))?;
            let time = (self.frames_submitted as i64) * 10_000_000 / i64::from(self.fps);
            let duration = 10_000_000 / i64::from(self.fps);
            sample
                .SetSampleTime(time)
                .map_err(|e| format!("sample time: {e}"))?;
            sample
                .SetSampleDuration(duration)
                .map_err(|e| format!("sample duration: {e}"))?;
            Ok(sample)
        }

        fn encode_async(
            &mut self,
            transform: &IMFTransform,
            events: &IMFMediaEventGenerator,
            sample: IMFSample,
            units: &mut Vec<AccessUnit>,
        ) -> Result<(), String> {
            let deadline = Instant::now() + NEED_INPUT_TIMEOUT;
            loop {
                if self.need_input_credits > 0 {
                    self.need_input_credits -= 1;
                    unsafe {
                        transform
                            .ProcessInput(self.input_stream_id, &sample, 0)
                            .map_err(|e| format!("ProcessInput: {e}"))?;
                    }
                    break;
                }
                if !self.poll(events, transform, units)? && Instant::now() >= deadline {
                    return Err("timed out waiting for the hardware encoder".into());
                }
                if self.need_input_credits == 0 {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            let grace = Instant::now() + OUTPUT_GRACE;
            let mut got = false;
            loop {
                let before = units.len();
                let handled = self.poll(events, transform, units)?;
                got |= units.len() > before;
                if !handled && (got || Instant::now() >= grace) {
                    break;
                }
                if !handled {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            Ok(())
        }

        fn encode_sync(
            &mut self,
            transform: &IMFTransform,
            sample: IMFSample,
            units: &mut Vec<AccessUnit>,
        ) -> Result<(), String> {
            unsafe {
                transform
                    .ProcessInput(self.input_stream_id, &sample, 0)
                    .map_err(|e| format!("ProcessInput: {e}"))?;
            }
            while self.pull_output(transform, units)? {}
            Ok(())
        }

        /// Handles one queued event. Returns false when the queue is empty.
        fn poll(
            &mut self,
            events: &IMFMediaEventGenerator,
            transform: &IMFTransform,
            units: &mut Vec<AccessUnit>,
        ) -> Result<bool, String> {
            let event = unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) };
            let event = match event {
                Ok(event) => event,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(false),
                Err(e) => return Err(format!("encoder event: {e}")),
            };
            let kind =
                unsafe { event.GetType() }.map_err(|e| format!("encoder event type: {e}"))?;
            if kind == METransformNeedInput.0 as u32 {
                self.need_input_credits += 1;
            } else if kind == METransformHaveOutput.0 as u32 {
                self.pull_output(transform, units)?;
            } else if kind == MEError.0 as u32 {
                return Err("hardware encoder reported an error".into());
            }
            Ok(true)
        }

        /// Pulls one output picture. Returns false when the encoder wants more input.
        fn pull_output(
            &mut self,
            transform: &IMFTransform,
            units: &mut Vec<AccessUnit>,
        ) -> Result<bool, String> {
            unsafe {
                loop {
                    let provided = if self.output_provides_samples {
                        None
                    } else {
                        Some(self.output_sample()?)
                    };
                    let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                        dwStreamID: self.output_stream_id,
                        pSample: ManuallyDrop::new(provided),
                        dwStatus: 0,
                        pEvents: ManuallyDrop::new(None),
                    }];
                    let mut status = 0u32;
                    let result = transform.ProcessOutput(0, &mut buffers, &mut status);
                    let sample = ManuallyDrop::take(&mut buffers[0].pSample);
                    let _events = ManuallyDrop::take(&mut buffers[0].pEvents);
                    match result {
                        Ok(()) => {
                            if let Some(sample) = sample
                                && let Some(unit) = read_sample(&sample)?
                            {
                                units.push(unit);
                            }
                            return Ok(true);
                        }
                        Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(false),
                        Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                            let new_type = transform
                                .GetOutputAvailableType(self.output_stream_id, 0)
                                .map_err(|err| format!("output type after stream change: {err}"))?;
                            transform
                                .SetOutputType(self.output_stream_id, &new_type, 0)
                                .map_err(|err| format!("resetting output type: {err}"))?;
                            if let Ok(info) = transform.GetOutputStreamInfo(self.output_stream_id) {
                                self.output_provides_samples = info.dwFlags
                                    & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                                        | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
                                    != 0;
                                self.output_buffer_size = info.cbSize;
                            }
                        }
                        Err(e) => return Err(format!("ProcessOutput: {e}")),
                    }
                }
            }
        }

        unsafe fn output_sample(&self) -> Result<IMFSample, String> {
            let size = if self.output_buffer_size > 0 {
                self.output_buffer_size
            } else {
                self.width * self.height * 3 / 2
            };
            let buffer = MFCreateMemoryBuffer(size).map_err(|e| format!("output buffer: {e}"))?;
            let sample = MFCreateSample().map_err(|e| format!("output sample: {e}"))?;
            sample
                .AddBuffer(&buffer)
                .map_err(|e| format!("attaching output buffer: {e}"))?;
            Ok(sample)
        }
    }

    impl Drop for HwEncoder {
        fn drop(&mut self) {
            unsafe {
                let _ = self
                    .transform
                    .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, 0);
                let _ = self.transform.ProcessMessage(
                    MFT_MESSAGE_NOTIFY_END_OF_STREAM,
                    self.input_stream_id as usize,
                );
                let _ = self
                    .transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
                let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
                // The MFT drops its manager reference above. These stay alive until then.
                let _ = (self.device.as_raw(), self.device_manager.as_raw());
            }
        }
    }

    /// Direct3D device on the GPU that owns one hardware encoder.
    struct Gpu {
        device: ID3D11Device,
        manager: IMFDXGIDeviceManager,
        adapter: String,
    }

    unsafe fn open_activated(
        activate: &IMFActivate,
        name: &str,
        width: u32,
        height: u32,
        fps: u32,
        params: EncodeParams,
    ) -> Result<HwEncoder, String> {
        let gpu = open_gpu(encoder_luid(activate))?;
        let transform: IMFTransform = activate
            .ActivateObject()
            .map_err(|e| format!("activating {name}: {e}"))?;
        let opened = bind(transform.clone(), gpu, name, width, height, fps, params);
        if opened.is_err() {
            let _ = activate.ShutdownObject();
        }
        opened
    }

    unsafe fn bind(
        transform: IMFTransform,
        gpu: Gpu,
        name: &str,
        width: u32,
        height: u32,
        fps: u32,
        params: EncodeParams,
    ) -> Result<HwEncoder, String> {
        let mut is_async = false;
        if let Ok(attrs) = transform.GetAttributes() {
            is_async = attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) == 1;
            if is_async {
                attrs
                    .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                    .map_err(|e| format!("unlocking {name}: {e}"))?;
            }
        }

        // Without a device on this adapter, SetOutputType returns
        // MF_E_UNSUPPORTED_D3D_TYPE (0xC00D6D76).
        transform
            .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, gpu.manager.as_raw() as usize)
            .map_err(|e| format!("D3D manager on {name}: {e}"))?;

        let mut input_ids = [0u32; 1];
        let mut output_ids = [0u32; 1];
        let (input_stream_id, output_stream_id) =
            match transform.GetStreamIDs(&mut input_ids, &mut output_ids) {
                Ok(()) => (input_ids[0], output_ids[0]),
                Err(_) => (0, 0),
            };

        let bitrate = params.bitrate;
        let codec_api: Option<ICodecAPI> = transform.cast().ok();

        // Set the media types first. Media Foundation drops codec properties
        // that were written before SetOutputType.
        set_output_type(
            &transform,
            output_stream_id,
            width,
            height,
            fps,
            bitrate,
            params.high_quality,
        )?;
        set_input_type(&transform, input_stream_id, width, height, fps)?;
        if let Some(api) = &codec_api {
            configure_codec(api, fps, bitrate, params);
        }

        let info = transform
            .GetOutputStreamInfo(output_stream_id)
            .map_err(|e| format!("output stream info: {e}"))?;
        let provides = info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
            != 0;
        let event_gen = if is_async {
            Some(
                transform
                    .cast::<IMFMediaEventGenerator>()
                    .map_err(|e| format!("{name} has no event generator: {e}"))?,
            )
        } else {
            None
        };
        transform
            .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
            .and_then(|()| transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0))
            .map_err(|e| format!("starting {name}: {e}"))?;

        tracing::info!(
            encoder = %name,
            adapter = %gpu.adapter,
            width,
            height,
            fps,
            bitrate,
            high_quality = params.high_quality,
            game = params.game,
            async_mft = is_async,
            "hardware h264 encoder"
        );

        Ok(HwEncoder {
            width,
            height,
            fps,
            transform,
            device: gpu.device,
            device_manager: gpu.manager,
            event_gen,
            codec_api,
            output_provides_samples: provides,
            output_buffer_size: info.cbSize,
            input_stream_id,
            output_stream_id,
            need_input_credits: 0,
            frames_submitted: 0,
        })
    }

    unsafe fn encoder_luid(activate: &IMFActivate) -> Option<u64> {
        activate.GetUINT64(&MFT_ENUM_ADAPTER_LUID).ok()
    }

    unsafe fn open_gpu(luid: Option<u64>) -> Result<Gpu, String> {
        let adapter = find_adapter(luid)?;
        let desc = adapter
            .GetDesc1()
            .map_err(|e| format!("adapter desc: {e}"))?;
        let adapter_name = adapter_name(&desc);
        let device = create_device(&adapter, &adapter_name)?;
        if let Ok(multithread) = device.cast::<ID3D11Multithread>() {
            let _ = multithread.SetMultithreadProtected(true);
        }
        let dxgi = device
            .cast::<IDXGIDevice>()
            .map_err(|e| format!("IDXGIDevice for {adapter_name}: {e}"))?;
        let mut token = 0u32;
        let mut manager = None;
        MFCreateDXGIDeviceManager(&mut token, &mut manager)
            .map_err(|e| format!("DXGI device manager: {e}"))?;
        let manager = manager.ok_or_else(|| "DXGI device manager was not created".to_owned())?;
        manager
            .ResetDevice(&dxgi, token)
            .map_err(|e| format!("binding {adapter_name} to the encoder: {e}"))?;
        Ok(Gpu {
            device,
            manager,
            adapter: adapter_name,
        })
    }

    unsafe fn find_adapter(luid: Option<u64>) -> Result<IDXGIAdapter1, String> {
        let factory: IDXGIFactory1 =
            CreateDXGIFactory1().map_err(|e| format!("DXGI factory: {e}"))?;
        let mut best: Option<(IDXGIAdapter1, usize)> = None;
        let mut index = 0u32;
        loop {
            let adapter = match factory.EnumAdapters1(index) {
                Ok(adapter) => adapter,
                Err(_) => break,
            };
            index += 1;
            let Ok(desc) = adapter.GetDesc1() else {
                continue;
            };
            let packed = u64::from(desc.AdapterLuid.LowPart)
                | (u64::from(desc.AdapterLuid.HighPart as u32) << 32);
            if let Some(want) = luid {
                if packed == want {
                    return Ok(adapter);
                }
                continue;
            }
            if desc.Flags & (DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0 {
                continue;
            }
            let memory = desc.DedicatedVideoMemory;
            if best.as_ref().is_none_or(|(_, best_mem)| memory > *best_mem) {
                best = Some((adapter, memory));
            }
        }
        match (luid, best) {
            (_, Some((adapter, _))) => Ok(adapter),
            (Some(_), None) => Err("encoder GPU is not in the DXGI adapter list".into()),
            (None, None) => Err("no hardware GPU adapter".into()),
        }
    }

    unsafe fn create_device(adapter: &IDXGIAdapter1, name: &str) -> Result<ID3D11Device, String> {
        let levels = [
            D3D_FEATURE_LEVEL_11_1,
            D3D_FEATURE_LEVEL_11_0,
            D3D_FEATURE_LEVEL_10_1,
            D3D_FEATURE_LEVEL_10_0,
        ];
        let flags = [
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        ];
        let mut last = String::from("D3D11CreateDevice was not called");
        for flag in flags {
            let mut device = None;
            match D3D11CreateDevice(
                adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE(ptr::null_mut()),
                flag,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            ) {
                Ok(()) => {
                    if let Some(device) = device {
                        return Ok(device);
                    }
                    last = format!("D3D11CreateDevice returned no device for {name}");
                }
                Err(error) => last = format!("D3D11 device on {name}: {error}"),
            }
        }
        Err(last)
    }

    fn adapter_name(desc: &DXGI_ADAPTER_DESC1) -> String {
        let end = desc
            .Description
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(desc.Description.len());
        String::from_utf16_lossy(&desc.Description[..end])
    }

    fn ensure_mf() -> Result<(), String> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        static STARTED: OnceLock<Result<(), String>> = OnceLock::new();
        STARTED
            .get_or_init(|| unsafe {
                MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(|e| format!("MFStartup: {e}"))
            })
            .clone()
    }

    unsafe fn enumerate() -> windows::core::Result<Vec<IMFActivate>> {
        let output_type = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };
        let mut activates: *mut Option<IMFActivate> = ptr::null_mut();
        let mut count = 0u32;
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0),
            None,
            Some(&output_type),
            &mut activates,
            &mut count,
        )?;
        let mut out = Vec::with_capacity(count as usize);
        if !activates.is_null() {
            for i in 0..count as usize {
                if let Some(activate) = (*activates.add(i)).take() {
                    out.push(activate);
                }
            }
            CoTaskMemFree(Some(activates.cast()));
        }
        Ok(out)
    }

    unsafe fn friendly_name(activate: &IMFActivate) -> Option<String> {
        let mut value = windows::core::PWSTR::null();
        let mut len = 0u32;
        activate
            .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut value, &mut len)
            .ok()?;
        if value.is_null() {
            return None;
        }
        let name = value.to_string().ok();
        CoTaskMemFree(Some(value.as_ptr().cast()));
        name
    }

    unsafe fn set_output_type(
        transform: &IMFTransform,
        stream: u32,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        high_quality: bool,
    ) -> Result<(), String> {
        // High profile is the universal fallback in the streaming spec: every
        // viewer decodes H.264, and High is the best of those. Drivers that
        // reject it fall through to Constrained Baseline, which OpenH264 still reads.
        let profiles: &[u32] = if high_quality {
            &[
                eAVEncH264VProfile_High.0 as u32,
                eAVEncH264VProfile_ConstrainedBase.0 as u32,
                eAVEncH264VProfile_Base.0 as u32,
            ]
        } else {
            &[
                eAVEncH264VProfile_ConstrainedBase.0 as u32,
                eAVEncH264VProfile_Base.0 as u32,
            ]
        };
        let mut last = String::from("no output type");
        for &profile in profiles {
            let output = match h264_output_type(width, height, fps, bitrate, profile) {
                Ok(output) => output,
                Err(e) => {
                    last = e;
                    continue;
                }
            };
            match transform.SetOutputType(stream, &output, 0) {
                Ok(()) => return Ok(()),
                Err(e) => last = e.to_string(),
            }
        }
        Err(format!("setting h264 output type: {last}"))
    }

    unsafe fn h264_output_type(
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        profile: u32,
    ) -> Result<windows::Win32::Media::MediaFoundation::IMFMediaType, String> {
        let output = MFCreateMediaType().map_err(|e| format!("output media type: {e}"))?;
        output
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .and_then(|()| output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264))
            .and_then(|()| output.SetUINT32(&MF_MT_AVG_BITRATE, bitrate))
            .and_then(|()| output.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height)))
            .and_then(|()| output.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1)))
            .and_then(|()| output.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1)))
            .and_then(|()| {
                output.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            })
            .and_then(|()| output.SetUINT32(&MF_MT_MPEG2_PROFILE, profile))
            .and_then(|()| {
                output.SetUINT32(
                    &MF_MT_MPEG2_LEVEL,
                    super::h264_level_idc(width, height, fps),
                )
            })
            .map_err(|e| format!("building h264 output type: {e}"))?;
        Ok(output)
    }

    unsafe fn set_input_type(
        transform: &IMFTransform,
        stream: u32,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<(), String> {
        let input = MFCreateMediaType().map_err(|e| format!("input media type: {e}"))?;
        input
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .and_then(|()| input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12))
            .and_then(|()| input.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height)))
            .and_then(|()| input.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1)))
            .and_then(|()| {
                input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            })
            .and_then(|()| input.SetUINT32(&MF_MT_DEFAULT_STRIDE, width))
            .map_err(|e| format!("building nv12 input type: {e}"))?;
        transform
            .SetInputType(stream, &input, 0)
            .map_err(|e| format!("setting nv12 input type: {e}"))
    }

    unsafe fn read_sample(sample: &IMFSample) -> Result<Option<AccessUnit>, String> {
        let buffer = sample
            .ConvertToContiguousBuffer()
            .map_err(|e| format!("output buffer: {e}"))?;
        let mut data_ptr: *mut u8 = ptr::null_mut();
        let mut current_len = 0u32;
        buffer
            .Lock(&mut data_ptr, None, Some(&mut current_len))
            .map_err(|e| format!("locking output: {e}"))?;
        let bytes = if data_ptr.is_null() || current_len == 0 {
            &[][..]
        } else {
            std::slice::from_raw_parts(data_ptr, current_len as usize)
        };
        let data = to_annex_b(bytes);
        let clean = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) == 1;
        buffer
            .Unlock()
            .map_err(|e| format!("unlocking output: {e}"))?;
        if data.is_empty() {
            return Ok(None);
        }
        Ok(Some(AccessUnit {
            keyframe: clean || contains_idr(&data),
            data,
        }))
    }

    fn pack(high: u32, low: u32) -> u64 {
        (u64::from(high) << 32) | u64::from(low)
    }

    fn variant_u32(value: u32) -> VARIANT {
        let mut var = VARIANT::default();
        var.Anonymous.Anonymous = ManuallyDrop::new(VARIANT_0_0 {
            vt: VT_UI4,
            wReserved1: 0,
            wReserved2: 0,
            wReserved3: 0,
            Anonymous: VARIANT_0_0_0 { ulVal: value },
        });
        var
    }

    fn variant_bool(value: bool) -> VARIANT {
        let mut var = VARIANT::default();
        var.Anonymous.Anonymous = ManuallyDrop::new(VARIANT_0_0 {
            vt: VT_BOOL,
            wReserved1: 0,
            wReserved2: 0,
            wReserved3: 0,
            Anonymous: VARIANT_0_0_0 {
                boolVal: if value {
                    VARIANT_TRUE
                } else {
                    windows::Win32::Foundation::VARIANT_FALSE
                },
            },
        });
        var
    }

    /// Rate control, content mode, and the high-quality VBV. Called after both
    /// media types are set, which is when the encoder keeps the values.
    fn configure_codec(api: &ICodecAPI, fps: u32, bitrate: u32, params: EncodeParams) {
        set_codec(
            api,
            &CODECAPI_AVEncCommonRateControlMode,
            variant_u32(eAVEncCommonRateControlMode_CBR.0 as u32),
        );
        set_codec(api, &CODECAPI_AVEncCommonMeanBitRate, variant_u32(bitrate));
        set_codec(api, &CODECAPI_AVLowLatencyMode, variant_bool(true));
        set_codec(api, &CODECAPI_AVEncMPVDefaultBPictureCount, variant_u32(0));
        set_codec(
            api,
            &CODECAPI_AVEncMPVGOPSize,
            variant_u32(params.gop_frames.max(1)),
        );
        // Display remoting keeps text sharp. Live streaming spends bits on motion.
        let scenario = if params.game {
            eAVScenarioInfo_LiveStreaming
        } else {
            eAVScenarioInfo_DisplayRemoting
        };
        set_codec(
            api,
            &CODECAPI_AVScenarioInfo,
            variant_u32(scenario.0 as u32),
        );
        if params.high_quality {
            // VBV holds about two frames. The property is in bits.
            let buffer_bits = bitrate.saturating_mul(2) / fps.max(1);
            set_codec(api, &CODECAPI_AVEncCommonMaxBitRate, variant_u32(bitrate));
            set_codec(
                api,
                &CODECAPI_AVEncCommonBufferSize,
                variant_u32(buffer_bits.max(1)),
            );
            // One reference for game motion, two for desktop text.
            set_codec(
                api,
                &CODECAPI_AVEncVideoMaxNumRefFrame,
                variant_u32(if params.game { 1 } else { 2 }),
            );
            set_codec(api, &CODECAPI_AVEncCommonLowLatency, variant_bool(true));
            // 0 is slowest, 100 is fastest. 33 ≈ NVENC P5, 50 ≈ P4.
            set_codec(
                api,
                &CODECAPI_AVEncCommonQualityVsSpeed,
                variant_u32(if params.game { 50 } else { 33 }),
            );
        }
    }

    fn set_codec(api: &ICodecAPI, guid: &windows::core::GUID, value: VARIANT) {
        unsafe {
            let _ = api.SetValue(guid, &value);
        }
    }
}

#[cfg(target_os = "linux")]
#[path = "hw_linux.rs"]
mod platform;

#[cfg(not(any(windows, target_os = "linux")))]
mod platform {
    use super::AccessUnit;

    /// Stub. Hardware encode is Media Foundation on Windows and VA-API on Linux.
    pub struct HwEncoder;

    impl HwEncoder {
        /// Hardware encode is not available on this platform.
        pub fn open(
            _width: u32,
            _height: u32,
            _fps: u32,
            _params: super::EncodeParams,
        ) -> Result<Self, String> {
            Err("no hardware h264 encoder".into())
        }

        /// Hardware encode is not available on this platform.
        pub fn encode(
            &mut self,
            _rgba: &[u8],
            _force_idr: bool,
        ) -> Result<Vec<AccessUnit>, String> {
            Err("no hardware h264 encoder".into())
        }
    }
}

pub use platform::HwEncoder;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annex_b_is_unchanged_and_avcc_gains_start_codes() {
        let annex = [0u8, 0, 0, 1, 0x65, 0x88];
        assert_eq!(to_annex_b(&annex), annex);
        let avcc = [0u8, 0, 0, 2, 0x65, 0x88];
        assert_eq!(to_annex_b(&avcc), vec![0, 0, 0, 1, 0x65, 0x88]);
    }

    #[test]
    fn bitrate_is_12_mbps_at_1440p60_and_floored() {
        assert_eq!(hardware_bitrate(2560, 1440, 60), 12_000_000);
        assert_eq!(hardware_bitrate(1920, 1080, 30), 4_000_000);
        assert_eq!(hardware_bitrate(3840, 2160, 60), 20_000_000);
    }

    #[test]
    fn level_stays_5_1_until_4k60() {
        assert_eq!(h264_level_idc(2560, 1440, 60), 51);
        assert_eq!(h264_level_idc(1920, 1080, 60), 51);
        assert_eq!(h264_level_idc(3840, 2160, 30), 51);
        assert_eq!(h264_level_idc(3840, 2160, 60), 52);
        assert_eq!(h264_level_idc(2560, 1440, 120), 52);
    }

    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn hardware_encoder_roundtrips_through_openh264_when_present() {
        let mut encoder = match HwEncoder::open(
            320,
            240,
            30,
            EncodeParams {
                bitrate: hardware_bitrate(320, 240, 30),
                gop_frames: 60,
                high_quality: false,
                game: false,
                rc_window_ms: 1_000,
            },
        ) {
            Ok(encoder) => encoder,
            Err(error) => {
                eprintln!("hardware encoder unavailable: {error}");
                return;
            }
        };
        let mut decoder = crate::screen::receive::new_decoder().expect("decoder");
        let mut rgba = vec![0u8; 320 * 240 * 4];
        for (i, px) in rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            px[0] = (i % 255) as u8;
            px[1] = 40;
            px[2] = 180;
            px[3] = 255;
        }
        let mut decoded = false;
        for n in 0..8 {
            rgba[0] = n as u8;
            let units = encoder.encode(&rgba, n == 0).expect("encode");
            for unit in units {
                assert!(
                    unit.data.starts_with(&[0, 0, 0, 1]) || unit.data.starts_with(&[0, 0, 1]),
                    "hardware encoder did not emit annex-b"
                );
                if let Some(frame) =
                    crate::screen::receive::decode_to_rgba(&mut decoder, &unit.data)
                        .expect("decode")
                {
                    assert_eq!((frame.width, frame.height), (320, 240));
                    decoded = true;
                }
            }
            if decoded {
                return;
            }
        }
        panic!("hardware encoder output did not decode");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn high_quality_hardware_encoder_uses_high_profile_when_present() {
        let params = crate::screen::encode_params(true, 320, 240, 60, true, false);
        let mut encoder = match HwEncoder::open(320, 240, 60, params) {
            Ok(encoder) => encoder,
            Err(error) => {
                eprintln!("hardware encoder unavailable: {error}");
                return;
            }
        };
        let mut rgba = vec![0u8; 320 * 240 * 4];
        for (i, px) in rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            px[0] = (i % 255) as u8;
            px[1] = 16;
            px[2] = 200;
            px[3] = 255;
        }
        let expect_high = encoder.high_profile_selected();
        let units = encoder.encode(&rgba, true).expect("encode");
        let mut saw_idr = false;
        let mut decoded = false;
        let mut decoder = crate::screen::receive::new_decoder().expect("decoder");
        for unit in units {
            if unit.keyframe {
                let profile = sps_profile_idc(&unit.data);
                if expect_high {
                    assert_eq!(
                        profile,
                        Some(100),
                        "high quality VA-API output should be H.264 High"
                    );
                } else {
                    assert!(
                        profile == Some(66) || profile == Some(100),
                        "unexpected fallback profile {profile:?}"
                    );
                }
                saw_idr = true;
            }
            if let Some(frame) =
                crate::screen::receive::decode_to_rgba(&mut decoder, &unit.data).expect("decode")
            {
                assert_eq!((frame.width, frame.height), (320, 240));
                decoded = true;
            }
        }
        assert!(saw_idr, "high quality encode did not emit an IDR");
        assert!(decoded, "high quality hardware output did not decode");
    }

    #[cfg(target_os = "linux")]
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
            if au[header] & 0x1f == 7 {
                return au.get(next).copied();
            }
            i = next;
        }
        None
    }
}
