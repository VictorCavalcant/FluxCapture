//! The realtime mixer.

use fluxcapture_time::{Hns, SampleClock};
use serde::{Deserialize, Serialize};

use crate::dsp::{Limiter, Ramp};
use crate::meter::{LevelMeter, MeterState, SourceMeter};
use crate::settings::{MixerSettings, SourceSettings};
use crate::source::{Source, SourceStats, Timeline};
use crate::util::db_to_gain;
use crate::{
    AudioChunk, AudioError, LIMITER_LOOKAHEAD_FRAMES, MAX_MIX_FRAMES, MAX_SOURCES, MAX_TRACKS, NOISE_SUPPRESSION_LATENCY_FRAMES, OUTPUT_RATE,
    PROCESSING_LOOKAHEAD_FRAMES, SourceId,
};

/// Internal processing block (frames); `mix` windows of any size are split into blocks.
const BLOCK: usize = 512;
/// Window starts within half a sample of the expected one are contiguous.
const CONTIGUITY_TOLERANCE_HNS: i64 = 104;

/// Output buffers: one interleaved stereo `Vec` per track.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MixBuffers {
    /// `tracks[t]` holds `frames * 2` samples after a `mix` call.
    pub tracks: Vec<Vec<f32>>,
}

impl MixBuffers {
    /// Buffers for `tracks` tracks (clamped to `1..=MAX_TRACKS`) pre-sized for windows of up to
    /// `max_frames` frames (clamped to [`MAX_MIX_FRAMES`]), so `mix` never allocates.
    pub fn new(tracks: usize, max_frames: usize) -> Self {
        let cap = 2 * max_frames.min(MAX_MIX_FRAMES);
        MixBuffers { tracks: (0..tracks.clamp(1, MAX_TRACKS)).map(|_| Vec::with_capacity(cap)).collect() }
    }
}

/// Mixer-wide counters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MixerStats {
    /// `mix` calls whose `start` was not contiguous with the previous window.
    pub window_discontinuities: u64,
    /// Chunks pushed for a source that does not exist.
    pub unknown_source_chunks: u64,
    /// Frames requested beyond [`MAX_MIX_FRAMES`] in a single call (not produced).
    pub truncated_frames: u64,
}

struct Track {
    accum: Vec<f32>,
    master: Ramp,
    limiter: Limiter,
    meter: MeterState,
}

/// Mixes any number (up to [`MAX_SOURCES`]) of sources into up to [`MAX_TRACKS`] 48 kHz stereo
/// tracks. Owned by one thread; no locks, no allocation in [`mix`](Mixer::mix) once warmed up.
pub struct Mixer {
    settings: MixerSettings,
    sources: Vec<Source>,
    tracks: Vec<Track>,
    timeline: Option<Timeline>,
    next: Option<(Hns, i64)>,
    stats: MixerStats,
}

impl Mixer {
    /// A mixer with `settings` (sanitized).
    pub fn new(settings: MixerSettings) -> Mixer {
        let settings = settings.sanitized();
        let master = db_to_gain(settings.master_volume_db);
        let tracks = (0..MAX_TRACKS)
            .map(|_| {
                let mut limiter = Limiter::new();
                limiter.configure(settings.limiter.as_ref());
                Track { accum: vec![0.0; 2 * BLOCK], master: Ramp::new(master), limiter, meter: MeterState::default() }
            })
            .collect();
        Mixer { settings, sources: Vec::with_capacity(MAX_SOURCES), tracks, timeline: None, next: None, stats: MixerStats::default() }
    }

    /// Current (sanitized) settings.
    pub fn settings(&self) -> &MixerSettings {
        &self.settings
    }

    /// Changes the mixer settings. Volume changes ramp; newly enabled tracks start clean.
    pub fn set_settings(&mut self, s: MixerSettings) {
        let s = s.sanitized();
        let old_tracks = self.settings.tracks;
        let master = db_to_gain(s.master_volume_db);
        for (t, tr) in self.tracks.iter_mut().enumerate() {
            if t >= old_tracks && t < s.tracks {
                tr.limiter.clear();
                tr.meter = MeterState::default();
                tr.master.jump(master);
            }
            tr.master.set(master);
            tr.limiter.configure(s.limiter.as_ref());
        }
        log::info!("audio mixer: {} track(s), master {:.1} dB, limiter {:?}", s.tracks, s.master_volume_db, s.limiter);
        self.settings = s;
    }

