//! What an installer needs from Windows: per-user folders, shell shortcuts and the entry in
//! Settings › Apps (`HKCU\…\Uninstall`). Everything is per user: no administrator rights needed.

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::Globalization::GetUserDefaultUILanguage;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IPersistFile};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, REG_DWORD, REG_SZ, RRF_RT_REG_SZ, RegDeleteTreeW, RegGetValueW, RegSetKeyValueW};
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_Programs, FOLDERID_UserProgramFiles, IShellLinkW, SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify, ShellLink,
};
use windows::Win32::UI::WindowsAndMessaging::{IDOK, MB_ICONERROR, MB_ICONINFORMATION, MB_ICONQUESTION, MB_OKCANCEL, MB_SETFOREGROUND, MessageBoxW};
use windows::core::{Interface, PCWSTR};

use super::system::known_folder;
use super::{Context, ensure_com, to_wide};
use crate::PlatformError;

/// `%LOCALAPPDATA%\Programs`: where per-user applications are installed.
pub fn user_programs_dir() -> Option<PathBuf> {
    known_folder(&FOLDERID_UserProgramFiles)
}

/// The user's Start menu "Programs" folder.
pub fn start_menu_dir() -> Option<PathBuf> {
    known_folder(&FOLDERID_Programs)
}

/// The user's desktop.
pub fn desktop_dir() -> Option<PathBuf> {
    known_folder(&FOLDERID_Desktop)
}

/// True when the user's display language is Portuguese (any region).
pub fn ui_language_is_portuguese() -> bool {
    // SAFETY: plain query with no arguments.
    let lang = unsafe { GetUserDefaultUILanguage() };
    lang & 0x3FF == 0x16 // LANG_PORTUGUESE
}

/// Creates (or replaces) a `.lnk` shortcut at `link` that starts `target` in its own folder, with
/// `target`'s first icon.
pub fn create_shortcut(link: &Path, target: &Path, description: &str) -> crate::Result<()> {
    ensure_com()?;
    let target_w = to_wide(&target.to_string_lossy());
    let dir_w = to_wide(&target.parent().unwrap_or(Path::new("")).to_string_lossy());
    let desc_w = to_wide(description);
    let link_w = to_wide(&link.to_string_lossy());
    // SAFETY: COM is initialised on this thread; ShellLink is the documented in-process class.
    let shell_link: IShellLinkW = unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) }.ctx("cannot create a shortcut")?;
    // SAFETY: every string is NUL-terminated and outlives the call that reads it.
    unsafe {
        shell_link.SetPath(PCWSTR(target_w.as_ptr())).ctx("shortcut target")?;
        shell_link.SetWorkingDirectory(PCWSTR(dir_w.as_ptr())).ctx("shortcut folder")?;
        shell_link.SetDescription(PCWSTR(desc_w.as_ptr())).ctx("shortcut description")?;
        shell_link.SetIconLocation(PCWSTR(target_w.as_ptr()), 0).ctx("shortcut icon")?;
    }
    let file: IPersistFile = shell_link.cast().ctx("cannot save the shortcut")?;
    // SAFETY: `link_w` is a NUL-terminated absolute path that outlives the call.
    unsafe { file.Save(PCWSTR(link_w.as_ptr()), true) }.ctx(&format!("cannot save {}", link.display()))
}

/// Asks Explorer to refresh icons (after shortcuts were added or removed).
pub fn refresh_shell() {
    // SAFETY: a broadcast notification without item pointers.
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
}

/// What a [`message_box`] says.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MessageKind {
    Info,
    Error,
    /// OK / Cancel.
    Question,
}

/// A plain Windows message box (for when no window can be opened). Returns true for OK.
pub fn message_box(title: &str, text: &str, kind: MessageKind) -> bool {
    let (title_w, text_w) = (to_wide(title), to_wide(text));
    let style = match kind {
        MessageKind::Info => MB_ICONINFORMATION,
        MessageKind::Error => MB_ICONERROR,
        MessageKind::Question => MB_OKCANCEL | MB_ICONQUESTION,
    } | MB_SETFOREGROUND;
    // SAFETY: both strings are NUL-terminated and outlive the (modal) call; no owner window.
    let pressed = unsafe { MessageBoxW(None, PCWSTR(text_w.as_ptr()), PCWSTR(title_w.as_ptr()), style) };
    pressed == IDOK
}

