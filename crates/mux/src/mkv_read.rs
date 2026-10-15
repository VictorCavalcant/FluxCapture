//! Tolerant Matroska scanner for recovery and remux.
//!
//! [`scan_mkv`] walks a file once and returns its tracks plus the position of every frame, without
//! loading payloads. It is written for files [`crate::MkvWriter`] produced (possibly cut short by a
//! crash) and handles simple foreign files too: unknown-size Segment/Clusters, a truncated tail (it stops
//! at the last complete block), SimpleBlock and BlockGroup, Void/CRC-32/unknown elements and 1–8 byte IDs
//! and sizes. Input is hostile: sizes are checked against the file length, allocations are capped and
//! nothing panics.

use std::io::{self, BufReader, Read, Seek, SeekFrom};

use fluxcapture_time::{FrameRate, Hns, SampleClock};

use crate::ebml::{id, read_vint};
use crate::{AudioCodec, AudioTrack, ColorInfo, MAX_CODEC_PRIVATE, MAX_DIMENSION, MAX_SAMPLE_RATE, MAX_TRACKS, MuxError, TrackSpec, VideoCodec, VideoTrack};

/// Largest EBML header / Info element accepted.
const MAX_SMALL_MASTER: u64 = 1 << 20;
/// Largest Tracks element accepted.
const MAX_TRACKS_ELEMENT: u64 = 16 << 20;
/// Longest string (CodecID, Name) kept; longer ones are truncated.
const MAX_STRING: u64 = 4096;

/// One frame found by [`scan_mkv`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct FrameRef {
    /// 0-based index into [`MkvIndex::tracks`].
    pub track: usize,
    pub pts: Hns,
    pub keyframe: bool,
    /// Absolute file offset of the frame payload.
    pub offset: u64,
    pub size: u32,
}

/// Result of [`scan_mkv`].
#[derive(Clone, Debug, PartialEq)]
pub struct MkvIndex {
    /// Supported tracks (H.264, HEVC, AAC, float PCM) in file order; other tracks are skipped.
    pub tracks: Vec<TrackSpec>,
    /// Frames in file order.
    pub frames: Vec<FrameRef>,
    pub duration: Hns,
    /// The Segment has a known size that fits the file and Cues are present (the file was finished).
    pub finalized: bool,
    /// Offset just past the last complete element parsed (block granularity inside a truncated
    /// cluster); bytes after it were not usable.
    pub valid_until: u64,
}

struct Reader<R> {
    r: BufReader<R>,
    pos: u64,
    len: u64,
}

impl<R: Read + Seek> Reader<R> {
    fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
        let end = self.pos.checked_add(buf.len() as u64).ok_or(io::ErrorKind::UnexpectedEof)?;
        if end > self.len {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        self.r.read_exact(buf)?;
        self.pos = end;
        Ok(())
    }

    fn seek_to(&mut self, p: u64) -> io::Result<()> {
        if p == self.pos {
            return Ok(());
        }
        match p.checked_sub(self.pos).and_then(|d| i64::try_from(d).ok()) {
            Some(d) => self.r.seek_relative(d)?,
            None => {
                self.r.seek(SeekFrom::Start(p))?;
            }
        }
        self.pos = p;
        Ok(())
    }

    /// Reads a vint (ID when `keep_marker`, else size) of 1–8 bytes.
    fn read_vint(&mut self, keep_marker: bool) -> Result<(u64, bool), HdrErr> {
        let mut buf = [0u8; 8];
        self.read_exact(&mut buf[..1]).map_err(HdrErr::from)?;
        let len = buf[0].leading_zeros() as usize + 1;
        if len > 8 {
            return Err(HdrErr::Invalid);
        }
        self.read_exact(&mut buf[1..len]).map_err(HdrErr::from)?;
        let (v, _, unknown) = read_vint(&buf[..len]).ok_or(HdrErr::Invalid)?;
        if keep_marker {
            let mut raw = 0u64;
            for &b in &buf[..len] {
                raw = (raw << 8) | u64::from(b);
            }
            Ok((raw, false))
        } else {
            Ok((v, unknown))
        }
    }

    fn read_header(&mut self) -> Result<Hdr, HdrErr> {
        let (id, _) = self.read_vint(true)?;
        let (size, unknown) = self.read_vint(false)?;
        Ok(Hdr { id, size: (!unknown).then_some(size), data_start: self.pos })
    }

