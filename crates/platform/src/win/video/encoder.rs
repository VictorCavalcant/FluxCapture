//! The Media Foundation video encoder (hardware MFTs: NVENC, AMF, Quick Sync; Microsoft's software
//! encoder as the fallback).
//!
//! Zero-copy path: the render thread converts the canvas into one of a small pool of NV12 textures
//! and hands its index to this thread, which wraps the texture in a *tracked* sample
//! (`MFCreateDXGISurfaceBuffer` + `MFCreateTrackedSample`). When the encoder releases the sample, a
//! callback returns the texture to the pool. Pixels never leave the GPU. A full pool means the
//! encoder is behind: the render thread counts that frame as dropped ("encoder backlog").
//!
//! Hardware MFTs are asynchronous: they ask for input (`METransformNeedInput`) and announce output
//! (`METransformHaveOutput`) through events. Software MFTs are synchronous and get NV12 copied to
//! system memory first.

use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TrySendError, bounded};
use fluxcapture_time::Hns;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_VIDEO_ENCODER, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_RESOURCE_MISC_FLAG, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Variant::VARIANT;
use windows::core::{GUID, Interface, implement};

use super::super::d3d::Gpu;
use super::super::mf::{self, MfApi, attr_string, pack};
use super::super::{Context, ensure_com};
use super::compositor::{Nv12Target, rtv};
use crate::clock::{ThreadKind, boost_thread};
use crate::{EncodedVideo, GpuVendor, PlatformError, RateControl, VideoCodecKind, VideoEncoderConfig, VideoEncoderInfo};

/// NV12 surfaces in the pool. Hardware encoders without B-frames hold one or two; the rest absorb
/// short GPU hiccups without dropping frames.
pub const SLOTS: usize = 6;

enum Msg {
    Frame { slot: usize, pts: Hns, keyframe: bool },
    Stop,
}

/// Counters shared with the render thread.
#[derive(Default)]
pub struct EncShared {
    pub submitted: AtomicU64,
    pub encoded: AtomicU64,
    pub bytes: AtomicU64,
    pub in_flight: AtomicU32,
    pub latency_us_sum: AtomicU64,
    pub latency_n: AtomicU64,
    pub error: Mutex<Option<String>>,
}

/// A running encoder thread and its surface pool.
pub struct EncoderHandle {
    pub info: VideoEncoderInfo,
    pub slots: Vec<Nv12Target>,
    pub shared: Arc<EncShared>,
    pub full_range: bool,
    free_rx: Receiver<usize>,
    tx: Sender<Msg>,
    thread: Option<JoinHandle<()>>,
}

impl EncoderHandle {
    /// Creates the surface pool and the encoder (on its own thread); returns once the encoder is
    /// configured or has failed.
    pub fn start(gpu: &Gpu, cfg: &VideoEncoderConfig, sink: Sender<EncodedVideo>) -> crate::Result<EncoderHandle> {
        if cfg.width < 16 || cfg.height < 16 || cfg.width > 8192 || cfg.height > 8192 || !cfg.width.is_multiple_of(2) || !cfg.height.is_multiple_of(2) {
            return Err(PlatformError(format!("unsupported output size {}x{} (even sizes from 16 to 8192)", cfg.width, cfg.height)));
        }
        let mut slots = Vec::with_capacity(SLOTS);
        for _ in 0..SLOTS {
            let texture = gpu
                .texture(cfg.width, cfg.height, DXGI_FORMAT_NV12, D3D11_BIND_RENDER_TARGET | D3D11_BIND_VIDEO_ENCODER, D3D11_RESOURCE_MISC_FLAG(0), 1)
                .or_else(|_| gpu.texture(cfg.width, cfg.height, DXGI_FORMAT_NV12, D3D11_BIND_RENDER_TARGET, D3D11_RESOURCE_MISC_FLAG(0), 1))
                .map_err(|e| PlatformError(format!("this GPU cannot render NV12 surfaces: {e}")))?;
            let y = rtv(gpu, &texture, Some(DXGI_FORMAT_R8_UNORM))?;
            let uv = rtv(gpu, &texture, Some(DXGI_FORMAT_R8G8_UNORM))?;
            slots.push(Nv12Target { texture, y, uv, width: cfg.width, height: cfg.height });
        }
        let (free_tx, free_rx) = bounded(SLOTS);
        for i in 0..SLOTS {
            let _ = free_tx.send(i);
        }
        let (tx, rx) = bounded::<Msg>(SLOTS + 2);
        let (init_tx, init_rx) = bounded::<crate::Result<VideoEncoderInfo>>(1);
        let shared = Arc::new(EncShared::default());
        let textures: Vec<ID3D11Texture2D> = slots.iter().map(|s| s.texture.clone()).collect();
        let (gpu2, cfg2, shared2) = (gpu.clone(), cfg.clone(), Arc::clone(&shared));
        let thread = std::thread::Builder::new()
            .name("fluxcapture-encoder".into())
            .spawn(move || {
                let r = crate::guarded("the video encoder", || encoder_thread(&gpu2, &cfg2, &textures, sink, rx, free_tx, &shared2, &init_tx));
                if let Err(e) = r {
                    log::error!("video encoder stopped: {e}");
                    let _ = init_tx.try_send(Err(e.clone()));
                    *shared2.error.lock().unwrap_or_else(PoisonError::into_inner) = Some(e.0);
                }
            })
            .map_err(|e| PlatformError(format!("cannot start the encoder thread: {e}")))?;
        let info = match init_rx.recv_timeout(Duration::from_secs(15)) {
            Ok(Ok(info)) => info,
            Ok(Err(e)) => {
                let _ = thread.join();
                return Err(e);
            }
            Err(_) => return Err(PlatformError("the encoder did not start within 15 s".into())),
        };
        Ok(EncoderHandle { info, slots, shared, full_range: cfg.full_range, free_rx, tx, thread: Some(thread) })
    }

