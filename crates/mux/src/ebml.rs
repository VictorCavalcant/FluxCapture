//! EBML (RFC 8794) primitives and the Matroska (RFC 9559) element IDs used by the writer and reader.

/// Element IDs (with their length marker bits, as they appear in the file).
#[allow(dead_code)]
pub(crate) mod id {
    pub const EBML: u64 = 0x1A45_DFA3;
    pub const EBML_VERSION: u64 = 0x4286;
    pub const EBML_READ_VERSION: u64 = 0x42F7;
    pub const EBML_MAX_ID_LENGTH: u64 = 0x42F2;
    pub const EBML_MAX_SIZE_LENGTH: u64 = 0x42F3;
    pub const DOC_TYPE: u64 = 0x4282;
    pub const DOC_TYPE_VERSION: u64 = 0x4287;
    pub const DOC_TYPE_READ_VERSION: u64 = 0x4285;
    pub const VOID: u64 = 0xEC;
    pub const CRC32: u64 = 0xBF;

    pub const SEGMENT: u64 = 0x1853_8067;
    pub const SEEK_HEAD: u64 = 0x114D_9B74;
    pub const SEEK: u64 = 0x4DBB;
    pub const SEEK_ID: u64 = 0x53AB;
    pub const SEEK_POSITION: u64 = 0x53AC;

    pub const INFO: u64 = 0x1549_A966;
    pub const TIMESTAMP_SCALE: u64 = 0x2A_D7B1;
    pub const DURATION: u64 = 0x4489;
    pub const DATE_UTC: u64 = 0x4461;
    pub const TITLE: u64 = 0x7BA9;
    pub const MUXING_APP: u64 = 0x4D80;
    pub const WRITING_APP: u64 = 0x5741;

    pub const TRACKS: u64 = 0x1654_AE6B;
    pub const TRACK_ENTRY: u64 = 0xAE;
    pub const TRACK_NUMBER: u64 = 0xD7;
    pub const TRACK_UID: u64 = 0x73C5;
    pub const TRACK_TYPE: u64 = 0x83;
    pub const FLAG_ENABLED: u64 = 0xB9;
    pub const FLAG_DEFAULT: u64 = 0x88;
    pub const FLAG_LACING: u64 = 0x9C;
    pub const DEFAULT_DURATION: u64 = 0x23_E383;
    pub const NAME: u64 = 0x536E;
    pub const LANGUAGE: u64 = 0x22_B59C;
    pub const CODEC_ID: u64 = 0x86;
    pub const CODEC_PRIVATE: u64 = 0x63A2;
    pub const CONTENT_ENCODINGS: u64 = 0x6D80;
    pub const VIDEO: u64 = 0xE0;
    pub const PIXEL_WIDTH: u64 = 0xB0;
    pub const PIXEL_HEIGHT: u64 = 0xBA;
    pub const COLOUR: u64 = 0x55B0;
    pub const MATRIX_COEFFICIENTS: u64 = 0x55B1;
    pub const RANGE: u64 = 0x55B9;
    pub const TRANSFER_CHARACTERISTICS: u64 = 0x55BA;
    pub const PRIMARIES: u64 = 0x55BB;
    pub const AUDIO: u64 = 0xE1;
    pub const SAMPLING_FREQUENCY: u64 = 0xB5;
    pub const CHANNELS: u64 = 0x9F;
    pub const BIT_DEPTH: u64 = 0x6264;

    pub const CLUSTER: u64 = 0x1F43_B675;
    pub const TIMESTAMP: u64 = 0xE7;
    pub const SILENT_TRACKS: u64 = 0x5854;
    pub const POSITION: u64 = 0xA7;
    pub const PREV_SIZE: u64 = 0xAB;
    pub const SIMPLE_BLOCK: u64 = 0xA3;
    pub const BLOCK_GROUP: u64 = 0xA0;
    pub const BLOCK: u64 = 0xA1;
    pub const BLOCK_DURATION: u64 = 0x9B;
    pub const REFERENCE_BLOCK: u64 = 0xFB;
    pub const ENCRYPTED_BLOCK: u64 = 0xAF;

