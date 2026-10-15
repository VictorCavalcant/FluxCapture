//! The Direct3D 11 device and small resource helpers.

use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCOMPILE_ENABLE_STRICTNESS, D3DCOMPILE_OPTIMIZATION_LEVEL3, D3DCompile};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1, ID3DBlob,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_FLAG, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_FLAG,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_RESOURCE_MISC_FLAG, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1, IDXGIFactory4};
use windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice;
use windows::core::{Interface, PCSTR};

use super::display::{luid_from_u64, luid_u64};
use super::{Context, wide_to_string};
use crate::{GpuVendor, PlatformError};

/// A Direct3D 11 device on one adapter, shared by capture, composition and encoding.
#[derive(Clone)]
pub struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    /// The same device as a WinRT object (Windows.Graphics.Capture wants one).
    pub winrt: IDirect3DDevice,
    pub luid: u64,
    pub name: String,
    #[allow(dead_code)]
    pub vendor: GpuVendor,
}

// SAFETY: ID3D11Device is free-threaded; the immediate context is used from several threads only
// with multithread protection on (`SetMultithreadProtected(true)` in `Gpu::new`), which makes the
// runtime serialise every context call. IDirect3DDevice is an agile WinRT wrapper of the same device.
unsafe impl Send for Gpu {}
// SAFETY: as above.
unsafe impl Sync for Gpu {}

impl Gpu {
    /// A device on the adapter `luid` (the default adapter when `None`).
    pub fn new(luid: Option<u64>) -> crate::Result<Gpu> {
        // SAFETY: plain factory creation.
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.ctx("cannot create a DXGI factory")?;
        let adapter: IDXGIAdapter1 = match luid {
            Some(l) => {
                let f4: IDXGIFactory4 = factory.cast().ctx("IDXGIFactory4")?;
                // SAFETY: plain lookup by LUID.
                unsafe { f4.EnumAdapterByLuid(luid_from_u64(l)) }.ctx("the selected GPU is not available")?
            }
            // SAFETY: adapter 0 is the default adapter.
            None => unsafe { factory.EnumAdapters1(0) }.ctx("no GPU found")?,
        };
        // SAFETY: plain getter.
        let desc = unsafe { adapter.GetDesc1() }.ctx("IDXGIAdapter1::GetDesc1")?;
        let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_10_0];
        let adapter_base: IDXGIAdapter = adapter.cast().ctx("IDXGIAdapter")?;
        let mut last_err = String::new();
        // Video support lets Media Foundation share the device; retry without it for adapters
        // (WARP, odd drivers) that refuse it — encoding then falls back to system memory.
        for flags in [D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_CREATE_DEVICE_BGRA_SUPPORT] {
            let (mut device, mut context, mut level) = (None, None, D3D_FEATURE_LEVEL::default());
            // SAFETY: explicit adapter ⇒ driver type UNKNOWN; out-pointers are live locals.
            let r = unsafe {
                D3D11CreateDevice(
                    &adapter_base,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE::default(),
                    flags,
                    Some(&levels),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    Some(&mut level),
                    Some(&mut context),
                )
            };
            match (r, device, context) {
                (Ok(()), Some(device), Some(context)) => return Gpu::finish(device, context, &desc, level, flags),
                (r, _, _) => last_err = format!("{r:?}"),
            }
        }
        Err(PlatformError(format!("cannot create a Direct3D 11 device on {}: {last_err}", wide_to_string(&desc.Description))))
    }

    fn finish(
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        desc: &windows::Win32::Graphics::Dxgi::DXGI_ADAPTER_DESC1,
        level: D3D_FEATURE_LEVEL,
        flags: D3D11_CREATE_DEVICE_FLAG,
    ) -> crate::Result<Gpu> {
        // Capture callbacks (Windows thread pool), the render thread and the encoder all use the
        // immediate context: let the runtime serialise them.
        if let Ok(mt) = context.cast::<ID3D11Multithread>() {
            // SAFETY: plain setter.
            let _ = unsafe { mt.SetMultithreadProtected(true) };
        } else if let Ok(mt) = context.cast::<windows::Win32::Graphics::Direct3D10::ID3D10Multithread>() {
            // SAFETY: plain setter.
            let _ = unsafe { mt.SetMultithreadProtected(true) };
        }
        let dxgi: IDXGIDevice = device.cast().ctx("IDXGIDevice")?;
        // Raise our GPU scheduling priority so a GPU-bound game does not starve composition
        // (what OBS does too). Needs no privilege for values up to 7 on WDDM 2+; failure is fine.
        // SAFETY: plain setter.
        let _ = unsafe { dxgi.SetGPUThreadPriority(7) };
        // SAFETY: wraps the DXGI device in a WinRT IDirect3DDevice.
        let winrt: IDirect3DDevice =
            unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }.ctx("CreateDirect3D11DeviceFromDXGIDevice")?.cast().ctx("IDirect3DDevice")?;
        let name = wide_to_string(&desc.Description);
        log::info!("GPU: {name} (feature level {:#x}, video support: {})", level.0, (flags.0 & D3D11_CREATE_DEVICE_VIDEO_SUPPORT.0) != 0);
        Ok(Gpu { device, context, winrt, luid: luid_u64(desc.AdapterLuid), name, vendor: GpuVendor::from_pci(desc.VendorId) })
    }

    /// A default-usage 2D texture.
    pub fn texture(
        &self,
        w: u32,
        h: u32,
        format: DXGI_FORMAT,
        bind: D3D11_BIND_FLAG,
        misc: D3D11_RESOURCE_MISC_FLAG,
        mips: u32,
    ) -> crate::Result<ID3D11Texture2D> {
        if w == 0 || h == 0 || w > 16384 || h > 16384 {
            return Err(PlatformError(format!("invalid texture size {w}x{h}")));
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: mips,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: bind.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: misc.0 as u32,
        };
        let mut tex = None;
        // SAFETY: valid descriptor; out-pointer is a live local.
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex)) }.ctx("CreateTexture2D")?;
        tex.ok_or_else(|| PlatformError("CreateTexture2D returned nothing".into()))
    }

    /// A render-target + shader-resource texture.
    pub fn render_texture(&self, w: u32, h: u32, format: DXGI_FORMAT) -> crate::Result<ID3D11Texture2D> {
        self.texture(w, h, format, D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE, D3D11_RESOURCE_MISC_FLAG(0), 1)
    }

    /// A CPU-readable staging texture.
    pub fn staging_texture(&self, w: u32, h: u32, format: DXGI_FORMAT) -> crate::Result<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut tex = None;
        // SAFETY: valid descriptor; out-pointer is a live local.
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex)) }.ctx("CreateTexture2D (staging)")?;
        tex.ok_or_else(|| PlatformError("CreateTexture2D returned nothing".into()))
    }
}

