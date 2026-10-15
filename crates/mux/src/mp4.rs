//! Faststart MP4 writer (ISO/IEC 14496-12 ISO BMFF, 14496-14 MP4, 14496-15 AVC/HEVC file format).
//!
//! The whole sample table is known up front (remux from Matroska), so `moov` is written before `mdat`
//! and the file plays progressively. Chunk offsets always use `co64`, which makes the size of `moov`
//! independent of the offsets: it is built once to measure, then again with the final offsets.
//!
//! Timing: samples carry presentation times; there are no B-frames, so decode time = presentation time.
//! Constant-frame-rate video snaps times to an exact frame grid (90 kHz when the frame duration is an
//! integer there, else the rate numerator), variable-rate video uses 90 kHz and AAC uses the sample rate
//! with a 1024-sample grid. A track that starts later than the earliest track gets an edit list with an
//! empty edit, so A/V alignment survives.

use std::io::{BufWriter, Write};

use fluxcapture_time::{HNS_PER_SEC, Hns, rescale_round};

use crate::{AudioCodec, MuxError, TrackSpec, VideoCodec, validate_tracks};

/// One sample to write. Per track, samples must be in decode order (no B-frames: pts == dts).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Mp4Sample {
    /// 0-based index into the track list.
    pub track: usize,
    pub pts: Hns,
    pub keyframe: bool,
    pub size: u32,
}

const MOVIE_TIMESCALE: u32 = 1000;
const VFR_TIMESCALE: u32 = 90_000;
const AAC_FRAME: u64 = 1024;
const CHUNK_MAX_BYTES: u64 = 4 << 20;

fn too_large(what: &str) -> MuxError {
    MuxError::TooLarge(format!("MP4 {what} out of range"))
}

/// Exact timescale for a constant frame rate and the frame duration in it.
pub(crate) fn cfr_timescale(num: u32, den: u32) -> Option<(u32, u64)> {
    if num == 0 || den == 0 {
        return None;
    }
    let (num, den) = (u64::from(num), u64::from(den));
    if (90_000 * den).is_multiple_of(num) { Some((VFR_TIMESCALE, 90_000 * den / num)) } else { Some((u32::try_from(num).ok()?, den)) }
}

#[derive(Debug, Default)]
struct TrackPlan {
    timescale: u32,
    /// Indices into the caller's sample slice, in decode order.
    samples: Vec<usize>,
    /// Decode times in track ticks relative to the first sample.
    times: Vec<u64>,
    durations: Vec<u32>,
    media_duration: u64,
    /// pts of the first sample.
    start: Hns,
    /// (first sample index in `samples`, sample count, byte size)
    chunks: Vec<(usize, usize, u64)>,
    chunk_offsets: Vec<u64>,
    /// Empty edit before the media, in movie ticks.
    delay: u64,
    max_sample: u32,
    total_bytes: u64,
}

/// Snaps relative times (Hns) to a grid of `step` ticks at `ticks_per_sec`, keeping them strictly
/// increasing (computed per sample from its own time, never accumulated). Returns the times and how many had to be pushed forward.
fn grid_times(rel: &[i64], ticks_per_sec: i64, step: u64) -> Result<(Vec<u64>, usize), MuxError> {
    let mut out = Vec::with_capacity(rel.len());
    let mut pushed = 0usize;
    let mut prev: Option<u64> = None;
    let step_i = i64::try_from(step).map_err(|_| too_large("frame duration"))?;
    for &r in rel {
        let k = rescale_round(r.max(0), ticks_per_sec, HNS_PER_SEC.saturating_mul(step_i)).ok_or_else(|| too_large("timestamp"))?;
        let mut k = u64::try_from(k.max(0)).unwrap_or(0);
        if let Some(p) = prev
            && k <= p
        {
            k = p + 1;
            pushed += 1;
        }
        prev = Some(k);
        out.push(k.checked_mul(step).ok_or_else(|| too_large("timestamp"))?);
    }
    Ok((out, pushed))
}

