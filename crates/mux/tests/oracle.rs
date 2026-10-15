//! ffmpeg/ffprobe oracle tests on real H.264/HEVC/AAC streams (skipped when the tools are missing).

mod common;

use std::io::Cursor;
use std::path::Path;

use common::*;
use fluxcapture_mux::codec::{aac_audio_specific_config, annexb_to_length_prefixed, avcc_from_annexb, hvcc_from_annexb, is_keyframe_annexb};
use fluxcapture_mux::{
    AudioCodec, AudioTrack, ColorInfo, FrameRate, Hns, MkvOptions, MkvWriter, TrackSpec, VideoCodec, VideoTrack, recover_mkv, remux_mkv_to_mp4, scan_mkv,
};
use fluxcapture_time::SampleClock;

struct Streams {
    tracks: Vec<TrackSpec>,
    /// (track, pts, keyframe, payload) sorted by pts.
    events: Vec<(usize, Hns, bool, Vec<u8>)>,
    video_frames: usize,
    audio_frames: usize,
    /// Expected end of the longest track.
    end: Hns,
}

fn load(codec: VideoCodec, audio_offset: Hns) -> Streams {
    let video_path = match codec {
        VideoCodec::H264 => h264_fixture(),
        VideoCodec::Hevc => hevc_fixture(),
    };
    let es = std::fs::read(video_path).unwrap();
    let aus = split_access_units(&es, codec);
    assert_eq!(aus.len(), 90, "fixture should have 3 s at 30 fps");
    assert!(is_keyframe_annexb(codec, &aus[0]));
    let config = match codec {
        VideoCodec::H264 => avcc_from_annexb(&aus[0]).unwrap(),
        VideoCodec::Hevc => hvcc_from_annexb(&aus[0]).unwrap(),
    };
    let (aac, rate, channels) = parse_adts(&std::fs::read(aac_fixture()).unwrap());
    let asc = aac_audio_specific_config(rate, channels).unwrap();
    let tracks = vec![
        TrackSpec::Video(VideoTrack {
            codec,
            width: 640,
            height: 360,
            frame_rate: Some(FrameRate::FPS_30),
            codec_private: config,
            color: ColorInfo { full_range: false },
            name: "Tela".into(),
        }),
        TrackSpec::Audio(AudioTrack { codec: AudioCodec::Aac, sample_rate: rate, channels, codec_private: asc.to_vec(), name: "Mix".into(), default: true }),
    ];
    let mut events = Vec::new();
    let mut lp = Vec::new();
    for (k, au) in aus.iter().enumerate() {
        annexb_to_length_prefixed(codec, au, &mut lp).unwrap();
        events.push((0, FrameRate::FPS_30.frame_time(k as u64), is_keyframe_annexb(codec, au), lp.clone()));
    }
    let clock = SampleClock::new(rate).unwrap();
    for (k, f) in aac.iter().enumerate() {
        events.push((1, audio_offset + clock.time_of(k as i64 * 1024), true, f.clone()));
    }
    events.sort_by_key(|e| (e.1, e.0));
    let end = Hns::from_secs(3).max(audio_offset + clock.time_of(aac.len() as i64 * 1024));
    Streams { tracks, events, video_frames: aus.len(), audio_frames: aac.len(), end }
}

/// Muxes to an in-memory MKV, flushing after each second; returns (final bytes, snapshots after flushes
/// with the number of (video, audio) frames they contain).
fn mux_mkv(s: &Streams, finish: bool) -> (Vec<u8>, Vec<(Vec<u8>, usize, usize)>) {
    let mut w = MkvWriter::new(Cursor::new(Vec::new()), &s.tracks, MkvOptions { title: "Oracle".into(), ..MkvOptions::default() }).unwrap();
    let mut snaps = vec![(w.get_ref().get_ref().clone(), 0, 0)];
    let (mut nv, mut na) = (0, 0);
    let mut next_flush = Hns::from_secs(1);
    for (t, p, k, d) in &s.events {
        if *p >= next_flush {
            w.flush().unwrap();
            snaps.push((w.get_ref().get_ref().clone(), nv, na));
            next_flush += Hns::from_secs(1);
        }
        w.write(*t, *p, *k, d).unwrap();
        if *t == 0 {
            nv += 1;
        } else {
            na += 1;
        }
    }
    let bytes = if finish {
        w.finish().unwrap().into_inner()
    } else {
        w.flush().unwrap();
        w.get_ref().get_ref().clone()
    };
    (bytes, snaps)
}

