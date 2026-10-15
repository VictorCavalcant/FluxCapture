//! One mixer input: format conversion, timeline placement with drift correction, FIFO and filters.
//!
//! # Timeline placement
//!
//! Positions are output frames (48 kHz) on the mixer's timeline ([`Timeline`] maps QPC time to a
//! frame position). For each source:
//!
//! * `write_pos` is the position of the next frame the resampler will produce; the FIFO holds the
//!   `len` frames just before it (`head = write_pos - len`).
//! * The resampler knows exactly where the next *input* frame will land:
//!   `write_pos + pending_output_frames()` — the "contiguous position".
//! * Each chunk's timestamp (+ sync offset) gives its "timestamp position". Their difference is the
//!   clock error. Normal timestamp jitter (a few ms) is absorbed: chunks are always appended
//!   contiguously and the error only steers the drift controller ([`crate::drift`]), which trims the
//!   resampling ratio by at most ±0.5 % so the long-term error converges to zero.
//! * The first chunk, a chunk flagged `discontinuity`, a format change, or an error beyond
//!   ±100 ms **anchors** the source instead: the chunk's first frame is placed exactly (to a
//!   fraction of a sample) at its timestamp position. A forward jump leaves a silent gap, a backward
//!   jump discards the overlapping newest audio.
//! * Non-continuous sources (loopback, which sends nothing while nothing plays) re-anchor on any
//!   forward error above 20 ms: a resume after a pause becomes silence + audio at the right time,
//!   never a stretched catch-up.
//!
//! At mix time frames older than the window are discarded (late data), positions before the FIFO
//! head are silence, and positions after `write_pos` are missing data — silence plus an underrun for
//! continuous sources.

use fluxcapture_time::{Hns, SampleClock};
use serde::{Deserialize, Serialize};

use crate::channels::{stereo_matrix, to_stereo};
use crate::denoise::NoiseSuppressor;
use crate::drift::DriftController;
use crate::dsp::{Compressor, Gate, HighPass, Ramp};
use crate::fifo::Fifo;
use crate::meter::MeterState;
use crate::resample::{MAX_INPUT_BLOCK, Resampler};
use crate::settings::SourceSettings;
use crate::util::db_to_gain;
use crate::{AudioChunk, AudioFormat, OUTPUT_RATE, SOURCE_BUFFER_FRAMES, SourceId};

/// Errors beyond this re-anchor instead of steering the drift controller.
const REANCHOR_FRAMES: f64 = 0.100 * OUTPUT_RATE as f64;
/// Forward errors beyond this re-anchor a non-continuous (loopback) source: it paused.
const GAP_FRAMES: f64 = 0.020 * OUTPUT_RATE as f64;
/// Positions are kept far inside `i64` (and exactly representable in `f64`).
const POS_LIMIT: f64 = 4.0e15;
const FRAMES_PER_HNS: f64 = OUTPUT_RATE as f64 / fluxcapture_time::HNS_PER_SEC as f64;

/// Maps QPC time to output-frame positions: `anchor_time` is frame `anchor_pos`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct Timeline {
    pub(crate) anchor_time: Hns,
    pub(crate) anchor_pos: i64,
}

impl Timeline {
    /// Fractional frame position of `t`.
    pub(crate) fn pos_of(&self, t: Hns) -> f64 {
        let p = self.anchor_pos as f64 + (t - self.anchor_time).0 as f64 * FRAMES_PER_HNS;
        p.clamp(-POS_LIMIT, POS_LIMIT)
    }

    /// Frame position of `t`, rounded to nearest.
    pub(crate) fn pos_round(&self, t: Hns) -> i64 {
        let n = SampleClock::new(OUTPUT_RATE).map_or(0, |c| c.samples_in(t - self.anchor_time));
        self.anchor_pos.saturating_add(n).clamp(-(POS_LIMIT as i64), POS_LIMIT as i64)
    }
}

/// Health and clock statistics of one source.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SourceStats {
    /// Source.
    pub id: SourceId,
    /// Display name.
    pub name: String,
    /// Converted audio waiting to be mixed, ms.
    pub buffered_ms: f32,
    /// Episodes of missing data for a continuous source (a window that could not be filled).
    pub underruns: u64,
    /// Frames of missing data for a continuous source.
    pub underrun_frames: u64,
    /// Chunks that overflowed the 2 s FIFO (oldest audio dropped).
    pub overruns: u64,
    /// Re-anchors caused by device discontinuities, format changes or timestamp jumps.
    pub discontinuities: u64,
    /// Re-anchors of a non-continuous source after a pause (normal for loopback).
    pub gaps: u64,
    /// Frames that arrived after their window had been mixed and were discarded.
    pub late_frames: u64,
    /// Chunks rejected for an unsupported format.
    pub rejected_chunks: u64,
    /// Estimated device clock drift relative to QPC, ppm (positive: the device runs fast).
    pub drift_ppm: f64,
    /// Current low-passed timeline error, ms (timestamp position minus contiguous position).
    pub timeline_error_ms: f64,
    /// Timestamp of the last chunk received.
    pub last_chunk_time: Option<Hns>,
    /// Format of the last accepted chunk.
    pub format: Option<AudioFormat>,
}

