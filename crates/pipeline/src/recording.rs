//! A file recording: Matroska written on its own thread.
//!
//! - Starts at a video keyframe; the timeline is rebased to 0 there.
//! - Pause drops packets; resume waits for a keyframe and closes the gap, so the file has no hole.
//! - Splits at keyframes by duration or size; each part starts at 0.
//! - Checks free space every two seconds and finalizes the file before the disk fills.
//! - Flushes the open cluster every second: a hard crash loses at most about one second.
//! - Keeps a session journal so an interrupted recording is offered for recovery at next start.
//! - Optionally remuxes to MP4 when done (the MKV is deleted only after the MP4 is complete).

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded};
use fluxcapture_mux::mkv::{MkvOptions, MkvWriter};
use fluxcapture_time::Hns;

use crate::journal::Journal;
use crate::{Packet, StreamKind, StreamSetup, offer};

/// Packets the writer may lag behind (~30 s of 60 fps video plus audio).
const QUEUE: usize = 8192;

/// How recordings are split.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Split {
    Never,
    Every(Hns),
    Bytes(u64),
}

/// Free-space probe (injected so this crate stays testable).
pub type DiskFree = fn(&Path) -> Option<u64>;

#[derive(Clone, Debug)]
pub struct RecordConfig {
    pub folder: PathBuf,
    /// File name without extension (already expanded and sanitized).
    pub base_name: String,
    pub split: Split,
    pub min_free_bytes: u64,
    pub warn_free_bytes: u64,
    pub remux_mp4: bool,
    pub delete_mkv_after_remux: bool,
    pub journal_dir: Option<PathBuf>,
    pub disk_free: Option<DiskFree>,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RecState {
    /// Waiting for the first keyframe.
    Starting,
    Recording,
    Paused,
    Finishing,
    Remuxing,
    Done,
    Failed(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum StopReason {
    User,
    LowDisk,
    Error(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct RecordingSummary {
    pub files: Vec<PathBuf>,
    pub duration: Hns,
    pub bytes: u64,
    pub reason: StopReason,
    pub dropped_packets: u64,
    pub errors: Vec<String>,
}

/// A snapshot for the UI.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordingStatus {
    pub state: RecState,
    pub duration: Hns,
    pub bytes: u64,
    pub current_file: Option<PathBuf>,
    pub parts: usize,
    pub disk_free: Option<u64>,
    pub warning: Option<String>,
    /// Achieved write throughput over the last window, bytes/s.
    pub write_rate: f64,
    pub write_ms_max: f32,
    pub backlog: usize,
}

enum Msg {
    Packet(Packet),
    Config(Vec<u8>),
    Pause(bool),
    Stop,
}

struct Shared {
    state: Mutex<RecState>,
    duration: AtomicI64,
    bytes: AtomicU64,
    files: Mutex<Vec<PathBuf>>,
    disk_free: Mutex<Option<u64>>,
    warning: Mutex<Option<String>>,
    write_rate: Mutex<(f64, f32)>,
    dropped: AtomicU64,
    summary: Mutex<Option<RecordingSummary>>,
    stopping: AtomicBool,
}

/// Handle to a recording (cheap to clone).
#[derive(Clone)]
pub struct Recording {
    tx: Sender<Msg>,
    shared: Arc<Shared>,
    thread: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl Recording {
    pub(crate) fn start(cfg: RecordConfig, setup: StreamSetup, config: Option<Vec<u8>>) -> Result<Recording, String> {
        std::fs::create_dir_all(&cfg.folder).map_err(|e| format!("cannot create the folder {}: {e}", cfg.folder.display()))?;
        if let Some(df) = cfg.disk_free
            && let Some(free) = df(&cfg.folder)
            && free < cfg.min_free_bytes
        {
            return Err(format!("not enough free space in {} ({} MB)", cfg.folder.display(), free / 1_000_000));
        }
        let (tx, rx) = bounded(QUEUE);
        let shared = Arc::new(Shared {
            state: Mutex::new(RecState::Starting),
            duration: AtomicI64::new(0),
            bytes: AtomicU64::new(0),
            files: Mutex::new(Vec::new()),
            disk_free: Mutex::new(None),
            warning: Mutex::new(None),
            write_rate: Mutex::new((0.0, 0.0)),
            dropped: AtomicU64::new(0),
            summary: Mutex::new(None),
            stopping: AtomicBool::new(false),
        });
        let shared2 = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("fluxcapture-writer".into())
            .spawn(move || {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| writer(cfg, setup, config, &rx, &shared2)));
                let summary = match r {
                    Ok(s) => s,
                    Err(_) => {
                        log::error!("the recording writer panicked");
                        RecordingSummary {
                            files: shared2.files.lock().unwrap_or_else(PoisonError::into_inner).clone(),
                            duration: Hns(shared2.duration.load(Ordering::Relaxed)),
                            bytes: shared2.bytes.load(Ordering::Relaxed),
                            reason: StopReason::Error("internal error in the writer".into()),
                            dropped_packets: 0,
                            errors: vec!["internal error in the writer".into()],
                        }
                    }
                };
                let failed = matches!(summary.reason, StopReason::Error(_));
                *shared2.state.lock().unwrap_or_else(PoisonError::into_inner) =
                    if failed { RecState::Failed(format!("{:?}", summary.reason)) } else { RecState::Done };
                *shared2.summary.lock().unwrap_or_else(PoisonError::into_inner) = Some(summary);
            })
            .map_err(|e| format!("cannot start the writer: {e}"))?;
        Ok(Recording { tx, shared, thread: Arc::new(Mutex::new(Some(thread))) })
    }

    /// Queues a packet; `false` if the queue is full (counted as a loss).
    pub(crate) fn offer(&self, p: Packet) -> bool {
        if self.shared.stopping.load(Ordering::Relaxed) {
            return true;
        }
        let ok = offer(&self.tx, Msg::Packet(p));
        if !ok {
            self.shared.dropped.fetch_add(1, Ordering::Relaxed);
        }
        ok
    }

    pub(crate) fn set_config(&self, c: Vec<u8>) {
        let _ = self.tx.send_timeout(Msg::Config(c), Duration::from_secs(1));
    }

    pub(crate) fn set_paused(&self, p: bool) {
        let _ = self.tx.send_timeout(Msg::Pause(p), Duration::from_secs(1));
    }

    pub(crate) fn stop(&self) {
        self.shared.stopping.store(true, Ordering::Relaxed);
        let _ = self.tx.send_timeout(Msg::Stop, Duration::from_secs(5));
    }

    /// Waits for the writer to finish (including the remux) up to `timeout`.
    pub fn wait(&self, timeout: Duration) -> Option<RecordingSummary> {
        let t0 = Instant::now();
        while t0.elapsed() < timeout {
            if let Some(s) = self.summary() {
                if let Some(t) = self.thread.lock().unwrap_or_else(PoisonError::into_inner).take() {
                    let _ = t.join();
                }
                return Some(s);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }

    /// The final summary, once the writer is done.
    pub fn summary(&self) -> Option<RecordingSummary> {
        self.shared.summary.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    pub fn backlog(&self) -> usize {
        self.tx.len()
    }

    pub fn status(&self) -> RecordingStatus {
        let s = &self.shared;
        let files = s.files.lock().unwrap_or_else(PoisonError::into_inner);
        let (rate, wmax) = *s.write_rate.lock().unwrap_or_else(PoisonError::into_inner);
        RecordingStatus {
            state: s.state.lock().unwrap_or_else(PoisonError::into_inner).clone(),
            duration: Hns(s.duration.load(Ordering::Relaxed)),
            bytes: s.bytes.load(Ordering::Relaxed),
            current_file: files.last().cloned(),
            parts: files.len(),
            disk_free: *s.disk_free.lock().unwrap_or_else(PoisonError::into_inner),
            warning: s.warning.lock().unwrap_or_else(PoisonError::into_inner).clone(),
            write_rate: rate,
            write_ms_max: wmax,
            backlog: self.tx.len(),
        }
    }
}

struct Segment {
    mkv: MkvWriter<BufWriter<File>>,
    path: PathBuf,
    /// Recording-timeline time at which this part starts.
    start: Hns,
    bytes_base: u64,
}

/// `folder/name.ext`, or `name (2).ext`… when taken.
fn unique_path(folder: &Path, name: &str, ext: &str) -> PathBuf {
    let first = folder.join(format!("{name}.{ext}"));
    if !first.exists() {
        return first;
    }
    for n in 2..10_000 {
        let p = folder.join(format!("{name} ({n}).{ext}"));
        if !p.exists() {
            return p;
        }
    }
    folder.join(format!("{name} ({}).{ext}", std::process::id()))
}

fn open_segment(cfg: &RecordConfig, setup: &StreamSetup, config: &[u8], part: usize, start: Hns, bytes_base: u64) -> Result<Segment, String> {
    let name = if part <= 1 { cfg.base_name.clone() } else { format!("{} - parte {part}", cfg.base_name) };
    let path = unique_path(&cfg.folder, &name, "mkv");
    let file = File::create(&path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    let opts = MkvOptions { title: cfg.title.clone(), writing_app: format!("FluxCapture {}", env!("CARGO_PKG_VERSION")), ..MkvOptions::default() };
    let mkv =
        MkvWriter::new(BufWriter::with_capacity(1 << 20, file), &setup.tracks(config), opts).map_err(|e| format!("cannot start {}: {e}", path.display()))?;
    log::info!("recording to {}", path.display());
    Ok(Segment { mkv, path, start, bytes_base })
}

fn writer(cfg: RecordConfig, setup: StreamSetup, mut config: Option<Vec<u8>>, rx: &Receiver<Msg>, shared: &Shared) -> RecordingSummary {
    let mut seg: Option<Segment> = None;
    let mut files: Vec<PathBuf> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut journal: Option<Journal> = None;
    let mut origin: Option<Hns> = None;
    let mut paused = false;
    let mut resume_wait = false;
    let mut paused_total = Hns(0);
    let mut last_video: Hns = Hns(0);
    let mut duration = Hns(0);
    let mut reason = StopReason::User;
    let frame = setup.frame_duration();
    let mut last_tick = Instant::now();
    let mut window_bytes = 0u64;
    let mut window_ms_max = 0f32;
    let mut window_start = Instant::now();
    let mut part = 0usize;

    let set_state = |s: RecState| *shared.state.lock().unwrap_or_else(PoisonError::into_inner) = s;
    let finish = |seg: Segment, errors: &mut Vec<String>| {
        if let Err(e) = seg.mkv.finish() {
            errors.push(format!("{}: {e}", seg.path.display()));
            log::error!("finishing {}: {e}", seg.path.display());
        }
    };

    'outer: loop {
        let msg = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(m) => Some(m),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        match msg {
            Some(Msg::Stop) => break,
            Some(Msg::Config(c)) => {
                if config.is_none() {
                    config = Some(c);
                }
            }
            Some(Msg::Pause(p)) => {
                if p && !paused {
                    paused = true;
                    set_state(RecState::Paused);
                } else if !p && paused {
                    paused = false;
                    resume_wait = true;
                    set_state(RecState::Recording);
                }
            }
            Some(Msg::Packet(p)) => {
                if paused {
                    continue;
                }
                let t: Hns;
                if p.kind == StreamKind::Video {
                    if seg.is_none() {
                        let Some(cfgdata) = config.as_ref() else { continue };
                        if !p.keyframe {
                            continue;
                        }
                        origin = Some(p.pts);
                        part = 1;
                        match open_segment(&cfg, &setup, cfgdata, part, Hns(0), 0) {
                            Ok(s) => {
                                files.push(s.path.clone());
                                *shared.files.lock().unwrap_or_else(PoisonError::into_inner) = files.clone();
                                journal = cfg.journal_dir.as_ref().and_then(|d| Journal::create(d, &files).map_err(|e| log::warn!("journal: {e}")).ok());
                                seg = Some(s);
                                set_state(RecState::Recording);
                            }
                            Err(e) => {
                                reason = StopReason::Error(e.clone());
                                errors.push(e);
                                break 'outer;
                            }
                        }
                    }
                    if resume_wait {
                        if !p.keyframe {
                            continue;
                        }
                        // Close the gap: the resumed frame follows the last frame before the pause.
                        let base = origin.unwrap_or(p.pts);
                        let expected = last_video.saturating_add(frame);
                        let actual = p.pts - base - paused_total;
                        if actual > expected {
                            paused_total += actual - expected;
                        }
                        resume_wait = false;
                    }
                    t = p.pts - origin.unwrap_or(p.pts) - paused_total;
                    last_video = t;
                    duration = duration.max(t.saturating_add(frame));
                    // Split at keyframes.
                    if p.keyframe
                        && let Some(s) = seg.as_ref()
                    {
                        let bytes_in_part = s.mkv.bytes_written();
                        let due = match cfg.split {
                            Split::Never => false,
                            Split::Every(d) => d.0 > 0 && t - s.start >= d,
                            Split::Bytes(b) => b > 0 && bytes_in_part >= b,
                        };
                        if due && let Some(old) = seg.take() {
                            let base = old.bytes_base + old.mkv.bytes_written();
                            finish(old, &mut errors);
                            part += 1;
                            match open_segment(&cfg, &setup, config.as_deref().unwrap_or(&[]), part, t, base) {
                                Ok(s) => {
                                    files.push(s.path.clone());
                                    *shared.files.lock().unwrap_or_else(PoisonError::into_inner) = files.clone();
                                    if let Some(j) = journal.as_mut() {
                                        let _ = j.update(&files);
                                    }
                                    seg = Some(s);
                                }
                                Err(e) => {
                                    reason = StopReason::Error(e.clone());
                                    errors.push(e);
                                    break 'outer;
                                }
                            }
                        }
                    }
                } else {
                    let (Some(o), false) = (origin, resume_wait) else { continue };
                    if p.pts < o {
                        continue;
                    }
                    t = p.pts - o - paused_total;
                }
                let Some(s) = seg.as_mut() else { continue };
                if t < s.start {
                    continue;
                }
                let w0 = Instant::now();
                if let Err(e) = s.mkv.write(p.track_index(), t - s.start, p.keyframe, &p.data) {
                    let msg = format!("write failed ({}): {e}", s.path.display());
                    log::error!("{msg}");
                    reason = StopReason::Error(msg.clone());
                    errors.push(msg);
                    break 'outer;
                }
                window_ms_max = window_ms_max.max(w0.elapsed().as_secs_f32() * 1000.0);
                window_bytes += p.data.len() as u64;
                shared.duration.store(duration.0, Ordering::Relaxed);
                shared.bytes.store(s.bytes_base + s.mkv.bytes_written(), Ordering::Relaxed);
            }
            None => {}
        }
        // Periodic work: flush, disk space, throughput.
        if last_tick.elapsed() >= Duration::from_secs(1) {
            last_tick = Instant::now();
            if let Some(s) = seg.as_mut() {
                let w0 = Instant::now();
                if let Err(e) = s.mkv.flush() {
                    let msg = format!("flush failed ({}): {e}", s.path.display());
                    reason = StopReason::Error(msg.clone());
                    errors.push(msg);
                    break;
                }
                window_ms_max = window_ms_max.max(w0.elapsed().as_secs_f32() * 1000.0);
            }
            let secs = window_start.elapsed().as_secs_f64().max(1e-3);
            *shared.write_rate.lock().unwrap_or_else(PoisonError::into_inner) = (window_bytes as f64 / secs, window_ms_max);
            window_bytes = 0;
            window_ms_max = 0.0;
            window_start = Instant::now();
            if let Some(df) = cfg.disk_free
                && let Some(free) = df(&cfg.folder)
            {
                *shared.disk_free.lock().unwrap_or_else(PoisonError::into_inner) = Some(free);
                if free < cfg.min_free_bytes {
                    log::warn!("stopping: only {} MB free", free / 1_000_000);
                    *shared.warning.lock().unwrap_or_else(PoisonError::into_inner) =
                        Some(format!("Gravação finalizada: restam só {} MB no disco.", free / 1_000_000));
                    reason = StopReason::LowDisk;
                    break;
                }
                let warn = (free < cfg.warn_free_bytes).then(|| format!("Pouco espaço em disco: {} MB livres.", free / 1_000_000));
                *shared.warning.lock().unwrap_or_else(PoisonError::into_inner) = warn;
            }
        }
    }
    set_state(RecState::Finishing);
    if let Some(s) = seg.take() {
        finish(s, &mut errors);
    }
    let dropped = shared.dropped.load(Ordering::Relaxed);
    let mut out_files = files.clone();
    if cfg.remux_mp4 && !files.is_empty() {
        set_state(RecState::Remuxing);
        out_files.clear();
        for f in &files {
            let mp4 = f.with_extension("mp4");
            let mp4 = if mp4.exists() {
                unique_path(f.parent().unwrap_or(Path::new(".")), &f.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(), "mp4")
            } else {
                mp4
            };
            match fluxcapture_mux::remux::remux_mkv_to_mp4(f, &mp4) {
                Ok(_) => {
                    out_files.push(mp4);
                    if cfg.delete_mkv_after_remux {
                        if let Err(e) = std::fs::remove_file(f) {
                            log::warn!("cannot delete {}: {e}", f.display());
                            out_files.push(f.clone());
                        }
                    } else {
                        out_files.push(f.clone());
                    }
                }
                Err(e) => {
                    errors.push(format!("MP4: {e} (o MKV foi mantido)"));
                    out_files.push(f.clone());
                }
            }
        }
    }
    if let Some(j) = journal.take() {
        j.complete();
    }
    if dropped > 0 {
        errors.push(format!("{dropped} pacotes perdidos: o disco não acompanhou a gravação"));
    }
    RecordingSummary { files: out_files, duration, bytes: shared.bytes.load(Ordering::Relaxed), reason, dropped_packets: dropped, errors }
}
