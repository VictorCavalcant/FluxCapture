//! Bottleneck diagnostics: from pipeline metrics to findings with a cause and a suggestion.
//!
//! Each rule looks at one stage and reports the metric that triggered it, so the user sees *why*
//! ("the encoder could not keep up: 37 frames dropped in 10 s, 21 ms per frame for a 16.7 ms
//! budget") and not just "dropped frames".

use serde::{Deserialize, Serialize};

/// Pipeline stage.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Area {
    Capture,
    Composition,
    Encoder,
    Disk,
    Audio,
    System,
    Preview,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

/// A low-risk change the advisor can propose (never applied without the user's approval).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Suggestion {
    LowerResolution,
    LowerFps,
    UseHardwareEncoder,
    FasterEncoderPreset,
    LowerBitrate,
    SimplerScaling,
    ReducePreview,
    ChangeDrive,
    FreeDiskSpace,
    CheckAudioDevice,
    CloseApps,
    RaisePriority,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub area: Area,
    pub severity: Severity,
    pub title: String,
    /// The observed metric.
    pub detail: String,
    pub suggestion: String,
    pub action: Option<Suggestion>,
}

/// Metrics over the last window (deltas are over `window_secs`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub recording: bool,
    pub window_secs: f32,
    pub fps_target: f32,
    pub frames_submitted: u64,
    pub dropped_render: u64,
    pub dropped_encoder: u64,
    pub render_ms_avg: f32,
    pub render_ms_max: f32,
    pub gpu_ms: Option<f32>,
    pub encode_latency_ms: f32,
    pub encoder_hardware: bool,
    pub encoder_name: String,
    /// Lowest delivered frame rate among capture sources.
    pub capture_fps_min: Option<f32>,
    pub sources_lost: Vec<String>,
    /// Achieved file write throughput vs what the stream needs, Mbit/s.
    pub disk_write_mbps: f32,
    pub disk_needed_mbps: f32,
    /// Worst time a write call blocked, ms.
    pub disk_write_ms_max: f32,
    /// Packets waiting for the writer.
    pub writer_backlog: usize,
    pub disk_free_bytes: Option<u64>,
    /// Recording time left on the disk at the current rate.
    pub minutes_left: Option<f32>,
    pub audio_underruns: u64,
    pub audio_overruns: u64,
    pub audio_dropped_chunks: u64,
    pub audio_devices_lost: Vec<String>,
    pub audio_clipping: Vec<String>,
    pub process_cpu: f32,
    pub system_cpu: f32,
    pub memory_available_mb: u64,
    pub preview_enabled: bool,
    pub preview_fps: f32,
    pub replay_mb: f32,
    pub replay_limit_mb: f32,
}

struct Builder {
    en: bool,
    out: Vec<Finding>,
}

impl Builder {
    fn add(&mut self, area: Area, severity: Severity, title: (&str, &str), detail: String, suggestion: (&str, &str), action: Option<Suggestion>) {
        let pick = |p: (&str, &str)| if self.en { p.1.to_string() } else { p.0.to_string() };
        let f = Finding { area, severity, title: pick(title), detail, suggestion: pick(suggestion), action };
        self.out.push(f);
    }
}

