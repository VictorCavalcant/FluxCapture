//! FluxCapture configuration.
//!
//! One TOML file (`config.toml`) holds everything: general preferences, recording options, the
//! profiles (Gaming / Quality / Low power / Custom), the scene collection, audio sources and
//! hotkeys. Every field has a default (`#[serde(default)]`), so files from older versions load,
//! and [`Settings::sanitized`] clamps every number into its valid range: the file is user-editable
//! and therefore hostile (AGENTS.md §0.4). A file that does not parse is kept aside
//! (`config.toml.broken-<time>`) and defaults are used — the user's file is never overwritten
//! silently.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::path::{Path, PathBuf};

use fluxcapture_audio::{CompressorSettings, FilterSettings, HighPassSettings, LimiterSettings, MixerSettings, NoiseSuppressionSettings, SourceSettings};
use fluxcapture_platform::{AudioEndpoint, CaptureMethod, CaptureOptions, Crop, Fit, Hotkey, PreviewConfig, RectF, ScaleFilter, VideoCodecKind};
use fluxcapture_time::FrameRate;
use serde::{Deserialize, Serialize};

pub mod template;

/// Current file format version.
pub const VERSION: u32 = 1;

/// UI language.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    #[default]
    PtBr,
    En,
}

/// UI theme.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Theme {
    #[default]
    Dark,
    Light,
    System,
}

/// Process priority class while FluxCapture runs.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Priority {
    Normal,
    #[default]
    AboveNormal,
    High,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    pub language: Language,
    pub theme: Theme,
    pub start_minimized: bool,
    /// Closing / minimizing the window keeps FluxCapture in the notification area.
    pub minimize_to_tray: bool,
    /// Keep FluxCapture's own windows out of every capture (Windows 10 2004+).
    pub exclude_from_capture: bool,
    pub confirm_stop: bool,
    pub process_priority: Priority,
    /// Show the first-run setup assistant.
    pub first_run: bool,
    /// Reduce UI animations and metric refreshes (discreet / economy mode).
    pub economy_ui: bool,
}

impl Default for General {
    fn default() -> Self {
        General {
            language: Language::PtBr,
            theme: Theme::Dark,
            start_minimized: false,
            minimize_to_tray: false,
            exclude_from_capture: true,
            confirm_stop: false,
            process_priority: Priority::AboveNormal,
            first_run: true,
            economy_ui: false,
        }
    }
}

/// Output resolution.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputResolution {
    /// The size of the first capture source's monitor.
    #[default]
    Native,
    P720,
    P1080,
    P1440,
    P2160,
    Custom {
        width: u32,
        height: u32,
    },
}

impl OutputResolution {
    /// Resolves to an even size, given the native size.
    pub fn resolve(self, native: (u32, u32)) -> (u32, u32) {
        let (w, h) = match self {
            OutputResolution::Native => native,
            OutputResolution::P720 => (1280, 720),
            OutputResolution::P1080 => (1920, 1080),
            OutputResolution::P1440 => (2560, 1440),
            OutputResolution::P2160 => (3840, 2160),
            OutputResolution::Custom { width, height } => (width, height),
        };
        (w.clamp(16, 8192) & !1, h.clamp(16, 8192) & !1)
    }

    pub fn label(self) -> String {
        match self {
            OutputResolution::Native => "Nativa".into(),
            OutputResolution::P720 => "1280×720".into(),
            OutputResolution::P1080 => "1920×1080".into(),
            OutputResolution::P1440 => "2560×1440".into(),
            OutputResolution::P2160 => "3840×2160".into(),
            OutputResolution::Custom { width, height } => format!("{width}×{height}"),
        }
    }
}

/// Rate control as the user sets it.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RateMode {
    /// Constant quality (best for local recordings).
    Quality,
    Vbr,
    Cbr,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoSettings {
    pub resolution: OutputResolution,
    pub fps: FrameRate,
    pub variable_frame_rate: bool,
    pub codec: VideoCodecKind,
    /// A specific encoder id (`mft:{…}`), or automatic.
    pub encoder: Option<String>,
    pub rate_mode: RateMode,
    /// 1..=100 for quality mode.
    pub quality: u8,
    /// kbit/s for VBR / CBR (0 = recommended for the resolution and frame rate).
    pub bitrate_kbps: u32,
    /// VBR peak, kbit/s (0 = 1.5 × bitrate).
    pub max_bitrate_kbps: u32,
    pub keyframe_interval_s: f32,
    /// 0 = fastest … 100 = best compression.
    pub speed_quality: u8,
    pub low_latency: bool,
    pub scale_filter: ScaleFilter,
    pub full_range: bool,
}