/// The per-source filter chain at 48 kHz stereo.
struct Chain {
    gain: Ramp,
    high_pass: HighPass,
    denoise: NoiseSuppressor,
    gate: Gate,
    compressor: Compressor,
    mono: bool,
    fader_l: Ramp,
    fader_r: Ramp,
}

impl Chain {
    fn new(s: &SourceSettings) -> Chain {
        let mut c = Chain {
            gain: Ramp::new(1.0),
            high_pass: HighPass::default(),
            denoise: NoiseSuppressor::new(),
            gate: Gate::default(),
            compressor: Compressor::default(),
            mono: false,
            fader_l: Ramp::new(1.0),
            fader_r: Ramp::new(1.0),
        };
        c.configure(s, true);
        c
    }

    fn configure(&mut self, s: &SourceSettings, initial: bool) {
        let f = &s.filters;
        let vol = if s.muted { 0.0 } else { db_to_gain(s.volume_db) };
        let gl = vol * (1.0 - s.balance).min(1.0);
        let gr = vol * (1.0 + s.balance).min(1.0);
        if initial {
            self.gain.jump(db_to_gain(f.gain_db));
            self.fader_l.jump(gl);
            self.fader_r.jump(gr);
        } else {
            self.gain.set(db_to_gain(f.gain_db));
            self.fader_l.set(gl);
            self.fader_r.set(gr);
        }
        self.high_pass.configure(f.high_pass.as_ref());
        self.denoise.configure(f.noise_suppression.as_ref());
        self.gate.configure(f.gate.as_ref());
        self.compressor.configure(f.compressor.as_ref());
        self.mono = s.force_mono;
    }

    fn process(&mut self, buf: &mut [f32]) {
        if !self.gain.is_steady_at(1.0) {
            for f in buf.as_chunks_mut::<2>().0.iter_mut() {
                let g = self.gain.next();
                f[0] *= g;
                f[1] *= g;
            }
        }
        self.high_pass.process(buf);
        self.denoise.process(buf);
        self.gate.process(buf);
        self.compressor.process(buf);
        let frames = buf.as_chunks_mut::<2>().0;
        if self.mono {
            for f in frames.iter_mut() {
                let m = 0.5 * (f[0] + f[1]);
                *f = [m, m];
            }
        }
        match (self.fader_l.steady(), self.fader_r.steady()) {
            (Some(gl), Some(gr)) => {
                if gl != 1.0 || gr != 1.0 {
                    for f in frames.iter_mut() {
                        *f = [f[0] * gl, f[1] * gr];
                    }
                }
            }
            _ => {
                for f in frames.iter_mut() {
                    *f = [f[0] * self.fader_l.next(), f[1] * self.fader_r.next()];
                }
            }
        }
    }
}

pub(crate) struct Source {
    pub(crate) id: SourceId,
    pub(crate) name: String,
    pub(crate) settings: SourceSettings,
    format: Option<AudioFormat>,
    matrix: Vec<[f32; 2]>,
    resampler: Option<Resampler>,
    fifo: Fifo,
    write_pos: i64,
    anchored: bool,
    ctl: DriftController,
    chain: Chain,
    /// Render scratch (interleaved stereo, one mix block).
    pub(crate) buf: Vec<f32>,
    /// Resampler output scratch for one input block (interleaved stereo).
    produced: Vec<f32>,
    pub(crate) meter: MeterState,
    /// Timeline position just after the last frame rendered.
    next_read: Option<i64>,
    in_underrun: bool,
    underruns: u64,
    underrun_frames: u64,
    overruns: u64,
    discontinuities: u64,
    gaps: u64,
    late_frames: u64,
    rejected_chunks: u64,
    last_chunk_time: Option<Hns>,
}

