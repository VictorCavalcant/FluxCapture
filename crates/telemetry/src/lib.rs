//! Observability: logs, metric history and diagnostics.
//!
//! - [`logger`]: a `log` backend writing a rotating file and keeping the last lines in memory for
//!   the UI; secrets (stream keys, tokens, passwords) are redacted before anything is stored.
//! - [`History`]: fixed-size ring buffers for the performance graphs.
//! - [`diagnose`]: turns a [`Snapshot`] of pipeline metrics into findings that name the stage that
//!   is the bottleneck (capture, composition, encoder, disk, audio, system), the metric that shows
//!   it, and what to do about it.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod diagnostics;
pub mod logger;

pub use diagnostics::{Area, Finding, Severity, Snapshot, Suggestion, diagnose};

use std::collections::VecDeque;

/// A bounded series of samples (oldest dropped first).
#[derive(Clone, Debug)]
pub struct History {
    cap: usize,
    data: VecDeque<f32>,
}

impl History {
    pub fn new(cap: usize) -> History {
        let cap = cap.clamp(1, 100_000);
        History { cap, data: VecDeque::with_capacity(cap) }
    }

    pub fn push(&mut self, v: f32) {
        if self.data.len() == self.cap {
            self.data.pop_front();
        }
        self.data.push_back(if v.is_finite() { v } else { 0.0 });
    }

    pub fn iter(&self) -> impl Iterator<Item = f32> + '_ {
        self.data.iter().copied()
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn last(&self) -> Option<f32> {
        self.data.back().copied()
    }

    pub fn max(&self) -> f32 {
        self.data.iter().copied().fold(0.0, f32::max)
    }

    pub fn mean(&self) -> f32 {
        if self.data.is_empty() { 0.0 } else { self.data.iter().sum::<f32>() / self.data.len() as f32 }
    }

    /// The `p`-th percentile (0..=1) of the stored samples.
    pub fn percentile(&self, p: f32) -> f32 {
        if self.data.is_empty() {
            return 0.0;
        }
        let mut v: Vec<f32> = self.data.iter().copied().collect();
        v.sort_by(f32::total_cmp);
        let idx = ((v.len() - 1) as f32 * p.clamp(0.0, 1.0)).round() as usize;
        v.get(idx).copied().unwrap_or(0.0)
    }
}

/// CPU usage from cumulative CPU-time readings (100 ns units), as a percentage of the whole
/// machine.
#[derive(Default, Debug, Clone)]
pub struct CpuMeter {
    last: Option<(i64, i64)>,
}

impl CpuMeter {
    /// `cpu_time`: process (or busy system) time so far; `wall`: wall-clock time so far, both in
    /// 100 ns; `cores`: logical processors. Returns `None` on the first call.
    pub fn sample(&mut self, cpu_time: i64, wall: i64, cores: usize) -> Option<f32> {
        let prev = self.last.replace((cpu_time, wall))?;
        let dcpu = cpu_time.saturating_sub(prev.0).max(0) as f64;
        let dwall = wall.saturating_sub(prev.1) as f64;
        if dwall <= 0.0 {
            return None;
        }
        Some((dcpu / dwall / cores.max(1) as f64 * 100.0).clamp(0.0, 100.0) as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_and_cpu() {
        let mut h = History::new(3);
        for v in [1.0, 2.0, f32::NAN, 4.0] {
            h.push(v);
        }
        assert_eq!(h.iter().collect::<Vec<_>>(), vec![2.0, 0.0, 4.0]);
        assert_eq!(h.max(), 4.0);
        assert_eq!(h.percentile(1.0), 4.0);
        assert_eq!(History::new(0).percentile(0.5), 0.0);
        let mut c = CpuMeter::default();
        assert!(c.sample(0, 0, 4).is_none());
        assert_eq!(c.sample(4_000_000, 10_000_000, 4), Some(10.0));
        assert!(c.sample(4_000_000, 10_000_000, 4).is_none());
    }
}
