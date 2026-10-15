//! `fluxcapture-cli` — FluxCapture without a window.
//!
//! ```text
//! fluxcapture-cli devices
//! fluxcapture-cli record  [--seconds N] [options]        record (Enter stops early)
//! fluxcapture-cli replay  [--seconds N] [--clip S] [options]
//! fluxcapture-cli bench   [--seconds N] [--json] [options]
//! fluxcapture-cli wizard
//! fluxcapture-cli remux   IN.mkv [OUT.mp4]
//! fluxcapture-cli recover IN.mkv [OUT.mkv]
//!
//! options: --out DIR  --codec h264|hevc  --fps N  --res native|720p|1080p|1440p|2160p|WxH
//!          --rate quality:N|vbr:KBPS|cbr:KBPS  --monitor ID  --window TEXT  --region X,Y,W,H
//!          --no-audio  --mp4  --profile N  --config DIR  --encoder ID
//! ```

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::io::BufRead;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use fluxcapture_engine::{Command, Engine, EngineOptions, EventKind};
use fluxcapture_platform::{CaptureOptions, Crop, Fit, RectF, VideoCodecKind};
use fluxcapture_settings::{OutputResolution, RateMode, Scene, SceneSource, Settings, SourceKind};
use fluxcapture_time::FrameRate;

struct Args {
    cmd: String,
    positional: Vec<String>,
    flags: Vec<(String, Option<String>)>,
}

impl Args {
    fn parse() -> Args {
        let mut it = std::env::args().skip(1);
        let cmd = it.next().unwrap_or_else(|| "help".into());
        let mut positional = Vec::new();
        let mut flags = Vec::new();
        let rest: Vec<String> = it.collect();
        let mut i = 0;
        while i < rest.len() {
            let a = &rest[i];
            if let Some(name) = a.strip_prefix("--") {
                let takes_value = !matches!(name, "no-audio" | "mp4" | "json");
                let v = if takes_value { rest.get(i + 1).cloned() } else { None };
                if v.is_some() {
                    i += 1;
                }
                flags.push((name.to_string(), v));
            } else {
                positional.push(a.clone());
            }
            i += 1;
        }
        Args { cmd, positional, flags }
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.flags.iter().rev().find(|(n, _)| n == name).and_then(|(_, v)| v.as_deref())
    }

    fn has(&self, name: &str) -> bool {
        self.flags.iter().any(|(n, _)| n == name)
    }
}

fn usage() {
    println!("{}", include_str!("usage.txt"));
}

