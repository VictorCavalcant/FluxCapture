//! User-facing settings. Every struct deserializes with defaults for missing fields and has a
//! `sanitized()` that clamps each field into its valid range (NaN becomes the default), so values
//! read from a profile file can never put the DSP into an invalid state.

use serde::{Deserialize, Serialize};

use crate::MAX_TRACKS;

/// Clamps `v` into `lo..=hi`; NaN becomes `default`. Infinities clamp to the bounds.
pub(crate) fn clamp_f(v: f32, lo: f32, hi: f32, default: f32) -> f32 {
    if v.is_nan() { default } else { v.clamp(lo, hi) }
}

/// Per-source settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SourceSettings {
    /// Fader in dB, `-60..=+20`. Use `muted` for silence.
    pub volume_db: f32,
    /// Mute (ramped, no click).
    pub muted: bool,
    /// Sync offset in ms, `-2000..=2000`; positive delays the source.
    pub sync_offset_ms: i32,
    /// Sum L and R to mono after the filters.
    pub force_mono: bool,
    /// Balance, `-1` (left only) ..= `1` (right only); centre keeps both channels at unity.
    pub balance: f32,
    /// Output tracks this source is mixed into, bit 0 = track 1.
    pub tracks: u8,
    /// `true` for microphones (missing data is an underrun); `false` for loopback, which delivers
    /// nothing while no application plays sound (missing data is silence, a resume re-anchors).
    pub continuous: bool,
    /// Filter chain.
    pub filters: FilterSettings,
}

impl Default for SourceSettings {
    fn default() -> Self {
        SourceSettings {
            volume_db: 0.0,
            muted: false,
            sync_offset_ms: 0,
            force_mono: false,
            balance: 0.0,
            tracks: 1,
            continuous: true,
            filters: FilterSettings::default(),
        }
    }
}

impl SourceSettings {
    /// Lowest fader value.
    pub const MIN_VOLUME_DB: f32 = -60.0;
    /// Highest fader value.
    pub const MAX_VOLUME_DB: f32 = 20.0;
    /// Largest sync offset magnitude.
    pub const MAX_SYNC_OFFSET_MS: i32 = 2000;

    /// Every field clamped into its valid range.
    pub fn sanitized(&self) -> Self {
        SourceSettings {
            volume_db: clamp_f(self.volume_db, Self::MIN_VOLUME_DB, Self::MAX_VOLUME_DB, 0.0),
            muted: self.muted,
            sync_offset_ms: self.sync_offset_ms.clamp(-Self::MAX_SYNC_OFFSET_MS, Self::MAX_SYNC_OFFSET_MS),
            force_mono: self.force_mono,
            balance: clamp_f(self.balance, -1.0, 1.0, 0.0),
            tracks: self.tracks & ((1u8 << MAX_TRACKS) - 1),
            continuous: self.continuous,
            filters: self.filters.sanitized(),
        }
    }
}

/// The per-source filter chain, applied in the order of the fields.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FilterSettings {
    /// Input gain in dB, `-30..=30`, applied first.
    pub gain_db: f32,
    /// 2nd-order Butterworth high-pass.
    pub high_pass: Option<HighPassSettings>,
    /// STFT spectral noise reduction.
    pub noise_suppression: Option<NoiseSuppressionSettings>,
    /// Noise gate.
    pub gate: Option<GateSettings>,
    /// Feed-forward compressor.
    pub compressor: Option<CompressorSettings>,
}

impl FilterSettings {
    /// Every field clamped into its valid range.
    pub fn sanitized(&self) -> Self {
        FilterSettings {
            gain_db: clamp_f(self.gain_db, -30.0, 30.0, 0.0),
            high_pass: self.high_pass.as_ref().map(HighPassSettings::sanitized),
            noise_suppression: self.noise_suppression.as_ref().map(NoiseSuppressionSettings::sanitized),
            gate: self.gate.as_ref().map(GateSettings::sanitized),
            compressor: self.compressor.as_ref().map(CompressorSettings::sanitized),
        }
    }
}

