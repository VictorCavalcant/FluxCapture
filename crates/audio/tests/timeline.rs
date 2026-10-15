//! Timeline placement: drift tracking, loopback gaps, sync offsets, discontinuities.

mod common;

use common::*;
use fluxcapture_audio::{AudioChunk, AudioFormat, MixBuffers, Mixer, SourceId, SourceSettings};
use fluxcapture_time::Hns;

/// Positions (output frames) of isolated peaks above `threshold` in channel 0.
fn find_peaks(x: &[f32], threshold: f32, base: usize) -> Vec<usize> {
    let mut peaks = Vec::new();
    let s: Vec<f32> = x.as_chunks::<2>().0.iter().map(|f| f[0]).collect();
    let mut i = 0;
    while i < s.len() {
        if s[i].abs() > threshold {
            let end = (i + 64).min(s.len());
            let (k, _) = s[i..end].iter().enumerate().fold((0, 0.0f32), |(bk, bv), (k, v)| if v.abs() > bv { (k, v.abs()) } else { (bk, bv) });
            peaks.push(base + i + k);
            i = end;
        } else {
            i += 1;
        }
    }
    peaks
}

#[test]
fn tracks_150_ppm_drift_over_10_minutes() {
    let mut h = Harness::new(no_limiter());
    h.collect = false;
    let id = SourceId(7);
    h.mixer.add_source(id, "mic".into(), SourceSettings::default()).unwrap();
    let drift = 150e-6;
    let mut f = Feeder::new(id, 48_000, 2, |i, frame| frame.fill(if i % 48_000 == 0 && i > 0 { 0.8 } else { 0.0 }));
    f.clock_factor = 1.0 + drift;
    f.jitter_ms = 2.0;
    let delay = Hns::from_millis(100);
    let total = 48_000 * 600;
    let mut errors = Vec::new();
    let mut max_buffered = 0.0f32;
    while h.mixed < total {
        let due = h.next_due(delay);
        f.feed_until(&mut h.mixer, due);
        let base = h.mixed;
        h.mix_next();
        for p in find_peaks(&h.bufs.tracks[0], 0.3, base) {
            // Impulse j is device frame 48000·j, captured at true time 48000·j / (48 kHz · (1 + d)).
            let j = ((p as f64) * (1.0 + drift) / 48_000.0).round();
            let expected = j * 48_000.0 / (1.0 + drift);
            errors.push((p, p as f64 - expected));
        }
        if h.mixed > 48_000 * 5 {
            max_buffered = max_buffered.max(h.mixer.source_stats()[0].buffered_ms);
        }
    }
    let st = h.mixer.source_stats()[0].clone();
    println!("stats: {st:?}");
    let max_err = |from_s: usize| errors.iter().filter(|(p, _)| *p > from_s * 48_000).map(|(_, e)| e.abs()).fold(0.0, f64::max);
    println!(
        "impulses found: {}; max error after 60 s: {:.2} frames; last minute: {:.2} frames; max buffered {:.1} ms",
        errors.len(),
        max_err(60),
        max_err(540),
        max_buffered
    );
    assert!(errors.len() >= 598, "found {} impulses", errors.len());
    // Every impulse within 5 ms over the whole run, and within 1 ms once converged.
    assert!(max_err(0) < 240.0, "max error {} frames", max_err(0));
    assert!(max_err(540) < 48.0, "final error {} frames", max_err(540));
    assert!((st.drift_ppm - 150.0).abs() < 15.0, "drift estimate {}", st.drift_ppm);
    assert_eq!((st.underruns, st.overruns, st.discontinuities), (0, 0, 0));
    assert!(st.timeline_error_ms.abs() < 1.0);
    assert!(max_buffered < 200.0);
}

#[test]
fn slow_device_and_44k1_drift_also_tracked() {
    for (rate, drift) in [(44_100u32, -200e-6), (96_000, 120e-6)] {
        let mut h = Harness::new(no_limiter());
        h.collect = false;
        let id = SourceId(1);
        h.mixer.add_source(id, "dev".into(), SourceSettings::default()).unwrap();
        let mut f = Feeder::new(id, rate, 2, |_, frame| frame.fill(0.0));
        f.clock_factor = 1.0 + drift;
        f.jitter_ms = 3.0;
        run(&mut h, std::slice::from_mut(&mut f), 48_000 * 180, Hns::from_millis(100));
        let st = &h.mixer.source_stats()[0];
        println!("{rate} Hz {drift:e}: {st:?}");
        assert!((st.drift_ppm - drift * 1e6).abs() < 20.0, "drift estimate {}", st.drift_ppm);
        assert!(st.timeline_error_ms.abs() < 1.0);
        assert_eq!((st.underruns, st.overruns, st.discontinuities), (0, 0, 0));
    }
}