/// Times for one track: (timescale, times, nominal duration of the last sample).
fn track_times(track: &TrackSpec, rel: &[i64]) -> Result<(u32, Vec<u64>, u64), MuxError> {
    match track {
        TrackSpec::Video(v) => {
            if let Some((ts, step)) = v.frame_rate.and_then(|r| cfr_timescale(r.num(), r.den())) {
                let (times, pushed) = grid_times(rel, i64::from(ts), step)?;
                // A stream that does not match its declared rate would drift if forced onto the grid.
                if pushed <= rel.len() / 100 + 1 {
                    return Ok((ts, times, step));
                }
                log::warn!("mp4: {pushed} of {} frames off the declared frame grid; writing variable frame rate", rel.len());
            }
            let (times, _) = grid_times(rel, i64::from(VFR_TIMESCALE), 1)?;
            let last = match times.as_slice() {
                [.., a, b] => b - a,
                _ => u64::from(VFR_TIMESCALE / 30),
            };
            Ok((VFR_TIMESCALE, times, last.max(1)))
        }
        TrackSpec::Audio(a) => {
            if a.codec != AudioCodec::Aac {
                return Err(MuxError::Unsupported("float PCM audio in MP4".into()));
            }
            let (times, _) = grid_times(rel, i64::from(a.sample_rate), AAC_FRAME)?;
            Ok((a.sample_rate, times, AAC_FRAME))
        }
    }
}

fn plan_tracks(tracks: &[TrackSpec], samples: &[Mp4Sample]) -> Result<Vec<TrackPlan>, MuxError> {
    let mut plans: Vec<TrackPlan> = tracks.iter().map(|_| TrackPlan::default()).collect();
    for (i, s) in samples.iter().enumerate() {
        let plan = plans.get_mut(s.track).ok_or_else(|| MuxError::InvalidData(format!("sample {i}: track {} does not exist", s.track)))?;
        if s.size == 0 {
            return Err(MuxError::InvalidData(format!("sample {i} is empty")));
        }
        plan.samples.push(i);
    }
    let origin = plans.iter().filter_map(|p| p.samples.first().and_then(|&i| samples.get(i)).map(|s| s.pts)).min().unwrap_or(Hns::ZERO);
    for (plan, track) in plans.iter_mut().zip(tracks) {
        if u32::try_from(plan.samples.len()).is_err() {
            return Err(too_large("sample count"));
        }
        let start = plan.samples.first().and_then(|&i| samples.get(i)).map_or(origin, |s| s.pts);
        let rel: Vec<i64> = plan.samples.iter().filter_map(|&i| samples.get(i)).map(|s| (s.pts - start).0).collect();
        let (ts, times, last) = track_times(track, &rel)?;
        plan.timescale = ts;
        plan.start = start;
        plan.durations = Vec::with_capacity(times.len());
        for w in times.windows(2) {
            let d = w.get(1).copied().unwrap_or(0).saturating_sub(w.first().copied().unwrap_or(0));
            plan.durations.push(u32::try_from(d).map_err(|_| too_large("sample duration"))?);
        }
        if !times.is_empty() {
            plan.durations.push(u32::try_from(last).map_err(|_| too_large("sample duration"))?);
        }
        plan.media_duration = times.last().map_or(0, |t| t.saturating_add(last));
        plan.delay = u64::try_from((start - origin).to_timescale(MOVIE_TIMESCALE).unwrap_or(0).max(0)).unwrap_or(0);
        plan.times = times;

        // Chunks of about half a second.
        let half = u64::from(ts / 2).max(1);
        let mut chunk_start: Option<u64> = None;
        let mut bytes = 0u64;
        for (k, &idx) in plan.samples.iter().enumerate() {
            let size = u64::from(samples.get(idx).map_or(0, |s| s.size));
            let t = plan.times.get(k).copied().unwrap_or(0);
            plan.max_sample = plan.max_sample.max(size as u32);
            plan.total_bytes = plan.total_bytes.saturating_add(size);
            match chunk_start {
                Some(t0) if t.saturating_sub(t0) < half && bytes + size <= CHUNK_MAX_BYTES => {
                    bytes += size;
                    if let Some(c) = plan.chunks.last_mut() {
                        c.1 += 1;
                        c.2 = bytes;
                    }
                }
                _ => {
                    chunk_start = Some(t);
                    bytes = size;
                    plan.chunks.push((k, 1, size));
                }
            }
        }
        if u32::try_from(plan.chunks.len()).is_err() {
            return Err(too_large("chunk count"));
        }
        plan.chunk_offsets = vec![0; plan.chunks.len()];
    }
    Ok(plans)
}

