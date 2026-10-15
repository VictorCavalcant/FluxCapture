//! Shared simulation helpers for the integration tests.
#![allow(dead_code)]

use fluxcapture_audio::{AudioChunk, AudioFormat, MixBuffers, Mixer, MixerSettings, SourceId};
use fluxcapture_time::{Hns, SampleClock};

/// QPC time of output frame 0.
pub const BASE: Hns = Hns(10_000_000_000);

pub fn out_clock() -> SampleClock {
    SampleClock::new(48_000).unwrap()
}

pub fn no_limiter() -> MixerSettings {
    MixerSettings { limiter: None, ..MixerSettings::default() }
}

/// Deterministic pseudo-random numbers.
pub struct Lcg(pub u64);

impl Lcg {
    pub fn next_f64(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform in [-1, 1).
    pub fn bipolar(&mut self) -> f32 {
        (self.next_f64() * 2.0 - 1.0) as f32
    }
}

/// Drives a mixer window by window from `BASE`, optionally keeping every track's output.
pub struct Harness {
    pub mixer: Mixer,
    pub bufs: MixBuffers,
    pub out: Vec<Vec<f32>>,
    pub mixed: usize,
    pub window: usize,
    pub collect: bool,
}

impl Harness {
    pub fn new(settings: MixerSettings) -> Harness {
        let tracks = settings.sanitized().tracks;
        Harness { mixer: Mixer::new(settings), bufs: MixBuffers::new(tracks, 480), out: vec![Vec::new(); tracks], mixed: 0, window: 480, collect: true }
    }

    pub fn time_of(&self, frame: usize) -> Hns {
        BASE + out_clock().time_of(frame as i64)
    }

    /// Real time at which the next window is mixed: its end plus `delay`.
    pub fn next_due(&self, delay: Hns) -> Hns {
        self.time_of(self.mixed + self.window) + delay
    }

    pub fn mix_next(&mut self) {
        let start = self.time_of(self.mixed);
        let n = self.mixer.mix(start, self.window, &mut self.bufs);
        assert_eq!(n, self.window);
        if self.collect {
            for (o, b) in self.out.iter_mut().zip(&self.bufs.tracks) {
                o.extend_from_slice(b);
            }
        }
        self.mixed += self.window;
    }
}

/// A simulated capture device.
pub struct Feeder {
    pub id: SourceId,
    pub rate: u32,
    pub channels: u16,
    pub chunk_frames: u64,
    /// Device frames per nominal frame (1 + drift).
    pub clock_factor: f64,
    /// True (QPC) time of device frame 0.
    pub start: Hns,
    /// Uniform timestamp jitter, ± ms.
    pub jitter_ms: f64,
    /// Chunks starting in this device-frame range are never delivered (loopback silence).
    pub pause: Option<(u64, u64)>,
    pub next_frame: u64,
    pub rng: Lcg,
    pub generator: Box<dyn FnMut(u64, &mut [f32])>,
}

impl Feeder {
    pub fn new(id: SourceId, rate: u32, channels: u16, generator: impl FnMut(u64, &mut [f32]) + 'static) -> Feeder {
        Feeder {
            id,
            rate,
            channels,
            chunk_frames: u64::from(rate) / 100,
            clock_factor: 1.0,
            start: BASE,
            jitter_ms: 0.0,
            pause: None,
            next_frame: 0,
            rng: Lcg(0x1234_5678),
            generator: Box::new(generator),
        }
    }

    pub fn true_time(&self, frame: u64) -> Hns {
        self.start + Hns::from_secs_f64(frame as f64 / (f64::from(self.rate) * self.clock_factor))
    }

