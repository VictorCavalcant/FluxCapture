//! Small utilities: dB conversion, dithered 16-bit conversion and a test-tone generator.

use crate::OUTPUT_RATE;

/// dB to linear gain. NaN and `-inf` give 0; the result is capped at +100 dB.
pub fn db_to_gain(db: f32) -> f32 {
    if db.is_nan() || db == f32::NEG_INFINITY {
        return 0.0;
    }
    10f32.powf(db.min(100.0) / 20.0)
}

/// Linear gain (or amplitude) to dB. Zero, negative and NaN give `-inf`.
pub fn gain_to_db(gain: f32) -> f32 {
    if gain > 0.0 { 20.0 * gain.log10() } else { f32::NEG_INFINITY }
}

/// Replaces NaN/inf by 0 and bounds absurd magnitudes (±64, i.e. +36 dBFS) so no filter state can
/// blow up.
#[inline]
pub(crate) fn clean(x: f32) -> f32 {
    if x.is_finite() { x.clamp(-64.0, 64.0) } else { 0.0 }
}

/// Flushes denormal-range values to zero (IIR states decaying in silence are slow on x86 otherwise).
#[inline]
pub(crate) fn flush(x: f32) -> f32 {
    if x.abs() < 1e-20 || !x.is_finite() { 0.0 } else { x }
}

/// `1 - exp(-1 / (ms · 48 kHz))`: per-sample one-pole smoothing factor for a time constant in ms.
pub(crate) fn one_pole_alpha(ms: f32, rate: f32) -> f32 {
    let samples = (ms.max(0.0) * 0.001 * rate).max(1e-3);
    1.0 - (-1.0 / samples).exp()
}

/// Milliseconds to whole frames at 48 kHz (at least 1).
pub(crate) fn ms_to_frames(ms: f32) -> u32 {
    let f = (ms.max(0.0) * OUTPUT_RATE as f32 / 1000.0).round();
    if f.is_finite() { (f as u32).max(1) } else { 1 }
}

/// TPDF dither state for [`f32_to_i16`] (a small xorshift generator; deterministic per seed).
#[derive(Clone, Debug)]
pub struct Dither {
    state: u64,
    enabled: bool,
}

impl Default for Dither {
    fn default() -> Self {
        Dither::new(0x9E37_79B9_7F4A_7C15)
    }
}

impl Dither {
    /// Dither seeded with `seed` (0 is replaced by a fixed non-zero seed).
    pub fn new(seed: u64) -> Dither {
        Dither { state: if seed == 0 { 0x2545_F491_4F6C_DD1D } else { seed }, enabled: true }
    }

    /// No dither: plain rounding.
    pub fn disabled() -> Dither {
        Dither { enabled: false, ..Dither::default() }
    }

    #[inline]
    fn uniform(&mut self) -> f32 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        (x >> 40) as f32 / (1u64 << 24) as f32
    }

    /// Triangular noise in `(-1, 1)` LSB.
    #[inline]
    fn tpdf(&mut self) -> f32 {
        if self.enabled { self.uniform() - self.uniform() } else { 0.0 }
    }
}

/// Converts `f32` samples (full scale ±1.0) to `i16` with TPDF dither and clamping. Converts
/// `min(src.len(), dst.len())` samples; NaN/inf become 0.
pub fn f32_to_i16(src: &[f32], dst: &mut [i16], dither: &mut Dither) {
    for (d, &s) in dst.iter_mut().zip(src) {
        let x = if s.is_finite() { s.clamp(-2.0, 2.0) } else { 0.0 };
        let v = (x * 32767.0 + dither.tpdf()).round().clamp(-32768.0, 32767.0);
        *d = v as i16;
    }
}

/// A sine generator (test signals, setup wizard).
#[derive(Clone, Debug)]
pub struct ToneGenerator {
    phase: f64,
    step: f64,
    amplitude: f32,
}

impl ToneGenerator {
    /// A sine of `freq_hz` at `level_db` dBFS peak, sampled at `sample_rate`. Hostile values are
    /// sanitized: a zero rate becomes 48 kHz, the frequency is clamped to `0..=rate/2`, the level to
    /// `..=0 dBFS`.
    pub fn new(freq_hz: f32, level_db: f32, sample_rate: u32) -> ToneGenerator {
        let rate = if sample_rate == 0 { f64::from(OUTPUT_RATE) } else { f64::from(sample_rate) };
        let f = if freq_hz.is_finite() { f64::from(freq_hz).clamp(0.0, rate / 2.0) } else { 0.0 };
        ToneGenerator { phase: 0.0, step: f / rate, amplitude: db_to_gain(level_db.min(0.0)) }
    }

    /// The next sample.
    #[inline]
    pub fn next_sample(&mut self) -> f32 {
        let v = (self.phase * std::f64::consts::TAU).sin() as f32 * self.amplitude;
        self.phase += self.step;
        if self.phase >= 1.0 {
            self.phase -= 1.0;
        }
        v
    }

    /// Fills interleaved `out` with the tone on every one of `channels` channels (0 is treated as 1).
    pub fn fill(&mut self, out: &mut [f32], channels: usize) {
        for frame in out.chunks_mut(channels.max(1)) {
            let v = self.next_sample();
            frame.fill(v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_conversions() {
        assert!((db_to_gain(-6.0206) - 0.5).abs() < 1e-4);
        assert_eq!(db_to_gain(f32::NAN), 0.0);
        assert_eq!(db_to_gain(f32::NEG_INFINITY), 0.0);
        assert!(db_to_gain(f32::INFINITY).is_finite());
        assert!((gain_to_db(0.5) + 6.0206).abs() < 1e-3);
        assert_eq!(gain_to_db(0.0), f32::NEG_INFINITY);
        assert_eq!(gain_to_db(f32::NAN), f32::NEG_INFINITY);
    }

    #[test]
    fn i16_conversion_clamps_and_dithers() {
        let src = [0.0, 1.0, -1.0, 5.0, -5.0, f32::NAN, f32::INFINITY, 0.5];
        let mut dst = [0i16; 8];
        f32_to_i16(&src, &mut dst, &mut Dither::default());
        assert!(dst[0].abs() <= 1);
        assert!(dst[1] >= 32766);
        assert!(dst[2] <= -32766);
        assert_eq!(dst[3], 32767);
        assert_eq!(dst[4], -32768);
        assert!(dst[5].abs() <= 1 && dst[6].abs() <= 1);
        assert!((dst[7] as i32 - 16384).abs() <= 1);
        // TPDF dither: mean ~0, never more than 1 LSB.
        let zeros = vec![0.0f32; 10_000];
        let mut out = vec![0i16; 10_000];
        f32_to_i16(&zeros, &mut out, &mut Dither::new(7));
        assert!(out.iter().all(|v| v.abs() <= 1));
        let mean: f64 = out.iter().map(|&v| f64::from(v)).sum::<f64>() / 10_000.0;
        assert!(mean.abs() < 0.05);
        let mut plain = [0i16; 1];
        f32_to_i16(&[0.25 / 32767.0], &mut plain, &mut Dither::disabled());
        assert_eq!(plain[0], 0);
    }

    #[test]
    fn tone_generator() {
        let mut g = ToneGenerator::new(1000.0, -6.0206, 48_000);
        let mut buf = vec![0.0; 96];
        g.fill(&mut buf, 2);
        let peak = buf.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!((peak - 0.5).abs() < 1e-3);
        assert_eq!(buf[0], buf[1]);
        let mut h = ToneGenerator::new(f32::NAN, f32::NAN, 0);
        assert_eq!(h.next_sample(), 0.0);
    }
}
