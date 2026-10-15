//! Level meters (post-fader per source, post-limiter per track).

use serde::{Deserialize, Serialize};

use crate::SourceId;
use crate::util::gain_to_db;

/// Silence is reported as this many dBFS (not `-inf`, so snapshots serialise cleanly).
pub const METER_FLOOR_DB: f32 = -100.0;

const PEAK_FALL_DB_PER_S: f32 = 20.0;
const RMS_TAU_S: f32 = 0.3;
const HOLD_S: f32 = 1.5;

/// A stereo level reading.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LevelMeter {
    /// Peak level per channel (instant attack, 20 dB/s fall-back), dBFS.
    pub peak_db: [f32; 2],
    /// RMS level per channel (300 ms), dBFS.
    pub rms_db: [f32; 2],
    /// Highest peak of the last 1.5 s per channel, dBFS.
    pub peak_hold_db: [f32; 2],
    /// A sample reached or exceeded full scale since the last `reset_clip`.
    pub clipped: bool,
}

/// Meter of one source (post-fader).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SourceMeter {
    /// Source.
    pub id: SourceId,
    /// Display name given to `add_source`.
    pub name: String,
    /// Levels.
    pub level: LevelMeter,
}

fn to_db(v: f32) -> f32 {
    gain_to_db(v).max(METER_FLOOR_DB)
}

#[derive(Clone, Debug, Default)]
pub(crate) struct MeterState {
    win_peak: [f32; 2],
    win_sq: [f64; 2],
    win_frames: usize,
    peak: [f32; 2],
    ms: [f32; 2],
    hold: [f32; 2],
    hold_age: [f32; 2],
    clipped: bool,
}

impl MeterState {
    /// Accumulates a block of interleaved stereo.
    pub(crate) fn accumulate(&mut self, buf: &[f32]) {
        // 8 lanes (4 frames: even lanes L, odd lanes R) so the max/sum reductions vectorise.
        let mut peak = [0f32; 8];
        let mut sq = [0f32; 8];
        let (chunks, rest) = buf.as_chunks::<8>();
        for c in chunks {
            for j in 0..8 {
                peak[j] = peak[j].max(c[j].abs());
                sq[j] += c[j] * c[j];
            }
        }
        for (j, &v) in rest.iter().enumerate() {
            peak[j] = peak[j].max(v.abs());
            sq[j] += v * v;
        }
        for c in 0..2 {
            let p = (0..4).fold(self.win_peak[c], |m, k| m.max(peak[2 * k + c]));
            self.win_peak[c] = p;
            self.win_sq[c] += (0..4).map(|k| f64::from(sq[2 * k + c])).sum::<f64>();
        }
        self.win_frames += buf.len() / 2;
    }

    /// Applies ballistics for the window just accumulated (`dt` seconds long).
    pub(crate) fn finish(&mut self, dt: f32) {
        if self.win_frames == 0 {
            return;
        }
        let fall = 10f32.powf(-PEAK_FALL_DB_PER_S * dt / 20.0);
        let alpha = 1.0 - (-dt / RMS_TAU_S).exp();
        for c in 0..2 {
            let wp = self.win_peak[c];
            if wp >= 1.0 {
                self.clipped = true;
            }
            self.peak[c] = wp.max(self.peak[c] * fall);
            let ms = (self.win_sq[c] / self.win_frames as f64) as f32;
            self.ms[c] += (ms - self.ms[c]) * alpha;
            if wp >= self.hold[c] {
                self.hold[c] = wp;
                self.hold_age[c] = 0.0;
            } else {
                self.hold_age[c] += dt;
                if self.hold_age[c] > HOLD_S {
                    self.hold[c] = self.peak[c];
                }
            }
            if !self.ms[c].is_finite() {
                self.ms[c] = 0.0;
            }
        }
        self.win_peak = [0.0; 2];
        self.win_sq = [0.0; 2];
        self.win_frames = 0;
    }

    pub(crate) fn reset_clip(&mut self) {
        self.clipped = false;
    }

    pub(crate) fn reading(&self) -> LevelMeter {
        LevelMeter {
            peak_db: [to_db(self.peak[0]), to_db(self.peak[1])],
            rms_db: [to_db(self.ms[0].sqrt()), to_db(self.ms[1].sqrt())],
            peak_hold_db: [to_db(self.hold[0]), to_db(self.hold[1])],
            clipped: self.clipped,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ballistics() {
        let mut m = MeterState::default();
        assert_eq!(m.reading().peak_db, [METER_FLOOR_DB; 2]);
        m.accumulate(&[0.5, 1.0, -0.5, 0.0]);
        m.finish(0.01);
        let r = m.reading();
        assert!((r.peak_db[0] + 6.02).abs() < 0.01);
        assert!(r.clipped);
        m.reset_clip();
        for _ in 0..100 {
            m.accumulate(&[0.0, 0.0]);
            m.finish(0.01);
        }
        let r = m.reading();
        assert!(!r.clipped);
        assert!(r.peak_db[0] < -25.0);
    }
}
