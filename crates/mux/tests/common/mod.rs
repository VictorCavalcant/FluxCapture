//! Shared test helpers: synthetic streams, ffmpeg/ffprobe oracles and fixture parsing.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use fluxcapture_mux::codec::nal_units;
use fluxcapture_mux::{AudioCodec, AudioTrack, ColorInfo, FrameRate, Hns, TrackSpec, VideoCodec, VideoTrack};

pub fn target_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("target")
}

/// A scratch directory for one test.
pub fn scratch(test: &str) -> PathBuf {
    let d = target_dir().join("mux-tests").join(test);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn tool_ok(tool: &str) -> bool {
    Command::new(tool).arg("-version").output().map(|o| o.status.success()).unwrap_or(false)
}

/// `true` when ffmpeg and ffprobe are on PATH; prints a skip notice otherwise.
pub fn have_ffmpeg(test: &str) -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    let ok = *OK.get_or_init(|| tool_ok("ffmpeg") && tool_ok("ffprobe"));
    if !ok {
        eprintln!("SKIPPED {test}: ffmpeg/ffprobe not found on PATH");
    }
    ok
}

/// Generates `target/fixtures/<name>` with ffmpeg (once; atomic rename so parallel runs are safe).
pub fn fixture(name: &str, args: &[&str]) -> PathBuf {
    static LOCK: Mutex<()> = Mutex::new(());
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = target_dir().join("fixtures");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    if path.exists() {
        return path;
    }
    let tmp = dir.join(format!("{name}.{}.tmp", std::process::id()));
    let out = Command::new("ffmpeg").args(["-y", "-v", "error", "-nostdin"]).args(args).arg(&tmp).output().unwrap();
    assert!(out.status.success(), "ffmpeg fixture {name} failed: {}", String::from_utf8_lossy(&out.stderr));
    std::fs::rename(&tmp, &path).unwrap();
    path
}

pub fn h264_fixture() -> PathBuf {
    fixture(
        "testsrc2_640x360_30_h264.h264",
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=640x360:rate=30",
            "-t",
            "3",
            "-c:v",
            "libx264",
            "-g",
            "30",
            "-bf",
            "0",
            "-pix_fmt",
            "yuv420p",
            "-bsf:v",
            "h264_mp4toannexb",
            "-f",
            "h264",
        ],
    )
}

pub fn hevc_fixture() -> PathBuf {
    fixture(
        "testsrc2_640x360_30_hevc.hevc",
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=640x360:rate=30",
            "-t",
            "3",
            "-c:v",
            "libx265",
            "-g",
            "30",
            "-x265-params",
            "bframes=0:log-level=error",
            "-pix_fmt",
            "yuv420p",
            "-f",
            "hevc",
        ],
    )
}

pub fn aac_fixture() -> PathBuf {
    fixture("sine_48k_stereo.aac", &["-f", "lavfi", "-i", "sine=f=440:sample_rate=48000", "-t", "3", "-ac", "2", "-c:a", "aac", "-b:a", "128k", "-f", "adts"])
}

/// Splits an Annex B elementary stream into access units (re-emitted with 4-byte start codes).
pub fn split_access_units(data: &[u8], codec: VideoCodec) -> Vec<Vec<u8>> {
    let mut aus: Vec<Vec<u8>> = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    let mut cur_has_vcl = false;
    for nal in nal_units(data) {
        let (vcl, first_slice, starts_au_if_after_vcl, aud) = match codec {
            VideoCodec::H264 => {
                let t = nal[0] & 0x1f;
                let vcl = (1..=5).contains(&t);
                (vcl, vcl && nal.get(1).is_some_and(|b| b & 0x80 != 0), matches!(t, 6..=8 | 14 | 15), t == 9)
            }
            VideoCodec::Hevc => {
                let t = (nal[0] >> 1) & 0x3f;
                let vcl = t < 32;
                (vcl, vcl && nal.get(2).is_some_and(|b| b & 0x80 != 0), matches!(t, 32..=34 | 39), t == 35)
            }
        };
        let new_au = aud || (starts_au_if_after_vcl && cur_has_vcl) || (first_slice && cur_has_vcl);
        if new_au && !cur.is_empty() {
            aus.push(std::mem::take(&mut cur));
            cur_has_vcl = false;
        }
        cur.extend_from_slice(&[0, 0, 0, 1]);
        cur.extend_from_slice(nal);
        cur_has_vcl |= vcl;
    }
    if !cur.is_empty() {
        aus.push(cur);
    }
    aus
}

