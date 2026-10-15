//! Format conversion, filters, limiter, meters and hostile input through the public mixer API.

mod common;

use common::*;
use fluxcapture_audio::*;
use fluxcapture_time::Hns;

fn one_source(mixer: MixerSettings, s: SourceSettings, rate: u32, channels: u16, generator: impl FnMut(u64, &mut [f32]) + 'static, secs: usize) -> Harness {
    let mut h = Harness::new(mixer);
    let id = SourceId(1);
    h.mixer.add_source(id, "src".into(), s).unwrap();
    let mut f = Feeder::new(id, rate, channels, generator);
    run(&mut h, std::slice::from_mut(&mut f), 48_000 * secs, Hns::from_millis(100));
    h
}

/// Output of the last second, channel-interleaved.
fn last_second(h: &Harness) -> &[f32] {
    let o = &h.out[0];
    &o[o.len() - 2 * 48_000..]
}

#[test]
fn downmix_5_1() {
    // Channel order FL FR FC LFE SL SR.
    let cases: [(usize, f64, f64); 4] =
        [(0, 0.5, 0.0), (1, 0.0, 0.5), (2, 0.5 * std::f64::consts::FRAC_1_SQRT_2, 0.5 * std::f64::consts::FRAC_1_SQRT_2), (3, 0.0, 0.0)];
    for (ch, want_l, want_r) in cases {
        let h = one_source(
            no_limiter(),
            SourceSettings::default(),
            48_000,
            6,
            move |i, frame| {
                frame.fill(0.0);
                frame[ch] = (std::f64::consts::TAU * 1000.0 * i as f64 / 48_000.0).sin() as f32 * 0.5;
            },
            2,
        );
        let out = last_second(&h);
        let (l, _) = sine_fit(out, 0, 1000.0, 48_000.0);
        let (r, _) = sine_fit(out, 1, 1000.0, 48_000.0);
        assert!((l - want_l).abs() < 0.002 && (r - want_r).abs() < 0.002, "channel {ch}: L {l} R {r}");
    }
    // 7.1 surround and mono upmix.
    let h = one_source(no_limiter(), SourceSettings::default(), 48_000, 1, sine(1000.0, 0.5, 48_000), 2);
    let out = last_second(&h);
    assert!((sine_fit(out, 0, 1000.0, 48_000.0).0 - 0.5).abs() < 0.002 && (sine_fit(out, 1, 1000.0, 48_000.0).0 - 0.5).abs() < 0.002);
    let h = one_source(no_limiter(), SourceSettings::default(), 44_100, 8, sine(1000.0, 0.1, 44_100), 2);
    let out = last_second(&h);
    let want = 0.1 * (1.0 + 3.0 * std::f64::consts::FRAC_1_SQRT_2);
    assert!((sine_fit(out, 0, 1000.0, 48_000.0).0 - want).abs() < 0.002);
}

#[test]
fn limiter_holds_the_ceiling_on_full_scale_noise() {
    for ceiling_db in [-1.0f32, -6.0, 0.0] {
        let mixer = MixerSettings { limiter: Some(LimiterSettings { ceiling_db, release_ms: 50.0 }), tracks: 2, ..MixerSettings::default() };
        let mut h = Harness::new(mixer);
        for k in 0..3 {
            let id = SourceId(k);
            h.mixer.add_source(id, format!("noise {k}"), SourceSettings { volume_db: 20.0, tracks: 0b11, ..SourceSettings::default() }).unwrap();
        }
        let mut feeders: Vec<Feeder> = (0..3)
            .map(|k| {
                let mut r = Lcg(1000 + k as u64);
                Feeder::new(SourceId(k), 48_000, 2, move |_, frame| frame.iter_mut().for_each(|s| *s = r.bipolar()))
            })
            .collect();
        run(&mut h, &mut feeders, 48_000 * 3, Hns::from_millis(100));
        let ceiling = db_to_gain(ceiling_db);
        for t in 0..2 {
            let p = peak(&h.out[t]);
            assert!(p <= ceiling, "track {t} ceiling {ceiling_db} dB: peak {p} > {ceiling}");
            assert!(p > ceiling * 0.9, "limiter should drive close to the ceiling, peak {p}");
        }
        let m = h.mixer.master_meters();
        assert_eq!(m.len(), 2);
        if ceiling_db < 0.0 {
            assert!(!m[0].clipped);
        }
        assert!(h.mixer.meters()[0].level.clipped, "post-fader source meter sees +20 dB noise clip");
        h.mixer.reset_clip();
        assert!(!h.mixer.meters()[0].level.clipped);
    }
}