/// High-pass filter.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HighPassSettings {
    /// Cutoff (-3 dB) in Hz, `10..=1000`.
    pub cutoff_hz: f32,
}

impl Default for HighPassSettings {
    fn default() -> Self {
        HighPassSettings { cutoff_hz: 80.0 }
    }
}

impl HighPassSettings {
    /// Every field clamped into its valid range.
    pub fn sanitized(&self) -> Self {
        HighPassSettings { cutoff_hz: clamp_f(self.cutoff_hz, 10.0, 1000.0, 80.0) }
    }
}

/// Noise suppression.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NoiseSuppressionSettings {
    /// `0..=1`: 0 leaves the signal untouched, 1 removes up to 30 dB of stationary noise.
    pub strength: f32,
}

impl Default for NoiseSuppressionSettings {
    fn default() -> Self {
        NoiseSuppressionSettings { strength: 0.6 }
    }
}

impl NoiseSuppressionSettings {
    /// Every field clamped into its valid range.
    pub fn sanitized(&self) -> Self {
        NoiseSuppressionSettings { strength: clamp_f(self.strength, 0.0, 1.0, 0.6) }
    }
}

/// Noise gate with hysteresis.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GateSettings {
    /// The gate opens when the level rises above this (dBFS, `-96..=0`).
    pub open_db: f32,
    /// The gate starts closing when the level falls below this (dBFS, `-96..=open_db`).
    pub close_db: f32,
    /// Fade-in time in ms, `0.1..=500`.
    pub attack_ms: f32,
    /// Time the gate stays open after the level fell below `close_db`, ms `0..=5000`.
    pub hold_ms: f32,
    /// Fade-out time in ms, `1..=5000`.
    pub release_ms: f32,
}

impl Default for GateSettings {
    fn default() -> Self {
        GateSettings { open_db: -26.0, close_db: -32.0, attack_ms: 25.0, hold_ms: 200.0, release_ms: 150.0 }
    }
}

impl GateSettings {
    /// Every field clamped into its valid range (`close_db` never above `open_db`).
    pub fn sanitized(&self) -> Self {
        let d = GateSettings::default();
        let open_db = clamp_f(self.open_db, -96.0, 0.0, d.open_db);
        let close_db = clamp_f(self.close_db, -96.0, 0.0, d.close_db.min(open_db)).min(open_db);
        GateSettings {
            open_db,
            close_db,
            attack_ms: clamp_f(self.attack_ms, 0.1, 500.0, d.attack_ms),
            hold_ms: clamp_f(self.hold_ms, 0.0, 5000.0, d.hold_ms),
            release_ms: clamp_f(self.release_ms, 1.0, 5000.0, d.release_ms),
        }
    }
}

/// Feed-forward compressor (RMS detector, 6 dB soft knee).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompressorSettings {
    /// Threshold in dBFS, `-60..=0`.
    pub threshold_db: f32,
    /// Ratio `1..=32`.
    pub ratio: f32,
    /// Attack in ms, `0.1..=500`.
    pub attack_ms: f32,
    /// Release in ms, `1..=5000`.
    pub release_ms: f32,
    /// Makeup gain in dB, `0..=30`.
    pub makeup_db: f32,
}

impl Default for CompressorSettings {
    fn default() -> Self {
        CompressorSettings { threshold_db: -18.0, ratio: 4.0, attack_ms: 6.0, release_ms: 60.0, makeup_db: 0.0 }
    }
}

impl CompressorSettings {
    /// Every field clamped into its valid range.
    pub fn sanitized(&self) -> Self {
        let d = CompressorSettings::default();
        CompressorSettings {
            threshold_db: clamp_f(self.threshold_db, -60.0, 0.0, d.threshold_db),
            ratio: clamp_f(self.ratio, 1.0, 32.0, d.ratio),
            attack_ms: clamp_f(self.attack_ms, 0.1, 500.0, d.attack_ms),
            release_ms: clamp_f(self.release_ms, 1.0, 5000.0, d.release_ms),
            makeup_db: clamp_f(self.makeup_db, 0.0, 30.0, d.makeup_db),
        }
    }
}