impl Default for VideoSettings {
    fn default() -> Self {
        VideoSettings {
            resolution: OutputResolution::Native,
            fps: FrameRate::FPS_60,
            variable_frame_rate: false,
            codec: VideoCodecKind::H264,
            encoder: None,
            rate_mode: RateMode::Vbr,
            quality: 70,
            bitrate_kbps: 0,
            max_bitrate_kbps: 0,
            keyframe_interval_s: 2.0,
            speed_quality: 50,
            low_latency: true,
            scale_filter: ScaleFilter::Bicubic,
            full_range: false,
        }
    }
}

/// Recommended bitrate (kbit/s) for local recordings of fast content.
pub fn recommended_kbps(width: u32, height: u32, fps: f64, codec: VideoCodecKind) -> u32 {
    let bits_per_pixel = match codec {
        VideoCodecKind::H264 => 0.12,
        VideoCodecKind::Hevc => 0.08,
        VideoCodecKind::Av1 => 0.07,
    };
    let fps = if fps.is_finite() { fps.clamp(1.0, 240.0) } else { 60.0 };
    // Sub-linear in frame rate: consecutive frames are more alike at high rates.
    let kbps = f64::from(width) * f64::from(height) * 60.0 * (fps / 60.0).powf(0.75) * bits_per_pixel / 1000.0;
    (kbps.round() as u32).clamp(1_000, 200_000)
}

impl VideoSettings {
    /// The platform rate control for an output size.
    pub fn rate_control(&self, width: u32, height: u32) -> fluxcapture_platform::RateControl {
        let kbps = if self.bitrate_kbps == 0 { recommended_kbps(width, height, self.fps.as_f64(), self.codec) } else { self.bitrate_kbps };
        match self.rate_mode {
            RateMode::Quality => fluxcapture_platform::RateControl::Quality { quality: self.quality.clamp(1, 100) },
            RateMode::Cbr => fluxcapture_platform::RateControl::Cbr { kbps },
            RateMode::Vbr => {
                let max = if self.max_bitrate_kbps == 0 { kbps.saturating_mul(3) / 2 } else { self.max_bitrate_kbps.max(kbps) };
                fluxcapture_platform::RateControl::Vbr { kbps, max_kbps: max }
            }
        }
    }
}

/// Audio encoding of the recording.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioCodec {
    #[default]
    Aac,
    /// Uncompressed float PCM (MKV only): no encoding cost, ~1.5 Mbit/s per stereo track.
    Pcm,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioEncoding {
    pub codec: AudioCodec,
    pub bitrate_kbps: u32,
}

impl Default for AudioEncoding {
    fn default() -> Self {
        AudioEncoding { codec: AudioCodec::Aac, bitrate_kbps: 192 }
    }
}

/// Parameters the automatic performance advisor must never propose to change.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Locks {
    pub resolution: bool,
    pub fps: bool,
    pub quality: bool,
    pub encoder: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProfileKind {
    Gaming,
    Quality,
    LowPower,
    Custom,
}

/// A named set of video / audio encoding and preview choices.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub name: String,
    pub kind: ProfileKind,
    pub video: VideoSettings,
    pub audio: AudioEncoding,
    pub preview: PreviewSettings,
    pub locks: Locks,
}

impl Default for Profile {
    fn default() -> Self {
        Profile::preset(ProfileKind::Gaming)
    }
}

