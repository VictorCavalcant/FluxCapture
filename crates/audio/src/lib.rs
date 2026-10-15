//! Audio for FluxCapture: timeline placement, resampling, filters and the realtime mixer.
//!
//! Platform code (WASAPI) captures each device on its own thread and hands [`AudioChunk`]s —
//! interleaved `f32` samples stamped with the QPC time ([`Hns`]) of their first frame — to the mixer
//! thread, which owns a [`Mixer`]. Every ~10 ms that thread asks [`Mixer::mix`] for the next window of
//! the fixed 48 kHz stereo output timeline, running a fixed latency behind real time.
//!
//! Per source the mixer:
//! 1. converts the chunk to stereo (downmix/upmix with ITU-style coefficients) and replaces NaN/inf by 0;
//! 2. places it on the output timeline and resamples it to 48 kHz with a band-limited polyphase
//!    windowed-sinc resampler whose ratio is continuously trimmed (at most ±0.5 %) by a PI controller
//!    so the source's clock cannot drift away from QPC (see `README.md` and [`Mixer::push`]);
//! 3. stores the result in a bounded FIFO (2 s);
//! 4. at mix time runs the filter chain (gain → high-pass → noise suppression → gate → compressor →
//!    mono/volume/balance) and sums the source into each output track it is routed to;
//! 5. applies the master volume and a lookahead peak limiter per track.
//!
//! Noise suppression and the limiter have a constant latency which the mixer compensates internally
//! by reading ahead ([`PROCESSING_LOOKAHEAD_FRAMES`]): output sample `n` of a window starting at
//! `start` really is the audio captured at `start + n / 48 kHz`. **No encoder-delay or timestamp
//! correction is needed downstream for either stage.** The caller only has to stay at least
//! [`processing_lookahead`] plus its device buffering behind real time ([`RECOMMENDED_MIX_DELAY`]).

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod channels;
mod denoise;
mod drift;
mod dsp;
mod fft;
mod fifo;
mod meter;
mod mixer;
mod resample;
mod settings;
mod source;
mod util;

use fluxcapture_time::{HNS_PER_SEC, Hns};
use serde::{Deserialize, Serialize};

pub use meter::{LevelMeter, METER_FLOOR_DB, SourceMeter};
pub use mixer::{MixBuffers, Mixer, MixerStats};
pub use resample::{MAX_CORRECTION, Resampler};
pub use settings::{
    CompressorSettings, FilterSettings, GateSettings, HighPassSettings, LimiterSettings, MixerSettings, NoiseSuppressionSettings, SourceSettings,
};
pub use source::SourceStats;
pub use util::{Dither, ToneGenerator, db_to_gain, f32_to_i16, gain_to_db};

/// Sample rate of the output timeline and of every mixed track.
pub const OUTPUT_RATE: u32 = 48_000;
/// Channels of every mixed track (interleaved L, R).
pub const OUTPUT_CHANNELS: usize = 2;
/// Maximum number of output tracks.
pub const MAX_TRACKS: usize = 6;
/// Maximum number of sources a mixer accepts.
pub const MAX_SOURCES: usize = 32;
/// Lowest accepted device sample rate.
pub const MIN_INPUT_RATE: u32 = 8_000;
/// Highest accepted device sample rate.
pub const MAX_INPUT_RATE: u32 = 384_000;
/// Highest accepted device channel count.
pub const MAX_INPUT_CHANNELS: u16 = 32;
/// Largest window one [`Mixer::mix`] call produces (1 s); bigger requests are truncated.
pub const MAX_MIX_FRAMES: usize = OUTPUT_RATE as usize;
/// Capacity of each source's FIFO of converted frames (2 s). Overflow drops the oldest frames and
/// counts an overrun.
pub const SOURCE_BUFFER_FRAMES: usize = 2 * OUTPUT_RATE as usize;
/// Constant latency of the noise-suppression stage (one 1024-point STFT frame, 21.3 ms). It is
/// present whether or not noise suppression is enabled, so toggling it never shifts the timeline.
pub const NOISE_SUPPRESSION_LATENCY_FRAMES: usize = denoise::FRAME;
/// Constant latency of the master limiter's lookahead (5 ms), present whether or not it is enabled.
pub const LIMITER_LOOKAHEAD_FRAMES: usize = dsp::LIMITER_LOOKAHEAD;
/// Total read-ahead the mixer performs to compensate the constant DSP latencies: to produce output
/// window `[p, p + n)` it needs source data up to `p + n + PROCESSING_LOOKAHEAD_FRAMES`.
pub const PROCESSING_LOOKAHEAD_FRAMES: usize = NOISE_SUPPRESSION_LATENCY_FRAMES + LIMITER_LOOKAHEAD_FRAMES;
/// A sensible distance between real time and the start of the window passed to [`Mixer::mix`]:
/// lookahead (26.3 ms) + WASAPI period and jitter (~10–20 ms) + resampler lookahead (< 1 ms) + margin.
pub const RECOMMENDED_MIX_DELAY: Hns = Hns::from_millis(80);

/// [`PROCESSING_LOOKAHEAD_FRAMES`] as a duration (26.3 ms).
pub fn processing_lookahead() -> Hns {
    let rate = i64::from(OUTPUT_RATE);
    Hns((PROCESSING_LOOKAHEAD_FRAMES as i64 * HNS_PER_SEC + rate - 1) / rate)
}

/// Identifies an audio source (one capture device or loopback endpoint).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SourceId(pub u32);

/// The format of the samples in an [`AudioChunk`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AudioFormat {
    /// Frames per second reported by the device.
    pub sample_rate: u32,
    /// Interleaved channels per frame, in WAVE order (FL, FR, FC, LFE, BL, BR, SL, SR…).
    pub channels: u16,
}

impl AudioFormat {
    /// 48 kHz stereo.
    pub const OUTPUT: AudioFormat = AudioFormat { sample_rate: OUTPUT_RATE, channels: 2 };

    /// Whether the mixer can take this format (rate within
    /// [`MIN_INPUT_RATE`]`..=`[`MAX_INPUT_RATE`], `1..=`[`MAX_INPUT_CHANNELS`] channels).
    pub fn is_supported(&self) -> bool {
        (MIN_INPUT_RATE..=MAX_INPUT_RATE).contains(&self.sample_rate) && (1..=MAX_INPUT_CHANNELS).contains(&self.channels)
    }
}

/// One packet of captured audio.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioChunk {
    /// The source it belongs to.
    pub source: SourceId,
    /// QPC time of the first frame.
    pub time: Hns,
    /// Format of `samples`.
    pub format: AudioFormat,
    /// Interleaved samples; a trailing partial frame is ignored. NaN/inf are treated as 0.
    pub samples: Vec<f32>,
    /// The device reported a gap before this chunk: it is re-anchored on its own timestamp.
    pub discontinuity: bool,
    /// The device flagged the packet as silent: samples are treated as zeros.
    pub silent: bool,
}

/// Errors returned by the mixer's configuration methods.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AudioError {
    /// [`MAX_SOURCES`] sources already exist.
    #[error("too many audio sources (at most {max})")]
    TooManySources {
        /// The limit.
        max: usize,
    },
}
