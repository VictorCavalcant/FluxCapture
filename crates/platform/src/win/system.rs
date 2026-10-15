//! System information and small OS services.

use std::path::{Path, PathBuf};

use fluxcapture_time::Hns;
use windows::Win32::Foundation::{FILETIME, HLOCAL, HWND, LPARAM, LocalFree, WPARAM};
use windows::Win32::Security::Cryptography::{CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData};
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX, OSVERSIONINFOW};
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes, GetSystemTimes};
use windows::Win32::UI::Shell::{FOLDERID_RoamingAppData, FOLDERID_Videos, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIconFromResourceEx, GetSystemMetrics, ICON_BIG, ICON_SMALL, LR_DEFAULTCOLOR, SM_CXICON, SM_CXSMICON, SendMessageW, SetWindowDisplayAffinity,
    WDA_EXCLUDEFROMCAPTURE, WDA_NONE, WM_SETICON,
};
use windows::core::{PCWSTR, s, w};

use super::{Context, to_wide};
use crate::{PlatformError, ProcessUsage, SystemTimes};

/// Bytes available to this user on the volume holding `path` (the nearest existing ancestor is
/// used if `path` does not exist yet).
pub fn disk_free(path: &Path) -> crate::Result<u64> {
    let mut p = path.to_path_buf();
    while !p.exists() {
        if !p.pop() {
            return Err(PlatformError(format!("no existing folder in {}", path.display())));
        }
    }
    let wide = to_wide(&p.to_string_lossy());
    let mut free = 0u64;
    // SAFETY: `wide` is a NUL-terminated path that outlives the call; the out-pointer is a live local.
    unsafe { GetDiskFreeSpaceExW(PCWSTR(wide.as_ptr()), Some(&mut free), None, None) }.ctx("cannot read the free disk space")?;
    Ok(free)
}

fn filetime_hns(f: FILETIME) -> Hns {
    Hns(((u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime)).min(i64::MAX as u64) as i64)
}

/// CPU time and memory of this process.
pub fn process_usage() -> crate::Result<ProcessUsage> {
    let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    // SAFETY: the pseudo-handle of this process and four live out-locals.
    unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) }.ctx("GetProcessTimes")?;
    let mut mem = PROCESS_MEMORY_COUNTERS_EX { cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32, ..Default::default() };
    // SAFETY: `mem` is a PROCESS_MEMORY_COUNTERS_EX whose size is passed in `cb`; the API accepts the
    // extended structure through the base pointer type.
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), (&mut mem as *mut PROCESS_MEMORY_COUNTERS_EX).cast::<PROCESS_MEMORY_COUNTERS>(), mem.cb) };
    let (ws, private) = if ok.as_bool() { (mem.WorkingSetSize as u64, mem.PrivateUsage as u64) } else { (0, 0) };
    Ok(ProcessUsage { cpu_time: filetime_hns(k) + filetime_hns(u), working_set: ws, private_bytes: private })
}

/// System-wide idle / kernel / user times (kernel includes idle).
pub fn system_times() -> crate::Result<SystemTimes> {
    let (mut i, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default());
    // SAFETY: three live out-locals.
    unsafe { GetSystemTimes(Some(&mut i), Some(&mut k), Some(&mut u)) }.ctx("GetSystemTimes")?;
    Ok(SystemTimes { idle: filetime_hns(i), kernel: filetime_hns(k), user: filetime_hns(u) })
}

/// Physical memory: (total, available) bytes.
pub fn memory_status() -> (u64, u64) {
    let mut m = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    // SAFETY: `m` is a live MEMORYSTATUSEX with its length set.
    if unsafe { GlobalMemoryStatusEx(&mut m) }.is_ok() { (m.ullTotalPhys, m.ullAvailPhys) } else { (0, 0) }
}

/// Logical processors.
pub fn cpu_count() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

pub(crate) fn known_folder(id: &windows::core::GUID) -> Option<PathBuf> {
    // SAFETY: a valid KNOWNFOLDERID; the returned string is freed with CoTaskMemFree below.
    let p = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None) }.ok()?;
    // SAFETY: `p` is a valid NUL-terminated wide string returned by the shell.
    let s = unsafe { p.to_string() }.ok();
    // SAFETY: frees the string allocated by SHGetKnownFolderPath.
    unsafe { CoTaskMemFree(Some(p.0 as *const _)) };
    s.map(PathBuf::from)
}

