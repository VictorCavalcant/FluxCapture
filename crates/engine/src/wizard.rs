//! The setup assistant: measures this machine and recommends a configuration.
//!
//! Steps: disk write speed in the recording folder, then a short real recording test (primary
//! monitor, 60 fps) with each hardware encoder of the selected GPU, at native resolution and — if
//! that drops frames — at 1080p. The recommendation is the most compatible configuration that ran
//! without dropping frames, with its disk usage per hour. Nothing is changed until the user applies it.

use std::io::Write;
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, bounded};
use fluxcapture_platform::video::{VideoEngine, VideoEngineConfig};
use fluxcapture_platform::{
    CaptureOptions, CaptureTarget, Crop, Fit, Layer, LayerSource, PreviewConfig, RateControl, RectF, ScaleFilter, VideoCodecKind, VideoEncoderConfig,
};
use fluxcapture_settings::{OutputResolution, recommended_kbps};
use fluxcapture_time::FrameRate;

#[derive(Clone, Debug, PartialEq)]
pub struct EncoderTest {
    pub name: String,
    pub codec: VideoCodecKind,
    pub hardware: bool,
    pub width: u32,
    pub height: u32,
    pub frames: u64,
    pub dropped: u64,
    pub latency_ms: f32,
    pub render_ms: f32,
    pub gpu_ms: Option<f32>,
    pub error: Option<String>,
}

impl EncoderTest {
    pub fn ok(&self) -> bool {
        self.error.is_none() && self.frames > 0 && self.dropped * 100 <= self.frames
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Recommendation {
    pub codec: VideoCodecKind,
    pub encoder_id: Option<String>,
    pub encoder_name: String,
    pub resolution: OutputResolution,
    pub fps: FrameRate,
    pub bitrate_kbps: u32,
    pub gb_per_hour: f32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct WizardReport {
    pub gpu: String,
    pub disk_write_mbps: Option<f32>,
    pub tests: Vec<EncoderTest>,
    pub recommendation: Option<Recommendation>,
    pub notes: Vec<String>,
}

enum Msg {
    Progress(String),
    Done(WizardReport),
}

pub struct WizardJob {
    rx: Receiver<Msg>,
    progress: Vec<String>,
    report: Option<WizardReport>,
    _thread: Option<JoinHandle<()>>,
}

impl WizardJob {
    pub fn start(gpu: Option<u64>, native: (u32, u32), folder: PathBuf, en: bool) -> WizardJob {
        let (tx, rx) = bounded(64);
        let thread = std::thread::Builder::new()
            .name("fluxcapture-wizard".into())
            .spawn(move || {
                let tx2 = tx.clone();
                let report = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run(gpu, native, &folder, en, &|m| {
                        let _ = tx2.send(Msg::Progress(m));
                    })
                }))
                .unwrap_or_else(|_| WizardReport { notes: vec!["internal error in the assistant".into()], ..WizardReport::default() });
                let _ = tx.send(Msg::Done(report));
            })
            .ok();
        WizardJob { rx, progress: Vec::new(), report: None, _thread: thread }
    }

    pub fn poll(&mut self) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Progress(p) => self.progress.push(p),
                Msg::Done(r) => self.report = Some(r),
            }
        }
    }

    pub fn progress(&self) -> &[String] {
        &self.progress
    }

    pub fn report(&self) -> Option<&WizardReport> {
        self.report.as_ref()
    }

    pub fn running(&self) -> bool {
        self.report.is_none()
    }
}

fn disk_speed(folder: &std::path::Path) -> Option<f32> {
    std::fs::create_dir_all(folder).ok()?;
    let path = folder.join(".fluxcapture-speedtest.tmp");
    let chunk = vec![0x5Au8; 4 << 20];
    let t0 = Instant::now();
    let r = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&path)?;
        for _ in 0..64 {
            f.write_all(&chunk)?;
        }
        f.sync_all()
    })();
    let secs = t0.elapsed().as_secs_f32();
    let _ = std::fs::remove_file(&path);
    r.ok()?;
    Some(256.0 * 8.0 * 1.048_576 / secs.max(1e-3))
}

fn test_encoder(gpu: Option<u64>, id: Option<String>, codec: VideoCodecKind, w: u32, h: u32) -> EncoderTest {
    let mut t = EncoderTest {
        name: String::new(),
        codec,
        hardware: false,
        width: w,
        height: h,
        frames: 0,
        dropped: 0,
        latency_ms: 0.0,
        render_ms: 0.0,
        gpu_ms: None,
        error: None,
    };
    let fps = FrameRate::FPS_60;
    let engine = match VideoEngine::new(VideoEngineConfig {
        gpu,
        width: w,
        height: h,
        fps,
        scale_filter: ScaleFilter::Bicubic,
        preview: PreviewConfig { enabled: false, ..PreviewConfig::default() },
        variable_frame_rate: false,
    }) {
        Ok(e) => e,
        Err(e) => {
            t.error = Some(e.0);
            return t;
        }
    };
    engine.set_scene(vec![Layer {
        id: 1,
        source: LayerSource::Capture { target: CaptureTarget::Monitor { id: String::new() }, options: CaptureOptions::default() },
        rect: RectF { x: 0.0, y: 0.0, w: w as f32, h: h as f32 },
        fit: Fit::Contain,
        crop: Crop::default(),
        opacity: 1.0,
        visible: true,
    }]);
    let (tx, rx) = bounded(1024);
    let kbps = recommended_kbps(w, h, 60.0, codec);
    let cfg = VideoEncoderConfig {
        encoder_id: id,
        codec,
        width: w,
        height: h,
        fps,
        rate: RateControl::Vbr { kbps, max_kbps: kbps * 3 / 2 },
        keyframe_interval_s: 2.0,
        speed_quality: 50,
        low_latency: true,
        full_range: false,
    };
    match engine.start_encoder(cfg, tx) {
        Ok(info) => {
            t.name = info.name;
            t.hardware = info.hardware;
        }
        Err(e) => {
            t.error = Some(e.0);
            return t;
        }
    }
    // Warm up, then measure 3 s.
    std::thread::sleep(Duration::from_millis(700));
    let before = engine.stats();
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(3) {
        while rx.try_recv().is_ok() {}
        std::thread::sleep(Duration::from_millis(20));
    }
    let after = engine.stats();
    engine.stop_encoder();
    t.frames = after.frames_submitted.saturating_sub(before.frames_submitted);
    t.dropped = (after.dropped_encoder_backlog + after.dropped_render_lag).saturating_sub(before.dropped_encoder_backlog + before.dropped_render_lag);
    t.latency_ms = after.encode_latency_ms_avg;
    t.render_ms = after.render_ms_avg;
    t.gpu_ms = after.gpu_ms_avg;
    if let Some(e) = after.encoder_error {
        t.error = Some(e);
    }
    t
}