#[test]
fn limiter_transparent_below_ceiling_and_latency_compensated() {
    // A -12 dB tone passes unchanged and on time with the limiter enabled.
    let h = one_source(MixerSettings::default(), SourceSettings::default(), 48_000, 2, sine(1000.0, 0.25, 48_000), 2);
    let out = last_second(&h);
    let start = h.out[0].len() / 2 - 48_000;
    for (k, f) in out.as_chunks::<2>().0.iter().enumerate().take(4800) {
        let ideal = (std::f64::consts::TAU * 1000.0 * (start + k) as f64 / 48_000.0).sin() as f32 * 0.25;
        assert!((f[0] - ideal).abs() < 1e-3, "frame {k}: {} vs {ideal}", f[0]);
    }
}

#[test]
fn gate_closes_on_quiet_and_opens_on_loud() {
    let gate = GateSettings { open_db: -26.0, close_db: -32.0, attack_ms: 5.0, hold_ms: 50.0, release_ms: 50.0 };
    let s = SourceSettings { filters: FilterSettings { gate: Some(gate), ..FilterSettings::default() }, ..SourceSettings::default() };
    // -40 dB for 1 s, then -10 dB for 1 s, then -40 dB again.
    let generator = |i: u64, frame: &mut [f32]| {
        let amp = if (48_000..96_000).contains(&i) { 0.316 } else { 0.01 };
        frame.fill((std::f64::consts::TAU * 300.0 * i as f64 / 48_000.0).sin() as f32 * amp);
    };
    let h = one_source(no_limiter(), s, 48_000, 2, generator, 3);
    let o = &h.out[0];
    assert!(peak(&o[2 * 10_000..2 * 48_000]) < 1e-6, "gate stays closed on -40 dB");
    let loud = &o[2 * 49_000..2 * 95_000];
    assert!((f64::from(peak(loud)) - 0.316).abs() < 0.01, "open gate passes the loud part: {}", peak(loud));
    assert!(peak(&o[2 * (96_000 + 9_600)..2 * 144_000]) < 1e-6, "gate closes again after hold + release");
}

#[test]
fn compressor_follows_its_curve() {
    let comp = CompressorSettings { threshold_db: -20.0, ratio: 4.0, attack_ms: 5.0, release_ms: 50.0, makeup_db: 0.0 };
    let s = SourceSettings { filters: FilterSettings { compressor: Some(comp.clone()), ..FilterSettings::default() }, ..SourceSettings::default() };
    // A full-scale sine: RMS -3.01 dBFS, 16.99 dB over → reduced by 16.99 · 3/4 = 12.74 dB.
    let h = one_source(no_limiter(), s.clone(), 48_000, 2, sine(1000.0, 1.0, 48_000), 2);
    let (amp, _) = sine_fit(last_second(&h), 0, 1000.0, 48_000.0);
    let db = 20.0 * amp.log10();
    assert!((db + 12.74).abs() < 0.5, "compressed level {db:.2} dB");
    // Below threshold (-40 dB): untouched.
    let h = one_source(no_limiter(), s, 48_000, 2, sine(1000.0, 0.01, 48_000), 2);
    let (amp, _) = sine_fit(last_second(&h), 0, 1000.0, 48_000.0);
    assert!((20.0 * (amp / 0.01).log10()).abs() < 0.05);
    // Makeup gain.
    let s = SourceSettings {
        filters: FilterSettings { compressor: Some(CompressorSettings { makeup_db: 6.0, ..comp }), ..FilterSettings::default() },
        ..SourceSettings::default()
    };
    let h = one_source(no_limiter(), s, 48_000, 2, sine(1000.0, 0.01, 48_000), 2);
    let (amp, _) = sine_fit(last_second(&h), 0, 1000.0, 48_000.0);
    assert!((20.0 * (amp / 0.01).log10() - 6.0).abs() < 0.05);
}