/// Picks the adapter with the best hardware encoder, preferring the one driving the primary monitor
/// (no cross-adapter copies). Used when the user leaves the GPU on "Automatic".
pub fn default_adapter_luid() -> Option<u64> {
    let gpus = super::display::list_gpus().ok()?;
    let monitors = super::display::list_monitors().unwrap_or_default();
    let primary = monitors.iter().find(|m| m.primary).map(|m| m.gpu_luid).filter(|l| *l != 0);
    let hardware: Vec<&crate::GpuInfo> = gpus.iter().filter(|g| !g.software).collect();
    if let Some(p) = primary
        && hardware.iter().any(|g| g.luid == p)
    {
        return Some(p);
    }
    hardware.first().map(|g| g.luid)
}

/// Compiles an HLSL entry point (d3dcompiler_47.dll ships with Windows 10 and 11).
pub fn compile(source: &str, entry: &str, target: &str) -> crate::Result<Vec<u8>> {
    let entry_c = format!("{entry}\0");
    let target_c = format!("{target}\0");
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: `source` is valid for `len` bytes; entry/target are NUL-terminated and outlive the
    // call; out-pointers are live locals.
    let r = unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            PCSTR(c"fluxcapture.hlsl".as_ptr().cast()),
            None,
            None,
            PCSTR(entry_c.as_ptr()),
            PCSTR(target_c.as_ptr()),
            D3DCOMPILE_OPTIMIZATION_LEVEL3 | D3DCOMPILE_ENABLE_STRICTNESS,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    let blob_bytes = |b: &ID3DBlob| -> Vec<u8> {
        // SAFETY: a blob's pointer is valid for GetBufferSize bytes while the blob lives.
        unsafe { std::slice::from_raw_parts(b.GetBufferPointer().cast::<u8>(), b.GetBufferSize()) }.to_vec()
    };
    match (r, code) {
        (Ok(()), Some(code)) => Ok(blob_bytes(&code)),
        (r, _) => {
            let msg = errors.as_ref().map(|e| String::from_utf8_lossy(&blob_bytes(e)).into_owned()).unwrap_or_else(|| format!("{r:?}"));
            Err(PlatformError(format!("shader {entry} failed to compile: {msg}")))
        }
    }
}