fn run(gpu: Option<u64>, native: (u32, u32), folder: &std::path::Path, en: bool, progress: &dyn Fn(String)) -> WizardReport {
    let tr = |pt: &str, e: &str| if en { e.to_string() } else { pt.to_string() };
    let mut report = WizardReport::default();
    let gpus = fluxcapture_platform::display::list_gpus().unwrap_or_default();
    let luid = gpu.or_else(|| gpus.iter().find(|g| !g.software).map(|g| g.luid));
    report.gpu = gpus.iter().find(|g| Some(g.luid) == luid).map(|g| g.name.clone()).unwrap_or_default();
    progress(tr("Medindo a velocidade do disco…", "Measuring disk speed…"));
    report.disk_write_mbps = disk_speed(folder);
    let encoders = fluxcapture_platform::video::list_video_encoders().unwrap_or_default();
    let vendor = gpus.iter().find(|g| Some(g.luid) == luid).map(|g| g.vendor);
    // Some vendors' MFTs do not say which adapter they belong to: fall back to the vendor.
    let on_this_gpu = |e: &fluxcapture_platform::VideoEncoderInfo| match e.gpu_luid {
        Some(l) => Some(l) == luid,
        None => e.vendor.is_some() && e.vendor == vendor,
    };
    let mine: Vec<_> = encoders.iter().filter(|e| e.hardware && on_this_gpu(e) && e.codec != VideoCodecKind::Av1).cloned().collect();
    let (nw, nh) = (native.0.clamp(16, 3840) & !1, native.1.clamp(16, 2160) & !1);
    let mut candidates: Vec<(Option<String>, VideoCodecKind)> = mine.iter().map(|e| (Some(e.id.clone()), e.codec)).collect();
    if candidates.is_empty() {
        report.notes.push(tr(
            "Nenhum encoder de hardware encontrado nesta GPU: o encoder por software será usado (mais CPU).",
            "No hardware encoder on this GPU: the software encoder will be used (more CPU).",
        ));
        candidates.push((None, VideoCodecKind::H264));
    }
    for (id, codec) in &candidates {
        progress(format!("{} {} {}×{}…", tr("Testando", "Testing"), codec.label(), nw, nh));
        let t = test_encoder(luid, id.clone(), *codec, nw, nh);
        let ok = t.ok();
        report.tests.push(t);
        if !ok && nh > 1080 {
            progress(format!("{} {} 1920×1080…", tr("Testando", "Testing"), codec.label()));
            report.tests.push(test_encoder(luid, id.clone(), *codec, 1920, 1080));
        }
    }
    // Prefer H.264 (plays and edits everywhere), then HEVC; native before 1080p.
    let mut ok: Vec<&EncoderTest> = report.tests.iter().filter(|t| t.ok()).collect();
    ok.sort_by_key(|t| (t.codec != VideoCodecKind::H264, std::cmp::Reverse(t.height), !t.hardware));
    if let Some(best) = ok.first() {
        let id = mine.iter().find(|e| e.name == best.name && e.codec == best.codec).map(|e| e.id.clone());
        let resolution = if (best.width, best.height) == (nw, nh) { OutputResolution::Native } else { OutputResolution::P1080 };
        let kbps = recommended_kbps(best.width, best.height, 60.0, best.codec);
        let gb_per_hour = (kbps as f32 + 3.0 * 192.0) * 1000.0 / 8.0 * 3600.0 / 1e9;
        report.recommendation = Some(Recommendation {
            codec: best.codec,
            encoder_id: id,
            encoder_name: best.name.clone(),
            resolution,
            fps: FrameRate::FPS_60,
            bitrate_kbps: kbps,
            gb_per_hour,
        });
        if let Some(d) = report.disk_write_mbps
            && d < kbps as f32 / 1000.0 * 3.0
        {
            report.notes.push(tr(
                "O disco é lento para esta taxa de bits: prefira gravar em um SSD.",
                "The disk is slow for this bitrate: prefer recording to an SSD.",
            ));
        }
        if report.tests.iter().any(|t| t.codec == VideoCodecKind::Hevc && t.ok()) && best.codec == VideoCodecKind::H264 {
            report.notes.push(tr(
                "HEVC também funciona aqui: arquivos ~35% menores com a mesma qualidade (perfil Qualidade).",
                "HEVC also works here: ~35% smaller files at the same quality (Quality profile).",
            ));
        }
    } else {
        report.notes.push(tr(
            "Nenhuma configuração gravou sem perder quadros; tente 30 FPS ou feche programas pesados.",
            "No configuration recorded without dropping frames; try 30 fps or close heavy programs.",
        ));
    }
    progress(tr("Concluído.", "Done."));
    report
}
