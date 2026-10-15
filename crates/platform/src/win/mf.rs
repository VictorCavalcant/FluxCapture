//! Media Foundation, loaded at run time.
//!
//! `mfplat.dll` is resolved with `LoadLibraryExW` instead of being imported: Windows "N" editions ship
//! without the Media Feature Pack, and FluxCapture must still start there (and say what is missing)
//! instead of failing to load.

use std::ffi::c_void;
use std::sync::OnceLock;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFAttributes, IMFDXGIDeviceManager, IMFMediaBuffer, IMFMediaType, IMFSample, IMFTrackedSample, MF_VERSION, MFT_REGISTER_TYPE_INFO,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};
use windows::core::{BOOL, GUID, HRESULT, Interface, PCSTR, w};

use super::{Context, ensure_com};
use crate::PlatformError;

type StartupFn = unsafe extern "system" fn(u32, u32) -> HRESULT;
type CreateFn = unsafe extern "system" fn(*mut *mut c_void) -> HRESULT;
type CreateBufferFn = unsafe extern "system" fn(u32, *mut *mut c_void) -> HRESULT;
type CreateAttributesFn = unsafe extern "system" fn(*mut *mut c_void, u32) -> HRESULT;
type CreateManagerFn = unsafe extern "system" fn(*mut u32, *mut *mut c_void) -> HRESULT;
type CreateSurfaceBufferFn = unsafe extern "system" fn(*const GUID, *mut c_void, u32, BOOL, *mut *mut c_void) -> HRESULT;
type EnumExFn = unsafe extern "system" fn(GUID, u32, *const MFT_REGISTER_TYPE_INFO, *const MFT_REGISTER_TYPE_INFO, *mut *mut *mut c_void, *mut u32) -> HRESULT;
type Enum2Fn =
    unsafe extern "system" fn(GUID, u32, *const MFT_REGISTER_TYPE_INFO, *const MFT_REGISTER_TYPE_INFO, *mut c_void, *mut *mut *mut c_void, *mut u32) -> HRESULT;

/// The `mfplat.dll` entry points FluxCapture uses.
pub struct MfApi {
    create_media_type: CreateFn,
    create_sample: CreateFn,
    create_tracked_sample: Option<CreateFn>,
    create_memory_buffer: CreateBufferFn,
    create_attributes: CreateAttributesFn,
    create_manager: CreateManagerFn,
    create_surface_buffer: CreateSurfaceBufferFn,
    enum_ex: EnumExFn,
    enum2: Option<Enum2Fn>,
}

/// The exported function `name` of `module` as `F`.
///
/// # Safety
/// `F` must be the `extern "system" fn` type that `name` really has.
unsafe fn symbol<F: Copy>(module: HMODULE, name: PCSTR) -> Option<F> {
    if std::mem::size_of::<F>() != std::mem::size_of::<usize>() {
        return None;
    }
    // SAFETY: `module` is a loaded module and `name` a NUL-terminated literal.
    let p = unsafe { GetProcAddress(module, name) }?;
    // SAFETY: `F` is pointer-sized and, by contract, the real type of the export.
    Some(unsafe { std::mem::transmute_copy::<_, F>(&p) })
}

macro_rules! sym {
    ($m:expr, $name:literal) => {
        // SAFETY: each symbol is resolved with the signature Microsoft documents for it.
        unsafe { symbol($m, PCSTR(concat!($name, "\0").as_ptr())) }.ok_or_else(|| PlatformError(format!("mfplat.dll lacks {}", $name)))?
    };
}

/// Media Foundation, loaded and started once per process (never shut down: encoders come and go,
/// and its worker threads end with the process).
pub fn api() -> crate::Result<&'static MfApi> {
    static API: OnceLock<Result<MfApi, String>> = OnceLock::new();
    API.get_or_init(|| load().map_err(|e| e.0)).as_ref().map_err(|e| PlatformError(e.clone()))
}

