//! STFT spectral noise suppression.
//!
//! * 1024-point frames (21.3 ms) with 50 % overlap and sqrt-Hann analysis + synthesis windows
//!   (their product, a periodic Hann, overlap-adds to exactly 1: perfect reconstruction at unity
//!   gain).
//! * Both channels share one complex FFT (L in the real part, R in the imaginary part). One real gain
//!   per bin, symmetric in `k` / `N-k`, is applied to the packed spectrum, which filters both channels
//!   identically (keeping the stereo image) at the cost of a single FFT pair per frame.
//! * Noise floor per bin: minimum statistics — the minimum of the smoothed periodogram over a ~1 s
//!   sliding window (8 sub-windows of 12 frames), times a bias factor. It follows rising noise within
//!   about a second and is not fooled by speech, which always has pauses.
//! * Gain: Wiener `ξ / (1 + ξ)` with the decision-directed a-priori SNR estimate (Ephraim–Malah,
//!   α = 0.98), which is what keeps residual noise smooth instead of "musical", plus 3-bin frequency
//!   smoothing and a floor of `-30 dB × strength`.
//! * Latency: exactly [`FRAME`] samples. When disabled the stage is a [`FRAME`]-sample delay line, so
//!   the source's timeline placement never depends on the setting; switching between the two hands
//!   the buffered audio over without a gap.

use crate::dsp::DelayLine;
use crate::fft::Fft;
use crate::settings::NoiseSuppressionSettings;
use crate::util::db_to_gain;

/// STFT frame length (and the stage's latency in frames).
pub(crate) const FRAME: usize = 1024;
const HOP: usize = FRAME / 2;
const BINS: usize = FRAME / 2 + 1;
const SUB_WINDOWS: usize = 8;
const SUB_FRAMES: u32 = 12;
const SMOOTH: f32 = 0.85;
const BIAS: f32 = 1.8;
const DD_ALPHA: f32 = 0.98;
const XI_MIN: f32 = 1e-3;

pub(crate) struct NoiseSuppressor {
    enabled: bool,
    floor: f32,
    over: f32,
    fft: Fft,
    window: Vec<f32>,
    in_l: Vec<f32>,
    in_r: Vec<f32>,
    acc_l: Vec<f32>,
    acc_r: Vec<f32>,
    out_l: Vec<f32>,
    out_r: Vec<f32>,
    hop_pos: usize,
    re: Vec<f32>,
    im: Vec<f32>,
    ps: Vec<f32>,
    cur_min: Vec<f32>,
    ring: Vec<f32>,
    ring_min: Vec<f32>,
    ring_idx: usize,
    sub_count: u32,
    prev_clean: Vec<f32>,
    power: Vec<f32>,
    /// Positions of bin `k` and of its mirror `N - k` in the FFT's permuted spectrum (equal for
    /// bins 0 and N/2, which are their own mirror).
    pos_a: Vec<usize>,
    pos_b: Vec<usize>,
    gain_raw: Vec<f32>,
    gain: Vec<f32>,
    bypass: DelayLine,
}

impl NoiseSuppressor {
    pub(crate) fn new() -> NoiseSuppressor {
        let window = (0..FRAME).map(|n| (0.5 * (1.0 - (std::f64::consts::TAU * n as f64 / FRAME as f64).cos())).sqrt() as f32).collect();
        let fft = Fft::new(FRAME);
        let mut pos_of = vec![0usize; FRAME];
        for p in 0..FRAME {
            if let Some(slot) = pos_of.get_mut(fft.bin_of(p)) {
                *slot = p;
            }
        }
        let pos_a = (0..BINS).map(|k| pos_of[k]).collect();
        let pos_b = (0..BINS).map(|k| pos_of[(FRAME - k) % FRAME]).collect();
        NoiseSuppressor {
            enabled: false,
            floor: 1.0,
            over: 1.0,
            fft,
            window,
            in_l: vec![0.0; FRAME],
            in_r: vec![0.0; FRAME],
            acc_l: vec![0.0; FRAME],
            acc_r: vec![0.0; FRAME],
            out_l: vec![0.0; HOP],
            out_r: vec![0.0; HOP],
            hop_pos: 0,
            re: vec![0.0; FRAME],
            im: vec![0.0; FRAME],
            ps: vec![0.0; BINS],
            cur_min: vec![f32::INFINITY; BINS],
            ring: vec![f32::INFINITY; BINS * SUB_WINDOWS],
            ring_min: vec![f32::INFINITY; BINS],
            ring_idx: 0,
            sub_count: 0,
            prev_clean: vec![0.0; BINS],
            power: vec![0.0; BINS],
            pos_a,
            pos_b,
            gain_raw: vec![1.0; BINS],
            gain: vec![1.0; BINS],
            bypass: DelayLine::new(FRAME),
        }
    }