    /// A free surface, or `None` when the encoder is behind.
    pub fn try_slot(&self) -> Option<usize> {
        self.free_rx.try_recv().ok()
    }

    /// Hands a converted surface to the encoder. `false` if the encoder is gone.
    pub fn submit(&self, slot: usize, pts: Hns, keyframe: bool) -> bool {
        match self.tx.try_send(Msg::Frame { slot, pts, keyframe }) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => false,
            Err(TrySendError::Disconnected(_)) => false,
        }
    }

    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Drains the encoder (every submitted frame comes out) and stops the thread.
    pub fn stop(mut self) {
        let _ = self.tx.send_timeout(Msg::Stop, Duration::from_secs(2));
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for EncoderHandle {
    fn drop(&mut self) {
        if let Some(t) = self.thread.take() {
            let _ = self.tx.send_timeout(Msg::Stop, Duration::from_secs(2));
            let _ = t.join();
        }
    }
}

/// Returns a pool surface when the encoder releases the tracked sample wrapping it.
#[implement(IMFAsyncCallback)]
struct SlotReturn {
    slot: usize,
    tx: Sender<usize>,
}

impl IMFAsyncCallback_Impl for SlotReturn_Impl {
    fn GetParameters(&self, _flags: *mut u32, _queue: *mut u32) -> windows::core::Result<()> {
        Err(windows::Win32::Foundation::E_NOTIMPL.into())
    }

    fn Invoke(&self, _result: windows::core::Ref<IMFAsyncResult>) -> windows::core::Result<()> {
        let _ = self.tx.try_send(self.slot);
        Ok(())
    }
}

fn subtype(codec: VideoCodecKind) -> GUID {
    match codec {
        VideoCodecKind::H264 => MFVideoFormat_H264,
        VideoCodecKind::Hevc => MFVideoFormat_HEVC,
        VideoCodecKind::Av1 => MFVideoFormat_AV1,
    }
}

fn activate_info(a: &IMFActivate, codec: VideoCodecKind, hardware: bool) -> VideoEncoderInfo {
    // SAFETY: plain attribute reads on a live activation object.
    let clsid = unsafe { a.GetGUID(&MFT_TRANSFORM_CLSID_Attribute) }.unwrap_or_default();
    let name = attr_string(a, &MFT_FRIENDLY_NAME_Attribute).unwrap_or_else(|| "Encoder".into());
    let vendor = attr_string(a, &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute)
        .and_then(|v| u32::from_str_radix(v.trim_start_matches("VEN_"), 16).ok())
        .map(GpuVendor::from_pci);
    let mut luid = [0u8; 8];
    let mut got = 0u32;
    // SAFETY: reads at most 8 bytes into `luid`.
    let gpu_luid = (unsafe { a.GetBlob(&MFT_ENUM_ADAPTER_LUID, &mut luid, Some(&mut got)) }.is_ok() && got == 8).then(|| u64::from_le_bytes(luid));
    VideoEncoderInfo { id: format!("mft:{clsid:?}"), name, codec, hardware, vendor, gpu_luid }
}

/// Every encoder for `codec`: hardware first (all adapters), then software.
pub fn list(codec: VideoCodecKind) -> crate::Result<Vec<VideoEncoderInfo>> {
    let api = mf::api()?;
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: subtype(codec) };
    let mut out = Vec::new();
    let hw_flags = (MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0) as u32;
    for a in api.enum_transforms(MFT_CATEGORY_VIDEO_ENCODER, hw_flags, None, Some(output), None)? {
        out.push(activate_info(&a, codec, true));
    }
    let sw_flags = (MFT_ENUM_FLAG_SYNCMFT.0 | MFT_ENUM_FLAG_SORTANDFILTER.0) as u32;
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
    for a in api.enum_transforms(MFT_CATEGORY_VIDEO_ENCODER, sw_flags, Some(input), Some(output), None)? {
        out.push(activate_info(&a, codec, false));
    }
    Ok(out)
}

