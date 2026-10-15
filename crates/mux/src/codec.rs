//! H.264 / HEVC / AAC bitstream helpers.
//!
//! * Annex B (ITU-T H.264 / H.265 Annex B) start-code splitting and conversion to the 4-byte
//!   length-prefixed form used inside Matroska and MP4 (ISO/IEC 14496-15).
//! * `AVCDecoderConfigurationRecord` (14496-15 §5.3.3) and `HEVCDecoderConfigurationRecord`
//!   (14496-15 §8.3.3) built from the parameter sets of a keyframe.
//! * AAC-LC `AudioSpecificConfig` (ISO/IEC 14496-3 §1.6.2.1).
//!
//! Every parser treats its input as hostile: all reads are bounds-checked and malformed data yields a
//! [`MuxError`], never a panic.

use crate::{MAX_CODEC_PRIVATE, MuxError, VideoCodec};

/// H.264 NAL unit types used here.
mod h264 {
    pub const IDR: u8 = 5;
    pub const SPS: u8 = 7;
    pub const PPS: u8 = 8;
    pub const AUD: u8 = 9;
}

/// HEVC NAL unit types used here.
mod hevc {
    pub const IRAP_FIRST: u8 = 16;
    pub const IRAP_LAST: u8 = 23;
    pub const VPS: u8 = 32;
    pub const SPS: u8 = 33;
    pub const PPS: u8 = 34;
    pub const AUD: u8 = 35;
}

/// H.264 `profile_idc` values whose SPS carries `chroma_format_idc` and bit depths, and whose `avcC`
/// gets the extension fields (14496-15 §5.3.3.1.1).
const H264_HIGH_PROFILES: [u8; 13] = [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135];

/// Index of the first byte of the next 3-byte start code (`00 00 01`) at or after `from`.
fn find_start_code(data: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    loop {
        let rest = data.get(i.checked_add(2)?..)?;
        let off = rest.iter().position(|&b| b == 1)?;
        let one = i + 2 + off;
        if data.get(one - 2..one) == Some(&[0, 0][..]) {
            return Some(one - 2);
        }
        // The next candidate `01` is at `one + 1` at the earliest, so its zeros start at `one - 1`.
        i = one - 1;
    }
}

/// Iterator over the NAL units of an Annex B byte stream. See [`nal_units`].
#[derive(Clone, Debug)]
pub struct NalUnits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Iterator for NalUnits<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        loop {
            if self.pos >= self.data.len() {
                return None;
            }
            let start = self.pos;
            let (end, next) = match find_start_code(self.data, start) {
                Some(sc) => (sc, sc + 3),
                None => (self.data.len(), self.data.len()),
            };
            self.pos = next;
            let mut nal = self.data.get(start..end).unwrap_or_default();
            // Trailing zeros belong to a 4-byte start code or are `trailing_zero_8bits`; a NAL unit never
            // ends in a zero byte (it ends with rbsp_trailing_bits or an emulation-prevention byte).
            while let [rest @ .., 0] = nal {
                nal = rest;
            }
            if !nal.is_empty() {
                return Some(nal);
            }
        }
    }
}

/// Splits an Annex B byte stream on 3- or 4-byte start codes. Bytes before the first start code are
/// ignored; empty NAL units are skipped. Yields NAL units including their header byte(s).
pub fn nal_units(annexb: &[u8]) -> NalUnits<'_> {
    let pos = find_start_code(annexb, 0).map_or(annexb.len(), |sc| sc + 3);
    NalUnits { data: annexb, pos }
}

/// Iterator over the NAL units of a 4-byte length-prefixed buffer. See [`length_prefixed_nal_units`].
#[derive(Clone, Debug)]
pub struct LengthPrefixedNalUnits<'a> {
    data: &'a [u8],
}

impl<'a> Iterator for LengthPrefixedNalUnits<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        loop {
            let (len, rest) = self.data.split_first_chunk::<4>()?;
            let len = usize::try_from(u32::from_be_bytes(*len)).ok()?;
            let Some(nal) = rest.get(..len) else {
                // Truncated or corrupt length: stop rather than guess.
                self.data = &[];
                return None;
            };
            self.data = rest.get(len..).unwrap_or_default();
            if !nal.is_empty() {
                return Some(nal);
            }
        }
    }
}

/// Splits a buffer of 4-byte big-endian length-prefixed NAL units (the Matroska/MP4 sample format).
/// Iteration stops at the first length that runs past the end of the buffer.
pub fn length_prefixed_nal_units(data: &[u8]) -> LengthPrefixedNalUnits<'_> {
    LengthPrefixedNalUnits { data }
}