/// Global chunk order: by start time across tracks (ties by track index).
fn chunk_order(plans: &[TrackPlan]) -> Vec<(usize, usize)> {
    let mut order: Vec<(Hns, usize, usize)> = Vec::new();
    for (ti, p) in plans.iter().enumerate() {
        for (ci, &(first, _, _)) in p.chunks.iter().enumerate() {
            let t = p.times.get(first).copied().unwrap_or(0);
            let rel = Hns::from_timescale(i64::try_from(t).unwrap_or(i64::MAX), p.timescale).unwrap_or(Hns::MAX);
            order.push((p.start.saturating_add(rel), ti, ci));
        }
    }
    order.sort_by_key(|&(t, ti, ci)| (t, ti, ci));
    order.into_iter().map(|(_, ti, ci)| (ti, ci)).collect()
}

// ---- box building -------------------------------------------------------------------------------

fn open(buf: &mut Vec<u8>, typ: &[u8; 4]) -> usize {
    let start = buf.len();
    buf.extend_from_slice(&[0, 0, 0, 0]);
    buf.extend_from_slice(typ);
    start
}

fn close(buf: &mut [u8], start: usize) -> Result<(), MuxError> {
    let len = u32::try_from(buf.len() - start).map_err(|_| too_large("box"))?;
    if let Some(s) = buf.get_mut(start..start + 4) {
        s.copy_from_slice(&len.to_be_bytes());
    }
    Ok(())
}

fn full(buf: &mut Vec<u8>, version: u8, flags: u32) {
    buf.push(version);
    buf.extend_from_slice(&flags.to_be_bytes()[1..]);
}

fn u16be(buf: &mut Vec<u8>, v: u16) {
    buf.extend_from_slice(&v.to_be_bytes());
}

fn u32be(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_be_bytes());
}

fn u64be(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_be_bytes());
}

const MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

fn matrix(buf: &mut Vec<u8>) {
    for v in MATRIX {
        u32be(buf, v);
    }
}

/// `version 0` with 32-bit time fields when everything fits, else `version 1`.
fn put_times(buf: &mut Vec<u8>, v1: bool, duration: u64) {
    if v1 {
        u64be(buf, duration);
    } else {
        u32be(buf, duration as u32);
    }
}

fn rescale_u64(v: u64, to: u32, from: u32) -> Result<u64, MuxError> {
    let v = i64::try_from(v).map_err(|_| too_large("duration"))?;
    let r = rescale_round(v, i64::from(to), i64::from(from)).ok_or_else(|| too_large("duration"))?;
    u64::try_from(r).map_err(|_| too_large("duration"))
}

/// Descriptor with an expandable size field (ISO/IEC 14496-1 §8.3.3).
fn descriptor(buf: &mut Vec<u8>, tag: u8, payload: &[u8]) -> Result<(), MuxError> {
    let len = payload.len();
    if len >= 1 << 28 {
        return Err(too_large("descriptor"));
    }
    buf.push(tag);
    let mut groups = 1;
    while groups < 4 && len >> (7 * groups) != 0 {
        groups += 1;
    }
    for g in (0..groups).rev() {
        let b = ((len >> (7 * g)) & 0x7F) as u8;
        buf.push(if g > 0 { b | 0x80 } else { b });
    }
    buf.extend_from_slice(payload);
    Ok(())
}

fn esds(buf: &mut Vec<u8>, track_id: u16, asc: &[u8], plan: &TrackPlan, samples: &[Mp4Sample]) -> Result<(), MuxError> {
    let ts = u64::from(plan.timescale.max(1));
    let avg = (u128::from(plan.total_bytes) * 8 * u128::from(ts) / u128::from(plan.media_duration.max(1))).min(u128::from(u32::MAX)) as u32;
    // Peak bit rate over one-second buckets.
    let mut peak = 0u64;
    let mut bucket = (0u64, 0u64);
    for (k, &i) in plan.samples.iter().enumerate() {
        let sec = plan.times.get(k).copied().unwrap_or(0) / ts;
        if sec != bucket.0 {
            bucket = (sec, 0);
        }
        bucket.1 = bucket.1.saturating_add(u64::from(samples.get(i).map_or(0, |s| s.size)));
        peak = peak.max(bucket.1);
    }
    let max_bitrate = u32::try_from(peak.saturating_mul(8)).unwrap_or(u32::MAX).max(avg);

    let mut dsi = Vec::new();
    descriptor(&mut dsi, 0x05, asc)?;
    let mut dcd = Vec::with_capacity(13 + dsi.len());
    dcd.push(0x40); // objectTypeIndication: MPEG-4 Audio
    dcd.push((5 << 2) | 1); // streamType 5 (audio), upStream 0, reserved 1
    let buffer = plan.max_sample.min(0x00FF_FFFF);
    dcd.extend_from_slice(&buffer.to_be_bytes()[1..]);
    u32be(&mut dcd, max_bitrate);
    u32be(&mut dcd, avg);
    dcd.extend_from_slice(&dsi);
    let mut es = Vec::with_capacity(8 + dcd.len());
    u16be(&mut es, track_id);
    es.push(0); // no dependency, URL or OCR stream
    descriptor(&mut es, 0x04, &dcd)?;
    descriptor(&mut es, 0x06, &[0x02])?; // SLConfigDescriptor: predefined = 2 (MP4)
    let b = open(buf, b"esds");
    full(buf, 0, 0);
    descriptor(buf, 0x03, &es)?;
    close(buf, b)
}

