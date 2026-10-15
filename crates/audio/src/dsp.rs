//! Per-sample DSP building blocks, all at 48 kHz on interleaved stereo blocks.

use crate::OUTPUT_RATE;
use crate::settings::{CompressorSettings, GateSettings, HighPassSettings, LimiterSettings};
use crate::util::{db_to_gain, flush, ms_to_frames, one_pole_alpha};

const RATE: f32 = OUTPUT_RATE as f32;
/// Limiter lookahead: 5 ms.
pub(crate) const LIMITER_LOOKAHEAD: usize = 240;
/// Gain changes (fader, mute, master, input gain) ramp linearly over 10 ms.
pub(crate) const RAMP_FRAMES: u32 = 480;
/// Limiter sliding-minimum deque capacity minus one (a power of two ≥ lookahead + 1, so ring
/// indices wrap with a mask instead of a division).
const DQ_MASK: usize = 511;
const _: () = assert!(DQ_MASK + 1 > LIMITER_LOOKAHEAD && (DQ_MASK + 1).is_power_of_two());

/// A linearly ramped gain (no zipper noise).
#[derive(Clone, Debug)]
pub(crate) struct Ramp {
    cur: f32,
    target: f32,
    step: f32,
    left: u32,
}

impl Ramp {
    pub(crate) fn new(v: f32) -> Ramp {
        Ramp { cur: v, target: v, step: 0.0, left: 0 }
    }

    /// Ramps to `target` over [`RAMP_FRAMES`].
    pub(crate) fn set(&mut self, target: f32) {
        if target == self.target {
            return;
        }
        self.target = target;
        self.left = RAMP_FRAMES;
        self.step = (target - self.cur) / RAMP_FRAMES as f32;
    }

    pub(crate) fn jump(&mut self, v: f32) {
        *self = Ramp::new(v);
    }

    #[inline]
    pub(crate) fn next(&mut self) -> f32 {
        if self.left > 0 {
            self.left -= 1;
            self.cur = if self.left == 0 { self.target } else { self.cur + self.step };
        }
        self.cur
    }

    /// The value, if not ramping (lets callers use a constant-gain loop).
    pub(crate) fn steady(&self) -> Option<f32> {
        (self.left == 0).then_some(self.cur)
    }

    /// Steady at exactly `v` (lets callers skip a multiply loop).
    pub(crate) fn is_steady_at(&self, v: f32) -> bool {
        self.left == 0 && self.cur == v
    }
}

/// 2nd-order Butterworth high-pass (RBJ cookbook), transposed direct form II, per channel.
#[derive(Clone, Debug, Default)]
pub(crate) struct HighPass {
    enabled: bool,
    b: [f32; 3],
    a: [f32; 2],
    z: [[f32; 2]; 2],
}

impl HighPass {
    pub(crate) fn configure(&mut self, s: Option<&HighPassSettings>) {
        let Some(s) = s else {
            self.enabled = false;
            self.z = [[0.0; 2]; 2];
            return;
        };
        let f = s.cutoff_hz.clamp(10.0, 1000.0);
        let w0 = std::f32::consts::TAU * f / RATE;
        let (sn, cs) = w0.sin_cos();
        let alpha = sn / (2.0 * std::f32::consts::FRAC_1_SQRT_2);
        let a0 = 1.0 + alpha;
        self.b = [(1.0 + cs) / 2.0 / a0, -(1.0 + cs) / a0, (1.0 + cs) / 2.0 / a0];
        self.a = [-2.0 * cs / a0, (1.0 - alpha) / a0];
        self.enabled = true;
    }

    pub(crate) fn process(&mut self, buf: &mut [f32]) {
        if !self.enabled {
            return;
        }
        let [b0, b1, b2] = self.b;
        let [a1, a2] = self.a;
        for frame in buf.as_chunks_mut::<2>().0.iter_mut() {
            for (x, z) in frame.iter_mut().zip(self.z.iter_mut()) {
                let y = b0 * *x + z[0];
                z[0] = b1 * *x - a1 * y + z[1];
                z[1] = b2 * *x - a2 * y;
                *x = y;
            }
        }
        for z in self.z.iter_mut().flatten() {
            *z = flush(*z);
        }
    }
}

