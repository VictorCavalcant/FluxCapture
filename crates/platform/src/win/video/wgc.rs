//! Windows.Graphics.Capture sources (monitors and windows).
//!
//! The OS calls `FrameArrived` on its thread pool whenever the content changes (nothing arrives for
//! a static screen — that costs nothing). The handler copies the frame into this source's own
//! texture on the GPU (one `CopySubresourceRegion`, no CPU access) and releases the OS buffer at
//! once, so the capture never stalls on composition; the render thread samples whatever is latest.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use fluxcapture_time::Hns;
use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_RESOURCE_MISC_FLAG, D3D11_RESOURCE_MISC_GENERATE_MIPS, ID3D11ShaderResourceView,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess;
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::core::{IInspectable, Interface};

use super::super::Context;
use super::super::d3d::Gpu;
use super::super::display::{monitor_handle, resolve_window};
use super::compositor::srv;
use super::{CaptureSource, CapturedFrame};
use crate::{CaptureOptions, CaptureTarget, PlatformError, SourceState};

struct Slot {
    texture: Option<ID3D11Texture2D>,
    view: Option<ID3D11ShaderResourceView>,
    width: u32,
    height: u32,
    mip_levels: u32,
    seq: u64,
}

struct Shared {
    gpu: Gpu,
    slot: Mutex<Slot>,
    want_mips: AtomicBool,
    lost: Mutex<Option<String>>,
    delivered: AtomicU64,
    last_time: AtomicI64,
    min_interval: i64,
}

/// A running Windows.Graphics.Capture session.
pub struct WgcSource {
    shared: Arc<Shared>,
    session: GraphicsCaptureSession,
    pool: Direct3D11CaptureFramePool,
    item: GraphicsCaptureItem,
    frame_token: i64,
    closed_token: i64,
}

// SAFETY: the WinRT capture objects are agile (free-threaded) per their metadata; the windows crate
// marks them Send. `Shared` holds a Gpu (Send + Sync) and std sync primitives.
unsafe impl Send for WgcSource {}

/// Whether Windows.Graphics.Capture works on this system (Windows 10 1903+).
pub fn supported() -> bool {
    GraphicsCaptureSession::IsSupported().unwrap_or(false)
}

fn create_item(target: &CaptureTarget) -> crate::Result<GraphicsCaptureItem> {
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>().ctx("Windows.Graphics.Capture is not available")?;
    match target {
        CaptureTarget::Monitor { id } => {
            let (hmon, _) = monitor_handle(id)?;
            // SAFETY: a live monitor handle from the enumeration.
            unsafe { interop.CreateForMonitor(hmon) }.ctx("cannot capture this monitor")
        }
        CaptureTarget::Window { handle, title, process } => {
            let hwnd = resolve_window(*handle, title, process)?;
            // SAFETY: a window handle that IsWindow just accepted.
            unsafe { interop.CreateForWindow(hwnd) }.ctx("cannot capture this window (protected or closed?)")
        }
    }
}

impl WgcSource {
    pub fn start(gpu: &Gpu, target: &CaptureTarget, options: &CaptureOptions) -> crate::Result<WgcSource> {
        if !supported() {
            return Err(PlatformError("Windows.Graphics.Capture needs Windows 10 version 1903 or later".into()));
        }
        let item = create_item(target)?;
        let size = item.Size().ctx("GraphicsCaptureItem::Size")?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&gpu.winrt, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, size)
            .ctx("cannot create the capture frame pool")?;
        let session = pool.CreateCaptureSession(&item).ctx("cannot create the capture session")?;
        // Optional properties: older Windows builds lack them, which is fine.
        let _ = session.SetIsCursorCaptureEnabled(options.cursor);
        let _ = session.SetIsBorderRequired(options.border);
        let min_interval = if options.max_fps > 0 { 10_000_000 / i64::from(options.max_fps) } else { 0 };
        if min_interval > 0 {
            // Windows 11 24H2+: the OS stops producing frames faster than this.
            let _ = session.SetMinUpdateInterval(TimeSpan { Duration: min_interval });
        }
        let shared = Arc::new(Shared {
            gpu: gpu.clone(),
            slot: Mutex::new(Slot { texture: None, view: None, width: 0, height: 0, mip_levels: 1, seq: 0 }),
            want_mips: AtomicBool::new(false),
            lost: Mutex::new(None),
            delivered: AtomicU64::new(0),
            last_time: AtomicI64::new(i64::MIN / 2),
            // Accept frames slightly early (5%) so jitter does not halve the rate.
            min_interval: min_interval * 95 / 100,
        });
        let handler_shared = Arc::clone(&shared);
        let pool_size = Arc::new(Mutex::new(size));
        let frame_token = pool
            .FrameArrived(&TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(move |pool, _| {
                let Some(pool) = pool.as_ref() else { return Ok(()) };
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| on_frame(&handler_shared, pool, &pool_size)));
                match r {
                    Ok(Err(e)) => log::debug!("capture frame dropped: {e}"),
                    Err(_) => log::error!("capture frame handler panicked"),
                    Ok(Ok(())) => {}
                }
                Ok(())
            }))
            .ctx("cannot subscribe to capture frames")?;
        let closed_shared = Arc::clone(&shared);
        let closed_token = item
            .Closed(&TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(move |_, _| {
                *closed_shared.lost.lock().unwrap_or_else(PoisonError::into_inner) = Some("the captured window or monitor is gone".into());
                Ok(())
            }))
            .ctx("cannot subscribe to capture closing")?;
        session.StartCapture().ctx("cannot start the capture")?;
        Ok(WgcSource { shared, session, pool, item, frame_token, closed_token })
    }
}

