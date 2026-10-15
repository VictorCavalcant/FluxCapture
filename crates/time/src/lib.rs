//! Exact media time.
//!
//! Every clock FluxCapture reads on Windows speaks the same unit: Windows.Graphics.Capture frame
//! times, WASAPI `u64QPCPosition`, Media Foundation sample times and `QueryPerformanceCounter`
//! converted by the platform layer are all 100-nanosecond ticks ([`Hns`]). Keeping one unit end to
//! end means no conversions (and no rounding) between capture, mixing, encoding and muxing.
//!
//! Frame and sample times are never accumulated: the time of frame `k` is computed from `k` with
//! 128-bit integer math ([`FrameRate::frame_time`]), so an 8-hour 59.94 fps recording has exactly the
//! same timestamps as its first second.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::fmt;
use std::ops::{Add, AddAssign, Sub, SubAssign};

use serde::{Deserialize, Serialize};

/// 100-ns ticks per second.
pub const HNS_PER_SEC: i64 = 10_000_000;
/// 100-ns ticks per millisecond.
pub const HNS_PER_MS: i64 = 10_000;

/// A time or duration in 100-nanosecond ticks. Arithmetic saturates instead of overflowing.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Hns(pub i64);

impl Hns {
    pub const ZERO: Hns = Hns(0);
    pub const MAX: Hns = Hns(i64::MAX);

    pub const fn from_secs(s: i64) -> Hns {
        Hns(s.saturating_mul(HNS_PER_SEC))
    }

    pub const fn from_millis(ms: i64) -> Hns {
        Hns(ms.saturating_mul(HNS_PER_MS))
    }

    pub const fn from_micros(us: i64) -> Hns {
        Hns(us.saturating_mul(10))
    }

    /// From seconds as a float; NaN becomes zero and out-of-range values saturate.
    pub fn from_secs_f64(s: f64) -> Hns {
        // `as` from f64 saturates and maps NaN to 0 — exactly the behaviour wanted here.
        Hns((s * HNS_PER_SEC as f64).round() as i64)
    }

    pub fn as_secs_f64(self) -> f64 {
        self.0 as f64 / HNS_PER_SEC as f64
    }

    /// Whole milliseconds, rounded to nearest (ties away from zero).
    pub const fn as_millis_rounded(self) -> i64 {
        let half = HNS_PER_MS / 2;
        if self.0 >= 0 { self.0.saturating_add(half) / HNS_PER_MS } else { self.0.saturating_sub(half) / HNS_PER_MS }
    }

    pub const fn as_millis(self) -> i64 {
        self.0 / HNS_PER_MS
    }

    pub const fn saturating_add(self, o: Hns) -> Hns {
        Hns(self.0.saturating_add(o.0))
    }

    pub const fn saturating_sub(self, o: Hns) -> Hns {
        Hns(self.0.saturating_sub(o.0))
    }

    pub const fn is_negative(self) -> bool {
        self.0 < 0
    }

    pub fn max(self, o: Hns) -> Hns {
        if self.0 >= o.0 { self } else { o }
    }

    pub fn min(self, o: Hns) -> Hns {
        if self.0 <= o.0 { self } else { o }
    }

    /// The value in units of `1/timescale` seconds, rounded to nearest. `None` for a zero timescale
    /// or a result outside `i64`.
    pub fn to_timescale(self, timescale: u32) -> Option<i64> {
        if timescale == 0 {
            return None;
        }
        rescale_round(self.0, i64::from(timescale), HNS_PER_SEC)
    }

    /// `value` units of `1/timescale` seconds as ticks, rounded to nearest. `None` for a zero
    /// timescale or overflow.
    pub fn from_timescale(value: i64, timescale: u32) -> Option<Hns> {
        rescale_round(value, HNS_PER_SEC, i64::from(timescale)).map(Hns)
    }

    /// `HH:MM:SS` (hours grow past 99 if they must), for status displays.
    pub fn hms(self) -> String {
        let total = self.0.max(0) / HNS_PER_SEC;
        format!("{:02}:{:02}:{:02}", total / 3600, (total / 60) % 60, total % 60)
    }
}

impl Add for Hns {
    type Output = Hns;
    fn add(self, o: Hns) -> Hns {
        self.saturating_add(o)
    }
}

impl Sub for Hns {
    type Output = Hns;
    fn sub(self, o: Hns) -> Hns {
        self.saturating_sub(o)
    }
}

impl AddAssign for Hns {
    fn add_assign(&mut self, o: Hns) {
        *self = self.saturating_add(o);
    }
}

impl SubAssign for Hns {
    fn sub_assign(&mut self, o: Hns) {
        *self = self.saturating_sub(o);
    }
}

