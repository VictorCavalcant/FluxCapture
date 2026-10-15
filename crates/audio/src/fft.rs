//! A small radix-4 complex FFT for the noise suppressor (split real/imaginary arrays).
//!
//! The forward transform is decimation-in-frequency and leaves the spectrum in base-4
//! digit-reversed order; the inverse is the exact transpose (decimation-in-time) and takes that
//! order back to natural order. Spectral processing that is element-wise (gains) therefore never
//! needs the bit-reversal shuffle: [`Fft::bin_of`] says which frequency each position holds.
//! Sizes are internal constants (powers of four), never input-derived.

pub(crate) struct Fft {
    n: usize,
    /// Per stage (largest first), twiddles `w^j, w^2j, w^3j` for `j < h` with `w = e^{-2πi/4h}`,
    /// stored as `[c1, s1, c2, s2, c3, s3]` planes of length `h`.
    stages: Vec<(usize, Vec<f32>)>,
    /// Frequency index held at each position of a permuted spectrum.
    bins: Vec<usize>,
}

impl Fft {
    /// `n` is rounded up to a power of four (at least 4).
    pub(crate) fn new(n: usize) -> Fft {
        let mut size = 4;
        while size < n {
            size *= 4;
        }
        let n = size;
        let mut stages = Vec::new();
        let mut h = n / 4;
        while h >= 1 {
            let l = (4 * h) as f64;
            let mut tw = vec![0f32; 6 * h];
            for j in 0..h {
                for r in 1..=3 {
                    let a = std::f64::consts::TAU * (r * j) as f64 / l;
                    tw[(2 * (r - 1)) * h + j] = a.cos() as f32;
                    tw[(2 * (r - 1) + 1) * h + j] = a.sin() as f32;
                }
            }
            stages.push((h, tw));
            h /= 4;
        }
        let digits = n.trailing_zeros() / 2;
        let bins = (0..n)
            .map(|p| {
                let mut v = p;
                let mut out = 0;
                for _ in 0..digits {
                    out = (out << 2) | (v & 3);
                    v >>= 2;
                }
                out
            })
            .collect();
        Fft { n, stages, bins }
    }

    pub(crate) fn len(&self) -> usize {
        self.n
    }

    /// Frequency index stored at position `pos` of a forward-transformed (permuted) spectrum.
    pub(crate) fn bin_of(&self, pos: usize) -> usize {
        self.bins.get(pos).copied().unwrap_or(0)
    }

    /// In-place forward transform `X[k] = Σ x[n] e^{-2πi kn/N}` (unscaled), output permuted.
    pub(crate) fn forward(&self, re: &mut [f32], im: &mut [f32]) {
        let n = self.n;
        let (Some(re), Some(im)) = (re.get_mut(..n), im.get_mut(..n)) else {
            return;
        };
        for (h, tw) in &self.stages {
            dif_stage(re, im, *h, tw);
        }
    }

    /// In-place inverse of [`forward`](Self::forward): permuted spectrum in, natural-order signal
    /// out, scaled by N.
    pub(crate) fn inverse(&self, re: &mut [f32], im: &mut [f32]) {
        let n = self.n;
        let (Some(re), Some(im)) = (re.get_mut(..n), im.get_mut(..n)) else {
            return;
        };
        for (h, tw) in self.stages.iter().rev() {
            dit_stage(re, im, *h, tw);
        }
    }
}

/// Splits a stage's twiddle table into its six planes.
#[inline]
fn planes(tw: &[f32], h: usize) -> Option<[&[f32]; 6]> {
    let tw = tw.get(..6 * h)?;
    let (c1, rest) = tw.split_at(h);
    let (s1, rest) = rest.split_at(h);
    let (c2, rest) = rest.split_at(h);
    let (s2, rest) = rest.split_at(h);
    let (c3, s3) = rest.split_at(h);
    Some([c1, s1, c2, s2, c3, s3.get(..h)?])
}