/// A value of an uninstall entry.
pub enum RegValue<'a> {
    Text(&'a str),
    Number(u32),
}

const UNINSTALL_ROOT: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\";

fn uninstall_key(id: &str) -> crate::Result<Vec<u16>> {
    if id.is_empty() || id.contains(['\\', '/']) {
        return Err(PlatformError(format!("invalid uninstall entry name {id:?}")));
    }
    Ok(to_wide(&format!("{UNINSTALL_ROOT}{id}")))
}

fn check(e: WIN32_ERROR, what: &str) -> crate::Result<()> {
    if e == ERROR_SUCCESS { Ok(()) } else { Err(PlatformError(format!("{what}: Windows error {}", e.0))) }
}

/// Writes the entry `id` of Settings › Apps for the current user (created if missing).
pub fn write_uninstall_entry(id: &str, values: &[(&str, RegValue)]) -> crate::Result<()> {
    let key = uninstall_key(id)?;
    for (name, value) in values {
        let name_w = to_wide(name);
        let e = match value {
            RegValue::Text(s) => {
                let data = to_wide(s);
                let bytes = u32::try_from(data.len() * 2).map_err(|_| "registry value too large")?;
                // SAFETY: `data` holds `bytes` bytes (NUL included) and outlives the call.
                unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr()), PCWSTR(name_w.as_ptr()), REG_SZ.0, Some(data.as_ptr().cast()), bytes) }
            }
            RegValue::Number(n) => {
                // SAFETY: `n` is 4 readable bytes for the duration of the call.
                unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr()), PCWSTR(name_w.as_ptr()), REG_DWORD.0, Some((n as *const u32).cast()), 4) }
            }
        };
        check(e, &format!("cannot write the uninstall entry ({name})"))?;
    }
    Ok(())
}

/// A text value of the uninstall entry `id`, or `None` if the entry or value does not exist.
pub fn read_uninstall_value(id: &str, name: &str) -> Option<String> {
    let key = uninstall_key(id).ok()?;
    let name_w = to_wide(name);
    let mut buf = vec![0u16; 1024];
    let mut bytes = u32::try_from(buf.len() * 2).ok()?;
    // SAFETY: `buf` has room for `bytes` bytes; RRF_RT_REG_SZ guarantees NUL termination.
    let e = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr()), PCWSTR(name_w.as_ptr()), RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr().cast()), Some(&mut bytes))
    };
    (e == ERROR_SUCCESS).then(|| super::wide_to_string(&buf))
}

/// Removes the uninstall entry `id` (nothing to do if it does not exist).
pub fn delete_uninstall_entry(id: &str) -> crate::Result<()> {
    let key = uninstall_key(id)?;
    // SAFETY: `key` is a NUL-terminated subkey path below HKCU.
    let e = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr())) };
    if e == ERROR_FILE_NOT_FOUND { Ok(()) } else { check(e, "cannot remove the uninstall entry") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_exist() {
        assert!(start_menu_dir().is_some_and(|p| p.is_absolute()));
        assert!(desktop_dir().is_some_and(|p| p.is_absolute()));
        assert!(user_programs_dir().is_some_and(|p| p.is_absolute()));
    }

    #[test]
    fn hostile_entry_names_are_refused() {
        assert!(uninstall_key("").is_err());
        assert!(uninstall_key(r"..\Run").is_err());
        assert!(write_uninstall_entry("a/b", &[]).is_err());
    }

    #[test]
    fn uninstall_entry_round_trip() {
        let id = format!("FluxCaptureTest{}", std::process::id());
        write_uninstall_entry(&id, &[("DisplayName", RegValue::Text("Teste ção")), ("NoModify", RegValue::Number(1))]).unwrap();
        assert_eq!(read_uninstall_value(&id, "DisplayName").as_deref(), Some("Teste ção"));
        assert_eq!(read_uninstall_value(&id, "Missing"), None);
        delete_uninstall_entry(&id).unwrap();
        assert_eq!(read_uninstall_value(&id, "DisplayName"), None);
        delete_uninstall_entry(&id).unwrap();
    }

    #[test]
    fn shortcut_is_written() {
        let dir = std::env::temp_dir().join(format!("fluxcapture-lnk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let link = dir.join("Test.lnk");
        create_shortcut(&link, Path::new(r"C:\Windows\notepad.exe"), "test").unwrap();
        assert!(std::fs::metadata(&link).unwrap().len() > 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
