//! The FluxCapture application core.
//!
//! [`Engine`] owns the live session: the video engine (capture → compose → preview/encode, always
//! on while the app shows a preview), the audio system (captures + mixer, always on for meters),
//! and — only while recording or the replay buffer is on — the encoders and the packet router.
//! The UI (or the CLI, or the control channel) calls [`Engine::dispatch`] with [`Command`]s and
//! [`Engine::tick`] regularly; all real-time work happens on the platform/pipeline threads, so a
//! slow or frozen UI never affects a recording.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod advisor;
pub mod audio_system;
pub mod command;
pub mod control;
pub mod scene;
pub mod wizard;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, bounded};
use fluxcapture_mux::codec::aac_audio_specific_config;
use fluxcapture_mux::{AudioTrack, VideoCodec};
use fluxcapture_pipeline::journal::{self, JournalEntry};
use fluxcapture_pipeline::{RecState, RecordConfig, Recording, ReplayConfig, Router, RouterStats, Split, StopReason, StreamSetup};
use fluxcapture_platform::hotkeys::HotkeyListener;
use fluxcapture_platform::video::{VideoEngine, VideoEngineConfig};
use fluxcapture_platform::{
    AudioDeviceInfo, AudioEndpoint, EncodedAudio, EncodedVideo, GpuInfo, MonitorInfo, PreviewFrame, SourceState, VideoCodecKind, VideoEncoderConfig,
    VideoEncoderInfo, VideoStats, WindowInfo,
};
use fluxcapture_settings::template::{Stamp, expand};
use fluxcapture_settings::{AudioCodec, HotkeyAction, Priority, Settings, TrackLayout};
use fluxcapture_telemetry::logger::LogHandle;
use fluxcapture_telemetry::{CpuMeter, Finding, History, Snapshot, diagnose};
use fluxcapture_time::Hns;

pub use advisor::Proposal;
pub use audio_system::AudioSnapshot;
pub use command::Command;
pub use fluxcapture_telemetry::{Area, Severity};

/// How the engine starts.
#[derive(Clone, Debug, Default)]
pub struct EngineOptions {
    /// Configuration folder (default: portable folder or %APPDATA%\FluxCapture).
    pub config_dir: Option<PathBuf>,
    /// No preview, no hotkeys (CLI / tests).
    pub headless: bool,
    /// Do not write the configuration back (CLI runs with overrides).
    pub read_only_settings: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum EventKind {
    Info,
    Success,
    Warning,
    Error,
}

/// Something to tell the user (a toast in the UI, a line in the CLI).
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub kind: EventKind,
    pub text: String,
    /// A file the event is about (open / reveal).
    pub path: Option<PathBuf>,
}

/// Recording status for display.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RecView {
    pub active: bool,
    pub paused: bool,
    pub state: Option<RecState>,
    pub duration: Hns,
    pub bytes: u64,
    pub file: Option<PathBuf>,
    pub parts: usize,
    pub finishing: usize,
    pub warning: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Histories {
    pub cpu: History,
    pub render_ms: History,
    pub gpu_ms: History,
    pub bitrate_kbps: History,
    pub dropped: History,
}

impl Default for Histories {
    fn default() -> Self {
        Histories {
            cpu: History::new(120),
            render_ms: History::new(120),
            gpu_ms: History::new(120),
            bitrate_kbps: History::new(120),
            dropped: History::new(120),
        }
    }
}

/// Everything the UI shows, refreshed by [`Engine::tick`].
#[derive(Clone, Debug, Default)]
pub struct EngineState {
    pub rec: RecView,
    pub replay_enabled: bool,
    pub replay_seconds: f32,
    pub replay_bytes: usize,
    pub encoding: bool,
    pub encoder: Option<VideoEncoderInfo>,
    pub video: VideoStats,
    pub router: RouterStats,
    pub audio: AudioSnapshot,
    pub sources: Vec<(u64, SourceState)>,
    pub source_errors: Vec<(u64, String)>,
    pub findings: Vec<Finding>,
    pub proposals: Vec<Proposal>,
    pub gpu_name: String,
    pub output: (u32, u32),
    pub native: (u32, u32),
    pub cpu_process: f32,
    pub cpu_system: f32,
    pub mem_process_mb: u64,
    pub mem_available_mb: u64,
    pub disk_free: Option<u64>,
    pub minutes_left: Option<f32>,
    pub history: Histories,
    pub mic_muted: bool,
    pub video_error: Option<String>,
    pub hotkey_failures: Vec<String>,
}

struct EncodeSession {
    router: Router,
    video_tx: Sender<EncodedVideo>,
    audio_tx: Sender<(u8, EncodedAudio)>,
    est_kbps: u32,
}

struct Window {
    at: Instant,
    video: VideoStats,
    audio_underruns: u64,
    audio_overruns: u64,
    audio_dropped: u64,
}

/// Device lists, refreshed on demand.
#[derive(Clone, Debug, Default)]
pub struct Devices {
    pub gpus: Vec<GpuInfo>,
    pub monitors: Vec<MonitorInfo>,
    pub windows: Vec<WindowInfo>,
    pub audio: Vec<AudioDeviceInfo>,
    pub webcams: Vec<(String, String)>,
    pub encoders: Vec<VideoEncoderInfo>,
}

pub struct Engine {
    settings: Settings,
    config_dir: PathBuf,
    opts: EngineOptions,
    log: LogHandle,
    video: Option<VideoEngine>,
    audio: Option<audio_system::AudioSystem>,
    encode: Option<EncodeSession>,
    recording: Option<Recording>,
    finishing: Vec<(Recording, Option<EncodeSession>)>,
    paused: bool,
    replay_on: bool,
    replay_saves: Vec<Receiver<Result<PathBuf, String>>>,
    hotkeys: Option<HotkeyListener>,
    hotkey_rx: Receiver<usize>,
    hotkey_tx: Sender<usize>,
    hotkey_actions: Vec<HotkeyAction>,
    control: Option<control::ControlServer>,
    events: VecDeque<Event>,
    state: EngineState,
    devices: Devices,
    scene_cache: scene::SceneCache,
    recoveries: Vec<(PathBuf, JournalEntry)>,
    recovery_jobs: Vec<Receiver<Event>>,
    dirty_since: Option<Instant>,
    last_metrics: Instant,
    last_diag: Instant,
    windows: VecDeque<Window>,
    cpu: CpuMeter,
    sys_cpu: CpuMeter,
    started: Instant,
    preview_auto_reduced: bool,
    wizard: Option<wizard::WizardJob>,
}