struct Mft {
    transform: IMFTransform,
    activate: IMFActivate,
    events: Option<IMFMediaEventGenerator>,
    codec_api: Option<ICodecAPI>,
    provides_samples: bool,
    out_size: u32,
    d3d: bool,
    info: VideoEncoderInfo,
    seq_header_done: bool,
    codec: VideoCodecKind,
    _manager: Option<IMFDXGIDeviceManager>,
}

fn set_codec(api: &ICodecAPI, key: &GUID, v: VARIANT) -> bool {
    // SAFETY: `v` lives across the call; the encoder copies the value.
    unsafe { api.SetValue(key, &v) }.is_ok()
}

fn apply_codec_settings(api: &ICodecAPI, cfg: &VideoEncoderConfig) {
    match cfg.rate {
        RateControl::Cbr { kbps } => {
            set_codec(api, &CODECAPI_AVEncCommonRateControlMode, VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32));
            set_codec(api, &CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(kbps.saturating_mul(1000)));
        }
        RateControl::Vbr { kbps, max_kbps } => {
            set_codec(api, &CODECAPI_AVEncCommonRateControlMode, VARIANT::from(eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32));
            set_codec(api, &CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(kbps.saturating_mul(1000)));
            set_codec(api, &CODECAPI_AVEncCommonMaxBitRate, VARIANT::from(max_kbps.max(kbps).saturating_mul(1000)));
        }
        RateControl::Quality { quality } => {
            set_codec(api, &CODECAPI_AVEncCommonRateControlMode, VARIANT::from(eAVEncCommonRateControlMode_Quality.0 as u32));
            set_codec(api, &CODECAPI_AVEncCommonQuality, VARIANT::from(u32::from(quality.clamp(1, 100))));
        }
    }
    set_codec(api, &CODECAPI_AVEncCommonQualityVsSpeed, VARIANT::from(u32::from(cfg.speed_quality.min(100))));
    let gop = (cfg.fps.as_f64() * f64::from(cfg.keyframe_interval_s.clamp(0.5, 10.0))).round().max(1.0) as u32;
    set_codec(api, &CODECAPI_AVEncMPVGOPSize, VARIANT::from(gop));
    set_codec(api, &CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0u32));
    set_codec(api, &CODECAPI_AVLowLatencyMode, VARIANT::from(cfg.low_latency));
    if cfg.codec == VideoCodecKind::H264 {
        set_codec(api, &CODECAPI_AVEncH264CABACEnable, VARIANT::from(true));
    }
}

fn set_color_attrs(t: &IMFMediaType, full_range: bool) {
    // SAFETY: plain attribute writes on a live media type; failures are harmless (VUI hints only).
    unsafe {
        let range = if full_range { MFNominalRange_0_255 } else { MFNominalRange_16_235 };
        let _ = t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, range.0 as u32);
        let _ = t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32);
        let _ = t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32);
        let _ = t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32);
    }
}