    /// Adds a source (or, if `id` exists, renames it and applies `s`).
    pub fn add_source(&mut self, id: SourceId, name: String, s: SourceSettings) -> Result<(), AudioError> {
        if let Some(src) = self.sources.iter_mut().find(|x| x.id == id) {
            src.name = name;
            src.update_settings(s);
            return Ok(());
        }
        if self.sources.len() >= MAX_SOURCES {
            log::warn!("audio mixer: cannot add source {name:?}: {MAX_SOURCES} sources already");
            return Err(AudioError::TooManySources { max: MAX_SOURCES });
        }
        log::info!("audio mixer: added source {id:?} {name:?}");
        self.sources.push(Source::new(id, name, s, BLOCK));
        Ok(())
    }

    /// Applies new settings to a source (unknown ids are ignored). Gain changes ramp; a sync offset
    /// change shifts the source's placement immediately.
    pub fn update_source(&mut self, id: SourceId, s: SourceSettings) {
        if let Some(src) = self.sources.iter_mut().find(|x| x.id == id) {
            src.update_settings(s);
        }
    }

    /// Removes a source and its buffered audio.
    pub fn remove_source(&mut self, id: SourceId) {
        let before = self.sources.len();
        self.sources.retain(|x| x.id != id);
        if self.sources.len() != before {
            log::info!("audio mixer: removed source {id:?}");
        }
    }

    /// Ids of the current sources.
    pub fn source_ids(&self) -> Vec<SourceId> {
        self.sources.iter().map(|s| s.id).collect()
    }

    /// Takes one captured chunk: converts it to 48 kHz stereo, places it on the timeline (see the
    /// crate `README.md`: anchoring, drift correction, gap handling) and appends it to the source's
    /// bounded FIFO. Chunks for unknown sources or in unsupported formats are counted and dropped.
    pub fn push(&mut self, chunk: AudioChunk) {
        let Some(src) = self.sources.iter_mut().find(|x| x.id == chunk.source) else {
            self.stats.unknown_source_chunks = self.stats.unknown_source_chunks.saturating_add(1);
            return;
        };
        let tl = *self.timeline.get_or_insert(Timeline { anchor_time: chunk.time, anchor_pos: 0 });
        src.ingest(&chunk, &tl);
    }

    /// Timeline position of the window starting at `start`.
    /// Timeline position of the window starting at `start`, and whether it continues the previous
    /// window.
    fn window_position(&mut self, start: Hns) -> (i64, bool) {
        if let Some((t, p)) = self.next {
            if start.0.saturating_sub(t.0).saturating_abs() <= CONTIGUITY_TOLERANCE_HNS {
                return (p, true);
            }
            self.stats.window_discontinuities = self.stats.window_discontinuities.saturating_add(1);
        }
        let tl = self.timeline.get_or_insert(Timeline { anchor_time: start, anchor_pos: 0 });
        let p = tl.pos_round(start);
        *tl = Timeline { anchor_time: start, anchor_pos: p };
        (p, false)
    }

