//! Webcams and capture cards through the Media Foundation source reader.
//!
//! The reader decodes whatever the device sends (MJPEG, YUY2, NV12…) to RGB32 at the selected mode;
//! each frame is uploaded into this source's texture. Webcam resolutions are small, so the upload
//! costs far less than a capture copy.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::thread::JoinHandle;

use fluxcapture_time::Hns;
use windows::Win32::Graphics::Direct3D11::{D3D11_BIND_SHADER_RESOURCE, D3D11_RESOURCE_MISC_FLAG, ID3D11ShaderResourceView, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};
use windows::core::{HRESULT, Interface, PCSTR, w};

use super::super::d3d::Gpu;
use super::super::mf::{self, attr_string, pack};
use super::super::{Context, ensure_com};
use super::compositor::srv;
use super::{CaptureSource, CapturedFrame};
use crate::clock::now;
use crate::{PlatformError, SourceState};

type EnumDevicesFn = unsafe extern "system" fn(*mut c_void, *mut *mut *mut c_void, *mut u32) -> HRESULT;
type CreateReaderFn = unsafe extern "system" fn(*mut c_void, *mut c_void, *mut *mut c_void) -> HRESULT;

struct Api {
    enum_devices: EnumDevicesFn,
    create_reader: CreateReaderFn,
}