    /// Every chunk fully captured by real time `now` (and not paused).
    pub fn chunks_until(&mut self, now: Hns) -> Vec<AudioChunk> {
        let mut out = Vec::new();
        loop {
            let first = self.next_frame;
            let end = first + self.chunk_frames;
            if self.true_time(end) > now {
                return out;
            }
            self.next_frame = end;
            if self.pause.is_some_and(|(a, b)| first >= a && first < b) {
                continue;
            }
            let ch = usize::from(self.channels);
            let mut samples = vec![0.0f32; self.chunk_frames as usize * ch];
            for (i, frame) in samples.chunks_mut(ch).enumerate() {
                (self.generator)(first + i as u64, frame);
            }
            let jitter = Hns::from_secs_f64((self.rng.next_f64() * 2.0 - 1.0) * self.jitter_ms / 1000.0);
            out.push(AudioChunk {
                source: self.id,
                time: self.true_time(first) + jitter,
                format: AudioFormat { sample_rate: self.rate, channels: self.channels },
                samples,
                discontinuity: false,
                silent: false,
            });
        }
    }

    /// Pushes every chunk fully captured by real time `now`.
    pub fn feed_until(&mut self, mixer: &mut Mixer, now: Hns) {
        for c in self.chunks_until(now) {
            mixer.push(c);
        }
    }
}

/// Runs the mixer `delay` behind real time until `frames` output frames are mixed.
pub fn run(h: &mut Harness, feeders: &mut [Feeder], frames: usize, delay: Hns) {
    while h.mixed < frames {
        let due = h.next_due(delay);
        for f in feeders.iter_mut() {
            f.feed_until(&mut h.mixer, due);
        }
        h.mix_next();
    }
}

/// Least-squares fit of a sine of `freq` (plus DC) to channel `ch` of interleaved stereo `x`.
/// Returns (amplitude, SNR dB of the residual).
pub fn sine_fit(x: &[f32], ch: usize, freq: f64, rate: f64) -> (f64, f64) {
    let s: Vec<f64> = x.as_chunks::<2>().0.iter().map(|f| f64::from(f[ch])).collect();
    let w = std::f64::consts::TAU * freq / rate;
    // Normal equations for [sin, cos, 1].
    let mut m = [[0.0f64; 3]; 3];
    let mut v = [0.0f64; 3];
    for (i, &y) in s.iter().enumerate() {
        let b = [(w * i as f64).sin(), (w * i as f64).cos(), 1.0];
        for r in 0..3 {
            v[r] += b[r] * y;
            for c in 0..3 {
                m[r][c] += b[r] * b[c];
            }
        }
    }
    let coef = solve3(m, v);
    let (mut sig, mut res) = (0.0, 0.0);
    for (i, &y) in s.iter().enumerate() {
        let fit = coef[0] * (w * i as f64).sin() + coef[1] * (w * i as f64).cos() + coef[2];
        sig += (fit - coef[2]) * (fit - coef[2]);
        res += (y - fit) * (y - fit);
    }
    ((coef[0] * coef[0] + coef[1] * coef[1]).sqrt(), 10.0 * (sig / res.max(1e-30)).log10())
}

fn solve3(mut m: [[f64; 3]; 3], mut v: [f64; 3]) -> [f64; 3] {
    for col in 0..3 {
        let pivot = (col..3).max_by(|&a, &b| m[a][col].abs().total_cmp(&m[b][col].abs())).unwrap();
        m.swap(col, pivot);
        v.swap(col, pivot);
        for r in 0..3 {
            if r != col {
                let f = m[r][col] / m[col][col];
                for c in 0..3 {
                    m[r][c] -= f * m[col][c];
                }
                v[r] -= f * v[col];
            }
        }
    }
    [v[0] / m[0][0], v[1] / m[1][1], v[2] / m[2][2]]
}

pub fn rms(x: &[f32]) -> f64 {
    (x.iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>() / x.len().max(1) as f64).sqrt()
}

pub fn peak(x: &[f32]) -> f32 {
    x.iter().fold(0.0f32, |m, v| m.max(v.abs()))
}

pub fn sine(freq: f64, amp: f32, rate: u32) -> impl FnMut(u64, &mut [f32]) {
    move |i, frame| {
        let v = (std::f64::consts::TAU * freq * i as f64 / f64::from(rate)).sin() as f32 * amp;
        frame.fill(v);
    }
}