    /// Mixes the output window `[start, start + frames / 48 kHz)` into `out.tracks[t]` (interleaved
    /// stereo, `frames * 2` samples) for every `t < settings.tracks`; further existing buffers are
    /// zero-filled. Consecutive windows must be contiguous (compute `start` with a
    /// [`SampleClock`] from a fixed base); a non-contiguous `start` is a discontinuity: the timeline
    /// re-anchors there and stale source audio is discarded.
    ///
    /// The output is time-aligned: noise suppression and limiter latencies are compensated by reading
    /// [`crate::PROCESSING_LOOKAHEAD_FRAMES`] ahead, so `start` must trail real time by at least
    /// [`crate::processing_lookahead`] plus device buffering ([`crate::RECOMMENDED_MIX_DELAY`]).
    ///
    /// Returns the number of frames produced: `frames`, or [`MAX_MIX_FRAMES`] if more was asked.
    pub fn mix(&mut self, start: Hns, frames: usize, out: &mut MixBuffers) -> usize {
        if frames > MAX_MIX_FRAMES {
            self.stats.truncated_frames = self.stats.truncated_frames.saturating_add((frames - MAX_MIX_FRAMES) as u64);
        }
        let frames = frames.min(MAX_MIX_FRAMES);
        let nt = self.settings.tracks.clamp(1, MAX_TRACKS);
        while out.tracks.len() < nt {
            out.tracks.push(Vec::with_capacity(2 * frames));
        }
        for v in out.tracks.iter_mut() {
            v.clear();
            v.resize(2 * frames, 0.0);
        }
        if frames == 0 {
            return 0;
        }
        let (pos, contiguous) = self.window_position(start);
        if !contiguous {
            // First window or a jump: run the lookahead span before `pos` through every delay line
            // (noise suppression, limiter) with the output discarded, so the window's first frames
            // carry the right audio instead of whatever the delay lines held.
            let lookahead = PROCESSING_LOOKAHEAD_FRAMES;
            let mut done = 0;
            while done < lookahead {
                let b = BLOCK.min(lookahead - done);
                self.mix_block(pos.saturating_sub((lookahead - done) as i64), b, 0, nt, None);
                done += b;
            }
        }
        let mut done = 0;
        while done < frames {
            let b = BLOCK.min(frames - done);
            self.mix_block(pos.saturating_add(done as i64), b, done, nt, Some(&mut *out));
            done += b;
        }
        let dur = SampleClock::new(OUTPUT_RATE).map_or(Hns::ZERO, |c| c.time_of(frames as i64));
        self.next = Some((start + dur, pos.saturating_add(frames as i64)));
        let dt = frames as f32 / OUTPUT_RATE as f32;
        for s in self.sources.iter_mut() {
            s.meter.finish(dt);
        }
        for tr in self.tracks.iter_mut().take(nt) {
            tr.meter.finish(dt);
        }
        frames
    }

    fn mix_block(&mut self, pos: i64, b: usize, offset: usize, nt: usize, mut out: Option<&mut MixBuffers>) {
        // Track (pre-limiter) position: the limiter delays by its lookahead. Sources are read a
        // further noise-suppression latency ahead because their chain delays by that much.
        let track_pos = pos.saturating_add(LIMITER_LOOKAHEAD_FRAMES as i64);
        let read_pos = track_pos.saturating_add(NOISE_SUPPRESSION_LATENCY_FRAMES as i64);
        let tracks = &mut self.tracks[..nt];
        for tr in tracks.iter_mut() {
            tr.accum[..2 * b].fill(0.0);
        }
        for src in self.sources.iter_mut() {
            src.render(read_pos, b);
            let mask = src.settings.tracks;
            let sb = &src.buf[..2 * b];
            for (t, tr) in tracks.iter_mut().enumerate() {
                if mask & (1 << t) != 0 {
                    for (a, s) in tr.accum.iter_mut().zip(sb) {
                        *a += s;
                    }
                }
            }
        }
        for (t, tr) in tracks.iter_mut().enumerate() {
            let acc = &mut tr.accum[..2 * b];
            if !tr.master.is_steady_at(1.0) {
                for f in acc.as_chunks_mut::<2>().0.iter_mut() {
                    let g = tr.master.next();
                    f[0] *= g;
                    f[1] *= g;
                }
            }
            tr.limiter.process(acc);
            tr.meter.accumulate(acc);
            if let Some(dst) = out.as_mut().and_then(|o| o.tracks.get_mut(t)).and_then(|o| o.get_mut(2 * offset..2 * (offset + b))) {
                for (d, &a) in dst.iter_mut().zip(acc.iter()) {
                    *d = if a.is_finite() { a } else { 0.0 };
                }
            }
        }
    }

    /// Post-fader meters of every source.
    pub fn meters(&self) -> Vec<SourceMeter> {
        self.sources.iter().map(|s| SourceMeter { id: s.id, name: s.name.clone(), level: s.meter.reading() }).collect()
    }

    /// Post-limiter meters of each active track.
    pub fn master_meters(&self) -> Vec<LevelMeter> {
        self.tracks.iter().take(self.settings.tracks).map(|t| t.meter.reading()).collect()
    }

    /// Buffer, clock and health statistics of every source.
    pub fn source_stats(&self) -> Vec<SourceStats> {
        self.sources.iter().map(Source::stats).collect()
    }

    /// Mixer-wide counters.
    pub fn stats(&self) -> MixerStats {
        self.stats.clone()
    }

    /// Clears every sticky clip indicator.
    pub fn reset_clip(&mut self) {
        for s in self.sources.iter_mut() {
            s.meter.reset_clip();
        }
        for t in self.tracks.iter_mut() {
            t.meter.reset_clip();
        }
    }
}
