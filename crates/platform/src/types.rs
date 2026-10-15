//! Plain data exchanged with the platform layer. Identical on every operating system, so the engine
//! and UI compile (and their tests run) everywhere; only the backends differ.

use std::fmt;
use std::sync::Arc;

use fluxcapture_time::{FrameRate, Hns};
use serde::{Deserialize, Serialize};

/// A platform error with a message the user can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformError(pub String);

impl PlatformError {
    pub fn new(msg: impl Into<String>) -> PlatformError {
        PlatformError(msg.into())
    }

    pub fn unsupported(what: &str) -> PlatformError {
        PlatformError(format!("{what} is not supported on this operating system yet"))
    }
}

impl fmt::Display for PlatformError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PlatformError {}

impl From<String> for PlatformError {
    fn from(s: String) -> Self {
        PlatformError(s)
    }
}

impl From<&str> for PlatformError {
    fn from(s: &str) -> Self {
        PlatformError(s.to_string())
    }
}

pub type Result<T> = std::result::Result<T, PlatformError>;

/// GPU vendor, from the PCI vendor id.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GpuVendor {
    Nvidia,
    Amd,
    Intel,
    Microsoft,
    Qualcomm,
    Other(u32),
}

impl GpuVendor {
    pub fn from_pci(id: u32) -> GpuVendor {
        match id {
            0x10DE => GpuVendor::Nvidia,
            0x1002 | 0x1022 => GpuVendor::Amd,
            0x8086 => GpuVendor::Intel,
            0x1414 => GpuVendor::Microsoft,
            0x5143 | 0x4D4F_4351 => GpuVendor::Qualcomm,
            other => GpuVendor::Other(other),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            GpuVendor::Nvidia => "NVIDIA",
            GpuVendor::Amd => "AMD",
            GpuVendor::Intel => "Intel",
            GpuVendor::Microsoft => "Microsoft",
            GpuVendor::Qualcomm => "Qualcomm",
            GpuVendor::Other(_) => "?",
        }
    }
}

/// A graphics adapter.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GpuInfo {
    /// Locally unique id (stable until reboot); what every other API takes.
    pub luid: u64,
    pub name: String,
    pub vendor: GpuVendor,
    pub dedicated_video_memory: u64,
    pub shared_system_memory: u64,
    /// Software adapters (WARP / Basic Render Driver).
    pub software: bool,
    /// Enumeration order; 0 is the system's default adapter.
    pub index: u32,
}

/// A monitor.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MonitorInfo {
    /// GDI device name (`\\.\DISPLAY1`): stable across runs as long as the display layout is.
    pub id: String,
    /// Friendly name (the EDID monitor name) when available.
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
    pub refresh_hz: f64,
    /// Advanced colour (HDR) is on for this output.
    pub hdr: bool,
    /// The adapter driving it (0 if unknown).
    pub gpu_luid: u64,
    /// DPI scale (1.0 = 96 DPI).
    pub scale: f32,
}

/// A top-level window that can be captured.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowInfo {
    pub handle: u64,
    pub title: String,
    /// Executable file name (`game.exe`).
    pub process: String,
    pub pid: u32,
    pub width: u32,
    pub height: u32,
    pub minimized: bool,
}

/// What a capture layer captures.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CaptureTarget {
    /// A whole monitor, by [`MonitorInfo::id`]. An empty id means the primary monitor.
    Monitor { id: String },
    /// A window. The handle is tried first; when it no longer exists (the app restarted) the
    /// first window of `process` whose title contains `title` is used.
    Window { handle: u64, title: String, process: String },
}

/// How a capture source grabs pixels.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CaptureMethod {
    /// Windows.Graphics.Capture for windows; for monitors too, unless it is unavailable.
    #[default]
    Auto,
    /// Windows.Graphics.Capture (Windows 10 1903+).
    Wgc,
    /// DXGI Desktop Duplication (monitors only; lowest overhead for full-screen games on the
    /// adapter that drives the monitor).
    Duplication,
}