/// Findings, most severe first. `en` selects English text (Portuguese otherwise).
pub fn diagnose(s: &Snapshot, en: bool) -> Vec<Finding> {
    let mut b = Builder { en, out: Vec::new() };
    let secs = if s.window_secs.is_finite() && s.window_secs > 0.0 { s.window_secs } else { 1.0 };
    let fps = if s.fps_target.is_finite() && s.fps_target > 0.0 { s.fps_target } else { 60.0 };
    let budget_ms = 1000.0 / fps;
    let expected = (fps * secs).max(1.0);
    let t = |pt: &str, e: &str| if en { e.to_string() } else { pt.to_string() };

    if s.recording && s.dropped_render > 0 {
        let pct = s.dropped_render as f32 / expected * 100.0;
        let sev = if pct > 2.0 { Severity::Critical } else { Severity::Warning };
        let gpu_bound = s.gpu_ms.is_some_and(|g| g > budget_ms * 0.7);
        let cpu_bound = s.render_ms_max > budget_ms;
        let detail = format!(
            "{} {} ({pct:.1}%) {}; {} {:.1}/{:.1} ms, GPU {} ms ({} {budget_ms:.1} ms)",
            s.dropped_render,
            t("quadros perdidos", "frames lost"),
            t(&format!("em {secs:.0} s"), &format!("in {secs:.0} s")),
            t("composição CPU média/pior", "composition CPU avg/worst"),
            s.render_ms_avg,
            s.render_ms_max,
            s.gpu_ms.map_or("?".into(), |g| format!("{g:.1}")),
            t("orçamento", "budget"),
        );
        if gpu_bound {
            b.add(
                Area::Composition,
                sev,
                ("A GPU não acompanha a composição", "The GPU cannot keep up with composition"),
                detail,
                (
                    "Reduza a resolução ou o FPS de saída, use escala bilinear ou limite o FPS do jogo.",
                    "Lower the output resolution or frame rate, use bilinear scaling, or cap the game's frame rate.",
                ),
                Some(Suggestion::LowerResolution),
            );
        } else if cpu_bound {
            b.add(
                Area::System,
                sev,
                ("O processador atrasou a renderização", "The CPU delayed rendering"),
                detail,
                (
                    "Feche programas pesados ou aumente a prioridade do FluxCapture (Configurações › Avançado).",
                    "Close heavy programs or raise FluxCapture's priority (Settings › Advanced).",
                ),
                Some(Suggestion::RaisePriority),
            );
        } else {
            b.add(
                Area::System,
                sev,
                ("O sistema atrasou o relógio de quadros", "The system delayed the frame clock"),
                detail,
                (
                    "Um jogo usando 100% da GPU pode bloquear a composição: limite o FPS do jogo a um pouco abaixo da taxa da tela.",
                    "A game using 100% of the GPU can block composition: cap the game slightly below the display rate.",
                ),
                Some(Suggestion::CloseApps),
            );
        }
    }

    if s.recording && s.dropped_encoder > 0 {
        let pct = s.dropped_encoder as f32 / expected * 100.0;
        let sev = if pct > 2.0 { Severity::Critical } else { Severity::Warning };
        let detail = format!(
            "{} {} ({pct:.1}%); {} {:.1} ms; {}",
            s.dropped_encoder,
            t("quadros descartados pelo encoder", "frames dropped by the encoder"),
            t("latência", "latency"),
            s.encode_latency_ms,
            s.encoder_name
        );
        if s.encoder_hardware {
            b.add(
                Area::Encoder,
                sev,
                ("O encoder de hardware está sobrecarregado", "The hardware encoder is overloaded"),
                detail,
                (
                    "Use um preset mais rápido, H.264 em vez de HEVC/AV1, ou reduza resolução/FPS.",
                    "Use a faster preset, H.264 instead of HEVC/AV1, or lower resolution / frame rate.",
                ),
                Some(Suggestion::FasterEncoderPreset),
            );
        } else {
            b.add(
                Area::Encoder,
                sev,
                ("O encoder por software não acompanha", "The software encoder cannot keep up"),
                detail,
                (
                    "Selecione um encoder de hardware (NVENC, AMF ou Quick Sync) no assistente.",
                    "Select a hardware encoder (NVENC, AMF or Quick Sync) in the assistant.",
                ),
                Some(Suggestion::UseHardwareEncoder),
            );
        }
    }

    for lost in &s.sources_lost {
        b.add(
            Area::Capture,
            Severity::Warning,
            ("Uma origem de captura foi perdida", "A capture source was lost"),
            lost.clone(),
            (
                "A janela pode ter sido fechada ou o monitor desconectado; o FluxCapture tenta reconectar a cada 2 s.",
                "The window may have closed or the monitor was unplugged; FluxCapture retries every 2 s.",
            ),
            None,
        );
    }
    if let Some(cf) = s.capture_fps_min
        && s.recording
        && cf.is_finite()
        && cf < fps * 0.5
        && cf > 0.0
    {
        b.add(
            Area::Capture,
            Severity::Info,
            ("A origem entrega menos quadros que a saída", "The source delivers fewer frames than the output"),
            format!("{cf:.0} fps {} {fps:.0} fps", t("capturados para", "captured for")),
            (
                "Normal para conteúdo parado (o Windows só envia quadros quando algo muda). Em jogos, verifique tela cheia exclusiva ou limite de FPS.",
                "Normal for still content (Windows only sends frames when something changes). For games, check exclusive full screen or a frame cap.",
            ),
            None,
        );
    }

    if s.recording {
        let backlog = s.writer_backlog > 200;
        let slow = s.disk_needed_mbps > 0.0 && s.disk_write_mbps > 0.0 && s.disk_write_mbps < s.disk_needed_mbps * 0.95 && s.writer_backlog > 30;
        if backlog || slow || s.disk_write_ms_max > 1500.0 {
            b.add(
                Area::Disk,
                if backlog { Severity::Critical } else { Severity::Warning },
                ("O disco não acompanha a gravação", "The disk cannot keep up with the recording"),
                format!(
                    "{} {:.1} Mbit/s, {} {:.1} Mbit/s, {} {:.0} ms, {} {}",
                    t("gravando", "writing"),
                    s.disk_write_mbps,
                    t("necessário", "needed"),
                    s.disk_needed_mbps,
                    t("pior escrita", "worst write"),
                    s.disk_write_ms_max,
                    t("pacotes na fila:", "packets queued:"),
                    s.writer_backlog
                ),
                ("Grave em um SSD ou em outro disco, ou reduza a taxa de bits.", "Record to an SSD or another drive, or lower the bitrate."),
                Some(Suggestion::ChangeDrive),
            );
        }
    }
    if let Some(m) = s.minutes_left
        && m.is_finite()
        && m < 60.0
    {
        b.add(
            Area::Disk,
            if m < 10.0 { Severity::Critical } else { Severity::Warning },
            ("Pouco espaço em disco", "Low disk space"),
            format!("~{m:.0} min {}", t("restantes na taxa atual", "left at the current rate")),
            (
                "Libere espaço ou escolha outra pasta; a gravação é finalizada com segurança antes de o disco encher.",
                "Free space or pick another folder; the recording is finalized safely before the disk fills.",
            ),
            Some(Suggestion::FreeDiskSpace),
        );
    }

    for d in &s.audio_devices_lost {
        b.add(
            Area::Audio,
            Severity::Warning,
            ("Dispositivo de áudio indisponível", "Audio device unavailable"),
            d.clone(),
            (
                "Reconecte o dispositivo; a captura volta sozinha (o trecho ausente fica em silêncio).",
                "Reconnect the device; capture resumes by itself (the gap is silent).",
            ),
            Some(Suggestion::CheckAudioDevice),
        );
    }
    if s.audio_underruns > 0 || s.audio_dropped_chunks > 0 || s.audio_overruns > 0 {
        b.add(
            Area::Audio,
            Severity::Warning,
            ("Falhas no fluxo de áudio", "Audio stream glitches"),
            format!(
                "{} {}, {} {}, {} {}",
                s.audio_underruns,
                t("faltas de dados", "underruns"),
                s.audio_overruns,
                t("excessos", "overruns"),
                s.audio_dropped_chunks,
                t("blocos descartados", "dropped blocks")
            ),
            ("Aumente o atraso do mixer (Avançado) ou atualize o driver de áudio.", "Raise the mixer delay (Advanced) or update the audio driver."),
            Some(Suggestion::CheckAudioDevice),
        );
    }
    for c in &s.audio_clipping {
        b.add(
            Area::Audio,
            Severity::Info,
            ("Áudio saturando (clipping)", "Audio clipping"),
            c.clone(),
            ("Reduza o volume ou o ganho dessa fonte.", "Lower this source's volume or gain."),
            None,
        );
    }

    if s.system_cpu > 92.0 {
        b.add(
            Area::System,
            Severity::Warning,
            ("Processador quase no limite", "CPU nearly saturated"),
            format!("{} {:.0}% (FluxCapture {:.1}%)", t("sistema", "system"), s.system_cpu, s.process_cpu),
            ("Feche programas em segundo plano.", "Close background programs."),
            Some(Suggestion::CloseApps),
        );
    }
    if s.memory_available_mb > 0 && s.memory_available_mb < 700 {
        b.add(
            Area::System,
            Severity::Warning,
            ("Pouca memória livre", "Low free memory"),
            format!("{} MB {}", s.memory_available_mb, t("disponíveis", "available")),
            ("Reduza o buffer de replay ou feche programas.", "Shrink the replay buffer or close programs."),
            None,
        );
    }
    if s.recording && s.preview_enabled && s.preview_fps > 20.0 && (s.dropped_render > 0 || s.dropped_encoder > 0) {
        b.add(
            Area::Preview,
            Severity::Info,
            ("A prévia consome recursos durante a gravação", "The preview costs resources while recording"),
            format!("{} {:.0} fps", t("prévia a", "preview at"), s.preview_fps),
            ("Ative “Pausar prévia durante a gravação” ou reduza o FPS da prévia.", "Enable “Pause preview while recording” or lower the preview frame rate."),
            Some(Suggestion::ReducePreview),
        );
    }
    if s.replay_limit_mb > 0.0 && s.replay_mb > s.replay_limit_mb * 0.98 {
        b.add(
            Area::System,
            Severity::Info,
            ("Buffer de replay no limite de memória", "Replay buffer at its memory limit"),
            format!("{:.0}/{:.0} MB", s.replay_mb, s.replay_limit_mb),
            (
                "O replay guarda menos segundos que o configurado; aumente o limite ou reduza a taxa de bits.",
                "The replay holds fewer seconds than configured; raise the limit or lower the bitrate.",
            ),
            Some(Suggestion::LowerBitrate),
        );
    }
    b.out.sort_by_key(|f| std::cmp::Reverse(f.severity));
    b.out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn healthy_pipeline_has_no_findings() {
        let s = Snapshot { recording: true, window_secs: 10.0, fps_target: 60.0, capture_fps_min: Some(60.0), ..Snapshot::default() };
        assert!(diagnose(&s, false).is_empty());
    }

    #[test]
    fn names_the_bottleneck() {
        let s = Snapshot {
            recording: true,
            window_secs: 10.0,
            fps_target: 60.0,
            dropped_encoder: 40,
            encoder_hardware: false,
            dropped_render: 3,
            gpu_ms: Some(15.0),
            minutes_left: Some(5.0),
            ..Snapshot::default()
        };
        let f = diagnose(&s, true);
        assert_eq!(f[0].severity, Severity::Critical);
        assert!(f.iter().any(|x| x.area == Area::Encoder && x.action == Some(Suggestion::UseHardwareEncoder)));
        assert!(f.iter().any(|x| x.area == Area::Composition));
        assert!(f.iter().any(|x| x.area == Area::Disk && x.action == Some(Suggestion::FreeDiskSpace)));
    }

    #[test]
    fn hostile_numbers() {
        let s = Snapshot {
            recording: true,
            window_secs: f32::NAN,
            fps_target: -1.0,
            dropped_render: u64::MAX,
            capture_fps_min: Some(f32::NAN),
            minutes_left: Some(f32::INFINITY),
            ..Snapshot::default()
        };
        let _ = diagnose(&s, false);
    }
}