/// NAL unit type of `nal` (`None` for an empty or too-short NAL unit).
pub fn nal_type(codec: VideoCodec, nal: &[u8]) -> Option<u8> {
    match codec {
        VideoCodec::H264 => nal.first().map(|b| b & 0x1f),
        VideoCodec::Hevc => {
            if nal.len() < 2 {
                return None;
            }
            nal.first().map(|b| (b >> 1) & 0x3f)
        }
    }
}

fn is_aud(codec: VideoCodec, nal: &[u8]) -> bool {
    nal_type(codec, nal)
        == Some(match codec {
            VideoCodec::H264 => h264::AUD,
            VideoCodec::Hevc => hevc::AUD,
        })
}

fn is_key_nal(codec: VideoCodec, nal: &[u8]) -> bool {
    match (codec, nal_type(codec, nal)) {
        (VideoCodec::H264, Some(t)) => t == h264::IDR,
        (VideoCodec::Hevc, Some(t)) => (hevc::IRAP_FIRST..=hevc::IRAP_LAST).contains(&t),
        _ => false,
    }
}

/// Converts an Annex B access unit to 4-byte big-endian length-prefixed NAL units in `out` (cleared
/// first; its capacity is reused). Access Unit Delimiters (H.264 type 9, HEVC type 35) and empty NAL
/// units are dropped; in-band VPS/SPS/PPS and SEI are kept.
///
/// Errors if the access unit contains no NAL unit.
pub fn annexb_to_length_prefixed(codec: VideoCodec, annexb: &[u8], out: &mut Vec<u8>) -> Result<(), MuxError> {
    out.clear();
    out.reserve(annexb.len().saturating_add(16));
    for nal in nal_units(annexb) {
        if is_aud(codec, nal) {
            continue;
        }
        let len = u32::try_from(nal.len()).map_err(|_| MuxError::InvalidData("NAL unit larger than 4 GiB".into()))?;
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(nal);
    }
    if out.is_empty() {
        return Err(MuxError::InvalidData("access unit without NAL units".into()));
    }
    Ok(())
}

/// `true` if the Annex B access unit holds an H.264 IDR slice / an HEVC IRAP picture (types 16–23).
pub fn is_keyframe_annexb(codec: VideoCodec, annexb: &[u8]) -> bool {
    nal_units(annexb).any(|n| is_key_nal(codec, n))
}

/// Same as [`is_keyframe_annexb`] for a length-prefixed sample.
pub fn is_keyframe_length_prefixed(codec: VideoCodec, data: &[u8]) -> bool {
    length_prefixed_nal_units(data).any(|n| is_key_nal(codec, n))
}

/// Removes emulation-prevention bytes (`00 00 03` → `00 00`), giving the RBSP.
fn unescape_rbsp(nal: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(nal.len());
    let mut zeros = 0u32;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
}

fn truncated() -> MuxError {
    MuxError::InvalidData("truncated parameter set".into())
}

/// MSB-first bit reader with Exp-Golomb support (H.264 §9.1).
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0 }
    }

    fn bit(&mut self) -> Result<u32, MuxError> {
        let byte = self.data.get(self.pos / 8).ok_or_else(truncated)?;
        let b = (byte >> (7 - (self.pos % 8))) & 1;
        self.pos += 1;
        Ok(u32::from(b))
    }

    fn bits(&mut self, n: u32) -> Result<u32, MuxError> {
        let mut v: u64 = 0;
        for _ in 0..n.min(32) {
            v = (v << 1) | u64::from(self.bit()?);
        }
        u32::try_from(v).map_err(|_| truncated())
    }

    fn skip(&mut self, n: usize) -> Result<(), MuxError> {
        let end = self.pos.checked_add(n).ok_or_else(truncated)?;
        if end > self.data.len().saturating_mul(8) {
            return Err(truncated());
        }
        self.pos = end;
        Ok(())
    }

    /// Unsigned Exp-Golomb `ue(v)`.
    fn ue(&mut self) -> Result<u32, MuxError> {
        let mut zeros = 0u32;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return Err(MuxError::InvalidData("malformed Exp-Golomb code".into()));
            }
        }
        let rest = u64::from(self.bits(zeros)?);
        u32::try_from((1u64 << zeros) - 1 + rest).map_err(|_| MuxError::InvalidData("Exp-Golomb value out of range".into()))
    }
}

/// Chroma format and bit depths of an H.264 SPS.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct H264Format {
    pub profile_idc: u8,
    pub level_idc: u8,
    pub chroma_format_idc: u8,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
}

