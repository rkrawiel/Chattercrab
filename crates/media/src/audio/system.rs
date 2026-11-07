//! Capture of other applications' audio, mixed into the outgoing voice while a screen
//! share asks for it.
//!
//! On Windows this is a WASAPI process loopback that excludes this process, so playback
//! of the call itself is not sent back to the room. Other platforms report that the
//! capture is unavailable; the screen share still starts.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use ringbuf::HeapProd;
use ringbuf::traits::Producer;

use super::resample::MonoResampler;
use super::share::ShareMix;
use super::{FRAME_SAMPLES, SAMPLE_RATE};
use crate::MediaError;

/// Capacity of the system-audio ring, in milliseconds at 48 kHz.
const RING_MS: u32 = 500;

/// A running system-audio capture. Dropping it stops the thread and detaches the mix.
pub(crate) struct SystemCapture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    mix: Arc<ShareMix>,
}

impl SystemCapture {
    /// Starts capturing other applications' audio into `mix`.
    ///
    /// Blocks until the capture is running or has failed.
    pub(crate) fn start(mix: Arc<ShareMix>) -> Result<Self, MediaError> {
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread_mix = mix.clone();
        let thread = std::thread::Builder::new()
            .name("cc-system-audio".into())
            .spawn(move || thread_main(thread_mix, thread_stop, ready_tx))
            .map_err(|e| MediaError::Audio(format!("spawn system audio thread: {e}")))?;
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self {
                stop,
                thread: Some(thread),
                mix,
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                stop.store(true, Ordering::Release);
                let _ = thread.join();
                Err(MediaError::Audio("system audio capture timed out".into()))
            }
        }
    }
}

impl Drop for SystemCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
        self.mix.detach();
    }
}

fn thread_main(
    mix: Arc<ShareMix>,
    stop: Arc<AtomicBool>,
    ready: std::sync::mpsc::SyncSender<Result<(), MediaError>>,
) {
    let run = || -> Result<(), MediaError> {
        let _com = com_init()?;
        let mut session = open_session()?;
        let Some(cons) = session.cons.take() else {
            return Err(MediaError::Audio(
                "system audio capture produced no buffer".into(),
            ));
        };
        mix.attach(cons);
        let _detach = Detach(mix);
        if ready.send(Ok(())).is_err() {
            return Ok(());
        }
        session.pump(&stop)
    };
    if let Err(e) = run() {
        let _ = ready.send(Err(e));
    }
}

/// Detaches the mix when the capture thread leaves, including on panic.
struct Detach(Arc<ShareMix>);