fn sample_entry(buf: &mut Vec<u8>, track: &TrackSpec, track_id: u16, plan: &TrackPlan, samples: &[Mp4Sample]) -> Result<(), MuxError> {
    match track {
        TrackSpec::Video(v) => {
            let (fourcc, config): (&[u8; 4], &[u8; 4]) = match v.codec {
                VideoCodec::H264 => (b"avc1", b"avcC"),
                VideoCodec::Hevc => (b"hvc1", b"hvcC"),
            };
            let e = open(buf, fourcc);
            buf.extend_from_slice(&[0; 6]);
            u16be(buf, 1); // data_reference_index
            buf.extend_from_slice(&[0; 16]); // pre_defined, reserved, pre_defined[3]
            u16be(buf, u16::try_from(v.width).map_err(|_| too_large("width"))?);
            u16be(buf, u16::try_from(v.height).map_err(|_| too_large("height"))?);
            u32be(buf, 0x0048_0000); // 72 dpi
            u32be(buf, 0x0048_0000);
            u32be(buf, 0);
            u16be(buf, 1); // frame_count
            buf.extend_from_slice(&[0; 32]); // compressorname
            u16be(buf, 0x0018); // depth
            u16be(buf, 0xFFFF); // pre_defined = -1
            let c = open(buf, config);
            buf.extend_from_slice(&v.codec_private);
            close(buf, c)?;
            let c = open(buf, b"colr");
            buf.extend_from_slice(b"nclx");
            u16be(buf, 1); // BT.709 primaries
            u16be(buf, 1); // BT.709 transfer
            u16be(buf, 1); // BT.709 matrix
            buf.push(if v.color.full_range { 0x80 } else { 0 });
            close(buf, c)?;
            close(buf, e)
        }
        TrackSpec::Audio(a) => {
            let e = open(buf, b"mp4a");
            buf.extend_from_slice(&[0; 6]);
            u16be(buf, 1); // data_reference_index
            buf.extend_from_slice(&[0; 8]);
            u16be(buf, a.channels);
            u16be(buf, 16); // samplesize
            u32be(buf, 0); // pre_defined, reserved
            // 16.16 sample rate; rates above 65535 Hz do not fit and decoders use the ASC anyway.
            u32be(buf, if a.sample_rate <= 0xFFFF { a.sample_rate << 16 } else { 0 });
            esds(buf, track_id, &a.codec_private, plan, samples)?;
            close(buf, e)
        }
    }
}