#[test]
fn volume_balance_mono_and_mute() {
    let s = SourceSettings { volume_db: -6.0206, balance: 0.5, ..SourceSettings::default() };
    let h = one_source(no_limiter(), s, 48_000, 2, sine(1000.0, 0.5, 48_000), 2);
    let out = last_second(&h);
    assert!((sine_fit(out, 0, 1000.0, 48_000.0).0 - 0.125).abs() < 1e-3);
    assert!((sine_fit(out, 1, 1000.0, 48_000.0).0 - 0.25).abs() < 1e-3);
    // Mono: L-only input lands in both channels at half level.
    let s = SourceSettings { force_mono: true, ..SourceSettings::default() };
    let h = one_source(
        no_limiter(),
        s,
        48_000,
        2,
        |i, f| {
            f[0] = (std::f64::consts::TAU * 1000.0 * i as f64 / 48_000.0).sin() as f32 * 0.5;
            f[1] = 0.0;
        },
        2,
    );
    let out = last_second(&h);
    assert!((sine_fit(out, 1, 1000.0, 48_000.0).0 - 0.25).abs() < 1e-3);
    // Mute ramps to silence without a click, unmute ramps back.
    let mut h = Harness::new(no_limiter());
    let id = SourceId(1);
    h.mixer.add_source(id, "x".into(), SourceSettings::default()).unwrap();
    let mut f = Feeder::new(id, 48_000, 2, sine(200.0, 0.5, 48_000));
    run(&mut h, std::slice::from_mut(&mut f), 48_000, Hns::from_millis(100));
    h.mixer.update_source(id, SourceSettings { muted: true, ..SourceSettings::default() });
    run(&mut h, std::slice::from_mut(&mut f), 48_000 * 2, Hns::from_millis(100));
    let o = &h.out[0];
    let max_step = o.as_chunks::<2>().0.iter().zip(o.as_chunks::<2>().0.iter().skip(1)).skip(24_000).map(|(a, b)| (b[0] - a[0]).abs()).fold(0.0f32, f32::max);
    assert!(max_step < 0.02, "click {max_step}");
    assert!(peak(&o[2 * 60_000..]) == 0.0);
    // Track routing: tracks bitmask 0b10 → only track 2.
    let mixer = MixerSettings { tracks: 3, limiter: None, ..MixerSettings::default() };
    let h = one_source(mixer, SourceSettings { tracks: 0b10, ..SourceSettings::default() }, 48_000, 2, sine(1000.0, 0.5, 48_000), 1);
    assert_eq!(peak(&h.out[0]), 0.0);
    assert!(peak(&h.out[1]) > 0.4);
    assert_eq!(peak(&h.out[2]), 0.0);
}