/// Noise gate: peak envelope (instant attack, 20 ms decay), open/close hysteresis, hold, linear
/// attack/release fades. Stereo-linked.
#[derive(Clone, Debug)]
pub(crate) struct Gate {
    enabled: bool,
    open_th: f32,
    close_th: f32,
    attack_step: f32,
    release_step: f32,
    hold: u32,
    env_decay: f32,
    env: f32,
    open: bool,
    hold_left: u32,
    gain: f32,
}

impl Default for Gate {
    fn default() -> Self {
        Gate {
            enabled: false,
            open_th: 0.0,
            close_th: 0.0,
            attack_step: 1.0,
            release_step: 1.0,
            hold: 0,
            env_decay: 1.0 - one_pole_alpha(20.0, RATE),
            env: 0.0,
            open: false,
            hold_left: 0,
            gain: 0.0,
        }
    }
}

impl Gate {
    pub(crate) fn configure(&mut self, s: Option<&GateSettings>) {
        let Some(s) = s else {
            self.enabled = false;
            return;
        };
        if !self.enabled {
            // Start closed with a fresh envelope.
            self.open = false;
            self.gain = 0.0;
            self.env = 0.0;
        }
        self.enabled = true;
        self.open_th = db_to_gain(s.open_db);
        self.close_th = db_to_gain(s.close_db.min(s.open_db));
        self.attack_step = 1.0 / ms_to_frames(s.attack_ms) as f32;
        self.release_step = 1.0 / ms_to_frames(s.release_ms) as f32;
        self.hold = if s.hold_ms > 0.0 { ms_to_frames(s.hold_ms) } else { 0 };
    }

    pub(crate) fn process(&mut self, buf: &mut [f32]) {
        if !self.enabled {
            return;
        }
        for frame in buf.as_chunks_mut::<2>().0.iter_mut() {
            let peak = frame[0].abs().max(frame[1].abs());
            self.env = if peak > self.env { peak } else { self.env * self.env_decay };
            if self.open {
                if self.env >= self.close_th {
                    self.hold_left = self.hold;
                } else if self.hold_left > 0 {
                    self.hold_left -= 1;
                } else {
                    self.open = false;
                }
            } else if self.env >= self.open_th {
                self.open = true;
                self.hold_left = self.hold;
            }
            self.gain = if self.open { (self.gain + self.attack_step).min(1.0) } else { (self.gain - self.release_step).max(0.0) };
            frame[0] *= self.gain;
            frame[1] *= self.gain;
        }
        self.env = flush(self.env);
    }
}

/// Feed-forward compressor: stereo-linked RMS detector (5 ms), soft knee (6 dB), attack/release
/// smoothing of the gain reduction in dB. The gain computer runs every 16 frames (0.33 ms) and the
/// linear gain is interpolated per sample in between, which keeps logarithms out of the per-sample
/// loop.
#[derive(Clone, Debug)]
pub(crate) struct Compressor {
    enabled: bool,
    threshold: f32,
    ratio: f32,
    makeup: f32,
    attack: f32,
    release: f32,
    ms_alpha: f32,
    ms: f32,
    gr_db: f32,
    gain: f32,
    phase: usize,
    step: f32,
}

const COMP_BLOCK: usize = 16;
const KNEE_DB: f32 = 6.0;

impl Default for Compressor {
    fn default() -> Self {
        Compressor {
            enabled: false,
            threshold: 0.0,
            ratio: 1.0,
            makeup: 0.0,
            attack: 1.0,
            release: 1.0,
            ms_alpha: one_pole_alpha(5.0, RATE),
            ms: 0.0,
            gr_db: 0.0,
            gain: 1.0,
            phase: 0,
            step: 0.0,
        }
    }
}