    fn read_uint(&mut self, size: u64) -> Result<u64, MuxError> {
        if size > 8 {
            return Err(MuxError::InvalidData(format!("{size}-byte integer")));
        }
        let mut buf = [0u8; 8];
        let n = size as usize;
        self.read_exact(&mut buf[8 - n..])?;
        Ok(u64::from_be_bytes(buf))
    }

    fn read_float(&mut self, size: u64) -> Result<f64, MuxError> {
        match size {
            0 => Ok(0.0),
            4 => {
                let mut b = [0u8; 4];
                self.read_exact(&mut b)?;
                Ok(f64::from(f32::from_be_bytes(b)))
            }
            8 => {
                let mut b = [0u8; 8];
                self.read_exact(&mut b)?;
                Ok(f64::from_be_bytes(b))
            }
            _ => Err(MuxError::InvalidData(format!("{size}-byte float"))),
        }
    }

    /// Reads up to `cap` bytes of a string element (the rest is skipped by the caller).
    fn read_string(&mut self, size: u64) -> Result<String, MuxError> {
        let mut b = vec![0u8; size.min(MAX_STRING) as usize];
        self.read_exact(&mut b)?;
        if let Some(nul) = b.iter().position(|&c| c == 0) {
            b.truncate(nul);
        }
        Ok(String::from_utf8_lossy(&b).into_owned())
    }
}

#[derive(Copy, Clone, Debug)]
struct Hdr {
    id: u64,
    /// `None` = unknown size.
    size: Option<u64>,
    data_start: u64,
}

#[derive(Debug)]
enum HdrErr {
    Eof,
    Invalid,
    Io(io::Error),
}

impl From<io::Error> for HdrErr {
    fn from(e: io::Error) -> Self {
        if e.kind() == io::ErrorKind::UnexpectedEof { HdrErr::Eof } else { HdrErr::Io(e) }
    }
}

fn is_eof(e: &MuxError) -> bool {
    matches!(e, MuxError::Io(io) if io.kind() == io::ErrorKind::UnexpectedEof)
}

/// Calls `f` for each child of a master element spanning up to `end`, then skips to the child's end.
/// Children must have known sizes that fit inside the parent.
fn for_each_child<R: Read + Seek>(rd: &mut Reader<R>, end: u64, mut f: impl FnMut(&mut Reader<R>, Hdr, u64) -> Result<(), MuxError>) -> Result<(), MuxError> {
    while rd.pos < end {
        let h = rd.read_header().map_err(|e| match e {
            HdrErr::Io(e) => MuxError::Io(e),
            _ => MuxError::InvalidData("malformed or truncated element header".into()),
        })?;
        let size = h.size.ok_or_else(|| MuxError::InvalidData("unknown-size element inside a header element".into()))?;
        let child_end = h.data_start.checked_add(size).filter(|&e| e <= end).ok_or_else(|| MuxError::InvalidData("element overruns its parent".into()))?;
        f(rd, h, child_end)?;
        rd.seek_to(child_end)?;
    }
    Ok(())
}

#[derive(Debug)]
struct RawTrack {
    number: u64,
    track_type: u64,
    codec_id: String,
    codec_private: Vec<u8>,
    codec_private_too_large: bool,
    name: String,
    flag_default: bool,
    default_duration: Option<u64>,
    width: u64,
    height: u64,
    full_range: bool,
    sampling_frequency: f64,
    channels: u64,
    bit_depth: u64,
    encoded: bool,
}

impl Default for RawTrack {
    fn default() -> Self {
        // Matroska defaults (RFC 9559 §5.1.4).
        RawTrack {
            number: 0,
            track_type: 0,
            codec_id: String::new(),
            codec_private: Vec::new(),
            codec_private_too_large: false,
            name: String::new(),
            flag_default: true,
            default_duration: None,
            width: 0,
            height: 0,
            full_range: false,
            sampling_frequency: 8000.0,
            channels: 1,
            bit_depth: 0,
            encoded: false,
        }
    }
}

/// Maps a `DefaultDuration` back to a frame rate (exact for the rates [`FrameRate::frame_duration_ns`]
/// produces, otherwise the nearest integer rate within 0.1%).
fn rate_from_default_duration(ns: u64) -> Option<FrameRate> {
    if ns == 0 {
        return None;
    }
    if let Some(r) = FrameRate::COMMON.iter().find(|r| r.frame_duration_ns() == ns) {
        return Some(*r);
    }
    let fps = 1e9 / ns as f64;
    let rounded = fps.round();
    if rounded >= 1.0 && (fps - rounded).abs() < rounded * 1e-3 { FrameRate::from_fps(rounded as u32).ok() } else { None }
}

