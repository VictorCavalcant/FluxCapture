//! Packet routing and outputs.
//!
//! One encode feeds every output: the [`Router`] thread receives the encoder's video access units
//! (Annex B) and the mixer's AAC frames, converts video to the length-prefixed form containers use,
//! derives the codec configuration from the first keyframe, and fans each packet out to the file
//! [`recording`] and the [`replay`] buffer. Packets are reference-counted (`Arc<[u8]>`): outputs share
//! them without copying. Every queue is bounded; a full recording queue (disk too slow) is counted
//! and reported, never silent.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod journal;
pub mod recording;
pub mod replay;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, select};
use fluxcapture_mux::codec::{annexb_to_length_prefixed, config_from_length_prefixed};
use fluxcapture_mux::{AudioTrack, ColorInfo, TrackSpec, VideoCodec, VideoTrack};
use fluxcapture_platform::{EncodedAudio, EncodedVideo};
use fluxcapture_time::{FrameRate, Hns};

pub use recording::{RecState, RecordConfig, Recording, RecordingStatus, RecordingSummary, Split, StopReason};
pub use replay::{ReplayBuffer, ReplayConfig};

/// Which stream a packet belongs to.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum StreamKind {
    Video,
    /// Audio track index (0 = first audio track).
    Audio(u8),
}

/// An encoded packet ready for a container (video length-prefixed).
#[derive(Clone, Debug)]
pub struct Packet {
    pub kind: StreamKind,
    /// Media-clock time (QPC domain); outputs rebase it.
    pub pts: Hns,
    pub keyframe: bool,
    pub data: Arc<[u8]>,
}

impl Packet {
    /// Index of the packet's track in [`StreamSetup::tracks`].
    pub fn track_index(&self) -> usize {
        match self.kind {
            StreamKind::Video => 0,
            StreamKind::Audio(t) => 1 + usize::from(t),
        }
    }
}

/// What the streams are.
#[derive(Clone, Debug, PartialEq)]
pub struct StreamSetup {
    pub video_codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub fps: FrameRate,
    pub variable_frame_rate: bool,
    pub full_range: bool,
    pub audio: Vec<AudioTrack>,
}

impl StreamSetup {
    /// Container tracks: video first, then the audio tracks; `video_config` is the avcC/hvcC record.
    pub fn tracks(&self, video_config: &[u8]) -> Vec<TrackSpec> {
        let mut t = vec![TrackSpec::Video(VideoTrack {
            codec: self.video_codec,
            width: self.width,
            height: self.height,
            frame_rate: (!self.variable_frame_rate).then_some(self.fps),
            codec_private: video_config.to_vec(),
            color: ColorInfo { full_range: self.full_range },
            name: "Vídeo".into(),
        })];
        t.extend(self.audio.iter().cloned().map(TrackSpec::Audio));
        t
    }

    /// Nominal bitrate needs are unknown here; the engine provides them for disk estimates.
    pub fn frame_duration(&self) -> Hns {
        self.fps.frame_duration()
    }
}

/// Router counters.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RouterStats {
    pub video_packets: u64,
    pub audio_packets: u64,
    pub video_bytes: u64,
    pub audio_bytes: u64,
    pub conversion_errors: u64,
    /// Packets the recording could not take because its queue was full (disk too slow).
    pub recording_dropped: u64,
    pub recording_backlog: usize,
    pub replay_seconds: f32,
    pub replay_bytes: usize,
    pub replay_enabled: bool,
}

enum Ctl {
    StartRecording(Box<RecordConfig>, Sender<Result<Recording, String>>),
    StopRecording(Sender<Option<Recording>>),
    Pause(bool),
    Replay(Option<ReplayConfig>),
    SaveReplay(PathBuf, bool, Sender<Result<Receiver<Result<PathBuf, String>>, String>>),
    Stop,
}

#[derive(Default)]
struct Shared {
    video_packets: AtomicU64,
    audio_packets: AtomicU64,
    video_bytes: AtomicU64,
    audio_bytes: AtomicU64,
    conversion_errors: AtomicU64,
    recording_dropped: AtomicU64,
    replay: Mutex<(f32, usize, bool)>,
    recording: Mutex<Option<Recording>>,
}