/// Capture options.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureOptions {
    pub cursor: bool,
    /// The yellow capture border (Windows 11 lets apps hide it).
    pub border: bool,
    pub method: CaptureMethod,
    /// Upper bound on frames delivered per second by the OS (Windows 11 24H2+ honours it at the
    /// source; elsewhere extra frames are simply not copied). `0` = no limit.
    pub max_fps: u32,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        CaptureOptions { cursor: true, border: false, method: CaptureMethod::Auto, max_fps: 0 }
    }
}

/// Straight-alpha RGBA8 pixels.
#[derive(Clone, PartialEq, Eq)]
pub struct RgbaImage {
    pub width: u32,
    pub height: u32,
    pub data: Arc<[u8]>,
}

impl fmt::Debug for RgbaImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RgbaImage({}x{})", self.width, self.height)
    }
}

impl RgbaImage {
    /// `None` unless `data` holds exactly `width * height * 4` bytes and the size is sane.
    pub fn new(width: u32, height: u32, data: Vec<u8>) -> Option<RgbaImage> {
        let expected = (width as usize).checked_mul(height as usize)?.checked_mul(4)?;
        (width > 0 && height > 0 && width <= 16384 && height <= 16384 && data.len() == expected).then(|| RgbaImage { width, height, data: data.into() })
    }
}

/// How a layer's content is fitted into its rectangle.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Fit {
    /// Keep the aspect ratio, letterbox inside the rectangle.
    #[default]
    Contain,
    /// Keep the aspect ratio, fill the rectangle (crop the overflow).
    Cover,
    /// Fill the rectangle exactly.
    Stretch,
    /// 1:1 pixels, centred.
    Original,
}

/// A rectangle in canvas pixels.
#[derive(Copy, Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RectF {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Pixels to cut from each edge of a source before fitting (region capture is a crop).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct Crop {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
}

/// The content of a scene layer.
#[derive(Clone, Debug, PartialEq)]
pub enum LayerSource {
    Capture {
        target: CaptureTarget,
        options: CaptureOptions,
    },
    /// Pre-rendered pixels: images and text (the engine rasterises text).
    Image(RgbaImage),
    /// A solid colour, straight-alpha RGBA in 0..=1.
    Color([f32; 4]),
    /// A webcam / capture card by device id (empty = first device).
    Webcam {
        device: String,
        width: u32,
        height: u32,
        fps: u32,
    },
}

/// One scene layer, drawn bottom-up in list order.
#[derive(Clone, Debug, PartialEq)]
pub struct Layer {
    /// Stable id (used to keep capture sessions alive across scene edits).
    pub id: u64,
    pub source: LayerSource,
    /// Destination in canvas pixels.
    pub rect: RectF,
    pub fit: Fit,
    pub crop: Crop,
    pub opacity: f32,
    pub visible: bool,
}

/// Resampling filter used when a layer is scaled.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ScaleFilter {
    /// Cheapest; fine for 1:1 and 2:1.
    Bilinear,
    /// Sharper; good default for downscaling.
    #[default]
    Bicubic,
    /// Sharpest; most GPU work.
    Lanczos,
}

/// Preview settings. The preview is a downscaled copy read back at a limited rate; it never
/// touches the recording path.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PreviewConfig {
    pub enabled: bool,
    /// Longest edge of the preview image in pixels.
    pub max_size: u32,
    pub fps: u32,
    /// Scale with the sharpest filter (Lanczos) instead of the cheap bilinear / bicubic pair.
    pub high_quality: bool,
}

impl Default for PreviewConfig {
    fn default() -> Self {
        PreviewConfig { enabled: true, max_size: 960, fps: 30, high_quality: false }
    }
}

/// A preview frame for the UI (RGBA8, tightly packed).
#[derive(Clone)]
pub struct PreviewFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub seq: u64,
}

/// Video codecs.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VideoCodecKind {
    #[default]
    H264,
    Hevc,
    Av1,
}