impl Source {
    pub(crate) fn new(id: SourceId, name: String, settings: SourceSettings, block: usize) -> Source {
        let settings = settings.sanitized();
        Source {
            id,
            name,
            chain: Chain::new(&settings),
            settings,
            format: None,
            matrix: Vec::new(),
            resampler: None,
            fifo: Fifo::new(SOURCE_BUFFER_FRAMES),
            write_pos: 0,
            anchored: false,
            ctl: DriftController::default(),
            buf: vec![0.0; 2 * block],
            // Up to 6× upsampling (8 kHz devices) of one input block, plus the drift trim.
            produced: Vec::with_capacity(2 * (MAX_INPUT_BLOCK * 6 + 64)),
            meter: MeterState::default(),
            next_read: None,
            in_underrun: false,
            underruns: 0,
            underrun_frames: 0,
            overruns: 0,
            discontinuities: 0,
            gaps: 0,
            late_frames: 0,
            rejected_chunks: 0,
            last_chunk_time: None,
        }
    }

    pub(crate) fn update_settings(&mut self, s: SourceSettings) {
        let s = s.sanitized();
        let delta_ms = i64::from(s.sync_offset_ms) - i64::from(self.settings.sync_offset_ms);
        if delta_ms != 0 && self.anchored {
            // Shift the placed audio with the offset; the next chunk then measures zero error.
            let delta = SampleClock::new(OUTPUT_RATE).map_or(0, |c| c.samples_in(Hns::from_millis(delta_ms)));
            self.write_pos = self.write_pos.saturating_add(delta);
        }
        self.chain.configure(&s, false);
        self.settings = s;
    }

    /// Converts, places and buffers one chunk.
    pub(crate) fn ingest(&mut self, chunk: &AudioChunk, timeline: &Timeline) {
        let fmt = chunk.format;
        if !fmt.is_supported() {
            self.rejected_chunks = self.rejected_chunks.saturating_add(1);
            return;
        }
        let ch = usize::from(fmt.channels);
        let frames = chunk.samples.len() / ch;
        if frames == 0 {
            return;
        }
        if self.format != Some(fmt) || self.resampler.is_none() {
            let rate_changed = self.resampler.as_ref().is_none_or(|r| r.input_rate() != fmt.sample_rate);
            if rate_changed {
                let Some(r) = Resampler::new(fmt.sample_rate) else {
                    self.rejected_chunks = self.rejected_chunks.saturating_add(1);
                    return;
                };
                self.resampler = Some(r);
                if self.anchored {
                    self.discontinuities = self.discontinuities.saturating_add(1);
                    self.anchored = false;
                }
            }
            self.matrix = stereo_matrix(ch);
            self.format = Some(fmt);
        }
        self.last_chunk_time = Some(chunk.time);

        let offset = Hns::from_millis(i64::from(self.settings.sync_offset_ms));
        let p_ts = timeline.pos_of(chunk.time + offset);
        if !self.anchored {
            self.anchor(p_ts);
        } else if chunk.discontinuity {
            self.discontinuities = self.discontinuities.saturating_add(1);
            self.anchor(p_ts);
        } else {
            let pending = self.resampler.as_ref().map_or(0.0, Resampler::pending_output_frames);
            let err = p_ts - (self.write_pos as f64 + pending);
            if !self.settings.continuous && err > GAP_FRAMES {
                self.gaps = self.gaps.saturating_add(1);
                self.anchor(p_ts);
            } else if err.abs() > REANCHOR_FRAMES {
                self.discontinuities = self.discontinuities.saturating_add(1);
                self.anchor(p_ts);
            } else {
                self.ctl.update(err / f64::from(OUTPUT_RATE), frames as f64 / f64::from(fmt.sample_rate));
                if let Some(rs) = self.resampler.as_mut() {
                    rs.set_correction(self.ctl.correction());
                }
            }
        }

        let Some(rs) = self.resampler.as_mut() else { return };
        let mut overflow = false;
        let samples = chunk.samples.get(..frames * ch).unwrap_or_default();
        for block in samples.chunks(ch * MAX_INPUT_BLOCK) {
            let n = block.len() / ch;
            let (l, r) = rs.input_slots(n);
            to_stereo(block, &self.matrix, l, r, chunk.silent);
            // Produce into the (pre-sized) scratch, then append to the FIFO in bulk.
            self.produced.clear();
            let out = &mut self.produced;
            rs.produce(|a, b| out.extend_from_slice(&[a, b]));
            overflow |= self.fifo.extend(&self.produced);
            self.write_pos = self.write_pos.saturating_add((self.produced.len() / 2) as i64);
        }
        if overflow {
            self.overruns = self.overruns.saturating_add(1);
        }
    }