/// Raw AAC frames of an ADTS stream plus (sample rate, channels).
pub fn parse_adts(data: &[u8]) -> (Vec<Vec<u8>>, u32, u16) {
    const RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];
    let mut frames = Vec::new();
    let mut i = 0;
    let (mut rate, mut channels) = (0, 0);
    while i + 7 <= data.len() {
        let h = &data[i..i + 7];
        assert!(h[0] == 0xFF && h[1] & 0xF0 == 0xF0, "ADTS sync lost at {i}");
        let protection_absent = h[1] & 1 == 1;
        rate = RATES[usize::from((h[2] >> 2) & 0xF)];
        channels = u16::from(((h[2] & 1) << 2) | (h[3] >> 6));
        let len = (usize::from(h[3] & 3) << 11) | (usize::from(h[4]) << 3) | usize::from(h[5] >> 5);
        let hdr = if protection_absent { 7 } else { 9 };
        frames.push(data[i + hdr..i + len].to_vec());
        i += len;
    }
    (frames, rate, channels)
}

pub fn ffprobe(path: &Path) -> serde_json::Value {
    let out = Command::new("ffprobe").args(["-v", "error", "-count_frames", "-show_streams", "-show_format", "-of", "json"]).arg(path).output().unwrap();
    assert!(out.status.success(), "ffprobe failed on {}: {}", path.display(), String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

/// Exit status and stderr of a full decode with `-v error`.
pub fn ffmpeg_decode(path: &Path) -> (bool, String) {
    let out = Command::new("ffmpeg").args(["-nostdin", "-v", "error", "-i"]).arg(path).args(["-f", "null", "-"]).output().unwrap();
    (out.status.success(), String::from_utf8_lossy(&out.stderr).into_owned())
}

pub fn assert_decodes_cleanly(path: &Path) {
    let (ok, err) = ffmpeg_decode(path);
    assert!(ok && err.trim().is_empty(), "decoding {} reported errors:\n{err}", path.display());
}

pub fn num(v: &serde_json::Value) -> f64 {
    match v {
        serde_json::Value::String(s) => s.parse().unwrap(),
        serde_json::Value::Number(n) => n.as_f64().unwrap(),
        other => panic!("not a number: {other}"),
    }
}

pub fn stream<'a>(probe: &'a serde_json::Value, codec_type: &str, nth: usize) -> &'a serde_json::Value {
    probe["streams"].as_array().unwrap().iter().filter(|s| s["codec_type"] == codec_type).nth(nth).unwrap_or_else(|| panic!("no {codec_type} stream #{nth}"))
}

// ---- synthetic streams (no ffmpeg needed) -----------------------------------------------------------

pub fn synthetic_tracks() -> Vec<TrackSpec> {
    vec![
        TrackSpec::Video(VideoTrack {
            codec: VideoCodec::H264,
            width: 320,
            height: 240,
            frame_rate: Some(FrameRate::FPS_30),
            codec_private: vec![1, 100, 0, 30, 0xFF, 0xE1, 0, 4, 0x67, 100, 0, 30, 1, 0, 2, 0x68, 0xEE],
            color: ColorInfo { full_range: true },
            name: "Tela".into(),
        }),
        TrackSpec::Audio(AudioTrack {
            codec: AudioCodec::Aac,
            sample_rate: 48_000,
            channels: 2,
            codec_private: vec![0x11, 0x90],
            name: "Mix".into(),
            default: true,
        }),
        TrackSpec::Audio(AudioTrack {
            codec: AudioCodec::PcmF32,
            sample_rate: 48_000,
            channels: 1,
            codec_private: vec![],
            name: "Microfone".into(),
            default: false,
        }),
    ]
}

/// Frame `k` of synthetic track `t`: (pts, keyframe, payload).
pub fn synthetic_frame(t: usize, k: u64) -> (Hns, bool, Vec<u8>) {
    match t {
        0 => {
            let len = 40 + (k as usize * 37) % 300;
            let mut d = vec![(k & 0xFF) as u8; len];
            d[..4].copy_from_slice(&((len - 4) as u32).to_be_bytes());
            (FrameRate::FPS_30.frame_time(k), k.is_multiple_of(15), d)
        }
        1 => (Hns(k as i64 * 1024 * 10_000_000 / 48_000), true, vec![0x21 ^ (k as u8); 20 + (k as usize % 7)]),
        _ => (Hns(k as i64 * 480 * 10_000_000 / 48_000), true, vec![0; 480 * 4]),
    }
}

/// Muxes `seconds` of the synthetic streams in timestamp order; returns the writer for further use.
pub fn synthetic_events(seconds: u64) -> Vec<(usize, Hns, bool, Vec<u8>)> {
    let mut ev = Vec::new();
    for k in 0..30 * seconds {
        let (p, key, d) = synthetic_frame(0, k);
        ev.push((0, p, key, d));
    }
    for k in 0..(48_000 * seconds / 1024) {
        let (p, key, d) = synthetic_frame(1, k);
        ev.push((1, p, key, d));
    }
    for k in 0..(100 * seconds) {
        let (p, key, d) = synthetic_frame(2, k);
        ev.push((2, p, key, d));
    }
    ev.sort_by_key(|e| (e.1, e.0));
    ev
}
