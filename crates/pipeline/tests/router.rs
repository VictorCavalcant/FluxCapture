//! End-to-end: encoded H.264 + AAC (ffmpeg fixtures) through the router into recordings with
//! pause, split, MP4 remux and a replay clip, checked with ffprobe. Skips without ffmpeg.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crossbeam_channel::bounded;
use fluxcapture_mux::codec::aac_audio_specific_config;
use fluxcapture_mux::{AudioCodec, AudioTrack, VideoCodec};
use fluxcapture_pipeline::*;
use fluxcapture_platform::{EncodedAudio, EncodedVideo};
use fluxcapture_time::{FrameRate, Hns};

fn have_ffmpeg() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success())
}

fn dir() -> PathBuf {
    let d = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/pipeline-tests");
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Access units of an Annex B stream written with AUDs (x264 `aud=1`).
fn split_aus(data: &[u8]) -> Vec<Vec<u8>> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 4 < data.len() {
        if data[i..i + 4] == [0, 0, 0, 1] && data[i + 4] & 0x1F == 9 {
            starts.push(i);
        }
        i += 1;
    }
    starts.push(data.len());
    starts.windows(2).map(|w| data[w[0]..w[1]].to_vec()).collect()
}

fn adts_frames(data: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 7 <= data.len() {
        let len = (((data[i + 3] & 3) as usize) << 11) | ((data[i + 4] as usize) << 3) | ((data[i + 5] as usize) >> 5);
        if len < 7 || i + len > data.len() {
            break;
        }
        out.push(data[i + 7..i + len].to_vec());
        i += len;
    }
    out
}

fn probe(path: &Path) -> serde_json::Value {
    let o = Command::new("ffprobe").args(["-v", "error", "-count_frames", "-show_streams", "-show_format", "-of", "json"]).arg(path).output().unwrap();
    serde_json::from_slice(&o.stdout).unwrap()
}

fn decodes(path: &Path) -> bool {
    let o = Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(path).args(["-f", "null", "-"]).output().unwrap();
    o.status.success() && o.stderr.is_empty()
}