#[test]
fn loopback_pause_reanchors_with_silence() {
    for pause_ms in [30u64, 500, 3000] {
        let mut h = Harness::new(no_limiter());
        let id = SourceId(2);
        h.mixer.add_source(id, "loopback".into(), SourceSettings { continuous: false, ..SourceSettings::default() }).unwrap();
        let mut f = Feeder::new(id, 48_000, 2, sine(1000.0, 0.5, 48_000));
        let resume = 48_000 + pause_ms * 48;
        f.pause = Some((48_000, resume));
        run(&mut h, std::slice::from_mut(&mut f), resume as usize + 48_000, Hns::from_millis(100));
        let out = &h.out[0];
        let st = h.mixer.source_stats()[0].clone();
        assert_eq!(st.underruns, 0, "{st:?}");
        assert_eq!(st.discontinuities, 0, "{st:?}");
        assert_eq!(st.gaps, 1, "{st:?}");
        assert!(st.drift_ppm.abs() < 5.0);
        // Silence in the pause (allowing the resampler kernel's few frames of ringing at the edges).
        let gap = &out[2 * (48_000 + 64)..2 * (resume as usize - 64)];
        assert!(peak(gap) < 1e-3, "pause {pause_ms} ms: peak {} in the gap", peak(gap));
        // The resumed tone starts exactly on time: compare with the ideal continuation.
        let seg = &out[2 * (resume as usize + 64)..2 * (resume as usize + 64 + 4800)];
        for (k, fr) in seg.as_chunks::<2>().0.iter().enumerate() {
            let i = resume + 64 + k as u64;
            let ideal = (std::f64::consts::TAU * 1000.0 * i as f64 / 48_000.0).sin() as f32 * 0.5;
            assert!((fr[0] - ideal).abs() < 2e-3, "pause {pause_ms} ms frame {i}: {} vs {ideal}", fr[0]);
        }
    }
}

#[test]
fn continuous_source_gap_counts_underruns() {
    let mut h = Harness::new(no_limiter());
    let id = SourceId(3);
    h.mixer.add_source(id, "mic".into(), SourceSettings::default()).unwrap();
    let mut f = Feeder::new(id, 48_000, 1, sine(500.0, 0.5, 48_000));
    // The device stops delivering for 300 ms, then delivers again with a discontinuity-free stamp.
    f.pause = Some((48_000, 48_000 + 14_400));
    run(&mut h, std::slice::from_mut(&mut f), 48_000 * 3, Hns::from_millis(100));
    let st = &h.mixer.source_stats()[0];
    assert_eq!(st.underruns, 1, "{st:?}");
    // Missing until the resumed audio arrived (the mixer trails real time by 100 ms, so the last
    // ~100 ms of the gap are known to be a gap by the time they are mixed: silence, not underrun).
    assert!((9_000..=14_400).contains(&st.underrun_frames), "{st:?}");
    assert!(st.late_frames < 2_000, "only the start-up lookahead is late: {st:?}");
    assert_eq!(st.discontinuities, 1, "a 300 ms jump re-anchors: {st:?}");
}

/// Mixes one chunk holding an impulse at frame 1000, stamped at output frame 4800, with `offset_ms`.
fn impulse_position(offset_ms: i32, rate: u32) -> usize {
    let mut m = Mixer::new(no_limiter());
    let id = SourceId(9);
    m.add_source(id, "s".into(), SourceSettings { sync_offset_ms: offset_ms, ..SourceSettings::default() }).unwrap();
    let frames = rate as usize / 2;
    let mut samples = vec![0.0f32; frames * 2];
    let at = (1000 * rate as usize) / 48_000;
    samples[2 * at] = 1.0;
    samples[2 * at + 1] = 1.0;
    m.push(AudioChunk {
        source: id,
        time: BASE + out_clock().time_of(4800),
        format: AudioFormat { sample_rate: rate, channels: 2 },
        samples,
        discontinuity: false,
        silent: false,
    });
    let mut bufs = MixBuffers::new(1, 480);
    let mut out = Vec::new();
    for n in 0..100 {
        m.mix(BASE + out_clock().time_of(n * 480), 480, &mut bufs);
        out.extend_from_slice(&bufs.tracks[0]);
    }
    let peaks = find_peaks(&out, 0.3, 0);
    assert_eq!(peaks.len(), 1, "offset {offset_ms}: {peaks:?}");
    peaks[0]
}

#[test]
fn sync_offset_is_honoured_to_the_sample() {
    assert_eq!(impulse_position(0, 48_000), 5800);
    assert_eq!(impulse_position(250, 48_000), 5800 + 12_000);
    assert_eq!(impulse_position(-40, 48_000), 5800 - 1920);
    assert_eq!(impulse_position(7, 48_000), 5800 + 336);
    // 44.1 kHz: frame 918 = 20.816 ms → output 999.18; the band-limited peak lands on 999 or 1000.
    let p = impulse_position(0, 44_100);
    assert!((5799..=5800).contains(&p), "44.1k peak at {p}");
}