impl fmt::Display for Hns {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.3} s", self.as_secs_f64())
    }
}

/// `value * mul / div` rounded to nearest with 128-bit intermediates; `None` if `div` is zero or the
/// result does not fit an `i64`.
pub fn rescale_round(value: i64, mul: i64, div: i64) -> Option<i64> {
    if div == 0 {
        return None;
    }
    let n = i128::from(value).checked_mul(i128::from(mul))?;
    let d = i128::from(div);
    let half = d.abs() / 2;
    let q = if (n >= 0) == (d > 0) { (n.checked_add(half)?) / d } else { (n.checked_sub(half)?) / d };
    i64::try_from(q).ok()
}

/// `value * mul / div` rounded toward negative infinity; `None` if `div` is not positive or overflow.
pub fn rescale_floor(value: i64, mul: i64, div: i64) -> Option<i64> {
    if div <= 0 {
        return None;
    }
    let n = i128::from(value).checked_mul(i128::from(mul))?;
    i64::try_from(n.div_euclid(i128::from(div))).ok()
}

/// Error for an invalid frame rate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidRate;

impl fmt::Display for InvalidRate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid frame rate (numerator and denominator must be positive, at most 1000 fps)")
    }
}

impl std::error::Error for InvalidRate {}

/// A frame rate `num / den` frames per second. Construction validates it (both positive, at most
/// 1000 fps), so every method can divide by it safely.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "RawRate", into = "RawRate")]
pub struct FrameRate {
    num: u32,
    den: u32,
}

#[derive(Serialize, Deserialize)]
struct RawRate {
    num: u32,
    den: u32,
}

impl TryFrom<RawRate> for FrameRate {
    type Error = InvalidRate;
    fn try_from(r: RawRate) -> Result<Self, InvalidRate> {
        FrameRate::new(r.num, r.den)
    }
}

impl From<FrameRate> for RawRate {
    fn from(r: FrameRate) -> Self {
        RawRate { num: r.num, den: r.den }
    }
}

impl FrameRate {
    pub const FPS_24: FrameRate = FrameRate { num: 24, den: 1 };
    pub const FPS_25: FrameRate = FrameRate { num: 25, den: 1 };
    pub const FPS_30: FrameRate = FrameRate { num: 30, den: 1 };
    pub const FPS_48: FrameRate = FrameRate { num: 48, den: 1 };
    pub const FPS_50: FrameRate = FrameRate { num: 50, den: 1 };
    pub const FPS_60: FrameRate = FrameRate { num: 60, den: 1 };
    pub const FPS_120: FrameRate = FrameRate { num: 120, den: 1 };
    pub const FPS_144: FrameRate = FrameRate { num: 144, den: 1 };
    pub const NTSC_30: FrameRate = FrameRate { num: 30000, den: 1001 };
    pub const NTSC_60: FrameRate = FrameRate { num: 60000, den: 1001 };

    /// The rates offered in pickers.
    pub const COMMON: [FrameRate; 9] = [
        FrameRate::FPS_24,
        FrameRate::FPS_30,
        FrameRate::FPS_48,
        FrameRate::FPS_60,
        FrameRate::FPS_120,
        FrameRate::FPS_144,
        FrameRate::NTSC_30,
        FrameRate::NTSC_60,
        FrameRate::FPS_25,
    ];

    pub fn new(num: u32, den: u32) -> Result<FrameRate, InvalidRate> {
        if num == 0 || den == 0 || u64::from(num) > u64::from(den).saturating_mul(1000) {
            return Err(InvalidRate);
        }
        let g = gcd(num, den);
        Ok(FrameRate { num: num / g, den: den / g })
    }

    /// Integer frames per second.
    pub fn from_fps(fps: u32) -> Result<FrameRate, InvalidRate> {
        FrameRate::new(fps, 1)
    }

    pub const fn num(self) -> u32 {
        self.num
    }

    pub const fn den(self) -> u32 {
        self.den
    }

    pub fn as_f64(self) -> f64 {
        f64::from(self.num) / f64::from(self.den)
    }

    /// Start time of frame `index` relative to frame 0, rounded up to a tick (so that
    /// `frame_index_at(frame_time(k)) == k`). Exact for every index: computed, not accumulated.
    pub fn frame_time(self, index: u64) -> Hns {
        let n = u128::from(index) * u128::from(self.den) * HNS_PER_SEC as u128;
        let d = u128::from(self.num);
        Hns(i64::try_from(n.div_ceil(d)).unwrap_or(i64::MAX))
    }

    /// Index of the frame whose interval contains `t` (frame 0 starts at 0). Negative times map to 0.
    pub fn frame_index_at(self, t: Hns) -> u64 {
        if t.0 <= 0 {
            return 0;
        }
        let n = t.0 as u128 * u128::from(self.num) / (u128::from(self.den) * HNS_PER_SEC as u128);
        u64::try_from(n).unwrap_or(u64::MAX)
    }