fn local_stamp() -> Stamp {
    let (year, month, day, hour, minute, second) = fluxcapture_platform::system::local_time();
    Stamp { year, month, day, hour, minute, second }
}

impl Engine {
    /// Starts the engine. Never fails: problems become events (and the affected part stays off).
    pub fn new(opts: EngineOptions) -> Engine {
        let config_dir = opts.config_dir.clone().unwrap_or_else(fluxcapture_settings::config_dir);
        let loaded = fluxcapture_settings::load(&config_dir);
        let settings = loaded.settings;
        fluxcapture_telemetry::logger::set_local_time(|| {
            let (_, _, _, h, m, s) = fluxcapture_platform::system::local_time();
            (h, m, s)
        });
        let log = fluxcapture_telemetry::logger::init(Some(&config_dir.join("logs")), settings.advanced.verbose_log);
        log::info!("FluxCapture {} on {}", env!("CARGO_PKG_VERSION"), fluxcapture_platform::system::os_description());
        let (hotkey_tx, hotkey_rx) = bounded(32);
        let mut e = Engine {
            settings,
            config_dir,
            opts,
            log,
            video: None,
            audio: None,
            encode: None,
            recording: None,
            finishing: Vec::new(),
            paused: false,
            replay_on: false,
            replay_saves: Vec::new(),
            hotkeys: None,
            hotkey_rx,
            hotkey_tx,
            hotkey_actions: Vec::new(),
            control: None,
            events: VecDeque::new(),
            state: EngineState::default(),
            devices: Devices::default(),
            scene_cache: scene::SceneCache::default(),
            recoveries: Vec::new(),
            recovery_jobs: Vec::new(),
            dirty_since: None,
            last_metrics: Instant::now() - Duration::from_secs(5),
            last_diag: Instant::now(),
            windows: VecDeque::new(),
            cpu: CpuMeter::default(),
            sys_cpu: CpuMeter::default(),
            started: Instant::now(),
            preview_auto_reduced: false,
            wizard: None,
        };
        if let Some(w) = loaded.warning {
            e.event(EventKind::Warning, w, None);
        }
        let prio = match e.settings.general.process_priority {
            Priority::Normal => fluxcapture_platform::clock::ProcessPriority::Normal,
            Priority::AboveNormal => fluxcapture_platform::clock::ProcessPriority::AboveNormal,
            Priority::High => fluxcapture_platform::clock::ProcessPriority::High,
        };
        if let Err(err) = fluxcapture_platform::clock::set_process_priority(prio) {
            log::warn!("{err}");
        }
        e.refresh_devices();
        e.start_video();
        e.start_audio();
        if !e.opts.headless {
            e.restart_hotkeys();
        }
        e.recoveries = journal::pending(&e.sessions_dir());
        if e.settings.replay.enabled_on_start
            && let Err(err) = e.set_replay(true)
        {
            e.event(EventKind::Error, err, None);
        }
        let port = e.settings.advanced.control_port;
        if port != 0 {
            match control::ControlServer::start(port) {
                Ok(c) => e.control = Some(c),
                Err(err) => e.event(EventKind::Warning, err, None),
            }
        }
        e
    }

    fn sessions_dir(&self) -> PathBuf {
        self.config_dir.join("sessions")
    }

    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    pub fn log(&self) -> &LogHandle {
        &self.log
    }

    fn en(&self) -> bool {
        self.settings.general.language == fluxcapture_settings::Language::En
    }

    fn t(&self, pt: &str, en: &str) -> String {
        if self.en() { en.to_string() } else { pt.to_string() }
    }

    fn event(&mut self, kind: EventKind, text: String, path: Option<PathBuf>) {
        match kind {
            EventKind::Error => log::error!("{text}"),
            EventKind::Warning => log::warn!("{text}"),
            _ => log::info!("{text}"),
        }
        if self.events.len() > 50 {
            self.events.pop_front();
        }
        self.events.push_back(Event { kind, text, path });
    }