/// Parses `profile_idc`, `level_idc`, `chroma_format_idc` and bit depths from an H.264 SPS NAL unit
/// (header byte included). Non-High profiles imply 4:2:0 8-bit (H.264 §7.4.2.1.1).
///
/// These fields all precede `seq_scaling_matrix_present_flag` in the SPS, so the scaling lists (whose
/// size depends on `chroma_format_idc`) never need to be walked.
pub fn parse_h264_sps_format(sps: &[u8]) -> Result<H264Format, MuxError> {
    if nal_type(VideoCodec::H264, sps) != Some(h264::SPS) {
        return Err(MuxError::InvalidData("not an H.264 SPS".into()));
    }
    let mut rbsp = Vec::new();
    unescape_rbsp(sps.get(1..).unwrap_or_default(), &mut rbsp);
    let mut br = BitReader::new(&rbsp);
    let profile_idc = br.bits(8)? as u8;
    br.skip(8)?; // constraint_set flags + reserved_zero_2bits
    let level_idc = br.bits(8)? as u8;
    let sps_id = br.ue()?;
    if sps_id > 31 {
        return Err(MuxError::InvalidData(format!("H.264 seq_parameter_set_id {sps_id}")));
    }
    let mut fmt = H264Format { profile_idc, level_idc, chroma_format_idc: 1, bit_depth_luma_minus8: 0, bit_depth_chroma_minus8: 0 };
    if H264_HIGH_PROFILES.contains(&profile_idc) {
        let chroma = br.ue()?;
        if chroma > 3 {
            return Err(MuxError::InvalidData(format!("H.264 chroma_format_idc {chroma}")));
        }
        if chroma == 3 {
            br.skip(1)?; // separate_colour_plane_flag
        }
        let luma = br.ue()?;
        let chroma_depth = br.ue()?;
        if luma > 6 || chroma_depth > 6 {
            return Err(MuxError::InvalidData("H.264 bit depth out of range".into()));
        }
        fmt.chroma_format_idc = chroma as u8;
        fmt.bit_depth_luma_minus8 = luma as u8;
        fmt.bit_depth_chroma_minus8 = chroma_depth as u8;
    }
    Ok(fmt)
}

fn push_unique<'a>(list: &mut Vec<&'a [u8]>, nal: &'a [u8]) {
    if !list.contains(&nal) {
        list.push(nal);
    }
}

fn push_nal_u16(out: &mut Vec<u8>, nal: &[u8]) -> Result<(), MuxError> {
    let len = u16::try_from(nal.len()).map_err(|_| MuxError::InvalidData("parameter set larger than 64 KiB".into()))?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(nal);
    Ok(())
}

/// Builds an `AVCDecoderConfigurationRecord` from NAL units (e.g. those of a keyframe).
pub(crate) fn avcc_from_nals<'a>(nals: impl Iterator<Item = &'a [u8]>) -> Result<Vec<u8>, MuxError> {
    let mut sps_list = Vec::new();
    let mut pps_list = Vec::new();
    for nal in nals {
        match nal_type(VideoCodec::H264, nal) {
            Some(h264::SPS) => push_unique(&mut sps_list, nal),
            Some(h264::PPS) => push_unique(&mut pps_list, nal),
            _ => {}
        }
    }
    let sps = *sps_list.first().ok_or_else(|| MuxError::InvalidData("no H.264 SPS in access unit".into()))?;
    if pps_list.is_empty() {
        return Err(MuxError::InvalidData("no H.264 PPS in access unit".into()));
    }
    if sps_list.len() > 31 || pps_list.len() > 255 {
        return Err(MuxError::InvalidData("too many H.264 parameter sets".into()));
    }
    let (head, _) = sps.split_first_chunk::<4>().ok_or_else(truncated)?;
    let [_, profile, compat, level] = *head;
    let mut out = Vec::with_capacity(16 + sps_list.iter().chain(&pps_list).map(|n| n.len() + 2).sum::<usize>());
    // configurationVersion, profile, compatibility, level, 6 reserved bits + lengthSizeMinusOne = 3.
    out.extend_from_slice(&[1, profile, compat, level, 0xFF, 0xE0 | sps_list.len() as u8]);
    for n in &sps_list {
        push_nal_u16(&mut out, n)?;
    }
    out.push(pps_list.len() as u8);
    for n in &pps_list {
        push_nal_u16(&mut out, n)?;
    }
    if H264_HIGH_PROFILES.contains(&profile) {
        // The extension is optional for decoders; a SPS we cannot parse leaves it out instead of failing.
        match parse_h264_sps_format(sps) {
            Ok(f) => out.extend_from_slice(&[0xFC | f.chroma_format_idc, 0xF8 | f.bit_depth_luma_minus8, 0xF8 | f.bit_depth_chroma_minus8, 0]),
            Err(e) => log::warn!("avcC: omitting High-profile extension, SPS not parsed: {e}"),
        }
    }
    if out.len() > MAX_CODEC_PRIVATE {
        return Err(MuxError::TooLarge("avcC record".into()));
    }
    Ok(out)
}

