//! The video engine: capture sources → scene composition → (preview, NV12 → encoder), all on one
//! GPU, driven by one high-priority render thread.
//!
//! The render thread ticks at the output frame rate on the media clock. Frame `k` is stamped with
//! its *scheduled* time (`epoch + FrameRate::frame_time(k)`), so pacing jitter never shows up in the
//! file. Each tick composes the latest frame of every source into the canvas (BGRA8, output size);
//! when encoding, the canvas is converted into a pool surface and handed to the encoder thread.
//! The preview is a downscaled copy read back through a ring of staging textures without ever
//! waiting for the GPU (`D3D11_MAP_FLAG_DO_NOT_WAIT`). With no preview and no encoder the thread
//! sleeps on its command channel and every capture session is stopped.

mod compositor;
mod dup;
mod encoder;
mod webcam;
mod wgc;

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use fluxcapture_time::{FrameRate, Hns};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_SHADER_RESOURCE, D3D11_MAP_FLAG_DO_NOT_WAIT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_QUERY_DATA_TIMESTAMP_DISJOINT, D3D11_QUERY_DESC,
    D3D11_QUERY_TIMESTAMP, D3D11_QUERY_TIMESTAMP_DISJOINT, D3D11_RESOURCE_MISC_GENERATE_MIPS, ID3D11Query, ID3D11RenderTargetView, ID3D11ShaderResourceView,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM};

use self::compositor::{Compositor, Quad, rtv, srv};
use self::encoder::EncoderHandle;
use super::d3d::Gpu;
use crate::clock::{PrecisionTimer, ThreadKind, boost_thread, now};
use crate::{
    CaptureMethod, CaptureOptions, CaptureTarget, Crop, EncodedVideo, Fit, Layer, LayerSource, PlatformError, PreviewConfig, PreviewFrame, RgbaImage,
    ScaleFilter, SourceState, VideoCodecKind, VideoEncoderConfig, VideoEncoderInfo, VideoStats,
};

pub use self::encoder::list as list_encoders_for;

/// The newest frame of a capture source.
pub(crate) struct CapturedFrame {
    pub view: ID3D11ShaderResourceView,
    pub width: u32,
    pub height: u32,
    pub seq: u64,
    pub mip_levels: u32,
    /// Cursor drawn by us (Desktop Duplication): view, top-left in source pixels, size.
    pub cursor: Option<(ID3D11ShaderResourceView, i32, i32, u32, u32)>,
}

/// A running capture (Windows.Graphics.Capture, Desktop Duplication, webcam).
pub(crate) trait CaptureSource: Send {
    fn frame(&self) -> Option<CapturedFrame>;
    fn state(&self) -> SourceState;
    /// The render thread downsamples this source by 2× or more: keep a mip chain.
    fn set_want_mips(&self, want: bool);
    /// Frames delivered by the OS so far.
    fn delivered(&self) -> u64;
    #[allow(dead_code)]
    fn last_frame_time(&self) -> Hns;
}

/// Every video encoder installed, for all codecs.
pub fn list_video_encoders() -> crate::Result<Vec<VideoEncoderInfo>> {
    let mut all = Vec::new();
    for codec in [VideoCodecKind::H264, VideoCodecKind::Hevc, VideoCodecKind::Av1] {
        all.extend(encoder::list(codec).unwrap_or_default());
    }
    // Some drivers register the same MFT once per adapter output: keep one per (id, adapter).
    let mut seen = std::collections::HashSet::new();
    all.retain(|e| seen.insert((e.id.clone(), e.gpu_luid)));
    Ok(all)
}

/// Whether Windows.Graphics.Capture is available.
pub fn capture_supported() -> bool {
    wgc::supported()
}

/// Webcams / capture cards: (id, name).
pub fn list_webcams() -> crate::Result<Vec<(String, String)>> {
    webcam::list()
}

/// Engine configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct VideoEngineConfig {
    /// Adapter LUID; `None` = automatic (the adapter of the primary monitor if it can encode).
    pub gpu: Option<u64>,
    pub width: u32,
    pub height: u32,
    pub fps: FrameRate,
    pub scale_filter: ScaleFilter,
    pub preview: PreviewConfig,
    /// Skip encoding ticks where nothing changed (variable frame rate; still at least one frame per
    /// second).
    pub variable_frame_rate: bool,
}

enum Cmd {
    Scene(Vec<Layer>),
    Preview(PreviewConfig),
    Canvas { width: u32, height: u32, fps: FrameRate, filter: ScaleFilter, vfr: bool, reply: Sender<crate::Result<()>> },
    StartEncoder { cfg: Box<VideoEncoderConfig>, sink: Sender<EncodedVideo>, reply: Sender<crate::Result<VideoEncoderInfo>> },
    StopEncoder { reply: Sender<()> },
    Keyframe,
    Screenshot { reply: Sender<crate::Result<RgbaImage>> },
    Stop,
}

#[derive(Default)]
struct Shared {
    stats: Mutex<VideoStats>,
    preview: Mutex<Option<Arc<PreviewFrame>>>,
    sources: Mutex<Vec<(u64, SourceState)>>,
    fatal: Mutex<Option<String>>,
}

/// Handle to the render thread. Dropping it stops everything (draining the encoder).
pub struct VideoEngine {
    tx: Sender<Cmd>,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    gpu_name: String,
    gpu_luid: u64,
}

impl VideoEngine {
    pub fn new(cfg: VideoEngineConfig) -> crate::Result<VideoEngine> {
        validate_canvas(cfg.width, cfg.height)?;
        let luid = cfg.gpu.or_else(super::d3d::default_adapter_luid);
        let gpu = Gpu::new(luid).or_else(|e| if luid.is_some() { Gpu::new(None) } else { Err(e) })?;
        let comp = Compositor::new(&gpu)?;
        let (gpu_name, gpu_luid) = (gpu.name.clone(), gpu.luid);
        let (tx, rx) = unbounded();
        let shared = Arc::new(Shared::default());
        let shared2 = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("fluxcapture-render".into())
            .spawn(move || {
                let r = crate::guarded("the render thread", || {
                    let mut r = Renderer::new(gpu, comp, cfg, Arc::clone(&shared2))?;
                    r.run(&rx)
                });
                if let Err(e) = r {
                    log::error!("render thread stopped: {e}");
                    *shared2.fatal.lock().unwrap_or_else(PoisonError::into_inner) = Some(e.0);
                }
            })
            .map_err(|e| PlatformError(format!("cannot start the render thread: {e}")))?;
        Ok(VideoEngine { tx, shared, thread: Some(thread), gpu_name, gpu_luid })
    }