    pub(crate) fn configure(&mut self, s: Option<&NoiseSuppressionSettings>) {
        match s {
            Some(s) => {
                let strength = s.strength.clamp(0.0, 1.0);
                self.floor = db_to_gain(-30.0 * strength);
                self.over = 1.0 + strength;
                if !self.enabled {
                    self.enable_from_bypass();
                }
            }
            None => {
                if self.enabled {
                    self.disable_to_bypass();
                }
            }
        }
    }

    /// Re-initialises the STFT state as if it had been running at unity gain over the audio
    /// currently in the bypass delay line, so the hand-over is seamless.
    fn enable_from_bypass(&mut self) {
        self.enabled = true;
        for k in 0..FRAME {
            let (l, r) = self.bypass.get(k);
            self.in_l[k] = l;
            self.in_r[k] = r;
        }
        // Next hop emits the oldest HOP frames; the accumulator holds the second half of the last
        // (virtual) frame weighted by the analysis·synthesis window.
        self.out_l.copy_from_slice(&self.in_l[..HOP]);
        self.out_r.copy_from_slice(&self.in_r[..HOP]);
        self.acc_l.fill(0.0);
        self.acc_r.fill(0.0);
        for k in 0..HOP {
            let w = self.window[HOP + k] * self.window[HOP + k];
            self.acc_l[k] = self.in_l[HOP + k] * w;
            self.acc_r[k] = self.in_r[HOP + k] * w;
        }
        // in_* keeps only the newest HOP frames in its first half (layout: [previous hop | new hop]).
        self.in_l.copy_within(HOP.., 0);
        self.in_r.copy_within(HOP.., 0);
        self.hop_pos = 0;
        self.ps.fill(0.0);
        self.cur_min.fill(f32::INFINITY);
        self.ring.fill(f32::INFINITY);
        self.ring_min.fill(f32::INFINITY);
        self.ring_idx = 0;
        self.sub_count = 0;
        self.prev_clean.fill(0.0);
        self.gain.fill(1.0);
    }

    /// Loads the bypass delay line with exactly what the STFT would have emitted next.
    fn disable_to_bypass(&mut self) {
        self.enabled = false;
        let hp = self.hop_pos;
        let pending = (hp..HOP).map(|k| (self.out_l[k], self.out_r[k]));
        let raw = (0..HOP + hp).map(|k| (self.in_l[k], self.in_r[k]));
        self.bypass.load(pending.chain(raw));
    }

    pub(crate) fn process(&mut self, buf: &mut [f32]) {
        if !self.enabled {
            self.bypass.process(buf);
            return;
        }
        let mut rest = buf;
        while !rest.is_empty() {
            let n = (HOP - self.hop_pos).min(rest.len() / 2);
            if n == 0 {
                break;
            }
            let (seg, tail) = rest.split_at_mut(2 * n);
            rest = tail;
            let hp = self.hop_pos;
            let ins = self.in_l[HOP + hp..HOP + hp + n].iter_mut().zip(self.in_r[HOP + hp..HOP + hp + n].iter_mut());
            let outs = self.out_l[hp..hp + n].iter().zip(self.out_r[hp..hp + n].iter());
            for ((f, (il, ir)), (&ol, &or)) in seg.as_chunks_mut::<2>().0.iter_mut().zip(ins).zip(outs) {
                *il = f[0];
                *ir = f[1];
                *f = [ol, or];
            }
            self.hop_pos += n;
            if self.hop_pos == HOP {
                self.process_frame();
                self.hop_pos = 0;
            }
        }
    }