fn check_probe(path: &Path, codec_name: &str, video_frames: usize, audio_frames: usize, duration: Option<f64>) -> serde_json::Value {
    let p = ffprobe(path);
    let v = stream(&p, "video", 0);
    assert_eq!(v["codec_name"], codec_name, "{}", path.display());
    assert_eq!(v["width"], 640);
    assert_eq!(v["height"], 360);
    assert_eq!(num(&v["nb_read_frames"]) as usize, video_frames, "video frames in {}", path.display());
    if audio_frames > 0 {
        let a = stream(&p, "audio", 0);
        assert_eq!(a["codec_name"], "aac");
        assert_eq!(num(&a["sample_rate"]) as u32, 48_000);
        assert_eq!(a["channels"], 2);
        assert_eq!(num(&a["nb_read_frames"]) as usize, audio_frames, "audio frames in {}", path.display());
    }
    if let Some(d) = duration {
        let got = num(&p["format"]["duration"]);
        assert!((got - d).abs() <= 1.0 / 30.0 + 0.001, "duration {got} vs expected {d} in {}", path.display());
    }
    p
}

/// Payload of the first `fourcc` box found in an MP4 (good enough for avcC/hvcC lookups).
fn find_box(data: &[u8], fourcc: &[u8; 4]) -> Vec<u8> {
    let i = data.windows(4).position(|w| w == fourcc).unwrap();
    let size = u32::from_be_bytes(data[i - 4..i].try_into().unwrap()) as usize;
    data[i + 4..i - 4 + size].to_vec()
}

fn reference_config(codec: VideoCodec, dir: &Path) -> Vec<u8> {
    let (src, fourcc) = match codec {
        VideoCodec::H264 => (h264_fixture(), b"avcC"),
        VideoCodec::Hevc => (hevc_fixture(), b"hvcC"),
    };
    let out = dir.join("ffmpeg_ref.mp4");
    let tag = match codec {
        VideoCodec::H264 => "avc1",
        VideoCodec::Hevc => "hvc1",
    };
    let st = std::process::Command::new("ffmpeg")
        .args(["-y", "-v", "error", "-nostdin", "-i"])
        .arg(&src)
        .args(["-c", "copy", "-tag:v", tag])
        .arg(&out)
        .status()
        .unwrap();
    assert!(st.success());
    find_box(&std::fs::read(out).unwrap(), fourcc)
}

#[test]
fn h264_aac_mkv_and_mp4() {
    if !have_ffmpeg("h264_aac_mkv_and_mp4") {
        return;
    }
    let dir = scratch("oracle_h264");
    let s = load(VideoCodec::H264, Hns::ZERO);
    // Our avcC equals the one ffmpeg's MP4 muxer derives from the same stream.
    let TrackSpec::Video(v) = &s.tracks[0] else { unreachable!() };
    assert_eq!(v.codec_private, reference_config(VideoCodec::H264, &dir));

    let (bytes, _) = mux_mkv(&s, true);
    let mkv = dir.join("out.mkv");
    std::fs::write(&mkv, &bytes).unwrap();
    let p = check_probe(&mkv, "h264", s.video_frames, s.audio_frames, Some(s.end.as_secs_f64()));
    let vs = stream(&p, "video", 0);
    assert_eq!(vs["color_range"], "tv");
    assert_eq!(vs["color_primaries"], "bt709");
    assert_eq!(stream(&p, "audio", 0)["tags"]["title"], "Mix");
    assert_decodes_cleanly(&mkv);
    let idx = scan_mkv(Cursor::new(&bytes), bytes.len() as u64, 100_000).unwrap();
    assert!(idx.finalized);
    assert_eq!(idx.tracks, s.tracks);

    let mp4 = dir.join("out.mp4");
    let r = remux_mkv_to_mp4(&mkv, &mp4).unwrap();
    assert_eq!(r.frames, s.video_frames + s.audio_frames);
    assert_eq!(r.dropped_frames, 0);
    assert_eq!(r.bytes, std::fs::metadata(&mp4).unwrap().len());
    let p = check_probe(&mp4, "h264", s.video_frames, s.audio_frames, Some(s.end.as_secs_f64()));
    let vs = stream(&p, "video", 0);
    assert!((num(&vs["duration"]) - 3.0).abs() < 1e-6, "video duration {}", vs["duration"]);
    assert_eq!(vs["r_frame_rate"], "30/1");
    assert_eq!(vs["color_range"], "tv");
    assert!(num(&vs["start_time"]).abs() < 1e-6);
    assert!(num(&stream(&p, "audio", 0)["start_time"]).abs() < 1e-6);
    assert_eq!(stream(&p, "audio", 0)["tags"]["handler_name"], "Mix");
    assert_decodes_cleanly(&mp4);
    // faststart: moov precedes mdat.
    let data = std::fs::read(&mp4).unwrap();
    let moov = data.windows(4).position(|w| w == b"moov").unwrap();
    let mdat = data.windows(4).position(|w| w == b"mdat").unwrap();
    assert!(moov < mdat);
}