fn api() -> crate::Result<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| {
        // SAFETY: system DLLs loaded from System32; symbols resolved with their documented signatures.
        unsafe {
            let mfdll = LoadLibraryExW(w!("mf.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32).ok()?;
            let rw = LoadLibraryExW(w!("mfreadwrite.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32).ok()?;
            let e = GetProcAddress(mfdll, PCSTR(c"MFEnumDeviceSources".as_ptr().cast()))?;
            let c = GetProcAddress(rw, PCSTR(c"MFCreateSourceReaderFromMediaSource".as_ptr().cast()))?;
            Some(Api {
                enum_devices: std::mem::transmute::<unsafe extern "system" fn() -> isize, EnumDevicesFn>(e),
                create_reader: std::mem::transmute::<unsafe extern "system" fn() -> isize, CreateReaderFn>(c),
            })
        }
    })
    .as_ref()
    .ok_or_else(|| PlatformError("webcam support needs Media Foundation (mf.dll / mfreadwrite.dll)".into()))
}

fn devices() -> crate::Result<Vec<IMFActivate>> {
    ensure_com()?;
    let mfapi = mf::api()?;
    let api = api()?;
    let attrs = mfapi.attributes(1)?;
    // SAFETY: attribute write, then enumeration into live out-locals; the returned array is freed
    // after taking ownership of each entry.
    unsafe {
        attrs.SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID).ctx("device attributes")?;
        let (mut list, mut count) = (std::ptr::null_mut::<*mut c_void>(), 0u32);
        let hr = (api.enum_devices)(attrs.as_raw(), &mut list, &mut count);
        if hr.is_err() {
            return Err(PlatformError(format!("cannot list webcams (0x{:08X})", hr.0 as u32)));
        }
        let mut out = Vec::new();
        if !list.is_null() {
            for i in 0..count.min(64) as usize {
                let p = *list.add(i);
                if !p.is_null() {
                    out.push(IMFActivate::from_raw(p));
                }
            }
            CoTaskMemFree(Some(list as *const c_void));
        }
        Ok(out)
    }
}

/// (id, name) of every video capture device.
pub fn list() -> crate::Result<Vec<(String, String)>> {
    Ok(devices()?
        .iter()
        .map(|a| {
            let id = attr_string(a, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK).unwrap_or_default();
            let name = attr_string(a, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME).unwrap_or_else(|| "Webcam".into());
            (id, name)
        })
        .collect())
}

struct Slot {
    texture: Option<ID3D11Texture2D>,
    view: Option<ID3D11ShaderResourceView>,
    width: u32,
    height: u32,
    seq: u64,
}

struct Shared {
    slot: Mutex<Slot>,
    state: Mutex<SourceState>,
    delivered: AtomicU64,
    last_time: AtomicI64,
    stop: AtomicBool,
}

pub struct WebcamSource {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl WebcamSource {
    pub fn start(gpu: &Gpu, device: &str, width: u32, height: u32, fps: u32) -> crate::Result<WebcamSource> {
        let shared = Arc::new(Shared {
            slot: Mutex::new(Slot { texture: None, view: None, width: 0, height: 0, seq: 0 }),
            state: Mutex::new(SourceState::Starting),
            delivered: AtomicU64::new(0),
            last_time: AtomicI64::new(0),
            stop: AtomicBool::new(false),
        });
        let (gpu2, dev, shared2) = (gpu.clone(), device.to_string(), Arc::clone(&shared));
        let thread = std::thread::Builder::new()
            .name("fluxcapture-webcam".into())
            .spawn(move || {
                let r = crate::guarded("the webcam", || run(&gpu2, &dev, width, height, fps, &shared2));
                if let Err(e) = r {
                    *shared2.state.lock().unwrap_or_else(PoisonError::into_inner) = SourceState::Failed(e.0);
                }
            })
            .map_err(|e| PlatformError(format!("cannot start the webcam thread: {e}")))?;
        Ok(WebcamSource { shared, thread: Some(thread) })
    }
}

fn open_reader(device: &str, want_w: u32, want_h: u32, want_fps: u32) -> crate::Result<(IMFSourceReader, IMFMediaSource, u32, u32)> {
    let mfapi = mf::api()?;
    let api = api()?;
    let list = devices()?;
    let activate = list
        .iter()
        .find(|a| device.is_empty() || attr_string(a, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK).as_deref() == Some(device))
        .ok_or_else(|| PlatformError("the webcam is not connected".into()))?;
    // SAFETY: documented COM calls on live objects; out-pointers are live locals.
    unsafe {
        let source: IMFMediaSource = activate.ActivateObject().ctx("cannot open the webcam (in use by another app?)")?;
        let attrs = mfapi.attributes(1)?;
        attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1).ctx("reader attributes")?;
        let mut raw = std::ptr::null_mut();
        let hr = (api.create_reader)(source.as_raw(), attrs.as_raw(), &mut raw);
        if hr.is_err() || raw.is_null() {
            return Err(PlatformError(format!("cannot read the webcam (0x{:08X})", hr.0 as u32)));
        }
        let reader = IMFSourceReader::from_raw(raw);
        let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        // Pick the native mode closest to the request (size first, then frame rate).
        let mut best: Option<(i64, IMFMediaType, u32, u32)> = None;
        for i in 0..256 {
            let Ok(t) = reader.GetNativeMediaType(stream, i) else { break };
            let size = t.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
            let (w, h) = ((size >> 32) as u32, size as u32);
            let rate = t.GetUINT64(&MF_MT_FRAME_RATE).unwrap_or(0);
            let (rn, rd) = ((rate >> 32) as u32, (rate as u32).max(1));
            let fps = rn / rd;
            let score = (i64::from(w) - i64::from(want_w)).abs() + (i64::from(h) - i64::from(want_h)).abs() + 4 * (i64::from(fps) - i64::from(want_fps)).abs();
            if w > 0 && h > 0 && best.as_ref().is_none_or(|b| score < b.0) {
                best = Some((score, t, w, h));
            }
        }
        let (_, native, w, h) = best.ok_or_else(|| PlatformError("the webcam reports no video modes".into()))?;
        reader.SetCurrentMediaType(stream, None, &native).ctx("cannot select the webcam mode")?;
        let out = mfapi.media_type()?;
        out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).ctx("output type")?;
        out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32).ctx("output type")?;
        out.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h)).ctx("output type")?;
        reader.SetCurrentMediaType(stream, None, &out).ctx("the webcam mode cannot be converted to RGB")?;
        Ok((reader, source, w, h))
    }
}