#[test]
fn sync_offset_change_shifts_live_audio() {
    let mut h = Harness::new(no_limiter());
    let id = SourceId(4);
    h.mixer.add_source(id, "mic".into(), SourceSettings::default()).unwrap();
    let mut f = Feeder::new(id, 48_000, 2, |i, frame| frame.fill(if i % 24_000 == 0 && i > 0 { 0.9 } else { 0.0 }));
    run(&mut h, std::slice::from_mut(&mut f), 48_000 * 2, Hns::from_millis(100));
    h.mixer.update_source(id, SourceSettings { sync_offset_ms: 100, ..SourceSettings::default() });
    run(&mut h, std::slice::from_mut(&mut f), 48_000 * 4, Hns::from_millis(100));
    let peaks = find_peaks(&h.out[0], 0.3, 0);
    assert!(peaks.contains(&24_000) && peaks.contains(&48_000), "{peaks:?}");
    assert!(peaks.contains(&(120_000 + 4800)) && peaks.contains(&(144_000 + 4800)), "{peaks:?}");
    let st = &h.mixer.source_stats()[0];
    assert_eq!((st.discontinuities, st.underruns), (0, 0), "{st:?}");
}

#[test]
fn discontinuity_flag_and_window_jumps() {
    let mut h = Harness::new(no_limiter());
    let id = SourceId(5);
    h.mixer.add_source(id, "mic".into(), SourceSettings::default()).unwrap();
    let mut f = Feeder::new(id, 48_000, 2, sine(440.0, 0.25, 48_000));
    run(&mut h, std::slice::from_mut(&mut f), 48_000, Hns::from_millis(100));
    // Device glitch: 50 ms lost, flagged.
    f.next_frame += 2400;
    let first = f.next_frame;
    let mut samples = vec![0.0; 960];
    for (i, fr) in samples.as_chunks_mut::<2>().0.iter_mut().enumerate() {
        fr.fill((std::f64::consts::TAU * 440.0 * (first + i as u64) as f64 / 48_000.0).sin() as f32 * 0.25);
    }
    h.mixer.push(AudioChunk {
        source: id,
        time: f.true_time(first),
        format: AudioFormat { sample_rate: 48_000, channels: 2 },
        samples,
        discontinuity: true,
        silent: false,
    });
    f.next_frame += 480;
    run(&mut h, std::slice::from_mut(&mut f), 48_000 * 2, Hns::from_millis(100));
    assert_eq!(h.mixer.source_stats()[0].discontinuities, 1);
    // The caller skips ahead 5 s (e.g. recording paused): the mixer re-anchors, nothing stale plays.
    h.mixed += 48_000 * 5;
    f.next_frame += 48_000 * 5;
    run(&mut h, std::slice::from_mut(&mut f), 48_000 * 9, Hns::from_millis(100));
    assert_eq!(h.mixer.stats().window_discontinuities, 1);
    let tail = &h.out[0][h.out[0].len() - 2 * 48_000..];
    let (_, snr) = sine_fit(&tail[..2 * 24_000], 0, 440.0, 48_000.0);
    assert!(snr > 70.0, "SNR after the jump {snr}");
}

#[test]
fn audio_at_the_very_start_of_the_timeline_is_not_lost() {
    // The source starts exactly at the first window: the mixer primes its delay lines (noise
    // suppression, limiter) on the first window, so nothing of the first 26 ms is dropped.
    let mut m = Mixer::new(fluxcapture_audio::MixerSettings::default());
    let id = SourceId(1);
    m.add_source(id, "s".into(), SourceSettings::default()).unwrap();
    let samples: Vec<f32> = (0..4800)
        .flat_map(|i| {
            let v = (std::f64::consts::TAU * 1000.0 * i as f64 / 48_000.0).sin() as f32 * 0.25;
            [v, v]
        })
        .collect();
    m.push(AudioChunk { source: id, time: BASE, format: AudioFormat::OUTPUT, samples, discontinuity: false, silent: false });
    let mut bufs = MixBuffers::new(1, 480);
    m.mix(BASE, 480, &mut bufs);
    for (i, f) in bufs.tracks[0].as_chunks::<2>().0.iter().enumerate().skip(32) {
        let ideal = (std::f64::consts::TAU * 1000.0 * i as f64 / 48_000.0).sin() as f32 * 0.25;
        assert!((f[0] - ideal).abs() < 2e-3, "frame {i}: {} vs {ideal}", f[0]);
    }
    assert_eq!(m.source_stats()[0].late_frames, 0);
}