/// Radix-4 DIF butterfly on `[ar, ai, br, bi, cr, ci, dr, di]` with twiddles
/// `[c1, s1, c2, s2, c3, s3]`: butterfly, then quarter `r` times `w^{rj}` (`w = c - i·s`).
#[inline(always)]
fn dif_bfly(x: [f32; 8], w: [f32; 6]) -> [f32; 8] {
    let [ar, ai, br, bi, cr, ci, dr, di] = x;
    let [c1, s1, c2, s2, c3, s3] = w;
    let (t0r, t0i, t1r, t1i) = (ar + cr, ai + ci, ar - cr, ai - ci);
    let (t2r, t2i) = (br + dr, bi + di);
    // (b - d)·(-i)
    let (t3r, t3i) = (bi - di, dr - br);
    let (y1r, y1i, y2r, y2i, y3r, y3i) = (t1r + t3r, t1i + t3i, t0r - t2r, t0i - t2i, t1r - t3r, t1i - t3i);
    [t0r + t2r, t0i + t2i, y1r * c1 + y1i * s1, y1i * c1 - y1r * s1, y2r * c2 + y2i * s2, y2i * c2 - y2r * s2, y3r * c3 + y3i * s3, y3i * c3 - y3r * s3]
}

/// Exact transpose of [`dif_bfly`] (times 4): quarter `r` times `w^{-rj}`, then the conjugate
/// butterfly.
#[inline(always)]
fn dit_bfly(x: [f32; 8], w: [f32; 6]) -> [f32; 8] {
    let [ar, ai, br, bi, cr, ci, dr, di] = x;
    let [c1, s1, c2, s2, c3, s3] = w;
    let (u1r, u1i) = (br * c1 - bi * s1, bi * c1 + br * s1);
    let (u2r, u2i) = (cr * c2 - ci * s2, ci * c2 + cr * s2);
    let (u3r, u3i) = (dr * c3 - di * s3, di * c3 + dr * s3);
    let (s02r, s02i, d02r, d02i) = (ar + u2r, ai + u2i, ar - u2r, ai - u2i);
    let (s13r, s13i) = (u1r + u3r, u1i + u3i);
    // (u1 - u3)·i
    let (d13r, d13i) = (u3i - u1i, u1r - u3r);
    [s02r + s13r, s02i + s13i, d02r + d13r, d02i + d13i, s02r - s13r, s02i - s13i, d02r - d13r, d02i - d13i]
}

/// Groups of a compile-time size `G = 4h` (small stages): fully unrolled inner loop.
#[inline(always)]
fn stage_fixed<const G: usize>(re: &mut [f32], im: &mut [f32], w: [&[f32]; 6], bfly: impl Fn([f32; 8], [f32; 6]) -> [f32; 8]) {
    let h = G / 4;
    if w.iter().any(|p| p.len() < h) {
        return;
    }
    for (r, m) in re.as_chunks_mut::<G>().0.iter_mut().zip(im.as_chunks_mut::<G>().0.iter_mut()) {
        for j in 0..h {
            let y = bfly(
                [r[j], m[j], r[j + h], m[j + h], r[j + 2 * h], m[j + 2 * h], r[j + 3 * h], m[j + 3 * h]],
                [w[0][j], w[1][j], w[2][j], w[3][j], w[4][j], w[5][j]],
            );
            [r[j], m[j], r[j + h], m[j + h], r[j + 2 * h], m[j + 2 * h], r[j + 3 * h], m[j + 3 * h]] = y;
        }
    }
}

/// Groups of `4h` with `h` known only at run time (large stages; the `j` loop vectorises).
#[inline(always)]
fn stage_dyn(re: &mut [f32], im: &mut [f32], h: usize, w: [&[f32]; 6], bfly: impl Fn([f32; 8], [f32; 6]) -> [f32; 8]) {
    for (rg, mg) in re.chunks_exact_mut(4 * h).zip(im.chunks_exact_mut(4 * h)) {
        let (ra, rest) = rg.split_at_mut(h);
        let (rb, rest) = rest.split_at_mut(h);
        let (rc, rd) = rest.split_at_mut(h);
        let (ma, rest) = mg.split_at_mut(h);
        let (mb, rest) = rest.split_at_mut(h);
        let (mc, md) = rest.split_at_mut(h);
        // Everything re-sliced to one length the optimiser can see: no bounds checks, vectorised.
        let n =
            [ra.len(), rb.len(), rc.len(), rd.len(), ma.len(), mb.len(), mc.len(), md.len()].into_iter().chain(w.iter().map(|p| p.len())).fold(h, usize::min);
        let (ra, rb, rc, rd, ma, mb, mc, md) = (&mut ra[..n], &mut rb[..n], &mut rc[..n], &mut rd[..n], &mut ma[..n], &mut mb[..n], &mut mc[..n], &mut md[..n]);
        let (c1, s1, c2, s2, c3, s3) = (&w[0][..n], &w[1][..n], &w[2][..n], &w[3][..n], &w[4][..n], &w[5][..n]);
        for j in 0..n {
            let y = bfly([ra[j], ma[j], rb[j], mb[j], rc[j], mc[j], rd[j], md[j]], [c1[j], s1[j], c2[j], s2[j], c3[j], s3[j]]);
            [ra[j], ma[j], rb[j], mb[j], rc[j], mc[j], rd[j], md[j]] = y;
        }
    }
}

