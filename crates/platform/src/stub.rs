//! The backend for operating systems without a native one yet (Linux: PipeWire / portals; macOS:
//! ScreenCaptureKit are planned). Same API as the Windows backend; everything that needs the OS
//! reports "unsupported", so the engine, UI and tests still build and run.

pub mod clock {
    use std::sync::OnceLock;
    use std::time::Instant;

    use fluxcapture_time::Hns;

    fn origin() -> Instant {
        static O: OnceLock<Instant> = OnceLock::new();
        *O.get_or_init(Instant::now)
    }

    pub fn now() -> Hns {
        Hns(i64::try_from(origin().elapsed().as_nanos() / 100).unwrap_or(i64::MAX))
    }

    pub struct PrecisionTimer;

    impl PrecisionTimer {
        pub fn new() -> crate::Result<PrecisionTimer> {
            Ok(PrecisionTimer)
        }

        pub fn wait_until(&self, target: Hns) {
            let d = target.0.saturating_sub(now().0);
            if d > 0 {
                std::thread::sleep(std::time::Duration::from_nanos(d as u64 * 100));
            }
        }
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum ThreadKind {
        Capture,
        ProAudio,
        Encode,
    }

    pub struct PriorityGuard;

    pub fn boost_thread(_kind: ThreadKind) -> PriorityGuard {
        PriorityGuard
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum ProcessPriority {
        Normal,
        AboveNormal,
        High,
    }

    pub fn set_process_priority(_p: ProcessPriority) -> crate::Result<()> {
        Ok(())
    }
}

pub mod display {
    use crate::{GpuInfo, MonitorInfo, WindowInfo};

    pub fn list_gpus() -> crate::Result<Vec<GpuInfo>> {
        Ok(Vec::new())
    }

    pub fn list_monitors() -> crate::Result<Vec<MonitorInfo>> {
        Ok(Vec::new())
    }

    pub fn list_windows() -> crate::Result<Vec<WindowInfo>> {
        Ok(Vec::new())
    }
}

pub mod system {
    use std::path::{Path, PathBuf};

    use crate::{PlatformError, ProcessUsage, SystemTimes};

    pub fn disk_free(_path: &Path) -> crate::Result<u64> {
        Err(PlatformError::unsupported("Free disk space"))
    }

    pub fn process_usage() -> crate::Result<ProcessUsage> {
        Err(PlatformError::unsupported("Process usage"))
    }

    pub fn system_times() -> crate::Result<SystemTimes> {
        Err(PlatformError::unsupported("System times"))
    }

    pub fn memory_status() -> (u64, u64) {
        (0, 0)
    }

    pub fn cpu_count() -> usize {
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
    }

    pub fn videos_dir() -> Option<PathBuf> {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Videos"))
    }

    pub fn config_dir() -> Option<PathBuf> {
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    }

    pub fn exclude_window_from_capture(_hwnd: u64, _exclude: bool) -> crate::Result<()> {
        Err(PlatformError::unsupported("Capture exclusion"))
    }

    pub fn window_icon_sizes() -> (u32, u32) {
        (32, 16)
    }

    pub fn set_window_icon(_hwnd: u64, _big: (&[u8], u32), _small: (&[u8], u32)) -> crate::Result<()> {
        Err(PlatformError::unsupported("Window icons"))
    }

    pub fn protect_secret(_plain: &[u8]) -> crate::Result<Vec<u8>> {
        Err(PlatformError::unsupported("Secret storage"))
    }

    pub fn unprotect_secret(_sealed: &[u8]) -> crate::Result<Vec<u8>> {
        Err(PlatformError::unsupported("Secret storage"))
    }

    pub fn os_version() -> (u32, u32, u32) {
        (0, 0, 0)
    }

    pub fn os_description() -> String {
        std::env::consts::OS.to_string()
    }

    pub fn local_time() -> (i32, u32, u32, u32, u32, u32) {
        let s = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let days = (s / 86_400) as i64;
        // Civil date from days since the epoch (UTC).
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        let y = (yoe + era * 400 + i64::from(m <= 2)) as i32;
        let r = s % 86_400;
        (y, m, d, (r / 3600) as u32, ((r / 60) % 60) as u32, (r % 60) as u32)
    }

    pub fn reveal_in_explorer(_path: &Path) -> crate::Result<()> {
        Err(PlatformError::unsupported("Revealing files"))
    }

    pub fn open_with_default_app(_path: &Path) -> crate::Result<()> {
        Err(PlatformError::unsupported("Opening files"))
    }
}

pub mod install {
    use std::path::{Path, PathBuf};

    use crate::PlatformError;

    pub fn user_programs_dir() -> Option<PathBuf> {
        None
    }

    pub fn start_menu_dir() -> Option<PathBuf> {
        None
    }

    pub fn desktop_dir() -> Option<PathBuf> {
        None
    }

    pub fn ui_language_is_portuguese() -> bool {
        std::env::var("LANG").is_ok_and(|l| l.starts_with("pt"))
    }

    pub fn create_shortcut(_link: &Path, _target: &Path, _description: &str) -> crate::Result<()> {
        Err(PlatformError::unsupported("Shortcuts"))
    }

    pub fn refresh_shell() {}

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum MessageKind {
        Info,
        Error,
        Question,
    }

    pub fn message_box(_title: &str, _text: &str, _kind: MessageKind) -> bool {
        false
    }

    pub enum RegValue<'a> {
        Text(&'a str),
        Number(u32),
    }

