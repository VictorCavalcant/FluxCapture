//! The audio side: one WASAPI capture per enabled source, and the mixer thread.
//!
//! The mixer thread wakes every 10 ms, feeds every captured chunk to the [`Mixer`], and mixes the
//! window that ends `delay` behind real time (so every device has delivered). While encoding it
//! converts each output track to the codec (AAC through Media Foundation, or float PCM) and sends
//! the packets to the router; meters are published for the UI about 30 times per second.

use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, bounded};
use fluxcapture_audio::{AudioChunk, Dither, LevelMeter, MixBuffers, Mixer, MixerSettings, SourceId, SourceMeter, SourceStats, f32_to_i16};
use fluxcapture_platform::audio::{AacEncoder, AudioCapture};
use fluxcapture_platform::clock::{PrecisionTimer, ThreadKind, boost_thread, now};
use fluxcapture_platform::{AudioCaptureState, EncodedAudio};
use fluxcapture_settings::{AudioCodec, AudioSourceConfig};
use fluxcapture_time::{Hns, SampleClock};

/// Frames per mix block (10 ms).
const BLOCK: usize = 480;

/// What the recording needs from the audio side.
#[derive(Clone, Debug)]
pub struct AudioEncodeConfig {
    pub tracks: usize,
    pub codec: AudioCodec,
    pub kbps: u32,
}

enum Ctl {
    Sources(Vec<(AudioSourceConfig, u8)>),
    Mixer(MixerSettings, Hns),
    StartEncoding(AudioEncodeConfig, Sender<(u8, EncodedAudio)>, Sender<Result<(), String>>),
    StopEncoding(Sender<()>),
    ResetClip,
    Stop,
}

/// Published for the UI and diagnostics.
#[derive(Clone, Debug, Default)]
pub struct AudioSnapshot {
    pub meters: Vec<SourceMeter>,
    pub master: Vec<LevelMeter>,
    pub stats: Vec<SourceStats>,
    pub captures: Vec<(u32, String, AudioCaptureState)>,
    pub dropped_chunks: u64,
    pub encode_error: Option<String>,
    pub encoding: bool,
}

pub struct AudioSystem {
    ctl: Sender<Ctl>,
    shared: Arc<Mutex<AudioSnapshot>>,
    thread: Option<JoinHandle<()>>,
}