#[test]
fn hevc_aac_mkv_and_mp4() {
    if !have_ffmpeg("hevc_aac_mkv_and_mp4") {
        return;
    }
    let dir = scratch("oracle_hevc");
    let s = load(VideoCodec::Hevc, Hns::ZERO);
    let TrackSpec::Video(v) = &s.tracks[0] else { unreachable!() };
    let ours = &v.codec_private;
    let theirs = reference_config(VideoCodec::Hevc, &dir);
    assert_eq!(ours[..13], theirs[..13], "profile/tier/level fields");
    assert_eq!(ours[15..19], theirs[15..19], "parallelism, chroma format, bit depths");
    assert_eq!(ours[21] & 0x3F, theirs[21] & 0x3F, "temporal layers, nesting, length size");
    // Each of our VPS/SPS/PPS appears in ffmpeg's record.
    let mut i = 23;
    for _ in 0..ours[22] {
        let n = u16::from_be_bytes([ours[i + 1], ours[i + 2]]);
        i += 3;
        for _ in 0..n {
            let len = usize::from(u16::from_be_bytes([ours[i], ours[i + 1]]));
            let nal = &ours[i + 2..i + 2 + len];
            assert!(theirs.windows(len).any(|x| x == nal), "parameter set missing from ffmpeg's hvcC");
            i += 2 + len;
        }
    }
    assert_eq!(i, ours.len());

    let (bytes, _) = mux_mkv(&s, true);
    let mkv = dir.join("out.mkv");
    std::fs::write(&mkv, &bytes).unwrap();
    check_probe(&mkv, "hevc", s.video_frames, s.audio_frames, Some(s.end.as_secs_f64()));
    assert_decodes_cleanly(&mkv);

    let mp4 = dir.join("out.mp4");
    remux_mkv_to_mp4(&mkv, &mp4).unwrap();
    let p = check_probe(&mp4, "hevc", s.video_frames, s.audio_frames, Some(s.end.as_secs_f64()));
    assert_eq!(stream(&p, "video", 0)["codec_tag_string"], "hvc1");
    assert_decodes_cleanly(&mp4);
}