impl Profile {
    /// The built-in presets.
    pub fn preset(kind: ProfileKind) -> Profile {
        let mut p = Profile {
            name: String::new(),
            kind,
            video: VideoSettings::default(),
            audio: AudioEncoding::default(),
            preview: PreviewSettings::default(),
            locks: Locks::default(),
        };
        match kind {
            ProfileKind::Gaming => {
                p.name = "Gaming".into();
                p.video.rate_mode = RateMode::Vbr;
                p.video.speed_quality = 30;
                p.video.low_latency = true;
                p.preview = PreviewSettings { enabled: true, max_size: 640, fps: 15, pause_while_recording: true, high_quality: false };
            }
            ProfileKind::Quality => {
                p.name = "Qualidade".into();
                p.video.rate_mode = RateMode::Quality;
                p.video.quality = 80;
                p.video.speed_quality = 90;
                p.video.low_latency = false;
                p.video.scale_filter = ScaleFilter::Lanczos;
                p.audio.bitrate_kbps = 192;
                p.preview = PreviewSettings { enabled: true, max_size: 960, fps: 30, pause_while_recording: false, high_quality: false };
            }
            ProfileKind::LowPower => {
                p.name = "Baixo consumo".into();
                p.video.rate_mode = RateMode::Vbr;
                p.video.speed_quality = 0;
                p.video.low_latency = true;
                p.video.variable_frame_rate = true;
                p.video.scale_filter = ScaleFilter::Bilinear;
                p.audio.bitrate_kbps = 128;
                p.preview = PreviewSettings { enabled: true, max_size: 480, fps: 10, pause_while_recording: true, high_quality: false };
            }
            ProfileKind::Custom => {
                p.name = "Personalizado".into();
            }
        }
        p
    }