impl VideoCodecKind {
    pub fn label(self) -> &'static str {
        match self {
            VideoCodecKind::H264 => "H.264",
            VideoCodecKind::Hevc => "HEVC",
            VideoCodecKind::Av1 => "AV1",
        }
    }
}

/// An available video encoder.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VideoEncoderInfo {
    /// Stable id: `mft:{CLSID}`.
    pub id: String,
    pub name: String,
    pub codec: VideoCodecKind,
    pub hardware: bool,
    pub vendor: Option<GpuVendor>,
    /// The adapter a hardware encoder runs on, when the OS says.
    pub gpu_luid: Option<u64>,
}

/// Rate control.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RateControl {
    /// Constant bitrate (kbit/s).
    Cbr { kbps: u32 },
    /// Peak-constrained variable bitrate.
    Vbr { kbps: u32, max_kbps: u32 },
    /// Constant quality, 1 (smallest) ..= 100 (best).
    Quality { quality: u8 },
}

impl RateControl {
    /// Expected average bitrate, for disk estimates (quality mode: a rough guess from the
    /// resolution, frame rate and level).
    pub fn estimated_kbps(self, width: u32, height: u32, fps: f64) -> u32 {
        match self {
            RateControl::Cbr { kbps } => kbps,
            RateControl::Vbr { kbps, .. } => kbps,
            RateControl::Quality { quality } => {
                let pixels_per_sec = f64::from(width) * f64::from(height) * fps.max(1.0);
                // ~0.04 bits/pixel at quality 50, growing exponentially with quality.
                let bpp = 0.04 * 2f64.powf((f64::from(quality.clamp(1, 100)) - 50.0) / 12.0);
                (pixels_per_sec * bpp / 1000.0).clamp(500.0, 400_000.0) as u32
            }
        }
    }
}

/// Encoder configuration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VideoEncoderConfig {
    /// A [`VideoEncoderInfo::id`], or `None` for the best hardware encoder of `codec` (software
    /// fallback).
    pub encoder_id: Option<String>,
    pub codec: VideoCodecKind,
    pub width: u32,
    pub height: u32,
    pub fps: FrameRate,
    pub rate: RateControl,
    /// Seconds between keyframes (0.5 ..= 10).
    pub keyframe_interval_s: f32,
    /// 0 = fastest ..= 100 = best quality per bit.
    pub speed_quality: u8,
    /// Ask for the encoder's low-latency mode (no lookahead / reordering): least memory and GPU
    /// time, slightly larger files.
    pub low_latency: bool,
    /// Full-range (0-255) YUV instead of TV range. TV range is what every player expects.
    pub full_range: bool,
}

/// One encoded video access unit, Annex B.
#[derive(Clone, Debug)]
pub struct EncodedVideo {
    pub pts: Hns,
    pub dts: Hns,
    pub keyframe: bool,
    pub data: Vec<u8>,
}

/// One encoded audio frame (raw AAC).
#[derive(Clone, Debug)]
pub struct EncodedAudio {
    pub pts: Hns,
    pub duration: Hns,
    pub data: Vec<u8>,
}

/// State of a scene layer's source.
#[derive(Clone, Debug, PartialEq)]
pub enum SourceState {
    Starting,
    Active {
        width: u32,
        height: u32,
    },
    /// The window is minimized / the content is not updating; the last frame is shown.
    Paused,
    /// The window closed, the monitor was disconnected or the device was removed.
    Lost(String),
    Failed(String),
}