    /// Events since the last call.
    pub fn take_events(&mut self) -> Vec<Event> {
        self.events.drain(..).collect()
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn state(&self) -> &EngineState {
        &self.state
    }

    pub fn devices(&self) -> &Devices {
        &self.devices
    }

    pub fn preview(&self) -> Option<Arc<PreviewFrame>> {
        self.video.as_ref().and_then(|v| v.preview())
    }

    pub fn is_recording(&self) -> bool {
        self.recording.is_some()
    }

    pub fn is_encoding(&self) -> bool {
        self.encode.is_some()
    }

    /// Re-enumerates GPUs, monitors, windows, audio devices, webcams and encoders.
    pub fn refresh_devices(&mut self) {
        use fluxcapture_platform::{audio, display, video};
        self.devices.gpus = display::list_gpus().unwrap_or_default();
        self.devices.monitors = display::list_monitors().unwrap_or_default();
        self.devices.windows = display::list_windows().unwrap_or_default();
        self.devices.audio = audio::list_audio_devices().unwrap_or_default();
        self.devices.webcams = video::list_webcams().unwrap_or_default();
        if self.devices.encoders.is_empty() {
            self.devices.encoders = video::list_video_encoders().unwrap_or_default();
        }
    }

    /// Only the window list (cheap; for pickers).
    pub fn refresh_windows(&mut self) {
        self.devices.windows = fluxcapture_platform::display::list_windows().unwrap_or_default();
    }

    fn native_size(&self) -> (u32, u32) {
        scene::native_size(self.settings.scene(), &self.devices.monitors, &self.devices.windows)
    }

    /// The output size for the active profile.
    pub fn output_size(&self) -> (u32, u32) {
        self.settings.profile().video.resolution.resolve(self.native_size())
    }

    fn gpu_choice(&self) -> Option<u64> {
        let a = &self.settings.advanced;
        // LUIDs change across reboots: find the adapter again by name.
        if !a.gpu_name.is_empty()
            && let Some(g) = self.devices.gpus.iter().find(|g| g.name == a.gpu_name)
        {
            return Some(g.luid);
        }
        a.gpu_luid.filter(|l| self.devices.gpus.iter().any(|g| g.luid == *l))
    }

    fn start_video(&mut self) {
        let (w, h) = self.output_size();
        let p = self.settings.profile().clone();
        let mut preview = p.preview.to_platform(false);
        if self.opts.headless {
            preview.enabled = false;
        }
        let cfg = VideoEngineConfig {
            gpu: self.gpu_choice(),
            width: w,
            height: h,
            fps: p.video.fps,
            scale_filter: p.video.scale_filter,
            preview,
            variable_frame_rate: p.video.variable_frame_rate,
        };
        match VideoEngine::new(cfg) {
            Ok(v) => {
                self.state.gpu_name = v.gpu_name().to_string();
                self.video = Some(v);
                self.state.video_error = None;
                self.push_scene();
            }
            Err(e) => {
                let msg = format!("{}: {e}", self.t("O motor de vídeo não pôde iniciar", "The video engine could not start"));
                self.state.video_error = Some(msg.clone());
                self.event(EventKind::Error, msg, None);
            }
        }
    }

    fn push_scene(&mut self) {
        let Some(v) = &self.video else { return };
        let (w, h) = self.state_output();
        let method = self.settings.advanced.capture_method;
        let (layers, errors) = match self.settings.scene() {
            Some(s) => self.scene_cache.layers(s, w, h, method),
            None => (Vec::new(), Vec::new()),
        };
        v.set_scene(layers);
        self.state.source_errors = errors;
    }

    fn state_output(&self) -> (u32, u32) {
        if self.state.output.0 > 0 { self.state.output } else { self.output_size() }
    }

    /// Applies canvas / preview / scene settings to the running video engine.
    fn apply_video(&mut self) {
        let Some(v) = &self.video else {
            self.start_video();
            return;
        };
        let p = self.settings.profile().clone();
        let size = self.output_size();
        if !self.is_encoding() {
            match v.set_canvas(size.0, size.1, p.video.fps, p.video.scale_filter, p.video.variable_frame_rate) {
                Ok(()) => self.state.output = size,
                Err(e) => {
                    let msg = e.to_string();
                    self.event(EventKind::Warning, msg, None);
                }
            }
        }
        let mut preview = p.preview.to_platform(self.is_recording());
        if self.opts.headless {
            preview.enabled = false;
        }
        if let Some(v) = &self.video {
            v.set_preview(preview);
        }
        self.push_scene();
    }

    /// Per-source track masks and the track names for the file.
    fn track_plan(&self) -> (Vec<(fluxcapture_settings::AudioSourceConfig, u8)>, Vec<String>) {
        let enabled: Vec<&fluxcapture_settings::AudioSourceConfig> = self.settings.audio_sources.iter().filter(|a| a.enabled).collect();
        let separate = self.settings.recording.tracks == TrackLayout::MixAndSeparate;
        let mut names = vec![self.t("Mixagem", "Mix")];
        let mut plan = Vec::new();
        for (i, a) in self.settings.audio_sources.iter().enumerate() {
            let pos = enabled.iter().position(|e| e.id == a.id);
            let mut mask = 1u8;
            if separate && let Some(p) = pos.filter(|p| *p < fluxcapture_audio::MAX_TRACKS - 1) {
                mask |= 1 << (1 + p);
                if names.len() < fluxcapture_audio::MAX_TRACKS {
                    names.push(a.name.clone());
                }
            }
            let _ = i;
            plan.push((a.clone(), mask));
        }
        (plan, names)
    }

    fn mix_delay(&self) -> Hns {
        let user = Hns::from_millis(i64::from(self.settings.advanced.audio_mix_delay_ms));
        user.max(fluxcapture_audio::RECOMMENDED_MIX_DELAY)
    }

    fn start_audio(&mut self) {
        let (plan, names) = self.track_plan();
        let mut mixer = self.settings.mixer.clone();
        mixer.tracks = names.len();
        match audio_system::AudioSystem::start(mixer, self.mix_delay()) {
            Ok(a) => {
                a.set_sources(plan);
                self.audio = Some(a);
            }
            Err(e) => self.event(EventKind::Error, e, None),
        }
    }

    fn apply_audio(&mut self) {
        let (plan, names) = self.track_plan();
        let mut mixer = self.settings.mixer.clone();
        mixer.tracks = names.len();
        let delay = self.mix_delay();
        if let Some(a) = &self.audio {
            a.set_mixer(mixer, delay);
            a.set_sources(plan);
        }
    }

    fn restart_hotkeys(&mut self) {
        self.hotkeys = None;
        let mut keys = Vec::new();
        self.hotkey_actions.clear();
        for (action, key) in self.settings.hotkeys.list() {
            if let Some(k) = key {
                keys.push(k);
                self.hotkey_actions.push(action);
            }
        }
        if keys.is_empty() {
            return;
        }
        match HotkeyListener::start(keys, self.hotkey_tx.clone()) {
            Ok(h) => {
                self.state.hotkey_failures = h.failures().into_iter().map(|(_, m)| m).collect();
                self.hotkeys = Some(h);
            }
            Err(e) => self.event(EventKind::Warning, e.to_string(), None),
        }
    }

    /// Changes settings (sanitized, saved shortly after, applied to the running session).
    pub fn update_settings(&mut self, f: impl FnOnce(&mut Settings)) {
        let old = self.settings.clone();
        let mut s = self.settings.clone();
        f(&mut s);
        self.settings = s.sanitized();
        if self.settings == old {
            return;
        }
        self.dirty_since = Some(Instant::now());
        let (np, op) = (self.settings.profile(), old.profile());
        let video_changed = np.video != op.video
            || np.preview != op.preview
            || self.settings.scene() != old.scene()
            || self.settings.advanced.capture_method != old.advanced.capture_method;
        let gpu_changed = self.settings.advanced.gpu_luid != old.advanced.gpu_luid || self.settings.advanced.gpu_name != old.advanced.gpu_name;
        if gpu_changed && !self.is_encoding() {
            self.video = None;
            self.state.output = (0, 0);
            self.start_video();
        } else if video_changed {
            self.apply_video();
        }
        if self.settings.audio_sources != old.audio_sources
            || self.settings.mixer != old.mixer
            || self.settings.recording.tracks != old.recording.tracks
            || self.settings.advanced.audio_mix_delay_ms != old.advanced.audio_mix_delay_ms
        {
            if self.is_encoding()
                && self.track_plan().1.len() != {
                    let s2 = Engine::track_names_for(&old, self.en());
                    s2.len()
                }
            {
                self.event(
                    EventKind::Info,
                    self.t("A mudança de trilhas vale a partir da próxima gravação.", "The track change applies from the next recording."),
                    None,
                );
            }
            self.apply_audio();
        }
        if self.settings.hotkeys != old.hotkeys && !self.opts.headless {
            self.restart_hotkeys();
        }
        if self.settings.advanced.control_port != old.advanced.control_port {
            self.control = None;
            if self.settings.advanced.control_port != 0 {
                match control::ControlServer::start(self.settings.advanced.control_port) {
                    Ok(c) => self.control = Some(c),
                    Err(e) => self.event(EventKind::Warning, e, None),
                }
            }
        }
    }

    fn track_names_for(s: &Settings, en: bool) -> Vec<String> {
        let mut n = vec![if en { "Mix".to_string() } else { "Mixagem".to_string() }];
        if s.recording.tracks == TrackLayout::MixAndSeparate {
            n.extend(s.audio_sources.iter().filter(|a| a.enabled).take(fluxcapture_audio::MAX_TRACKS - 1).map(|a| a.name.clone()));
        }
        n
    }

    /// Saves now (normally done automatically a second after the last change).
    pub fn save_settings(&mut self) {
        self.dirty_since = None;
        if self.opts.read_only_settings {
            return;
        }
        if let Err(e) = fluxcapture_settings::save(&self.config_dir, &self.settings) {
            self.event(EventKind::Error, format!("{}: {e}", self.t("Não foi possível salvar as configurações", "Could not save the settings")), None);
        }
    }

    /// Replaces the settings wholesale without persisting them (CLI overrides).
    pub fn set_settings_transient(&mut self, s: Settings) {
        let ro = self.opts.read_only_settings;
        self.opts.read_only_settings = true;
        self.update_settings(|cur| *cur = s);
        self.opts.read_only_settings = ro;
        self.dirty_since = None;
    }

    fn ensure_encoding(&mut self) -> Result<(), String> {
        if self.encode.is_some() {
            return Ok(());
        }
        if self.video.is_none() {
            self.start_video();
        }
        // Make sure the canvas matches the current settings before the size is frozen.
        self.apply_video();
        let p = self.settings.profile().clone();
        let (w, h) = self.state_output();
        let (_, names) = self.track_plan();
        let mut codec = p.audio.codec;
        if codec == AudioCodec::Pcm && (self.settings.recording.remux_to_mp4 || self.settings.replay.save_mp4) {
            codec = AudioCodec::Aac;
            self.event(EventKind::Info, self.t("Áudio PCM não cabe em MP4: usando AAC.", "PCM audio does not fit MP4: using AAC."), None);
        }
        let video = self.video.as_ref().ok_or_else(|| self.state.video_error.clone().unwrap_or_else(|| "no video engine".into()))?;
        let audio = self.audio.as_ref().ok_or("no audio system")?;
        let asc = aac_audio_specific_config(fluxcapture_audio::OUTPUT_RATE, 2).map(|a| a.to_vec()).unwrap_or_default();
        let audio_tracks: Vec<AudioTrack> = names
            .iter()
            .enumerate()
            .map(|(i, n)| AudioTrack {
                codec: if codec == AudioCodec::Pcm { fluxcapture_mux::AudioCodec::PcmF32 } else { fluxcapture_mux::AudioCodec::Aac },
                sample_rate: fluxcapture_audio::OUTPUT_RATE,
                channels: 2,
                codec_private: if codec == AudioCodec::Pcm { Vec::new() } else { asc.clone() },
                name: n.clone(),
                default: i == 0,
            })
            .collect();
        let vcodec = match p.video.codec {
            VideoCodecKind::H264 => VideoCodec::H264,
            VideoCodecKind::Hevc => VideoCodec::Hevc,
            VideoCodecKind::Av1 => {
                return Err(
                    self.t("AV1 ainda não é suportado no contêiner; escolha H.264 ou HEVC.", "AV1 is not supported in the container yet; pick H.264 or HEVC.")
                );
            }
        };
        let setup = StreamSetup {
            video_codec: vcodec,
            width: w,
            height: h,
            fps: p.video.fps,
            variable_frame_rate: p.video.variable_frame_rate,
            full_range: p.video.full_range,
            audio: audio_tracks,
        };
        let (video_tx, video_rx) = bounded(512);
        let (audio_tx, audio_rx) = bounded(2048);
        let router = Router::start(setup, video_rx, audio_rx)?;
        audio.start_encoding(audio_system::AudioEncodeConfig { tracks: names.len(), codec, kbps: p.audio.bitrate_kbps }, audio_tx.clone())?;
        let rate = p.video.rate_control(w, h);
        let cfg = VideoEncoderConfig {
            encoder_id: p.video.encoder.clone(),
            codec: p.video.codec,
            width: w,
            height: h,
            fps: p.video.fps,
            rate,
            keyframe_interval_s: p.video.keyframe_interval_s,
            speed_quality: p.video.speed_quality,
            low_latency: p.video.low_latency,
            full_range: p.video.full_range,
        };
        let info = match video.start_encoder(cfg, video_tx.clone()) {
            Ok(i) => i,
            Err(e) => {
                audio.stop_encoding();
                return Err(format!("{}: {e}", self.t("O encoder de vídeo não iniciou", "The video encoder did not start")));
            }
        };
        let est_kbps = rate.estimated_kbps(w, h, p.video.fps.as_f64()) + names.len() as u32 * p.audio.bitrate_kbps;
        log::info!("encoding {}x{} @ {} with {} ({} audio tracks)", w, h, p.video.fps, info.name, names.len());
        self.state.encoder = Some(info);
        self.encode = Some(EncodeSession { router, video_tx, audio_tx, est_kbps });
        Ok(())
    }

    /// Stops the encoders; the session (router) is returned so a finishing recording keeps it.
    fn stop_encoding(&mut self) -> Option<EncodeSession> {
        let s = self.encode.take()?;
        if let Some(v) = &self.video {
            v.stop_encoder();
        }
        if let Some(a) = &self.audio {
            a.stop_encoding();
        }
        // Let the router take the drained packets.
        let t0 = Instant::now();
        while (!s.video_tx.is_empty() || !s.audio_tx.is_empty()) && t0.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(5));
        }
        self.state.encoder = None;
        self.apply_video();
        Some(s)
    }

    fn start_recording(&mut self) -> Result<(), String> {
        if self.recording.is_some() {
            return Ok(());
        }
        let folder = self.settings.recording_folder();
        std::fs::create_dir_all(&folder)
            .map_err(|e| format!("{} {}: {e}", self.t("Não foi possível criar a pasta", "Cannot create the folder"), folder.display()))?;
        self.ensure_encoding()?;
        let scene_name = self.settings.scene().map(|s| s.name.clone()).unwrap_or_default();
        let name = expand(&self.settings.recording.filename_template, local_stamp(), &self.settings.profile().name, &scene_name, 1);
        let r = &self.settings.recording;
        let cfg = RecordConfig {
            folder,
            base_name: name,
            split: match r.split {
                fluxcapture_settings::Split::Never => Split::Never,
                fluxcapture_settings::Split::Minutes(m) => Split::Every(Hns::from_secs(i64::from(m) * 60)),
                fluxcapture_settings::Split::Megabytes(mb) => Split::Bytes(u64::from(mb) * 1_000_000),
            },
            min_free_bytes: u64::from(r.min_free_mb) * 1_000_000,
            warn_free_bytes: u64::from(r.warn_free_mb) * 1_000_000,
            remux_mp4: r.remux_to_mp4,
            delete_mkv_after_remux: r.delete_mkv_after_remux,
            journal_dir: Some(self.sessions_dir()),
            disk_free: Some(|p: &Path| fluxcapture_platform::system::disk_free(p).ok()),
            title: format!("FluxCapture — {}", self.settings.profile().name),
        };
        let session = self.encode.as_ref().ok_or("no encoding session")?;
        let rec = match session.router.start_recording(cfg) {
            Ok(r) => r,
            Err(e) => {
                if !self.replay_on {
                    self.stop_encoding();
                }
                return Err(e);
            }
        };
        if let Some(v) = &self.video {
            v.request_keyframe();
        }
        self.recording = Some(rec);
        self.paused = false;
        self.preview_auto_reduced = false;
        self.windows.clear();
        self.apply_video();
        self.event(EventKind::Info, self.t("Gravação iniciada", "Recording started"), None);
        Ok(())
    }

    fn stop_recording(&mut self) {
        if self.recording.is_none() {
            return;
        }
        // Without the replay buffer the encoders stop first, so every frame reaches the file.
        let session = if self.replay_on { None } else { self.stop_encoding() };
        let rec = match &session {
            Some(s) => s.router.stop_recording(),
            None => self.encode.as_ref().and_then(|s| s.router.stop_recording()),
        };
        self.recording = None;
        self.paused = false;
        if let Some(r) = rec {
            self.finishing.push((r, session));
        }
        self.apply_video();
    }

    fn set_replay(&mut self, on: bool) -> Result<(), String> {
        if on == self.replay_on {
            return Ok(());
        }
        if on {
            self.ensure_encoding()?;
            let r = &self.settings.replay;
            let cfg = ReplayConfig { seconds: r.seconds, max_bytes: r.max_memory_mb as usize * 1024 * 1024 };
            if let Some(s) = &self.encode {
                s.router.set_replay(Some(cfg));
            }
            self.replay_on = true;
            self.event(EventKind::Info, format!("{} ({} s)", self.t("Buffer de replay ativo", "Replay buffer on"), self.settings.replay.seconds), None);
        } else {
            if let Some(s) = &self.encode {
                s.router.set_replay(None);
            }
            self.replay_on = false;
            if self.recording.is_none()
                && let Some(s) = self.stop_encoding()
            {
                drop(s);
            }
        }
        Ok(())
    }

    fn save_replay(&mut self) -> Result<(), String> {
        let s =
            self.encode.as_ref().filter(|_| self.replay_on).ok_or_else(|| self.t("Ative o buffer de replay primeiro.", "Turn the replay buffer on first."))?;
        let mp4 = self.settings.replay.save_mp4;
        let name = expand("Replay {date} {time}", local_stamp(), "", "", 1);
        let path = self.settings.recording_folder().join(format!("{name}.{}", if mp4 { "mp4" } else { "mkv" }));
        let rx = s.router.save_replay(path, mp4)?;
        self.replay_saves.push(rx);
        Ok(())
    }

    fn screenshot(&mut self) -> Result<(), String> {
        let v = self.video.as_ref().ok_or("no video engine")?;
        let img = v.screenshot().map_err(|e| e.to_string())?;
        let name = expand("Captura {date} {time}", local_stamp(), "", "", 1);
        let folder = self.settings.recording_folder();
        let path = folder.join(format!("{name}.png"));
        let (tx, rx) = bounded(1);
        let ok_text = self.t("Captura de tela salva", "Screenshot saved");
        std::thread::Builder::new()
            .name("fluxcapture-screenshot".into())
            .spawn(move || {
                let r = std::fs::create_dir_all(&folder)
                    .map_err(|e| e.to_string())
                    .and_then(|()| image::save_buffer(&path, &img.data, img.width, img.height, image::ExtendedColorType::Rgba8).map_err(|e| e.to_string()));
                let ev = match r {
                    Ok(()) => Event { kind: EventKind::Success, text: ok_text, path: Some(path) },
                    Err(e) => Event { kind: EventKind::Error, text: format!("PNG: {e}"), path: None },
                };
                let _ = tx.send(ev);
            })
            .map_err(|e| e.to_string())?;
        self.recovery_jobs.push(rx);
        Ok(())
    }

    fn toggle_mic(&mut self) {
        let mut muted = false;
        self.update_settings(|s| {
            let any_unmuted = s.audio_sources.iter().any(|a| is_mic(&a.endpoint) && !a.settings.muted);
            for a in s.audio_sources.iter_mut().filter(|a| is_mic(&a.endpoint)) {
                a.settings.muted = any_unmuted;
            }
            muted = any_unmuted;
        });
        self.state.mic_muted = muted;
        let text = if muted { self.t("Microfone mudo", "Microphone muted") } else { self.t("Microfone ativo", "Microphone live") };
        self.event(EventKind::Info, text, None);
    }

    /// Runs a command.
    pub fn dispatch(&mut self, cmd: Command) -> Result<(), String> {
        log::debug!("command {}", cmd.id());
        let r = match cmd {
            Command::ToggleRecording => {
                if self.recording.is_some() {
                    self.stop_recording();
                    Ok(())
                } else {
                    self.start_recording()
                }
            }
            Command::StartRecording => self.start_recording(),
            Command::StopRecording => {
                self.stop_recording();
                Ok(())
            }
            Command::TogglePause => match (&self.encode, self.recording.is_some()) {
                (Some(s), true) => {
                    self.paused = !self.paused;
                    s.router.set_paused(self.paused);
                    if !self.paused
                        && let Some(v) = &self.video
                    {
                        v.request_keyframe();
                    }
                    Ok(())
                }
                _ => Err(self.t("Nenhuma gravação em andamento.", "Nothing is being recorded.")),
            },
            Command::ToggleReplay => self.set_replay(!self.replay_on),
            Command::SaveReplay => self.save_replay(),
            Command::Screenshot => self.screenshot(),
            Command::ToggleMicMute => {
                self.toggle_mic();
                Ok(())
            }
            Command::SetProfile(i) => {
                if self.is_encoding() {
                    Err(self.t("Pare a gravação/replay para trocar de perfil.", "Stop recording / replay to switch profiles."))
                } else if i < self.settings.profiles.len() {
                    self.update_settings(|s| s.active_profile = i);
                    Ok(())
                } else {
                    Err("no such profile".into())
                }
            }
            Command::SetScene(i) => {
                if i < self.settings.scenes.len() {
                    self.update_settings(|s| s.active_scene = i);
                    Ok(())
                } else {
                    Err("no such scene".into())
                }
            }
            Command::OpenRecordingsFolder => {
                let f = self.settings.recording_folder();
                let _ = std::fs::create_dir_all(&f);
                fluxcapture_platform::system::reveal_in_explorer(&f).map_err(|e| e.to_string())
            }
            Command::ApplyProposal(i) => {
                let p = self.state.proposals.get(i).cloned().ok_or("no such proposal")?;
                if self.is_encoding() && !matches!(p.change, advisor::Change::Preview { .. }) {
                    return Err(self.t(
                        "Esta mudança vale para a próxima gravação; pare a gravação para aplicá-la.",
                        "This change applies to the next recording; stop recording to apply it.",
                    ));
                }
                self.update_settings(|s| p.change.apply(s.profile_mut()));
                self.event(EventKind::Success, p.title, None);
                Ok(())
            }
            Command::RefreshDevices => {
                self.refresh_devices();
                self.apply_video();
                Ok(())
            }
            Command::ResetAudioClip => {
                if let Some(a) = &self.audio {
                    a.reset_clip();
                }
                Ok(())
            }
        };
        if let Err(e) = &r {
            self.event(EventKind::Error, e.clone(), None);
        }
        r
    }

    /// Interrupted recordings found at start-up.
    pub fn pending_recoveries(&self) -> &[(PathBuf, JournalEntry)] {
        &self.recoveries
    }

    /// Recovers interrupted session `i` in the background (originals untouched).
    pub fn recover(&mut self, i: usize) {
        if i >= self.recoveries.len() {
            return;
        }
        let (journal_file, entry) = self.recoveries.remove(i);
        let (tx, rx) = bounded(16);
        let en = self.en();
        let _ = std::thread::Builder::new().name("fluxcapture-recover".into()).spawn(move || {
            for f in entry.files.iter().filter(|f| f.exists()) {
                let dst = journal::recovered_path(f);
                let ev = match fluxcapture_mux::remux::recover_mkv(f, &dst) {
                    Ok(r) => Event {
                        kind: EventKind::Success,
                        text: format!("{} ({:.0} s)", if en { "Recording recovered" } else { "Gravação recuperada" }, r.duration.as_secs_f64()),
                        path: Some(dst),
                    },
                    Err(e) => Event {
                        kind: EventKind::Error,
                        text: format!("{} {}: {e}", if en { "Could not recover" } else { "Não foi possível recuperar" }, f.display()),
                        path: Some(f.clone()),
                    },
                };
                let _ = tx.send(ev);
            }
            journal::dismiss(&journal_file);
        });
        self.recovery_jobs.push(rx);
    }

    pub fn dismiss_recovery(&mut self, i: usize) {
        if i < self.recoveries.len() {
            let (f, _) = self.recoveries.remove(i);
            journal::dismiss(&f);
        }
    }

    /// Starts the setup assistant (hardware test) in the background.
    pub fn start_wizard(&mut self) -> Result<(), String> {
        if self.is_encoding() {
            return Err(self.t("Pare a gravação antes de testar o hardware.", "Stop recording before testing the hardware."));
        }
        let folder = self.settings.recording_folder();
        self.wizard = Some(wizard::WizardJob::start(self.gpu_choice(), self.native_size(), folder, self.en()));
        Ok(())
    }

    pub fn wizard(&self) -> Option<&wizard::WizardJob> {
        self.wizard.as_ref()
    }

    pub fn close_wizard(&mut self) {
        self.wizard = None;
    }

    /// Applies the assistant's recommendation to the active profile.
    pub fn apply_wizard(&mut self) {
        let Some(rec) = self.wizard.as_ref().and_then(|w| w.report()).and_then(|r| r.recommendation.clone()) else { return };
        self.update_settings(|s| {
            let p = s.profile_mut();
            p.video.codec = rec.codec;
            p.video.encoder = rec.encoder_id.clone();
            p.video.resolution = rec.resolution;
            p.video.fps = rec.fps;
            p.video.bitrate_kbps = rec.bitrate_kbps;
            s.general.first_run = false;
        });
        self.event(EventKind::Success, self.t("Configuração recomendada aplicada", "Recommended settings applied"), None);
    }

    /// Call regularly (every UI frame, or ≥ 10 Hz).
    pub fn tick(&mut self) {
        while let Ok(i) = self.hotkey_rx.try_recv() {
            let cmd = match self.hotkey_actions.get(i) {
                Some(HotkeyAction::Record) => Command::ToggleRecording,
                Some(HotkeyAction::Pause) => Command::TogglePause,
                Some(HotkeyAction::SaveReplay) => Command::SaveReplay,
                Some(HotkeyAction::Screenshot) => Command::Screenshot,
                Some(HotkeyAction::ToggleMic) => Command::ToggleMicMute,
                Some(HotkeyAction::ToggleReplay) => Command::ToggleReplay,
                None => continue,
            };
            let _ = self.dispatch(cmd);
        }
        if let Some(c) = &self.control {
            for req in c.drain() {
                let reply = control::handle(self, &req.request);
                req.respond(reply);
            }
        }
        self.poll_finishing();
        self.poll_jobs();
        if let Some(w) = self.wizard.as_mut() {
            w.poll();
        }
        // The writer can stop by itself (disk full, write error).
        if let Some(rec) = &self.recording
            && let Some(summary) = rec.summary()
        {
            let reason = summary.reason.clone();
            self.recording = None;
            if !self.replay_on {
                drop(self.stop_encoding());
            }
            self.report_summary(&summary);
            if reason == StopReason::LowDisk {
                self.event(
                    EventKind::Warning,
                    self.t("Gravação finalizada por falta de espaço em disco.", "Recording stopped: the disk is almost full."),
                    None,
                );
            }
        }
        if let Some(v) = &self.video
            && let Some(err) = v.fatal_error()
        {
            self.video = None;
            let msg = format!("{}: {err}", self.t("O motor de vídeo parou; reiniciando", "The video engine stopped; restarting"));
            self.event(EventKind::Error, msg, None);
            if self.recording.is_some() {
                self.recording = None;
            }
            self.encode = None;
            self.start_video();
        }
        if self.last_metrics.elapsed() >= Duration::from_millis(1000) {
            self.update_metrics();
        }
        if self.last_diag.elapsed() >= Duration::from_secs(2) {
            self.update_diagnostics();
        }
        self.update_live_state();
        if let Some(t) = self.dirty_since
            && t.elapsed() > Duration::from_secs(1)
        {
            self.save_settings();
        }
    }

    fn report_summary(&mut self, s: &fluxcapture_pipeline::RecordingSummary) {
        for e in &s.errors {
            self.event(EventKind::Error, e.clone(), None);
        }
        let file = s.files.iter().find(|f| f.extension().is_some_and(|e| e == "mp4")).or(s.files.first()).cloned();
        if let Some(f) = file {
            let text = format!("{} — {} ({:.1} MB)", self.t("Gravação salva", "Recording saved"), s.duration.hms(), s.bytes as f64 / 1e6);
            self.event(EventKind::Success, text, Some(f));
        }
    }

    fn poll_finishing(&mut self) {
        let mut done = Vec::new();
        for (i, (rec, _)) in self.finishing.iter().enumerate() {
            if let Some(s) = rec.summary() {
                done.push((i, s));
            }
        }
        for (i, s) in done.into_iter().rev() {
            self.finishing.remove(i);
            self.report_summary(&s);
        }
    }

    fn poll_jobs(&mut self) {
        let mut evs = Vec::new();
        self.replay_saves.retain(|rx| match rx.try_recv() {
            Ok(r) => {
                evs.push(r);
                false
            }
            Err(crossbeam_channel::TryRecvError::Empty) => true,
            Err(_) => false,
        });
        for r in evs {
            match r {
                Ok(p) => {
                    let t = self.t("Replay salvo", "Replay saved");
                    self.event(EventKind::Success, t, Some(p));
                }
                Err(e) => self.event(EventKind::Error, format!("Replay: {e}"), None),
            }
        }
        let mut jobs = Vec::new();
        self.recovery_jobs.retain(|rx| {
            loop {
                match rx.try_recv() {
                    Ok(e) => jobs.push(e),
                    Err(crossbeam_channel::TryRecvError::Empty) => break true,
                    Err(_) => break false,
                }
            }
        });
        for e in jobs {
            self.event(e.kind, e.text, e.path);
        }
    }

    fn update_live_state(&mut self) {
        let st = &mut self.state;
        if let Some(v) = &self.video {
            st.sources = v.source_states();
        }
        if let Some(a) = &self.audio {
            st.audio = a.snapshot();
        }
        st.encoding = self.encode.is_some();
        st.replay_enabled = self.replay_on;
        if let Some(s) = &self.encode {
            st.router = s.router.stats();
            st.replay_seconds = st.router.replay_seconds;
            st.replay_bytes = st.router.replay_bytes;
        } else {
            st.router = RouterStats::default();
            st.replay_seconds = 0.0;
            st.replay_bytes = 0;
        }
        st.rec.finishing = self.finishing.len();
        match &self.recording {
            Some(r) => {
                let s = r.status();
                st.rec.active = true;
                st.rec.paused = self.paused;
                st.rec.duration = s.duration;
                st.rec.bytes = s.bytes;
                st.rec.file = s.current_file;
                st.rec.parts = s.parts;
                st.rec.warning = s.warning;
                st.rec.state = Some(s.state);
                if s.disk_free.is_some() {
                    st.disk_free = s.disk_free;
                }
            }
            None => {
                st.rec = RecView { finishing: self.finishing.len(), ..RecView::default() };
            }
        }
        st.mic_muted = self.settings.audio_sources.iter().filter(|a| is_mic(&a.endpoint)).all(|a| a.settings.muted)
            && self.settings.audio_sources.iter().any(|a| is_mic(&a.endpoint));
    }

    fn update_metrics(&mut self) {
        self.last_metrics = Instant::now();
        let wall = self.started.elapsed().as_nanos() as i64 / 100;
        let cores = fluxcapture_platform::system::cpu_count();
        if let Ok(u) = fluxcapture_platform::system::process_usage() {
            if let Some(c) = self.cpu.sample(u.cpu_time.0, wall, cores) {
                self.state.cpu_process = c;
                self.state.history.cpu.push(c);
            }
            self.state.mem_process_mb = u.private_bytes / 1_000_000;
        }
        if let Ok(t) = fluxcapture_platform::system::system_times() {
            // Kernel time includes idle time.
            let busy = (t.kernel.0 - t.idle.0).max(0) + t.user.0;
            let total = t.kernel.0 + t.user.0;
            if let Some(c) = self.sys_cpu.sample(busy, total, 1) {
                self.state.cpu_system = c;
            }
        }
        self.state.mem_available_mb = fluxcapture_platform::system::memory_status().1 / 1_000_000;
        if let Some(v) = &self.video {
            let vs = v.stats();
            self.state.history.render_ms.push(vs.render_ms_avg);
            self.state.history.gpu_ms.push(vs.gpu_ms_avg.unwrap_or(0.0));
            self.state.history.bitrate_kbps.push(vs.bitrate_kbps);
            let prev = self.state.video.dropped_encoder_backlog + self.state.video.dropped_render_lag;
            let now = vs.dropped_encoder_backlog + vs.dropped_render_lag;
            self.state.history.dropped.push(now.saturating_sub(prev) as f32);
            self.state.output = (vs.canvas_width, vs.canvas_height);
            self.state.video = vs;
        }
        self.state.native = self.native_size();
        if self.recording.is_none() || self.state.disk_free.is_none() {
            self.state.disk_free = fluxcapture_platform::system::disk_free(&self.settings.recording_folder()).ok();
        }
        let kbps = self.encode.as_ref().map(|e| e.est_kbps).unwrap_or_else(|| {
            let p = self.settings.profile();
            let (w, h) = self.output_size();
            p.video.rate_control(w, h).estimated_kbps(w, h, p.video.fps.as_f64()) + p.audio.bitrate_kbps * 3
        });
        self.state.minutes_left = self.state.disk_free.map(|b| b as f32 / (kbps.max(1) as f32 * 125.0) / 60.0);
        // Keep ~10 s of windows for diagnostics.
        let a = &self.state.audio;
        let (u, o) = a.stats.iter().fold((0u64, 0u64), |acc, s| (acc.0 + s.underruns, acc.1 + s.overruns));
        self.windows.push_back(Window {
            at: Instant::now(),
            video: self.state.video.clone(),
            audio_underruns: u,
            audio_overruns: o,
            audio_dropped: a.dropped_chunks,
        });
        while self.windows.len() > 11 {
            self.windows.pop_front();
        }
    }

    fn update_diagnostics(&mut self) {
        self.last_diag = Instant::now();
        let (Some(first), Some(last)) = (self.windows.front(), self.windows.back()) else { return };
        let secs = last.at.duration_since(first.at).as_secs_f32();
        if secs < 1.0 {
            return;
        }
        let v = &last.video;
        let f = &first.video;
        let st = &self.state;
        let rec_status = self.recording.as_ref().map(|r| r.status());
        let snap = Snapshot {
            recording: self.recording.is_some() && !self.paused,
            window_secs: secs,
            fps_target: v.fps_target as f32,
            frames_submitted: v.frames_submitted.saturating_sub(f.frames_submitted),
            dropped_render: v.dropped_render_lag.saturating_sub(f.dropped_render_lag),
            dropped_encoder: v.dropped_encoder_backlog.saturating_sub(f.dropped_encoder_backlog),
            render_ms_avg: v.render_ms_avg,
            render_ms_max: v.render_ms_max,
            gpu_ms: v.gpu_ms_avg,
            encode_latency_ms: v.encode_latency_ms_avg,
            encoder_hardware: v.encoder_hardware,
            encoder_name: v.encoder_name.clone().unwrap_or_default(),
            capture_fps_min: v.capture_fps.iter().map(|(_, f)| *f).reduce(f32::min),
            sources_lost: st
                .sources
                .iter()
                .filter_map(|(id, s)| match s {
                    SourceState::Lost(m) => Some(format!("{}: {m}", self.source_name(*id))),
                    _ => None,
                })
                .collect(),
            disk_write_mbps: rec_status.as_ref().map_or(0.0, |r| (r.write_rate * 8.0 / 1e6) as f32),
            disk_needed_mbps: self.encode.as_ref().map_or(0.0, |e| e.est_kbps as f32 / 1000.0),
            disk_write_ms_max: rec_status.as_ref().map_or(0.0, |r| r.write_ms_max),
            writer_backlog: rec_status.as_ref().map_or(0, |r| r.backlog),
            disk_free_bytes: st.disk_free,
            minutes_left: if self.recording.is_some() { st.minutes_left } else { st.minutes_left.filter(|m| *m < 30.0) },
            audio_underruns: last.audio_underruns.saturating_sub(first.audio_underruns),
            audio_overruns: last.audio_overruns.saturating_sub(first.audio_overruns),
            audio_dropped_chunks: last.audio_dropped.saturating_sub(first.audio_dropped),
            audio_devices_lost: st
                .audio
                .captures
                .iter()
                .filter_map(|(_, name, s)| match s {
                    fluxcapture_platform::AudioCaptureState::Reconnecting(m) | fluxcapture_platform::AudioCaptureState::Failed(m) => {
                        Some(format!("{name}: {m}"))
                    }
                    _ => None,
                })
                .collect(),
            audio_clipping: st.audio.meters.iter().filter(|m| m.level.clipped).map(|m| m.name.clone()).collect(),
            process_cpu: st.cpu_process,
            system_cpu: st.cpu_system,
            memory_available_mb: st.mem_available_mb,
            preview_enabled: v.preview_enabled,
            preview_fps: v.preview_fps,
            replay_mb: st.replay_bytes as f32 / 1_048_576.0,
            replay_limit_mb: if self.replay_on { self.settings.replay.max_memory_mb as f32 } else { 0.0 },
        };
        let findings = diagnose(&snap, self.en());
        // Overload policy: first cut non-essential work (the preview), and say so.
        if snap.recording && (snap.dropped_render > 0 || snap.dropped_encoder > 0) && v.preview_enabled && !self.preview_auto_reduced {
            self.preview_auto_reduced = true;
            if let Some(vid) = &self.video {
                let mut p = self.settings.profile().preview.to_platform(true);
                p.fps = p.fps.min(5);
                p.max_size = p.max_size.min(480);
                vid.set_preview(p);
            }
            self.event(
                EventKind::Warning,
                self.t(
                    "Sobrecarga detectada: a prévia foi reduzida para 5 FPS (a gravação não foi alterada).",
                    "Overload detected: the preview was reduced to 5 fps (the recording is unchanged).",
                ),
                None,
            );
        }
        self.state.proposals = advisor::proposals(&findings, &self.settings, self.native_size(), self.en());
        self.state.findings = findings;
    }

    fn source_name(&self, id: u64) -> String {
        self.settings.scene().and_then(|s| s.sources.iter().find(|x| x.id == id)).map(|s| s.name.clone()).unwrap_or_else(|| format!("#{id}"))
    }

    /// Recording / encoding health for the CLI and control channel.
    pub fn status_json(&self) -> serde_json::Value {
        let s = &self.state;
        serde_json::json!({
            "recording": s.rec.active,
            "paused": s.rec.paused,
            "duration_s": s.rec.duration.as_secs_f64(),
            "bytes": s.rec.bytes,
            "file": s.rec.file,
            "replay": { "enabled": s.replay_enabled, "seconds": s.replay_seconds, "bytes": s.replay_bytes },
            "encoder": s.encoder.as_ref().map(|e| e.name.clone()),
            "gpu": s.gpu_name,
            "output": [s.output.0, s.output.1],
            "video": {
                "rendered": s.video.frames_rendered,
                "submitted": s.video.frames_submitted,
                "encoded": s.video.frames_encoded,
                "dropped_render": s.video.dropped_render_lag,
                "dropped_encoder": s.video.dropped_encoder_backlog,
                "render_ms": s.video.render_ms_avg,
                "gpu_ms": s.video.gpu_ms_avg,
                "encode_latency_ms": s.video.encode_latency_ms_avg,
                "bitrate_kbps": s.video.bitrate_kbps,
            },
            "cpu": { "process": s.cpu_process, "system": s.cpu_system },
            "memory_mb": s.mem_process_mb,
            "findings": s.findings.iter().map(|f| serde_json::json!({"area": format!("{:?}", f.area), "severity": format!("{:?}", f.severity), "title": f.title, "detail": f.detail})).collect::<Vec<_>>(),
        })
    }

    /// Stops everything cleanly (finalizes any recording). Blocks until files are complete (bounded).
    pub fn shutdown(&mut self) {
        if self.recording.is_some() {
            self.stop_recording();
        }
        let _ = self.set_replay(false);
        drop(self.stop_encoding());
        let deadline = Instant::now() + Duration::from_secs(60);
        for (rec, _) in self.finishing.drain(..) {
            let left = deadline.saturating_duration_since(Instant::now());
            if let Some(s) = rec.wait(left) {
                log::info!("finished {:?}", s.files);
            }
        }
        if self.dirty_since.is_some() {
            self.save_settings();
        }
        self.hotkeys = None;
        self.video = None;
        self.audio = None;
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn is_mic(e: &AudioEndpoint) -> bool {
    matches!(e, AudioEndpoint::DefaultInput | AudioEndpoint::Device { loopback: false, .. })
}
