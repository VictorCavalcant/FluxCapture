//! Global hotkeys (`RegisterHotKey`) on a dedicated message-loop thread.

use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;

use crossbeam_channel::{Sender, bounded};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey, UnregisterHotKey,
};
use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, PM_NOREMOVE, PeekMessageW, PostThreadMessageW, WM_HOTKEY, WM_QUIT, WM_USER};

use crate::{Hotkey, PlatformError};

/// Listens for a set of hotkeys; each press sends its index to the sink. Dropping it unregisters.
pub struct HotkeyListener {
    thread_id: u32,
    thread: Option<JoinHandle<()>>,
    failures: Arc<Mutex<Vec<(usize, String)>>>,
}

impl HotkeyListener {
    /// Registers `keys`. Keys another application already owns are reported by [`failures`]
    /// (the rest still work).
    ///
    /// [`failures`]: HotkeyListener::failures
    pub fn start(keys: Vec<Hotkey>, sink: Sender<usize>) -> crate::Result<HotkeyListener> {
        let (ready_tx, ready_rx) = bounded::<u32>(1);
        let failures = Arc::new(Mutex::new(Vec::new()));
        let failures2 = Arc::clone(&failures);
        let thread = std::thread::Builder::new()
            .name("fluxcapture-hotkeys".into())
            .spawn(move || {
                let _ = crate::guarded("the hotkey listener", || {
                    let mut msg = MSG::default();
                    // SAFETY: creates this thread's message queue before announcing its id.
                    unsafe {
                        let _ = PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE);
                    }
                    // SAFETY: plain query.
                    let _ = ready_tx.send(unsafe { GetCurrentThreadId() });
                    for (i, k) in keys.iter().enumerate() {
                        let mut mods = MOD_NOREPEAT;
                        for (on, m) in [(k.ctrl, MOD_CONTROL), (k.alt, MOD_ALT), (k.shift, MOD_SHIFT), (k.win, MOD_WIN)] {
                            if on {
                                mods = HOT_KEY_MODIFIERS(mods.0 | m.0);
                            }
                        }
                        // SAFETY: thread-level hotkey (no window); ids are small positive integers.
                        if let Err(e) = unsafe { RegisterHotKey(None, i as i32 + 1, mods, k.vk) } {
                            failures2.lock().unwrap_or_else(PoisonError::into_inner).push((i, format!("{} is in use by another app ({e})", k.label())));
                        }
                    }
                    // SAFETY: standard message loop on this thread's queue; ends with WM_QUIT.
                    unsafe {
                        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                            if msg.message == WM_HOTKEY {
                                let id = msg.wParam.0;
                                if id >= 1 {
                                    let _ = sink.try_send(id - 1);
                                }
                            }
                        }
                        for i in 0..keys.len() {
                            let _ = UnregisterHotKey(None, i as i32 + 1);
                        }
                    }
                    Ok(())
                });
            })
            .map_err(|e| PlatformError(format!("cannot start the hotkey thread: {e}")))?;
        let thread_id = ready_rx.recv().map_err(|_| PlatformError("the hotkey thread failed to start".into()))?;
        // Give registration a moment so `failures` is meaningful right after `start`.
        std::thread::sleep(std::time::Duration::from_millis(20));
        Ok(HotkeyListener { thread_id, thread: Some(thread), failures })
    }

    /// Hotkeys that could not be registered: (index, reason).
    pub fn failures(&self) -> Vec<(usize, String)> {
        self.failures.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

impl Drop for HotkeyListener {
    fn drop(&mut self) {
        // SAFETY: posts WM_QUIT to our own listener thread.
        let _ = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
