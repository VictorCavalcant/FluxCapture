//! GPUs, monitors and windows.

use std::collections::HashMap;

use windows::Win32::Devices::Display::{
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME, DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO,
    DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME, DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes,
    QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
};
use windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIFactory1, IDXGIOutput6};
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplayMonitors, EnumDisplaySettingsW, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::System::Threading::{GetCurrentProcessId, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GWL_EXSTYLE, GetWindow, GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow,
    IsWindowVisible, MONITORINFOF_PRIMARY, WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
};
use windows::core::{BOOL, PCWSTR, PWSTR};

use super::{Context, wide_to_string};
use crate::{GpuInfo, GpuVendor, MonitorInfo, PlatformError, WindowInfo};

pub(crate) fn luid_u64(l: windows::Win32::Foundation::LUID) -> u64 {
    (u64::from(l.HighPart as u32) << 32) | u64::from(l.LowPart)
}

pub(crate) fn luid_from_u64(v: u64) -> windows::Win32::Foundation::LUID {
    windows::Win32::Foundation::LUID { LowPart: v as u32, HighPart: (v >> 32) as i32 }
}

/// Every adapter, the system default first.
pub fn list_gpus() -> crate::Result<Vec<GpuInfo>> {
    // SAFETY: plain factory creation.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.ctx("cannot create a DXGI factory")?;
    let mut out = Vec::new();
    for i in 0..64u32 {
        // SAFETY: enumeration stops at DXGI_ERROR_NOT_FOUND.
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(i) }) else { break };
        // SAFETY: plain getter on a live adapter.
        let Ok(desc) = (unsafe { adapter.GetDesc1() }) else { continue };
        out.push(GpuInfo {
            luid: luid_u64(desc.AdapterLuid),
            name: wide_to_string(&desc.Description),
            vendor: GpuVendor::from_pci(desc.VendorId),
            dedicated_video_memory: desc.DedicatedVideoMemory as u64,
            shared_system_memory: desc.SharedSystemMemory as u64,
            software: (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0,
            index: i,
        });
    }
    Ok(out)
}

struct OutputFacts {
    gpu_luid: u64,
    hdr: bool,
}

/// DXGI facts per GDI device name: the adapter driving each output and whether HDR is on.
fn output_facts() -> HashMap<String, OutputFacts> {
    let mut map = HashMap::new();
    // SAFETY: plain factory creation.
    let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else { return map };
    for i in 0..64u32 {
        // SAFETY: enumeration stops at DXGI_ERROR_NOT_FOUND.
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(i) }) else { break };
        // SAFETY: plain getter.
        let Ok(adesc) = (unsafe { adapter.GetDesc1() }) else { continue };
        for j in 0..32u32 {
            // SAFETY: enumeration stops at DXGI_ERROR_NOT_FOUND.
            let Ok(output) = (unsafe { adapter.EnumOutputs(j) }) else { break };
            // SAFETY: plain getter.
            let Ok(odesc) = (unsafe { output.GetDesc() }) else { continue };
            let hdr = windows::core::Interface::cast::<IDXGIOutput6>(&output)
                .ok()
                // SAFETY: plain getter on IDXGIOutput6.
                .and_then(|o6| unsafe { o6.GetDesc1() }.ok())
                .is_some_and(|d| d.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020);
            map.insert(wide_to_string(&odesc.DeviceName), OutputFacts { gpu_luid: luid_u64(adesc.AdapterLuid), hdr });
        }
    }
    map
}

/// Friendly monitor names (EDID) per GDI device name, from the display configuration API.
fn friendly_names() -> HashMap<String, String> {
    let mut map = HashMap::new();
    let (mut np, mut nm) = (0u32, 0u32);
    // SAFETY: two live out-locals.
    if unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm) } != ERROR_SUCCESS || np > 1024 || nm > 4096 {
        return map;
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
    // SAFETY: the arrays have the sizes passed in `np` / `nm`, which the call may lower.
    if unsafe { QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), None) } != ERROR_SUCCESS {
        return map;
    }
    for path in paths.iter().take(np as usize) {
        let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                adapterId: path.sourceInfo.adapterId,
                id: path.sourceInfo.id,
            },
            ..Default::default()
        };
        let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
                size: std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
                adapterId: path.targetInfo.adapterId,
                id: path.targetInfo.id,
            },
            ..Default::default()
        };
        // SAFETY: each packet starts with a header whose size matches the structure passed.
        let ok = unsafe { DisplayConfigGetDeviceInfo(&mut source.header) == 0 && DisplayConfigGetDeviceInfo(&mut target.header) == 0 };
        if ok {
            let name = wide_to_string(&target.monitorFriendlyDeviceName);
            if !name.is_empty() {
                map.insert(wide_to_string(&source.viewGdiDeviceName), name);
            }
        }
    }
    map
}