    fn process_frame(&mut self) {
        for ((((re, im), &l), &r), &w) in self.re.iter_mut().zip(self.im.iter_mut()).zip(&self.in_l).zip(&self.in_r).zip(&self.window) {
            *re = l * w;
            *im = r * w;
        }
        self.fft.forward(&mut self.re, &mut self.im);
        self.update_gains();
        // The spectrum is in the FFT's permuted order: bin k lives at `pos_a[k]`, its mirror at
        // `pos_b[k]` (the gain is real and symmetric, so both get it; bins 0 and N/2 once).
        for (k, (&a, &b)) in self.pos_a.iter().zip(&self.pos_b).enumerate() {
            let g = self.gain[k];
            self.re[a] *= g;
            self.im[a] *= g;
            if b != a {
                self.re[b] *= g;
                self.im[b] *= g;
            }
        }
        self.fft.inverse(&mut self.re, &mut self.im);
        let scale = 1.0 / self.fft.len() as f32;
        for ((((al, ar), &re), &im), &w) in self.acc_l.iter_mut().zip(self.acc_r.iter_mut()).zip(&self.re).zip(&self.im).zip(&self.window) {
            let w = w * scale;
            *al += re * w;
            *ar += im * w;
        }
        self.out_l.copy_from_slice(&self.acc_l[..HOP]);
        self.out_r.copy_from_slice(&self.acc_r[..HOP]);
        self.acc_l.copy_within(HOP.., 0);
        self.acc_r.copy_within(HOP.., 0);
        self.acc_l[HOP..].fill(0.0);
        self.acc_r[HOP..].fill(0.0);
        self.in_l.copy_within(HOP.., 0);
        self.in_r.copy_within(HOP.., 0);
    }