impl Mft {
    fn open(gpu: &Gpu, cfg: &VideoEncoderConfig) -> crate::Result<Mft> {
        let api = mf::api()?;
        let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: subtype(cfg.codec) };
        let mut candidates: Vec<(IMFActivate, bool)> = Vec::new();
        let hw_flags = (MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0) as u32;
        // Hardware encoders of *our* adapter only: a texture of one GPU cannot feed another's encoder.
        let hw = api.enum_transforms(MFT_CATEGORY_VIDEO_ENCODER, hw_flags, None, Some(output), Some(gpu.luid)).unwrap_or_default();
        for a in hw {
            let info = activate_info(&a, cfg.codec, true);
            if info.gpu_luid.is_none_or(|l| l == gpu.luid) {
                candidates.push((a, true));
            }
        }
        let sw_flags = (MFT_ENUM_FLAG_SYNCMFT.0 | MFT_ENUM_FLAG_SORTANDFILTER.0) as u32;
        let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
        for a in api.enum_transforms(MFT_CATEGORY_VIDEO_ENCODER, sw_flags, Some(input), Some(output), None).unwrap_or_default() {
            candidates.push((a, false));
        }
        if let Some(id) = &cfg.encoder_id {
            candidates.retain(|(a, hw)| activate_info(a, cfg.codec, *hw).id == *id);
            if candidates.is_empty() {
                return Err(PlatformError(format!("the selected encoder is not available on {} (pick another encoder or GPU)", gpu.name)));
            }
        }
        if candidates.is_empty() {
            return Err(PlatformError(format!("no {} encoder is installed", cfg.codec.label())));
        }
        let mut last = PlatformError(String::new());
        for (activate, hardware) in candidates {
            let info = activate_info(&activate, cfg.codec, hardware);
            match Mft::configure(api, gpu, cfg, &activate, info.clone()) {
                Ok(m) => return Ok(m),
                Err(e) => {
                    log::warn!("encoder {} refused the configuration: {e}", info.name);
                    // SAFETY: releases whatever the failed activation created.
                    let _ = unsafe { activate.ShutdownObject() };
                    last = PlatformError(format!("{}: {e}", info.name));
                }
            }
        }
        Err(last)
    }

    fn configure(api: &MfApi, gpu: &Gpu, cfg: &VideoEncoderConfig, activate: &IMFActivate, info: VideoEncoderInfo) -> crate::Result<Mft> {
        // SAFETY: every call below is a documented COM call on live objects created here; media
        // types and attribute values are plain data.
        unsafe {
            let transform: IMFTransform = activate.ActivateObject().ctx("cannot create the encoder")?;
            let attrs = transform.GetAttributes().ok();
            let is_async = attrs.as_ref().is_some_and(|a| a.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) == 1);
            if is_async && let Some(a) = &attrs {
                a.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1).ctx("MF_TRANSFORM_ASYNC_UNLOCK")?;
            }
            let d3d_aware = attrs.as_ref().is_some_and(|a| a.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) == 1);
            let mut manager = None;
            if d3d_aware {
                let m = api.device_manager(&gpu.device)?;
                transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, m.as_raw() as usize).ctx("the encoder cannot use this GPU")?;
                manager = Some(m);
            } else if info.hardware {
                return Err(PlatformError("hardware encoder without Direct3D 11 support".into()));
            }
            let codec_api = transform.cast::<ICodecAPI>().ok();
            if let Some(c) = &codec_api {
                apply_codec_settings(c, cfg);
            }
            let bps = cfg.rate.estimated_kbps(cfg.width, cfg.height, cfg.fps.as_f64()).saturating_mul(1000);
            let out = api.media_type()?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).ctx("output type")?;
            out.SetGUID(&MF_MT_SUBTYPE, &subtype(cfg.codec)).ctx("output type")?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, bps).ctx("output type")?;
            out.SetUINT64(&MF_MT_FRAME_SIZE, pack(cfg.width, cfg.height)).ctx("output type")?;
            out.SetUINT64(&MF_MT_FRAME_RATE, pack(cfg.fps.num(), cfg.fps.den())).ctx("output type")?;
            out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).ctx("output type")?;
            out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1)).ctx("output type")?;
            let profile = match cfg.codec {
                VideoCodecKind::H264 => eAVEncH264VProfile_High.0 as u32,
                VideoCodecKind::Hevc => eAVEncH265VProfile_Main_420_8.0 as u32,
                VideoCodecKind::Av1 => 0,
            };
            if cfg.codec != VideoCodecKind::Av1 {
                out.SetUINT32(&MF_MT_MPEG2_PROFILE, profile).ctx("output type")?;
            }
            set_color_attrs(&out, cfg.full_range);
            transform.SetOutputType(0, &out, 0).ctx("output format refused")?;

            let inp = api.media_type()?;
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).ctx("input type")?;
            inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).ctx("input type")?;
            inp.SetUINT64(&MF_MT_FRAME_SIZE, pack(cfg.width, cfg.height)).ctx("input type")?;
            inp.SetUINT64(&MF_MT_FRAME_RATE, pack(cfg.fps.num(), cfg.fps.den())).ctx("input type")?;
            inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).ctx("input type")?;
            inp.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1)).ctx("input type")?;
            set_color_attrs(&inp, cfg.full_range);
            if transform.SetInputType(0, &inp, 0).is_err() {
                // Some encoders only take types derived from their own list.
                let mut accepted = false;
                for i in 0..64 {
                    let Ok(t) = transform.GetInputAvailableType(0, i) else { break };
                    if t.GetGUID(&MF_MT_SUBTYPE).ok() != Some(MFVideoFormat_NV12) {
                        continue;
                    }
                    let _ = t.SetUINT64(&MF_MT_FRAME_SIZE, pack(cfg.width, cfg.height));
                    let _ = t.SetUINT64(&MF_MT_FRAME_RATE, pack(cfg.fps.num(), cfg.fps.den()));
                    set_color_attrs(&t, cfg.full_range);
                    if transform.SetInputType(0, &t, 0).is_ok() {
                        accepted = true;
                        break;
                    }
                }
                if !accepted {
                    return Err(PlatformError("the encoder does not accept NV12 input".into()));
                }
            }
            // Some drivers only honour codec settings once the types are set.
            if let Some(c) = &codec_api {
                apply_codec_settings(c, cfg);
            }
            let si = transform.GetOutputStreamInfo(0).ctx("GetOutputStreamInfo")?;
            let provides = (si.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32) != 0;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).ctx("begin streaming")?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).ctx("start of stream")?;
            let events = if is_async { Some(transform.cast::<IMFMediaEventGenerator>().ctx("async encoder without events")?) } else { None };
            log::info!("video encoder: {} ({}, async: {is_async}, d3d: {d3d_aware})", info.name, if info.hardware { "hardware" } else { "software" });
            Ok(Mft {
                transform,
                activate: activate.clone(),
                events,
                codec_api,
                provides_samples: provides,
                out_size: si.cbSize.max(cfg.width.saturating_mul(cfg.height)),
                d3d: d3d_aware,
                info,
                seq_header_done: false,
                codec: cfg.codec,
                _manager: manager,
            })
        }
    }

    /// `Ok(false)` when the encoder is full (MF_E_NOTACCEPTING).
    fn feed(&self, sample: &IMFSample, keyframe: bool) -> crate::Result<bool> {
        if keyframe {
            self.force_keyframe();
        }
        // SAFETY: hands the sample to the encoder, which keeps its own reference.
        match unsafe { self.transform.ProcessInput(0, sample, 0) } {
            Ok(()) => Ok(true),
            Err(e) if e.code() == MF_E_NOTACCEPTING => Ok(false),
            Err(e) => Err(PlatformError(format!("encoder input failed: {e}"))),
        }
    }

    fn force_keyframe(&self) {
        if let Some(c) = &self.codec_api {
            set_codec(c, &CODECAPI_AVEncVideoForceKeyFrame, VARIANT::from(1u32));
        }
    }

    /// One `ProcessOutput`; `Ok(None)` when the encoder needs more input.
    fn pull(&mut self, api: &MfApi) -> crate::Result<Option<EncodedVideo>> {
        for _ in 0..4 {
            let sample_in = if self.provides_samples {
                None
            } else {
                let s = api.sample()?;
                let b = api.memory_buffer(self.out_size)?;
                // SAFETY: plain COM call.
                unsafe { s.AddBuffer(&b) }.ctx("AddBuffer")?;
                Some(s)
            };
            let mut buf = [MFT_OUTPUT_DATA_BUFFER { dwStreamID: 0, pSample: ManuallyDrop::new(sample_in), dwStatus: 0, pEvents: ManuallyDrop::new(None) }];
            let mut status = 0u32;
            // SAFETY: one output buffer descriptor; the sample/events references it holds afterwards
            // are taken out of the ManuallyDrop wrappers below, so nothing leaks.
            let r = unsafe { self.transform.ProcessOutput(0, &mut buf, &mut status) };
            // SAFETY: each ManuallyDrop field is taken exactly once.
            let (sample, _events) = unsafe { (ManuallyDrop::take(&mut buf[0].pSample), ManuallyDrop::take(&mut buf[0].pEvents)) };
            match r {
                Ok(()) => {
                    let Some(sample) = sample else { return Ok(None) };
                    return self.convert(&sample).map(Some);
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // SAFETY: renegotiates with the encoder's own first proposal.
                    unsafe {
                        let t = self.transform.GetOutputAvailableType(0, 0).ctx("stream change")?;
                        self.transform.SetOutputType(0, &t, 0).ctx("stream change")?;
                        let si = self.transform.GetOutputStreamInfo(0).ctx("GetOutputStreamInfo")?;
                        self.provides_samples = (si.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32) != 0;
                        self.out_size = si.cbSize.max(self.out_size);
                    }
                }
                Err(e) => return Err(PlatformError(format!("encoder output failed: {e}"))),
            }
        }
        Ok(None)
    }

    fn convert(&mut self, sample: &IMFSample) -> crate::Result<EncodedVideo> {
        // SAFETY: reads on a live output sample; the buffer is locked only while copying `len`
        // bytes it reports as valid.
        let (pts, dts, clean, mut data) = unsafe {
            let pts = sample.GetSampleTime().unwrap_or(0);
            let dts = sample.GetUINT64(&MFSampleExtension_DecodeTimestamp).map(|v| v as i64).unwrap_or(pts);
            let clean = sample.GetUINT32(&MFSampleExtension_CleanPoint).ok().map(|v| v != 0);
            let buffer = sample.ConvertToContiguousBuffer().ctx("output buffer")?;
            let (mut ptr, mut len) = (std::ptr::null_mut::<u8>(), 0u32);
            buffer.Lock(&mut ptr, None, Some(&mut len)).ctx("lock output")?;
            let data = if ptr.is_null() { Vec::new() } else { std::slice::from_raw_parts(ptr, len as usize).to_vec() };
            let _ = buffer.Unlock();
            (pts, dts, clean, data)
        };
        let keyframe = clean.unwrap_or_else(|| has_idr(&data, self.codec));
        if keyframe && !self.seq_header_done {
            self.seq_header_done = true;
            if !has_parameter_sets(&data, self.codec) {
                // SAFETY: reads the sequence header blob of the current output type.
                let header = unsafe {
                    self.transform.GetOutputCurrentType(0).ok().and_then(|t| {
                        let n = t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER).ok()?;
                        if n == 0 || n > 65536 {
                            return None;
                        }
                        let mut v = vec![0u8; n as usize];
                        t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut v, None).ok()?;
                        Some(v)
                    })
                };
                if let Some(mut h) = header {
                    h.extend_from_slice(&data);
                    data = h;
                }
            }
        }
        Ok(EncodedVideo { pts: Hns(pts), dts: Hns(dts), keyframe, data })
    }

    fn shutdown(&self) {
        // SAFETY: end-of-life calls on live objects; errors are irrelevant at this point.
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Ok(s) = self.transform.cast::<IMFShutdown>() {
                let _ = s.Shutdown();
            }
            let _ = self.activate.ShutdownObject();
        }
    }
}