    pub fn gpu_name(&self) -> &str {
        &self.gpu_name
    }

    pub fn gpu_luid(&self) -> u64 {
        self.gpu_luid
    }

    /// Replaces the scene. Layers keep their running capture when their id and source are unchanged.
    pub fn set_scene(&self, layers: Vec<Layer>) {
        let _ = self.tx.send(Cmd::Scene(layers));
    }

    pub fn set_preview(&self, p: PreviewConfig) {
        let _ = self.tx.send(Cmd::Preview(p));
    }

    /// Changes the output canvas (refused while encoding).
    pub fn set_canvas(&self, width: u32, height: u32, fps: FrameRate, filter: ScaleFilter, vfr: bool) -> crate::Result<()> {
        let (reply, rx) = bounded(1);
        self.tx.send(Cmd::Canvas { width, height, fps, filter, vfr, reply }).map_err(|_| self.dead())?;
        rx.recv_timeout(Duration::from_secs(10)).map_err(|_| self.dead())?
    }

    /// Starts encoding the canvas; packets go to `sink` (bounded by the caller).
    pub fn start_encoder(&self, cfg: VideoEncoderConfig, sink: Sender<EncodedVideo>) -> crate::Result<VideoEncoderInfo> {
        let (reply, rx) = bounded(1);
        self.tx.send(Cmd::StartEncoder { cfg: Box::new(cfg), sink, reply }).map_err(|_| self.dead())?;
        rx.recv_timeout(Duration::from_secs(20)).map_err(|_| self.dead())?
    }

    /// Stops encoding after every submitted frame has come out.
    pub fn stop_encoder(&self) {
        let (reply, rx) = bounded(1);
        if self.tx.send(Cmd::StopEncoder { reply }).is_ok() {
            let _ = rx.recv_timeout(Duration::from_secs(10));
        }
    }

    pub fn request_keyframe(&self) {
        let _ = self.tx.send(Cmd::Keyframe);
    }

    /// The canvas as RGBA (full resolution).
    pub fn screenshot(&self) -> crate::Result<RgbaImage> {
        let (reply, rx) = bounded(1);
        self.tx.send(Cmd::Screenshot { reply }).map_err(|_| self.dead())?;
        rx.recv_timeout(Duration::from_secs(5)).map_err(|_| self.dead())?
    }