impl Drop for Detach {
    fn drop(&mut self) {
        self.0.detach();
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
struct Session {
    cons: Option<ringbuf::HeapCons<f32>>,
    pump: Pump,
}

#[cfg_attr(not(windows), allow(dead_code))]
struct Pump {
    prod: HeapProd<f32>,
    rate: u32,
    channels: usize,
    kind: SampleKind,
    resampler: Option<MonoResampler>,
    pending: Vec<f32>,
    chunk_buf: Vec<f32>,
    #[cfg(windows)]
    client: windows::Win32::Media::Audio::IAudioClient,
    #[cfg(windows)]
    capture: windows::Win32::Media::Audio::IAudioCaptureClient,
}

impl Session {
    fn pump(&mut self, stop: &AtomicBool) -> Result<(), MediaError> {
        self.pump.run(stop)
    }
}

#[derive(Clone, Copy)]
#[cfg_attr(not(windows), allow(dead_code))]
enum SampleKind {
    F32,
    I16,
}

impl Pump {
    #[cfg_attr(not(windows), allow(dead_code))]
    fn emit(&mut self, mono: &[f32]) {
        if let Some(resampler) = self.resampler.as_mut() {
            self.pending.extend_from_slice(mono);
            let chunk = resampler.chunk();
            while self.pending.len() >= chunk {
                self.chunk_buf.copy_from_slice(&self.pending[..chunk]);
                self.pending.drain(..chunk);
                match resampler.process(&self.chunk_buf) {
                    Ok(out) => {
                        let _ = self.prod.push_slice(out);
                    }
                    Err(e) => tracing::warn!("system audio resample: {e}"),
                }
            }
        } else {
            let _ = self.prod.push_slice(mono);
        }
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    fn push_silence(&mut self, frames: usize) {
        let zeros = [0f32; 480];
        let mut left = frames;
        while left > 0 {
            let n = left.min(zeros.len());
            self.emit(&zeros[..n]);
            left -= n;
        }
    }
}

#[cfg(windows)]
impl Drop for Pump {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

/// Downmix interleaved `f32` frames to mono.
pub(crate) fn downmix_f32(interleaved: &[f32], channels: usize, out: &mut Vec<f32>) {
    out.clear();
    if channels == 0 {
        return;
    }
    let scale = 1.0 / channels as f32;
    out.reserve(interleaved.len() / channels);
    for frame in interleaved.chunks_exact(channels) {
        let sum: f32 = frame.iter().copied().sum();
        out.push(sum * scale);
    }
}

/// Downmix interleaved `i16` frames to mono `f32` in [-1, 1].
pub(crate) fn downmix_i16(interleaved: &[i16], channels: usize, out: &mut Vec<f32>) {
    out.clear();
    if channels == 0 {
        return;
    }
    let scale = 1.0 / (channels as f32 * 32768.0);
    out.reserve(interleaved.len() / channels);
    for frame in interleaved.chunks_exact(channels) {
        let sum: i32 = frame.iter().map(|s| i32::from(*s)).sum();
        out.push(sum as f32 * scale);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downmix_averages_channels() {
        let mut out = Vec::new();
        downmix_f32(&[0.0, 1.0, 0.5, 0.5], 2, &mut out);
        assert_eq!(out, vec![0.5, 0.5]);
        downmix_i16(&[0, 0, 16384, -16384], 2, &mut out);
        assert!(out[0].abs() < 1e-6);
        assert!(out[1].abs() < 1e-4, "{}", out[1]);
    }
}

#[cfg(not(windows))]
fn com_init() -> Result<(), MediaError> {
    Ok(())
}

#[cfg(not(windows))]
fn open_session() -> Result<Session, MediaError> {
    Err(MediaError::Audio(
        "sharing audio from other apps is only available on Windows".into(),
    ))
}

#[cfg(not(windows))]
impl Pump {
    fn run(&mut self, _stop: &AtomicBool) -> Result<(), MediaError> {
        Ok(())
    }
}

#[cfg(windows)]
fn com_init() -> Result<ComGuard, MediaError> {
    ComGuard::init()
}

#[cfg(windows)]
fn open_session() -> Result<Session, MediaError> {
    windows_impl::open_session()
}

#[cfg(windows)]
struct ComGuard {
    uninit: bool,
}

#[cfg(windows)]
impl ComGuard {
    fn init() -> Result<Self, MediaError> {
        let hr = unsafe {
            windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_MULTITHREADED,
            )
        };
        if hr.is_err() {
            return Err(MediaError::Audio(format!("COM init failed: {hr}")));
        }
        Ok(Self { uninit: hr.0 == 0 })
    }
}

#[cfg(windows)]
impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.uninit {
            unsafe { windows::Win32::System::Com::CoUninitialize() }
        }
    }
}

#[cfg(windows)]
mod windows_impl {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use ringbuf::HeapRb;
    use ringbuf::traits::Split;
    use windows::Win32::Media::Audio::{
        AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
        AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
        AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
        ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
        IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
        IAudioCaptureClient, IAudioClient, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
        VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
    };
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows::Win32::System::Com::{BLOB, CoTaskMemFree};
    use windows::Win32::System::Variant::VT_BLOB;
    use windows::core::{HRESULT, Interface, implement};

    use super::{
        FRAME_SAMPLES, Pump, RING_MS, SAMPLE_RATE, SampleKind, Session, downmix_f32, downmix_i16,
    };
    use crate::MediaError;
    use crate::audio::resample::MonoResampler;

    const WAVE_FORMAT_PCM: u16 = 1;
    const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
    const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
    const SUBTYPE_PCM: windows::core::GUID =
        windows::core::GUID::from_u128(0x0000_0001_0000_0010_8000_00aa_0038_9b71);
    const SUBTYPE_FLOAT: windows::core::GUID =
        windows::core::GUID::from_u128(0x0000_0003_0000_0010_8000_00aa_0038_9b71);

    #[implement(IActivateAudioInterfaceCompletionHandler)]
    struct ActivateHandler {
        tx: std::sync::mpsc::SyncSender<windows::core::Result<IAudioClient>>,
    }

    impl IActivateAudioInterfaceCompletionHandler_Impl for ActivateHandler_Impl {
        fn ActivateCompleted(
            &self,
            operation: windows::core::Ref<'_, IActivateAudioInterfaceAsyncOperation>,
        ) -> windows::core::Result<()> {
            let result = (|| -> windows::core::Result<IAudioClient> {
                let op = operation.ok()?;
                let mut hr = HRESULT(0);
                let mut unknown = None;
                unsafe { op.GetActivateResult(&mut hr, &mut unknown)? };
                hr.ok()?;
                unknown
                    .ok_or_else(|| {
                        windows::core::Error::from(windows::Win32::Foundation::E_NOINTERFACE)
                    })?
                    .cast()
            })();
            let _ = self.tx.send(result);
            Ok(())
        }
    }

    pub(super) fn open_session() -> Result<Session, MediaError> {
        let client = activate_client()?;
        let format = unsafe { client.GetMixFormat() }
            .map_err(|e| MediaError::Audio(format!("system audio mix format: {e}")))?;
        let format = FormatGuard(format);
        let parsed = parse_format(format.0)?;
        unsafe {
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK,
                    0,
                    0,
                    format.0,
                    None,
                )
                .map_err(|e| MediaError::Audio(format!("system audio init: {e}")))?;
        }
        drop(format);
        let capture = unsafe { client.GetService::<IAudioCaptureClient>() }
            .map_err(|e| MediaError::Audio(format!("system audio capture client: {e}")))?;
        unsafe { client.Start() }
            .map_err(|e| MediaError::Audio(format!("system audio start: {e}")))?;

        let capacity = (SAMPLE_RATE * RING_MS / 1000) as usize;
        let ring = HeapRb::<f32>::new(capacity.max(FRAME_SAMPLES * 4));
        let (prod, cons) = ring.split();
        let (resampler, chunk_buf) = if parsed.rate == SAMPLE_RATE {
            (None, Vec::new())
        } else {
            let chunk = (parsed.rate / 100).max(1) as usize;
            let resampler = MonoResampler::new(parsed.rate, SAMPLE_RATE, chunk)?;
            (Some(resampler), vec![0f32; chunk])
        };
        tracing::info!(
            rate = parsed.rate,
            channels = parsed.channels,
            "system audio capture started"
        );
        Ok(Session {
            cons: Some(cons),
            pump: Pump {
                prod,
                rate: parsed.rate,
                channels: parsed.channels,
                kind: parsed.kind,
                resampler,
                pending: Vec::new(),
                chunk_buf,
                client,
                capture,
            },
        })
    }

    struct FormatGuard(*mut WAVEFORMATEX);

    impl Drop for FormatGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CoTaskMemFree(Some(self.0.cast())) };
            }
        }
    }

    struct ParsedFormat {
        rate: u32,
        channels: usize,
        kind: SampleKind,
    }

    fn parse_format(format: *const WAVEFORMATEX) -> Result<ParsedFormat, MediaError> {
        if format.is_null() {
            return Err(MediaError::Audio("system audio format was empty".into()));
        }
        let header = unsafe { std::ptr::read_unaligned(format) };
        let channels = header.nChannels as usize;
        let rate = header.nSamplesPerSec;
        let bits = header.wBitsPerSample;
        let block = header.nBlockAlign;
        if channels == 0 || rate == 0 {
            return Err(MediaError::Audio(
                "system audio format has no channels or sample rate".into(),
            ));
        }
        let tag = header.wFormatTag;
        let subformat = if tag == WAVE_FORMAT_EXTENSIBLE {
            if header.cbSize < 22 {
                return Err(MediaError::Audio(
                    "system audio extensible format is truncated".into(),
                ));
            }
            let ext = unsafe { std::ptr::read_unaligned(format.cast::<WAVEFORMATEXTENSIBLE>()) };
            Some(unsafe { std::ptr::addr_of!(ext.SubFormat).read_unaligned() })
        } else {
            None
        };
        let kind = match (tag, subformat) {
            (WAVE_FORMAT_PCM, _) if bits == 16 => SampleKind::I16,
            (WAVE_FORMAT_IEEE_FLOAT, _) if bits == 32 => SampleKind::F32,
            (WAVE_FORMAT_EXTENSIBLE, Some(sub)) if sub == SUBTYPE_PCM && bits == 16 => {
                SampleKind::I16
            }
            (WAVE_FORMAT_EXTENSIBLE, Some(sub)) if sub == SUBTYPE_FLOAT && bits == 32 => {
                SampleKind::F32
            }
            _ => {
                return Err(MediaError::Audio(format!(
                    "unsupported system audio format tag {tag:#x}, {bits} bits"
                )));
            }
        };
        let expect = match kind {
            SampleKind::F32 => channels * 4,
            SampleKind::I16 => channels * 2,
        };
        if usize::from(block) != expect {
            return Err(MediaError::Audio(format!(
                "system audio block align {block} does not match {expect}"
            )));
        }
        Ok(ParsedFormat {
            rate,
            channels,
            kind,
        })
    }

    fn activate_client() -> Result<IAudioClient, MediaError> {
        let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: std::process::id(),
                    ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
                },
            },
        };
        let prop = blob_prop(&mut params);
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let handler: IActivateAudioInterfaceCompletionHandler = ActivateHandler { tx }.into();
        unsafe {
            ActivateAudioInterfaceAsync(
                VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
                &IAudioClient::IID,
                Some(&prop),
                &handler,
            )
        }
        .map_err(|e| MediaError::Audio(format!("system audio activation: {e}")))?;
        // `params` must stay alive until activation finishes reading the blob.
        match rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(client)) => Ok(client),
            Ok(Err(e)) => Err(MediaError::Audio(format!(
                "system audio activation failed: {e}"
            ))),
            Err(_) => Err(MediaError::Audio(
                "system audio activation timed out".into(),
            )),
        }
    }

    fn blob_prop(params: *mut AUDIOCLIENT_ACTIVATION_PARAMS) -> PROPVARIANT {
        let mut prop = PROPVARIANT::default();
        unsafe {
            let inner = &mut *std::ptr::addr_of_mut!(prop).cast::<PropHeader>();
            inner.vt = VT_BLOB;
            inner.blob = BLOB {
                cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                pBlobData: params.cast(),
            };
        }
        prop
    }

    /// Layout of the start of a `PROPVARIANT`: `vt` then padding, then the blob.
    #[repr(C)]
    struct PropHeader {
        vt: windows::Win32::System::Variant::VARENUM,
        reserved1: u16,
        reserved2: u16,
        reserved3: u16,
        blob: BLOB,
    }

    impl Pump {
        pub(super) fn run(&mut self, stop: &AtomicBool) -> Result<(), MediaError> {
            let mut mono = Vec::new();
            while !stop.load(Ordering::Acquire) {
                let mut progressed = false;
                loop {
                    let packet = unsafe { self.capture.GetNextPacketSize() }
                        .map_err(|e| MediaError::Audio(format!("system audio packet size: {e}")))?;
                    if packet == 0 {
                        break;
                    }
                    progressed = true;
                    self.read_packet(&mut mono)?;
                }
                if !progressed {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            Ok(())
        }

        fn read_packet(&mut self, mono: &mut Vec<f32>) -> Result<(), MediaError> {
            let mut data = std::ptr::null_mut();
            let mut got = 0u32;
            let mut flags = 0u32;
            unsafe {
                self.capture
                    .GetBuffer(&mut data, &mut got, &mut flags, None, None)
                    .map_err(|e| MediaError::Audio(format!("system audio read: {e}")))?;
            }
            let frames = (got as usize).min(self.rate as usize);
            let silent = flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
            if silent || data.is_null() || frames == 0 {
                self.push_silence(frames);
            } else {
                let samples = frames * self.channels;
                match self.kind {
                    SampleKind::F32 => {
                        let interleaved =
                            unsafe { std::slice::from_raw_parts(data.cast::<f32>(), samples) };
                        downmix_f32(interleaved, self.channels, mono);
                    }
                    SampleKind::I16 => {
                        let interleaved =
                            unsafe { std::slice::from_raw_parts(data.cast::<i16>(), samples) };
                        downmix_i16(interleaved, self.channels, mono);
                    }
                }
                self.emit(mono);
            }
            unsafe { self.capture.ReleaseBuffer(got) }
                .map_err(|e| MediaError::Audio(format!("system audio release: {e}")))?;
            Ok(())
        }
    }
}
