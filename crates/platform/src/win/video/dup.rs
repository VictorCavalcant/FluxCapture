//! DXGI Desktop Duplication sources (monitors on the render GPU).
//!
//! Lowest-overhead path for full-screen content on the adapter that drives the monitor, and no
//! capture border on Windows 10. The pointer is not part of the image: its shape and position come
//! separately and the compositor draws it.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use fluxcapture_time::Hns;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_RESOURCE_MISC_FLAG, D3D11_RESOURCE_MISC_GENERATE_MIPS, ID3D11ShaderResourceView,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME, IDXGIDevice, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::Interface;

use super::super::d3d::Gpu;
use super::super::{Context, ensure_com, wide_to_string};
use super::compositor::srv;
use super::{CaptureSource, CapturedFrame};
use crate::clock::{PrecisionTimer, ThreadKind, boost_thread, now};
use crate::{CaptureOptions, CaptureTarget, PlatformError, SourceState};

struct Cursor {
    view: ID3D11ShaderResourceView,
    width: u32,
    height: u32,
}

struct Slot {
    texture: Option<ID3D11Texture2D>,
    view: Option<ID3D11ShaderResourceView>,
    width: u32,
    height: u32,
    mips: u32,
    seq: u64,
    cursor: Option<Cursor>,
    cursor_pos: (i32, i32),
    cursor_visible: bool,
}

struct Shared {
    slot: Mutex<Slot>,
    state: Mutex<SourceState>,
    want_mips: AtomicBool,
    delivered: AtomicU64,
    last_time: AtomicI64,
    stop: AtomicBool,
}

