//! Windows backends (FFI). Every `unsafe` block carries a `// SAFETY:` comment; public items are
//! safe and return `Result`.

pub mod audio;
pub mod clock;
pub mod display;
pub mod hotkeys;
pub mod install;
pub mod system;
pub mod video;

mod com;
mod d3d;
mod mf;

pub(crate) use com::ensure_com;

use crate::PlatformError;

/// `windows::core::Error` → [`PlatformError`] with context.
pub(crate) trait Context<T> {
    fn ctx(self, what: &str) -> crate::Result<T>;
}

impl<T> Context<T> for windows::core::Result<T> {
    fn ctx(self, what: &str) -> crate::Result<T> {
        self.map_err(|e| PlatformError(format!("{what}: {} (0x{:08X})", e.message().trim(), e.code().0 as u32)))
    }
}

/// A NUL-terminated UTF-16 buffer → `String` (stops at the first NUL).
pub(crate) fn wide_to_string(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(w.get(..end).unwrap_or(&[]))
}

/// `&str` → NUL-terminated UTF-16.
pub(crate) fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
