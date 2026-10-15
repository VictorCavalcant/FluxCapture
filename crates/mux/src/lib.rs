//! Containers for FluxCapture.
//!
//! Hardware encoders hand us compressed packets (Media Foundation H.264/HEVC MFTs emit Annex B access
//! units, the AAC MFT emits raw AAC frames). This crate turns them into files and reads our own files
//! back:
//!
//! * [`codec`] — Annex B / length-prefixed NAL helpers, `avcC` / `hvcC` / AAC `AudioSpecificConfig`
//!   builders (ISO/IEC 14496-15, 14496-3).
//! * [`mkv`] — a Matroska writer (RFC 8794 EBML + RFC 9559) built for crash resilience: the Segment has
//!   an unknown size and every Cluster is written as one complete element, so the file on disk is
//!   playable at every cluster boundary even if the process dies.
//! * [`mkv_read`] — a tolerant Matroska index scanner used by recovery and remux.
//! * [`mp4`] — a faststart ISO BMFF writer (ISO/IEC 14496-12/-14/-15).
//! * [`remux`] — MKV → MP4 remux and recovery of unfinished MKV files (never touching the source).
//!
//! All media time is [`Hns`] (100 ns ticks).

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod codec;
mod ebml;
pub mod mkv;
pub mod mkv_read;
pub mod mp4;
pub mod remux;

pub use fluxcapture_time::{FrameRate, Hns};
pub use mkv::{MkvOptions, MkvWriter};
pub use mkv_read::{FrameRef, MkvIndex, scan_mkv};
pub use mp4::{Mp4Sample, write_mp4};
pub use remux::{RemuxReport, recover_mkv, remux_mkv_to_mp4};

/// Most tracks a file may carry (Matroska track numbers stay a single byte in every block header).
pub const MAX_TRACKS: usize = 64;
/// Largest codec configuration record accepted (`avcC`, `hvcC`, `AudioSpecificConfig`).
pub const MAX_CODEC_PRIVATE: usize = 1 << 20;
/// Largest single encoded frame accepted (a 8K intra frame at absurd quality is far below this).
pub const MAX_FRAME_BYTES: usize = 256 << 20;
/// Largest video dimension accepted (MP4 sample entries store 16-bit sizes).
pub const MAX_DIMENSION: u32 = 65_535;
/// Highest audio sample rate accepted.
pub const MAX_SAMPLE_RATE: u32 = 768_000;

/// Video codec of a track.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum VideoCodec {
    /// H.264 / AVC.
    H264,
    /// H.265 / HEVC.
    Hevc,
}

/// Audio codec of a track.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum AudioCodec {
    /// AAC-LC raw frames (1024 samples each).
    Aac,
    /// 32-bit IEEE float PCM, little-endian, interleaved (`A_PCM/IEEE_FLOAT`; Matroska only).
    PcmF32,
}

/// Colour description of a video track. Primaries, transfer and matrix are always BT.709 (1/1/1).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct ColorInfo {
    /// `true` for full range (0–255), `false` for limited/studio range (16–235).
    pub full_range: bool,
}

/// A video track.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoTrack {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    /// `Some` for a constant frame rate (timestamps lie on the frame grid), `None` for variable.
    pub frame_rate: Option<FrameRate>,
    /// `AVCDecoderConfigurationRecord` (`avcC`) or `HEVCDecoderConfigurationRecord` (`hvcC`).
    pub codec_private: Vec<u8>,
    pub color: ColorInfo,
    pub name: String,
}

/// An audio track.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioTrack {
    pub codec: AudioCodec,
    pub sample_rate: u32,
    pub channels: u16,
    /// `AudioSpecificConfig` for AAC, empty for PCM.
    pub codec_private: Vec<u8>,
    /// Display name, e.g. "Mix", "Sistema", "Microfone".
    pub name: String,
    /// Whether players should pick this track by default.
    pub default: bool,
}

/// One track of a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrackSpec {
    Video(VideoTrack),
    Audio(AudioTrack),
}

impl TrackSpec {
    /// `true` for a video track.
    pub fn is_video(&self) -> bool {
        matches!(self, TrackSpec::Video(_))
    }

    /// The track's display name.
    pub fn name(&self) -> &str {
        match self {
            TrackSpec::Video(v) => &v.name,
            TrackSpec::Audio(a) => &a.name,
        }
    }
}

/// Errors of this crate.
#[derive(Debug, thiserror::Error)]
pub enum MuxError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid track: {0}")]
    InvalidTrack(String),
    #[error("invalid data: {0}")]
    InvalidData(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("limit exceeded: {0}")]
    TooLarge(String),
    /// An earlier write failed part-way; the output may end in a partial element, so the writer refuses
    /// to append more. The bytes before the failed cluster are still a valid file.
    #[error("the writer failed earlier and cannot continue")]
    Failed,
}

/// Checks the track list shared by every writer.
pub(crate) fn validate_tracks(tracks: &[TrackSpec]) -> Result<(), MuxError> {
    if tracks.is_empty() {
        return Err(MuxError::InvalidTrack("no tracks".into()));
    }
    if tracks.len() > MAX_TRACKS {
        return Err(MuxError::InvalidTrack(format!("{} tracks (at most {MAX_TRACKS})", tracks.len())));
    }
    for (i, t) in tracks.iter().enumerate() {
        match t {
            TrackSpec::Video(v) => {
                if v.width == 0 || v.height == 0 || v.width > MAX_DIMENSION || v.height > MAX_DIMENSION {
                    return Err(MuxError::InvalidTrack(format!("track {i}: video size {}x{}", v.width, v.height)));
                }
                let min = match v.codec {
                    VideoCodec::H264 => 7,
                    VideoCodec::Hevc => 23,
                };
                if v.codec_private.len() < min || v.codec_private.len() > MAX_CODEC_PRIVATE || v.codec_private.first() != Some(&1) {
                    return Err(MuxError::InvalidTrack(format!("track {i}: missing or malformed {:?} decoder configuration record", v.codec)));
                }
            }
            TrackSpec::Audio(a) => {
                if a.sample_rate == 0 || a.sample_rate > MAX_SAMPLE_RATE {
                    return Err(MuxError::InvalidTrack(format!("track {i}: sample rate {}", a.sample_rate)));
                }
                if a.channels == 0 || a.channels > 64 {
                    return Err(MuxError::InvalidTrack(format!("track {i}: {} channels", a.channels)));
                }
                if a.codec == AudioCodec::Aac && (a.codec_private.len() < 2 || a.codec_private.len() > MAX_CODEC_PRIVATE) {
                    return Err(MuxError::InvalidTrack(format!("track {i}: AAC track without a valid AudioSpecificConfig")));
                }
            }
        }
    }
    Ok(())
}