fn on_frame(shared: &Shared, pool: &Direct3D11CaptureFramePool, pool_size: &Mutex<SizeInt32>) -> crate::Result<()> {
    let frame = pool.TryGetNextFrame().ctx("TryGetNextFrame")?;
    let time = frame.SystemRelativeTime().map(|t| t.Duration).unwrap_or(0);
    let last = shared.last_time.load(Ordering::Relaxed);
    if shared.min_interval > 0 && time.saturating_sub(last) < shared.min_interval {
        let _ = frame.Close();
        return Ok(());
    }
    let content = frame.ContentSize().ctx("ContentSize")?;
    let surface = frame.Surface().ctx("Surface")?;
    let access: IDirect3DDxgiInterfaceAccess = surface.cast().ctx("IDirect3DDxgiInterfaceAccess")?;
    // SAFETY: plain interface query on a live surface.
    let src: ID3D11Texture2D = unsafe { access.GetInterface() }.ctx("frame texture")?;
    let mut desc = Default::default();
    // SAFETY: plain getter into a live local.
    unsafe { src.GetDesc(&mut desc) };
    // The pool buffers keep their old size until recreated: copy only the valid region.
    let w = (content.Width.max(1) as u32).min(desc.Width);
    let h = (content.Height.max(1) as u32).min(desc.Height);
    {
        let mut slot = shared.slot.lock().unwrap_or_else(PoisonError::into_inner);
        let want_mips = shared.want_mips.load(Ordering::Relaxed);
        let has_mips = slot.mip_levels > 1;
        if slot.texture.is_none() || slot.width != w || slot.height != h || want_mips != has_mips {
            let (misc, mips) = if want_mips { (D3D11_RESOURCE_MISC_GENERATE_MIPS, 0) } else { (D3D11_RESOURCE_MISC_FLAG(0), 1) };
            let tex = shared.gpu.texture(w, h, DXGI_FORMAT_B8G8R8A8_UNORM, D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET, misc, mips)?;
            let mut td = Default::default();
            // SAFETY: plain getter into a live local.
            unsafe { tex.GetDesc(&mut td) };
            slot.view = Some(srv(&shared.gpu, &tex)?);
            slot.texture = Some(tex);
            slot.width = w;
            slot.height = h;
            slot.mip_levels = td.MipLevels.max(1);
        }
        if let (Some(dst), Some(view)) = (slot.texture.as_ref(), slot.view.as_ref()) {
            let region = D3D11_BOX { left: 0, top: 0, front: 0, right: w, bottom: h, back: 1 };
            // SAFETY: both textures are BGRA8 on the same device; the box lies inside both (w/h are
            // clamped to the source and equal the destination size).
            unsafe {
                shared.gpu.context.CopySubresourceRegion(dst, 0, 0, 0, 0, &src, 0, Some(&region));
                if slot.mip_levels > 1 {
                    shared.gpu.context.GenerateMips(view);
                }
            }
        }
        slot.seq = slot.seq.wrapping_add(1);
    }
    drop(src);
    let _ = frame.Close();
    shared.last_time.store(time, Ordering::Relaxed);
    shared.delivered.fetch_add(1, Ordering::Relaxed);
    // Window resized: recreate the pool at the new size (after releasing the frame).
    let mut size = pool_size.lock().unwrap_or_else(PoisonError::into_inner);
    if content.Width > 0 && content.Height > 0 && (content.Width != size.Width || content.Height != size.Height) {
        *size = content;
        drop(size);
        pool.Recreate(&shared.gpu.winrt, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, content).ctx("Recreate")?;
    }
    Ok(())
}

impl CaptureSource for WgcSource {
    fn frame(&self) -> Option<CapturedFrame> {
        let slot = self.shared.slot.lock().unwrap_or_else(PoisonError::into_inner);
        Some(CapturedFrame { view: slot.view.clone()?, width: slot.width, height: slot.height, seq: slot.seq, mip_levels: slot.mip_levels, cursor: None })
    }

    fn state(&self) -> SourceState {
        if let Some(msg) = self.shared.lost.lock().unwrap_or_else(PoisonError::into_inner).clone() {
            return SourceState::Lost(msg);
        }
        let slot = self.shared.slot.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.texture.is_none() { SourceState::Starting } else { SourceState::Active { width: slot.width, height: slot.height } }
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

impl Drop for WgcSource {
    fn drop(&mut self) {
        let _ = self.pool.RemoveFrameArrived(self.frame_token);
        let _ = self.item.RemoveClosed(self.closed_token);
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}