/// Mixer-wide settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MixerSettings {
    /// Number of output tracks, `1..=MAX_TRACKS`.
    pub tracks: usize,
    /// Master volume applied to every track, dB `-60..=20`.
    pub master_volume_db: f32,
    /// Lookahead peak limiter on every track.
    pub limiter: Option<LimiterSettings>,
}

impl Default for MixerSettings {
    fn default() -> Self {
        MixerSettings { tracks: 1, master_volume_db: 0.0, limiter: Some(LimiterSettings::default()) }
    }
}

impl MixerSettings {
    /// Every field clamped into its valid range.
    pub fn sanitized(&self) -> Self {
        MixerSettings {
            tracks: self.tracks.clamp(1, MAX_TRACKS),
            master_volume_db: clamp_f(self.master_volume_db, -60.0, 20.0, 0.0),
            limiter: self.limiter.as_ref().map(LimiterSettings::sanitized),
        }
    }
}

/// Peak limiter.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LimiterSettings {
    /// Output never exceeds this (dBFS, `-30..=0`).
    pub ceiling_db: f32,
    /// Release time in ms, `1..=2000`.
    pub release_ms: f32,
}

impl Default for LimiterSettings {
    fn default() -> Self {
        LimiterSettings { ceiling_db: -1.0, release_ms: 60.0 }
    }
}

impl LimiterSettings {
    /// Every field clamped into its valid range.
    pub fn sanitized(&self) -> Self {
        LimiterSettings { ceiling_db: clamp_f(self.ceiling_db, -30.0, 0.0, -1.0), release_ms: clamp_f(self.release_ms, 1.0, 2000.0, 60.0) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostile_values_are_clamped() {
        let s = SourceSettings {
            volume_db: f32::NAN,
            sync_offset_ms: i32::MIN,
            balance: f32::INFINITY,
            tracks: 0xFF,
            filters: FilterSettings {
                gain_db: f32::NEG_INFINITY,
                high_pass: Some(HighPassSettings { cutoff_hz: f32::NAN }),
                noise_suppression: Some(NoiseSuppressionSettings { strength: 7.0 }),
                gate: Some(GateSettings { open_db: -200.0, close_db: 5.0, attack_ms: -1.0, hold_ms: f32::NAN, release_ms: 1e30 }),
                compressor: Some(CompressorSettings { threshold_db: 9.0, ratio: 0.0, attack_ms: 0.0, release_ms: f32::NAN, makeup_db: -5.0 }),
            },
            ..SourceSettings::default()
        }
        .sanitized();
        assert_eq!(s.volume_db, 0.0);
        assert_eq!(s.sync_offset_ms, -2000);
        assert_eq!(s.balance, 1.0);
        assert_eq!(s.tracks, 0x3F);
        assert_eq!(s.filters.gain_db, -30.0);
        assert_eq!(s.filters.high_pass, Some(HighPassSettings { cutoff_hz: 80.0 }));
        let g = s.filters.gate.unwrap();
        assert_eq!(g.open_db, -96.0);
        assert!(g.close_db <= g.open_db);
        assert_eq!(g.attack_ms, 0.1);
        assert_eq!(g.release_ms, 5000.0);
        let c = s.filters.compressor.unwrap();
        assert_eq!(c.ratio, 1.0);
        assert_eq!(c.threshold_db, 0.0);
        let m = MixerSettings { tracks: 0, master_volume_db: f32::NAN, limiter: Some(LimiterSettings { ceiling_db: 3.0, release_ms: 0.0 }) }.sanitized();
        assert_eq!(m.tracks, 1);
        assert_eq!(m.limiter.unwrap().ceiling_db, 0.0);
        assert_eq!(MixerSettings { tracks: 99, ..Default::default() }.sanitized().tracks, MAX_TRACKS);
    }
}