impl RawTrack {
    fn into_spec(self) -> Option<TrackSpec> {
        if self.encoded || self.codec_private_too_large {
            log::warn!("mkv scan: skipping track {} (content encoding or oversized CodecPrivate)", self.number);
            return None;
        }
        match (self.track_type, self.codec_id.as_str()) {
            (1, "V_MPEG4/ISO/AVC" | "V_MPEGH/ISO/HEVC") => {
                let codec = if self.codec_id == "V_MPEG4/ISO/AVC" { VideoCodec::H264 } else { VideoCodec::Hevc };
                let width = u32::try_from(self.width).ok().filter(|w| (1..=MAX_DIMENSION).contains(w))?;
                let height = u32::try_from(self.height).ok().filter(|h| (1..=MAX_DIMENSION).contains(h))?;
                Some(TrackSpec::Video(VideoTrack {
                    codec,
                    width,
                    height,
                    frame_rate: self.default_duration.and_then(rate_from_default_duration),
                    codec_private: self.codec_private,
                    color: ColorInfo { full_range: self.full_range },
                    name: self.name,
                }))
            }
            (2, codec_id) => {
                let codec = if codec_id == "A_AAC" || codec_id.starts_with("A_AAC/") {
                    AudioCodec::Aac
                } else if codec_id == "A_PCM/IEEE_FLOAT" && self.bit_depth == 32 {
                    AudioCodec::PcmF32
                } else {
                    return None;
                };
                let f = self.sampling_frequency;
                if !f.is_finite() || f < 1.0 || f > f64::from(MAX_SAMPLE_RATE) {
                    return None;
                }
                let channels = u16::try_from(self.channels).ok().filter(|c| (1..=64).contains(c))?;
                Some(TrackSpec::Audio(AudioTrack {
                    codec,
                    sample_rate: f.round() as u32,
                    channels,
                    codec_private: self.codec_private,
                    name: self.name,
                    default: self.flag_default,
                }))
            }
            _ => None,
        }
    }
}

fn parse_track_entry<R: Read + Seek>(rd: &mut Reader<R>, end: u64) -> Result<RawTrack, MuxError> {
    let mut t = RawTrack::default();
    for_each_child(rd, end, |rd, h, child_end| {
        let size = child_end - h.data_start;
        match h.id {
            id::TRACK_NUMBER => t.number = rd.read_uint(size)?,
            id::TRACK_TYPE => t.track_type = rd.read_uint(size)?,
            id::CODEC_ID => t.codec_id = rd.read_string(size)?,
            id::NAME => t.name = rd.read_string(size)?,
            id::FLAG_DEFAULT => t.flag_default = rd.read_uint(size)? != 0,
            id::DEFAULT_DURATION => t.default_duration = Some(rd.read_uint(size)?),
            id::CONTENT_ENCODINGS => t.encoded = true,
            id::CODEC_PRIVATE => {
                if size > MAX_CODEC_PRIVATE as u64 {
                    t.codec_private_too_large = true;
                } else {
                    let mut b = vec![0u8; size as usize];
                    rd.read_exact(&mut b)?;
                    t.codec_private = b;
                }
            }
            id::VIDEO => for_each_child(rd, child_end, |rd, h, e| {
                let size = e - h.data_start;
                match h.id {
                    id::PIXEL_WIDTH => t.width = rd.read_uint(size)?,
                    id::PIXEL_HEIGHT => t.height = rd.read_uint(size)?,
                    id::COLOUR => for_each_child(rd, e, |rd, h, e| {
                        if h.id == id::RANGE {
                            t.full_range = rd.read_uint(e - h.data_start)? == 2;
                        }
                        Ok(())
                    })?,
                    _ => {}
                }
                Ok(())
            })?,
            id::AUDIO => for_each_child(rd, child_end, |rd, h, e| {
                let size = e - h.data_start;
                match h.id {
                    id::SAMPLING_FREQUENCY => t.sampling_frequency = rd.read_float(size)?,
                    id::CHANNELS => t.channels = rd.read_uint(size)?,
                    id::BIT_DEPTH => t.bit_depth = rd.read_uint(size)?,
                    _ => {}
                }
                Ok(())
            })?,
            _ => {}
        }
        Ok(())
    })?;
    Ok(t)
}