    pub fn sanitized(&self) -> Profile {
        let mut p = self.clone();
        p.name = clean_name(&p.name, "Perfil");
        let v = &mut p.video;
        if let OutputResolution::Custom { width, height } = v.resolution {
            v.resolution = OutputResolution::Custom { width: width.clamp(16, 8192) & !1, height: height.clamp(16, 8192) & !1 };
        }
        v.quality = v.quality.clamp(1, 100);
        v.bitrate_kbps = v.bitrate_kbps.min(500_000);
        v.max_bitrate_kbps = v.max_bitrate_kbps.min(800_000);
        v.keyframe_interval_s = if v.keyframe_interval_s.is_finite() { v.keyframe_interval_s.clamp(0.5, 10.0) } else { 2.0 };
        v.speed_quality = v.speed_quality.min(100);
        p.audio.bitrate_kbps = match p.audio.bitrate_kbps {
            0..=111 => 96,
            112..=143 => 128,
            144..=175 => 160,
            _ => 192,
        };
        p.preview = p.preview.sanitized();
        p
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PreviewSettings {
    pub enabled: bool,
    pub max_size: u32,
    pub fps: u32,
    /// Stop refreshing the preview while recording (saves GPU and CPU).
    pub pause_while_recording: bool,
    /// Optional high-quality preview: near-native resolution (up to [`PREVIEW_HQ_MAX_SIZE`]) and the
    /// sharpest scaling filter. It costs GPU and memory bandwidth, so it is removed while recording,
    /// exactly like `pause_while_recording`.
    pub high_quality: bool,
}

/// Longest preview edge, in pixels, when the high-quality preview is on.
pub const PREVIEW_HQ_MAX_SIZE: u32 = 2560;

impl Default for PreviewSettings {
    fn default() -> Self {
        PreviewSettings { enabled: true, max_size: 960, fps: 30, pause_while_recording: false, high_quality: false }
    }
}

impl PreviewSettings {
    pub fn sanitized(self) -> PreviewSettings {
        PreviewSettings { max_size: self.max_size.clamp(160, 3840), fps: self.fps.clamp(1, 60), ..self }
    }

    /// Whether the preview is taken away while a recording runs.
    pub fn removed_while_recording(self) -> bool {
        self.pause_while_recording || self.high_quality
    }

    pub fn to_platform(self, recording: bool) -> PreviewConfig {
        PreviewConfig {
            enabled: self.enabled && !(recording && self.removed_while_recording()),
            max_size: if self.high_quality { self.max_size.max(PREVIEW_HQ_MAX_SIZE) } else { self.max_size },
            fps: self.fps,
            high_quality: self.high_quality,
        }
    }
}

/// How recordings are split into files.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Split {
    #[default]
    Never,
    /// Every N minutes.
    Minutes(u32),
    /// Every N megabytes.
    Megabytes(u32),
}

/// Which audio tracks the file carries.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrackLayout {
    /// One mixed track.
    MixOnly,
    /// Track 1 = mix, then one track per source (system sound, microphone…): edit them apart later.
    #[default]
    MixAndSeparate,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecordingSettings {
    /// Empty = the user's Videos folder / FluxCapture.
    pub folder: PathBuf,
    /// See [`template::expand`].
    pub filename_template: String,
    /// Remux the MKV into an MP4 when the recording finishes (the MKV is kept until it succeeds).
    pub remux_to_mp4: bool,
    /// Delete the MKV after a successful remux.
    pub delete_mkv_after_remux: bool,
    pub split: Split,
    pub tracks: TrackLayout,
    /// Stop (and finalize the file) below this much free space.
    pub min_free_mb: u32,
    /// Warn below this much free space.
    pub warn_free_mb: u32,
}

impl Default for RecordingSettings {
    fn default() -> Self {
        RecordingSettings {
            folder: PathBuf::new(),
            filename_template: "FluxCapture {date} {time}".into(),
            remux_to_mp4: false,
            delete_mkv_after_remux: false,
            split: Split::Never,
            tracks: TrackLayout::MixAndSeparate,
            min_free_mb: 500,
            warn_free_mb: 5_000,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReplaySettings {
    /// Start the replay buffer with the app.
    pub enabled_on_start: bool,
    pub seconds: u32,
    /// Hard memory cap for the buffer.
    pub max_memory_mb: u32,
    /// Save replays as MP4 (otherwise MKV).
    pub save_mp4: bool,
}

impl Default for ReplaySettings {
    fn default() -> Self {
        ReplaySettings { enabled_on_start: false, seconds: 30, max_memory_mb: 512, save_mp4: true }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hotkeys {
    pub record: Option<Hotkey>,
    pub pause: Option<Hotkey>,
    pub save_replay: Option<Hotkey>,
    pub screenshot: Option<Hotkey>,
    pub toggle_mic: Option<Hotkey>,
    /// Turns the replay buffer on / off.
    pub toggle_replay: Option<Hotkey>,
}

const fn key(ctrl: bool, alt: bool, shift: bool, vk: u32) -> Option<Hotkey> {
    Some(Hotkey { ctrl, alt, shift, win: false, vk })
}

impl Default for Hotkeys {
    fn default() -> Self {
        Hotkeys {
            record: key(true, false, true, 0x78),        // Ctrl+Shift+F9
            pause: key(true, false, true, 0x79),         // Ctrl+Shift+F10
            save_replay: key(true, false, true, 0x7A),   // Ctrl+Shift+F11
            screenshot: key(true, false, true, 0x7B),    // Ctrl+Shift+F12
            toggle_mic: key(true, false, true, 0x4D),    // Ctrl+Shift+M
            toggle_replay: key(true, false, true, 0x52), // Ctrl+Shift+R
        }
    }
}

impl Hotkeys {
    /// (action name, key) pairs in a fixed order.
    pub fn list(&self) -> [(HotkeyAction, Option<Hotkey>); 6] {
        [
            (HotkeyAction::Record, self.record),
            (HotkeyAction::Pause, self.pause),
            (HotkeyAction::SaveReplay, self.save_replay),
            (HotkeyAction::Screenshot, self.screenshot),
            (HotkeyAction::ToggleMic, self.toggle_mic),
            (HotkeyAction::ToggleReplay, self.toggle_replay),
        ]
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HotkeyAction {
    Record,
    Pause,
    SaveReplay,
    Screenshot,
    ToggleMic,
    ToggleReplay,
}

/// What a scene source shows.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SourceKind {
    /// A monitor (empty id = primary). Region capture is a display with a crop.
    Display {
        monitor: String,
        options: CaptureOptions,
    },
    Window {
        handle: u64,
        title: String,
        process: String,
        options: CaptureOptions,
    },
    Image {
        path: PathBuf,
    },
    Text(TextStyle),
    Color {
        rgba: [f32; 4],
    },
    Webcam {
        device: String,
        width: u32,
        height: u32,
        fps: u32,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TextStyle {
    pub text: String,
    /// Font family installed on the system ("Segoe UI").
    pub font: String,
    pub size_px: f32,
    pub color: [u8; 4],
    pub background: Option<[u8; 4]>,
    pub outline: Option<[u8; 4]>,
    pub bold: bool,
}

impl Default for TextStyle {
    fn default() -> Self {
        TextStyle {
            text: "FluxCapture".into(),
            font: "Segoe UI".into(),
            size_px: 48.0,
            color: [255, 255, 255, 255],
            background: None,
            outline: Some([0, 0, 0, 200]),
            bold: true,
        }
    }
}

/// A source placed in a scene. Coordinates are fractions of the canvas, so a scene survives
/// changes of output resolution.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SceneSource {
    pub id: u64,
    pub name: String,
    pub kind: SourceKind,
    /// x, y, w, h in 0..=1 of the canvas.
    pub rect: RectF,
    pub fit: Fit,
    pub crop: Crop,
    pub opacity: f32,
    pub visible: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Scene {
    pub name: String,
    pub sources: Vec<SceneSource>,
}

impl Scene {
    /// The default scene: the primary monitor, full canvas.
    pub fn default_desktop() -> Scene {
        Scene {
            name: "Cena 1".into(),
            sources: vec![SceneSource {
                id: 1,
                name: "Tela principal".into(),
                kind: SourceKind::Display { monitor: String::new(), options: CaptureOptions::default() },
                rect: RectF { x: 0.0, y: 0.0, w: 1.0, h: 1.0 },
                fit: Fit::Contain,
                crop: Crop::default(),
                opacity: 1.0,
                visible: true,
            }],
        }
    }

    /// The next free source id.
    pub fn next_id(&self) -> u64 {
        self.sources.iter().map(|s| s.id).max().unwrap_or(0).saturating_add(1)
    }
}

/// An audio input of the mixer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioSourceConfig {
    pub id: u32,
    pub name: String,
    pub endpoint: AudioEndpoint,
    pub enabled: bool,
    pub settings: SourceSettings,
}

pub fn default_audio_sources() -> Vec<AudioSourceConfig> {
    let desktop = SourceSettings { tracks: 0b011, continuous: false, ..SourceSettings::default() };
    let mic = SourceSettings {
        tracks: 0b101,
        continuous: true,
        filters: FilterSettings {
            high_pass: Some(HighPassSettings { cutoff_hz: 80.0 }),
            noise_suppression: Some(NoiseSuppressionSettings { strength: 0.6 }),
            gate: None,
            compressor: Some(CompressorSettings { threshold_db: -18.0, ratio: 3.0, attack_ms: 6.0, release_ms: 120.0, makeup_db: 4.0 }),
            ..FilterSettings::default()
        },
        ..SourceSettings::default()
    };
    vec![
        AudioSourceConfig { id: 1, name: "Áudio do sistema".into(), endpoint: AudioEndpoint::DefaultOutput, enabled: true, settings: desktop },
        AudioSourceConfig { id: 2, name: "Microfone".into(), endpoint: AudioEndpoint::DefaultInput, enabled: true, settings: mic },
    ]
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Advanced {
    /// GPU (adapter LUID) for capture, composition and encoding; `None` = automatic. LUIDs change
    /// across reboots, so the adapter name is kept too and used to find it again.
    pub gpu_luid: Option<u64>,
    pub gpu_name: String,
    pub capture_method: CaptureMethod,
    /// Debug logging.
    pub verbose_log: bool,
    /// Local JSON-lines control server port (0 = off).
    pub control_port: u16,
    /// Delay of the audio mix behind real time, ms (latency vs. robustness to device jitter).
    pub audio_mix_delay_ms: u32,
}

impl Default for Advanced {
    fn default() -> Self {
        Advanced { gpu_luid: None, gpu_name: String::new(), capture_method: CaptureMethod::Auto, verbose_log: false, control_port: 0, audio_mix_delay_ms: 80 }
    }
}

/// Everything.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub version: u32,
    pub general: General,
    pub recording: RecordingSettings,
    pub replay: ReplaySettings,
    pub hotkeys: Hotkeys,
    pub advanced: Advanced,
    pub active_profile: usize,
    pub profiles: Vec<Profile>,
    pub active_scene: usize,
    pub scenes: Vec<Scene>,
    pub audio_sources: Vec<AudioSourceConfig>,
    pub mixer: MixerSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            version: VERSION,
            general: General::default(),
            recording: RecordingSettings::default(),
            replay: ReplaySettings::default(),
            hotkeys: Hotkeys::default(),
            advanced: Advanced::default(),
            active_profile: 0,
            profiles: vec![
                Profile::preset(ProfileKind::Gaming),
                Profile::preset(ProfileKind::Quality),
                Profile::preset(ProfileKind::LowPower),
                Profile::preset(ProfileKind::Custom),
            ],
            active_scene: 0,
            scenes: vec![Scene::default_desktop()],
            audio_sources: default_audio_sources(),
            mixer: MixerSettings { tracks: 3, master_volume_db: 0.0, limiter: Some(LimiterSettings { ceiling_db: -1.0, release_ms: 80.0 }) },
        }
    }
}

fn clean_name(s: &str, fallback: &str) -> String {
    let t: String = s.chars().filter(|c| !c.is_control()).take(64).collect();
    let t = t.trim();
    if t.is_empty() { fallback.to_string() } else { t.to_string() }
}

fn finite(v: f32, lo: f32, hi: f32, default: f32) -> f32 {
    if v.is_finite() { v.clamp(lo, hi) } else { default }
}

impl Settings {
    pub fn profile(&self) -> &Profile {
        // `sanitized` guarantees at least one profile; the fallback keeps this total anyway.
        self.profiles.get(self.active_profile).or_else(|| self.profiles.first()).unwrap_or(&FALLBACK_PROFILE)
    }

    pub fn profile_mut(&mut self) -> &mut Profile {
        if self.profiles.is_empty() {
            self.profiles.push(Profile::preset(ProfileKind::Gaming));
        }
        let i = self.active_profile.min(self.profiles.len() - 1);
        self.active_profile = i;
        &mut self.profiles[i]
    }

    pub fn scene(&self) -> Option<&Scene> {
        self.scenes.get(self.active_scene).or_else(|| self.scenes.first())
    }

    pub fn scene_mut(&mut self) -> &mut Scene {
        if self.scenes.is_empty() {
            self.scenes.push(Scene::default_desktop());
        }
        let i = self.active_scene.min(self.scenes.len() - 1);
        self.active_scene = i;
        &mut self.scenes[i]
    }

    /// Every value clamped into range, lists bounded, indices valid.
    pub fn sanitized(&self) -> Settings {
        let mut s = self.clone();
        s.version = VERSION;
        if s.profiles.is_empty() {
            s.profiles = Settings::default().profiles;
        }
        s.profiles.truncate(32);
        s.profiles = s.profiles.iter().map(Profile::sanitized).collect();
        s.active_profile = s.active_profile.min(s.profiles.len() - 1);
        if s.scenes.is_empty() {
            s.scenes.push(Scene::default_desktop());
        }
        s.scenes.truncate(64);
        for scene in &mut s.scenes {
            scene.name = clean_name(&scene.name, "Cena");
            scene.sources.truncate(64);
            for src in &mut scene.sources {
                src.name = clean_name(&src.name, "Fonte");
                src.opacity = finite(src.opacity, 0.0, 1.0, 1.0);
                let r = src.rect;
                src.rect =
                    RectF { x: finite(r.x, -4.0, 4.0, 0.0), y: finite(r.y, -4.0, 4.0, 0.0), w: finite(r.w, 0.001, 8.0, 1.0), h: finite(r.h, 0.001, 8.0, 1.0) };
                match &mut src.kind {
                    SourceKind::Text(t) => {
                        t.size_px = finite(t.size_px, 6.0, 512.0, 48.0);
                        t.text = t.text.chars().take(4096).collect();
                    }
                    SourceKind::Color { rgba } => *rgba = rgba.map(|c| finite(c, 0.0, 1.0, 1.0)),
                    SourceKind::Webcam { width, height, fps, .. } => {
                        *width = (*width).clamp(160, 3840);
                        *height = (*height).clamp(120, 2160);
                        *fps = (*fps).clamp(5, 120);
                    }
                    SourceKind::Display { options, .. } | SourceKind::Window { options, .. } => options.max_fps = options.max_fps.min(480),
                    SourceKind::Image { .. } => {}
                }
            }
            // Ids must be unique: renumber duplicates.
            let mut seen = std::collections::HashSet::new();
            let mut next = scene.next_id();
            for src in &mut scene.sources {
                if !seen.insert(src.id) {
                    src.id = next;
                    seen.insert(next);
                    next = next.saturating_add(1);
                }
            }
        }
        s.active_scene = s.active_scene.min(s.scenes.len() - 1);
        s.audio_sources.truncate(16);
        let mut ids = std::collections::HashSet::new();
        s.audio_sources.retain(|a| ids.insert(a.id));
        for a in &mut s.audio_sources {
            a.name = clean_name(&a.name, "Áudio");
            a.settings = a.settings.sanitized();
        }
        s.mixer = s.mixer.sanitized();
        s.replay.seconds = s.replay.seconds.clamp(5, 600);
        s.replay.max_memory_mb = s.replay.max_memory_mb.clamp(64, 8192);
        s.recording.filename_template = if s.recording.filename_template.trim().is_empty() {
            RecordingSettings::default().filename_template
        } else {
            s.recording.filename_template.chars().take(200).collect()
        };
        s.recording.split = match s.recording.split {
            Split::Minutes(m) => Split::Minutes(m.clamp(1, 24 * 60)),
            Split::Megabytes(mb) => Split::Megabytes(mb.clamp(100, 1_000_000)),
            Split::Never => Split::Never,
        };
        s.recording.min_free_mb = s.recording.min_free_mb.clamp(100, 100_000);
        s.recording.warn_free_mb = s.recording.warn_free_mb.clamp(s.recording.min_free_mb, 1_000_000);
        s.advanced.audio_mix_delay_ms = s.advanced.audio_mix_delay_ms.clamp(30, 500);
        s
    }

    /// Parses TOML (then sanitizes).
    pub fn from_toml(text: &str) -> Result<Settings, String> {
        toml::from_str::<Settings>(text).map(|s| s.sanitized()).map_err(|e| e.to_string())
    }

    pub fn to_toml(&self) -> Result<String, String> {
        toml::to_string_pretty(self).map_err(|e| e.to_string())
    }

    /// The recording folder (the configured one, or Videos\FluxCapture).
    pub fn recording_folder(&self) -> PathBuf {
        if self.recording.folder.as_os_str().is_empty() {
            fluxcapture_platform::system::videos_dir().unwrap_or_else(|| PathBuf::from(".")).join("FluxCapture")
        } else {
            self.recording.folder.clone()
        }
    }

    /// The preview configuration for the platform.
    pub fn preview_config(&self, recording: bool) -> PreviewConfig {
        self.profile().preview.to_platform(recording)
    }
}

static FALLBACK_PROFILE: std::sync::LazyLock<Profile> = std::sync::LazyLock::new(|| Profile::preset(ProfileKind::Gaming));

/// Where the configuration lives: `FLUXCAPTURE_CONFIG_DIR` if set; next to the executable in portable mode (a `portable` file in
/// the executable's folder), `%APPDATA%\FluxCapture` otherwise.
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("FLUXCAPTURE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
        && (dir.join("portable").exists() || dir.join("portable.txt").exists())
    {
        return dir.join("config");
    }
    fluxcapture_platform::system::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("FluxCapture")
}

/// Result of loading the configuration.
pub struct Loaded {
    pub settings: Settings,
    /// Something the user should know (the file was broken and set aside, etc.).
    pub warning: Option<String>,
}

/// Loads `dir/config.toml`; a missing file gives defaults, a broken one is set aside.
pub fn load(dir: &Path) -> Loaded {
    let path = dir.join("config.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Loaded { settings: Settings::default(), warning: None },
        Err(e) => return Loaded { settings: Settings::default(), warning: Some(format!("Não foi possível ler {}: {e}", path.display())) },
    };
    match Settings::from_toml(&text) {
        Ok(settings) => Loaded { settings, warning: None },
        Err(e) => {
            let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            let aside = dir.join(format!("config.toml.broken-{stamp}"));
            let moved = std::fs::rename(&path, &aside).is_ok();
            log::error!("config.toml is invalid: {e}");
            let where_ = if moved { format!("guardado como {}", aside.display()) } else { "mantido".into() };
            Loaded {
                settings: Settings::default(),
                warning: Some(format!("O arquivo de configuração era inválido ({e}); o original foi {where_} e os padrões foram carregados.")),
            }
        }
    }
}

/// Saves atomically (temporary file + rename), so a crash never leaves a half-written file.
pub fn save(dir: &Path, settings: &Settings) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let text = settings.to_toml()?;
    let tmp = dir.join("config.toml.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, dir.join("config.toml")).map_err(|e| format!("cannot replace config.toml: {e}"))
}

/// Exports one profile as TOML (for sharing).
pub fn export_profile(p: &Profile) -> Result<String, String> {
    toml::to_string_pretty(p).map_err(|e| e.to_string())
}

/// Imports a profile exported by [`export_profile`].
pub fn import_profile(text: &str) -> Result<Profile, String> {
    toml::from_str::<Profile>(text).map(|p| p.sanitized()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip() {
        let s = Settings::default();
        let t = s.to_toml().unwrap();
        assert_eq!(Settings::from_toml(&t).unwrap(), s.sanitized());
        assert_eq!(Settings::from_toml("").unwrap(), Settings::default().sanitized());
    }

    #[test]
    fn replay_toggle_hotkey_has_a_default_and_old_files_get_it() {
        let hk = Hotkeys::default().toggle_replay.unwrap();
        assert!(hk.ctrl && hk.shift && !hk.alt && hk.vk == 0x52);
        // A config written before the hotkey existed still loads and gets the default.
        let s = Settings::from_toml("[hotkeys]\nrecord = { ctrl = true, alt = false, shift = true, win = false, vk = 120 }\n").unwrap();
        assert_eq!(s.hotkeys.toggle_replay, Hotkeys::default().toggle_replay);
        // No two default hotkeys collide.
        let list = Hotkeys::default().list();
        for (i, (_, a)) in list.iter().enumerate() {
            assert!(list.iter().skip(i + 1).all(|(_, b)| a != b));
        }
    }

    #[test]
    fn high_quality_preview_is_sharper_and_removed_while_recording() {
        let normal = PreviewSettings { max_size: 640, ..PreviewSettings::default() };
        assert!(!normal.high_quality);
        assert_eq!(normal.to_platform(false).max_size, 640);
        assert!(normal.to_platform(true).enabled, "off by default: unchanged while recording");

        let hq = PreviewSettings { high_quality: true, ..normal };
        let idle = hq.to_platform(false);
        assert!(idle.enabled && idle.high_quality && idle.max_size >= PREVIEW_HQ_MAX_SIZE);
        assert!(!hq.to_platform(true).enabled, "removed while recording, even without pause_while_recording");
        // Old profiles without the field load with it off.
        let p = import_profile("name = \"x\"\n[preview]\nenabled = true\n").unwrap();
        assert!(!p.preview.high_quality);
    }

    #[test]
    fn hostile_values_are_clamped() {
        let mut s = Settings::default();
        s.profiles.clear();
        s.scenes.clear();
        s.active_profile = 99;
        s.replay.seconds = 0;
        s.recording.min_free_mb = 1_000_000;
        s.recording.warn_free_mb = 0;
        let s = s.sanitized();
        assert_eq!(s.profiles.len(), 4);
        assert_eq!(s.active_profile, 3);
        assert_eq!(s.scenes.len(), 1);
        assert_eq!(s.replay.seconds, 5);
        assert!(s.recording.warn_free_mb >= s.recording.min_free_mb);
        let bad = r#"
            [[scenes]]
            name = ""
            [[scenes.sources]]
            id = 7
            name = "a"
            kind = { Color = { rgba = [5.0, -1.0, 0.5, 1.0] } }
            rect = { x = 0.0, y = 0.0, w = -3.0, h = 1e30 }
            fit = "Stretch"
            crop = {}
            opacity = 9.0
            visible = true
            [[scenes.sources]]
            id = 7
            name = "b"
            kind = { Color = { rgba = [1.0, 1.0, 1.0, 1.0] } }
            rect = { x = 0.0, y = 0.0, w = 1.0, h = 1.0 }
            fit = "Stretch"
            crop = {}
            opacity = 1.0
            visible = true
        "#;
        let s = Settings::from_toml(bad).unwrap();
        let src = &s.scenes[0].sources;
        assert_eq!(src[0].opacity, 1.0);
        assert!(src[0].rect.w > 0.0 && src[0].rect.h <= 8.0);
        assert_ne!(src[0].id, src[1].id);
        assert_eq!(s.scenes[0].name, "Cena");
    }

    #[test]
    fn broken_file_is_set_aside() {
        let dir = std::env::temp_dir().join(format!("fluxcapture-settings-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), "this is = = not toml").unwrap();
        let l = load(&dir);
        assert!(l.warning.is_some());
        assert!(!dir.join("config.toml").exists());
        save(&dir, &l.settings).unwrap();
        let l2 = load(&dir);
        assert!(l2.warning.is_none());
        assert_eq!(l2.settings, l.settings);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bitrates() {
        let k = recommended_kbps(1920, 1080, 60.0, VideoCodecKind::H264);
        assert!((14_000..=30_000).contains(&k), "{k}");
        assert!(recommended_kbps(1920, 1080, 60.0, VideoCodecKind::Hevc) < k);
        assert!(recommended_kbps(0, 0, f64::NAN, VideoCodecKind::H264) >= 1_000);
        let p = Profile::preset(ProfileKind::Gaming);
        match p.video.rate_control(1920, 1080) {
            fluxcapture_platform::RateControl::Vbr { kbps, max_kbps } => assert!(max_kbps > kbps),
            other => panic!("{other:?}"),
        }
        let exported = export_profile(&p).unwrap();
        assert_eq!(import_profile(&exported).unwrap(), p.sanitized());
    }
}