impl Compressor {
    pub(crate) fn configure(&mut self, s: Option<&CompressorSettings>) {
        let Some(s) = s else {
            self.enabled = false;
            return;
        };
        if !self.enabled {
            self.ms = 0.0;
            self.gr_db = 0.0;
            self.gain = db_to_gain(s.makeup_db);
            self.step = 0.0;
        }
        self.enabled = true;
        self.threshold = s.threshold_db;
        self.ratio = s.ratio.max(1.0);
        self.makeup = s.makeup_db;
        let block_rate = RATE / COMP_BLOCK as f32;
        self.attack = 1.0 - one_pole_alpha(s.attack_ms, block_rate);
        self.release = 1.0 - one_pole_alpha(s.release_ms, block_rate);
    }

    /// Static curve: gain reduction (≤ 0 dB) for an input level in dB.
    pub(crate) fn gain_reduction(threshold: f32, ratio: f32, level_db: f32) -> f32 {
        let over = level_db - threshold;
        let slope = 1.0 / ratio - 1.0;
        if 2.0 * over < -KNEE_DB {
            0.0
        } else if 2.0 * over.abs() <= KNEE_DB {
            let k = over + KNEE_DB / 2.0;
            slope * k * k / (2.0 * KNEE_DB)
        } else {
            slope * over
        }
    }

    pub(crate) fn process(&mut self, buf: &mut [f32]) {
        if !self.enabled {
            return;
        }
        for frame in buf.as_chunks_mut::<2>().0.iter_mut() {
            if self.phase == 0 {
                let level_db = 10.0 * (self.ms + 1e-20).log10();
                let target = Self::gain_reduction(self.threshold, self.ratio, level_db);
                let coef = if target < self.gr_db { self.attack } else { self.release };
                self.gr_db = target + coef * (self.gr_db - target);
                let next = db_to_gain(self.gr_db + self.makeup);
                self.step = (next - self.gain) / COMP_BLOCK as f32;
            }
            self.phase = (self.phase + 1) % COMP_BLOCK;
            let p = (frame[0] * frame[0]).max(frame[1] * frame[1]);
            self.ms += (p - self.ms) * self.ms_alpha;
            self.gain += self.step;
            frame[0] *= self.gain;
            frame[1] *= self.gain;
        }
        self.ms = flush(self.ms);
    }
}

/// A stereo delay line of a fixed number of frames.
#[derive(Clone, Debug)]
pub(crate) struct DelayLine {
    buf: Vec<f32>,
    idx: usize,
}

impl DelayLine {
    pub(crate) fn new(frames: usize) -> DelayLine {
        DelayLine { buf: vec![0.0; 2 * frames.max(1)], idx: 0 }
    }

    pub(crate) fn frames(&self) -> usize {
        self.buf.len() / 2
    }

    /// Delays one frame.
    #[inline]
    pub(crate) fn tick(&mut self, l: f32, r: f32) -> (f32, f32) {
        let i = 2 * self.idx;
        let out = (self.buf[i], self.buf[i + 1]);
        self.buf[i] = l;
        self.buf[i + 1] = r;
        self.idx += 1;
        if self.idx == self.frames() {
            self.idx = 0;
        }
        out
    }

    /// Delays a block: swapping the block with the ring, segment by segment, outputs the oldest
    /// frames and stores the new ones in one vectorised pass.
    pub(crate) fn process(&mut self, buf: &mut [f32]) {
        let frames = self.frames();
        let even = buf.len() / 2 * 2;
        let mut rest = &mut buf[..even];
        while !rest.is_empty() {
            let seg = (frames - self.idx).min(rest.len() / 2);
            let (head, tail) = rest.split_at_mut(2 * seg);
            head.swap_with_slice(&mut self.buf[2 * self.idx..2 * (self.idx + seg)]);
            self.idx += seg;
            if self.idx == frames {
                self.idx = 0;
            }
            rest = tail;
        }
    }