fn run(gpu: &Gpu, device: &str, want_w: u32, want_h: u32, want_fps: u32, shared: &Shared) -> crate::Result<()> {
    ensure_com()?;
    let (reader, source, w, h) = open_reader(device, want_w.max(160), want_h.max(120), want_fps.clamp(5, 120))?;
    let tex = gpu.texture(w, h, DXGI_FORMAT_B8G8R8A8_UNORM, D3D11_BIND_SHADER_RESOURCE, D3D11_RESOURCE_MISC_FLAG(0), 1)?;
    let view = srv(gpu, &tex)?;
    let row = w as usize * 4;
    let mut pixels = vec![0u8; row * h as usize];
    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    while !shared.stop.load(Ordering::Relaxed) {
        let (mut flags, mut sample) = (0u32, None::<IMFSample>);
        // SAFETY: synchronous read into live out-locals.
        unsafe { reader.ReadSample(stream, 0, None, Some(&mut flags), None, Some(&mut sample)) }.ctx("webcam read failed")?;
        if (flags & (MF_SOURCE_READERF_ERROR.0 | MF_SOURCE_READERF_ENDOFSTREAM.0) as u32) != 0 {
            return Err(PlatformError("the webcam stopped (disconnected?)".into()));
        }
        let Some(sample) = sample else { continue };
        // SAFETY: the buffer is locked while copying rows that lie inside it: Lock2D gives the first
        // scanline and a (possibly negative) pitch; every row read is `row` bytes from a scanline
        // start inside the locked buffer of h rows.
        unsafe {
            let buf = sample.ConvertToContiguousBuffer().ctx("webcam buffer")?;
            if let Ok(b2) = buf.cast::<IMF2DBuffer>() {
                let (mut scan0, mut pitch) = (std::ptr::null_mut::<u8>(), 0i32);
                if b2.Lock2D(&mut scan0, &mut pitch).is_ok() {
                    if !scan0.is_null() && pitch.unsigned_abs() as usize >= row {
                        for y in 0..h as usize {
                            let src = scan0.offset(pitch as isize * y as isize);
                            if let Some(dst) = pixels.get_mut(y * row..(y + 1) * row) {
                                std::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), row);
                            }
                        }
                    }
                    let _ = b2.Unlock2D();
                }
            } else {
                let (mut p, mut len) = (std::ptr::null_mut::<u8>(), 0u32);
                if buf.Lock(&mut p, None, Some(&mut len)).is_ok() {
                    if !p.is_null() && len as usize >= pixels.len() {
                        std::ptr::copy_nonoverlapping(p, pixels.as_mut_ptr(), pixels.len());
                    }
                    let _ = buf.Unlock();
                }
            }
        }
        // RGB32 has an undefined alpha byte: force opaque.
        for a in pixels.iter_mut().skip(3).step_by(4) {
            *a = 255;
        }
        let mut slot = shared.slot.lock().unwrap_or_else(PoisonError::into_inner);
        // SAFETY: `pixels` holds exactly w*h*4 bytes with pitch w*4 for a w×h BGRA texture.
        unsafe { gpu.context.UpdateSubresource(&tex, 0, None, pixels.as_ptr().cast(), w * 4, 0) };
        slot.texture = Some(tex.clone());
        slot.view = Some(view.clone());
        slot.width = w;
        slot.height = h;
        slot.seq = slot.seq.wrapping_add(1);
        drop(slot);
        shared.delivered.fetch_add(1, Ordering::Relaxed);
        shared.last_time.store(now().0, Ordering::Relaxed);
        *shared.state.lock().unwrap_or_else(PoisonError::into_inner) = SourceState::Active { width: w, height: h };
    }
    // SAFETY: releases the device for other apps.
    let _ = unsafe { source.Shutdown() };
    Ok(())
}

impl CaptureSource for WebcamSource {
    fn frame(&self) -> Option<CapturedFrame> {
        let slot = self.shared.slot.lock().unwrap_or_else(PoisonError::into_inner);
        Some(CapturedFrame { view: slot.view.clone()?, width: slot.width, height: slot.height, seq: slot.seq, mip_levels: 1, cursor: None })
    }

    fn state(&self) -> SourceState {
        self.shared.state.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn set_want_mips(&self, _want: bool) {}

    fn delivered(&self) -> u64 {
        self.shared.delivered.load(Ordering::Relaxed)
    }

    fn last_frame_time(&self) -> Hns {
        Hns(self.shared.last_time.load(Ordering::Relaxed))
    }
}

impl Drop for WebcamSource {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        // The reader thread wakes with the next frame (≤ one webcam frame interval).
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