fn main() -> ExitCode {
    let args = Args::parse();
    let r = match args.cmd.as_str() {
        "devices" => devices(),
        "record" => record(&args, Mode::Record),
        "replay" => record(&args, Mode::Replay),
        "bench" => record(&args, Mode::Bench),
        "wizard" => wizard(&args),
        "remux" => remux(&args),
        "recover" => recover(&args),
        "help" | "--help" | "-h" => {
            usage();
            Ok(())
        }
        other => Err(format!("unknown command {other:?} (try `fluxcapture-cli help`)")),
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn devices() -> Result<(), String> {
    use fluxcapture_platform::{audio, display, video};
    println!("{}", fluxcapture_platform::system::os_description());
    println!("\nGPUs:");
    for g in display::list_gpus().map_err(|e| e.0)? {
        println!(
            "  [{:016x}] {} ({}, {} MB VRAM){}",
            g.luid,
            g.name,
            g.vendor.label(),
            g.dedicated_video_memory / 1_048_576,
            if g.software { " software" } else { "" }
        );
    }
    println!("\nMonitors:");
    for m in display::list_monitors().map_err(|e| e.0)? {
        println!(
            "  {} {} {}x{} @ {:.0} Hz{}{}",
            m.id,
            m.name,
            m.width,
            m.height,
            m.refresh_hz,
            if m.primary { " primary" } else { "" },
            if m.hdr { " HDR" } else { "" }
        );
    }
    println!("\nWindows:");
    for w in display::list_windows().map_err(|e| e.0)?.iter().take(40) {
        println!("  {:>10} {:<24} {}", w.handle, w.process, w.title.chars().take(70).collect::<String>());
    }
    println!("\nVideo encoders:");
    for e in video::list_video_encoders().map_err(|e| e.0)? {
        println!("  {:<6} {:<10} {} ({})", e.codec.label(), if e.hardware { "hardware" } else { "software" }, e.name, e.id);
    }
    println!("\nAudio devices:");
    for d in audio::list_audio_devices().map_err(|e| e.0)? {
        println!("  {:?}{} {}", d.flow, if d.default { " (default)" } else { "" }, d.name);
    }
    println!("\nApps playing audio:");
    for p in audio::list_audio_processes().unwrap_or_default() {
        println!("  {:>6} {}", p.pid, p.name);
    }
    println!("\nWebcams:");
    for (id, name) in video::list_webcams().unwrap_or_default() {
        println!("  {name} ({id})");
    }
    println!("\nCapture: Windows.Graphics.Capture {}", if video::capture_supported() { "available" } else { "NOT available" });
    println!("Per-app audio capture: {}", if audio::process_capture_supported() { "available" } else { "not available" });
    Ok(())
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Mode {
    Record,
    Replay,
    Bench,
}

fn apply_overrides(s: &mut Settings, args: &Args, mode: Mode) -> Result<(), String> {
    if let Some(p) = args.get("profile") {
        s.active_profile = p.parse().map_err(|_| "--profile takes an index")?;
    }
    if let Some(out) = args.get("out") {
        s.recording.folder = PathBuf::from(out);
    } else if mode == Mode::Bench {
        s.recording.folder = std::env::temp_dir().join("fluxcapture-bench");
    }
    if args.has("mp4") {
        s.recording.remux_to_mp4 = true;
        s.replay.save_mp4 = true;
    }
    let mut scene: Option<Scene> = None;
    let full = RectF { x: 0.0, y: 0.0, w: 1.0, h: 1.0 };
    if let Some(title) = args.get("window") {
        let w = fluxcapture_platform::display::list_windows()
            .map_err(|e| e.0)?
            .into_iter()
            .find(|w| w.title.to_lowercase().contains(&title.to_lowercase()) || w.process.eq_ignore_ascii_case(title))
            .ok_or_else(|| format!("no window matches {title:?}"))?;
        scene = Some(Scene {
            name: "CLI".into(),
            sources: vec![SceneSource {
                id: 1,
                name: w.title.clone(),
                kind: SourceKind::Window { handle: w.handle, title: w.title, process: w.process, options: CaptureOptions::default() },
                rect: full,
                fit: Fit::Contain,
                crop: Crop::default(),
                opacity: 1.0,
                visible: true,
            }],
        });
    } else if args.get("monitor").is_some() || args.get("region").is_some() {
        let mut crop = Crop::default();
        let monitor = args.get("monitor").unwrap_or("").to_string();
        if let Some(r) = args.get("region") {
            let v: Vec<u32> = r.split(',').filter_map(|x| x.trim().parse().ok()).collect();
            let [x, y, w, h] = v.as_slice() else { return Err("--region takes X,Y,W,H".into()) };
            let mons = fluxcapture_platform::display::list_monitors().map_err(|e| e.0)?;
            let m = mons.iter().find(|m| if monitor.is_empty() { m.primary } else { m.id == monitor }).ok_or("monitor not found")?;
            crop = Crop { left: *x, top: *y, right: m.width.saturating_sub(x + w), bottom: m.height.saturating_sub(y + h) };
        }
        scene = Some(Scene {
            name: "CLI".into(),
            sources: vec![SceneSource {
                id: 1,
                name: "Monitor".into(),
                kind: SourceKind::Display { monitor, options: CaptureOptions::default() },
                rect: full,
                fit: Fit::Contain,
                crop,
                opacity: 1.0,
                visible: true,
            }],
        });
    }
    if let Some(sc) = scene {
        s.scenes = vec![sc];
        s.active_scene = 0;
    }
    if args.has("no-audio") {
        for a in &mut s.audio_sources {
            a.enabled = false;
        }
    }
    let p = s.profile_mut();
    if let Some(c) = args.get("codec") {
        p.video.codec = match c {
            "h264" | "avc" => VideoCodecKind::H264,
            "hevc" | "h265" => VideoCodecKind::Hevc,
            _ => return Err("--codec h264|hevc".into()),
        };
    }
    if let Some(f) = args.get("fps") {
        p.video.fps = FrameRate::from_fps(f.parse().map_err(|_| "--fps takes a number")?).map_err(|e| e.to_string())?;
    }
    if let Some(e) = args.get("encoder") {
        p.video.encoder = Some(e.to_string());
    }
    if let Some(r) = args.get("res") {
        p.video.resolution = match r {
            "native" => OutputResolution::Native,
            "720p" => OutputResolution::P720,
            "1080p" => OutputResolution::P1080,
            "1440p" => OutputResolution::P1440,
            "2160p" | "4k" => OutputResolution::P2160,
            wh => {
                let (w, h) = wh.split_once('x').ok_or("--res native|720p|1080p|1440p|2160p|WxH")?;
                OutputResolution::Custom { width: w.parse().map_err(|_| "bad width")?, height: h.parse().map_err(|_| "bad height")? }
            }
        };
    }
    if let Some(r) = args.get("rate") {
        let (mode, v) = r.split_once(':').ok_or("--rate quality:N|vbr:KBPS|cbr:KBPS")?;
        let n: u32 = v.parse().map_err(|_| "--rate value must be a number")?;
        match mode {
            "quality" => {
                p.video.rate_mode = RateMode::Quality;
                p.video.quality = n.min(100) as u8;
            }
            "vbr" => {
                p.video.rate_mode = RateMode::Vbr;
                p.video.bitrate_kbps = n;
            }
            "cbr" => {
                p.video.rate_mode = RateMode::Cbr;
                p.video.bitrate_kbps = n;
            }
            _ => return Err("--rate quality:N|vbr:KBPS|cbr:KBPS".into()),
        }
    }
    p.preview.enabled = false;
    Ok(())
}

fn print_events(engine: &mut Engine) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for e in engine.take_events() {
        let tag = match e.kind {
            EventKind::Info => "info",
            EventKind::Success => "ok",
            EventKind::Warning => "warning",
            EventKind::Error => "ERROR",
        };
        match &e.path {
            Some(p) => {
                println!("[{tag}] {} → {}", e.text, p.display());
                files.push(p.clone());
            }
            None => println!("[{tag}] {}", e.text),
        }
    }
    files
}

fn record(args: &Args, mode: Mode) -> Result<(), String> {
    let seconds: f64 = args.get("seconds").map(|s| s.parse().map_err(|_| "--seconds takes a number")).transpose()?.unwrap_or(match mode {
        Mode::Record => 3600.0 * 24.0,
        Mode::Replay => 15.0,
        Mode::Bench => 20.0,
    });
    let opts = EngineOptions { config_dir: args.get("config").map(PathBuf::from), headless: true, read_only_settings: true };
    let mut engine = Engine::new(opts);
    let mut s = engine.settings().clone();
    apply_overrides(&mut s, args, mode)?;
    if mode == Mode::Replay {
        s.replay.seconds = args.get("clip").and_then(|c| c.parse().ok()).unwrap_or(10);
    }
    engine.set_settings_transient(s);
    // Let sources start before the clock runs.
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(600) {
        engine.tick();
        std::thread::sleep(Duration::from_millis(20));
    }
    print_events(&mut engine);
    match mode {
        Mode::Record | Mode::Bench => engine.dispatch(Command::StartRecording)?,
        Mode::Replay => engine.dispatch(Command::ToggleReplay)?,
    }
    let (out, out_rx) = std::sync::mpsc::channel::<()>();
    if mode == Mode::Record {
        std::thread::spawn(move || {
            // Only a real Enter stops; EOF (no console attached) means "run for --seconds".
            let mut line = String::new();
            if std::io::stdin().lock().read_line(&mut line).is_ok_and(|n| n > 0) {
                let _ = out.send(());
            }
        });
        println!("Recording… press Enter to stop.");
    }
    let started = Instant::now();
    let mut last_print = Instant::now();
    let mut cpu_samples = Vec::new();
    let mut mem_peak = 0u64;
    while started.elapsed().as_secs_f64() < seconds && out_rx.try_recv().is_err() {
        engine.tick();
        print_events(&mut engine);
        if last_print.elapsed() >= Duration::from_secs(1) {
            last_print = Instant::now();
            let st = engine.state();
            let v = &st.video;
            if started.elapsed() > Duration::from_secs(2) {
                cpu_samples.push(st.cpu_process);
            }
            mem_peak = mem_peak.max(st.mem_process_mb);
            if !args.has("json") {
                println!(
                    "{:>8}  {:>7.1} MB  {}  render {:.2} ms  gpu {} ms  enc {:.1} ms  drop {}/{}  cpu {:.1}%  mem {} MB{}",
                    if mode == Mode::Replay { format!("{:.0} s", st.replay_seconds) } else { st.rec.duration.hms() },
                    st.rec.bytes as f64 / 1e6,
                    st.encoder.as_ref().map(|e| e.name.as_str()).unwrap_or("-"),
                    v.render_ms_avg,
                    v.gpu_ms_avg.map_or("-".into(), |g| format!("{g:.2}")),
                    v.encode_latency_ms_avg,
                    v.dropped_render_lag,
                    v.dropped_encoder_backlog,
                    st.cpu_process,
                    st.mem_process_mb,
                    st.findings.iter().find(|f| f.severity >= fluxcapture_telemetry_severity()).map(|f| format!("  ⚠ {}", f.title)).unwrap_or_default()
                );
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let final_state = engine.state().clone();
    match mode {
        Mode::Record | Mode::Bench => engine.dispatch(Command::StopRecording)?,
        Mode::Replay => engine.dispatch(Command::SaveReplay)?,
    }
    // Wait for files to be finalized.
    let mut files = Vec::new();
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(120) {
        engine.tick();
        files.extend(print_events(&mut engine));
        if !files.is_empty() && engine.state().rec.finishing == 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if mode == Mode::Bench {
        let v = &final_state.video;
        let avg_cpu = if cpu_samples.is_empty() { 0.0 } else { cpu_samples.iter().sum::<f32>() / cpu_samples.len() as f32 };
        let report = serde_json::json!({
            "seconds": seconds,
            "output": [final_state.output.0, final_state.output.1],
            "fps": v.fps_target,
            "encoder": final_state.encoder.as_ref().map(|e| e.name.clone()),
            "gpu": final_state.gpu_name,
            "frames_submitted": v.frames_submitted,
            "frames_encoded": v.frames_encoded,
            "dropped_render": v.dropped_render_lag,
            "dropped_encoder": v.dropped_encoder_backlog,
            "render_ms_avg": v.render_ms_avg,
            "gpu_ms_avg": v.gpu_ms_avg,
            "encode_latency_ms": v.encode_latency_ms_avg,
            "process_cpu_percent_avg": avg_cpu,
            "process_memory_peak_mb": mem_peak,
            "files": files,
        });
        println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
        for f in &files {
            let _ = std::fs::remove_file(f);
        }
    }
    engine.shutdown();
    if files.is_empty() { Err("no file was produced".into()) } else { Ok(()) }
}

fn fluxcapture_telemetry_severity() -> fluxcapture_engine::Severity {
    fluxcapture_engine::Severity::Warning
}

fn wizard(args: &Args) -> Result<(), String> {
    let mut engine = Engine::new(EngineOptions { config_dir: args.get("config").map(PathBuf::from), headless: true, read_only_settings: true });
    engine.start_wizard()?;
    let mut shown = 0;
    loop {
        engine.tick();
        let Some(w) = engine.wizard() else { break };
        for p in w.progress().iter().skip(shown) {
            println!("… {p}");
        }
        shown = w.progress().len();
        if let Some(r) = w.report() {
            println!("\nGPU: {}", r.gpu);
            if let Some(d) = r.disk_write_mbps {
                println!("Disk write: {d:.0} Mbit/s");
            }
            for t in &r.tests {
                println!(
                    "  {:<6} {}x{} {:<28} {} frames, {} dropped, enc {:.1} ms, render {:.2} ms{}",
                    t.codec.label(),
                    t.width,
                    t.height,
                    t.name,
                    t.frames,
                    t.dropped,
                    t.latency_ms,
                    t.render_ms,
                    t.error.as_ref().map(|e| format!(" — {e}")).unwrap_or_default()
                );
            }
            if let Some(rec) = &r.recommendation {
                println!(
                    "\nRecommended: {} via {} at {} {} fps, {} kbit/s (~{:.1} GB/h)",
                    rec.codec.label(),
                    rec.encoder_name,
                    rec.resolution.label(),
                    rec.fps.label(),
                    rec.bitrate_kbps,
                    rec.gb_per_hour
                );
            }
            for n in &r.notes {
                println!("note: {n}");
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

fn remux(args: &Args) -> Result<(), String> {
    let src = PathBuf::from(args.positional.first().ok_or("remux IN.mkv [OUT.mp4]")?);
    let dst = args.positional.get(1).map(PathBuf::from).unwrap_or_else(|| src.with_extension("mp4"));
    let r = fluxcapture_mux::remux::remux_mkv_to_mp4(&src, &dst).map_err(|e| e.to_string())?;
    println!("{} frames, {:.1} s → {}", r.frames, r.duration.as_secs_f64(), dst.display());
    Ok(())
}

fn recover(args: &Args) -> Result<(), String> {
    let Some(first) = args.positional.first() else {
        // No file: recover every interrupted session found in the journal.
        let mut engine = Engine::new(EngineOptions { config_dir: args.get("config").map(PathBuf::from), headless: true, read_only_settings: true });
        let n = engine.pending_recoveries().len();
        println!("{n} interrupted session(s)");
        for _ in 0..n {
            engine.recover(0);
        }
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(120) && n > 0 {
            engine.tick();
            if !print_events(&mut engine).is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        return Ok(());
    };
    let src = PathBuf::from(first);
    let dst = args.positional.get(1).map(PathBuf::from).unwrap_or_else(|| fluxcapture_engine_recovered(&src));
    let r = fluxcapture_mux::remux::recover_mkv(&src, &dst).map_err(|e| e.to_string())?;
    println!("{} frames, {:.1} s recovered ({} bytes cut) → {}", r.frames, r.duration.as_secs_f64(), r.dropped_tail_bytes, dst.display());
    Ok(())
}

fn fluxcapture_engine_recovered(src: &std::path::Path) -> PathBuf {
    let stem = src.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    src.with_file_name(format!("{stem} (recuperado).mkv"))
}
