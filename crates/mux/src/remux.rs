//! MKV → MP4 remux and recovery of unfinished MKV files.
//!
//! Both read the source through [`scan_mkv`] and write `dst` + `.part`, renaming it over `dst` only after
//! everything was written and synced. The source is opened read-only and never modified. Frames before
//! the first video keyframe (and any frame timed before it) cannot be decoded and are dropped; the output
//! timeline starts at that keyframe.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use fluxcapture_time::Hns;

use crate::codec::config_from_length_prefixed;
use crate::mkv::{MkvOptions, MkvWriter};
use crate::mkv_read::{FrameRef, MkvIndex, scan_mkv};
use crate::mp4::{Mp4Sample, write_mp4};
use crate::{AudioCodec, MAX_FRAME_BYTES, MuxError, TrackSpec};

/// Most frames a source may have (≈ 10 h of 60 fps video plus two 48 kHz AAC tracks is 6 M).
const MAX_FRAMES: usize = 30_000_000;
const IO_BUFFER: usize = 1 << 20;

/// Outcome of [`remux_mkv_to_mp4`] / [`recover_mkv`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemuxReport {
    /// Frames written.
    pub frames: usize,
    /// Frames dropped because they precede the first video keyframe.
    pub dropped_frames: usize,
    /// Duration of the output.
    pub duration: Hns,
    /// Size of the output file.
    pub bytes: u64,
    /// Bytes at the end of the source that were not usable (truncated or damaged tail).
    pub dropped_tail_bytes: u64,
}

