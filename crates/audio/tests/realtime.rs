//! Realtime factor: 4 sources with every filter, 2 tracks, limiter, 60 s of audio.

mod common;

use std::time::{Duration, Instant};

use common::*;
use fluxcapture_audio::*;
use fluxcapture_time::Hns;

/// Builds the scenario and returns the time spent in `push` + `mix` for 60 s of audio.
fn run_once(formats: &[(u32, u16)]) -> Duration {
    let filters = FilterSettings {
        gain_db: 3.0,
        high_pass: Some(HighPassSettings::default()),
        noise_suppression: Some(NoiseSuppressionSettings { strength: 0.8 }),
        gate: Some(GateSettings::default()),
        compressor: Some(CompressorSettings::default()),
    };
    let mut h = Harness::new(MixerSettings { tracks: 2, ..MixerSettings::default() });
    h.collect = false;
    let mut feeders = Vec::new();
    for (k, &(rate, ch)) in formats.iter().enumerate() {
        let id = SourceId(k as u32);
        let s = SourceSettings { filters: filters.clone(), tracks: if k % 2 == 0 { 0b11 } else { 0b01 }, continuous: k != 3, ..SourceSettings::default() };
        h.mixer.add_source(id, format!("src {k}"), s).unwrap();
        let mut rng = Lcg(k as u64 + 1);
        let mut f = Feeder::new(id, rate, ch, move |i, frame| {
            let tone = (std::f64::consts::TAU * 440.0 * i as f64 / f64::from(rate)).sin() as f32 * 0.3;
            for s in frame.iter_mut() {
                *s = tone + 0.01 * rng.bipolar();
            }
        });
        f.clock_factor = 1.0 + 50e-6 * k as f64;
        f.jitter_ms = 1.0;
        feeders.push(f);
    }
    // Pre-generate the chunks so only the mixer's own work (push + mix) is timed.
    let delay = Hns::from_millis(100);
    let frames = 48_000 * 60;
    let mut schedule: Vec<Vec<AudioChunk>> = Vec::with_capacity(frames / 480);
    for n in (0..frames).step_by(480) {
        let due = h.time_of(n + 480) + delay;
        schedule.push(feeders.iter_mut().flat_map(|f| f.chunks_until(due)).collect());
    }
    let t0 = Instant::now();
    for chunks in schedule {
        for c in chunks {
            h.mixer.push(c);
        }
        h.mix_next();
    }
    let elapsed = t0.elapsed();
    for st in h.mixer.source_stats() {
        assert_eq!((st.underruns, st.overruns, st.discontinuities), (0, 0, 0), "{st:?}");
    }
    assert!(h.bufs.tracks.iter().all(|t| t.iter().all(|v| v.is_finite())));
    elapsed
}

fn best_of(runs: usize, good_enough: Duration, mut f: impl FnMut() -> Duration) -> Duration {
    let mut best = Duration::MAX;
    for _ in 0..runs {
        best = best.min(f());
        if best <= good_enough {
            break;
        }
    }
    best
}

/// One test (not two running in parallel) so the measurements do not compete for cores.
#[test]
fn realtime_factor() {
    // A mic (48 kHz stereo), a 44.1 kHz device, a mono mic and a 5.1 loopback endpoint.
    let formats = [(48_000, 2), (44_100, 2), (48_000, 1), (48_000, 6)];
    // Best of up to five runs (stopping at the first comfortably inside the bound): the figure of
    // merit is the code's cost, not a scheduler hiccup or a busy SMT sibling.
    let best = best_of(5, Duration::from_millis(700), || run_once(&formats));
    println!("60 s of 4-source audio with all filters mixed in {best:?} ({:.0}x realtime)", 60.0 / best.as_secs_f64());
    assert!(best.as_secs_f64() < 1.0, "too slow: {best:?}");

    let best = best_of(5, Duration::from_millis(300), || run_once(&[(96_000, 6)]));
    println!("60 s of one 96 kHz 5.1 source with all filters in {best:?}");
    assert!(best.as_secs_f64() < 0.5, "too slow: {best:?}");
}