    pub fn preview(&self) -> Option<Arc<PreviewFrame>> {
        self.shared.preview.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    pub fn stats(&self) -> VideoStats {
        self.shared.stats.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    pub fn source_states(&self) -> Vec<(u64, SourceState)> {
        self.shared.sources.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Why the render thread died, if it did.
    pub fn fatal_error(&self) -> Option<String> {
        self.shared.fatal.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn dead(&self) -> PlatformError {
        PlatformError(self.fatal_error().unwrap_or_else(|| "the video engine is not running".into()))
    }
}

impl Drop for VideoEngine {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn validate_canvas(w: u32, h: u32) -> crate::Result<()> {
    if !(16..=8192).contains(&w) || !(16..=8192).contains(&h) || !w.is_multiple_of(2) || !h.is_multiple_of(2) {
        return Err(PlatformError(format!("invalid output size {w}x{h} (even sizes from 16 to 8192)")));
    }
    Ok(())
}

struct Canvas {
    texture: ID3D11Texture2D,
    rtv: ID3D11RenderTargetView,
    srv: ID3D11ShaderResourceView,
    width: u32,
    height: u32,
}

impl Canvas {
    fn new(gpu: &Gpu, w: u32, h: u32) -> crate::Result<Canvas> {
        let texture = gpu.render_texture(w, h, DXGI_FORMAT_B8G8R8A8_UNORM)?;
        Ok(Canvas { rtv: rtv(gpu, &texture, None)?, srv: srv(gpu, &texture)?, texture, width: w, height: h })
    }
}

struct StaticTexture {
    srv: ID3D11ShaderResourceView,
    width: u32,
    height: u32,
    mips: u32,
}

enum Runtime {
    Capture(Box<dyn CaptureSource>),
    /// A capture that failed to start; retried periodically (the window may open later).
    Waiting {
        error: String,
        retry_at: Hns,
    },
    Static(StaticTexture),
    Color([f32; 4]),
}

struct LayerRt {
    layer: Layer,
    rt: Runtime,
    last_seq: u64,
    delivered_mark: u64,
}

struct Preview {
    cfg: PreviewConfig,
    target: Option<(ID3D11Texture2D, ID3D11RenderTargetView, u32, u32)>,
    staging: Vec<ID3D11Texture2D>,
    written: u64,
    seq: u64,
    published: u64,
    /// Preview refreshes still owed after the last change (readback lags by the ring size).
    flush: u32,
}

impl Preview {
    /// Readback refreshes needed to push one change through the staging ring.
    const FLUSH: u32 = 4;

    /// Drops the preview textures (size change) and owes a full flush so the new size shows up
    /// even when nothing on screen moves.
    fn reset_target(&mut self) {
        self.target = None;
        self.staging.clear();
        self.written = 0;
        // The ring restarts at zero: a stale `published` would hold back every new frame until
        // `written` caught up with it, freezing the preview.
        self.published = 0;
        self.flush = Self::FLUSH;
    }
}

struct GpuTimer {
    sets: Vec<(ID3D11Query, ID3D11Query, ID3D11Query)>,
    index: usize,
    issued: usize,
    sum_ms: f64,
    n: u32,
}

impl GpuTimer {
    fn new(gpu: &Gpu) -> Option<GpuTimer> {
        let make = |q| -> Option<ID3D11Query> {
            let mut out = None;
            // SAFETY: valid descriptor, live out-local.
            unsafe { gpu.device.CreateQuery(&D3D11_QUERY_DESC { Query: q, MiscFlags: 0 }, Some(&mut out)) }.ok()?;
            out
        };
        let mut sets = Vec::new();
        for _ in 0..4 {
            sets.push((make(D3D11_QUERY_TIMESTAMP_DISJOINT)?, make(D3D11_QUERY_TIMESTAMP)?, make(D3D11_QUERY_TIMESTAMP)?));
        }
        Some(GpuTimer { sets, index: 0, issued: 0, sum_ms: 0.0, n: 0 })
    }

    fn begin(&self, gpu: &Gpu) {
        if let Some((d, a, _)) = self.sets.get(self.index) {
            // SAFETY: queries created on this device.
            unsafe {
                gpu.context.Begin(d);
                gpu.context.End(a);
            }
        }
    }

    fn end(&mut self, gpu: &Gpu) {
        if let Some((d, _, b)) = self.sets.get(self.index) {
            // SAFETY: queries created on this device.
            unsafe {
                gpu.context.End(b);
                gpu.context.End(d);
            }
        }
        self.index = (self.index + 1) % self.sets.len();
        self.issued += 1;
        // Read the set issued three frames ago, without waiting.
        if self.issued >= self.sets.len()
            && let Some((d, a, b)) = self.sets.get(self.index)
        {
            let mut dis = D3D11_QUERY_DATA_TIMESTAMP_DISJOINT::default();
            let (mut ta, mut tb) = (0u64, 0u64);
            // SAFETY: each GetData writes exactly the size passed into a live local; flag 1 =
            // D3D11_ASYNC_GETDATA_DONOTFLUSH.
            let ok = unsafe {
                gpu.context.GetData(d, Some((&mut dis as *mut D3D11_QUERY_DATA_TIMESTAMP_DISJOINT).cast()), std::mem::size_of_val(&dis) as u32, 1).is_ok()
                    && gpu.context.GetData(a, Some((&mut ta as *mut u64).cast()), 8, 1).is_ok()
                    && gpu.context.GetData(b, Some((&mut tb as *mut u64).cast()), 8, 1).is_ok()
            };
            if ok && !dis.Disjoint.as_bool() && dis.Frequency > 0 && tb >= ta {
                self.sum_ms += (tb - ta) as f64 * 1000.0 / dis.Frequency as f64;
                self.n += 1;
            }
        }
    }

    fn take_avg(&mut self) -> Option<f32> {
        let r = (self.n > 0).then(|| (self.sum_ms / f64::from(self.n)) as f32);
        self.sum_ms = 0.0;
        self.n = 0;
        r
    }
}

#[derive(Default)]
struct Window {
    start: Hns,
    ticks: u32,
    cpu_sum: Hns,
    cpu_max: Hns,
    bytes_mark: u64,
    preview_frames: u32,
}

struct Renderer {
    gpu: Gpu,
    comp: Compositor,
    canvas: Canvas,
    fps: FrameRate,
    filter: ScaleFilter,
    vfr: bool,
    layers: Vec<LayerRt>,
    preview: Preview,
    encoder: Option<EncoderHandle>,
    keyframe_pending: bool,
    last_encoded: Hns,
    scene_dirty: bool,
    stats: VideoStats,
    shared: Arc<Shared>,
    gpu_timer: Option<GpuTimer>,
    window: Window,
    /// Ticks since the thread started (never reset).
    ticks: u64,
    /// A command blocked the thread before encoding began: restart the frame clock instead of
    /// counting the stall as lost frames.
    resync: bool,
}

impl Renderer {
    fn new(gpu: Gpu, comp: Compositor, cfg: VideoEngineConfig, shared: Arc<Shared>) -> crate::Result<Renderer> {
        let canvas = Canvas::new(&gpu, cfg.width, cfg.height)?;
        let gpu_timer = GpuTimer::new(&gpu);
        Ok(Renderer {
            canvas,
            fps: cfg.fps,
            filter: cfg.scale_filter,
            vfr: cfg.variable_frame_rate,
            layers: Vec::new(),
            preview: Preview { cfg: cfg.preview, target: None, staging: Vec::new(), written: 0, seq: 0, published: 0, flush: Preview::FLUSH },
            ticks: 0,
            resync: false,
            encoder: None,
            keyframe_pending: false,
            last_encoded: Hns(i64::MIN / 2),
            scene_dirty: true,
            stats: VideoStats::default(),
            shared,
            gpu_timer,
            window: Window::default(),
            gpu,
            comp,
        })
    }

    fn active(&self) -> bool {
        self.encoder.is_some() || self.preview.cfg.enabled
    }

    /// Returns `false` to stop the thread.
    fn handle(&mut self, cmd: Cmd) -> bool {
        match cmd {
            Cmd::Scene(layers) => self.set_scene(layers),
            Cmd::Preview(p) => {
                if p.max_size != self.preview.cfg.max_size {
                    self.preview.reset_target();
                }
                if p != self.preview.cfg {
                    // Show the new settings at once, even on a static screen.
                    self.preview.flush = Preview::FLUSH;
                }
                let was = self.preview.cfg.enabled;
                self.preview.cfg = p;
                if p.enabled && !was {
                    self.wake_captures();
                    self.resync = true;
                }
                if !p.enabled {
                    *self.shared.preview.lock().unwrap_or_else(PoisonError::into_inner) = None;
                }
            }
            Cmd::Canvas { width, height, fps, filter, vfr, reply } => {
                let r = if self.encoder.is_some() && (width != self.canvas.width || height != self.canvas.height || fps != self.fps) {
                    Err(PlatformError("cannot change the output size or frame rate while recording".into()))
                } else {
                    validate_canvas(width, height).and_then(|()| {
                        if width != self.canvas.width || height != self.canvas.height {
                            self.canvas = Canvas::new(&self.gpu, width, height)?;
                            self.preview.reset_target();
                        }
                        self.resync = true;
                        self.fps = fps;
                        self.filter = filter;
                        self.vfr = vfr;
                        self.scene_dirty = true;
                        Ok(())
                    })
                };
                let _ = reply.send(r);
            }
            Cmd::StartEncoder { cfg, sink, reply } => {
                let r = if self.encoder.is_some() {
                    Err(PlatformError("already encoding".into()))
                } else if cfg.width != self.canvas.width || cfg.height != self.canvas.height || cfg.fps != self.fps {
                    Err(PlatformError("the encoder size / frame rate must match the canvas".into()))
                } else {
                    self.resync = true;
                    // Start released captures now, before the clock runs, not inside encoded ticks.
                    self.wake_captures();
                    EncoderHandle::start(&self.gpu, &cfg, sink).map(|h| {
                        let info = h.info.clone();
                        self.encoder = Some(h);
                        self.keyframe_pending = true;
                        self.stats.frames_submitted = 0;
                        self.stats.frames_encoded = 0;
                        self.stats.dropped_encoder_backlog = 0;
                        self.stats.dropped_render_lag = 0;
                        self.stats.skipped_static = 0;
                        self.window.bytes_mark = 0;
                        info
                    })
                };
                let _ = reply.send(r);
            }
            Cmd::StopEncoder { reply } => {
                if let Some(e) = self.encoder.take() {
                    e.stop();
                }
                let _ = reply.send(());
            }
            Cmd::Keyframe => self.keyframe_pending = true,
            Cmd::Screenshot { reply } => {
                let _ = reply.send(self.screenshot());
            }
            Cmd::Stop => {
                if let Some(e) = self.encoder.take() {
                    e.stop();
                }
                return false;
            }
        }
        true
    }

    fn run(&mut self, rx: &Receiver<Cmd>) -> crate::Result<()> {
        let timer = PrecisionTimer::new()?;
        let _prio = boost_thread(ThreadKind::Capture);
        let mut epoch = now();
        let mut k: u64 = 0;
        let mut fps = self.fps;
        loop {
            while let Ok(cmd) = rx.try_recv() {
                if !self.handle(cmd) {
                    return Ok(());
                }
            }
            if !self.active() {
                // Nothing consumes frames: release every capture session and sleep.
                self.release_captures();
                self.publish_stats();
                match rx.recv() {
                    Ok(cmd) => {
                        if !self.handle(cmd) {
                            return Ok(());
                        }
                    }
                    Err(_) => return Ok(()),
                }
                epoch = now();
                k = 0;
                continue;
            }
            if fps != self.fps || self.resync {
                fps = self.fps;
                self.resync = false;
                epoch = now();
                k = 0;
            }
            let mut target = epoch + fps.frame_time(k);
            timer.wait_until(target);
            let late_by = now() - target;
            if late_by > fps.frame_duration() {
                // Fell behind by whole frames: skip them (they are lost frames if encoding).
                let current = fps.frame_index_at(now() - epoch);
                let missed = current.saturating_sub(k);
                if self.encoder.is_some() {
                    self.stats.dropped_render_lag += missed;
                }
                k = current;
                target = epoch + fps.frame_time(k);
            }
            self.tick(target);
            k += 1;
            if k > 1 << 40 {
                epoch = target;
                k = 0;
            }
        }
    }

    /// Recreates captures that were released while idle (or are waiting to retry).
    fn wake_captures(&mut self) {
        let t = now();
        for i in 0..self.layers.len() {
            let due = matches!(self.layers.get(i).map(|l| &l.rt), Some(Runtime::Waiting { retry_at, .. }) if *retry_at <= t);
            if due && let Some(src) = self.layers.get(i).map(|l| l.layer.source.clone()) {
                let rt = self.create_runtime(&src);
                if let Some(l) = self.layers.get_mut(i) {
                    l.rt = rt;
                }
            }
        }
    }

    fn release_captures(&mut self) {
        for l in &mut self.layers {
            if matches!(l.rt, Runtime::Capture(_)) {
                l.rt = Runtime::Waiting { error: "paused".into(), retry_at: Hns(0) };
            }
        }
    }

    fn set_scene(&mut self, layers: Vec<Layer>) {
        let mut old: HashMap<u64, LayerRt> = self.layers.drain(..).map(|l| (l.layer.id, l)).collect();
        for layer in layers {
            let keep = old.remove(&layer.id).filter(|o| same_source(&o.layer.source, &layer.source));
            let rt = match keep {
                Some(o) => LayerRt { layer, rt: o.rt, last_seq: o.last_seq, delivered_mark: o.delivered_mark },
                None => LayerRt { rt: self.create_runtime(&layer.source), layer, last_seq: 0, delivered_mark: 0 },
            };
            self.layers.push(rt);
        }
        self.scene_dirty = true;
    }

    fn create_runtime(&self, source: &LayerSource) -> Runtime {
        match source {
            LayerSource::Capture { target, options } => match start_capture(&self.gpu, target, options) {
                Ok(c) => Runtime::Capture(c),
                Err(e) => Runtime::Waiting { error: e.0, retry_at: now() + Hns::from_secs(2) },
            },
            LayerSource::Webcam { device, width, height, fps } => match webcam::WebcamSource::start(&self.gpu, device, *width, *height, *fps) {
                Ok(c) => Runtime::Capture(Box::new(c)),
                Err(e) => Runtime::Waiting { error: e.0, retry_at: now() + Hns::from_secs(3) },
            },
            LayerSource::Image(img) => match static_texture(&self.gpu, img) {
                Ok(t) => Runtime::Static(t),
                Err(e) => Runtime::Waiting { error: e.0, retry_at: Hns::MAX },
            },
            LayerSource::Color(c) => Runtime::Color(*c),
        }
    }

    /// Restarts captures that failed or were released, and reports source states.
    fn maintain_sources(&mut self, t: Hns) {
        let mut states = Vec::with_capacity(self.layers.len());
        for i in 0..self.layers.len() {
            // A lost source (window closed, monitor unplugged) keeps its last frame on screen and is
            // retried every two seconds.
            if let Some(l) = self.layers.get_mut(i)
                && let Runtime::Capture(c) = &l.rt
                && let SourceState::Lost(msg) = c.state()
            {
                l.rt = Runtime::Waiting { error: msg, retry_at: t + Hns::from_secs(2) };
            }
            let retry = matches!(self.layers.get(i).map(|l| &l.rt), Some(Runtime::Waiting { retry_at, .. }) if t >= *retry_at);
            if retry && let Some(src) = self.layers.get(i).map(|l| l.layer.source.clone()) {
                let rt = self.create_runtime(&src);
                if let Some(l) = self.layers.get_mut(i) {
                    l.rt = rt;
                }
            }
            if let Some(l) = self.layers.get(i) {
                let state = match &l.rt {
                    Runtime::Capture(c) => c.state(),
                    Runtime::Waiting { error, .. } => SourceState::Failed(error.clone()),
                    Runtime::Static(t) => SourceState::Active { width: t.width, height: t.height },
                    Runtime::Color(_) => SourceState::Active { width: self.canvas.width, height: self.canvas.height },
                };
                states.push((l.layer.id, state));
            }
        }
        *self.shared.sources.lock().unwrap_or_else(PoisonError::into_inner) = states;
    }

    fn tick(&mut self, pts: Hns) {
        let started = now();
        if self.window.ticks == 0 && self.window.start.0 == 0 {
            self.window.start = started;
        }
        self.ticks += 1;
        // Source upkeep twice a second.
        if self.ticks % (self.fps.as_f64() / 2.0).max(1.0) as u64 == 1 {
            self.maintain_sources(started);
        }
        let mut changed = self.scene_dirty;
        for l in &mut self.layers {
            if let Runtime::Capture(c) = &l.rt
                && let Some(f) = c.frame()
                && f.seq != l.last_seq
            {
                l.last_seq = f.seq;
                changed = true;
            }
        }
        self.scene_dirty = false;
        let encode = self.encoder.is_some();
        let encode_now = encode && (!self.vfr || changed || self.keyframe_pending || pts - self.last_encoded >= Hns::from_secs(1));
        if encode && !encode_now {
            self.stats.skipped_static += 1;
        }
        let preview_now = self.preview.cfg.enabled && self.preview_due(changed);
        if encode_now || preview_now || changed {
            if let Some(t) = self.gpu_timer.as_ref() {
                t.begin(&self.gpu);
            }
            if let Err(e) = self.compose() {
                log::warn!("composition failed: {e}");
            }
            if encode_now {
                self.encode(pts);
            }
            if preview_now && let Err(e) = self.update_preview() {
                log::debug!("preview: {e}");
            }
            if let Some(t) = self.gpu_timer.as_mut() {
                t.end(&self.gpu);
            }
            // SAFETY: plain flush of the immediate context (starts the GPU on this frame's work).
            unsafe { self.gpu.context.Flush() };
            self.stats.frames_rendered += 1;
        }
        let cpu = now() - started;
        self.window.ticks += 1;
        self.window.cpu_sum += cpu;
        self.window.cpu_max = self.window.cpu_max.max(cpu);
        if started - self.window.start >= Hns::from_millis(250) {
            self.publish_window(started);
        }
    }

    /// The preview refreshes at its own rate, and only while something changes (plus the few
    /// refreshes needed to push the last change through the readback ring).
    fn preview_due(&mut self, changed: bool) -> bool {
        if changed {
            self.preview.flush = Preview::FLUSH;
        }
        let every = (self.fps.as_f64() / f64::from(self.preview.cfg.fps.clamp(1, 240))).round().max(1.0) as u64;
        if !self.ticks.is_multiple_of(every) || self.preview.flush == 0 {
            return false;
        }
        self.preview.flush -= 1;
        true
    }

    fn compose(&mut self) -> crate::Result<()> {
        let (cw, ch) = (self.canvas.width, self.canvas.height);
        self.comp.begin(&self.gpu, &self.canvas.rtv, cw, ch, Some([0.0, 0.0, 0.0, 1.0]));
        for l in &self.layers {
            if !l.layer.visible || l.layer.opacity <= 0.0 {
                continue;
            }
            match &l.rt {
                Runtime::Color(c) => {
                    let r = l.layer.rect;
                    self.comp.draw_color(&self.gpu, [r.x, r.y, r.x + r.w, r.y + r.h], [c[0], c[1], c[2], c[3] * l.layer.opacity], cw, ch)?;
                }
                Runtime::Static(t) => {
                    if let Some(p) = place(&l.layer, t.width, t.height) {
                        let lod = lod_for(p.scale, t.mips);
                        let q = Quad { srv: &t.srv, dst: p.dst, src: p.src, opacity: l.layer.opacity, opaque: false, lod, filter: self.filter_for(p.scale) };
                        self.comp.draw_quad(&self.gpu, &q, cw, ch)?;
                    }
                }
                Runtime::Capture(c) => {
                    let Some(f) = c.frame() else { continue };
                    let Some(p) = place(&l.layer, f.width, f.height) else { continue };
                    c.set_want_mips(p.scale >= 1.9);
                    let lod = lod_for(p.scale, f.mip_levels);
                    let q = Quad { srv: &f.view, dst: p.dst, src: p.src, opacity: l.layer.opacity, opaque: true, lod, filter: self.filter_for(p.scale) };
                    self.comp.draw_quad(&self.gpu, &q, cw, ch)?;
                    if let Some((cursor, cx, cy, cwid, chei)) = &f.cursor {
                        // Desktop Duplication: draw the pointer ourselves, mapped like the source.
                        let sx = (p.dst[2] - p.dst[0]) / (p.src_px[2] - p.src_px[0]).max(1.0);
                        let sy = (p.dst[3] - p.dst[1]) / (p.src_px[3] - p.src_px[1]).max(1.0);
                        let x0 = p.dst[0] + (*cx as f32 - p.src_px[0]) * sx;
                        let y0 = p.dst[1] + (*cy as f32 - p.src_px[1]) * sy;
                        let dst = [x0, y0, x0 + *cwid as f32 * sx, y0 + *chei as f32 * sy];
                        let q = Quad {
                            srv: cursor,
                            dst,
                            src: [0.0, 0.0, 1.0, 1.0],
                            opacity: l.layer.opacity,
                            opaque: false,
                            lod: 0.0,
                            filter: ScaleFilter::Bilinear,
                        };
                        self.comp.draw_quad(&self.gpu, &q, cw, ch)?;
                    }
                }
                Runtime::Waiting { .. } => {}
            }
        }
        Ok(())
    }

    /// 1:1 and mild scaling look identical with bilinear; save the GPU.
    fn filter_for(&self, scale: f32) -> ScaleFilter {
        if (scale - 1.0).abs() < 0.01 { ScaleFilter::Bilinear } else { self.filter }
    }

    fn encode(&mut self, pts: Hns) {
        let Some(enc) = self.encoder.as_ref() else { return };
        if let Some(e) = enc.error() {
            self.stats.encoder_error = Some(e);
            return;
        }
        let Some(slot) = enc.try_slot() else {
            self.stats.dropped_encoder_backlog += 1;
            // The next frame that does get in should be a keyframe only if many were lost.
            return;
        };
        let Some(target) = enc.slots.get(slot) else { return };
        if let Err(e) = self.comp.convert_nv12(&self.gpu, &self.canvas.srv, target, enc.full_range) {
            log::warn!("colour conversion failed: {e}");
        }
        if enc.submit(slot, pts, self.keyframe_pending) {
            self.keyframe_pending = false;
            self.last_encoded = pts;
            self.stats.frames_submitted += 1;
        } else {
            self.stats.dropped_encoder_backlog += 1;
        }
    }

    fn preview_size(&self) -> (u32, u32) {
        let (w, h) = (self.canvas.width as f32, self.canvas.height as f32);
        let max = self.preview.cfg.max_size.clamp(64, 4096) as f32;
        let s = (max / w.max(h)).min(1.0);
        (((w * s) as u32).max(2) & !1, ((h * s) as u32).max(2) & !1)
    }

    fn update_preview(&mut self) -> crate::Result<()> {
        let (pw, ph) = self.preview_size();
        if self.preview.target.as_ref().is_none_or(|t| t.2 != pw || t.3 != ph) {
            let tex = self.gpu.render_texture(pw, ph, DXGI_FORMAT_R8G8B8A8_UNORM)?;
            let view = rtv(&self.gpu, &tex, None)?;
            self.preview.target = Some((tex, view, pw, ph));
            self.preview.staging = (0..3).map(|_| self.gpu.staging_texture(pw, ph, DXGI_FORMAT_R8G8B8A8_UNORM)).collect::<crate::Result<_>>()?;
            self.preview.written = 0;
            self.preview.published = 0;
        }
        let Some((tex, view, pw, ph)) = self.preview.target.as_ref() else { return Ok(()) };
        let (pw, ph) = (*pw, *ph);
        self.comp.begin(&self.gpu, view, pw, ph, Some([0.0, 0.0, 0.0, 1.0]));
        let scale = self.canvas.width as f32 / pw as f32;
        let q = Quad {
            srv: &self.canvas.srv,
            dst: [0.0, 0.0, pw as f32, ph as f32],
            src: [0.0, 0.0, 1.0, 1.0],
            opacity: 1.0,
            opaque: true,
            lod: 0.0,
            filter: match (self.preview.cfg.high_quality, scale > 1.5) {
                (true, true) => ScaleFilter::Lanczos,
                (_, true) => ScaleFilter::Bicubic,
                _ => ScaleFilter::Bilinear,
            },
        };
        self.comp.draw_quad(&self.gpu, &q, pw, ph)?;
        let n = self.preview.staging.len() as u64;
        if n == 0 {
            return Ok(());
        }
        if let Some(st) = self.preview.staging.get((self.preview.written % n) as usize) {
            // SAFETY: same size and format (RGBA8 pw×ph).
            unsafe { self.gpu.context.CopyResource(st, tex) };
        }
        self.preview.written += 1;
        // Read the oldest copy still pending, if the GPU has finished it.
        if self.preview.written >= n {
            let read = self.preview.written - n + 1;
            if read > self.preview.published
                && let Some(st) = self.preview.staging.get((read % n) as usize)
            {
                match read_staging(&self.gpu, st, pw, ph, true)? {
                    Some(frame) => {
                        self.preview.published = read;
                        self.preview.seq += 1;
                        self.window.preview_frames += 1;
                        let frame = PreviewFrame { width: pw, height: ph, rgba: frame, seq: self.preview.seq };
                        *self.shared.preview.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(frame));
                    }
                    // The GPU has not finished that copy yet: owe one more refresh so the last
                    // change still reaches the screen when nothing else moves.
                    None => self.preview.flush = (self.preview.flush + 1).min(Preview::FLUSH),
                }
            }
        }
        Ok(())
    }

    fn screenshot(&mut self) -> crate::Result<RgbaImage> {
        self.compose()?;
        let (w, h) = (self.canvas.width, self.canvas.height);
        let st = self.gpu.staging_texture(w, h, DXGI_FORMAT_B8G8R8A8_UNORM)?;
        // SAFETY: same size and format.
        unsafe { self.gpu.context.CopyResource(&st, &self.canvas.texture) };
        let mut px = read_staging(&self.gpu, &st, w, h, false)?.ok_or("screenshot not ready")?;
        for p in px.as_chunks_mut::<4>().0 {
            p.swap(0, 2);
            p[3] = 255;
        }
        RgbaImage::new(w, h, px).ok_or_else(|| PlatformError("bad screenshot size".into()))
    }

    fn publish_window(&mut self, t: Hns) {
        let w = std::mem::take(&mut self.window);
        let secs = (t - w.start).as_secs_f64().max(1e-3);
        self.stats.render_ms_avg = if w.ticks > 0 { (w.cpu_sum.as_secs_f64() * 1000.0 / f64::from(w.ticks)) as f32 } else { 0.0 };
        self.stats.render_ms_max = (w.cpu_max.as_secs_f64() * 1000.0) as f32;
        self.stats.preview_fps = (f64::from(w.preview_frames) / secs) as f32;
        if let Some(gt) = self.gpu_timer.as_mut()
            && let Some(avg) = gt.take_avg()
        {
            self.stats.gpu_ms_avg = Some(avg);
        }
        self.stats.capture_fps = self
            .layers
            .iter_mut()
            .filter_map(|l| match &l.rt {
                Runtime::Capture(c) => {
                    let d = c.delivered();
                    let fps = d.saturating_sub(l.delivered_mark) as f64 / secs;
                    l.delivered_mark = d;
                    Some((l.layer.id, fps as f32))
                }
                _ => None,
            })
            .collect();
        let mut bytes_mark = w.bytes_mark;
        if let Some(e) = &self.encoder {
            let s = &e.shared;
            let encoded = s.encoded.load(Ordering::Relaxed);
            let bytes = s.bytes.load(Ordering::Relaxed);
            self.stats.frames_encoded = encoded;
            self.stats.bitrate_kbps = (bytes.saturating_sub(w.bytes_mark) as f64 * 8.0 / 1000.0 / secs) as f32;
            bytes_mark = bytes;
            self.stats.encoder_in_flight = s.in_flight.load(Ordering::Relaxed);
            let n = s.latency_n.swap(0, Ordering::Relaxed);
            let sum = s.latency_us_sum.swap(0, Ordering::Relaxed);
            if n > 0 {
                self.stats.encode_latency_ms_avg = (sum as f64 / n as f64 / 1000.0) as f32;
            }
            self.stats.encoder_name = Some(e.info.name.clone());
            self.stats.encoder_hardware = e.info.hardware;
            if let Some(err) = e.error() {
                self.stats.encoder_error = Some(err);
            }
        } else {
            self.stats.encoder_name = None;
            self.stats.bitrate_kbps = 0.0;
            self.stats.encoder_in_flight = 0;
        }
        self.window.start = t;
        self.window.bytes_mark = bytes_mark;
        self.publish_stats();
    }

    fn publish_stats(&mut self) {
        self.stats.canvas_width = self.canvas.width;
        self.stats.canvas_height = self.canvas.height;
        self.stats.fps_target = self.fps.as_f64();
        self.stats.preview_enabled = self.preview.cfg.enabled;
        *self.shared.stats.lock().unwrap_or_else(PoisonError::into_inner) = self.stats.clone();
    }
}

fn same_source(a: &LayerSource, b: &LayerSource) -> bool {
    match (a, b) {
        (LayerSource::Capture { target: ta, options: oa }, LayerSource::Capture { target: tb, options: ob }) => ta == tb && oa == ob,
        (LayerSource::Image(a), LayerSource::Image(b)) => Arc::ptr_eq(&a.data, &b.data) || a == b,
        (LayerSource::Color(_), LayerSource::Color(_)) => false, // cheap to rebuild, picks up the new colour
        (LayerSource::Webcam { device: da, width: wa, height: ha, fps: fa }, LayerSource::Webcam { device: db, width: wb, height: hb, fps: fb }) => {
            da == db && wa == wb && ha == hb && fa == fb
        }
        _ => false,
    }
}

fn start_capture(gpu: &Gpu, target: &CaptureTarget, options: &CaptureOptions) -> crate::Result<Box<dyn CaptureSource>> {
    let monitor = matches!(target, CaptureTarget::Monitor { .. });
    match (options.method, monitor) {
        (CaptureMethod::Duplication, true) => Ok(Box::new(dup::DupSource::start(gpu, target, options)?)),
        (CaptureMethod::Auto, true) if !wgc::supported() => Ok(Box::new(dup::DupSource::start(gpu, target, options)?)),
        _ => Ok(Box::new(wgc::WgcSource::start(gpu, target, options)?)),
    }
}

fn static_texture(gpu: &Gpu, img: &RgbaImage) -> crate::Result<StaticTexture> {
    let (w, h) = (img.width, img.height);
    let expected = (w as usize).checked_mul(h as usize).and_then(|p| p.checked_mul(4)).ok_or("image too large")?;
    if img.data.len() != expected {
        return Err(PlatformError("image data does not match its size".into()));
    }
    // A full mip chain so a large image shrunk into a small layer stays smooth.
    let tex = gpu.texture(
        w,
        h,
        DXGI_FORMAT_R8G8B8A8_UNORM,
        D3D11_BIND_SHADER_RESOURCE | windows::Win32::Graphics::Direct3D11::D3D11_BIND_RENDER_TARGET,
        D3D11_RESOURCE_MISC_GENERATE_MIPS,
        0,
    )?;
    let view = srv(gpu, &tex)?;
    let mut desc = Default::default();
    // SAFETY: plain getter; then the pixels of mip 0 are uploaded from a buffer of exactly
    // `w * h * 4` bytes (checked above) with row pitch `w * 4`.
    unsafe {
        tex.GetDesc(&mut desc);
        gpu.context.UpdateSubresource(&tex, 0, None, img.data.as_ptr().cast(), w * 4, 0);
        gpu.context.GenerateMips(&view);
    }
    Ok(StaticTexture { srv: view, width: w, height: h, mips: desc.MipLevels.max(1) })
}

/// Copies a mapped staging texture into a tight `w*h*4` buffer. With `no_wait`, `Ok(None)` while the
/// GPU is still busy with it.
fn read_staging(gpu: &Gpu, st: &ID3D11Texture2D, w: u32, h: u32, no_wait: bool) -> crate::Result<Option<Vec<u8>>> {
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    let flags = if no_wait { D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32 } else { 0 };
    // SAFETY: maps a staging texture for reading; unmapped below on every path.
    if unsafe { gpu.context.Map(st, 0, D3D11_MAP_READ, flags, Some(&mut mapped)) }.is_err() {
        return if no_wait { Ok(None) } else { Err(PlatformError("cannot read back the frame".into())) };
    }
    let row = w as usize * 4;
    let pitch = mapped.RowPitch as usize;
    let mut out = vec![0u8; row * h as usize];
    if !mapped.pData.is_null() && pitch >= row {
        for y in 0..h as usize {
            // SAFETY: row `y` of the mapping starts at y*pitch and has at least `row` bytes
            // (pitch ≥ row); the destination slice has exactly `row` bytes.
            unsafe {
                let src = mapped.pData.cast::<u8>().add(y * pitch);
                if let Some(dst) = out.get_mut(y * row..(y + 1) * row) {
                    std::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), row);
                }
            }
        }
    }
    // SAFETY: balances the Map above.
    unsafe { gpu.context.Unmap(st, 0) };
    Ok(Some(out))
}

struct Placement {
    /// Destination in canvas pixels.
    dst: [f32; 4],
    /// Source UV.
    src: [f32; 4],
    /// Source rectangle in pixels (after crop / cover).
    src_px: [f32; 4],
    /// Source pixels per destination pixel (≥ 1 when shrinking).
    scale: f32,
}

/// Where a `sw`×`sh` source lands for a layer (crop, then fit into the rectangle).
fn place(layer: &Layer, sw: u32, sh: u32) -> Option<Placement> {
    let Crop { left, top, right, bottom } = layer.crop;
    let (sw_f, sh_f) = (sw as f32, sh as f32);
    let (mut x0, mut y0) = (left.min(sw) as f32, top.min(sh) as f32);
    let (mut x1, mut y1) = (sw.saturating_sub(right) as f32, sh.saturating_sub(bottom) as f32);
    if x1 - x0 < 1.0 || y1 - y0 < 1.0 {
        return None;
    }
    let r = layer.rect;
    if !(r.w.is_finite() && r.h.is_finite() && r.x.is_finite() && r.y.is_finite()) || r.w < 1.0 || r.h < 1.0 {
        return None;
    }
    let (cw, ch) = (x1 - x0, y1 - y0);
    let mut dst = [r.x, r.y, r.x + r.w, r.y + r.h];
    match layer.fit {
        Fit::Stretch => {}
        Fit::Contain => {
            let s = (r.w / cw).min(r.h / ch);
            let (dw, dh) = (cw * s, ch * s);
            let (ox, oy) = ((r.w - dw) / 2.0, (r.h - dh) / 2.0);
            dst = [r.x + ox, r.y + oy, r.x + ox + dw, r.y + oy + dh];
        }
        Fit::Cover => {
            let s = (r.w / cw).max(r.h / ch);
            let (vw, vh) = (r.w / s, r.h / s);
            x0 += (cw - vw) / 2.0;
            y0 += (ch - vh) / 2.0;
            x1 = x0 + vw;
            y1 = y0 + vh;
        }
        Fit::Original => {
            let (dw, dh) = (cw.min(r.w), ch.min(r.h));
            let (ox, oy) = ((r.w - dw) / 2.0, (r.h - dh) / 2.0);
            dst = [r.x + ox, r.y + oy, r.x + ox + dw, r.y + oy + dh];
            x0 += (cw - dw) / 2.0;
            y0 += (ch - dh) / 2.0;
            x1 = x0 + dw;
            y1 = y0 + dh;
        }
    }
    let scale = ((x1 - x0) / (dst[2] - dst[0]).max(1e-3)).max((y1 - y0) / (dst[3] - dst[1]).max(1e-3));
    Some(Placement { dst, src: [x0 / sw_f, y0 / sh_f, x1 / sw_f, y1 / sh_f], src_px: [x0, y0, x1, y1], scale })
}

/// Mip level for a downscale factor: whole octaves come from the mip chain, the rest from the filter.
fn lod_for(scale: f32, mips: u32) -> f32 {
    if mips <= 1 || scale < 2.0 || !scale.is_finite() {
        return 0.0;
    }
    scale.log2().floor().min((mips - 1) as f32).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RectF;

    fn layer(fit: Fit, crop: Crop, rect: RectF) -> Layer {
        Layer { id: 1, source: LayerSource::Color([0.0; 4]), rect, fit, crop, opacity: 1.0, visible: true }
    }

    #[test]
    fn placement() {
        let full = RectF { x: 0.0, y: 0.0, w: 1920.0, h: 1080.0 };
        let p = place(&layer(Fit::Contain, Crop::default(), full), 3840, 2160).unwrap();
        assert_eq!(p.dst, [0.0, 0.0, 1920.0, 1080.0]);
        assert!((p.scale - 2.0).abs() < 1e-4);
        assert_eq!(lod_for(p.scale, 10), 1.0);
        // 4:3 into 16:9: pillarboxed.
        let p = place(&layer(Fit::Contain, Crop::default(), full), 1024, 768).unwrap();
        assert_eq!(p.dst[1], 0.0);
        assert!((p.dst[0] - 240.0).abs() < 1e-3);
        // Region capture = crop.
        let crop = Crop { left: 100, top: 50, right: 820, bottom: 490 };
        let p = place(&layer(Fit::Stretch, crop, full), 1920, 1080).unwrap();
        assert_eq!(p.src_px, [100.0, 50.0, 1100.0, 590.0]);
        // Hostile values never panic and yield nothing.
        assert!(place(&layer(Fit::Cover, Crop { left: 5000, ..Crop::default() }, full), 1920, 1080).is_none());
        assert!(place(&layer(Fit::Cover, Crop::default(), RectF { w: f32::NAN, ..full }), 1920, 1080).is_none());
        assert_eq!(lod_for(f32::INFINITY, 4), 0.0);
    }

    /// Waits for a preview frame of the given size (the screen is static: only the engine's own
    /// flush after a change can deliver it).
    fn wait_preview(engine: &VideoEngine, size: (u32, u32)) -> Option<Arc<PreviewFrame>> {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if let Some(f) = engine.preview()
                && (f.width, f.height) == size
            {
                return Some(f);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }

    /// Regression: after the preview textures were recreated (preview resolution or canvas size
    /// changed) the readback ring restarted while `published` did not, so the preview froze on the
    /// old image.
    #[test]
    fn preview_follows_size_changes_on_a_static_scene() {
        let preview = PreviewConfig { enabled: true, max_size: 320, fps: 30, high_quality: false };
        let cfg = VideoEngineConfig {
            gpu: None,
            width: 640,
            height: 360,
            fps: FrameRate::new(30, 1).unwrap(),
            scale_filter: ScaleFilter::Bilinear,
            preview,
            variable_frame_rate: false,
        };
        let engine = VideoEngine::new(cfg).unwrap();
        let full = RectF { x: 0.0, y: 0.0, w: 1.0, h: 1.0 };
        engine.set_scene(vec![Layer { source: LayerSource::Color([1.0, 0.0, 0.0, 1.0]), ..layer(Fit::Stretch, Crop::default(), full) }]);
        assert!(wait_preview(&engine, (320, 180)).is_some(), "first preview frame");
        // Preview resolution changed in the settings.
        engine.set_preview(PreviewConfig { max_size: 160, ..preview });
        let f = wait_preview(&engine, (160, 90)).expect("preview at the new resolution");
        assert_eq!(f.rgba.len(), 160 * 90 * 4);
        // Back again (the old textures' sequence numbers must not hold the new ones back).
        engine.set_preview(preview);
        assert!(wait_preview(&engine, (320, 180)).is_some(), "preview back at the first resolution");
        // The canvas changes size (e.g. a display source with another size was removed or added).
        engine.set_canvas(480, 360, FrameRate::new(30, 1).unwrap(), ScaleFilter::Bilinear, false).unwrap();
        assert!(wait_preview(&engine, (320, 240)).is_some(), "preview after a canvas size change");
    }
}
