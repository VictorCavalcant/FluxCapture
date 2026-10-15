//! Operating-system integration for FluxCapture.
//!
//! Everything that talks to the OS lives here, behind a safe, `Result`-returning API:
//!
//! - [`clock`]: the QPC clock every media timestamp uses, a high-resolution frame timer and MMCSS
//!   thread scheduling.
//! - [`display`]: GPUs, monitors and windows.
//! - [`video`]: the video engine — capture sources (Windows.Graphics.Capture / DXGI Desktop
//!   Duplication), the Direct3D 11 scene compositor, the RGB → NV12 converter and the Media
//!   Foundation encoder, all on the GPU (frames never cross to system memory on the recording path).
//! - [`audio`]: WASAPI capture (loopback, microphones, per-process) and the AAC encoder.
//! - [`hotkeys`]: global hotkeys.
//! - [`system`]: disk space, CPU / memory usage, known folders, capture exclusion, DPAPI.
//! - [`install`]: per-user install support for the setup program (shortcuts, Settings › Apps entry).
//!
//! This is the only crate allowed `unsafe` (AGENTS.md §0.3). Other operating systems get a stub
//! backend that reports "unsupported", so the rest of the workspace builds and tests everywhere.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod types;
pub use types::*;

#[cfg(windows)]
#[allow(unsafe_code)]
mod win;
#[cfg(windows)]
pub use win::{audio, clock, display, hotkeys, install, system, video};

#[cfg(not(windows))]
mod stub;
#[cfg(not(windows))]
pub use stub::{audio, clock, display, hotkeys, install, system, video};

/// Runs `f`, turning a panic into an error message (worker threads use this so that a bug in one
/// stage reports an error instead of taking the recording down; AGENTS.md §0.6).
pub fn guarded<T>(what: &str, f: impl FnOnce() -> Result<T>) -> Result<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            log::error!("{what} panicked: {msg}");
            Err(PlatformError(format!("internal error in {what}: {msg}")))
        }
    }
}