/// Rendering / encoding statistics, refreshed about four times per second.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VideoStats {
    pub canvas_width: u32,
    pub canvas_height: u32,
    pub fps_target: f64,
    /// Frames rendered (composited) since the engine started.
    pub frames_rendered: u64,
    /// Frames handed to the encoder since encoding started.
    pub frames_submitted: u64,
    /// Packets produced by the encoder since encoding started.
    pub frames_encoded: u64,
    /// Frame ticks missed because rendering ran late (GPU or CPU saturated).
    pub dropped_render_lag: u64,
    /// Frames dropped because the encoder fell behind (all input surfaces in use).
    pub dropped_encoder_backlog: u64,
    /// Ticks skipped on purpose in variable-frame-rate mode (nothing changed).
    pub skipped_static: u64,
    /// CPU time of one render tick (average / worst over the last window), ms.
    pub render_ms_avg: f32,
    pub render_ms_max: f32,
    /// GPU time of one render tick, ms (when timestamp queries work).
    pub gpu_ms_avg: Option<f32>,
    /// Encoder input → output latency, ms.
    pub encode_latency_ms_avg: f32,
    /// Frames inside the encoder right now.
    pub encoder_in_flight: u32,
    pub encoder_name: Option<String>,
    pub encoder_hardware: bool,
    /// Achieved output bitrate over the last window, kbit/s.
    pub bitrate_kbps: f32,
    /// Captured frames per second per layer id (what the OS delivers).
    pub capture_fps: Vec<(u64, f32)>,
    pub preview_fps: f32,
    pub preview_enabled: bool,
    pub encoder_error: Option<String>,
}

/// Where an audio capture reads from.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AudioEndpoint {
    /// What the default playback device plays (loopback); follows default-device changes.
    DefaultOutput,
    /// The default recording device (microphone); follows default-device changes.
    DefaultInput,
    /// A specific device by id; `loopback` for playback devices.
    Device { id: String, loopback: bool },
    /// Only the sound of one process tree (Windows 10 2004+).
    Process { pid: u32, name: String },
}

/// Audio device direction.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AudioFlow {
    Playback,
    Recording,
}

/// An audio endpoint device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioDeviceInfo {
    pub id: String,
    pub name: String,
    pub flow: AudioFlow,
    pub default: bool,
}

/// A process currently playing audio (for per-application capture).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioProcessInfo {
    pub pid: u32,
    pub name: String,
}

/// State of an audio capture.
#[derive(Clone, Debug, PartialEq)]
pub enum AudioCaptureState {
    Starting,
    Running {
        device: String,
    },
    /// The device disappeared; retrying.
    Reconnecting(String),
    Failed(String),
    Stopped,
}

/// Process resource usage.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct ProcessUsage {
    /// Kernel + user CPU time consumed so far.
    pub cpu_time: Hns,
    pub working_set: u64,
    pub private_bytes: u64,
}

/// System-wide CPU times (for total CPU usage).
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct SystemTimes {
    pub idle: Hns,
    pub kernel: Hns,
    pub user: Hns,
}

/// A global hotkey: modifiers + virtual-key code.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Hotkey {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
    /// Windows virtual-key code (`0x78` = F9).
    pub vk: u32,
}

impl Hotkey {
    pub fn label(&self) -> String {
        let mut s = String::new();
        for (on, name) in [(self.ctrl, "Ctrl+"), (self.alt, "Alt+"), (self.shift, "Shift+"), (self.win, "Win+")] {
            if on {
                s.push_str(name);
            }
        }
        s.push_str(&vk_name(self.vk));
        s
    }
}

/// A readable name for a virtual-key code.
pub fn vk_name(vk: u32) -> String {
    match vk {
        0x70..=0x87 => format!("F{}", vk - 0x6F),
        0x30..=0x39 | 0x41..=0x5A => char::from_u32(vk).map(String::from).unwrap_or_default(),
        0x60..=0x69 => format!("Num{}", vk - 0x60),
        0x20 => "Space".into(),
        0x2C => "PrintScreen".into(),
        0x13 => "Pause".into(),
        0x91 => "ScrollLock".into(),
        0x2D => "Insert".into(),
        0x24 => "Home".into(),
        0x23 => "End".into(),
        0x21 => "PageUp".into(),
        0x22 => "PageDown".into(),
        _ => format!("VK{vk:#04X}"),
    }
}
