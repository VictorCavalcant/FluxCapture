//! Resampler quality: SNR, passband flatness, aliasing, continuity across chunk boundaries.

mod common;

use common::*;
use fluxcapture_audio::{Resampler, SourceId, SourceSettings};
use fluxcapture_time::Hns;

/// Resamples a stereo sine of `freq` at `rate` (amplitude 0.5) in chunks of `chunk` frames and
/// returns one second of 48 kHz output after a 0.5 s settle.
fn resample_tone(rate: u32, freq: f64, chunk: usize) -> Vec<f32> {
    let mut r = Resampler::new(rate).unwrap();
    let frames = rate as usize * 2;
    let input: Vec<f32> = (0..frames)
        .flat_map(|i| {
            let v = (std::f64::consts::TAU * freq * i as f64 / f64::from(rate)).sin() as f32 * 0.5;
            [v, v]
        })
        .collect();
    let mut out = Vec::new();
    for block in input.chunks(2 * chunk) {
        r.process_interleaved(block, &mut out);
    }
    out[2 * 24_000..2 * 72_000].to_vec()
}

#[test]
fn snr_44k1_to_48k_above_80_db() {
    for (freq, min_snr) in [(1000.0, 80.0), (100.0, 80.0), (5000.0, 80.0), (12_000.0, 75.0)] {
        let out = resample_tone(44_100, freq, 441);
        let (amp, snr) = sine_fit(&out, 0, freq, 48_000.0);
        println!("44.1k->48k {freq} Hz: amplitude {amp:.5}, SNR {snr:.1} dB");
        assert!(snr > min_snr, "{freq} Hz: SNR {snr:.1} dB");
        assert!((amp - 0.5).abs() < 0.5 * 0.012, "{freq} Hz amplitude {amp}");
    }
}

#[test]
fn passband_is_flat() {
    for rate in [44_100, 48_000, 96_000, 32_000] {
        for freq in [50.0, 1000.0, 8000.0, 14_000.0] {
            if freq > f64::from(rate) * 0.42 {
                continue;
            }
            let out = resample_tone(rate, freq, 997);
            let (amp, snr) = sine_fit(&out, 1, freq, 48_000.0);
            let db = 20.0 * (amp / 0.5).log10();
            println!("{rate} -> 48k {freq} Hz: {db:+.3} dB, SNR {snr:.1} dB");
            assert!(db.abs() < 0.1, "{rate} Hz {freq} Hz: {db} dB");
            assert!(snr > 75.0, "{rate} Hz {freq} Hz: SNR {snr}");
        }
    }
}

#[test]
fn unity_rate_with_drift_trim_stays_clean() {
    // A 48 kHz device with a 500 ppm trim: the phase sweeps through every sub-sample offset.
    for corr in [500e-6, -500e-6, 4e-3] {
        for freq in [1000.0, 8000.0, 15_000.0] {
            let mut r = Resampler::new(48_000).unwrap();
            r.set_correction(corr);
            let input: Vec<f32> = (0..96_000)
                .flat_map(|i| {
                    let v = (std::f64::consts::TAU * freq * i as f64 / 48_000.0).sin() as f32 * 0.5;
                    [v, v]
                })
                .collect();
            let mut out = Vec::new();
            for block in input.chunks(960) {
                r.process_interleaved(block, &mut out);
            }
            let seg = &out[2 * 24_000..2 * 72_000];
            // Consuming input (1 + c) times faster raises the output frequency by (1 + c).
            let (amp, snr) = sine_fit(seg, 0, freq * (1.0 + corr), 48_000.0);
            let db = 20.0 * (amp / 0.5).log10();
            println!("48k trim {corr:e}, {freq} Hz: {db:+.4} dB, SNR {snr:.1} dB");
            assert!(db.abs() < 0.05, "{corr} {freq}: {db} dB");
            assert!(snr > 70.0, "{corr} {freq}: SNR {snr}");
        }
    }
}

#[test]
fn downsampling_rejects_aliases() {
    // 30 kHz at 96 kHz would alias to 18 kHz at 48 kHz.
    let out = resample_tone(96_000, 30_000.0, 960);
    let level = 20.0 * (rms(&out) / (0.5 / 2f64.sqrt())).log10();
    println!("96k->48k 30 kHz residual: {level:.1} dB");
    assert!(level < -80.0, "alias at {level} dB");
    // 40 kHz from 192 kHz too (would alias to 8 kHz). Note the transition band deliberately ends
    // ~2 kHz above 24 kHz: only 24–26 kHz content can fold, and only into the inaudible 22–24 kHz.
    let out = resample_tone(192_000, 40_000.0, 1920);
    let level = 20.0 * (rms(&out) / (0.5 / 2f64.sqrt())).log10();
    println!("192k->48k 40 kHz residual: {level:.1} dB");
    assert!(level < -80.0, "alias at {level} dB");
}

#[test]
fn continuous_across_odd_chunk_boundaries_through_the_mixer() {
    let mut h = Harness::new(no_limiter());
    let id = SourceId(1);
    h.mixer.add_source(id, "tone".into(), SourceSettings::default()).unwrap();
    let mut f = Feeder::new(id, 44_100, 2, sine(1000.0, 0.5, 44_100));
    // Irregular packet sizes: the feeder uses a fixed size, so vary it between runs.
    let mut rng = Lcg(99);
    let total = 48_000 * 3;
    while h.mixed < total {
        f.chunk_frames = 1 + (rng.next_f64() * 900.0) as u64;
        let due = h.next_due(Hns::from_millis(100));
        f.feed_until(&mut h.mixer, due);
        h.mix_next();
    }
    let out = &h.out[0];
    // Skip the start-up (lookahead + mixer delay) and check the steady part.
    let steady = &out[2 * 24_000..];
    let max_step = steady.as_chunks::<2>().0.iter().zip(steady.as_chunks::<2>().0.iter().skip(1)).map(|(a, b)| (b[0] - a[0]).abs()).fold(0.0f32, f32::max);
    let bound = (std::f64::consts::TAU * 1000.0 / 48_000.0 * 0.5) as f32;
    assert!(max_step <= bound * 1.01, "max step {max_step} > {bound}");
    let (amp, snr) = sine_fit(&steady[..2 * 96_000], 0, 1000.0, 48_000.0);
    println!("mixer 44.1k tone: amplitude {amp:.5}, SNR {snr:.1} dB");
    assert!(snr > 75.0, "SNR {snr}");
    let st = &h.mixer.source_stats()[0];
    assert_eq!((st.underruns, st.overruns, st.discontinuities), (0, 0, 0), "{st:?}");
}