/// The user's Videos folder.
pub fn videos_dir() -> Option<PathBuf> {
    known_folder(&FOLDERID_Videos)
}

/// `%APPDATA%`.
pub fn config_dir() -> Option<PathBuf> {
    known_folder(&FOLDERID_RoamingAppData)
}

/// Keeps a window of ours out of every screen capture (Windows 10 2004+; older systems ignore it,
/// which is reported as an error the caller may ignore).
pub fn exclude_window_from_capture(hwnd: u64, exclude: bool) -> crate::Result<()> {
    let affinity = if exclude { WDA_EXCLUDEFROMCAPTURE } else { WDA_NONE };
    // SAFETY: the handle comes from our own window (winit); a stale handle only makes the call fail.
    unsafe { SetWindowDisplayAffinity(HWND(hwnd as usize as *mut _), affinity) }.ctx("SetWindowDisplayAffinity")
}

/// Pixel sizes Windows wants for a window's (big, small) icons: taskbar / Alt+Tab and title bar.
pub fn window_icon_sizes() -> (u32, u32) {
    // SAFETY: plain getters with no side effects.
    let (big, small) = unsafe { (GetSystemMetrics(SM_CXICON), GetSystemMetrics(SM_CXSMICON)) };
    let fit = |v: i32, fallback: u32| u32::try_from(v).ok().filter(|v| (8..=256).contains(v)).unwrap_or(fallback);
    (fit(big, 32), fit(small, 16))
}

/// Sets a window's big (taskbar, Alt+Tab) and small (title bar) icons. Each image is one icon
/// entry as stored in an `.ico` file (a 32-bit DIB with its AND mask, or a PNG) of the given size.
pub fn set_window_icon(hwnd: u64, big: (&[u8], u32), small: (&[u8], u32)) -> crate::Result<()> {
    let hwnd = HWND(hwnd as usize as *mut _);
    for (kind, (bits, size)) in [(ICON_BIG, big), (ICON_SMALL, small)] {
        if bits.len() < 40 || u32::try_from(bits.len()).is_err() {
            return Err(PlatformError("invalid icon image".into()));
        }
        let size = i32::try_from(size).map_err(|_| "invalid icon size")?;
        // SAFETY: `bits` is a complete icon image that outlives the call (length checked to fit a
        // u32); 0x00030000 is the only valid format version.
        let icon = unsafe { CreateIconFromResourceEx(bits, true, 0x0003_0000, size, size, LR_DEFAULTCOLOR) }.ctx("CreateIconFromResourceEx")?;
        // SAFETY: a handle of our own window (a stale one only makes the message go nowhere). The
        // icon is deliberately never destroyed: the window keeps using it for the process lifetime.
        unsafe { SendMessageW(hwnd, WM_SETICON, Some(WPARAM(kind as usize)), Some(LPARAM(icon.0 as isize))) };
    }
    Ok(())
}

/// Encrypts a secret for the current Windows user (DPAPI); stream keys are stored this way.
pub fn protect_secret(plain: &[u8]) -> crate::Result<Vec<u8>> {
    let input = CRYPT_INTEGER_BLOB { cbData: u32::try_from(plain.len()).map_err(|_| "secret too large")?, pbData: plain.as_ptr() as *mut u8 };
    let mut out = CRYPT_INTEGER_BLOB::default();
    // SAFETY: `input` points at `plain` for the duration of the call; `out` receives a LocalAlloc'ed
    // buffer that is copied and freed below.
    unsafe { CryptProtectData(&input, w!("FluxCapture"), None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out) }.ctx("cannot protect the secret")?;
    Ok(take_blob(out))
}