    /// Frame `k` of the content, oldest first (`k < frames()`).
    pub(crate) fn get(&self, k: usize) -> (f32, f32) {
        let i = 2 * ((self.idx + k) % self.frames());
        (self.buf[i], self.buf[i + 1])
    }

    /// Replaces the content, oldest first; missing frames become silence.
    pub(crate) fn load(&mut self, frames: impl Iterator<Item = (f32, f32)>) {
        self.buf.fill(0.0);
        self.idx = 0;
        for (f, (l, r)) in self.buf.as_chunks_mut::<2>().0.iter_mut().zip(frames) {
            f[0] = l;
            f[1] = r;
        }
    }

    pub(crate) fn clear(&mut self) {
        self.buf.fill(0.0);
        self.idx = 0;
    }
}

/// Lookahead peak limiter, stereo-linked, guaranteeing `|y| <= ceiling` for every sample.
///
/// For each input sample the required gain is `g_req = min(1, ceiling / peak)`. The applied gain is
/// a boxcar average (length L+1) of a sliding minimum (window L+1) of `g_req`, further restricted by
/// a one-pole release that only ever *lowers* it. Every term of that average is a minimum over a
/// window containing sample `n - L`, so the gain applied to the delayed sample `x[n - L]` is at most
/// `g_req[n - L]` — a hard guarantee, not a statistical one — while the attack is a smooth L-sample
/// ramp. A final clamp absorbs float rounding. Latency: exactly [`LIMITER_LOOKAHEAD`] frames, whether
/// enabled or not.
#[derive(Clone, Debug)]
pub(crate) struct Limiter {
    enabled: bool,
    ceiling: f32,
    release: f32,
    delay: DelayLine,
    dq_val: Vec<f32>,
    dq_idx: Vec<u64>,
    dq_head: usize,
    dq_len: usize,
    n: u64,
    held: f32,
    boxcar: Vec<f32>,
    box_pos: usize,
    box_sum: f64,
}

impl Limiter {
    pub(crate) fn new() -> Limiter {
        let w = LIMITER_LOOKAHEAD + 1;
        let mut l = Limiter {
            enabled: false,
            ceiling: 1.0,
            release: 0.0,
            delay: DelayLine::new(LIMITER_LOOKAHEAD),
            dq_val: vec![1.0; DQ_MASK + 1],
            dq_idx: vec![0; DQ_MASK + 1],
            dq_head: 0,
            dq_len: 0,
            n: 0,
            held: 1.0,
            boxcar: vec![1.0; w],
            box_pos: 0,
            box_sum: w as f64,
        };
        l.reset_gain();
        l
    }

    fn reset_gain(&mut self) {
        self.dq_head = 0;
        self.dq_len = 0;
        self.held = 1.0;
        self.boxcar.fill(1.0);
        self.box_pos = 0;
        self.box_sum = self.boxcar.len() as f64;
    }

    pub(crate) fn configure(&mut self, s: Option<&LimiterSettings>) {
        match s {
            Some(s) => {
                if !self.enabled {
                    self.reset_gain();
                }
                self.enabled = true;
                self.ceiling = db_to_gain(s.ceiling_db.min(0.0)).clamp(1e-3, 1.0);
                self.release = one_pole_alpha(s.release_ms, RATE);
            }
            None => self.enabled = false,
        }
    }

    pub(crate) fn clear(&mut self) {
        self.delay.clear();
        self.reset_gain();
    }

