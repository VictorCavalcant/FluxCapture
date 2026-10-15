//! Band-limited streaming resampler to 48 kHz with a continuously variable ratio.
//!
//! Polyphase windowed sinc: the kernel `2fc·sinc(2fc·d)·kaiser(d/half)` (β = 8.6, ≈ 86 dB stopband, 32 taps — 64 at 96 kHz input) is
//! tabulated at 256 sub-sample phases and linearly interpolated between neighbouring phases (4096
//! nearest phases for 48 kHz devices, whose phase only moves with the drift trim), so any
//! fractional position — and therefore any ratio, varying from one output sample to the next — is
//! served with the same quality. The cutoff is 91 % of the lower of the two Nyquist frequencies and
//! the kernel is stretched by the decimation factor when downsampling (96 kHz → 48 kHz uses twice the
//! taps), so nothing above 24 kHz aliases into the output.
//!
//! The kernel is centred (zero phase): input frame `i` is reproduced at output time
//! `(i - pos) / step` relative to the next output frame, with no group delay. Production simply waits
//! for `half` frames of lookahead input (0.5 ms at 48 kHz).

use std::sync::{Arc, OnceLock};

use crate::{MAX_INPUT_RATE, MIN_INPUT_RATE, OUTPUT_RATE};

/// Largest ratio trim [`Resampler::set_correction`] accepts (±0.5 %, ≈ ±8.6 cents: inaudible on
/// speech and game audio, and far more than the ±200 ppm clock drift it exists to absorb).
pub const MAX_CORRECTION: f64 = 0.005;

const FRAC_BITS: u32 = 32;
const FRAC_MASK: u64 = (1 << FRAC_BITS) - 1;

fn to_fixed(v: f64) -> u64 {
    // Saturating, NaN → 0 (`as` semantics); callers pass small positive values.
    (v.max(0.0) * (1u64 << FRAC_BITS) as f64).round() as u64
}

fn from_fixed(v: u64) -> f64 {
    v as f64 / (1u64 << FRAC_BITS) as f64
}

/// General kernel: 32 taps (scaled up when downsampling), β = 8.6, 256 phases interpolated.
const BASE_HALF: usize = 16;
const KAISER_BETA: f64 = 8.6;
const INTERP_PHASE_BITS: u32 = 8;
const CUTOFF: f64 = 0.91;
/// Kernel for a 48 kHz device: the ratio only deviates from 1 by the drift trim, so nothing can
/// alias and the kernel just has to be a flat fractional-delay filter up to ~17 kHz (24 taps,
/// β = 7, ≈ 72 dB). The phase moves slowly (by the drift), so a dense 4096-phase table is used
/// without interpolation (timing error < 1/4096 sample: below -77 dB at 15 kHz) and stays in
/// cache. This is the common case and the cheap path; the table is built once per process.
const UNITY_HALF: usize = 12;
const UNITY_BETA: f64 = 7.0;
const UNITY_PHASE_BITS: u32 = 12;
/// Largest number of input frames accepted per [`Resampler::input_slots`] call.
pub(crate) const MAX_INPUT_BLOCK: usize = 2048;

/// A tabulated polyphase kernel (immutable, shareable).
#[derive(Debug)]
struct Kernel {
    half: usize,
    taps: usize,
    phase_bits: u32,
    /// `2^phase_bits` rows of `taps` coefficients (row `p` = kernel at fractional offset `p / P`).
    table: Vec<f32>,
    /// Row `p + 1` minus row `p` for linear interpolation between phases; empty when the kernel
    /// is used with nearest-phase lookup.
    dtable: Vec<f32>,
}

