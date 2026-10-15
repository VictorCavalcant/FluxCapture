//! Conversion of any device channel layout to stereo.
//!
//! WASAPI delivers channels in WAVE order (FL, FR, FC, LFE, BL, BR, FLC, FRC, BC, SL, SR…); without
//! the channel mask the common layouts are recognised by count. Downmix follows the ITU-R BS.775 /
//! Dolby Lo/Ro convention: centre and surrounds at -3 dB into their side, LFE discarded. Rows are not
//! normalised: stereo content carried in a surround mix format (the usual case for loopback on a
//! 5.1/7.1 device) keeps unity gain, and the master limiter catches the rare full-scale surround peak.

use crate::util::clean;

const M3: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// `(gain into L, gain into R)` for each input channel.
pub(crate) fn stereo_matrix(channels: usize) -> Vec<[f32; 2]> {
    const FL: [f32; 2] = [1.0, 0.0];
    const FR: [f32; 2] = [0.0, 1.0];
    const FC: [f32; 2] = [M3, M3];
    const LFE: [f32; 2] = [0.0, 0.0];
    const SL: [f32; 2] = [M3, 0.0];
    const SR: [f32; 2] = [0.0, M3];
    const BC: [f32; 2] = [0.5, 0.5];
    match channels {
        0 => Vec::new(),
        1 => vec![[1.0, 1.0]],
        2 => vec![FL, FR],
        3 => vec![FL, FR, FC],
        4 => vec![FL, FR, SL, SR],     // quad
        5 => vec![FL, FR, FC, SL, SR], // 5.0
        6 => vec![FL, FR, FC, LFE, SL, SR],
        7 => vec![FL, FR, FC, LFE, BC, SL, SR], // 6.1
        _ => {
            // 7.1 (FL FR FC LFE BL BR SL SR); anything beyond alternates into L/R at -6 dB.
            let mut m = vec![FL, FR, FC, LFE, SL, SR, SL, SR];
            m.extend((8..channels).map(|i| if i % 2 == 0 { [0.5, 0.0] } else { [0.0, 0.5] }));
            m
        }
    }
}

/// Converts `frames` interleaved frames of `matrix.len()` channels into planar stereo, cleaning
/// NaN/inf. `silent` writes zeros.
pub(crate) fn to_stereo(src: &[f32], matrix: &[[f32; 2]], l: &mut [f32], r: &mut [f32], silent: bool) {
    let ch = matrix.len();
    if silent || ch == 0 {
        l.fill(0.0);
        r.fill(0.0);
        return;
    }
    match ch {
        1 => {
            for ((&x, l), r) in src.iter().zip(l.iter_mut()).zip(r.iter_mut()) {
                let v = clean(x);
                *l = v;
                *r = v;
            }
        }
        2 => {
            for ((f, l), r) in src.as_chunks::<2>().0.iter().zip(l.iter_mut()).zip(r.iter_mut()) {
                *l = clean(f[0]);
                *r = clean(f[1]);
            }
        }
        _ => {
            for ((f, l), r) in src.chunks_exact(ch).zip(l.iter_mut()).zip(r.iter_mut()) {
                let (mut a, mut b) = (0.0, 0.0);
                for (&x, g) in f.iter().zip(matrix) {
                    let v = clean(x);
                    a += v * g[0];
                    b += v * g[1];
                }
                *l = a;
                *r = b;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surround_downmix() {
        let m = stereo_matrix(6);
        let mut l = [0.0; 1];
        let mut r = [0.0; 1];
        to_stereo(&[0.0, 0.0, 1.0, 0.0, 0.0, 0.0], &m, &mut l, &mut r, false);
        assert!((l[0] - M3).abs() < 1e-6 && (r[0] - M3).abs() < 1e-6);
        to_stereo(&[0.0, 0.0, 0.0, 1.0, 0.0, 0.0], &m, &mut l, &mut r, false);
        assert_eq!((l[0], r[0]), (0.0, 0.0));
        to_stereo(&[f32::NAN, 0.5, 0.0, 0.0, 0.0, f32::INFINITY], &m, &mut l, &mut r, false);
        assert_eq!((l[0], r[0]), (0.0, 0.5));
        assert_eq!(stereo_matrix(12).len(), 12);
    }
}