/// Decrypts a secret made by [`protect_secret`].
pub fn unprotect_secret(sealed: &[u8]) -> crate::Result<Vec<u8>> {
    let input = CRYPT_INTEGER_BLOB { cbData: u32::try_from(sealed.len()).map_err(|_| "secret too large")?, pbData: sealed.as_ptr() as *mut u8 };
    let mut out = CRYPT_INTEGER_BLOB::default();
    // SAFETY: as in `protect_secret`.
    unsafe { CryptUnprotectData(&input, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out) }.ctx("cannot read the protected secret")?;
    Ok(take_blob(out))
}

fn take_blob(b: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    if b.pbData.is_null() {
        return Vec::new();
    }
    // SAFETY: DPAPI returned `cbData` readable bytes at `pbData`.
    let v = unsafe { std::slice::from_raw_parts(b.pbData, b.cbData as usize) }.to_vec();
    // SAFETY: the buffer was allocated by DPAPI with LocalAlloc.
    unsafe { LocalFree(Some(HLOCAL(b.pbData.cast()))) };
    v
}

/// Windows version `(major, minor, build)` from `RtlGetVersion` (not subject to manifest lies).
pub fn os_version() -> (u32, u32, u32) {
    type RtlGetVersion = unsafe extern "system" fn(*mut OSVERSIONINFOW) -> i32;
    // SAFETY: ntdll.dll is always loaded; the symbol has the documented signature.
    let f = unsafe {
        let Ok(ntdll) = GetModuleHandleW(w!("ntdll.dll")) else { return (10, 0, 0) };
        GetProcAddress(ntdll, s!("RtlGetVersion"))
    };
    let Some(f) = f else { return (10, 0, 0) };
    // SAFETY: the function pointer has the RtlGetVersion signature (documented, stable since NT).
    let f: RtlGetVersion = unsafe { std::mem::transmute(f) };
    let mut info = OSVERSIONINFOW { dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32, ..Default::default() };
    // SAFETY: `info` is a live structure with its size set.
    let status = unsafe { f(&mut info) };
    if status != 0 {
        return (10, 0, 0);
    }
    (info.dwMajorVersion, info.dwMinorVersion, info.dwBuildNumber)
}

/// "Windows 11 (build 26100)".
pub fn os_description() -> String {
    let (major, minor, build) = os_version();
    let name = if major == 10 && build >= 22000 { "Windows 11".to_string() } else { format!("Windows {major}.{minor}") };
    let name = if major == 10 && build < 22000 { "Windows 10".to_string() } else { name };
    format!("{name} (build {build})")
}

/// Local wall-clock time: (year, month, day, hour, minute, second).
pub fn local_time() -> (i32, u32, u32, u32, u32, u32) {
    // SAFETY: plain query.
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    (i32::from(t.wYear), u32::from(t.wMonth), u32::from(t.wDay), u32::from(t.wHour), u32::from(t.wMinute), u32::from(t.wSecond))
}

/// Shows a file selected in Explorer (or opens a folder).
pub fn reveal_in_explorer(path: &Path) -> crate::Result<()> {
    let mut cmd = std::process::Command::new("explorer.exe");
    if path.is_dir() {
        cmd.arg(path);
    } else {
        cmd.arg(format!("/select,{}", path.display()));
    }
    cmd.spawn().map(|_| ()).map_err(|e| PlatformError(format!("cannot open Explorer: {e}")))
}

/// Opens a file with its default application.
pub fn open_with_default_app(path: &Path) -> crate::Result<()> {
    std::process::Command::new("explorer.exe").arg(path).spawn().map(|_| ()).map_err(|e| PlatformError(format!("cannot open {}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basics() {
        assert!(disk_free(Path::new("C:\\")).unwrap() > 0);
        assert!(disk_free(Path::new("C:\\definitely\\missing\\folder")).unwrap() > 0);
        let u = process_usage().unwrap();
        assert!(u.working_set > 0);
        assert!(system_times().unwrap().kernel.0 > 0);
        assert!(videos_dir().is_some());
        let (major, _, build) = os_version();
        assert!(major >= 10 && build > 0);
        let sealed = protect_secret(b"stream-key").unwrap();
        assert_ne!(sealed, b"stream-key");
        assert_eq!(unprotect_secret(&sealed).unwrap(), b"stream-key");
        assert!(unprotect_secret(b"garbage").is_err());
    }
}