/// Builds an `AVCDecoderConfigurationRecord` (ISO/IEC 14496-15 §5.3.3, `lengthSizeMinusOne` = 3) from
/// the SPS/PPS NAL units of an Annex B keyframe access unit.
pub fn avcc_from_annexb(annexb: &[u8]) -> Result<Vec<u8>, MuxError> {
    avcc_from_nals(nal_units(annexb))
}

/// Fields of an HEVC SPS needed for the `hvcC` record.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct HevcSpsInfo {
    /// `general_profile_space` … `general_level_idc` of `profile_tier_level` (12 bytes, byte-aligned).
    pub general_ptl: [u8; 12],
    pub max_sub_layers_minus1: u8,
    pub temporal_id_nesting: bool,
    pub chroma_format_idc: u8,
    pub width: u32,
    pub height: u32,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
}

/// Parses an HEVC SPS NAL unit (2-byte header included) up to the bit depths (H.265 §7.3.2.2).
pub fn parse_hevc_sps(sps: &[u8]) -> Result<HevcSpsInfo, MuxError> {
    if nal_type(VideoCodec::Hevc, sps) != Some(hevc::SPS) {
        return Err(MuxError::InvalidData("not an HEVC SPS".into()));
    }
    let mut rbsp = Vec::new();
    unescape_rbsp(sps.get(2..).unwrap_or_default(), &mut rbsp);
    let first = *rbsp.first().ok_or_else(truncated)?;
    let max_sub_layers_minus1 = (first >> 1) & 7;
    if max_sub_layers_minus1 > 6 {
        return Err(MuxError::InvalidData("HEVC sps_max_sub_layers_minus1 > 6".into()));
    }
    let temporal_id_nesting = first & 1 == 1;
    let general_ptl: [u8; 12] = rbsp.get(1..13).and_then(|s| s.try_into().ok()).ok_or_else(truncated)?;
    let mut br = BitReader::new(&rbsp);
    br.skip(8 + 96)?;
    // profile_tier_level(1, sps_max_sub_layers_minus1), H.265 §7.3.3.
    let n = usize::from(max_sub_layers_minus1);
    let mut present = [(false, false); 8];
    for p in present.iter_mut().take(n) {
        *p = (br.bit()? == 1, br.bit()? == 1);
    }
    if n > 0 {
        br.skip(2 * (8 - n))?; // reserved_zero_2bits
    }
    for &(profile, level) in present.iter().take(n) {
        if profile {
            br.skip(88)?; // sub_layer profile space .. reserved bits
        }
        if level {
            br.skip(8)?; // sub_layer_level_idc
        }
    }
    let sps_id = br.ue()?;
    if sps_id > 15 {
        return Err(MuxError::InvalidData(format!("HEVC sps_seq_parameter_set_id {sps_id}")));
    }
    let chroma = br.ue()?;
    if chroma > 3 {
        return Err(MuxError::InvalidData(format!("HEVC chroma_format_idc {chroma}")));
    }
    if chroma == 3 {
        br.skip(1)?; // separate_colour_plane_flag
    }
    let width = br.ue()?;
    let height = br.ue()?;
    if br.bit()? == 1 {
        for _ in 0..4 {
            br.ue()?; // conformance window offsets
        }
    }
    let luma = br.ue()?;
    let chroma_depth = br.ue()?;
    if luma > 7 || chroma_depth > 7 {
        return Err(MuxError::InvalidData("HEVC bit depth out of range".into()));
    }
    Ok(HevcSpsInfo {
        general_ptl,
        max_sub_layers_minus1,
        temporal_id_nesting,
        chroma_format_idc: chroma as u8,
        width,
        height,
        bit_depth_luma_minus8: luma as u8,
        bit_depth_chroma_minus8: chroma_depth as u8,
    })
}