/// NAL unit types in an Annex B buffer.
fn nal_types(data: &[u8], codec: VideoCodecKind) -> impl Iterator<Item = u8> + '_ {
    let mut i = 0usize;
    std::iter::from_fn(move || {
        while i + 3 < data.len() {
            let found = data.get(i..i + 3) == Some(&[0, 0, 1]);
            i += 1;
            if found {
                let header = *data.get(i + 2)?;
                i += 2;
                return Some(match codec {
                    VideoCodecKind::H264 => header & 0x1F,
                    _ => (header >> 1) & 0x3F,
                });
            }
        }
        None
    })
}

fn has_idr(data: &[u8], codec: VideoCodecKind) -> bool {
    match codec {
        VideoCodecKind::H264 => nal_types(data, codec).any(|t| t == 5),
        VideoCodecKind::Hevc => nal_types(data, codec).any(|t| (16..=23).contains(&t)),
        VideoCodecKind::Av1 => false,
    }
}

fn has_parameter_sets(data: &[u8], codec: VideoCodecKind) -> bool {
    match codec {
        VideoCodecKind::H264 => nal_types(data, codec).any(|t| t == 7),
        VideoCodecKind::Hevc => nal_types(data, codec).any(|t| t == 33),
        VideoCodecKind::Av1 => true,
    }
}