/// Segment-level IDs: inside an unknown-size Cluster, one of these ends the cluster.
fn is_segment_level(id: u64) -> bool {
    matches!(id, id::CLUSTER | id::CUES | id::TAGS | id::CHAPTERS | id::ATTACHMENTS | id::SEEK_HEAD | id::INFO | id::TRACKS | id::SEGMENT | id::EBML)
}

enum ClusterEnd {
    /// The cluster (and its children) parsed completely.
    Complete,
    /// Data ran out or became unparseable inside the cluster: stop scanning.
    Stop,
    /// An unknown-size cluster ended at this segment-level element (already read).
    Next(Hdr),
}

struct Scan {
    timestamp_scale: u64,
    /// Matroska track number → index into `tracks` (only supported tracks).
    track_map: Vec<(u64, usize)>,
    frames: Vec<FrameRef>,
    max_frames: usize,
    valid_until: u64,
    laced_blocks: u64,
}

impl Scan {
    fn ticks_to_hns(&self, ticks: i64) -> Hns {
        let ns = i128::from(ticks) * i128::from(self.timestamp_scale);
        Hns(i64::try_from(ns / 100).unwrap_or(if ns < 0 { i64::MIN } else { i64::MAX }))
    }

    /// Parses a (Simple)Block header at `start` (payload runs to `end`) and records the frame.
    fn block<R: Read + Seek>(&mut self, rd: &mut Reader<R>, start: u64, end: u64, cluster_ts: i64, keyframe: Option<bool>) -> Result<(), MuxError> {
        let (track_number, _) = rd.read_vint(false).map_err(|_| MuxError::InvalidData("bad block track number".into()))?;
        let mut b = [0u8; 3];
        rd.read_exact(&mut b)?;
        let rel = i16::from_be_bytes([b[0], b[1]]);
        let flags = b[2];
        let payload = rd.pos;
        if payload > end {
            return Err(MuxError::InvalidData("block shorter than its header".into()));
        }
        if flags & 0x06 != 0 {
            // Laced blocks pack several frames; our writer never produces them. Skip, counted.
            self.laced_blocks += 1;
            log::warn!("mkv scan: {}", MuxError::Unsupported(format!("laced block at offset {start}")));
            return Ok(());
        }
        let Some(&(_, track)) = self.track_map.iter().find(|(n, _)| *n == track_number) else {
            return Ok(()); // unsupported or unknown track
        };
        let Ok(size) = u32::try_from(end - payload) else {
            return Ok(());
        };
        if size == 0 {
            return Ok(());
        }
        if self.frames.len() >= self.max_frames {
            return Err(MuxError::TooLarge(format!("more than {} frames", self.max_frames)));
        }
        let pts = self.ticks_to_hns(cluster_ts.saturating_add(i64::from(rel)));
        self.frames.push(FrameRef { track, pts, keyframe: keyframe.unwrap_or(flags & 0x80 != 0), offset: payload, size });
        Ok(())
    }

    fn cluster<R: Read + Seek>(&mut self, rd: &mut Reader<R>, hdr: Hdr, segment_end: u64) -> Result<ClusterEnd, MuxError> {
        let end = match hdr.size {
            Some(s) => hdr.data_start.saturating_add(s).min(segment_end),
            None => segment_end,
        };
        let mut cluster_ts: i64 = 0;
        loop {
            if rd.pos >= end {
                return Ok(ClusterEnd::Complete);
            }
            let ch = match rd.read_header() {
                Ok(h) => h,
                Err(HdrErr::Io(e)) => return Err(e.into()),
                Err(_) => return Ok(ClusterEnd::Stop),
            };
            if hdr.size.is_none() && is_segment_level(ch.id) {
                return Ok(ClusterEnd::Next(ch));
            }
            let Some(size) = ch.size else { return Ok(ClusterEnd::Stop) };
            let Some(child_end) = ch.data_start.checked_add(size).filter(|&e| e <= end) else {
                return Ok(ClusterEnd::Stop); // truncated (or corrupt) child: the tail is unusable
            };
            let res = match ch.id {
                id::TIMESTAMP => rd.read_uint(size).map(|v| cluster_ts = i64::try_from(v).unwrap_or(i64::MAX)),
                id::SIMPLE_BLOCK => self.block(rd, ch.data_start, child_end, cluster_ts, None),
                id::BLOCK_GROUP => self.block_group(rd, child_end, cluster_ts),
                _ => Ok(()),
            };
            match res {
                Ok(()) => {}
                Err(e @ MuxError::TooLarge(_)) => return Err(e),
                Err(MuxError::Io(e)) if e.kind() != io::ErrorKind::UnexpectedEof => return Err(e.into()),
                Err(e) => {
                    log::warn!("mkv scan: stopping at offset {}: {e}", ch.data_start);
                    return Ok(ClusterEnd::Stop);
                }
            }
            rd.seek_to(child_end)?;
            self.valid_until = child_end;
        }
    }

