//! COM initialisation per thread.

use std::cell::Cell;

use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

thread_local! {
    static INITIALISED: Cell<bool> = const { Cell::new(false) };
}

/// Joins the multithreaded apartment on this thread (once). A thread that is already in a
/// single-threaded apartment (the UI thread, where winit initialises OLE) is fine as it is.
/// COM is never uninitialised: the threads that use it live until they end.
pub fn ensure_com() -> crate::Result<()> {
    if INITIALISED.with(Cell::get) {
        return Ok(());
    }
    // SAFETY: plain COM initialisation of the calling thread; no reserved pointer.
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if hr.is_ok() || hr == RPC_E_CHANGED_MODE {
        INITIALISED.with(|c| c.set(true));
        Ok(())
    } else {
        Err(crate::PlatformError(format!("COM initialisation failed (0x{:08X})", hr.0 as u32)))
    }
}