/// Copies an NV12 texture into a system-memory sample (software encoders).
fn cpu_sample(api: &MfApi, gpu: &Gpu, src: &ID3D11Texture2D, staging: &ID3D11Texture2D, w: u32, h: u32) -> crate::Result<IMFSample> {
    let size = w.checked_mul(h).and_then(|p| p.checked_mul(3)).map(|v| v / 2).ok_or("frame too large")?;
    let buffer = api.memory_buffer(size)?;
    // SAFETY: the staging texture is NV12 of w×h (luma rows, then h/2 chroma rows at RowPitch*h);
    // every row copy stays inside both the mapped region (RowPitch ≥ w) and the destination buffer
    // (`size` bytes, locked until Unlock).
    unsafe {
        gpu.context.CopyResource(staging, src);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        gpu.context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).ctx("map NV12")?;
        let (mut dst, mut max) = (std::ptr::null_mut::<u8>(), 0u32);
        let locked = buffer.Lock(&mut dst, Some(&mut max), None);
        let pitch = mapped.RowPitch as usize;
        let ok = locked.is_ok() && !dst.is_null() && max >= size && !mapped.pData.is_null() && pitch >= w as usize;
        if ok {
            let src_base = mapped.pData.cast::<u8>();
            for row in 0..(h as usize + h as usize / 2) {
                std::ptr::copy_nonoverlapping(src_base.add(row * pitch), dst.add(row * w as usize), w as usize);
            }
        }
        if locked.is_ok() {
            let _ = buffer.Unlock();
        }
        gpu.context.Unmap(staging, 0);
        if !ok {
            return Err(PlatformError("cannot read the NV12 frame".into()));
        }
        buffer.SetCurrentLength(size).ctx("SetCurrentLength")?;
    }
    let sample = api.sample()?;
    // SAFETY: plain COM call.
    unsafe { sample.AddBuffer(&buffer) }.ctx("AddBuffer")?;
    Ok(sample)
}

