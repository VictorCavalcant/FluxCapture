//! The media clock (QPC in 100 ns ticks), a high-resolution frame timer and MMCSS scheduling.
//!
//! Windows.Graphics.Capture frame times (`SystemRelativeTime`), WASAPI `u64QPCPosition` and this
//! clock are the same timeline, so capture, audio and render timestamps compare directly.

use std::sync::OnceLock;

use fluxcapture_time::{HNS_PER_SEC, Hns};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{
    ABOVE_NORMAL_PRIORITY_CLASS, AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW,
    GetCurrentProcess, GetCurrentThread, HIGH_PRIORITY_CLASS, INFINITE, NORMAL_PRIORITY_CLASS, SetPriorityClass, SetThreadPriority, SetWaitableTimer,
    THREAD_PRIORITY_ABOVE_NORMAL, THREAD_PRIORITY_HIGHEST, TIMER_ALL_ACCESS, WaitForSingleObject,
};
use windows::core::{PCWSTR, w};

use super::Context;

fn qpc_frequency() -> i64 {
    static FREQ: OnceLock<i64> = OnceLock::new();
    *FREQ.get_or_init(|| {
        let mut f = 0i64;
        // SAFETY: writes the frequency into a live local; cannot fail on Windows XP and later.
        let _ = unsafe { QueryPerformanceFrequency(&mut f) };
        if f > 0 { f } else { HNS_PER_SEC }
    })
}

/// Now, on the media clock.
pub fn now() -> Hns {
    let mut c = 0i64;
    // SAFETY: writes the counter into a live local; cannot fail on Windows XP and later.
    let _ = unsafe { QueryPerformanceCounter(&mut c) };
    let f = qpc_frequency();
    if f == HNS_PER_SEC {
        return Hns(c);
    }
    Hns(i64::try_from(i128::from(c) * i128::from(HNS_PER_SEC) / i128::from(f)).unwrap_or(i64::MAX))
}

/// Sleeps until a point on the media clock with sub-millisecond precision, without spinning:
/// a high-resolution waitable timer (Windows 10 1803+), or a 1 ms system timer period on older
/// systems.
pub struct PrecisionTimer {
    handle: HANDLE,
    raised_period: bool,
}

// SAFETY: a waitable timer handle may be used from any thread.
unsafe impl Send for PrecisionTimer {}

impl PrecisionTimer {
    pub fn new() -> crate::Result<PrecisionTimer> {
        // SAFETY: creates an unnamed timer with default security.
        let hi = unsafe { CreateWaitableTimerExW(None, PCWSTR::null(), CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS.0) };
        if let Ok(handle) = hi {
            return Ok(PrecisionTimer { handle, raised_period: false });
        }
        // SAFETY: as above, without the high-resolution flag.
        let handle = unsafe { CreateWaitableTimerExW(None, PCWSTR::null(), 0, TIMER_ALL_ACCESS.0) }.ctx("cannot create a timer")?;
        // SAFETY: raises the system timer resolution; undone in Drop.
        let raised_period = unsafe { timeBeginPeriod(1) } == 0;
        Ok(PrecisionTimer { handle, raised_period })
    }

    /// Sleeps until `target` (returns at once if it has passed).
    pub fn wait_until(&self, target: Hns) {
        // Two rounds at most: the timer may fire a little early.
        for _ in 0..4 {
            let remaining = target.0.saturating_sub(now().0);
            if remaining <= 0 {
                return;
            }
            let due = -remaining; // negative = relative, in 100 ns units
            // SAFETY: `handle` is our live timer; no completion routine.
            if unsafe { SetWaitableTimer(self.handle, &due, 0, None, None, false) }.is_err() {
                std::thread::sleep(std::time::Duration::from_micros((remaining / 10).max(0) as u64));
                return;
            }
            // SAFETY: waits on our live timer handle.
            let r = unsafe { WaitForSingleObject(self.handle, INFINITE) };
            if r != WAIT_OBJECT_0 {
                return;
            }
        }
    }
}

impl Drop for PrecisionTimer {
    fn drop(&mut self) {
        // SAFETY: closes the handle this object owns; balances timeBeginPeriod.
        unsafe {
            let _ = CloseHandle(self.handle);
            if self.raised_period {
                timeEndPeriod(1);
            }
        }
    }
}

/// What a thread does, for scheduling.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ThreadKind {
    /// Frame pacing / composition (MMCSS "Capture").
    Capture,
    /// Audio capture and mixing (MMCSS "Pro Audio").
    ProAudio,
    /// Encoding / muxing (above normal priority).
    Encode,
}

/// Restores the thread's scheduling on drop.
pub struct PriorityGuard {
    mmcss: Option<HANDLE>,
}

impl Drop for PriorityGuard {
    fn drop(&mut self) {
        if let Some(h) = self.mmcss.take() {
            // SAFETY: reverts the MMCSS registration made by `boost_thread` on this thread.
            let _ = unsafe { AvRevertMmThreadCharacteristics(h) };
        }
    }
}

/// Registers the calling thread with the Multimedia Class Scheduler (so a busy game does not
/// starve frame pacing or audio), falling back to a raised thread priority.
pub fn boost_thread(kind: ThreadKind) -> PriorityGuard {
    let task = match kind {
        ThreadKind::Capture => Some(w!("Capture")),
        ThreadKind::ProAudio => Some(w!("Pro Audio")),
        ThreadKind::Encode => None,
    };
    if let Some(task) = task {
        let mut index = 0u32;
        // SAFETY: registers the current thread under a built-in MMCSS task name.
        if let Ok(h) = unsafe { AvSetMmThreadCharacteristicsW(task, &mut index) } {
            return PriorityGuard { mmcss: Some(h) };
        }
    }
    let prio = if kind == ThreadKind::Encode { THREAD_PRIORITY_ABOVE_NORMAL } else { THREAD_PRIORITY_HIGHEST };
    // SAFETY: changes the priority of the calling thread only.
    let _ = unsafe { SetThreadPriority(GetCurrentThread(), prio) };
    PriorityGuard { mmcss: None }
}

/// Process priority class.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ProcessPriority {
    Normal,
    AboveNormal,
    High,
}

/// Sets the priority class of this process.
pub fn set_process_priority(p: ProcessPriority) -> crate::Result<()> {
    let class = match p {
        ProcessPriority::Normal => NORMAL_PRIORITY_CLASS,
        ProcessPriority::AboveNormal => ABOVE_NORMAL_PRIORITY_CLASS,
        ProcessPriority::High => HIGH_PRIORITY_CLASS,
    };
    // SAFETY: changes this process's own priority class.
    unsafe { SetPriorityClass(GetCurrentProcess(), class) }.ctx("cannot change the process priority")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_is_monotonic_and_timer_waits() {
        let t = PrecisionTimer::new().unwrap();
        let a = now();
        t.wait_until(a + Hns::from_millis(5));
        let b = now();
        assert!(b - a >= Hns::from_millis(5), "{:?}", b - a);
        assert!(b - a < Hns::from_millis(50), "{:?}", b - a);
    }
}