impl Kernel {
    fn build(half: usize, beta: f64, fc: f64, phase_bits: u32, interpolate: bool) -> Kernel {
        let taps = 2 * half;
        let phases = 1usize << phase_bits;
        let i0_beta = bessel_i0(beta);
        let mut rows = vec![0f32; (phases + 1) * taps];
        for (p, row) in rows.chunks_exact_mut(taps).enumerate() {
            let f = p as f64 / phases as f64;
            let vals: Vec<f64> = (0..taps).map(|k| kernel(k as f64 - (half as f64 - 1.0) - f, fc, half as f64, beta, i0_beta)).collect();
            // Normalise every phase to unity DC gain (no phase-dependent level ripple).
            let sum: f64 = vals.iter().sum();
            let norm = if sum.abs() > 1e-9 { 1.0 / sum } else { 1.0 };
            for (c, v) in row.iter_mut().zip(&vals) {
                *c = (v * norm) as f32;
            }
        }
        let dtable = if interpolate { (0..phases * taps).map(|i| rows[i + taps] - rows[i]).collect() } else { Vec::new() };
        rows.truncate(phases * taps);
        Kernel { half, taps, phase_bits, table: rows, dtable }
    }

    fn for_rate(in_rate: u32) -> Arc<Kernel> {
        if in_rate == OUTPUT_RATE {
            static UNITY: OnceLock<Arc<Kernel>> = OnceLock::new();
            return UNITY.get_or_init(|| Arc::new(Kernel::build(UNITY_HALF, UNITY_BETA, 0.5 * CUTOFF, UNITY_PHASE_BITS, false))).clone();
        }
        let scale = (f64::from(in_rate) / f64::from(OUTPUT_RATE)).max(1.0);
        // Taps a multiple of 8 so the dot products run in full 8-lane chunks.
        let half = ((BASE_HALF as f64 * scale).ceil() as usize).div_ceil(4).saturating_mul(4).clamp(4, 256);
        Arc::new(Kernel::build(half, KAISER_BETA, 0.5 * CUTOFF / scale, INTERP_PHASE_BITS, true))
    }
}

/// Streaming stereo resampler from a device rate to [`OUTPUT_RATE`].
#[derive(Clone, Debug)]
pub struct Resampler {
    in_rate: u32,
    base_step: f64,
    /// Input frames per output frame, 32.32 fixed point (integer stepping keeps float→int
    /// conversions out of the per-sample loop; 2⁻³² resolution is far below what the drift
    /// controller resolves).
    step: u64,
    half: usize,
    taps: usize,
    kernel: Arc<Kernel>,
    buf_l: Vec<f32>,
    buf_r: Vec<f32>,
    /// Scratch for the interpolated kernel (`taps` long).
    coef: Vec<f32>,
    /// Input position (index into `buf_*`, 32.32 fixed point) of the next output frame's centre.
    pos: u64,
}

fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let q = x * x / 4.0;
    for k in 1..200 {
        term *= q / (k * k) as f64;
        sum += term;
        if term < sum * 1e-15 {
            break;
        }
    }
    sum
}

fn kernel(d: f64, fc: f64, half: f64, beta: f64, i0_beta: f64) -> f64 {
    let x = d / half;
    if x.abs() >= 1.0 {
        return 0.0;
    }
    let w = bessel_i0(beta * (1.0 - x * x).sqrt()) / i0_beta;
    let s = if d.abs() < 1e-12 { 2.0 * fc } else { (std::f64::consts::TAU * fc * d).sin() / (std::f64::consts::PI * d) };
    s * w
}

impl Resampler {
    /// A resampler from `in_rate` to 48 kHz; `None` outside
    /// [`MIN_INPUT_RATE`]`..=`[`MAX_INPUT_RATE`].
    pub fn new(in_rate: u32) -> Option<Resampler> {
        if !(MIN_INPUT_RATE..=MAX_INPUT_RATE).contains(&in_rate) {
            return None;
        }
        let base_step = f64::from(in_rate) / f64::from(OUTPUT_RATE);
        let kernel = Kernel::for_rate(in_rate);
        let (half, taps) = (kernel.half, kernel.taps);
        let cap = taps + MAX_INPUT_BLOCK + 8;
        let mut r = Resampler {
            in_rate,
            base_step,
            step: to_fixed(base_step),
            half,
            taps,
            kernel,
            buf_l: Vec::with_capacity(cap),
            buf_r: Vec::with_capacity(cap),
            coef: vec![0.0; taps],
            pos: 0,
        };
        r.reset_with_offset(0.0);
        Some(r)
    }