    fn block_group<R: Read + Seek>(&mut self, rd: &mut Reader<R>, end: u64, cluster_ts: i64) -> Result<(), MuxError> {
        let mut block = None;
        let mut referenced = false;
        for_each_child(rd, end, |_, h, e| {
            match h.id {
                id::BLOCK => block = Some((h.data_start, e)),
                id::REFERENCE_BLOCK => referenced = true,
                _ => {}
            }
            Ok(())
        })?;
        if let Some((start, e)) = block {
            rd.seek_to(start)?;
            self.block(rd, start, e, cluster_ts, Some(!referenced))?;
        }
        Ok(())
    }
}

/// Indexes a Matroska file: tracks, frame positions, duration and whether it was finalized.
///
/// `file_len` bounds every read (pass the file's length); `max_frames` caps the index size (exceeding it
/// is a [`MuxError::TooLarge`] error, never a silent truncation). Fails only when the header (EBML
/// header, Segment, Info, Tracks) is unusable; damage after that ends the scan at the last complete block.
pub fn scan_mkv<R: Read + Seek>(r: R, file_len: u64, max_frames: usize) -> Result<MkvIndex, MuxError> {
    let mut r = BufReader::with_capacity(1 << 20, r);
    r.seek(SeekFrom::Start(0))?;
    let mut rd = Reader { r, pos: 0, len: file_len };
    let bad_header = |what: &str| MuxError::InvalidData(format!("not a readable Matroska file: {what}"));

    // EBML header.
    let h = rd.read_header().map_err(|_| bad_header("missing EBML header"))?;
    let size = h.size.filter(|&s| h.id == id::EBML && s <= MAX_SMALL_MASTER).ok_or_else(|| bad_header("bad EBML header"))?;
    let ebml_end = h.data_start.checked_add(size).filter(|&e| e <= file_len).ok_or_else(|| bad_header("truncated EBML header"))?;
    let mut doc_type = String::new();
    let mut limits_ok = true;
    for_each_child(&mut rd, ebml_end, |rd, h, e| {
        match h.id {
            id::DOC_TYPE => doc_type = rd.read_string(e - h.data_start)?,
            id::EBML_MAX_ID_LENGTH => limits_ok &= rd.read_uint(e - h.data_start)? <= 8,
            id::EBML_MAX_SIZE_LENGTH => limits_ok &= rd.read_uint(e - h.data_start)? <= 8,
            _ => {}
        }
        Ok(())
    })?;
    if doc_type != "matroska" && doc_type != "webm" {
        return Err(MuxError::Unsupported(format!("EBML document type {doc_type:?}")));
    }
    if !limits_ok {
        return Err(MuxError::Unsupported("EBML IDs or sizes longer than 8 bytes".into()));
    }

    // Segment (skipping top-level Voids and the like).
    let segment = loop {
        let h = rd.read_header().map_err(|_| bad_header("no Segment"))?;
        if h.id == id::SEGMENT {
            break h;
        }
        let end = h.size.and_then(|s| h.data_start.checked_add(s)).filter(|&e| e <= file_len).ok_or_else(|| bad_header("junk before Segment"))?;
        rd.seek_to(end)?;
    };
    let declared_end = segment.size.and_then(|s| segment.data_start.checked_add(s));
    let segment_end = declared_end.map_or(file_len, |e| e.min(file_len));
    let size_known = declared_end.is_some_and(|e| e <= file_len);

    let mut scan = Scan { timestamp_scale: 1_000_000, track_map: Vec::new(), frames: Vec::new(), max_frames, valid_until: rd.pos, laced_blocks: 0 };
    let mut tracks: Option<Vec<TrackSpec>> = None;
    let mut info_duration: Option<f64> = None;
    let mut cues = false;
    let mut pending: Option<Hdr> = None;

    while pending.is_some() || rd.pos < segment_end {
        let h = match pending.take() {
            Some(h) => h,
            None => match rd.read_header() {
                Ok(h) => h,
                Err(HdrErr::Io(e)) => return Err(e.into()),
                Err(_) => break,
            },
        };
        if h.id == id::CLUSTER {
            if tracks.is_none() {
                return Err(bad_header("Cluster before Tracks"));
            }
            match scan.cluster(&mut rd, h, segment_end)? {
                ClusterEnd::Complete => {
                    scan.valid_until = rd.pos;
                    continue;
                }
                ClusterEnd::Next(next) => {
                    pending = Some(next);
                    continue;
                }
                ClusterEnd::Stop => break,
            }
        }
        // Any other element needs a known size that fits the segment.
        let Some(end) = h.size.and_then(|s| h.data_start.checked_add(s)).filter(|&e| e <= segment_end) else {
            if tracks.is_none() {
                return Err(bad_header("truncated or malformed header element"));
            }
            break;
        };
        let size = end - h.data_start;
        let res = match h.id {
            id::INFO if size <= MAX_SMALL_MASTER => for_each_child(&mut rd, end, |rd, h, e| {
                match h.id {
                    id::TIMESTAMP_SCALE => scan.timestamp_scale = rd.read_uint(e - h.data_start)?,
                    id::DURATION => info_duration = Some(rd.read_float(e - h.data_start)?),
                    _ => {}
                }
                Ok(())
            }),
            id::TRACKS if size <= MAX_TRACKS_ELEMENT && tracks.is_none() => {
                let mut raw = Vec::new();
                let r = for_each_child(&mut rd, end, |rd, h, e| {
                    if h.id == id::TRACK_ENTRY {
                        if raw.len() >= MAX_TRACKS {
                            return Err(MuxError::TooLarge(format!("more than {MAX_TRACKS} tracks")));
                        }
                        raw.push(parse_track_entry(rd, e)?);
                    }
                    Ok(())
                });
                let mut specs = Vec::new();
                for t in raw {
                    let number = t.number;
                    if number == 0 || scan.track_map.iter().any(|(n, _)| *n == number) {
                        continue;
                    }
                    if let Some(spec) = t.into_spec() {
                        scan.track_map.push((number, specs.len()));
                        specs.push(spec);
                    }
                }
                tracks = Some(specs);
                r
            }
            id::CUES => {
                cues = true;
                Ok(())
            }
            _ => Ok(()),
        };
        if let Err(e) = res {
            if tracks.is_none() || !is_eof(&e) {
                return Err(e);
            }
            break;
        }
        rd.seek_to(end)?;
        scan.valid_until = end;
    }

    let tracks = tracks.ok_or_else(|| bad_header("no Tracks element"))?;
    if scan.timestamp_scale == 0 {
        return Err(bad_header("TimestampScale 0"));
    }
    if scan.laced_blocks > 0 {
        log::warn!("mkv scan: skipped {} laced blocks", scan.laced_blocks);
    }
    let finalized = size_known && cues;
    let computed = computed_duration(&tracks, &scan.frames);
    let duration = match info_duration {
        Some(d) if finalized && d.is_finite() && d > 0.0 => {
            let hns = d * scan.timestamp_scale as f64 / 100.0;
            if hns < i64::MAX as f64 { Hns(hns.round() as i64) } else { computed }
        }
        _ => computed,
    };
    Ok(MkvIndex { tracks, frames: scan.frames, duration, finalized, valid_until: scan.valid_until })
}

/// Largest `pts + nominal frame duration` over all frames.
fn computed_duration(tracks: &[TrackSpec], frames: &[FrameRef]) -> Hns {
    let nominal: Vec<(Hns, Option<(SampleClock, usize)>)> = tracks
        .iter()
        .map(|t| match t {
            TrackSpec::Video(v) => (v.frame_rate.map_or(Hns::ZERO, |r| r.frame_duration()), None),
            TrackSpec::Audio(a) => {
                let clock = SampleClock::new(a.sample_rate);
                match a.codec {
                    AudioCodec::Aac => (clock.map_or(Hns::ZERO, |c| c.time_of(1024)), None),
                    AudioCodec::PcmF32 => (Hns::ZERO, clock.map(|c| (c, 4 * usize::from(a.channels).max(1)))),
                }
            }
        })
        .collect();
    frames
        .iter()
        .map(|f| {
            let d = match nominal.get(f.track) {
                Some((_, Some((clock, frame_bytes)))) => clock.time_of((f.size as usize / frame_bytes) as i64),
                Some((d, None)) => *d,
                None => Hns::ZERO,
            };
            f.pts.saturating_add(d)
        })
        .max()
        .unwrap_or(Hns::ZERO)
        .max(Hns::ZERO)
}