    fn update_gains(&mut self) {
        // Per-bin power summed over both channels: |X_L|² + |X_R|² = (|Z_k|² + |Z_{N-k}|²) / 2.
        // (The constant factor cancels in every ratio below.) Bins 0 and N/2 are their own mirror.
        for ((p, &a), &b) in self.power.iter_mut().zip(&self.pos_a).zip(&self.pos_b) {
            // Bins 0 and N/2 are their own mirror: a == b counts them twice, as the formula says.
            *p = self.re[a] * self.re[a] + self.im[a] * self.im[a] + self.re[b] * self.re[b] + self.im[b] * self.im[b];
        }
        let scale = BIAS * self.over;
        // Branch-free element-wise pass over equal-length slices (vectorises).
        let n = BINS;
        let (power, ps, cur_min, ring_min, prev_clean, gain_raw) =
            (&self.power[..n], &mut self.ps[..n], &mut self.cur_min[..n], &self.ring_min[..n], &mut self.prev_clean[..n], &mut self.gain_raw[..n]);
        for k in 0..n {
            let p = power[k];
            let smoothed = SMOOTH * ps[k] + (1.0 - SMOOTH) * p;
            ps[k] = smoothed;
            let cm = cur_min[k].min(smoothed);
            cur_min[k] = cm;
            let inv = 1.0 / (ring_min[k].min(cm) * scale + 1e-12);
            let gamma = p * inv;
            let xi = (DD_ALPHA * prev_clean[k] * inv + (1.0 - DD_ALPHA) * (gamma - 1.0).max(0.0)).max(XI_MIN);
            let gain = xi / (1.0 + xi);
            gain_raw[k] = gain;
            prev_clean[k] = gain * gain * p;
        }
        // 3-bin frequency smoothing, then the strength floor.
        let floor = self.floor;
        let finish = |g: f32| if g.is_finite() { g.clamp(floor, 1.0) } else { 1.0 };
        for (out, w) in self.gain[1..BINS - 1].iter_mut().zip(self.gain_raw.windows(3)) {
            *out = finish(0.25 * w[0] + 0.5 * w[1] + 0.25 * w[2]);
        }
        self.gain[0] = finish(0.75 * self.gain_raw[0] + 0.25 * self.gain_raw[1]);
        self.gain[BINS - 1] = finish(0.75 * self.gain_raw[BINS - 1] + 0.25 * self.gain_raw[BINS - 2]);
        self.sub_count += 1;
        if self.sub_count >= SUB_FRAMES {
            self.sub_count = 0;
            let base = self.ring_idx * BINS;
            self.ring[base..base + BINS].copy_from_slice(&self.cur_min);
            self.ring_idx = (self.ring_idx + 1) % SUB_WINDOWS;
            self.cur_min.fill(f32::INFINITY);
            self.ring_min.fill(f32::INFINITY);
            for sub in self.ring.as_chunks::<BINS>().0 {
                for (m, &v) in self.ring_min.iter_mut().zip(sub) {
                    *m = m.min(v);
                }
            }
        }
        let clean = |v: &mut f32| {
            if !v.is_finite() || *v < 1e-30 {
                *v = 0.0;
            }
        };
        self.prev_clean.iter_mut().for_each(clean);
        self.ps.iter_mut().for_each(clean);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(ns: &mut NoiseSuppressor, input: &[f32]) -> Vec<f32> {
        let mut out = input.to_vec();
        for block in out.chunks_mut(2 * 300) {
            ns.process(block);
        }
        out
    }

    #[test]
    fn latency_is_one_frame_enabled_or_not() {
        for enabled in [false, true] {
            let mut ns = NoiseSuppressor::new();
            ns.configure(enabled.then_some(&NoiseSuppressionSettings { strength: 0.0 }));
            let mut input = vec![0.0; 2 * 8000];
            // A loud tone burst: unity gain at strength 0 means a faithful, delayed copy.
            for i in 3000..5000 {
                let v = 0.5 * (i as f32 * 0.3).sin();
                input[2 * i] = v;
                input[2 * i + 1] = -v;
            }
            let out = run(&mut ns, &input);
            for i in 0..8000 - FRAME {
                assert!((out[2 * (i + FRAME)] - input[2 * i]).abs() < 1e-3, "enabled={enabled} i={i}");
                assert!((out[2 * (i + FRAME) + 1] - input[2 * i + 1]).abs() < 1e-3);
            }
        }
    }

    #[test]
    fn suppresses_stationary_noise_and_keeps_tone() {
        let mut ns = NoiseSuppressor::new();
        ns.configure(Some(&NoiseSuppressionSettings { strength: 1.0 }));
        let mut seed = 1u32;
        let mut noise = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        };
        let n = 48_000 * 4;
        let mut input = vec![0.0; 2 * n];
        for i in 0..n {
            let tone = if i > 48_000 * 3 { 0.3 * (i as f32 * std::f32::consts::TAU * 1000.0 / 48_000.0).sin() } else { 0.0 };
            input[2 * i] = 0.02 * noise() + tone;
            input[2 * i + 1] = 0.02 * noise() + tone;
        }
        let out = run(&mut ns, &input);
        let rms = |s: &[f32]| (s.iter().map(|v| v * v).sum::<f32>() / s.len() as f32).sqrt();
        let noise_in = rms(&input[2 * 48_000 * 2..2 * 48_000 * 3]);
        let noise_out = rms(&out[2 * (48_000 * 2 + FRAME)..2 * (48_000 * 3)]);
        assert!(noise_out < noise_in * 0.2, "noise {noise_in} -> {noise_out}");
        let tone_in = rms(&input[2 * (48_000 * 3 + 4800)..]);
        let tone_out = rms(&out[2 * (48_000 * 3 + 4800 + FRAME)..]);
        assert!(tone_out > tone_in * 0.85, "tone {tone_in} -> {tone_out}");
        assert!(out.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn toggling_hands_over_without_gap() {
        let mut ns = NoiseSuppressor::new();
        let input: Vec<f32> = (0..2 * 20_000).map(|i| 0.5 * ((i / 2) as f32 * 0.05).sin()).collect();
        let mut out = input.clone();
        let mut pos = 0;
        let mut on = false;
        for (k, len) in [700usize, 1300, 900, 2000, 513, 3000, 777, 2500].iter().cycle().enumerate() {
            if pos >= out.len() {
                break;
            }
            let end = (pos + 2 * len).min(out.len());
            ns.process(&mut out[pos..end]);
            pos = end;
            on = !on;
            let _ = k;
            ns.configure(on.then_some(&NoiseSuppressionSettings { strength: 0.0 }));
        }
        // Strength 0 is unity gain, so toggling must leave a faithful delayed copy.
        for i in 0..20_000 - FRAME {
            assert!((out[2 * (i + FRAME)] - input[2 * i]).abs() < 2e-3, "i={i}");
        }
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    #[test]
    #[ignore = "timing"]
    fn denoise_speed() {
        let mut ns = NoiseSuppressor::new();
        ns.configure(Some(&NoiseSuppressionSettings { strength: 0.8 }));
        let mut buf: Vec<f32> = (0..2 * 512).map(|i| ((i * 7919) % 1000) as f32 * 1e-4).collect();
        let mut best = std::time::Duration::MAX;
        for _ in 0..5 {
            let t0 = std::time::Instant::now();
            for _ in 0..5625 {
                ns.process(&mut buf);
            }
            best = best.min(t0.elapsed());
        }
        println!("noise suppression, 60 s stereo: {best:?}");
    }
}