fn part_path(dst: &Path) -> PathBuf {
    let mut s: OsString = dst.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Reads frame payloads, seeking only when the next frame is not where the reader already is.
struct PayloadReader {
    r: BufReader<File>,
    pos: u64,
}

impl PayloadReader {
    fn new(f: File) -> Self {
        PayloadReader { r: BufReader::with_capacity(IO_BUFFER, f), pos: u64::MAX }
    }

    fn read(&mut self, f: &FrameRef, buf: &mut Vec<u8>) -> io::Result<()> {
        if f.offset != self.pos {
            match f.offset.checked_sub(self.pos).and_then(|d| i64::try_from(d).ok()).filter(|&d| d < IO_BUFFER as i64) {
                Some(d) => self.r.seek_relative(d)?,
                None => {
                    self.r.seek(SeekFrom::Start(f.offset))?;
                }
            }
        }
        let size = f.size as usize;
        if size > MAX_FRAME_BYTES {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
        }
        buf.clear();
        buf.resize(size, 0);
        self.r.read_exact(buf)?;
        self.pos = f.offset + u64::from(f.size);
        Ok(())
    }
}

struct Source {
    index: MkvIndex,
    reader: PayloadReader,
    file_len: u64,
}

fn open_source(src: &Path, dst: &Path) -> Result<Source, MuxError> {
    if same_file(src, dst) || same_file(src, &part_path(dst)) {
        return Err(MuxError::InvalidData("destination is the source file".into()));
    }
    let f = File::open(src)?;
    let file_len = f.metadata()?.len();
    let index = scan_mkv(&f, file_len, MAX_FRAMES)?;
    if index.tracks.is_empty() {
        return Err(MuxError::Unsupported("no supported tracks in source".into()));
    }
    Ok(Source { index, reader: PayloadReader::new(f), file_len })
}

/// Frames to keep (file order), the number dropped and the timeline origin.
fn select_frames(index: &MkvIndex) -> Result<(Vec<FrameRef>, usize, Hns), MuxError> {
    let video: Vec<usize> = index.tracks.iter().enumerate().filter(|(_, t)| t.is_video()).map(|(i, _)| i).collect();
    let origin = match video.first() {
        Some(&v) => index
            .frames
            .iter()
            .find(|f| f.track == v && f.keyframe)
            .map(|f| f.pts)
            .ok_or_else(|| MuxError::InvalidData("no video keyframe in source".into()))?,
        None => index.frames.iter().map(|f| f.pts).min().unwrap_or(Hns::ZERO),
    };
    let mut started = vec![false; index.tracks.len()];
    let mut kept = Vec::with_capacity(index.frames.len());
    for f in &index.frames {
        let Some(s) = started.get_mut(f.track) else { continue };
        if video.contains(&f.track) && !*s {
            *s = f.keyframe;
        } else if !video.contains(&f.track) {
            *s = true;
        }
        if *s && f.pts >= origin {
            kept.push(*f);
        }
    }
    let dropped = index.frames.len() - kept.len();
    Ok((kept, dropped, origin))
}

/// Video tracks without a decoder configuration record get one built from the in-band parameter sets
/// of their first keyframe.
fn fill_codec_private(tracks: &mut [TrackSpec], frames: &[FrameRef], reader: &mut PayloadReader) -> Result<(), MuxError> {
    let mut buf = Vec::new();
    for (i, t) in tracks.iter_mut().enumerate() {
        if let TrackSpec::Video(v) = t
            && v.codec_private.is_empty()
        {
            let f = frames.iter().find(|f| f.track == i && f.keyframe).ok_or_else(|| MuxError::InvalidTrack(format!("track {i}: no keyframe")))?;
            reader.read(f, &mut buf)?;
            v.codec_private = config_from_length_prefixed(v.codec, &buf)?;
        }
    }
    Ok(())
}

/// Writes through `write` into `dst.part`, then syncs and renames it to `dst`; removes the part file on
/// failure.
fn with_part_file(dst: &Path, write: impl FnOnce(File) -> Result<File, MuxError>) -> Result<u64, MuxError> {
    let part = part_path(dst);
    let res = (|| {
        let f = File::create(&part)?;
        let f = write(f)?;
        f.sync_all()?;
        let len = f.metadata()?.len();
        drop(f);
        fs::rename(&part, dst)?;
        Ok(len)
    })();
    if res.is_err() {
        let _ = fs::remove_file(&part);
    }
    res
}

/// Remuxes a FluxCapture Matroska file (finished or not) into a faststart MP4 at `dst`.
///
/// Fails with [`MuxError::Unsupported`] if the source has float PCM audio (not storable in MP4).
pub fn remux_mkv_to_mp4(src: &Path, dst: &Path) -> Result<RemuxReport, MuxError> {
    let Source { index, mut reader, file_len } = open_source(src, dst)?;
    if index.tracks.iter().any(|t| matches!(t, TrackSpec::Audio(a) if a.codec == AudioCodec::PcmF32)) {
        return Err(MuxError::Unsupported("float PCM audio cannot be stored in MP4".into()));
    }
    let (kept, dropped, origin) = select_frames(&index)?;
    let mut tracks = index.tracks.clone();
    fill_codec_private(&mut tracks, &kept, &mut reader)?;
    let samples: Vec<Mp4Sample> = kept.iter().map(|f| Mp4Sample { track: f.track, pts: f.pts - origin, keyframe: f.keyframe, size: f.size }).collect();
    let bytes = with_part_file(dst, |mut f| {
        write_mp4(&mut f, &tracks, &samples, &mut |i, buf| match kept.get(i) {
            Some(fr) => reader.read(fr, buf),
            None => Err(io::Error::new(io::ErrorKind::InvalidInput, "sample index out of range")),
        })?;
        Ok(f)
    })?;
    let duration = (index.duration - origin).max(Hns::ZERO);
    Ok(RemuxReport { frames: kept.len(), dropped_frames: dropped, duration, bytes, dropped_tail_bytes: file_len.saturating_sub(index.valid_until) })
}

/// Rewrites an unfinished or truncated Matroska file as a finalized one (Cues, Duration, Segment size)
/// at `dst`. The source is not modified.
pub fn recover_mkv(src: &Path, dst: &Path) -> Result<RemuxReport, MuxError> {
    let Source { index, mut reader, file_len } = open_source(src, dst)?;
    let (kept, dropped, origin) = select_frames(&index)?;
    let mut tracks = index.tracks.clone();
    fill_codec_private(&mut tracks, &kept, &mut reader)?;
    let mut duration = Hns::ZERO;
    let opts = MkvOptions { title: String::new(), date_utc: None, ..MkvOptions::default() };
    let bytes = with_part_file(dst, |f| {
        let mut w = MkvWriter::new(BufWriter::with_capacity(IO_BUFFER, f), &tracks, opts)?;
        let mut buf = Vec::new();
        for fr in &kept {
            reader.read(fr, &mut buf)?;
            w.write(fr.track, fr.pts - origin, fr.keyframe, &buf)?;
        }
        duration = w.duration();
        w.finish()?.into_inner().map_err(|e| MuxError::Io(e.into_error()))
    })?;
    Ok(RemuxReport { frames: kept.len(), dropped_frames: dropped, duration, bytes, dropped_tail_bytes: file_len.saturating_sub(index.valid_until) })
}