pub struct DupSource {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl DupSource {
    pub fn start(gpu: &Gpu, target: &CaptureTarget, options: &CaptureOptions) -> crate::Result<DupSource> {
        let CaptureTarget::Monitor { id } = target else {
            return Err(PlatformError("Desktop Duplication captures monitors only".into()));
        };
        // Fail fast (so "Auto" can fall back) when the monitor is not on this GPU.
        let output = find_output(gpu, id)?;
        drop(output);
        let shared = Arc::new(Shared {
            slot: Mutex::new(Slot { texture: None, view: None, width: 0, height: 0, mips: 1, seq: 0, cursor: None, cursor_pos: (0, 0), cursor_visible: false }),
            state: Mutex::new(SourceState::Starting),
            want_mips: AtomicBool::new(false),
            delivered: AtomicU64::new(0),
            last_time: AtomicI64::new(0),
            stop: AtomicBool::new(false),
        });
        let (gpu2, id2, shared2, cursor, max_fps) = (gpu.clone(), id.clone(), Arc::clone(&shared), options.cursor, options.max_fps);
        let thread = std::thread::Builder::new()
            .name("fluxcapture-dup".into())
            .spawn(move || {
                let r = crate::guarded("desktop duplication", || run(&gpu2, &id2, &shared2, cursor, max_fps));
                if let Err(e) = r {
                    *shared2.state.lock().unwrap_or_else(PoisonError::into_inner) = SourceState::Failed(e.0);
                }
            })
            .map_err(|e| PlatformError(format!("cannot start the duplication thread: {e}")))?;
        Ok(DupSource { shared, thread: Some(thread) })
    }
}

fn find_output(gpu: &Gpu, id: &str) -> crate::Result<IDXGIOutput1> {
    let dxgi: IDXGIDevice = gpu.device.cast().ctx("IDXGIDevice")?;
    // SAFETY: plain getters / enumeration on live DXGI objects.
    unsafe {
        let adapter = dxgi.GetAdapter().ctx("GetAdapter")?;
        let primary = super::super::display::list_monitors().ok().and_then(|m| m.into_iter().find(|m| m.primary)).map(|m| m.id);
        let want = if id.is_empty() { primary.unwrap_or_default() } else { id.to_string() };
        for i in 0..16 {
            let Ok(output) = adapter.EnumOutputs(i) else { break };
            let Ok(desc) = output.GetDesc() else { continue };
            if wide_to_string(&desc.DeviceName) == want {
                return output.cast::<IDXGIOutput1>().ctx("IDXGIOutput1");
            }
        }
    }
    Err(PlatformError(format!("monitor {id} is driven by another GPU: use Windows.Graphics.Capture or select that GPU")))
}

fn run(gpu: &Gpu, id: &str, shared: &Shared, draw_cursor: bool, max_fps: u32) -> crate::Result<()> {
    ensure_com()?;
    let _prio = boost_thread(ThreadKind::Capture);
    let min_interval = if max_fps > 0 { Hns(10_000_000 / i64::from(max_fps) * 95 / 100) } else { Hns(0) };
    let mut shape_buf: Vec<u8> = Vec::new();
    // AcquireNextFrame with a timeout waits *inside* the device's multithread lock, which would
    // stall the render thread for the whole wait. Poll with a zero timeout instead, at twice the
    // monitor's refresh rate, sleeping on our own timer outside the lock.
    let refresh = super::super::display::list_monitors()
        .ok()
        .and_then(|m| m.into_iter().find(|m| if id.is_empty() { m.primary } else { m.id == id }))
        .map_or(60.0, |m| m.refresh_hz)
        .clamp(24.0, 500.0);
    let poll = Hns((10_000_000.0 / refresh / 2.0) as i64);
    let timer = PrecisionTimer::new()?;
    while !shared.stop.load(Ordering::Relaxed) {
        let dup = match find_output(gpu, id).and_then(|o| {
            // SAFETY: duplicates a live output for our device.
            unsafe { o.DuplicateOutput(&gpu.device) }.ctx("cannot duplicate this monitor (another app may be using it, or a secure desktop is shown)")
        }) {
            Ok(d) => d,
            Err(e) => {
                *shared.state.lock().unwrap_or_else(PoisonError::into_inner) = SourceState::Lost(e.0);
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
        };
        let mut last_copy = Hns(i64::MIN / 2);
        loop {
            if shared.stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            // SAFETY: live out-locals; a successful acquire is always released below.
            match unsafe { dup.AcquireNextFrame(0, &mut info, &mut resource) } {
                Ok(()) => {
                    let t = now();
                    let r = handle_frame(gpu, &dup, shared, &info, resource.as_ref(), draw_cursor, t - last_copy >= min_interval, &mut shape_buf);
                    // SAFETY: releases the frame acquired above.
                    let _ = unsafe { dup.ReleaseFrame() };
                    match r {
                        Ok(true) => last_copy = t,
                        Ok(false) => {}
                        Err(e) => log::debug!("duplication frame: {e}"),
                    }
                }
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => timer.wait_until(now() + poll),
                Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                    // Mode change, full-screen switch, UAC prompt: recreate.
                    std::thread::sleep(Duration::from_millis(50));
                    break;
                }
                Err(e) => {
                    *shared.state.lock().unwrap_or_else(PoisonError::into_inner) = SourceState::Lost(format!("desktop duplication: {e}"));
                    std::thread::sleep(Duration::from_millis(500));
                    break;
                }
            }
        }
    }
    Ok(())
}

fn handle_frame(
    gpu: &Gpu,
    dup: &IDXGIOutputDuplication,
    shared: &Shared,
    info: &DXGI_OUTDUPL_FRAME_INFO,
    resource: Option<&IDXGIResource>,
    draw_cursor: bool,
    copy_allowed: bool,
    shape_buf: &mut Vec<u8>,
) -> crate::Result<bool> {
    let mut slot = shared.slot.lock().unwrap_or_else(PoisonError::into_inner);
    let mut copied = false;
    if info.LastPresentTime != 0
        && copy_allowed
        && let Some(res) = resource
    {
        let src: ID3D11Texture2D = res.cast().ctx("desktop texture")?;
        let mut desc = Default::default();
        // SAFETY: plain getter.
        unsafe { src.GetDesc(&mut desc) };
        let want_mips = shared.want_mips.load(Ordering::Relaxed);
        if slot.texture.is_none() || slot.width != desc.Width || slot.height != desc.Height || want_mips != (slot.mips > 1) {
            let (misc, mips) = if want_mips { (D3D11_RESOURCE_MISC_GENERATE_MIPS, 0) } else { (D3D11_RESOURCE_MISC_FLAG(0), 1) };
            let tex = gpu.texture(desc.Width, desc.Height, DXGI_FORMAT_B8G8R8A8_UNORM, D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET, misc, mips)?;
            let mut td = Default::default();
            // SAFETY: plain getter.
            unsafe { tex.GetDesc(&mut td) };
            slot.view = Some(srv(gpu, &tex)?);
            slot.texture = Some(tex);
            slot.width = desc.Width;
            slot.height = desc.Height;
            slot.mips = td.MipLevels.max(1);
        }
        if let (Some(dst), Some(view)) = (slot.texture.as_ref(), slot.view.as_ref()) {
            // SAFETY: same size, both BGRA8 (duplication always delivers BGRA8 for DuplicateOutput).
            unsafe {
                gpu.context.CopySubresourceRegion(dst, 0, 0, 0, 0, &src, 0, None);
                if slot.mips > 1 {
                    gpu.context.GenerateMips(view);
                }
            }
        }
        slot.seq = slot.seq.wrapping_add(1);
        shared.delivered.fetch_add(1, Ordering::Relaxed);
        shared.last_time.store(now().0, Ordering::Relaxed);
        copied = true;
        *shared.state.lock().unwrap_or_else(PoisonError::into_inner) = SourceState::Active { width: slot.width, height: slot.height };
    }
    if draw_cursor && info.LastMouseUpdateTime != 0 {
        slot.cursor_visible = info.PointerPosition.Visible.as_bool();
        slot.cursor_pos = (info.PointerPosition.Position.x, info.PointerPosition.Position.y);
        if info.PointerShapeBufferSize > 0 && info.PointerShapeBufferSize <= 4 * 1024 * 1024 {
            shape_buf.resize(info.PointerShapeBufferSize as usize, 0);
            let mut needed = 0u32;
            let mut shape = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
            // SAFETY: the buffer holds exactly the size passed.
            if unsafe { dup.GetFramePointerShape(info.PointerShapeBufferSize, shape_buf.as_mut_ptr().cast(), &mut needed, &mut shape) }.is_ok()
                && let Some((w, h, rgba)) = cursor_rgba(&shape, shape_buf)
            {
                let tex = gpu.texture(w, h, DXGI_FORMAT_R8G8B8A8_UNORM, D3D11_BIND_SHADER_RESOURCE, D3D11_RESOURCE_MISC_FLAG(0), 1)?;
                // SAFETY: `rgba` holds exactly w*h*4 bytes with pitch w*4.
                unsafe { gpu.context.UpdateSubresource(&tex, 0, None, rgba.as_ptr().cast(), w * 4, 0) };
                slot.cursor = Some(Cursor { view: srv(gpu, &tex)?, width: w, height: h });
            }
        }
        slot.seq = slot.seq.wrapping_add(1);
    }
    Ok(copied)
}

/// Converts a DXGI pointer shape into straight RGBA (XOR pixels are approximated: they cannot be
/// reproduced without reading the screen behind them).
fn cursor_rgba(shape: &DXGI_OUTDUPL_POINTER_SHAPE_INFO, buf: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let (w, pitch) = (shape.Width as usize, shape.Pitch as usize);
    let t = shape.Type as i32;
    let h = if t == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 { shape.Height as usize / 2 } else { shape.Height as usize };
    if w == 0 || h == 0 || w > 256 || h > 256 {
        return None;
    }
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let px: [u8; 4] = if t == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 {
                let and = (buf.get(y * pitch + x / 8)? >> (7 - (x % 8))) & 1;
                let xor = (buf.get((y + h) * pitch + x / 8)? >> (7 - (x % 8))) & 1;
                match (and, xor) {
                    (0, 0) => [0, 0, 0, 255],
                    (0, 1) => [255, 255, 255, 255],
                    (1, 0) => [0, 0, 0, 0],
                    _ => [0, 0, 0, 255], // inverted: black reads well on most backgrounds
                }
            } else {
                let o = y * pitch + x * 4;
                let (b, g, r, a) = (*buf.get(o)?, *buf.get(o + 1)?, *buf.get(o + 2)?, *buf.get(o + 3)?);
                if t == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 {
                    [r, g, b, a]
                } else if t == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 {
                    // Mask 0 = opaque colour; mask 0xFF = XOR with the screen (approximated).
                    if a == 0 {
                        [r, g, b, 255]
                    } else if (r | g | b) == 0 {
                        [0, 0, 0, 0]
                    } else {
                        [r, g, b, 255]
                    }
                } else {
                    return None;
                }
            };
            if let Some(d) = out.get_mut((y * w + x) * 4..(y * w + x) * 4 + 4) {
                d.copy_from_slice(&px);
            }
        }
    }
    Some((w as u32, h as u32, out))
}