fn gpu_sample(api: &MfApi, texture: &ID3D11Texture2D, slot: usize, free_tx: &Sender<usize>) -> crate::Result<IMFSample> {
    let buffer = api.surface_buffer(texture)?;
    // SAFETY: plain COM calls on objects created here.
    unsafe {
        if let Ok(b2) = buffer.cast::<IMF2DBuffer>()
            && let Ok(len) = b2.GetContiguousLength()
        {
            let _ = buffer.SetCurrentLength(len);
        }
        let tracked = api.tracked_sample()?;
        let sample: IMFSample = tracked.cast().ctx("IMFSample")?;
        sample.AddBuffer(&buffer).ctx("AddBuffer")?;
        let cb: IMFAsyncCallback = SlotReturn { slot, tx: free_tx.clone() }.into();
        tracked.SetAllocator(&cb, None).ctx("SetAllocator")?;
        Ok(sample)
    }
}

struct Pending {
    pts: Hns,
    at: Instant,
}

fn encoder_thread(
    gpu: &Gpu,
    cfg: &VideoEncoderConfig,
    textures: &[ID3D11Texture2D],
    sink: Sender<EncodedVideo>,
    rx: Receiver<Msg>,
    free_tx: Sender<usize>,
    shared: &EncShared,
    init_tx: &Sender<crate::Result<VideoEncoderInfo>>,
) -> crate::Result<()> {
    ensure_com()?;
    let _prio = boost_thread(ThreadKind::Encode);
    let api = mf::api()?;
    let mut mft = match Mft::open(gpu, cfg) {
        Ok(m) => m,
        Err(e) => {
            let _ = init_tx.send(Err(e));
            return Ok(());
        }
    };
    let staging: Vec<ID3D11Texture2D> = if mft.d3d {
        Vec::new()
    } else {
        (0..textures.len()).map(|_| gpu.staging_texture(cfg.width, cfg.height, DXGI_FORMAT_NV12)).collect::<crate::Result<_>>()?
    };
    let _ = init_tx.send(Ok(mft.info.clone()));
    let frame_dur = cfg.fps.frame_duration().0;
    let mft_d3d = mft.d3d;
    let mut pending: VecDeque<Pending> = VecDeque::new();
    let mut emit = |v: EncodedVideo, pending: &mut VecDeque<Pending>| -> bool {
        if let Some(pos) = pending.iter().position(|p| p.pts.0 == v.pts.0) {
            if let Some(p) = pending.remove(pos) {
                shared.latency_us_sum.fetch_add(p.at.elapsed().as_micros() as u64, Ordering::Relaxed);
                shared.latency_n.fetch_add(1, Ordering::Relaxed);
            }
        } else if pending.len() > 256 {
            pending.pop_front();
        }
        shared.encoded.fetch_add(1, Ordering::Relaxed);
        shared.bytes.fetch_add(v.data.len() as u64, Ordering::Relaxed);
        shared.in_flight.store(pending.len() as u32, Ordering::Relaxed);
        sink.send(v).is_ok()
    };

    let make = |slot: usize, pts: Hns| -> crate::Result<IMFSample> {
        let tex = textures.get(slot).ok_or("bad surface index")?;
        let sample = if mft_d3d {
            gpu_sample(api, tex, slot, &free_tx)?
        } else {
            let st = staging.get(slot).ok_or("bad surface index")?;
            let s = cpu_sample(api, gpu, tex, st, cfg.width, cfg.height);
            let _ = free_tx.try_send(slot);
            s?
        };
        // SAFETY: plain COM calls on a live sample.
        unsafe {
            sample.SetSampleTime(pts.0).ctx("SetSampleTime")?;
            sample.SetSampleDuration(frame_dur).ctx("SetSampleDuration")?;
        }
        Ok(sample)
    };

    let result = if let Some(events) = mft.events.clone() {
        run_async(&mut mft, api, &events, &rx, &mut pending, shared, &mut emit, &make)
    } else {
        run_sync(&mut mft, api, &rx, &mut pending, shared, &mut emit, &make)
    };
    mft.shutdown();
    result
}