/// Builds an `HEVCDecoderConfigurationRecord` from NAL units (e.g. those of a keyframe).
pub(crate) fn hvcc_from_nals<'a>(nals: impl Iterator<Item = &'a [u8]>) -> Result<Vec<u8>, MuxError> {
    let mut vps = Vec::new();
    let mut sps = Vec::new();
    let mut pps = Vec::new();
    for nal in nals {
        match nal_type(VideoCodec::Hevc, nal) {
            Some(hevc::VPS) => push_unique(&mut vps, nal),
            Some(hevc::SPS) => push_unique(&mut sps, nal),
            Some(hevc::PPS) => push_unique(&mut pps, nal),
            _ => {}
        }
    }
    if vps.is_empty() || sps.is_empty() || pps.is_empty() {
        return Err(MuxError::InvalidData("HEVC access unit lacks VPS, SPS or PPS".into()));
    }
    let info = parse_hevc_sps(sps.first().copied().unwrap_or_default())?;
    let mut out = Vec::with_capacity(64 + vps.iter().chain(&sps).chain(&pps).map(|n| n.len() + 2).sum::<usize>());
    out.push(1); // configurationVersion
    out.extend_from_slice(&info.general_ptl);
    out.extend_from_slice(&[0xF0, 0x00]); // reserved + min_spatial_segmentation_idc = 0 (unknown)
    out.push(0xFC); // reserved + parallelismType = 0 (unknown)
    out.push(0xFC | info.chroma_format_idc);
    out.push(0xF8 | info.bit_depth_luma_minus8);
    out.push(0xF8 | info.bit_depth_chroma_minus8);
    out.extend_from_slice(&[0, 0]); // avgFrameRate = 0 (unspecified)
    // constantFrameRate = 0, numTemporalLayers, temporalIdNested, lengthSizeMinusOne = 3.
    out.push(((info.max_sub_layers_minus1 + 1) << 3) | (u8::from(info.temporal_id_nesting) << 2) | 3);
    out.push(3); // numOfArrays
    for (typ, list) in [(hevc::VPS, &vps), (hevc::SPS, &sps), (hevc::PPS, &pps)] {
        let count = u16::try_from(list.len()).map_err(|_| MuxError::InvalidData("too many HEVC parameter sets".into()))?;
        out.push(0x80 | typ); // array_completeness = 1, reserved = 0, NAL_unit_type
        out.extend_from_slice(&count.to_be_bytes());
        for n in list {
            push_nal_u16(&mut out, n)?;
        }
    }
    if out.len() > MAX_CODEC_PRIVATE {
        return Err(MuxError::TooLarge("hvcC record".into()));
    }
    Ok(out)
}

/// Builds an `HEVCDecoderConfigurationRecord` (ISO/IEC 14496-15 §8.3.3, `lengthSizeMinusOne` = 3,
/// complete VPS/SPS/PPS arrays) from an Annex B keyframe access unit.
pub fn hvcc_from_annexb(annexb: &[u8]) -> Result<Vec<u8>, MuxError> {
    hvcc_from_nals(nal_units(annexb))
}

/// Builds the decoder configuration record of `codec` from length-prefixed NAL units (used when a file
/// carries its parameter sets only in-band).
pub fn config_from_length_prefixed(codec: VideoCodec, data: &[u8]) -> Result<Vec<u8>, MuxError> {
    match codec {
        VideoCodec::H264 => avcc_from_nals(length_prefixed_nal_units(data)),
        VideoCodec::Hevc => hvcc_from_nals(length_prefixed_nal_units(data)),
    }
}

/// MPEG-4 audio sampling frequency table (ISO/IEC 14496-3 Table 1.18).
const AAC_RATES: [u32; 13] = [96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000, 7_350];

/// The 2-byte AAC-LC `AudioSpecificConfig` (object type 2, frame length 1024) for a sample rate from the
/// MPEG-4 table and 1–6 or 8 channels.
pub fn aac_audio_specific_config(sample_rate: u32, channels: u16) -> Result<[u8; 2], MuxError> {
    let index = AAC_RATES.iter().position(|&r| r == sample_rate).ok_or_else(|| MuxError::Unsupported(format!("AAC sample rate {sample_rate} Hz")))? as u16;
    let config: u16 = match channels {
        1..=6 => channels,
        8 => 7,
        _ => return Err(MuxError::Unsupported(format!("AAC with {channels} channels"))),
    };
    // audioObjectType (5) | samplingFrequencyIndex (4) | channelConfiguration (4) | GASpecificConfig (3 zero bits).
    let v: u16 = (2 << 11) | (index << 7) | (config << 3);
    Ok(v.to_be_bytes())
}