impl CaptureSource for DupSource {
    fn frame(&self) -> Option<CapturedFrame> {
        let slot = self.shared.slot.lock().unwrap_or_else(PoisonError::into_inner);
        let cursor =
            if slot.cursor_visible { slot.cursor.as_ref().map(|c| (c.view.clone(), slot.cursor_pos.0, slot.cursor_pos.1, c.width, c.height)) } else { None };
        Some(CapturedFrame { view: slot.view.clone()?, width: slot.width, height: slot.height, seq: slot.seq, mip_levels: slot.mips, cursor })
    }

    fn state(&self) -> SourceState {
        self.shared.state.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn set_want_mips(&self, want: bool) {
        self.shared.want_mips.store(want, Ordering::Relaxed);
    }

    fn delivered(&self) -> u64 {
        self.shared.delivered.load(Ordering::Relaxed)
    }

    fn last_frame_time(&self) -> Hns {
        Hns(self.shared.last_time.load(Ordering::Relaxed))
    }
}

impl Drop for DupSource {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_shapes_are_bounded() {
        let mono = DXGI_OUTDUPL_POINTER_SHAPE_INFO { Type: 1, Width: 8, Height: 16, Pitch: 1, ..Default::default() };
        let mut buf = vec![0xFFu8; 8];
        buf.extend([0u8; 8]);
        let (w, h, px) = cursor_rgba(&mono, &buf).unwrap();
        assert_eq!((w, h), (8, 8));
        assert!(px.chunks(4).all(|p| p[3] == 0));
        // Truncated buffer / hostile sizes: no panic.
        assert!(cursor_rgba(&mono, &buf[..3]).is_none());
        let huge = DXGI_OUTDUPL_POINTER_SHAPE_INFO { Type: 2, Width: 100_000, Height: 2, Pitch: 400_000, ..Default::default() };
        assert!(cursor_rgba(&huge, &[]).is_none());
    }
}