    /// Places the next input frame at fractional position `p_ts`.
    fn anchor(&mut self, p_ts: f64) {
        let p = if p_ts.is_finite() { p_ts.clamp(-POS_LIMIT, POS_LIMIT) } else { 0.0 };
        let first = p.ceil();
        let frac = first - p;
        let new_wp = first as i64;
        // Everything before `next_read` has been mixed already: audio placed there is stale.
        let stale_before = self.next_read.unwrap_or(i64::MIN);
        if self.anchored
            && self.write_pos > stale_before
            && let Some(rs) = self.resampler.as_mut()
        {
            // Forward jump (gap): let the old segment's last frames out of the kernel first.
            let tail = rs.pending_output_frames().max(0.0).ceil() as i64 + 2;
            if new_wp > self.write_pos.saturating_add(tail) {
                let fifo = &mut self.fifo;
                let write_pos = &mut self.write_pos;
                let mut overflow = false;
                rs.flush(|a, b| {
                    overflow |= fifo.push(a, b);
                    *write_pos = write_pos.saturating_add(1);
                });
                if overflow {
                    self.overruns = self.overruns.saturating_add(1);
                }
            }
        }
        if self.anchored && self.fifo.len() > 0 && self.write_pos <= stale_before {
            // Nothing buffered can still be played (the mixer is past it): drop it.
            self.late_frames = self.late_frames.saturating_add(self.fifo.len() as u64);
            self.fifo.clear();
        }
        if self.anchored && self.fifo.len() > 0 {
            if new_wp >= self.write_pos {
                let gap = usize::try_from(new_wp.saturating_sub(self.write_pos)).unwrap_or(usize::MAX);
                if gap <= self.fifo.free() {
                    self.fifo.push_silence(gap);
                } else {
                    // A jump longer than the buffer: the unmixed audio cannot stay contiguous.
                    self.overruns = self.overruns.saturating_add(1);
                    self.fifo.clear();
                }
            } else {
                let back = usize::try_from(self.write_pos.saturating_sub(new_wp)).unwrap_or(usize::MAX);
                self.fifo.drop_back(back);
            }
        } else {
            self.fifo.clear();
        }
        self.write_pos = new_wp;
        self.anchored = true;
        self.ctl.reanchored();
        if let Some(rs) = self.resampler.as_mut() {
            rs.reset_with_offset(frac);
            rs.set_correction(self.ctl.correction());
        }
    }

    /// Renders frames `[read_pos, read_pos + frames)` of this source's input timeline through the
    /// filter chain into `self.buf[..2 * frames]`.
    pub(crate) fn render(&mut self, read_pos: i64, frames: usize) {
        let frames = frames.min(self.buf.len() / 2);
        self.next_read = Some(read_pos.saturating_add(frames as i64));
        let buf = &mut self.buf[..2 * frames];
        let len = self.fifo.len() as i64;
        let head = self.write_pos.saturating_sub(len);
        if head < read_pos {
            let stale = usize::try_from(read_pos.saturating_sub(head).min(len)).unwrap_or(0);
            self.fifo.drop_front(stale);
            self.late_frames = self.late_frames.saturating_add(stale as u64);
        }
        let len = self.fifo.len();
        let head = self.write_pos.saturating_sub(len as i64);
        let gap = usize::try_from(head.saturating_sub(read_pos).clamp(0, frames as i64)).unwrap_or(0);
        buf[..2 * gap].fill(0.0);
        let avail = len.min(frames - gap);
        self.fifo.pop_into(&mut buf[2 * gap..2 * (gap + avail)]);
        buf[2 * (gap + avail)..].fill(0.0);
        let missing = frames - gap - avail;
        if missing > 0 && self.settings.continuous && self.anchored {
            if !self.in_underrun {
                self.underruns = self.underruns.saturating_add(1);
                self.in_underrun = true;
            }
            self.underrun_frames = self.underrun_frames.saturating_add(missing as u64);
        } else if avail > 0 {
            self.in_underrun = false;
        }
        self.chain.process(buf);
        self.meter.accumulate(buf);
    }

    pub(crate) fn stats(&self) -> SourceStats {
        let pending = self.resampler.as_ref().map_or(0.0, Resampler::buffered_output_frames);
        SourceStats {
            id: self.id,
            name: self.name.clone(),
            buffered_ms: ((self.fifo.len() as f64 + pending) * 1000.0 / f64::from(OUTPUT_RATE)) as f32,
            underruns: self.underruns,
            underrun_frames: self.underrun_frames,
            overruns: self.overruns,
            discontinuities: self.discontinuities,
            gaps: self.gaps,
            late_frames: self.late_frames,
            rejected_chunks: self.rejected_chunks,
            drift_ppm: self.ctl.drift_ppm(),
            timeline_error_ms: self.ctl.error() * 1000.0,
            last_chunk_time: self.last_chunk_time,
            format: self.format,
        }
    }
}