/// The routing thread.
pub struct Router {
    ctl: Sender<Ctl>,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Router {
    pub fn start(setup: StreamSetup, video: Receiver<EncodedVideo>, audio: Receiver<(u8, EncodedAudio)>) -> Result<Router, String> {
        let (ctl, ctl_rx) = bounded(64);
        let shared = Arc::new(Shared::default());
        let shared2 = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("fluxcapture-router".into())
            .spawn(move || {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| route(setup, &video, &audio, &ctl_rx, &shared2)));
                if r.is_err() {
                    log::error!("the packet router panicked");
                }
            })
            .map_err(|e| format!("cannot start the router: {e}"))?;
        Ok(Router { ctl, shared, thread: Some(thread) })
    }

    fn call<T>(&self, make: impl FnOnce(Sender<T>) -> Ctl) -> Result<T, String> {
        let (tx, rx) = bounded(1);
        self.ctl.send(make(tx)).map_err(|_| "the router is not running".to_string())?;
        rx.recv_timeout(Duration::from_secs(10)).map_err(|_| "the router did not answer".to_string())
    }

    /// Starts a file recording; it begins at the next video keyframe (ask the encoder for one).
    pub fn start_recording(&self, cfg: RecordConfig) -> Result<Recording, String> {
        self.call(|tx| Ctl::StartRecording(Box::new(cfg), tx))?
    }

    /// Stops the recording; the returned handle reports finishing / remuxing progress.
    pub fn stop_recording(&self) -> Option<Recording> {
        self.call(Ctl::StopRecording).ok().flatten()
    }

    pub fn set_paused(&self, paused: bool) {
        let _ = self.ctl.send(Ctl::Pause(paused));
    }

    /// Enables (or with `None` disables and frees) the replay buffer.
    pub fn set_replay(&self, cfg: Option<ReplayConfig>) {
        let _ = self.ctl.send(Ctl::Replay(cfg));
    }

    /// Writes the replay buffer to `path` in the background; the receiver gets the result.
    pub fn save_replay(&self, path: PathBuf, mp4: bool) -> Result<Receiver<Result<PathBuf, String>>, String> {
        self.call(|tx| Ctl::SaveReplay(path, mp4, tx))?
    }

    /// The current recording, if any.
    pub fn recording(&self) -> Option<Recording> {
        self.shared.recording.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    pub fn stats(&self) -> RouterStats {
        let s = &self.shared;
        let (replay_seconds, replay_bytes, replay_enabled) = *s.replay.lock().unwrap_or_else(PoisonError::into_inner);
        RouterStats {
            video_packets: s.video_packets.load(Ordering::Relaxed),
            audio_packets: s.audio_packets.load(Ordering::Relaxed),
            video_bytes: s.video_bytes.load(Ordering::Relaxed),
            audio_bytes: s.audio_bytes.load(Ordering::Relaxed),
            conversion_errors: s.conversion_errors.load(Ordering::Relaxed),
            recording_dropped: s.recording_dropped.load(Ordering::Relaxed),
            recording_backlog: self.recording().map_or(0, |r| r.backlog()),
            replay_seconds,
            replay_bytes,
            replay_enabled,
        }
    }
}