    /// Nominal duration of one frame (rounded to nearest tick; use [`frame_time`](Self::frame_time)
    /// differences for exact per-frame durations).
    pub fn frame_duration(self) -> Hns {
        Hns(rescale_round(HNS_PER_SEC, i64::from(self.den), i64::from(self.num)).unwrap_or(HNS_PER_SEC))
    }

    /// One frame in nanoseconds, rounded (Matroska `DefaultDuration`).
    pub fn frame_duration_ns(self) -> u64 {
        let ns = (u128::from(self.den) * 1_000_000_000 + u128::from(self.num) / 2) / u128::from(self.num);
        u64::try_from(ns).unwrap_or(u64::MAX)
    }

    /// A short label: `60`, `59.94`, `29.97`.
    pub fn label(self) -> String {
        if self.den == 1 {
            self.num.to_string()
        } else {
            let s = format!("{:.2}", self.as_f64());
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        }
    }
}

impl fmt::Display for FrameRate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} fps", self.label())
    }
}

fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a.max(1)
}

/// Converts between sample counts at `rate` Hz and ticks, exactly.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SampleClock {
    rate: u32,
}

impl SampleClock {
    /// `None` for a zero rate.
    pub fn new(rate: u32) -> Option<SampleClock> {
        (rate > 0).then_some(SampleClock { rate })
    }

    pub const fn rate(self) -> u32 {
        self.rate
    }

    /// Time of sample `n` relative to sample 0 (floored).
    pub fn time_of(self, n: i64) -> Hns {
        Hns(rescale_floor(n, HNS_PER_SEC, i64::from(self.rate)).unwrap_or(if n < 0 { i64::MIN } else { i64::MAX }))
    }

    /// Number of samples (rounded to nearest) in duration `d`.
    pub fn samples_in(self, d: Hns) -> i64 {
        rescale_round(d.0, i64::from(self.rate), HNS_PER_SEC).unwrap_or(if d.0 < 0 { i64::MIN } else { i64::MAX })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_times_are_exact_and_monotonic() {
        let r = FrameRate::NTSC_60;
        assert_eq!(r.frame_time(0), Hns(0));
        assert_eq!(r.frame_time(60000), Hns::from_secs(1001));
        let mut last = Hns(-1);
        for k in 0..10_000 {
            let t = r.frame_time(k);
            assert!(t > last);
            assert_eq!(r.frame_index_at(t), k, "frame {k}");
            last = t;
        }
        // Eight hours in, still exact.
        let k = 8 * 3600 * 60;
        assert_eq!(FrameRate::FPS_60.frame_time(k), Hns::from_secs(8 * 3600));
    }

    #[test]
    fn rate_validation() {
        assert!(FrameRate::new(0, 1).is_err());
        assert!(FrameRate::new(1, 0).is_err());
        assert!(FrameRate::new(1001, 1).is_err());
        assert_eq!(FrameRate::new(120, 2).unwrap(), FrameRate::FPS_60);
        assert_eq!(FrameRate::NTSC_60.label(), "59.94");
        assert_eq!(FrameRate::FPS_144.label(), "144");
        assert_eq!(FrameRate::FPS_60.frame_duration_ns(), 16_666_667);
        let bad: Result<FrameRate, _> = serde_json::from_str(r#"{"num":0,"den":1}"#);
        assert!(bad.is_err());
    }

    #[test]
    fn rescale_rounds_and_rejects_hostile_input() {
        assert_eq!(rescale_round(15, 1, 10), Some(2));
        assert_eq!(rescale_round(-15, 1, 10), Some(-2));
        assert_eq!(rescale_round(1, 1, 0), None);
        assert_eq!(rescale_round(i64::MAX, i64::MAX, 1), None);
        assert_eq!(Hns::from_secs(2).to_timescale(48_000), Some(96_000));
        assert_eq!(Hns(1).to_timescale(0), None);
        assert_eq!(Hns::from_secs_f64(f64::NAN), Hns(0));
        assert_eq!(Hns::from_secs_f64(f64::INFINITY), Hns(i64::MAX));
    }

    #[test]
    fn sample_clock() {
        let c = SampleClock::new(48_000).unwrap();
        assert_eq!(c.time_of(48_000), Hns::from_secs(1));
        assert_eq!(c.samples_in(Hns::from_millis(10)), 480);
        assert!(SampleClock::new(0).is_none());
        assert_eq!(Hns::from_secs(3725).hms(), "01:02:05");
        assert_eq!(Hns(-5).hms(), "00:00:00");
    }
}