    pub const CUES: u64 = 0x1C53_BB6B;
    pub const CUE_POINT: u64 = 0xBB;
    pub const CUE_TIME: u64 = 0xB3;
    pub const CUE_TRACK_POSITIONS: u64 = 0xB7;
    pub const CUE_TRACK: u64 = 0xF7;
    pub const CUE_CLUSTER_POSITION: u64 = 0xF1;
    pub const CUE_RELATIVE_POSITION: u64 = 0xF0;

    pub const CHAPTERS: u64 = 0x1043_A770;
    pub const TAGS: u64 = 0x1254_C367;
    pub const ATTACHMENTS: u64 = 0x1941_A469;
}

/// Largest value a vint can carry (the all-ones pattern of 8 bytes means "unknown size").
pub(crate) const VINT_MAX: u64 = (1u64 << 56) - 2;

/// The 8-byte "unknown size" vint.
pub(crate) const UNKNOWN_SIZE_8: [u8; 8] = [0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];

/// Number of bytes of an element ID (IDs carry their own length marker).
pub(crate) fn id_len(id: u64) -> usize {
    match id {
        0..=0xFF => 1,
        0x100..=0xFFFF => 2,
        0x1_0000..=0xFF_FFFF => 3,
        _ => 4,
    }
}

pub(crate) fn put_id(buf: &mut Vec<u8>, id: u64) {
    let n = id_len(id);
    buf.extend_from_slice(id.to_be_bytes().get(8 - n..).unwrap_or_default());
}

/// Shortest vint length for `v` (all-ones patterns are reserved, so `2^(7n) - 1` needs `n + 1` bytes).
pub(crate) fn vint_len(v: u64) -> usize {
    (1..=8).find(|&n| v < (1u64 << (7 * n)) - 1).unwrap_or(8)
}

/// Writes `v` as an `n`-byte vint. `v` must fit (callers check against [`VINT_MAX`]).
pub(crate) fn put_vint_n(buf: &mut Vec<u8>, v: u64, n: usize) {
    let n = n.clamp(1, 8);
    let marked = v.min(VINT_MAX) | (1u64 << (7 * n));
    buf.extend_from_slice(marked.to_be_bytes().get(8 - n..).unwrap_or_default());
}

/// Writes `v` as the shortest vint.
pub(crate) fn put_size(buf: &mut Vec<u8>, v: u64) {
    put_vint_n(buf, v, vint_len(v));
}

fn uint_len(v: u64) -> usize {
    ((64 - v.leading_zeros() as usize).div_ceil(8)).max(1)
}

pub(crate) fn put_uint(buf: &mut Vec<u8>, id: u64, v: u64) {
    put_id(buf, id);
    let n = uint_len(v);
    put_size(buf, n as u64);
    buf.extend_from_slice(v.to_be_bytes().get(8 - n..).unwrap_or_default());
}

/// An unsigned integer padded to 8 bytes so it can be patched in place later.
pub(crate) fn put_uint8(buf: &mut Vec<u8>, id: u64, v: u64) {
    put_id(buf, id);
    put_size(buf, 8);
    buf.extend_from_slice(&v.to_be_bytes());
}

pub(crate) fn put_int8(buf: &mut Vec<u8>, id: u64, v: i64) {
    put_id(buf, id);
    put_size(buf, 8);
    buf.extend_from_slice(&v.to_be_bytes());
}

pub(crate) fn put_float(buf: &mut Vec<u8>, id: u64, v: f64) {
    put_id(buf, id);
    put_size(buf, 8);
    buf.extend_from_slice(&v.to_be_bytes());
}

pub(crate) fn put_bytes(buf: &mut Vec<u8>, id: u64, data: &[u8]) {
    put_id(buf, id);
    put_size(buf, data.len() as u64);
    buf.extend_from_slice(data);
}