/// Sample rate and channel count described by an `AudioSpecificConfig` (`None` if malformed or using a
/// channel configuration of 0, i.e. a program config element).
pub fn parse_audio_specific_config(asc: &[u8]) -> Option<(u32, u16)> {
    let mut br = BitReader::new(asc);
    let mut aot = br.bits(5).ok()?;
    if aot == 31 {
        aot = 32 + br.bits(6).ok()?;
    }
    let _ = aot;
    let index = br.bits(4).ok()?;
    let rate = if index == 15 { br.bits(24).ok()? } else { *AAC_RATES.get(index as usize)? };
    let channels = match br.bits(4).ok()? {
        0 => return None,
        c @ 1..=6 => c as u16,
        7 => 8,
        _ => return None,
    };
    (rate > 0).then_some((rate, channels))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal MSB-first bit writer for crafting parameter sets.
    struct BitWriter {
        out: Vec<u8>,
        bit: u32,
    }

    impl BitWriter {
        fn new() -> Self {
            BitWriter { out: Vec::new(), bit: 0 }
        }
        fn put(&mut self, v: u32, n: u32) {
            for i in (0..n).rev() {
                if self.bit.is_multiple_of(8) {
                    self.out.push(0);
                }
                if (v >> i) & 1 == 1 {
                    *self.out.last_mut().unwrap() |= 0x80 >> (self.bit % 8);
                }
                self.bit += 1;
            }
        }
        fn ue(&mut self, v: u32) {
            let x = v + 1;
            let len = 32 - x.leading_zeros();
            self.put(0, len - 1);
            self.put(x, len);
        }
        fn finish(mut self) -> Vec<u8> {
            self.put(1, 1); // rbsp_stop_one_bit
            while !self.bit.is_multiple_of(8) {
                self.put(0, 1);
            }
            self.out
        }
    }

    #[test]
    fn splits_three_and_four_byte_start_codes() {
        let s = [0, 0, 0, 1, 0x09, 0xF0, 0, 0, 1, 0x67, 0x64, 0, 0, 0, 0, 1, 0x65, 0x88, 0x84, 0, 0];
        let v: Vec<&[u8]> = nal_units(&s).collect();
        assert_eq!(v, vec![&[0x09, 0xF0][..], &[0x67, 0x64][..], &[0x65, 0x88, 0x84][..]]);
        // Garbage before the first start code and empty NAL units.
        let s = [7, 7, 0, 0, 1, 0, 0, 1, 0x41, 0];
        let v: Vec<&[u8]> = nal_units(&s).collect();
        assert_eq!(v, vec![&[0x41][..]]);
        assert_eq!(nal_units(&[]).count(), 0);
        assert_eq!(nal_units(&[0, 0]).count(), 0);
        assert_eq!(nal_units(&[1, 2, 3]).count(), 0);
    }

    #[test]
    fn converts_to_length_prefixed_and_drops_aud() {
        let s = [0, 0, 0, 1, 0x09, 0xF0, 0, 0, 1, 0x67, 0x64, 0, 0, 1, 0x65, 0x88];
        let mut out = vec![9; 3];
        annexb_to_length_prefixed(VideoCodec::H264, &s, &mut out).unwrap();
        assert_eq!(out, [0, 0, 0, 2, 0x67, 0x64, 0, 0, 0, 2, 0x65, 0x88]);
        assert!(is_keyframe_length_prefixed(VideoCodec::H264, &out));
        assert!(is_keyframe_annexb(VideoCodec::H264, &s));
        let nals: Vec<&[u8]> = length_prefixed_nal_units(&out).collect();
        assert_eq!(nals.len(), 2);
        // HEVC AUD (type 35 → 0x46 0x01) dropped, IDR_W_RADL (19 → 0x26 0x01) is a keyframe.
        let s = [0, 0, 1, 0x46, 0x01, 0x50, 0, 0, 1, 0x26, 0x01, 0xAF];
        annexb_to_length_prefixed(VideoCodec::Hevc, &s, &mut out).unwrap();
        assert_eq!(out, [0, 0, 0, 3, 0x26, 0x01, 0xAF]);
        assert!(is_keyframe_length_prefixed(VideoCodec::Hevc, &out));
        // TRAIL_R (1 → 0x02 0x01) is not.
        assert!(!is_keyframe_annexb(VideoCodec::Hevc, &[0, 0, 1, 0x02, 0x01, 0xAF]));
        assert!(annexb_to_length_prefixed(VideoCodec::H264, &[0, 0, 1, 0x09, 0x10], &mut out).is_err());
        // Corrupt length prefix stops iteration.
        assert_eq!(length_prefixed_nal_units(&[0, 0, 0, 9, 1, 2]).count(), 0);
    }

    #[test]
    fn rbsp_unescape() {
        let mut out = Vec::new();
        unescape_rbsp(&[0, 0, 3, 1, 0, 0, 3, 0, 0, 3], &mut out);
        assert_eq!(out, [0, 0, 1, 0, 0, 0, 0]);
    }

    #[test]
    fn exp_golomb() {
        let mut w = BitWriter::new();
        for v in [0, 1, 2, 3, 7, 255, 65_535] {
            w.ue(v);
        }
        let data = w.finish();
        let mut r = BitReader::new(&data);
        for v in [0, 1, 2, 3, 7, 255, 65_535] {
            assert_eq!(r.ue().unwrap(), v);
        }
        assert!(BitReader::new(&[0, 0, 0, 0, 0]).ue().is_err());
    }

    /// High-profile SPS: profile 100, level 40, 4:2:0, 8-bit (+ some trailing fields).
    fn high_sps(chroma: u32, depth: u32) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.put(100, 8);
        w.put(0, 8);
        w.put(40, 8);
        w.ue(0); // sps id
        w.ue(chroma);
        if chroma == 3 {
            w.put(0, 1);
        }
        w.ue(depth);
        w.ue(depth);
        w.put(0, 1); // qpprime
        w.put(0, 1); // scaling matrix
        w.ue(0); // log2_max_frame_num_minus4
        let mut nal = vec![0x67];
        nal.extend(w.finish());
        nal
    }

    #[test]
    fn avcc_known_vector() {
        let sps = high_sps(1, 0);
        let pps = [0x68, 0xEB, 0xE3, 0xCB, 0x22, 0xC0];
        let mut au = vec![0, 0, 0, 1];
        au.extend(&sps);
        au.extend([0, 0, 0, 1]);
        au.extend(pps);
        au.extend([0, 0, 1, 0x65, 0x88, 0x80]);
        let avcc = avcc_from_annexb(&au).unwrap();
        let mut want = vec![1, 100, 0, 40, 0xFF, 0xE1, 0, sps.len() as u8];
        want.extend(&sps);
        want.extend([1, 0, 6]);
        want.extend(pps);
        want.extend([0xFD, 0xF8, 0xF8, 0]);
        assert_eq!(avcc, want);

        // 4:4:4 10-bit.
        let f = parse_h264_sps_format(&high_sps(3, 2)).unwrap();
        assert_eq!((f.chroma_format_idc, f.bit_depth_luma_minus8, f.bit_depth_chroma_minus8), (3, 2, 2));
    }

    #[test]
    fn avcc_real_x264_sps() {
        // SPS + PPS of an x264 640x360 High@L3.0 stream (ffmpeg testsrc2 fixture, see tests/oracle.rs) and
        // the avcC ffmpeg's MP4 muxer writes for it (an oracle-checked vector, not ffmpeg code).
        let sps: &[u8] = &[
            0x67, 0x64, 0x00, 0x1e, 0xac, 0xb2, 0x01, 0x40, 0x5f, 0xf2, 0xe0, 0x22, 0x00, 0x00, 0x03, 0x00, 0x02, 0x00, 0x00, 0x03, 0x00, 0x78, 0x1e, 0x2c,
            0x5c, 0x90,
        ];
        let pps: &[u8] = &[0x68, 0xeb, 0xc3, 0xcb, 0x22, 0xc0];
        let want: &[u8] = &[
            0x01, 0x64, 0x00, 0x1e, 0xff, 0xe1, 0x00, 0x1a, 0x67, 0x64, 0x00, 0x1e, 0xac, 0xb2, 0x01, 0x40, 0x5f, 0xf2, 0xe0, 0x22, 0x00, 0x00, 0x03, 0x00,
            0x02, 0x00, 0x00, 0x03, 0x00, 0x78, 0x1e, 0x2c, 0x5c, 0x90, 0x01, 0x00, 0x06, 0x68, 0xeb, 0xc3, 0xcb, 0x22, 0xc0, 0xfd, 0xf8, 0xf8, 0x00,
        ];
        let mut au = vec![0, 0, 0, 1];
        au.extend(sps);
        au.extend([0, 0, 0, 1]);
        au.extend(pps);
        au.extend([0, 0, 1, 0x65, 0x88, 0x84]);
        assert_eq!(avcc_from_annexb(&au).unwrap(), want);
        let f = parse_h264_sps_format(sps).unwrap();
        assert_eq!((f.profile_idc, f.level_idc, f.chroma_format_idc), (100, 30, 1));
    }

    #[test]
    fn avcc_rejects_missing_or_truncated_sets() {
        assert!(avcc_from_annexb(&[0, 0, 1, 0x65, 0x88]).is_err());
        assert!(avcc_from_annexb(&[0, 0, 1, 0x67, 0x64, 0, 0, 1, 0x68, 0xEB]).is_err()); // SPS too short
        assert!(avcc_from_annexb(&[0, 0, 1, 0x67, 0x64, 0, 0x1e]).is_err()); // no PPS
        // Truncated High SPS: the extension is omitted, the record still builds.
        let avcc = avcc_from_annexb(&[0, 0, 1, 0x67, 0x64, 0x00, 0x1e, 0, 0, 1, 0x68, 0xEB]).unwrap();
        assert_eq!(avcc.len(), 6 + 2 + 4 + 1 + 2 + 2);
    }

    fn hevc_sps(max_sub_layers_minus1: u32) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.put(0, 4); // vps id
        w.put(max_sub_layers_minus1, 3);
        w.put(1, 1); // temporal id nesting
        // general PTL: space 0, tier 0, profile 1 (Main)
        w.put(1, 8);
        w.put(0x6000_0000, 32); // compat flags 1 and 2
        w.put(0x90, 8); // progressive + frame_only
        w.put(0, 32);
        w.put(0, 8);
        w.put(93, 8); // level 3.1
        for i in 0..max_sub_layers_minus1 {
            w.put(u32::from(i == 0), 1); // profile present for sub-layer 0 only
            w.put(1, 1); // level present
        }
        if max_sub_layers_minus1 > 0 {
            for _ in max_sub_layers_minus1..8 {
                w.put(0, 2);
            }
        }
        for i in 0..max_sub_layers_minus1 {
            if i == 0 {
                w.put(0, 32);
                w.put(0, 32);
                w.put(0, 24);
            }
            w.put(90, 8);
        }
        w.ue(0); // sps id
        w.ue(1); // 4:2:0
        w.ue(640);
        w.ue(368);
        w.put(1, 1); // conformance window
        w.ue(0);
        w.ue(0);
        w.ue(0);
        w.ue(4);
        w.ue(2); // luma 10-bit
        w.ue(2);
        let mut nal = vec![0x42, 0x01];
        nal.extend(w.finish());
        nal
    }

    #[test]
    fn hvcc_known_vector() {
        for msl in [0, 2] {
            let sps = hevc_sps(msl);
            let info = parse_hevc_sps(&sps).unwrap();
            assert_eq!((info.width, info.height, info.chroma_format_idc), (640, 368, 1));
            assert_eq!((info.bit_depth_luma_minus8, info.bit_depth_chroma_minus8), (2, 2));
            assert_eq!(info.max_sub_layers_minus1, msl as u8);
            let vps = [0x40, 0x01, 0x0C];
            let pps = [0x44, 0x01, 0xC1];
            let mut au = Vec::new();
            for n in [&vps[..], &sps, &pps, &[0x26, 0x01, 0xAF]] {
                au.extend([0, 0, 0, 1]);
                au.extend(n);
            }
            let hvcc = hvcc_from_annexb(&au).unwrap();
            let mut want = vec![1, 0x01, 0x60, 0, 0, 0, 0x90, 0, 0, 0, 0, 0, 93, 0xF0, 0, 0xFC, 0xFD, 0xFA, 0xFA, 0, 0];
            want.push((((msl + 1) as u8) << 3) | 0x04 | 3);
            want.push(3);
            want.extend([0xA0, 0, 1, 0, 3]);
            want.extend(vps);
            want.extend([0xA1, 0, 1, 0, sps.len() as u8]);
            want.extend(&sps);
            want.extend([0xA2, 0, 1, 0, 3]);
            want.extend(pps);
            assert_eq!(hvcc, want, "max_sub_layers_minus1 = {msl}");
        }
        assert!(hvcc_from_annexb(&[0, 0, 1, 0x42, 0x01, 0x01]).is_err());
    }

    #[test]
    fn audio_specific_config() {
        assert_eq!(aac_audio_specific_config(48_000, 2).unwrap(), [0x11, 0x90]);
        assert_eq!(aac_audio_specific_config(44_100, 2).unwrap(), [0x12, 0x10]);
        assert_eq!(aac_audio_specific_config(48_000, 1).unwrap(), [0x11, 0x88]);
        assert_eq!(aac_audio_specific_config(48_000, 8).unwrap(), [0x11, 0xB8]);
        assert!(aac_audio_specific_config(47_000, 2).is_err());
        assert!(aac_audio_specific_config(48_000, 7).is_err());
        assert!(aac_audio_specific_config(48_000, 0).is_err());
        assert_eq!(parse_audio_specific_config(&[0x11, 0x90]), Some((48_000, 2)));
        assert_eq!(parse_audio_specific_config(&[0x11]), None);
    }

    #[test]
    fn parsers_survive_garbage() {
        let mut seed = 0x1234_5678_u32;
        for len in 0..200 {
            let data: Vec<u8> = (0..len)
                .map(|_| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (seed >> 24) as u8
                })
                .collect();
            let _ = nal_units(&data).count();
            let _ = length_prefixed_nal_units(&data).count();
            let _ = avcc_from_annexb(&data);
            let _ = hvcc_from_annexb(&data);
            let mut sps = vec![0x67];
            sps.extend(&data);
            let _ = parse_h264_sps_format(&sps);
            let mut sps = vec![0x42, 0x01];
            sps.extend(&data);
            let _ = parse_hevc_sps(&sps);
            let _ = parse_audio_specific_config(&data);
        }
    }
}