fn load() -> crate::Result<MfApi> {
    ensure_com()?;
    // SAFETY: loads a system DLL by name from System32 only.
    let m = unsafe { LoadLibraryExW(w!("mfplat.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }.map_err(|_| {
        PlatformError("Media Foundation is not installed (Windows N/KN editions: install the \"Media Feature Pack\" from Settings › Optional features)".into())
    })?;
    let startup: StartupFn = sym!(m, "MFStartup");
    let api = MfApi {
        create_media_type: sym!(m, "MFCreateMediaType"),
        create_sample: sym!(m, "MFCreateSample"),
        // SAFETY: documented signature.
        create_tracked_sample: unsafe { symbol(m, PCSTR(c"MFCreateTrackedSample".as_ptr().cast())) },
        create_memory_buffer: sym!(m, "MFCreateMemoryBuffer"),
        create_attributes: sym!(m, "MFCreateAttributes"),
        create_manager: sym!(m, "MFCreateDXGIDeviceManager"),
        create_surface_buffer: sym!(m, "MFCreateDXGISurfaceBuffer"),
        enum_ex: sym!(m, "MFTEnumEx"),
        // SAFETY: documented signature (Windows 10 1703+; absent before).
        enum2: unsafe { symbol(m, PCSTR(c"MFTEnum2".as_ptr().cast())) },
    };
    // SAFETY: MFSTARTUP_FULL (0) with the version these bindings were built for.
    let hr = unsafe { startup(MF_VERSION, 0) };
    if hr.is_err() {
        return Err(PlatformError(format!("MFStartup failed (0x{:08X})", hr.0 as u32)));
    }
    Ok(api)
}

/// An out-pointer creation result as an interface.
fn made<T: Interface>(hr: HRESULT, raw: *mut c_void, what: &str) -> crate::Result<T> {
    if hr.is_err() || raw.is_null() {
        return Err(PlatformError(format!("{what} failed (0x{:08X})", hr.0 as u32)));
    }
    // SAFETY: the creation function succeeded, so `raw` is a new reference to a `T` we now own.
    Ok(unsafe { T::from_raw(raw) })
}

impl MfApi {
    pub fn media_type(&self) -> crate::Result<IMFMediaType> {
        let mut raw = std::ptr::null_mut();
        // SAFETY: the out-pointer is a live local.
        let hr = unsafe { (self.create_media_type)(&mut raw) };
        made(hr, raw, "MFCreateMediaType")
    }

    pub fn sample(&self) -> crate::Result<IMFSample> {
        let mut raw = std::ptr::null_mut();
        // SAFETY: the out-pointer is a live local.
        let hr = unsafe { (self.create_sample)(&mut raw) };
        made(hr, raw, "MFCreateSample")
    }

    pub fn tracked_sample(&self) -> crate::Result<IMFTrackedSample> {
        let f = self.create_tracked_sample.ok_or("MFCreateTrackedSample is not available")?;
        let mut raw = std::ptr::null_mut();
        // SAFETY: the out-pointer is a live local.
        let hr = unsafe { f(&mut raw) };
        made(hr, raw, "MFCreateTrackedSample")
    }

    pub fn memory_buffer(&self, len: u32) -> crate::Result<IMFMediaBuffer> {
        let mut raw = std::ptr::null_mut();
        // SAFETY: the out-pointer is a live local.
        let hr = unsafe { (self.create_memory_buffer)(len, &mut raw) };
        made(hr, raw, "MFCreateMemoryBuffer")
    }

    pub fn attributes(&self, initial: u32) -> crate::Result<IMFAttributes> {
        let mut raw = std::ptr::null_mut();
        // SAFETY: the out-pointer is a live local.
        let hr = unsafe { (self.create_attributes)(&mut raw, initial) };
        made(hr, raw, "MFCreateAttributes")
    }

    /// A device manager sharing `device` with Media Foundation components.
    pub fn device_manager(&self, device: &ID3D11Device) -> crate::Result<IMFDXGIDeviceManager> {
        let (mut token, mut raw) = (0u32, std::ptr::null_mut());
        // SAFETY: both out-pointers are live locals.
        let hr = unsafe { (self.create_manager)(&mut token, &mut raw) };
        let manager: IMFDXGIDeviceManager = made(hr, raw, "MFCreateDXGIDeviceManager")?;
        // SAFETY: plain COM call with the token just returned.
        unsafe { manager.ResetDevice(device, token) }.ctx("IMFDXGIDeviceManager::ResetDevice")?;
        Ok(manager)
    }

    /// A media buffer wrapping a texture (no copy).
    pub fn surface_buffer(&self, texture: &ID3D11Texture2D) -> crate::Result<IMFMediaBuffer> {
        let mut raw = std::ptr::null_mut();
        // SAFETY: `texture.as_raw()` is a live ID3D11Texture2D for the duration of the call (the buffer
        // takes its own reference); the out-pointer is a live local.
        let hr = unsafe { (self.create_surface_buffer)(&ID3D11Texture2D::IID, texture.as_raw(), 0, BOOL(0), &mut raw) };
        made(hr, raw, "MFCreateDXGISurfaceBuffer")
    }

    /// Transforms of `category` matching the type pair, as activation objects (sorted by merit,
    /// hardware first when `flags` asks for it). With `adapter_luid`, only transforms of that adapter
    /// (MFTEnum2, Windows 10 1703+).
    pub fn enum_transforms(
        &self,
        category: GUID,
        flags: u32,
        input: Option<MFT_REGISTER_TYPE_INFO>,
        output: Option<MFT_REGISTER_TYPE_INFO>,
        adapter_luid: Option<u64>,
    ) -> crate::Result<Vec<IMFActivate>> {
        let (mut list, mut count) = (std::ptr::null_mut::<*mut c_void>(), 0u32);
        let ip = input.as_ref().map_or(std::ptr::null(), |t| t as *const _);
        let op = output.as_ref().map_or(std::ptr::null(), |t| t as *const _);
        let hr = match (adapter_luid, self.enum2) {
            (Some(luid), Some(enum2)) => {
                let attrs = self.attributes(1)?;
                // SAFETY: plain attribute store write.
                unsafe { attrs.SetBlob(&windows::Win32::Media::MediaFoundation::MFT_ENUM_ADAPTER_LUID, &luid.to_le_bytes()) }.ctx("MFT_ENUM_ADAPTER_LUID")?;
                // SAFETY: type infos and out-pointers are live locals; `attrs` outlives the call.
                unsafe { enum2(category, flags, ip, op, attrs.as_raw(), &mut list, &mut count) }
            }
            // SAFETY: type infos and out-pointers are live locals.
            _ => unsafe { (self.enum_ex)(category, flags, ip, op, &mut list, &mut count) },
        };
        if hr.is_err() {
            return Err(PlatformError(format!("MFTEnum failed (0x{:08X})", hr.0 as u32)));
        }
        let mut out = Vec::new();
        if !list.is_null() {
            for i in 0..count.min(256) as usize {
                // SAFETY: `list` holds `count` pointers (CoTaskMemAlloc'ed), each null or an owned
                // reference to an IMFActivate that we take over.
                unsafe {
                    let p = *list.add(i);
                    if !p.is_null() {
                        out.push(IMFActivate::from_raw(p));
                    }
                }
            }
            // SAFETY: frees the array MFTEnum allocated with CoTaskMemAlloc (its entries are owned
            // by `out` now; any beyond 256 are leaked, which cannot happen in practice).
            unsafe { CoTaskMemFree(Some(list as *const c_void)) };
        }
        Ok(out)
    }
}

/// A string attribute of an attribute store.
pub fn attr_string(attrs: &IMFAttributes, key: &GUID) -> Option<String> {
    // SAFETY: plain attribute reads into a buffer sized from GetStringLength.
    unsafe {
        let len = attrs.GetStringLength(key).ok()?;
        if len > 4096 {
            return None;
        }
        let mut buf = vec![0u16; len as usize + 1];
        let mut got = 0u32;
        attrs.GetString(key, &mut buf, Some(&mut got)).ok()?;
        Some(String::from_utf16_lossy(buf.get(..got as usize).unwrap_or(&[])))
    }
}

/// Packs two u32 into an MF "ratio"/"size" u64 attribute.
pub fn pack(hi: u32, lo: u32) -> u64 {
    (u64::from(hi) << 32) | u64::from(lo)
}