#[test]
fn unfinished_mkv_is_playable_at_every_flush_and_recoverable() {
    if !have_ffmpeg("unfinished_mkv_is_playable_at_every_flush_and_recoverable") {
        return;
    }
    let dir = scratch("oracle_crash");
    let s = load(VideoCodec::H264, Hns::ZERO);
    let (bytes, snaps) = mux_mkv(&s, false);

    // Right after `new` the file is a valid, empty Matroska file.
    let header = dir.join("header_only.mkv");
    std::fs::write(&header, &snaps[0].0).unwrap();
    let p = ffprobe(&header);
    assert_eq!(p["streams"].as_array().unwrap().len(), 2);

    // After each flush: everything written so far plays, nothing reported as an error.
    for (i, (snap, nv, na)) in snaps.iter().enumerate().skip(1) {
        let path = dir.join(format!("snap{i}.mkv"));
        std::fs::write(&path, snap).unwrap();
        check_probe(&path, "h264", *nv, *na, None);
        assert_decodes_cleanly(&path);
        let idx = scan_mkv(Cursor::new(snap), snap.len() as u64, 100_000).unwrap();
        assert_eq!(idx.frames.len(), nv + na);
        assert!(!idx.finalized);
    }

    // Cut mid-cluster (a crash with a partially flushed write buffer): ffprobe still opens it.
    let cut = &bytes[..bytes.len() - 3000];
    let crashed = dir.join("crashed.mkv");
    std::fs::write(&crashed, cut).unwrap();
    let p = ffprobe(&crashed);
    assert!(num(&stream(&p, "video", 0)["nb_read_frames"]) as usize > 60);

    // Recovery: finalized output, all complete frames kept, decodes cleanly.
    let recovered = dir.join("recovered.mkv");
    let r = recover_mkv(&crashed, &recovered).unwrap();
    assert!(r.dropped_tail_bytes > 0);
    let out = std::fs::read(&recovered).unwrap();
    let idx = scan_mkv(Cursor::new(&out), out.len() as u64, 100_000).unwrap();
    assert!(idx.finalized);
    assert_eq!(idx.frames.len(), r.frames);
    let nv = idx.frames.iter().filter(|f| f.track == 0).count();
    check_probe(&recovered, "h264", nv, r.frames - nv, Some(r.duration.as_secs_f64()));
    assert_decodes_cleanly(&recovered);
    let p = ffprobe(&recovered);
    assert!(p["format"]["duration"].is_string());

    // Remux of the crashed file directly.
    let mp4 = dir.join("crashed.mp4");
    let r2 = remux_mkv_to_mp4(&crashed, &mp4).unwrap();
    assert_eq!(r2.frames, r.frames);
    check_probe(&mp4, "h264", nv, r.frames - nv, None);
    assert_decodes_cleanly(&mp4);
}

#[test]
fn remux_keeps_av_alignment_and_drops_leading_non_keyframes() {
    if !have_ffmpeg("remux_keeps_av_alignment_and_drops_leading_non_keyframes") {
        return;
    }
    let dir = scratch("oracle_align");
    // Audio starts 100 ms after video.
    let s = load(VideoCodec::H264, Hns::from_millis(100));
    let (bytes, _) = mux_mkv(&s, true);
    let mkv = dir.join("late_audio.mkv");
    std::fs::write(&mkv, &bytes).unwrap();
    let mp4 = dir.join("late_audio.mp4");
    remux_mkv_to_mp4(&mkv, &mp4).unwrap();
    let p = ffprobe(&mp4);
    assert!(num(&stream(&p, "video", 0)["start_time"]).abs() < 1e-6);
    let a_start = num(&stream(&p, "audio", 0)["start_time"]);
    assert!((a_start - 0.1).abs() < 0.002, "audio start {a_start}");
    assert_decodes_cleanly(&mp4);

    // A stream that begins mid-GOP: frames before the first keyframe (at 1 s) and audio before it go.
    let s = load(VideoCodec::H264, Hns::ZERO);
    let mut w = MkvWriter::new(Cursor::new(Vec::new()), &s.tracks, MkvOptions::default()).unwrap();
    for (t, p, k, d) in s.events.iter().filter(|e| e.1 >= Hns::from_millis(200)) {
        w.write(*t, *p, *k, d).unwrap();
    }
    let bytes = w.finish().unwrap().into_inner();
    let mkv = dir.join("mid_gop.mkv");
    std::fs::write(&mkv, &bytes).unwrap();
    let mp4 = dir.join("mid_gop.mp4");
    let r = remux_mkv_to_mp4(&mkv, &mp4).unwrap();
    assert!(r.dropped_frames > 24, "dropped {}", r.dropped_frames);
    let p = check_probe(&mp4, "h264", 60, s.events.iter().filter(|e| e.0 == 1 && e.1 >= Hns::from_secs(1)).count(), None);
    assert!(num(&stream(&p, "audio", 0)["start_time"]).abs() < 0.022);
    assert_decodes_cleanly(&mp4);
    let rec = dir.join("mid_gop_recovered.mkv");
    let r = recover_mkv(&mkv, &rec).unwrap();
    assert!(r.dropped_frames > 24);
    assert_decodes_cleanly(&rec);
}