#[test]
fn record_pause_split_remux_replay() {
    if !have_ffmpeg() {
        eprintln!("SKIPPED: ffmpeg not found");
        return;
    }
    let d = dir();
    let h264 = d.join("in.h264");
    let aac = d.join("in.aac");
    assert!(
        Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=320x240:rate=30",
                "-t",
                "8",
                "-c:v",
                "libx264",
                "-g",
                "30",
                "-bf",
                "0",
                "-x264-params",
                "aud=1"
            ])
            .args(["-bsf:v", "h264_mp4toannexb", "-f", "h264"])
            .arg(&h264)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=f=440:sample_rate=48000",
                "-t",
                "8",
                "-ac",
                "2",
                "-c:a",
                "aac",
                "-b:a",
                "128k",
                "-f",
                "adts"
            ])
            .arg(&aac)
            .status()
            .unwrap()
            .success()
    );
    let aus = split_aus(&std::fs::read(&h264).unwrap());
    let frames = adts_frames(&std::fs::read(&aac).unwrap());
    assert!(aus.len() >= 239, "{}", aus.len());
    let asc = aac_audio_specific_config(48_000, 2).unwrap().to_vec();
    let setup = StreamSetup {
        video_codec: VideoCodec::H264,
        width: 320,
        height: 240,
        fps: FrameRate::FPS_30,
        variable_frame_rate: false,
        full_range: false,
        audio: vec![AudioTrack { codec: AudioCodec::Aac, sample_rate: 48_000, channels: 2, codec_private: asc, name: "Mix".into(), default: true }],
    };
    let (vtx, vrx) = bounded(1024);
    let (atx, arx) = bounded(1024);
    let router = Router::start(setup, vrx, arx).unwrap();
    router.set_replay(Some(ReplayConfig { seconds: 3, max_bytes: 64 << 20 }));
    // A large media-clock offset, as with real QPC times.
    let base = Hns::from_secs(123_456);
    let cfg = RecordConfig {
        folder: d.clone(),
        base_name: "rec".into(),
        split: Split::Every(Hns::from_secs(3)),
        min_free_bytes: 0,
        warn_free_bytes: 0,
        remux_mp4: true,
        delete_mkv_after_remux: false,
        journal_dir: Some(d.join("journal")),
        disk_free: None,
        title: "test".into(),
    };
    for f in std::fs::read_dir(&d).unwrap().flatten() {
        let n = f.file_name().to_string_lossy().into_owned();
        if n.starts_with("rec") || n.starts_with("clip") {
            let _ = std::fs::remove_file(f.path());
        }
    }
    let rec = router.start_recording(cfg).unwrap();
    let mut ai = 0usize;
    for (k, au) in aus.iter().enumerate() {
        let pts = base + FrameRate::FPS_30.frame_time(k as u64);
        // Pause between 2 s and 3 s of input.
        if k == 60 {
            std::thread::sleep(Duration::from_millis(100));
            router.set_paused(true);
            std::thread::sleep(Duration::from_millis(50));
        }
        if k == 90 {
            std::thread::sleep(Duration::from_millis(100));
            router.set_paused(false);
            std::thread::sleep(Duration::from_millis(50));
        }
        vtx.send(EncodedVideo { pts, dts: pts, keyframe: k % 30 == 0, data: au.clone() }).unwrap();
        while ai < frames.len() && Hns::from_timescale(ai as i64 * 1024, 48_000).unwrap() <= pts - base {
            let apts = base + Hns::from_timescale(ai as i64 * 1024, 48_000).unwrap();
            atx.send((0, EncodedAudio { pts: apts, duration: Hns::from_timescale(1024, 48_000).unwrap(), data: frames[ai].clone() })).unwrap();
            ai += 1;
        }
    }
    std::thread::sleep(Duration::from_millis(300));
    let clip = router.save_replay(d.join("clip.mp4"), true).unwrap().recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
    let stopped = router.stop_recording().unwrap();
    let summary = stopped.wait(Duration::from_secs(30)).unwrap();
    drop(rec);
    assert_eq!(summary.reason, StopReason::User, "{:?}", summary.errors);
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
    // 8 s of input minus ~1 s paused = ~7 s; split every 3 s → 3 parts, each MKV + MP4.
    let mkvs: Vec<&PathBuf> = summary.files.iter().filter(|f| f.extension().is_some_and(|e| e == "mkv")).collect();
    let mp4s: Vec<&PathBuf> = summary.files.iter().filter(|f| f.extension().is_some_and(|e| e == "mp4")).collect();
    assert_eq!(mkvs.len(), 3, "{:?}", summary.files);
    assert_eq!(mp4s.len(), 3);
    assert!((summary.duration.as_secs_f64() - 7.0).abs() < 0.2, "{}", summary.duration);
    let mut total_frames = 0;
    for f in mkvs.iter().chain(mp4s.iter()) {
        assert!(decodes(f), "{}", f.display());
        let j = probe(f);
        let streams = j["streams"].as_array().unwrap();
        assert_eq!(streams.len(), 2, "{}", f.display());
        if f.extension().unwrap() == "mkv" {
            total_frames += streams[0]["nb_read_frames"].as_str().unwrap().parse::<u64>().unwrap();
            // Audio and video stay aligned in every part.
            let vd: f64 = j["format"]["duration"].as_str().unwrap().parse().unwrap();
            assert!(vd > 0.9 && vd < 3.2, "{} duration {vd}", f.display());
        }
    }
    assert_eq!(total_frames, aus.len() as u64 - 30, "paused second must be absent");
    // The replay clip: ~3 s starting on a keyframe.
    let j = probe(&clip);
    let dur: f64 = j["format"]["duration"].as_str().unwrap().parse().unwrap();
    assert!((2.9..=4.2).contains(&dur), "replay {dur}");
    assert!(decodes(&clip));
    // Journal removed after a clean stop.
    assert!(journal::pending(&d.join("journal")).is_empty());
}