#[test]
fn nan_inf_and_huge_samples_never_propagate() {
    let all = FilterSettings {
        gain_db: 6.0,
        high_pass: Some(HighPassSettings::default()),
        noise_suppression: Some(NoiseSuppressionSettings { strength: 1.0 }),
        gate: Some(GateSettings::default()),
        compressor: Some(CompressorSettings::default()),
    };
    let s = SourceSettings { filters: all, ..SourceSettings::default() };
    let generator = |i: u64, frame: &mut [f32]| {
        let v = match (i / 4800) % 4 {
            0 => f32::NAN,
            1 => f32::INFINITY,
            2 => {
                if i.is_multiple_of(2) {
                    f32::MAX
                } else {
                    f32::NEG_INFINITY
                }
            }
            _ => (i as f32 * 0.05).sin() * 0.5,
        };
        frame.fill(v);
    };
    for limiter in [None, Some(LimiterSettings::default())] {
        let h = one_source(MixerSettings { limiter, ..MixerSettings::default() }, s.clone(), 44_100, 3, generator, 3);
        assert!(h.out[0].iter().all(|v| v.is_finite() && v.abs() < 1000.0));
        let m = h.mixer.meters();
        assert!(m[0].level.peak_db.iter().chain(m[0].level.rms_db.iter()).all(|v| v.is_finite()));
    }
}

#[test]
fn silent_flag_and_hostile_chunks() {
    let mut m = Mixer::new(MixerSettings::default());
    let id = SourceId(1);
    m.add_source(id, "x".into(), SourceSettings::default()).unwrap();
    let push = |m: &mut Mixer, time: Hns, rate: u32, channels: u16, samples: Vec<f32>, silent: bool| {
        m.push(AudioChunk { source: id, time, format: AudioFormat { sample_rate: rate, channels }, samples, discontinuity: false, silent });
    };
    push(&mut m, BASE, 0, 2, vec![1.0; 100], false);
    push(&mut m, BASE, 48_000, 0, vec![1.0; 100], false);
    push(&mut m, BASE, u32::MAX, 2, vec![1.0; 100], false);
    push(&mut m, BASE, 48_000, u16::MAX, vec![1.0; 100], false);
    push(&mut m, BASE, 48_000, 2, vec![], false);
    push(&mut m, BASE, 48_000, 3, vec![1.0; 2], false);
    assert_eq!(m.source_stats()[0].rejected_chunks, 4);
    push(&mut m, Hns(i64::MAX), 48_000, 2, vec![0.5; 960], false);
    push(&mut m, Hns(i64::MIN), 48_000, 2, vec![0.5; 960], false);
    push(&mut m, Hns(i64::MIN), 48_000, 2, vec![0.5; 960], true);
    m.push(AudioChunk { source: SourceId(99), time: BASE, format: AudioFormat::OUTPUT, samples: vec![0.0; 2], discontinuity: false, silent: false });
    assert_eq!(m.stats().unknown_source_chunks, 1);
    let mut bufs = MixBuffers::new(1, 480);
    for t in [Hns(i64::MIN), Hns(i64::MAX), Hns(0), Hns(-1), BASE] {
        assert_eq!(m.mix(t, 480, &mut bufs), 480);
        assert!(bufs.tracks[0].iter().all(|v| v.is_finite()));
    }
    assert_eq!(m.mix(BASE, usize::MAX, &mut bufs), MAX_MIX_FRAMES);
    assert_eq!(m.mix(BASE, 0, &mut bufs), 0);
    // Silent-flagged chunk with garbage samples mixes to silence.
    let mut m = Mixer::new(no_limiter());
    m.add_source(id, "x".into(), SourceSettings::default()).unwrap();
    for k in 0..20 {
        push(&mut m, BASE + Hns::from_millis(10 * k), 48_000, 2, vec![0.9; 960], true);
    }
    for n in 0..10 {
        m.mix(BASE + out_clock().time_of(n * 480), 480, &mut bufs);
        assert_eq!(peak(&bufs.tracks[0]), 0.0);
    }
}