    /// The input rate.
    pub fn input_rate(&self) -> u32 {
        self.in_rate
    }

    /// Kernel length in input frames.
    pub fn taps(&self) -> usize {
        self.taps
    }

    /// Clears the history; the next input frame is reproduced exactly at the next output frame.
    pub fn reset(&mut self) {
        self.reset_with_offset(0.0);
    }

    /// Clears the history so that the next input frame lands `offset` output frames (`0..1`)
    /// *before* the next output frame.
    pub(crate) fn reset_with_offset(&mut self, offset: f64) {
        self.buf_l.clear();
        self.buf_r.clear();
        self.buf_l.resize(self.half, 0.0);
        self.buf_r.resize(self.half, 0.0);
        let offset = if offset.is_finite() { offset.clamp(0.0, 1.0) } else { 0.0 };
        self.pos = ((self.half as u64) << FRAC_BITS) + to_fixed(offset * from_fixed(self.step));
    }

    /// Trims the ratio by `correction` (clamped to ±[`MAX_CORRECTION`]): positive consumes input
    /// faster (for a device clock running fast).
    pub fn set_correction(&mut self, correction: f64) {
        let c = if correction.is_finite() { correction.clamp(-MAX_CORRECTION, MAX_CORRECTION) } else { 0.0 };
        self.step = to_fixed(self.base_step * (1.0 + c));
    }

    /// Output-frame distance from the next output frame to where the next input frame will land.
    pub(crate) fn pending_output_frames(&self) -> f64 {
        (self.buf_l.len() as f64 - from_fixed(self.pos)) / from_fixed(self.step)
    }

    /// Input frames buffered but not yet fully consumed, as output frames.
    pub(crate) fn buffered_output_frames(&self) -> f64 {
        self.pending_output_frames().max(0.0)
    }

    /// Makes room for `n` (at most [`MAX_INPUT_BLOCK`]) new input frames and returns their L and R
    /// slots (zeroed).
    pub(crate) fn input_slots(&mut self, n: usize) -> (&mut [f32], &mut [f32]) {
        let n = n.min(MAX_INPUT_BLOCK);
        // Drop input no future output frame needs (keeps the buffer within its reserved capacity).
        let keep_from = ((self.pos >> FRAC_BITS) as usize + 1).saturating_sub(self.half);
        let keep_from = keep_from.min(self.buf_l.len());
        if keep_from > 0 {
            self.buf_l.drain(..keep_from);
            self.buf_r.drain(..keep_from);
            self.pos -= (keep_from as u64) << FRAC_BITS;
        }
        let old = self.buf_l.len();
        self.buf_l.resize(old + n, 0.0);
        self.buf_r.resize(old + n, 0.0);
        (&mut self.buf_l[old..], &mut self.buf_r[old..])
    }

    /// Produces every output frame whose kernel window is fully available, calling `emit(l, r)`.
    pub(crate) fn produce(&mut self, mut emit: impl FnMut(f32, f32)) {
        let len = self.buf_l.len();
        let (taps, half) = (self.taps, self.half);
        let k = &*self.kernel;
        let shift = FRAC_BITS - k.phase_bits;
        let interp = !k.dtable.is_empty();
        let t_scale = 1.0 / (1u64 << shift) as f32;
        let mut pos = self.pos;
        loop {
            // `pos >= half - 1` by construction; the second test only guards the index arithmetic.
            let i0 = (pos >> FRAC_BITS) as usize;
            if i0 + half >= len || i0 + 1 < half {
                break;
            }
            let start = i0 + 1 - half;
            let frac = pos & FRAC_MASK;
            let c = (frac >> shift) as usize * taps;
            let (Some(row), Some(xl), Some(xr)) = (k.table.get(c..c + taps), self.buf_l.get(start..start + taps), self.buf_r.get(start..start + taps)) else {
                break;
            };
            let (l, r) = if interp {
                let Some(drow) = k.dtable.get(c..c + taps) else { break };
                let t = (frac & ((1 << shift) - 1)) as f32 * t_scale;
                dot2(&mut self.coef, row, drow, t, xl, xr)
            } else {
                (dot(row, xl), dot(row, xr))
            };
            emit(l, r);
            pos = pos.saturating_add(self.step);
        }
        self.pos = pos;
    }