    pub fn write_uninstall_entry(_id: &str, _values: &[(&str, RegValue)]) -> crate::Result<()> {
        Err(PlatformError::unsupported("Uninstall entries"))
    }

    pub fn read_uninstall_value(_id: &str, _name: &str) -> Option<String> {
        None
    }

    pub fn delete_uninstall_entry(_id: &str) -> crate::Result<()> {
        Ok(())
    }
}

pub mod audio {
    use crossbeam_channel::Sender;
    use fluxcapture_audio::{AudioChunk, SourceId};
    use fluxcapture_time::Hns;

    use crate::{AudioCaptureState, AudioDeviceInfo, AudioEndpoint, AudioProcessInfo, EncodedAudio, PlatformError};

    pub fn list_audio_devices() -> crate::Result<Vec<AudioDeviceInfo>> {
        Ok(Vec::new())
    }

    pub fn list_audio_processes() -> crate::Result<Vec<AudioProcessInfo>> {
        Ok(Vec::new())
    }

    pub fn process_capture_supported() -> bool {
        false
    }

    pub struct AudioCapture;

    impl AudioCapture {
        pub fn start(_endpoint: AudioEndpoint, _source: SourceId, _sink: Sender<AudioChunk>) -> crate::Result<AudioCapture> {
            Err(PlatformError::unsupported("Audio capture"))
        }

        pub fn state(&self) -> AudioCaptureState {
            AudioCaptureState::Failed("unsupported".into())
        }

        pub fn dropped_chunks(&self) -> u64 {
            0
        }
    }

    pub struct AacEncoder;

    impl AacEncoder {
        pub const BITRATES: [u32; 4] = [96, 128, 160, 192];

        pub fn new(_sample_rate: u32, _channels: u16, _kbps: u32) -> crate::Result<AacEncoder> {
            Err(PlatformError::unsupported("AAC encoding"))
        }

        pub fn sample_rate(&self) -> u32 {
            48_000
        }

        pub fn channels(&self) -> u16 {
            2
        }

        pub fn encode(&mut self, _pcm: &[i16], _time: Hns, _out: &mut Vec<EncodedAudio>) -> crate::Result<()> {
            Err(PlatformError::unsupported("AAC encoding"))
        }

        pub fn finish(&mut self, _out: &mut Vec<EncodedAudio>) -> crate::Result<()> {
            Ok(())
        }
    }
}

pub mod hotkeys {
    use crossbeam_channel::Sender;

    use crate::{Hotkey, PlatformError};

    pub struct HotkeyListener;

    impl HotkeyListener {
        pub fn start(_keys: Vec<Hotkey>, _sink: Sender<usize>) -> crate::Result<HotkeyListener> {
            Err(PlatformError::unsupported("Global hotkeys"))
        }

        pub fn failures(&self) -> Vec<(usize, String)> {
            Vec::new()
        }
    }
}

pub mod video {
    use std::sync::Arc;

    use crossbeam_channel::Sender;
    use fluxcapture_time::FrameRate;

    use crate::{
        EncodedVideo, Layer, PlatformError, PreviewConfig, PreviewFrame, RgbaImage, ScaleFilter, SourceState, VideoEncoderConfig, VideoEncoderInfo, VideoStats,
    };

    pub fn list_video_encoders() -> crate::Result<Vec<VideoEncoderInfo>> {
        Ok(Vec::new())
    }

    pub fn capture_supported() -> bool {
        false
    }

    pub fn list_webcams() -> crate::Result<Vec<(String, String)>> {
        Ok(Vec::new())
    }

    #[derive(Clone, Debug, PartialEq)]
    pub struct VideoEngineConfig {
        pub gpu: Option<u64>,
        pub width: u32,
        pub height: u32,
        pub fps: FrameRate,
        pub scale_filter: ScaleFilter,
        pub preview: PreviewConfig,
        pub variable_frame_rate: bool,
    }

    pub struct VideoEngine;

    impl VideoEngine {
        pub fn new(_cfg: VideoEngineConfig) -> crate::Result<VideoEngine> {
            Err(PlatformError::unsupported("Screen capture"))
        }

        pub fn gpu_name(&self) -> &str {
            ""
        }

        pub fn gpu_luid(&self) -> u64 {
            0
        }

        pub fn set_scene(&self, _layers: Vec<Layer>) {}

        pub fn set_preview(&self, _p: PreviewConfig) {}

        pub fn set_canvas(&self, _w: u32, _h: u32, _fps: FrameRate, _f: ScaleFilter, _vfr: bool) -> crate::Result<()> {
            Err(PlatformError::unsupported("Screen capture"))
        }

        pub fn start_encoder(&self, _cfg: VideoEncoderConfig, _sink: Sender<EncodedVideo>) -> crate::Result<VideoEncoderInfo> {
            Err(PlatformError::unsupported("Video encoding"))
        }

        pub fn stop_encoder(&self) {}

        pub fn request_keyframe(&self) {}

        pub fn screenshot(&self) -> crate::Result<RgbaImage> {
            Err(PlatformError::unsupported("Screenshots"))
        }

        pub fn preview(&self) -> Option<Arc<PreviewFrame>> {
            None
        }

        pub fn stats(&self) -> VideoStats {
            VideoStats::default()
        }

        pub fn source_states(&self) -> Vec<(u64, SourceState)> {
            Vec::new()
        }

        pub fn fatal_error(&self) -> Option<String> {
            None
        }
    }
}