impl AudioSystem {
    pub fn start(mixer: MixerSettings, delay: Hns) -> Result<AudioSystem, String> {
        let (ctl, rx) = bounded(64);
        let shared = Arc::new(Mutex::new(AudioSnapshot::default()));
        let shared2 = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("fluxcapture-mixer".into())
            .spawn(move || {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(mixer, delay, &rx, &shared2)));
                if r.is_err() {
                    log::error!("the audio mixer panicked");
                    shared2.lock().unwrap_or_else(PoisonError::into_inner).encode_error = Some("internal error in the audio mixer".into());
                }
            })
            .map_err(|e| format!("cannot start the mixer: {e}"))?;
        Ok(AudioSystem { ctl, shared, thread: Some(thread) })
    }

    /// Replaces the sources; each comes with its track mask.
    pub fn set_sources(&self, sources: Vec<(AudioSourceConfig, u8)>) {
        let _ = self.ctl.send(Ctl::Sources(sources));
    }

    pub fn set_mixer(&self, m: MixerSettings, delay: Hns) {
        let _ = self.ctl.send(Ctl::Mixer(m, delay));
    }

    pub fn start_encoding(&self, cfg: AudioEncodeConfig, sink: Sender<(u8, EncodedAudio)>) -> Result<(), String> {
        let (tx, rx) = bounded(1);
        self.ctl.send(Ctl::StartEncoding(cfg, sink, tx)).map_err(|_| "the mixer is not running".to_string())?;
        rx.recv_timeout(Duration::from_secs(10)).map_err(|_| "the mixer did not answer".to_string())?
    }

    /// Flushes the encoders (their last frames are sent) and stops encoding.
    pub fn stop_encoding(&self) {
        let (tx, rx) = bounded(1);
        if self.ctl.send(Ctl::StopEncoding(tx)).is_ok() {
            let _ = rx.recv_timeout(Duration::from_secs(5));
        }
    }

    pub fn reset_clip(&self) {
        let _ = self.ctl.send(Ctl::ResetClip);
    }

    pub fn snapshot(&self) -> AudioSnapshot {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

impl Drop for AudioSystem {
    fn drop(&mut self) {
        let _ = self.ctl.send(Ctl::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

enum TrackEncoder {
    Aac(AacEncoder),
    Pcm,
}

struct Encoding {
    encoders: Vec<TrackEncoder>,
    sink: Sender<(u8, EncodedAudio)>,
    pcm: Vec<i16>,
    out: Vec<EncodedAudio>,
    dither: Dither,
}

struct Capture {
    id: u32,
    name: String,
    capture: AudioCapture,
}

fn run(mixer_settings: MixerSettings, delay: Hns, rx: &Receiver<Ctl>, shared: &Mutex<AudioSnapshot>) {
    let _prio = boost_thread(ThreadKind::ProAudio);
    let timer = PrecisionTimer::new().ok();
    let mut mixer = Mixer::new(mixer_settings);
    let mut delay = delay;
    let (chunk_tx, chunk_rx) = bounded::<AudioChunk>(1024);
    let mut captures: Vec<Capture> = Vec::new();
    let mut configs: Vec<(AudioSourceConfig, u8)> = Vec::new();
    let Some(clock) = SampleClock::new(fluxcapture_audio::OUTPUT_RATE) else { return };
    let mut base = now() - delay;
    let mut pos: i64 = 0;
    let mut buffers = MixBuffers::new(fluxcapture_audio::MAX_TRACKS, BLOCK);
    let mut encoding: Option<Encoding> = None;
    let mut meter_tick = 0u32;
    let mut next_wake = now();
    loop {
        while let Ok(c) = rx.try_recv() {
            match c {
                Ctl::Stop => {
                    if let Some(mut e) = encoding.take() {
                        flush(&mut e);
                    }
                    return;
                }
                Ctl::Sources(list) => {
                    apply_sources(&mut mixer, &mut captures, &mut configs, list, &chunk_tx);
                }
                Ctl::Mixer(m, d) => {
                    mixer.set_settings(m);
                    if d != delay {
                        delay = d;
                        base = now() - delay;
                        pos = 0;
                    }
                }
                Ctl::StartEncoding(cfg, sink, reply) => {
                    let r = start_encoders(&cfg).map(|encoders| {
                        encoding = Some(Encoding { encoders, sink, pcm: Vec::with_capacity(BLOCK * 2), out: Vec::new(), dither: Dither::new(0x5EED) });
                    });
                    shared.lock().unwrap_or_else(PoisonError::into_inner).encode_error = r.as_ref().err().cloned();
                    let _ = reply.send(r);
                }
                Ctl::StopEncoding(reply) => {
                    if let Some(mut e) = encoding.take() {
                        flush(&mut e);
                    }
                    let _ = reply.send(());
                }
                Ctl::ResetClip => mixer.reset_clip(),
            }
        }
        while let Ok(chunk) = chunk_rx.try_recv() {
            mixer.push(chunk);
        }
        // Mix every complete block that is now `delay` old.
        let horizon = now() - delay;
        let mut blocks = 0;
        loop {
            let start = base + clock.time_of(pos);
            let end = base + clock.time_of(pos + BLOCK as i64);
            if end > horizon {
                break;
            }
            if blocks > 50 {
                // More than half a second behind (the thread was starved): skip ahead instead of
                // mixing a backlog that would arrive too late anyway.
                log::warn!("audio mixer fell behind; skipping ahead");
                pos = clock.samples_in(horizon - base).max(pos);
                break;
            }
            mixer.mix(start, BLOCK, &mut buffers);
            if let Some(e) = encoding.as_mut()
                && let Err(err) = encode_block(e, &buffers, start)
            {
                log::error!("audio encoding failed: {err}");
                shared.lock().unwrap_or_else(PoisonError::into_inner).encode_error = Some(err);
                encoding = None;
            }
            pos += BLOCK as i64;
            blocks += 1;
        }
        meter_tick = meter_tick.wrapping_add(1);
        if meter_tick.is_multiple_of(3) {
            let mut s = shared.lock().unwrap_or_else(PoisonError::into_inner);
            s.meters = mixer.meters();
            s.master = mixer.master_meters();
            if meter_tick.is_multiple_of(30) {
                s.stats = mixer.source_stats();
                s.captures = captures.iter().map(|c| (c.id, c.name.clone(), c.capture.state())).collect();
                s.dropped_chunks = captures.iter().map(|c| c.capture.dropped_chunks()).sum();
            }
            s.encoding = encoding.is_some();
        }
        next_wake = next_wake.max(now() - Hns::from_millis(50)) + Hns::from_millis(10);
        match &timer {
            Some(t) => t.wait_until(next_wake),
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

fn apply_sources(
    mixer: &mut Mixer,
    captures: &mut Vec<Capture>,
    configs: &mut Vec<(AudioSourceConfig, u8)>,
    list: Vec<(AudioSourceConfig, u8)>,
    chunk_tx: &Sender<AudioChunk>,
) {
    // Remove sources that disappeared, were disabled or changed endpoint.
    let keep = |c: &AudioSourceConfig| list.iter().any(|(n, _)| n.id == c.id && n.enabled && n.endpoint == c.endpoint);
    captures.retain(|cap| configs.iter().find(|(c, _)| c.id == cap.id).is_some_and(|(c, _)| keep(c)));
    for (c, _) in configs.iter() {
        if !keep(c) {
            mixer.remove_source(SourceId(c.id));
        }
    }
    for (cfg, mask) in &list {
        if !cfg.enabled {
            continue;
        }
        let mut s = cfg.settings.clone();
        s.tracks = *mask;
        let id = SourceId(cfg.id);
        if captures.iter().any(|c| c.id == cfg.id) {
            mixer.update_source(id, s);
        } else {
            if let Err(e) = mixer.add_source(id, cfg.name.clone(), s) {
                log::warn!("audio source {}: {e}", cfg.name);
                continue;
            }
            match AudioCapture::start(cfg.endpoint.clone(), id, chunk_tx.clone()) {
                Ok(capture) => captures.push(Capture { id: cfg.id, name: cfg.name.clone(), capture }),
                Err(e) => log::warn!("cannot capture {}: {e}", cfg.name),
            }
        }
    }
    *configs = list;
}

fn start_encoders(cfg: &AudioEncodeConfig) -> Result<Vec<TrackEncoder>, String> {
    (0..cfg.tracks.clamp(1, fluxcapture_audio::MAX_TRACKS))
        .map(|_| match cfg.codec {
            AudioCodec::Aac => AacEncoder::new(fluxcapture_audio::OUTPUT_RATE, 2, cfg.kbps).map(TrackEncoder::Aac).map_err(|e| e.0),
            AudioCodec::Pcm => Ok(TrackEncoder::Pcm),
        })
        .collect()
}

fn encode_block(e: &mut Encoding, buffers: &MixBuffers, start: Hns) -> Result<(), String> {
    let dur = Hns(BLOCK as i64 * 10_000_000 / i64::from(fluxcapture_audio::OUTPUT_RATE));
    for (t, enc) in e.encoders.iter_mut().enumerate() {
        let Some(samples) = buffers.tracks.get(t) else { continue };
        match enc {
            TrackEncoder::Aac(aac) => {
                e.pcm.resize(samples.len(), 0);
                f32_to_i16(samples, &mut e.pcm, &mut e.dither);
                aac.encode(&e.pcm, start, &mut e.out).map_err(|x| x.0)?;
            }
            TrackEncoder::Pcm => {
                let mut data = Vec::with_capacity(samples.len() * 4);
                for s in samples {
                    data.extend_from_slice(&s.to_le_bytes());
                }
                e.out.push(EncodedAudio { pts: start, duration: dur, data });
            }
        }
        for pkt in e.out.drain(..) {
            if e.sink.try_send((t as u8, pkt)).is_err() {
                log::warn!("audio packet dropped: the router queue is full");
            }
        }
    }
    Ok(())
}

fn flush(e: &mut Encoding) {
    for (t, enc) in e.encoders.iter_mut().enumerate() {
        if let TrackEncoder::Aac(aac) = enc {
            let _ = aac.finish(&mut e.out);
        }
        for pkt in e.out.drain(..) {
            let _ = e.sink.send_timeout((t as u8, pkt), Duration::from_millis(200));
        }
    }
}