struct RawMonitor {
    handle: HMONITOR,
    id: String,
    rect: RECT,
    primary: bool,
}

fn raw_monitors() -> Vec<RawMonitor> {
    unsafe extern "system" fn cb(h: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        // SAFETY: `data` is the `&mut Vec<RawMonitor>` passed by `raw_monitors`, alive for the
        // whole enumeration.
        let list = unsafe { &mut *(data.0 as *mut Vec<RawMonitor>) };
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        // SAFETY: `info` is a MONITORINFOEXW with cbSize set, accepted through the base pointer.
        if unsafe { GetMonitorInfoW(h, (&mut info as *mut MONITORINFOEXW).cast::<MONITORINFO>()) }.as_bool() {
            list.push(RawMonitor {
                handle: h,
                id: wide_to_string(&info.szDevice),
                rect: info.monitorInfo.rcMonitor,
                primary: (info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY) != 0,
            });
        }
        BOOL(1)
    }
    let mut list: Vec<RawMonitor> = Vec::new();
    // SAFETY: the callback only touches `list` through the LPARAM, which outlives the call.
    let _ = unsafe { EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut list as *mut Vec<RawMonitor> as isize)) };
    list
}

/// Every monitor, the primary first.
pub fn list_monitors() -> crate::Result<Vec<MonitorInfo>> {
    let facts = output_facts();
    let names = friendly_names();
    let mut out: Vec<MonitorInfo> = raw_monitors()
        .into_iter()
        .enumerate()
        .map(|(i, m)| {
            let wide = super::to_wide(&m.id);
            let mut dm = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
            // SAFETY: `wide` is a NUL-terminated device name; `dm` has its size set.
            let refresh = if unsafe { EnumDisplaySettingsW(PCWSTR(wide.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm) }.as_bool() {
                f64::from(dm.dmDisplayFrequency)
            } else {
                60.0
            };
            let (mut dx, mut dy) = (96u32, 96u32);
            // SAFETY: a monitor handle from the enumeration and two live out-locals.
            let scale = if unsafe { GetDpiForMonitor(m.handle, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) }.is_ok() { dx as f32 / 96.0 } else { 1.0 };
            let fact = facts.get(&m.id);
            MonitorInfo {
                name: names.get(&m.id).cloned().unwrap_or_else(|| format!("Monitor {}", i + 1)),
                x: m.rect.left,
                y: m.rect.top,
                width: (m.rect.right - m.rect.left).max(0) as u32,
                height: (m.rect.bottom - m.rect.top).max(0) as u32,
                primary: m.primary,
                refresh_hz: if refresh > 1.0 { refresh } else { 60.0 },
                hdr: fact.is_some_and(|f| f.hdr),
                gpu_luid: fact.map_or(0, |f| f.gpu_luid),
                scale,
                id: m.id,
            }
        })
        .collect();
    out.sort_by_key(|m| (!m.primary, m.x, m.y));
    Ok(out)
}

/// The monitor handle for an id (`""` = primary).
pub(crate) fn monitor_handle(id: &str) -> crate::Result<(HMONITOR, RECT)> {
    let list = raw_monitors();
    list.iter()
        .find(|m| if id.is_empty() { m.primary } else { m.id == id })
        .or_else(|| if id.is_empty() { list.first() } else { None })
        .map(|m| (m.handle, m.rect))
        .ok_or_else(|| PlatformError(format!("monitor {id} is not connected")))
}

pub(crate) fn process_name(pid: u32) -> String {
    // SAFETY: opening with the least privilege that allows querying the image name.
    let Ok(h) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else { return String::new() };
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` holds `len` u16s; the handle is closed right after.
    let ok = unsafe { QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len) }.is_ok();
    // SAFETY: closes the handle opened above.
    let _ = unsafe { CloseHandle(h) };
    if !ok {
        return String::new();
    }
    let path = String::from_utf16_lossy(buf.get(..len as usize).unwrap_or(&[]));
    path.rsplit(['\\', '/']).next().unwrap_or("").to_string()
}

fn window_info(hwnd: HWND) -> Option<WindowInfo> {
    // SAFETY: all calls below are plain queries on a window handle from EnumWindows; a window that
    // vanished meanwhile only makes them fail.
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return None;
        }
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        let owned = GetWindow(hwnd, GW_OWNER).is_ok_and(|o| !o.is_invalid());
        if (ex & WS_EX_TOOLWINDOW.0) != 0 || (ex & WS_EX_NOACTIVATE.0) != 0 || (owned && (ex & WS_EX_APPWINDOW.0) == 0) {
            return None;
        }
        let mut cloaked = 0u32;
        if DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, (&mut cloaked as *mut u32).cast(), 4).is_ok() && cloaked != 0 {
            return None;
        }
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return None;
        }
        let mut title = vec![0u16; (len as usize).min(1024) + 1];
        let n = GetWindowTextW(hwnd, &mut title);
        let title = String::from_utf16_lossy(title.get(..n.max(0) as usize).unwrap_or(&[]));
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == GetCurrentProcessId() {
            return None;
        }
        let mut r = RECT::default();
        let _ = DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS, (&mut r as *mut RECT).cast(), std::mem::size_of::<RECT>() as u32);
        Some(WindowInfo {
            handle: hwnd.0 as usize as u64,
            title,
            process: process_name(pid),
            pid,
            width: (r.right - r.left).max(0) as u32,
            height: (r.bottom - r.top).max(0) as u32,
            minimized: IsIconic(hwnd).as_bool(),
        })
    }
}

/// Capturable top-level windows (visible, titled, not tool windows, not cloaked, not ours), in
/// Z-order.
pub fn list_windows() -> crate::Result<Vec<WindowInfo>> {
    unsafe extern "system" fn cb(hwnd: HWND, data: LPARAM) -> BOOL {
        // SAFETY: `data` is the `&mut Vec<HWND>` passed below, alive for the whole enumeration.
        let list = unsafe { &mut *(data.0 as *mut Vec<HWND>) };
        if list.len() < 4096 {
            list.push(hwnd);
        }
        BOOL(1)
    }
    let mut handles: Vec<HWND> = Vec::new();
    // SAFETY: the callback only touches `handles` through the LPARAM, which outlives the call.
    unsafe { EnumWindows(Some(cb), LPARAM(&mut handles as *mut Vec<HWND> as isize)) }.ctx("EnumWindows")?;
    Ok(handles.into_iter().filter_map(window_info).collect())
}

/// Resolves a window target: the handle if it still exists, else the first window of `process`
/// whose title contains `title` (apps that were restarted get a new handle).
pub(crate) fn resolve_window(handle: u64, title: &str, process: &str) -> crate::Result<HWND> {
    let hwnd = HWND(handle as usize as *mut _);
    // SAFETY: IsWindow accepts any value and reports whether it names a window.
    if handle != 0 && unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return Ok(hwnd);
    }
    let list = list_windows()?;
    let found = list
        .iter()
        .find(|w| !process.is_empty() && w.process.eq_ignore_ascii_case(process) && (title.is_empty() || w.title.contains(title)))
        .or_else(|| list.iter().find(|w| !process.is_empty() && w.process.eq_ignore_ascii_case(process)))
        .or_else(|| list.iter().find(|w| !title.is_empty() && w.title == title));
    found.map(|w| HWND(w.handle as usize as *mut _)).ok_or_else(|| PlatformError(format!("the window \"{title}\" ({process}) is not open")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumerates() {
        let gpus = list_gpus().unwrap();
        assert!(!gpus.is_empty());
        let monitors = list_monitors().unwrap();
        assert!(!monitors.is_empty());
        assert!(monitors[0].primary);
        assert!(monitors.iter().all(|m| m.width > 0 && m.height > 0));
        assert!(monitor_handle("").is_ok());
        assert!(monitor_handle("\\\\.\\NOPE").is_err());
        let _ = list_windows().unwrap();
        assert!(resolve_window(0, "", "").is_err());
    }
}