#[test]
fn hostile_settings_are_sanitized_and_survive_the_mixer() {
    let json = r#"{"volume_db": 1e30, "balance": -7, "sync_offset_ms": 99999999, "tracks": 255, "filters": {"gain_db": -1e30,
        "high_pass": {"cutoff_hz": -5}, "noise_suppression": {}, "gate": {"open_db": 50, "close_db": 60, "attack_ms": 0, "hold_ms": -1, "release_ms": 0},
        "compressor": {"ratio": 1e30, "attack_ms": -3}, "unknown": 1}}"#;
    let s: SourceSettings = serde_json::from_str(json).unwrap();
    let s = s.sanitized();
    assert_eq!(s.volume_db, 20.0);
    assert_eq!(s.balance, -1.0);
    assert_eq!(s.sync_offset_ms, 2000);
    assert_eq!(s.tracks, 0x3F);
    assert_eq!(s.filters.noise_suppression, Some(NoiseSuppressionSettings::default()));
    assert_eq!(s.clone().sanitized(), s, "sanitized is idempotent");
    let m: MixerSettings = serde_json::from_str(r#"{"tracks": 1000, "master_volume_db": -1e9, "limiter": {"ceiling_db": 99}}"#).unwrap();
    assert_eq!(m.sanitized().tracks, MAX_TRACKS);
    let empty: SourceSettings = serde_json::from_str("{}").unwrap();
    assert_eq!(empty, SourceSettings::default());
    // Raw NaN-laden settings go straight into the mixer (it sanitizes internally).
    let nan = SourceSettings {
        volume_db: f32::NAN,
        balance: f32::NAN,
        filters: FilterSettings {
            gain_db: f32::NAN,
            high_pass: Some(HighPassSettings { cutoff_hz: f32::NAN }),
            noise_suppression: Some(NoiseSuppressionSettings { strength: f32::NAN }),
            gate: Some(GateSettings { open_db: f32::NAN, close_db: f32::INFINITY, attack_ms: f32::NAN, hold_ms: f32::INFINITY, release_ms: f32::NAN }),
            compressor: Some(CompressorSettings { threshold_db: f32::NAN, ratio: f32::NAN, attack_ms: f32::NAN, release_ms: f32::NAN, makeup_db: f32::NAN }),
        },
        ..SourceSettings::default()
    };
    let mixer = MixerSettings { tracks: usize::MAX, master_volume_db: f32::NAN, limiter: Some(LimiterSettings { ceiling_db: f32::NAN, release_ms: f32::NAN }) };
    let h = one_source(mixer, nan, 48_000, 2, sine(1000.0, 0.5, 48_000), 2);
    assert!(h.out.iter().flatten().all(|v| v.is_finite()));
    assert!(peak(&h.out[0]) > 0.1);
    // Too many sources is an error, not a panic or silent drop.
    let mut m = Mixer::new(MixerSettings::default());
    for k in 0..MAX_SOURCES as u32 {
        m.add_source(SourceId(k), String::new(), SourceSettings::default()).unwrap();
    }
    assert_eq!(m.add_source(SourceId(1000), String::new(), SourceSettings::default()), Err(AudioError::TooManySources { max: MAX_SOURCES }));
    assert!(m.add_source(SourceId(3), "renamed".into(), SourceSettings::default()).is_ok());
    m.remove_source(SourceId(3));
    assert_eq!(m.source_ids().len(), MAX_SOURCES - 1);
}

#[test]
fn overflow_drops_oldest_and_counts() {
    let mut m = Mixer::new(MixerSettings::default());
    let id = SourceId(1);
    m.add_source(id, "x".into(), SourceSettings::default()).unwrap();
    // 3 s of audio and nobody mixing: the 2 s FIFO overflows.
    for k in 0..300 {
        m.push(AudioChunk {
            source: id,
            time: BASE + Hns::from_millis(10 * k),
            format: AudioFormat::OUTPUT,
            samples: vec![0.1; 960],
            discontinuity: false,
            silent: false,
        });
    }
    let st = &m.source_stats()[0];
    assert!(st.overruns > 0);
    assert!(st.buffered_ms <= 2001.0, "{st:?}");
}

#[test]
fn mixer_can_move_to_the_mixer_thread() {
    fn assert_send<T: Send + 'static>() {}
    assert_send::<Mixer>();
    assert_send::<AudioChunk>();
    assert_send::<Resampler>();
}