fn stbl(buf: &mut Vec<u8>, track: &TrackSpec, track_id: u16, plan: &TrackPlan, samples: &[Mp4Sample]) -> Result<(), MuxError> {
    let st = open(buf, b"stbl");

    let b = open(buf, b"stsd");
    full(buf, 0, 0);
    u32be(buf, 1);
    sample_entry(buf, track, track_id, plan, samples)?;
    close(buf, b)?;

    // stts: run-length (count, delta).
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for &d in &plan.durations {
        match runs.last_mut() {
            Some((n, delta)) if *delta == d => *n += 1,
            _ => runs.push((1, d)),
        }
    }
    let b = open(buf, b"stts");
    full(buf, 0, 0);
    u32be(buf, runs.len() as u32);
    for (n, d) in runs {
        u32be(buf, n);
        u32be(buf, d);
    }
    close(buf, b)?;

    if track.is_video() {
        let keys: Vec<u32> = plan.samples.iter().enumerate().filter(|&(_, &i)| samples.get(i).is_some_and(|s| s.keyframe)).map(|(k, _)| k as u32 + 1).collect();
        if keys.len() != plan.samples.len() {
            let b = open(buf, b"stss");
            full(buf, 0, 0);
            u32be(buf, keys.len() as u32);
            for k in keys {
                u32be(buf, k);
            }
            close(buf, b)?;
        }
    }

    let b = open(buf, b"stsz");
    full(buf, 0, 0);
    u32be(buf, 0); // sizes follow
    u32be(buf, plan.samples.len() as u32);
    for &i in &plan.samples {
        u32be(buf, samples.get(i).map_or(0, |s| s.size));
    }
    close(buf, b)?;

    let mut stsc: Vec<(u32, u32)> = Vec::new();
    for (ci, &(_, count, _)) in plan.chunks.iter().enumerate() {
        if stsc.last().is_none_or(|&(_, n)| n != count as u32) {
            stsc.push((ci as u32 + 1, count as u32));
        }
    }
    let b = open(buf, b"stsc");
    full(buf, 0, 0);
    u32be(buf, stsc.len() as u32);
    for (first, n) in stsc {
        u32be(buf, first);
        u32be(buf, n);
        u32be(buf, 1); // sample_description_index
    }
    close(buf, b)?;

    let b = open(buf, b"co64");
    full(buf, 0, 0);
    u32be(buf, plan.chunk_offsets.len() as u32);
    for &o in &plan.chunk_offsets {
        u64be(buf, o);
    }
    close(buf, b)?;

    close(buf, st)
}

fn trak(
    buf: &mut Vec<u8>,
    track: &TrackSpec,
    index: usize,
    plan: &TrackPlan,
    samples: &[Mp4Sample],
    enabled: bool,
    alternate_group: u16,
) -> Result<u64, MuxError> {
    let track_id = u16::try_from(index + 1).map_err(|_| too_large("track count"))?;
    let media_mv = rescale_u64(plan.media_duration, MOVIE_TIMESCALE, plan.timescale.max(1))?;
    let tkhd_duration = plan.delay.saturating_add(media_mv);
    let t = open(buf, b"trak");

    let v1 = tkhd_duration > u64::from(u32::MAX);
    let b = open(buf, b"tkhd");
    full(buf, u8::from(v1), if enabled { 0x3 } else { 0x2 }); // track_enabled | track_in_movie
    if v1 {
        u64be(buf, 0);
        u64be(buf, 0);
    } else {
        u32be(buf, 0);
        u32be(buf, 0);
    }
    u32be(buf, u32::from(track_id));
    u32be(buf, 0);
    put_times(buf, v1, tkhd_duration);
    buf.extend_from_slice(&[0; 8]);
    u16be(buf, 0); // layer
    u16be(buf, alternate_group);
    u16be(buf, if track.is_video() { 0 } else { 0x0100 }); // volume
    u16be(buf, 0);
    matrix(buf);
    let (w, h) = match track {
        TrackSpec::Video(v) => (v.width, v.height),
        TrackSpec::Audio(_) => (0, 0),
    };
    u32be(buf, w << 16);
    u32be(buf, h << 16);
    close(buf, b)?;

    if plan.delay > 0 && !plan.samples.is_empty() {
        let v1 = plan.delay > i32::MAX as u64 || media_mv > u64::from(u32::MAX);
        let e = open(buf, b"edts");
        let b = open(buf, b"elst");
        full(buf, u8::from(v1), 0);
        u32be(buf, 2);
        for (dur, media_time) in [(plan.delay, -1i64), (media_mv, 0)] {
            if v1 {
                u64be(buf, dur);
                buf.extend_from_slice(&media_time.to_be_bytes());
            } else {
                u32be(buf, dur as u32);
                buf.extend_from_slice(&(media_time as i32).to_be_bytes());
            }
            u16be(buf, 1); // media_rate_integer
            u16be(buf, 0);
        }
        close(buf, b)?;
        close(buf, e)?;
    }

    let m = open(buf, b"mdia");
    let v1 = plan.media_duration > u64::from(u32::MAX);
    let b = open(buf, b"mdhd");
    full(buf, u8::from(v1), 0);
    if v1 {
        u64be(buf, 0);
        u64be(buf, 0);
    } else {
        u32be(buf, 0);
        u32be(buf, 0);
    }
    u32be(buf, plan.timescale);
    put_times(buf, v1, plan.media_duration);
    u16be(buf, 0x55C4); // language "und"
    u16be(buf, 0);
    close(buf, b)?;

    let b = open(buf, b"hdlr");
    full(buf, 0, 0);
    u32be(buf, 0);
    buf.extend_from_slice(if track.is_video() { b"vide" } else { b"soun" });
    buf.extend_from_slice(&[0; 12]);
    let name = match track.name() {
        "" if track.is_video() => "VideoHandler",
        "" => "SoundHandler",
        n => n,
    };
    buf.extend_from_slice(name.replace('\0', " ").as_bytes());
    buf.push(0);
    close(buf, b)?;

    let mi = open(buf, b"minf");
    if track.is_video() {
        let b = open(buf, b"vmhd");
        full(buf, 0, 1);
        buf.extend_from_slice(&[0; 8]);
        close(buf, b)?;
    } else {
        let b = open(buf, b"smhd");
        full(buf, 0, 0);
        buf.extend_from_slice(&[0; 4]);
        close(buf, b)?;
    }
    let d = open(buf, b"dinf");
    let r = open(buf, b"dref");
    full(buf, 0, 0);
    u32be(buf, 1);
    let u = open(buf, b"url ");
    full(buf, 0, 1); // media data is in this file
    close(buf, u)?;
    close(buf, r)?;
    close(buf, d)?;
    stbl(buf, track, track_id, plan, samples)?;
    close(buf, mi)?;
    close(buf, m)?;
    close(buf, t)?;
    Ok(tkhd_duration)
}