pub(crate) fn put_str(buf: &mut Vec<u8>, id: u64, s: &str) {
    put_bytes(buf, id, s.as_bytes());
}

pub(crate) fn put_master(buf: &mut Vec<u8>, id: u64, body: &[u8]) {
    put_bytes(buf, id, body);
}

/// A Void element of exactly `total` bytes (`total >= 2`; smaller requests write nothing).
pub(crate) fn put_void(buf: &mut Vec<u8>, total: usize) {
    if total < 2 {
        return;
    }
    buf.push(0xEC);
    if total <= 128 {
        put_vint_n(buf, (total - 2) as u64, 1);
        buf.resize(buf.len() + total - 2, 0);
    } else {
        put_vint_n(buf, (total - 9) as u64, 8);
        buf.resize(buf.len() + total - 9, 0);
    }
}

/// Decodes a vint at the start of `data`: `(value, length, is_unknown)` with the marker removed.
/// `None` if `data` is too short or the first byte is zero (length > 8).
pub(crate) fn read_vint(data: &[u8]) -> Option<(u64, usize, bool)> {
    let first = *data.first()?;
    if first == 0 {
        return None;
    }
    let len = first.leading_zeros() as usize + 1;
    let mut v = u64::from(first) & (0xFF >> len);
    for &b in data.get(1..len)? {
        v = (v << 8) | u64::from(b);
    }
    let unknown = v == (1u64 << (7 * len)) - 1;
    Some((v, len, unknown))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vint_lengths_avoid_reserved_patterns() {
        assert_eq!(vint_len(0), 1);
        assert_eq!(vint_len(126), 1);
        assert_eq!(vint_len(127), 2); // 0xFF would mean "unknown"
        assert_eq!(vint_len(16_382), 2);
        assert_eq!(vint_len(16_383), 3);
        assert_eq!(vint_len(VINT_MAX), 8);
        let mut b = Vec::new();
        put_size(&mut b, 1);
        put_size(&mut b, 127);
        put_size(&mut b, 0x1234);
        assert_eq!(b, [0x81, 0x40, 0x7F, 0x52, 0x34]);
        for v in [0, 1, 126, 127, 128, 16_382, 16_383, 1 << 20, 1 << 40, VINT_MAX] {
            let mut b = Vec::new();
            put_size(&mut b, v);
            assert_eq!(read_vint(&b), Some((v, b.len(), false)), "{v}");
        }
        assert_eq!(read_vint(&UNKNOWN_SIZE_8), Some(((1 << 56) - 1, 8, true)));
        assert_eq!(read_vint(&[0xFF]), Some((127, 1, true)));
        assert_eq!(read_vint(&[0x00, 1]), None);
        assert_eq!(read_vint(&[0x40]), None);
    }

    #[test]
    fn ids_uints_and_voids() {
        let mut b = Vec::new();
        put_id(&mut b, id::EBML);
        put_id(&mut b, id::TIMESTAMP_SCALE);
        put_id(&mut b, id::SEEK);
        put_id(&mut b, id::VOID);
        assert_eq!(b, [0x1A, 0x45, 0xDF, 0xA3, 0x2A, 0xD7, 0xB1, 0x4D, 0xBB, 0xEC]);
        let mut b = Vec::new();
        put_uint(&mut b, id::TIMESTAMP, 0);
        put_uint(&mut b, id::TIMESTAMP, 1_000_000);
        assert_eq!(b, [0xE7, 0x81, 0x00, 0xE7, 0x83, 0x0F, 0x42, 0x40]);
        for total in [2, 3, 11, 127, 128, 129, 130, 1000] {
            let mut b = Vec::new();
            put_void(&mut b, total);
            assert_eq!(b.len(), total);
            let (size, n, _) = read_vint(&b[1..]).unwrap();
            assert_eq!(1 + n + size as usize, total);
        }
    }
}