impl Drop for Router {
    fn drop(&mut self) {
        let _ = self.ctl.send(Ctl::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn route(setup: StreamSetup, video: &Receiver<EncodedVideo>, audio: &Receiver<(u8, EncodedAudio)>, ctl: &Receiver<Ctl>, shared: &Shared) {
    let mut config: Option<Vec<u8>> = None;
    let mut recording: Option<Recording> = None;
    let mut replay: Option<ReplayBuffer> = None;
    let mut buf = Vec::new();
    let mut video_open = true;
    let mut audio_open = true;
    let publish_replay = |replay: &Option<ReplayBuffer>| {
        let v = replay.as_ref().map_or((0.0, 0, false), |r| (r.seconds_held(), r.bytes(), true));
        *shared.replay.lock().unwrap_or_else(PoisonError::into_inner) = v;
    };
    let dispatch = |p: Packet, recording: &Option<Recording>, replay: &mut Option<ReplayBuffer>| {
        if let Some(r) = replay.as_mut() {
            r.push(p.clone());
        }
        if let Some(rec) = recording
            && !rec.offer(p)
        {
            shared.recording_dropped.fetch_add(1, Ordering::Relaxed);
        }
    };
    let mut replay_publish_tick = 0u32;
    loop {
        select! {
            recv(video) -> msg => match msg {
                Ok(v) => {
                    shared.video_packets.fetch_add(1, Ordering::Relaxed);
                    shared.video_bytes.fetch_add(v.data.len() as u64, Ordering::Relaxed);
                    if annexb_to_length_prefixed(setup.video_codec, &v.data, &mut buf).is_err() {
                        shared.conversion_errors.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    if v.keyframe && config.is_none() {
                        match config_from_length_prefixed(setup.video_codec, &buf) {
                            Ok(c) => {
                                if let Some(r) = &recording {
                                    r.set_config(c.clone());
                                }
                                config = Some(c);
                            }
                            Err(e) => log::warn!("no codec configuration in the first keyframe: {e}"),
                        }
                    }
                    let p = Packet { kind: StreamKind::Video, pts: v.pts, keyframe: v.keyframe, data: Arc::from(buf.as_slice()) };
                    dispatch(p, &recording, &mut replay);
                    replay_publish_tick = replay_publish_tick.wrapping_add(1);
                    if replay_publish_tick.is_multiple_of(15) {
                        publish_replay(&replay);
                    }
                }
                Err(_) => { video_open = false; }
            },
            recv(audio) -> msg => match msg {
                Ok((track, a)) => {
                    shared.audio_packets.fetch_add(1, Ordering::Relaxed);
                    shared.audio_bytes.fetch_add(a.data.len() as u64, Ordering::Relaxed);
                    if usize::from(track) < setup.audio.len() {
                        let p = Packet { kind: StreamKind::Audio(track), pts: a.pts, keyframe: true, data: Arc::from(a.data.as_slice()) };
                        dispatch(p, &recording, &mut replay);
                    }
                }
                Err(_) => { audio_open = false; }
            },
            recv(ctl) -> msg => match msg {
                Ok(Ctl::StartRecording(cfg, reply)) => {
                    let r = if recording.is_some() {
                        Err("already recording".to_string())
                    } else {
                        Recording::start(*cfg, setup.clone(), config.clone()).inspect(|rec| {
                            recording = Some(rec.clone());
                            *shared.recording.lock().unwrap_or_else(PoisonError::into_inner) = Some(rec.clone());
                        })
                    };
                    let _ = reply.send(r);
                }
                Ok(Ctl::StopRecording(reply)) => {
                    let rec = recording.take();
                    if let Some(r) = &rec {
                        r.stop();
                    }
                    *shared.recording.lock().unwrap_or_else(PoisonError::into_inner) = None;
                    let _ = reply.send(rec);
                }
                Ok(Ctl::Pause(p)) => {
                    if let Some(r) = &recording {
                        r.set_paused(p);
                    }
                }
                Ok(Ctl::Replay(cfg)) => {
                    replay = cfg.map(ReplayBuffer::new);
                    publish_replay(&replay);
                }
                Ok(Ctl::SaveReplay(path, mp4, reply)) => {
                    let r = match (&replay, &config) {
                        (None, _) => Err("the replay buffer is off".to_string()),
                        (Some(_), None) => Err("the replay buffer is still empty".to_string()),
                        (Some(rb), Some(cfg)) => {
                            let packets = rb.snapshot();
                            let tracks = setup.tracks(cfg);
                            replay::save_clip_async(path, tracks, packets, mp4)
                        }
                    };
                    let _ = reply.send(r);
                }
                Ok(Ctl::Stop) | Err(_) => break,
            },
        }
        if !video_open && !audio_open {
            // Inputs gone: keep serving control messages until told to stop.
            match ctl.recv_timeout(Duration::from_millis(200)) {
                Ok(Ctl::Stop) | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                Ok(Ctl::StopRecording(reply)) => {
                    let rec = recording.take();
                    if let Some(r) = &rec {
                        r.stop();
                    }
                    *shared.recording.lock().unwrap_or_else(PoisonError::into_inner) = None;
                    let _ = reply.send(rec);
                }
                _ => {}
            }
        }
    }
    if let Some(r) = recording.take() {
        r.stop();
        r.wait(Duration::from_secs(30));
    }
    *shared.recording.lock().unwrap_or_else(PoisonError::into_inner) = None;
}

/// Sends to a bounded channel without blocking; `false` when full or closed.
pub(crate) fn offer<T>(tx: &Sender<T>, v: T) -> bool {
    match tx.try_send(v) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
    }
}