fn moov(tracks: &[TrackSpec], plans: &[TrackPlan], samples: &[Mp4Sample]) -> Result<Vec<u8>, MuxError> {
    let mut traks = Vec::with_capacity(4096);
    let audio_count = tracks.iter().filter(|t| !t.is_video()).count();
    let first_default_audio = tracks.iter().position(|t| matches!(t, TrackSpec::Audio(a) if a.default)).or_else(|| tracks.iter().position(|t| !t.is_video()));
    let mut movie_duration = 0u64;
    for (i, (track, plan)) in tracks.iter().zip(plans).enumerate() {
        // Players treat tracks of one alternate group as alternatives and pick the enabled one.
        let (enabled, group) = match track {
            TrackSpec::Video(_) => (true, 0),
            TrackSpec::Audio(_) => (Some(i) == first_default_audio, if audio_count > 1 { 1 } else { 0 }),
        };
        movie_duration = movie_duration.max(trak(&mut traks, track, i, plan, samples, enabled, group)?);
    }
    let mut buf = Vec::with_capacity(traks.len() + 128);
    let m = open(&mut buf, b"moov");
    let v1 = movie_duration > u64::from(u32::MAX);
    let b = open(&mut buf, b"mvhd");
    full(&mut buf, u8::from(v1), 0);
    if v1 {
        u64be(&mut buf, 0);
        u64be(&mut buf, 0);
    } else {
        u32be(&mut buf, 0);
        u32be(&mut buf, 0);
    }
    u32be(&mut buf, MOVIE_TIMESCALE);
    put_times(&mut buf, v1, movie_duration);
    u32be(&mut buf, 0x0001_0000); // rate 1.0
    u16be(&mut buf, 0x0100); // volume 1.0
    buf.extend_from_slice(&[0; 10]);
    matrix(&mut buf);
    buf.extend_from_slice(&[0; 24]); // pre_defined
    u32be(&mut buf, tracks.len() as u32 + 1); // next_track_ID
    close(&mut buf, b)?;
    buf.extend_from_slice(&traks);
    close(&mut buf, m)?;
    Ok(buf)
}

fn ftyp(tracks: &[TrackSpec]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(32);
    let b = open(&mut buf, b"ftyp");
    buf.extend_from_slice(b"isom");
    u32be(&mut buf, 512);
    buf.extend_from_slice(b"isomiso2");
    if tracks.iter().any(|t| matches!(t, TrackSpec::Video(v) if v.codec == VideoCodec::H264)) {
        buf.extend_from_slice(b"avc1");
    }
    if tracks.iter().any(|t| matches!(t, TrackSpec::Video(v) if v.codec == VideoCodec::Hevc)) {
        buf.extend_from_slice(b"hvc1");
    }
    buf.extend_from_slice(b"mp41");
    let _ = close(&mut buf, b);
    buf
}

