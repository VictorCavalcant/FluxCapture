//! Crash-resilient Matroska writer (RFC 8794 EBML, RFC 9559 Matroska).
//!
//! Layout:
//!
//! ```text
//! EBML header
//! Segment (size written as 8-byte "unknown", patched by finish)
//!   Void (reserved for the SeekHead, written by finish)
//!   Info (Duration slot reserved as an 11-byte Void, replaced by finish)
//!   Tracks
//!   Cluster               (empty, Timestamp 0: even a file with no packets opens everywhere)
//!   Cluster, Cluster, …   (each written whole, with a known size)
//!   Cues                  (finish)
//! ```
//!
//! Clusters are assembled in a reused in-memory buffer and appended to the output in one piece when they
//! close, so the bytes on disk are always "header + N complete clusters (+ a partially flushed one)". An
//! unknown-size Segment followed by complete Clusters is exactly what live Matroska streams look like,
//! so any player can open the file after a crash; `recover_mkv` rewrites it with Cues and a Duration.

use std::io::{Seek, SeekFrom, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use fluxcapture_time::{HNS_PER_SEC, Hns, SampleClock};

use crate::ebml::{self, UNKNOWN_SIZE_8, VINT_MAX, id};
use crate::{AudioCodec, MAX_FRAME_BYTES, MuxError, TrackSpec, VideoCodec, validate_tracks};

/// Bytes reserved after the Segment header for the SeekHead (3 Seek entries need 68).
const SEEK_HEAD_RESERVE: usize = 96;
/// Bytes of the Info Duration element (2-byte ID, 1-byte size, 8-byte float) reserved as a Void.
const DURATION_SLOT: usize = 11;
/// Upper bound for a cluster's span: block timestamps are i16 milliseconds relative to the cluster.
const MAX_CLUSTER_SPAN_MS: i64 = 30_000;
/// Seconds between 1970-01-01 and the Matroska DateUTC epoch, 2001-01-01T00:00:00 UTC.
const MKV_EPOCH_UNIX_SECS: i64 = 978_307_200;

/// Options of [`MkvWriter`].
#[derive(Clone, Debug)]
pub struct MkvOptions {
    /// Segment title (omitted when empty).
    pub title: String,
    /// `WritingApp`, e.g. "FluxCapture 0.1.0".
    pub writing_app: String,
    /// Longest span of a cluster (default 5 s; capped at 30 s by the i16 block timestamps).
    pub max_cluster_duration: Hns,
    /// Largest cluster payload before a new one starts (default 32 MiB).
    pub max_cluster_bytes: usize,
    /// Recording start written as `DateUTC` (default: now; `None` omits it).
    pub date_utc: Option<SystemTime>,
}

impl Default for MkvOptions {
    fn default() -> Self {
        MkvOptions {
            title: String::new(),
            writing_app: concat!("FluxCapture ", env!("CARGO_PKG_VERSION")).into(),
            max_cluster_duration: Hns::from_secs(5),
            max_cluster_bytes: 32 << 20,
            date_utc: Some(SystemTime::now()),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    Video { frame_duration: Option<Hns> },
    Aac { clock: Option<SampleClock> },
    Pcm { clock: Option<SampleClock>, frame_bytes: usize },
}

#[derive(Debug)]
struct TrackState {
    number: u8,
    kind: Kind,
    last_pts: Option<Hns>,
    last_delta: Hns,
}

impl TrackState {
    /// Estimated duration of a frame of `len` bytes at `pts` (used for the file duration only).
    fn frame_duration(&mut self, pts: Hns, len: usize) -> Hns {
        let d = match self.kind {
            Kind::Video { frame_duration: Some(d) } => d,
            Kind::Video { frame_duration: None } => {
                if let Some(last) = self.last_pts.filter(|&l| pts > l) {
                    self.last_delta = pts - last;
                }
                self.last_delta
            }
            Kind::Aac { clock } => clock.map_or(Hns::ZERO, |c| c.time_of(1024)),
            Kind::Pcm { clock, frame_bytes } => clock.map_or(Hns::ZERO, |c| c.time_of((len / frame_bytes.max(1)) as i64)),
        };
        self.last_pts = Some(pts);
        d
    }
}

#[derive(Clone, Copy, Debug)]
struct Cue {
    time_ms: u64,
    track: u8,
    cluster_pos: u64,
    relative_pos: u64,
}

/// Streams encoded packets into a Matroska file. See the module docs for the crash-resilience design.
#[derive(Debug)]
pub struct MkvWriter<W: Write + Seek> {
    out: W,
    tracks: Vec<TrackState>,
    /// The track whose keyframes start clusters and get CuePoints (the first video track, else track 0).
    cue_track: usize,
    cue_track_is_video: bool,
    max_cluster_ms: i64,
    max_cluster_bytes: usize,
    /// Absolute stream position of the next byte to write.
    pos: u64,
    segment_size_pos: u64,
    segment_data_start: u64,
    seek_head_pos: u64,
    info_pos: u64,
    tracks_pos: u64,
    duration_pos: u64,
    cluster: Vec<u8>,
    cluster_open: bool,
    cluster_ts: i64,
    cues: Vec<Cue>,
    duration: Hns,
    failed: bool,
}

fn codec_id(t: &TrackSpec) -> &'static str {
    match t {
        TrackSpec::Video(v) => match v.codec {
            VideoCodec::H264 => "V_MPEG4/ISO/AVC",
            VideoCodec::Hevc => "V_MPEGH/ISO/HEVC",
        },
        TrackSpec::Audio(a) => match a.codec {
            AudioCodec::Aac => "A_AAC",
            AudioCodec::PcmF32 => "A_PCM/IEEE_FLOAT",
        },
    }
}

/// SplitMix64: a well-mixed, non-zero TrackUID from a seed.
fn uid(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    (z ^ (z >> 31)).max(1)
}

fn track_entry(i: usize, t: &TrackSpec, seed: u64) -> Vec<u8> {
    let mut e = Vec::with_capacity(128);
    ebml::put_uint(&mut e, id::TRACK_NUMBER, i as u64 + 1);
    ebml::put_uint(&mut e, id::TRACK_UID, uid(seed ^ (i as u64).wrapping_mul(0x1000_0000_01B3)));
    ebml::put_uint(&mut e, id::TRACK_TYPE, if t.is_video() { 1 } else { 2 });
    ebml::put_uint(&mut e, id::FLAG_LACING, 0);
    ebml::put_str(&mut e, id::LANGUAGE, "und");
    if !t.name().is_empty() {
        ebml::put_str(&mut e, id::NAME, t.name());
    }
    ebml::put_str(&mut e, id::CODEC_ID, codec_id(t));
    match t {
        TrackSpec::Video(v) => {
            ebml::put_uint(&mut e, id::FLAG_DEFAULT, 1);
            ebml::put_bytes(&mut e, id::CODEC_PRIVATE, &v.codec_private);
            if let Some(rate) = v.frame_rate {
                ebml::put_uint(&mut e, id::DEFAULT_DURATION, rate.frame_duration_ns());
            }
            let mut colour = Vec::new();
            ebml::put_uint(&mut colour, id::MATRIX_COEFFICIENTS, 1);
            ebml::put_uint(&mut colour, id::RANGE, if v.color.full_range { 2 } else { 1 });
            ebml::put_uint(&mut colour, id::TRANSFER_CHARACTERISTICS, 1);
            ebml::put_uint(&mut colour, id::PRIMARIES, 1);
            let mut video = Vec::new();
            ebml::put_uint(&mut video, id::PIXEL_WIDTH, u64::from(v.width));
            ebml::put_uint(&mut video, id::PIXEL_HEIGHT, u64::from(v.height));
            ebml::put_master(&mut video, id::COLOUR, &colour);
            ebml::put_master(&mut e, id::VIDEO, &video);
        }
        TrackSpec::Audio(a) => {
            ebml::put_uint(&mut e, id::FLAG_DEFAULT, u64::from(a.default));
            if a.codec == AudioCodec::Aac {
                ebml::put_bytes(&mut e, id::CODEC_PRIVATE, &a.codec_private);
            }
            let mut audio = Vec::new();
            ebml::put_float(&mut audio, id::SAMPLING_FREQUENCY, f64::from(a.sample_rate));
            ebml::put_uint(&mut audio, id::CHANNELS, u64::from(a.channels));
            if a.codec == AudioCodec::PcmF32 {
                ebml::put_uint(&mut audio, id::BIT_DEPTH, 32);
            }
            ebml::put_master(&mut e, id::AUDIO, &audio);
        }
    }
    e
}

impl<W: Write + Seek> MkvWriter<W> {
    /// Writes the file header (EBML header, Segment start, SeekHead reserve, Info, Tracks) and flushes
    /// it, so a valid (empty) file exists from this point on.
    pub fn new(mut out: W, tracks: &[TrackSpec], opts: MkvOptions) -> Result<Self, MuxError> {
        validate_tracks(tracks)?;
        let start = out.stream_position()?;
        let mut h = Vec::with_capacity(1024);

        let mut ebml_body = Vec::new();
        ebml::put_uint(&mut ebml_body, id::EBML_VERSION, 1);
        ebml::put_uint(&mut ebml_body, id::EBML_READ_VERSION, 1);
        ebml::put_uint(&mut ebml_body, id::EBML_MAX_ID_LENGTH, 4);
        ebml::put_uint(&mut ebml_body, id::EBML_MAX_SIZE_LENGTH, 8);
        ebml::put_str(&mut ebml_body, id::DOC_TYPE, "matroska");
        ebml::put_uint(&mut ebml_body, id::DOC_TYPE_VERSION, 4);
        ebml::put_uint(&mut ebml_body, id::DOC_TYPE_READ_VERSION, 2);
        ebml::put_master(&mut h, id::EBML, &ebml_body);

        ebml::put_id(&mut h, id::SEGMENT);
        let segment_size_pos = start + h.len() as u64;
        h.extend_from_slice(&UNKNOWN_SIZE_8);
        let segment_data_start = start + h.len() as u64;

        let seek_head_pos = start + h.len() as u64;
        ebml::put_void(&mut h, SEEK_HEAD_RESERVE);

        let mut info = Vec::with_capacity(256);
        ebml::put_uint(&mut info, id::TIMESTAMP_SCALE, 1_000_000);
        let duration_off = info.len();
        ebml::put_void(&mut info, DURATION_SLOT);
        ebml::put_str(&mut info, id::MUXING_APP, concat!("fluxcapture-mux ", env!("CARGO_PKG_VERSION")));
        ebml::put_str(&mut info, id::WRITING_APP, &opts.writing_app);
        if let Some(date) = opts.date_utc {
            // Nanoseconds since 2001-01-01; a clock before 1970 just omits the element.
            if let Ok(d) = date.duration_since(UNIX_EPOCH) {
                let secs = i64::try_from(d.as_secs()).unwrap_or(i64::MAX).saturating_sub(MKV_EPOCH_UNIX_SECS);
                let ns = secs.saturating_mul(1_000_000_000).saturating_add(i64::from(d.subsec_nanos()));
                ebml::put_int8(&mut info, id::DATE_UTC, ns);
            }
        }
        if !opts.title.is_empty() {
            ebml::put_str(&mut info, id::TITLE, &opts.title);
        }
        let info_pos = start + h.len() as u64;
        ebml::put_id(&mut h, id::INFO);
        ebml::put_size(&mut h, info.len() as u64);
        let duration_pos = start + (h.len() + duration_off) as u64;
        h.extend_from_slice(&info);

        let seed = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0x5EED);
        let mut tracks_body = Vec::with_capacity(512);
        for (i, t) in tracks.iter().enumerate() {
            ebml::put_master(&mut tracks_body, id::TRACK_ENTRY, &track_entry(i, t, seed));
        }
        let tracks_pos = start + h.len() as u64;
        ebml::put_master(&mut h, id::TRACKS, &tracks_body);
        // An empty Cluster (Timestamp 0, no blocks — legal per RFC 9559) so that a file cut before the
        // first packet still opens in demuxers that refuse a Segment without any Cluster (ffmpeg does).
        let mut empty = Vec::with_capacity(4);
        ebml::put_uint(&mut empty, id::TIMESTAMP, 0);
        ebml::put_master(&mut h, id::CLUSTER, &empty);

        out.write_all(&h)?;
        out.flush()?;

        let states = tracks
            .iter()
            .enumerate()
            .map(|(i, t)| TrackState {
                number: (i + 1) as u8,
                kind: match t {
                    TrackSpec::Video(v) => Kind::Video { frame_duration: v.frame_rate.map(|r| r.frame_duration()) },
                    TrackSpec::Audio(a) => match a.codec {
                        AudioCodec::Aac => Kind::Aac { clock: SampleClock::new(a.sample_rate) },
                        AudioCodec::PcmF32 => Kind::Pcm { clock: SampleClock::new(a.sample_rate), frame_bytes: 4 * usize::from(a.channels) },
                    },
                },
                last_pts: None,
                last_delta: Hns::ZERO,
            })
            .collect();
        let video = tracks.iter().position(TrackSpec::is_video);
        let max_cluster_ms = opts.max_cluster_duration.as_millis().clamp(1, MAX_CLUSTER_SPAN_MS);
        let max_cluster_bytes = opts.max_cluster_bytes.clamp(64 << 10, 1 << 30);
        Ok(MkvWriter {
            out,
            tracks: states,
            cue_track: video.unwrap_or(0),
            cue_track_is_video: video.is_some(),
            max_cluster_ms,
            max_cluster_bytes,
            pos: start + h.len() as u64,
            segment_size_pos,
            segment_data_start,
            seek_head_pos,
            info_pos,
            tracks_pos,
            duration_pos,
            cluster: Vec::with_capacity(max_cluster_bytes.min(4 << 20)),
            cluster_open: false,
            cluster_ts: 0,
            cues: Vec::new(),
            duration: Hns::ZERO,
            failed: false,
        })
    }

    /// Appends one frame as a SimpleBlock.
    ///
    /// `track` is the 0-based index into the track list given to [`new`](Self::new); `pts` is relative to
    /// the recording start (negative values clamp to 0). Video data must be length-prefixed
    /// ([`crate::codec::annexb_to_length_prefixed`]). Audio frames are always flagged as keyframes.
    pub fn write(&mut self, track: usize, pts: Hns, keyframe: bool, data: &[u8]) -> Result<(), MuxError> {
        if self.failed {
            return Err(MuxError::Failed);
        }
        let n_tracks = self.tracks.len();
        let state = self.tracks.get_mut(track).ok_or_else(|| MuxError::InvalidTrack(format!("track index {track} of {n_tracks}")))?;
        if data.is_empty() || data.len() > MAX_FRAME_BYTES {
            return Err(MuxError::InvalidData(format!("frame of {} bytes", data.len())));
        }
        let pts = pts.max(Hns::ZERO);
        let ms = pts.as_millis_rounded();
        let is_video = matches!(state.kind, Kind::Video { .. });
        let keyframe = keyframe || !is_video;
        let number = state.number;
        let end = pts.saturating_add(state.frame_duration(pts, data.len()));
        let starts_cluster = track == self.cue_track && keyframe;

        let block_len = 4 + data.len() as u64; // track number (1 byte) + i16 timestamp + flags + payload
        let element_len = 1 + ebml::vint_len(block_len) + block_len as usize;
        if self.cluster_open {
            let rel = ms - self.cluster_ts;
            let split = (starts_cluster && self.cue_track_is_video)
                || rel >= self.max_cluster_ms
                || rel < i64::from(i16::MIN)
                || self.cluster.len() + element_len > self.max_cluster_bytes;
            if split {
                self.close_cluster()?;
            }
        }
        if !self.cluster_open {
            self.cluster.clear();
            self.cluster_ts = ms;
            self.cluster_open = true;
            ebml::put_uint(&mut self.cluster, id::TIMESTAMP, ms as u64);
            // Audio-only files cue every cluster start of track 0 so they get Cues too.
            if starts_cluster {
                self.cues.push(Cue {
                    time_ms: ms as u64,
                    track: number,
                    cluster_pos: self.pos - self.segment_data_start,
                    relative_pos: self.cluster.len() as u64,
                });
            }
        }
        let rel = (ms - self.cluster_ts).clamp(i64::from(i16::MIN), i64::from(i16::MAX)) as i16;
        let c = &mut self.cluster;
        c.push(0xA3);
        ebml::put_size(c, block_len);
        c.push(0x80 | number);
        c.extend_from_slice(&rel.to_be_bytes());
        c.push(if keyframe { 0x80 } else { 0 });
        c.extend_from_slice(data);
        self.duration = self.duration.max(end);
        Ok(())
    }

    /// Appends the open cluster to the output as one complete element.
    fn close_cluster(&mut self) -> Result<(), MuxError> {
        if !self.cluster_open {
            return Ok(());
        }
        let mut hdr = Vec::with_capacity(12);
        ebml::put_id(&mut hdr, id::CLUSTER);
        ebml::put_size(&mut hdr, self.cluster.len() as u64);
        // Pessimistic: if either write fails the output may end mid-cluster; refuse further appends.
        self.failed = true;
        self.out.write_all(&hdr)?;
        self.out.write_all(&self.cluster)?;
        self.failed = false;
        self.pos += (hdr.len() + self.cluster.len()) as u64;
        self.cluster.clear();
        self.cluster_open = false;
        Ok(())
    }

    /// Closes the current cluster and flushes the output, so everything written so far is a valid file
    /// on disk. Call periodically (e.g. every second) for crash resilience.
    pub fn flush(&mut self) -> Result<(), MuxError> {
        if self.failed {
            return Err(MuxError::Failed);
        }
        self.close_cluster()?;
        self.out.flush()?;
        Ok(())
    }

    /// Bytes handed to the output so far (excluding the open cluster).
    pub fn bytes_written(&self) -> u64 {
        self.pos
    }

    /// Largest `pts + frame duration` written so far.
    pub fn duration(&self) -> Hns {
        self.duration
    }

    /// The output (e.g. to inspect what an unfinished file looks like).
    pub fn get_ref(&self) -> &W {
        &self.out
    }

    /// Closes the last cluster, writes Cues, fills in the SeekHead, Duration and Segment size, flushes
    /// and returns the output positioned at the end of the file.
    pub fn finish(mut self) -> Result<W, MuxError> {
        if self.failed {
            return Err(MuxError::Failed);
        }
        self.close_cluster()?;

        let mut cues_pos = None;
        if !self.cues.is_empty() {
            let mut body = Vec::with_capacity(self.cues.len() * 24);
            let mut point = Vec::with_capacity(32);
            let mut positions = Vec::with_capacity(24);
            for cue in &self.cues {
                positions.clear();
                ebml::put_uint(&mut positions, id::CUE_TRACK, u64::from(cue.track));
                ebml::put_uint(&mut positions, id::CUE_CLUSTER_POSITION, cue.cluster_pos);
                ebml::put_uint(&mut positions, id::CUE_RELATIVE_POSITION, cue.relative_pos);
                point.clear();
                ebml::put_uint(&mut point, id::CUE_TIME, cue.time_ms);
                ebml::put_master(&mut point, id::CUE_TRACK_POSITIONS, &positions);
                ebml::put_master(&mut body, id::CUE_POINT, &point);
            }
            let mut cues = Vec::with_capacity(body.len() + 12);
            ebml::put_master(&mut cues, id::CUES, &body);
            self.failed = true;
            self.out.write_all(&cues)?;
            self.failed = false;
            cues_pos = Some(self.pos - self.segment_data_start);
            self.pos += cues.len() as u64;
        }
        let end = self.pos;
        let segment_size = end - self.segment_data_start;
        if segment_size > VINT_MAX {
            return Err(MuxError::TooLarge("segment larger than 2^56 bytes".into()));
        }

        let mut seek_head = Vec::with_capacity(SEEK_HEAD_RESERVE);
        let mut seeks = Vec::with_capacity(72);
        let entries =
            [(id::INFO, Some(self.info_pos - self.segment_data_start)), (id::TRACKS, Some(self.tracks_pos - self.segment_data_start)), (id::CUES, cues_pos)];
        for (target, pos) in entries {
            let Some(pos) = pos else { continue };
            let mut seek = Vec::with_capacity(24);
            let mut target_id = Vec::with_capacity(4);
            ebml::put_id(&mut target_id, target);
            ebml::put_bytes(&mut seek, id::SEEK_ID, &target_id);
            ebml::put_uint8(&mut seek, id::SEEK_POSITION, pos);
            ebml::put_master(&mut seeks, id::SEEK, &seek);
        }
        ebml::put_master(&mut seek_head, id::SEEK_HEAD, &seeks);
        let pad = SEEK_HEAD_RESERVE.saturating_sub(seek_head.len());
        ebml::put_void(&mut seek_head, pad);
        if seek_head.len() != SEEK_HEAD_RESERVE {
            return Err(MuxError::InvalidData("SeekHead does not fit its reserved space".into()));
        }

        let mut size = Vec::with_capacity(8);
        ebml::put_vint_n(&mut size, segment_size, 8);

        // Patches are in-place overwrites of same-sized regions; a failure leaves a still-valid file.
        self.out.seek(SeekFrom::Start(self.seek_head_pos))?;
        self.out.write_all(&seek_head)?;
        let ms = self.duration.0 as f64 / (HNS_PER_SEC / 1000) as f64;
        if ms > 0.0 {
            let mut dur = Vec::with_capacity(DURATION_SLOT);
            ebml::put_float(&mut dur, id::DURATION, ms);
            self.out.seek(SeekFrom::Start(self.duration_pos))?;
            self.out.write_all(&dur)?;
        }
        self.out.seek(SeekFrom::Start(self.segment_size_pos))?;
        self.out.write_all(&size)?;
        self.out.seek(SeekFrom::Start(end))?;
        self.out.flush()?;
        let MkvWriter { out, .. } = self;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AudioTrack, ColorInfo, VideoTrack};
    use fluxcapture_time::FrameRate;
    use std::io::Cursor;

    fn tracks() -> Vec<TrackSpec> {
        vec![
            TrackSpec::Video(VideoTrack {
                codec: VideoCodec::H264,
                width: 64,
                height: 32,
                frame_rate: Some(FrameRate::FPS_30),
                codec_private: vec![1, 100, 0, 30, 0xFF, 0xE0, 0],
                color: ColorInfo::default(),
                name: "Video".into(),
            }),
            TrackSpec::Audio(AudioTrack {
                codec: AudioCodec::Aac,
                sample_rate: 48_000,
                channels: 2,
                codec_private: vec![0x11, 0x90],
                name: "Mix".into(),
                default: true,
            }),
        ]
    }

    #[test]
    fn clusters_split_at_keyframes_and_cues_are_recorded() {
        let mut w = MkvWriter::new(Cursor::new(Vec::new()), &tracks(), MkvOptions::default()).unwrap();
        let header_len = w.bytes_written();
        for k in 0..90u64 {
            let pts = FrameRate::FPS_30.frame_time(k);
            w.write(0, pts, k % 30 == 0, &[0, 0, 0, 1, 0x41]).unwrap();
            w.write(1, pts, false, &[0x21, 0x10]).unwrap();
        }
        // Two clusters closed by keyframes, the third still open.
        assert!(w.bytes_written() > header_len);
        assert!(w.cluster_open);
        assert_eq!(w.cues.len(), 3);
        assert_eq!(w.cues[0].cluster_pos, header_len - w.segment_data_start);
        let d = w.duration();
        assert_eq!(d, FrameRate::FPS_30.frame_time(89) + FrameRate::FPS_30.frame_duration());
        let out = w.finish().unwrap().into_inner();
        // Segment size patched: no unknown-size marker left.
        assert!(!out.windows(8).any(|x| x == UNKNOWN_SIZE_8));
    }

    #[test]
    fn rejects_bad_input() {
        let mut w = MkvWriter::new(Cursor::new(Vec::new()), &tracks(), MkvOptions::default()).unwrap();
        assert!(w.write(2, Hns::ZERO, true, &[1]).is_err());
        assert!(w.write(0, Hns::ZERO, true, &[]).is_err());
        assert!(MkvWriter::new(Cursor::new(Vec::new()), &[], MkvOptions::default()).is_err());
    }

    #[test]
    fn out_of_order_and_long_gaps_keep_i16_timestamps() {
        let mut w = MkvWriter::new(Cursor::new(Vec::new()), &tracks(), MkvOptions::default()).unwrap();
        w.write(0, Hns::from_millis(1000), true, &[1]).unwrap();
        w.write(1, Hns::from_millis(700), true, &[1]).unwrap(); // 300 ms late: negative relative timestamp
        assert!(w.cluster_open);
        w.write(1, Hns::from_secs(100), true, &[1]).unwrap(); // gap: new cluster
        w.write(1, Hns::from_secs(10), true, &[1]).unwrap(); // far back: new cluster
        w.write(1, Hns(-5), true, &[1]).unwrap();
        w.finish().unwrap();
    }
}