    /// Feeds `half` frames of silence and produces the pending output, i.e. lets the end of a
    /// segment ring out instead of being cut half a kernel early (used before a gap).
    pub(crate) fn flush(&mut self, emit: impl FnMut(f32, f32)) {
        let half = self.half;
        self.input_slots(half);
        self.produce(emit);
    }

    /// Convenience streaming API: resamples interleaved stereo `input` and appends the produced
    /// interleaved stereo frames to `out`.
    pub fn process_interleaved(&mut self, input: &[f32], out: &mut Vec<f32>) {
        for block in input.chunks(2 * MAX_INPUT_BLOCK) {
            let n = block.len() / 2;
            let (l, r) = self.input_slots(n);
            for ((fr, l), r) in block.as_chunks::<2>().0.iter().zip(l.iter_mut()).zip(r.iter_mut()) {
                *l = crate::util::clean(fr[0]);
                *r = crate::util::clean(fr[1]);
            }
            self.produce(|l, r| {
                out.push(l);
                out.push(r);
            });
        }
    }
}

/// Interpolates the coefficients for fractional phase `t`, then runs one dot product per channel.
/// Written as three simple element-wise loops so each vectorises along the taps.
#[inline]
fn dot2(coef: &mut [f32], row: &[f32], drow: &[f32], t: f32, xl: &[f32], xr: &[f32]) -> (f32, f32) {
    for ((c, &r), &d) in coef.iter_mut().zip(row).zip(drow) {
        *c = r + t * d;
    }
    (dot(coef, xl), dot(coef, xr))
}

/// Dot product with 8 independent accumulators (vectorises without reassociating float sums).
#[inline]
fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = [0f32; 8];
    for (x, y) in a.as_chunks::<8>().0.iter().zip(b.as_chunks::<8>().0.iter()) {
        for j in 0..8 {
            acc[j] += x[j] * y[j];
        }
    }
    let h = [acc[0] + acc[4], acc[1] + acc[5], acc[2] + acc[6], acc[3] + acc[7]];
    (h[0] + h[2]) + (h[1] + h[3])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_hostile_rates() {
        assert!(Resampler::new(0).is_none());
        assert!(Resampler::new(1).is_none());
        assert!(Resampler::new(u32::MAX).is_none());
        assert!(Resampler::new(384_000).is_some_and(|r| r.taps() % 8 == 0));
    }

    #[test]
    fn unity_ratio_is_transparent() {
        let mut r = Resampler::new(48_000).unwrap();
        let input: Vec<f32> = (0..4000).flat_map(|i| [(i as f32 * 0.01).sin(), (i as f32 * 0.02).cos()]).collect();
        let mut out = Vec::new();
        r.process_interleaved(&input, &mut out);
        assert_eq!(out.len(), input.len() - 2 * r.half);
        for (a, b) in out.iter().zip(&input).skip(4 * r.taps) {
            assert!((a - b).abs() < 1e-3, "{a} {b}");
        }
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    #[test]
    #[ignore = "timing"]
    fn produce_speed() {
        for rate in [48_000u32, 44_100] {
            let mut r = Resampler::new(rate).unwrap();
            let n = rate as usize / 100;
            let mut best = std::time::Duration::MAX;
            let mut acc = 0.0f32;
            for _ in 0..5 {
                let t0 = std::time::Instant::now();
                for k in 0..6000 {
                    let (l, rr) = r.input_slots(n);
                    for (i, (a, b)) in l.iter_mut().zip(rr.iter_mut()).enumerate() {
                        *a = ((k * n + i) % 1000) as f32 * 1e-3;
                        *b = *a;
                    }
                    r.produce(|a, b| acc += a + b);
                }
                best = best.min(t0.elapsed());
            }
            println!("produce {rate}: {best:?} per 60 s ({acc})");
        }
    }
}