/// Writes a faststart MP4 (`ftyp`, `moov`, `mdat`) and returns the number of bytes written.
///
/// `fetch(i, buf)` must fill `buf` (cleared by the callee or not — it is overwritten) with the payload of
/// `samples[i]`, exactly `samples[i].size` bytes; video payloads are length-prefixed NAL units. Samples
/// are fetched in file (interleaved chunk) order, roughly the order they appear in a Matroska source.
/// Float PCM tracks are refused with [`MuxError::Unsupported`].
pub fn write_mp4<W: Write>(
    out: W,
    tracks: &[TrackSpec],
    samples: &[Mp4Sample],
    fetch: &mut dyn FnMut(usize, &mut Vec<u8>) -> std::io::Result<()>,
) -> Result<u64, MuxError> {
    validate_tracks(tracks)?;
    if let Some(i) = tracks.iter().position(|t| matches!(t, TrackSpec::Audio(a) if a.codec == AudioCodec::PcmF32)) {
        return Err(MuxError::Unsupported(format!("track {i}: float PCM audio cannot be stored in MP4")));
    }
    let mut plans = plan_tracks(tracks, samples)?;
    let order = chunk_order(&plans);

    let ftyp = ftyp(tracks);
    let moov_len = moov(tracks, &plans, samples)?.len() as u64;
    let payload: u64 = plans.iter().map(|p| p.total_bytes).sum();
    let data_start = ftyp.len() as u64 + moov_len + 16;
    let mut offset = data_start;
    for &(ti, ci) in &order {
        if let Some(p) = plans.get_mut(ti) {
            let bytes = p.chunks.get(ci).map_or(0, |c| c.2);
            if let Some(o) = p.chunk_offsets.get_mut(ci) {
                *o = offset;
            }
            offset = offset.checked_add(bytes).ok_or_else(|| too_large("file size"))?;
        }
    }
    let moov = moov(tracks, &plans, samples)?;
    if moov.len() as u64 != moov_len {
        return Err(MuxError::InvalidData("internal error: moov size changed".into()));
    }

    let mut w = BufWriter::with_capacity(1 << 20, out);
    w.write_all(&ftyp)?;
    w.write_all(&moov)?;
    w.write_all(&1u32.to_be_bytes())?;
    w.write_all(b"mdat")?;
    w.write_all(&(16 + payload).to_be_bytes())?;
    let mut buf = Vec::new();
    for &(ti, ci) in &order {
        let Some(p) = plans.get(ti) else { continue };
        let Some(&(first, count, _)) = p.chunks.get(ci) else { continue };
        for &idx in p.samples.iter().skip(first).take(count) {
            buf.clear();
            fetch(idx, &mut buf)?;
            let want = samples.get(idx).map_or(0, |s| s.size as usize);
            if buf.len() != want {
                return Err(MuxError::InvalidData(format!("sample {idx}: fetched {} bytes, expected {want}", buf.len())));
            }
            w.write_all(&buf)?;
        }
    }
    w.flush()?;
    Ok(data_start + payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AudioTrack, ColorInfo, VideoTrack};
    use fluxcapture_time::FrameRate;

    #[test]
    fn cfr_timescales_are_exact() {
        assert_eq!(cfr_timescale(30, 1), Some((90_000, 3000)));
        assert_eq!(cfr_timescale(30_000, 1001), Some((90_000, 3003)));
        assert_eq!(cfr_timescale(60_000, 1001), Some((60_000, 1001)));
        assert_eq!(cfr_timescale(144, 1), Some((90_000, 625)));
        assert_eq!(cfr_timescale(7, 1), Some((7, 1)));
        assert_eq!(cfr_timescale(0, 1), None);
    }

    #[test]
    fn grid_snapping_is_monotonic_and_drift_free() {
        let rate = FrameRate::NTSC_30;
        let rel: Vec<i64> = (0..1000).map(|k| rate.frame_time(k).0 + if k % 2 == 0 { 3 } else { -3 }).collect();
        let (t, pushed) = grid_times(&rel, 90_000, 3003).unwrap();
        assert_eq!(pushed, 0);
        assert!(t.iter().enumerate().all(|(k, &v)| v == k as u64 * 3003));
        // Duplicates are pushed to the next slot.
        let (t, pushed) = grid_times(&[0, 0, 0, 5_000_000], 48_000, 1024).unwrap();
        assert_eq!(t, vec![0, 1024, 2048, 23 * 1024]);
        assert_eq!(pushed, 2);
    }

    fn video(rate: Option<FrameRate>) -> TrackSpec {
        TrackSpec::Video(VideoTrack {
            codec: VideoCodec::H264,
            width: 64,
            height: 32,
            frame_rate: rate,
            codec_private: vec![1, 100, 0, 30, 0xFF, 0xE0, 0],
            color: ColorInfo::default(),
            name: String::new(),
        })
    }

    fn aac() -> TrackSpec {
        TrackSpec::Audio(AudioTrack {
            codec: AudioCodec::Aac,
            sample_rate: 48_000,
            channels: 2,
            codec_private: vec![0x11, 0x90],
            name: "Mix".into(),
            default: true,
        })
    }

    #[test]
    fn plans_stts_chunks_and_edit_delay() {
        let tracks = vec![video(Some(FrameRate::FPS_30)), aac()];
        let mut samples = Vec::new();
        for k in 0..60 {
            samples.push(Mp4Sample { track: 0, pts: FrameRate::FPS_30.frame_time(k), keyframe: k % 30 == 0, size: 100 });
        }
        for k in 0..90 {
            // Audio starts 20 ms late.
            let pts = Hns::from_millis(20) + Hns(k * 1024 * HNS_PER_SEC / 48_000);
            samples.push(Mp4Sample { track: 1, pts, keyframe: true, size: 10 });
        }
        let plans = plan_tracks(&tracks, &samples).unwrap();
        let v = &plans[0];
        assert_eq!(v.timescale, 90_000);
        assert!(v.durations.iter().all(|&d| d == 3000));
        assert_eq!(v.media_duration, 60 * 3000);
        assert_eq!(v.delay, 0);
        assert_eq!(v.chunks.len(), 4); // 15 frames per half second
        assert!(v.chunks.iter().all(|c| c.1 == 15));
        let a = &plans[1];
        assert_eq!(a.timescale, 48_000);
        assert!(a.durations.iter().all(|&d| d == 1024));
        assert_eq!(a.delay, 20);
        let order = chunk_order(&plans);
        assert_eq!(order.len(), plans[0].chunks.len() + plans[1].chunks.len());
        assert_eq!(order[0], (0, 0));
        assert_eq!(order[1], (1, 0));

        let mut fetched = 0;
        let mut out = Vec::new();
        let n = write_mp4(&mut out, &tracks, &samples, &mut |i, buf| {
            fetched += 1;
            buf.resize(samples[i].size as usize, 0xAB);
            Ok(())
        })
        .unwrap();
        assert_eq!(n as usize, out.len());
        assert_eq!(fetched, samples.len());
        assert_eq!(&out[4..8], b"ftyp");
    }

    #[test]
    fn vfr_and_refusals() {
        let tracks = vec![video(None)];
        let samples: Vec<Mp4Sample> = [0, 10, 25, 70].iter().map(|&ms| Mp4Sample { track: 0, pts: Hns::from_millis(ms), keyframe: ms == 0, size: 5 }).collect();
        let plans = plan_tracks(&tracks, &samples).unwrap();
        assert_eq!(plans[0].durations, vec![900, 1350, 4050, 4050]);
        let pcm = TrackSpec::Audio(AudioTrack {
            codec: AudioCodec::PcmF32,
            sample_rate: 48_000,
            channels: 2,
            codec_private: vec![],
            name: String::new(),
            default: true,
        });
        assert!(matches!(write_mp4(Vec::new(), &[pcm], &[], &mut |_, _| Ok(())), Err(MuxError::Unsupported(_))));
        let bad = [Mp4Sample { track: 3, pts: Hns::ZERO, keyframe: true, size: 1 }];
        assert!(write_mp4(Vec::new(), &tracks, &bad, &mut |_, _| Ok(())).is_err());
        // Fetch returning the wrong size is an error, not a corrupt file.
        assert!(
            write_mp4(Vec::new(), &tracks, &samples, &mut |_, b| {
                b.push(1);
                Ok(())
            })
            .is_err()
        );
    }

    #[test]
    fn descriptor_sizes() {
        let mut b = Vec::new();
        descriptor(&mut b, 5, &[1, 2]).unwrap();
        assert_eq!(b, [5, 2, 1, 2]);
        let mut b = Vec::new();
        descriptor(&mut b, 5, &[0; 200]).unwrap();
        assert_eq!(&b[..3], &[5, 0x81, 0x48]);
    }
}
