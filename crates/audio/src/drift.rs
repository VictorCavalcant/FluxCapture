//! Clock-drift controller.
//!
//! A source's samples are appended to its timeline contiguously, so after `t` seconds a device whose
//! clock runs `d` (e.g. +150 ppm) fast relative to QPC has delivered `t·d` seconds more audio than
//! the QPC time elapsed: its contiguous placement runs ahead of its timestamps. The measured error
//! `e = timestamp position − contiguous position` (seconds) therefore obeys `de/dt = c − d`, where `c`
//! is the ratio trim applied to the resampler (`step = nominal · (1 + c)`).
//!
//! The controller is a PI loop on a low-passed error:
//! * the error is clamped to ±20 ms (outliers do not wind it up) and low-passed with τ = 1 s, which
//!   averages away timestamp jitter (±several ms per 10 ms packet) to well under 0.1 ms;
//! * `c = −(KP·e_lp + KI·∫e_lp)` with `KI = 0.01 s⁻²`, `KP = 0.14 s⁻¹`: natural frequency 0.1 rad/s,
//!   damping ≈ 0.7 — it settles in about a minute, the residual pitch wobble from jitter is a few ppm
//!   and a constant drift leaves zero steady-state error (the integrator converges to `d`);
//! * the trim is clamped to ±0.5 % (the integrator is clamped too: anti-windup).
//!
//! The integral term is the drift estimate reported to the user. It survives re-anchoring: a gap or
//! discontinuity does not change the device's clock.

use crate::resample::MAX_CORRECTION;

const KP: f64 = 0.14;
const KI: f64 = 0.01;
const LP_TAU: f64 = 1.0;
const ERROR_CLAMP: f64 = 0.020;

#[derive(Clone, Debug, Default)]
pub(crate) struct DriftController {
    err_lp: f64,
    integ: f64,
    corr: f64,
}

impl DriftController {
    /// Feeds one measurement: `err` seconds of timeline error over a chunk lasting `dt` seconds.
    pub(crate) fn update(&mut self, err: f64, dt: f64) {
        if !err.is_finite() || !dt.is_finite() {
            return;
        }
        let dt = dt.clamp(0.0, 0.5);
        let e = err.clamp(-ERROR_CLAMP, ERROR_CLAMP);
        self.err_lp += (e - self.err_lp) * (1.0 - (-dt / LP_TAU).exp());
        let limit = MAX_CORRECTION / KI;
        self.integ = (self.integ + self.err_lp * dt).clamp(-limit, limit);
        self.corr = (-(KP * self.err_lp + KI * self.integ)).clamp(-MAX_CORRECTION, MAX_CORRECTION);
    }

    /// Forgets the error history (after re-anchoring) but keeps the drift estimate.
    pub(crate) fn reanchored(&mut self) {
        self.err_lp = 0.0;
        self.corr = (-KI * self.integ).clamp(-MAX_CORRECTION, MAX_CORRECTION);
    }

    /// Ratio trim to apply to the resampler.
    pub(crate) fn correction(&self) -> f64 {
        self.corr
    }

    /// Estimated device clock drift relative to QPC, in ppm (positive: device runs fast).
    pub(crate) fn drift_ppm(&self) -> f64 {
        -KI * self.integ * 1e6
    }

    /// Low-passed timeline error in seconds.
    pub(crate) fn error(&self) -> f64 {
        self.err_lp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converges_on_constant_drift() {
        // Integrate the plant de/dt = c - d directly.
        let d = 150e-6;
        let mut c = DriftController::default();
        let mut e = 0.0;
        for _ in 0..60_000 {
            e += (c.correction() - d) * 0.01;
            c.update(e, 0.01);
        }
        assert!(e.abs() < 1e-4, "error {e}");
        assert!((c.drift_ppm() - 150.0).abs() < 2.0, "drift {}", c.drift_ppm());
        c.update(f64::NAN, 0.01);
        assert!(c.correction().is_finite());
    }
}
