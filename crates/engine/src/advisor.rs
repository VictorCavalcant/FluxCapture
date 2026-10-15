//! Auto Performance: turns diagnostic findings into concrete, low-risk setting changes.
//!
//! Proposals that touch the output (resolution, frame rate, encoder, quality) are only *proposed*:
//! the user applies them, and locked parameters are never proposed. Preview reductions are the
//! one thing the engine applies on its own (they never affect the file), and it says so.

use fluxcapture_platform::ScaleFilter;
use fluxcapture_settings::{OutputResolution, Profile, Settings};
use fluxcapture_telemetry::{Finding, Suggestion};
use fluxcapture_time::FrameRate;

/// A change the user can apply with one click.
#[derive(Clone, Debug, PartialEq)]
pub struct Proposal {
    pub title: String,
    pub change: Change,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Change {
    Resolution(OutputResolution),
    Fps(FrameRate),
    SpeedQuality(u8),
    AutoEncoder,
    Scale(ScaleFilter),
    Preview { fps: u32, pause_while_recording: bool },
    Bitrate(u32),
}

impl Change {
    pub fn apply(&self, p: &mut Profile) {
        match self {
            Change::Resolution(r) => p.video.resolution = *r,
            Change::Fps(f) => p.video.fps = *f,
            Change::SpeedQuality(s) => {
                p.video.speed_quality = *s;
                p.video.low_latency = true;
            }
            Change::AutoEncoder => p.video.encoder = None,
            Change::Scale(s) => p.video.scale_filter = *s,
            Change::Preview { fps, pause_while_recording } => {
                p.preview.fps = *fps;
                p.preview.pause_while_recording = *pause_while_recording;
            }
            Change::Bitrate(k) => p.video.bitrate_kbps = *k,
        }
    }
}

fn lower_resolution(r: OutputResolution, native: (u32, u32)) -> Option<OutputResolution> {
    let (_, h) = r.resolve(native);
    match h {
        h if h > 1440 => Some(OutputResolution::P1440),
        h if h > 1080 => Some(OutputResolution::P1080),
        h if h > 720 => Some(OutputResolution::P720),
        _ => None,
    }
}

/// Proposals for the current findings (deduplicated, at most one per kind of change).
pub fn proposals(findings: &[Finding], settings: &Settings, native: (u32, u32), en: bool) -> Vec<Proposal> {
    let p = settings.profile();
    let locks = &p.locks;
    let t = |pt: &str, e: &str| if en { e.to_string() } else { pt.to_string() };
    let mut out: Vec<Proposal> = Vec::new();
    let mut push = |prop: Proposal| {
        if !out.iter().any(|o| std::mem::discriminant(&o.change) == std::mem::discriminant(&prop.change)) {
            out.push(prop);
        }
    };
    for f in findings {
        let Some(s) = f.action else { continue };
        match s {
            Suggestion::LowerResolution if !locks.resolution => {
                if let Some(r) = lower_resolution(p.video.resolution, native) {
                    push(Proposal {
                        title: format!("{} {}", t("Reduzir a resolução de saída para", "Lower the output resolution to"), r.label()),
                        change: Change::Resolution(r),
                    });
                } else if !locks.fps && p.video.fps.as_f64() > 31.0 {
                    push(Proposal { title: t("Gravar a 30 FPS", "Record at 30 fps"), change: Change::Fps(FrameRate::FPS_30) });
                }
            }
            Suggestion::LowerFps if !locks.fps && p.video.fps.as_f64() > 31.0 => {
                push(Proposal { title: t("Gravar a 30 FPS", "Record at 30 fps"), change: Change::Fps(FrameRate::FPS_30) });
            }
            Suggestion::FasterEncoderPreset if !locks.encoder && p.video.speed_quality > 0 => {
                let s = p.video.speed_quality.saturating_sub(40);
                push(Proposal {
                    title: format!("{} ({s})", t("Usar um preset mais rápido no encoder", "Use a faster encoder preset")),
                    change: Change::SpeedQuality(s),
                });
            }
            Suggestion::UseHardwareEncoder if !locks.encoder => {
                push(Proposal {
                    title: t("Escolher automaticamente o encoder de hardware", "Pick the hardware encoder automatically"),
                    change: Change::AutoEncoder,
                });
            }
            Suggestion::SimplerScaling if p.video.scale_filter != ScaleFilter::Bilinear => {
                push(Proposal { title: t("Usar escala bilinear (mais leve)", "Use bilinear scaling (lighter)"), change: Change::Scale(ScaleFilter::Bilinear) });
            }
            Suggestion::ReducePreview => {
                push(Proposal {
                    title: t("Pausar a prévia durante a gravação", "Pause the preview while recording"),
                    change: Change::Preview { fps: p.preview.fps.min(10), pause_while_recording: true },
                });
            }
            Suggestion::LowerBitrate if !locks.quality => {
                let current = if p.video.bitrate_kbps == 0 {
                    let (w, h) = p.video.resolution.resolve(native);
                    fluxcapture_settings::recommended_kbps(w, h, p.video.fps.as_f64(), p.video.codec)
                } else {
                    p.video.bitrate_kbps
                };
                let k = (current * 3 / 4).max(1000);
                push(Proposal { title: format!("{} {} kbit/s", t("Reduzir a taxa de bits para", "Lower the bitrate to"), k), change: Change::Bitrate(k) });
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluxcapture_telemetry::{Area, Severity};

    fn finding(a: Suggestion) -> Finding {
        Finding { area: Area::Encoder, severity: Severity::Warning, title: String::new(), detail: String::new(), suggestion: String::new(), action: Some(a) }
    }

    #[test]
    fn respects_locks() {
        let mut s = Settings::default();
        let f = [finding(Suggestion::LowerResolution), finding(Suggestion::FasterEncoderPreset), finding(Suggestion::LowerResolution)];
        let p = proposals(&f, &s, (2560, 1440), false);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].change, Change::Resolution(OutputResolution::P1080));
        s.profile_mut().locks.resolution = true;
        s.profile_mut().locks.encoder = true;
        let p = proposals(&f, &s, (2560, 1440), false);
        assert!(p.is_empty(), "{p:?}");
        let mut prof = Profile::default();
        Change::Fps(FrameRate::FPS_30).apply(&mut prof);
        assert_eq!(prof.video.fps, FrameRate::FPS_30);
    }
}