type Emit<'a> = dyn FnMut(EncodedVideo, &mut VecDeque<Pending>) -> bool + 'a;
type Make<'a> = dyn Fn(usize, Hns) -> crate::Result<IMFSample> + 'a;

fn run_async(
    mft: &mut Mft,
    api: &MfApi,
    events: &IMFMediaEventGenerator,
    rx: &Receiver<Msg>,
    pending: &mut VecDeque<Pending>,
    shared: &EncShared,
    emit: &mut Emit<'_>,
    make: &Make<'_>,
) -> crate::Result<()> {
    let mut need_input = 0u32;
    let mut draining = false;
    let mut drain_started = Instant::now();
    let mut last_progress = Instant::now();
    loop {
        // Take every pending event without waiting.
        loop {
            // SAFETY: non-blocking event query on a live generator.
            match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(ev) => {
                    // SAFETY: plain getter.
                    let kind = unsafe { ev.GetType() }.unwrap_or(0);
                    if kind == METransformNeedInput.0 as u32 {
                        need_input = need_input.saturating_add(1);
                    } else if kind == METransformHaveOutput.0 as u32 {
                        if let Some(v) = mft.pull(api)? {
                            last_progress = Instant::now();
                            if !emit(v, pending) {
                                return Ok(());
                            }
                        }
                    } else if kind == METransformDrainComplete.0 as u32 {
                        return Ok(());
                    }
                }
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => break,
                Err(e) => return Err(PlatformError(format!("encoder event failed: {e}"))),
            }
        }
        if draining {
            if drain_started.elapsed() > Duration::from_secs(3) {
                log::warn!("encoder drain timed out");
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(1));
            continue;
        }
        if need_input == 0 {
            if !pending.is_empty() && last_progress.elapsed() > Duration::from_secs(10) {
                return Err(PlatformError(format!("the encoder ({}) stopped responding", mft.info.name)));
            }
            // The encoder is busy; its next event comes within a frame.
            std::thread::sleep(Duration::from_micros(500));
            continue;
        }
        match rx.recv_timeout(Duration::from_millis(4)) {
            Ok(Msg::Frame { slot, pts, keyframe }) => {
                pending.push_back(Pending { pts, at: Instant::now() });
                shared.submitted.fetch_add(1, Ordering::Relaxed);
                if pending.len() == 1 {
                    last_progress = Instant::now();
                }
                let sample = make(slot, pts)?;
                if !mft.feed(&sample, keyframe)? {
                    return Err(PlatformError("the encoder refused input it asked for".into()));
                }
                need_input -= 1;
            }
            Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => {
                // SAFETY: plain messages to a live transform.
                unsafe {
                    let _ = mft.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
                    mft.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0).ctx("drain")?;
                }
                draining = true;
                drain_started = Instant::now();
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

fn run_sync(
    mft: &mut Mft,
    api: &MfApi,
    rx: &Receiver<Msg>,
    pending: &mut VecDeque<Pending>,
    shared: &EncShared,
    emit: &mut Emit<'_>,
    make: &Make<'_>,
) -> crate::Result<()> {
    loop {
        match rx.recv() {
            Ok(Msg::Frame { slot, pts, keyframe }) => {
                pending.push_back(Pending { pts, at: Instant::now() });
                shared.submitted.fetch_add(1, Ordering::Relaxed);
                let sample = make(slot, pts)?;
                if !mft.feed(&sample, keyframe)? {
                    // Full: take its output first, then retry once with the same sample.
                    while let Some(v) = mft.pull(api)? {
                        if !emit(v, pending) {
                            return Ok(());
                        }
                    }
                    if !mft.feed(&sample, false)? {
                        return Err(PlatformError("the encoder does not accept input".into()));
                    }
                }
                drop(sample);
                while let Some(v) = mft.pull(api)? {
                    if !emit(v, pending) {
                        return Ok(());
                    }
                }
            }
            Ok(Msg::Stop) | Err(_) => {
                // SAFETY: plain message to a live transform.
                unsafe { mft.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0) }.ctx("drain")?;
                while let Some(v) = mft.pull(api)? {
                    if !emit(v, pending) {
                        break;
                    }
                }
                return Ok(());
            }
        }
    }
}