    pub(crate) fn process(&mut self, buf: &mut [f32]) {
        if !self.enabled {
            self.delay.process(buf);
            return;
        }
        let w = self.boxcar.len();
        let lookahead = LIMITER_LOOKAHEAD as u64;
        // Fast path (the common case): fully recovered and nothing in this block needs limiting.
        // The state after the block is exactly what the per-sample loop would leave: the deque's
        // single entry is the newest index with gain 1 and the boxcar stays all ones.
        let frames = buf.len() / 2;
        if frames > 0 && self.held == 1.0 && self.dq_len == 1 && self.dq_val[self.dq_head] == 1.0 && self.box_sum == w as f64 && block_peak(buf) <= self.ceiling
        {
            self.dq_idx[self.dq_head] = self.n + frames as u64 - 1;
            self.n += frames as u64;
            self.delay.process(buf);
            return;
        }
        for f in buf.as_chunks_mut::<2>().0.iter_mut() {
            let peak = f[0].abs().max(f[1].abs());
            let g_req = if peak > self.ceiling { self.ceiling / peak } else { 1.0 };
            // Sliding minimum over [n - L, n] (monotonic deque in a ring).
            while self.dq_len > 0 && self.dq_val[(self.dq_head + self.dq_len - 1) & DQ_MASK] >= g_req {
                self.dq_len -= 1;
            }
            let tail = (self.dq_head + self.dq_len) & DQ_MASK;
            self.dq_val[tail] = g_req;
            self.dq_idx[tail] = self.n;
            self.dq_len += 1;
            while self.dq_len > 0 && self.dq_idx[self.dq_head] + lookahead < self.n {
                self.dq_head = (self.dq_head + 1) & DQ_MASK;
                self.dq_len -= 1;
            }
            let h = self.dq_val[self.dq_head];
            // Release: rise slowly, fall instantly (never above h).
            self.held = if h < self.held {
                h
            } else if h - self.held < 1e-6 {
                // Snap: a one-pole never reaches its target exactly in floating point.
                h
            } else {
                self.held + (h - self.held) * self.release
            };
            self.box_sum += f64::from(self.held) - f64::from(self.boxcar[self.box_pos]);
            self.boxcar[self.box_pos] = self.held;
            self.box_pos += 1;
            if self.box_pos == w {
                self.box_pos = 0;
                // Re-sum exactly once per window so rounding cannot accumulate over hours.
                self.box_sum = self.boxcar.iter().map(|&v| f64::from(v)).sum();
            }
            let g = (self.box_sum / w as f64) as f32;
            let (dl, dr) = self.delay.tick(f[0], f[1]);
            f[0] = (dl * g).clamp(-self.ceiling, self.ceiling);
            f[1] = (dr * g).clamp(-self.ceiling, self.ceiling);
            self.n += 1;
        }
    }
}

/// Largest absolute sample of a block (8 lanes so the reduction vectorises).
fn block_peak(buf: &[f32]) -> f32 {
    let mut m = [0f32; 8];
    let (chunks, rest) = buf.as_chunks::<8>();
    for c in chunks {
        for j in 0..8 {
            m[j] = m[j].max(c[j].abs());
        }
    }
    let lanes = m.iter().fold(0f32, |a, &b| a.max(b));
    rest.iter().fold(lanes, |a, &b| a.max(b.abs()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soft_knee_curve() {
        assert_eq!(Compressor::gain_reduction(-20.0, 4.0, -40.0), 0.0);
        assert!((Compressor::gain_reduction(-20.0, 4.0, 0.0) + 15.0).abs() < 1e-4);
        let at = Compressor::gain_reduction(-20.0, 4.0, -20.0);
        assert!(at < 0.0 && at > -1.5);
    }

    #[test]
    fn limiter_is_delay_when_disabled() {
        let mut l = Limiter::new();
        let mut buf = vec![0.0; 2 * 600];
        buf[0] = 3.0;
        l.process(&mut buf);
        assert_eq!(buf[2 * LIMITER_LOOKAHEAD], 3.0);
    }

    #[test]
    fn high_pass_removes_dc() {
        let mut hp = HighPass::default();
        hp.configure(Some(&HighPassSettings { cutoff_hz: 100.0 }));
        let mut buf = vec![1.0; 2 * 48_000];
        hp.process(&mut buf);
        assert!(buf[buf.len() - 1].abs() < 1e-4);
    }
}