fn run_stage(re: &mut [f32], im: &mut [f32], h: usize, tw: &[f32], bfly: impl Fn([f32; 8], [f32; 6]) -> [f32; 8] + Copy) {
    const ONE: [f32; 1] = [1.0];
    const ZERO: [f32; 1] = [0.0];
    let w = if h == 1 { Some([&ONE[..], &ZERO, &ONE, &ZERO, &ONE, &ZERO]) } else { planes(tw, h) };
    let Some(w) = w else { return };
    match h {
        1 => stage_fixed::<4>(re, im, w, bfly),
        4 => stage_fixed::<16>(re, im, w, bfly),
        16 => stage_fixed::<64>(re, im, w, bfly),
        _ => stage_dyn(re, im, h, w, bfly),
    }
}

/// One radix-4 decimation-in-frequency stage.
fn dif_stage(re: &mut [f32], im: &mut [f32], h: usize, tw: &[f32]) {
    run_stage(re, im, h, tw, dif_bfly);
}

/// One radix-4 decimation-in-time stage (transpose of [`dif_stage`]).
fn dit_stage(re: &mut [f32], im: &mut [f32], h: usize, tw: &[f32]) {
    run_stage(re, im, h, tw, dit_bfly);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_naive_dft_and_round_trips() {
        for n in [4usize, 16, 64, 256, 1024] {
            check_size(n);
        }
    }

    fn check_size(n: usize) {
        let fft = Fft::new(n);
        assert_eq!(fft.len(), n);
        let x: Vec<(f32, f32)> = (0..n).map(|i| (((i * 7) % 11) as f32 - 5.0, ((i * 3) % 5) as f32 - 2.0)).collect();
        let mut re: Vec<f32> = x.iter().map(|v| v.0).collect();
        let mut im: Vec<f32> = x.iter().map(|v| v.1).collect();
        fft.forward(&mut re, &mut im);
        for pos in 0..n {
            let k = fft.bin_of(pos);
            let (mut sr, mut si) = (0.0f64, 0.0f64);
            for (i, &(a, b)) in x.iter().enumerate() {
                let w = -std::f64::consts::TAU * ((k * i) % n) as f64 / n as f64;
                sr += a as f64 * w.cos() - b as f64 * w.sin();
                si += a as f64 * w.sin() + b as f64 * w.cos();
            }
            let tol = 1e-4 * n as f64;
            assert!((sr - re[pos] as f64).abs() < tol && (si - im[pos] as f64).abs() < tol, "n={n} bin {k}");
        }
        fft.inverse(&mut re, &mut im);
        for (i, &(a, b)) in x.iter().enumerate() {
            assert!((re[i] / n as f32 - a).abs() < 1e-4 && (im[i] / n as f32 - b).abs() < 1e-4, "n={n} i={i}");
        }
    }

    #[test]
    #[ignore = "timing"]
    fn fft_speed() {
        let fft = Fft::new(1024);
        let mut re: Vec<f32> = (0..1024).map(|i| (i as f32 * 0.1).sin()).collect();
        let mut im = re.clone();
        let mut best = std::time::Duration::MAX;
        for _ in 0..5 {
            let t0 = std::time::Instant::now();
            for _ in 0..11_250 {
                fft.forward(&mut re, &mut im);
                fft.inverse(&mut re, &mut im);
                for v in re.iter_mut() {
                    *v *= 1.0 / 1024.0;
                }
                for v in im.iter_mut() {
                    *v *= 1.0 / 1024.0;
                }
            }
            best = best.min(t0.elapsed());
        }
        println!("22500 FFTs of 1024: {best:?}");
    }
}
