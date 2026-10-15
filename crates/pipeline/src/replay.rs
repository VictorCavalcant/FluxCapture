//! The replay buffer: the last N seconds of encoded packets in memory, bounded by duration and
//! bytes, saved on demand as a clip without re-encoding.

use std::collections::VecDeque;
use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;

use crossbeam_channel::{Receiver, bounded};
use fluxcapture_mux::TrackSpec;
use fluxcapture_mux::mkv::{MkvOptions, MkvWriter};
use fluxcapture_mux::mp4::{Mp4Sample, write_mp4};
use fluxcapture_time::Hns;

use crate::{Packet, StreamKind};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ReplayConfig {
    pub seconds: u32,
    pub max_bytes: usize,
}

pub struct ReplayBuffer {
    cfg: ReplayConfig,
    packets: VecDeque<Packet>,
    bytes: usize,
    newest_video: Hns,
}

impl ReplayBuffer {
    pub fn new(cfg: ReplayConfig) -> ReplayBuffer {
        ReplayBuffer {
            cfg: ReplayConfig { seconds: cfg.seconds.clamp(1, 3600), max_bytes: cfg.max_bytes.max(1 << 20) },
            packets: VecDeque::new(),
            bytes: 0,
            newest_video: Hns(0),
        }
    }

    pub fn push(&mut self, p: Packet) {
        if p.kind == StreamKind::Video {
            self.newest_video = self.newest_video.max(p.pts);
        }
        self.bytes += p.data.len();
        self.packets.push_back(p);
        // Keep the window plus one GOP of slack, so the saved clip can start on a keyframe and
        // still cover the full duration.
        let keep = Hns::from_secs(i64::from(self.cfg.seconds) + 10);
        while let Some(front) = self.packets.front() {
            let too_old = self.newest_video - front.pts > keep;
            if !(too_old || self.bytes > self.cfg.max_bytes) {
                break;
            }
            if let Some(f) = self.packets.pop_front() {
                self.bytes = self.bytes.saturating_sub(f.data.len());
            }
        }
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Seconds of video held (oldest keyframe to newest frame).
    pub fn seconds_held(&self) -> f32 {
        self.packets.iter().find(|p| p.kind == StreamKind::Video && p.keyframe).map_or(0.0, |k| (self.newest_video - k.pts).as_secs_f64() as f32)
    }

    /// The clip: from the latest keyframe at or before `newest - seconds` (or the oldest keyframe
    /// held) to the end. Audio before that keyframe is left out.
    pub fn snapshot(&self) -> Vec<Packet> {
        let from = self.newest_video - Hns::from_secs(i64::from(self.cfg.seconds));
        let keys: Vec<usize> = self.packets.iter().enumerate().filter(|(_, p)| p.kind == StreamKind::Video && p.keyframe).map(|(i, _)| i).collect();
        let Some(&first) = keys.first() else { return Vec::new() };
        let start = keys.iter().rev().copied().find(|&i| self.packets.get(i).is_some_and(|p| p.pts <= from)).unwrap_or(first);
        let Some(origin) = self.packets.get(start).map(|p| p.pts) else { return Vec::new() };
        self.packets.iter().skip(start).filter(|p| p.pts >= origin).cloned().collect()
    }
}

/// Writes a clip on a background thread; the receiver gets the final path or an error.
pub fn save_clip_async(path: PathBuf, tracks: Vec<TrackSpec>, packets: Vec<Packet>, mp4: bool) -> Result<Receiver<Result<PathBuf, String>>, String> {
    if packets.is_empty() {
        return Err("the replay buffer is still empty".into());
    }
    let (tx, rx) = bounded(1);
    std::thread::Builder::new()
        .name("fluxcapture-replay-save".into())
        .spawn(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| save_clip(&path, &tracks, &packets, mp4)))
                .unwrap_or_else(|_| Err("internal error while saving the replay".into()));
            let _ = tx.send(r.map(|()| path));
        })
        .map_err(|e| format!("cannot start the save thread: {e}"))?;
    Ok(rx)
}

/// Writes `packets` (starting at a video keyframe) to `path`.
pub fn save_clip(path: &std::path::Path, tracks: &[TrackSpec], packets: &[Packet], mp4: bool) -> Result<(), String> {
    let origin = packets.first().map(|p| p.pts).ok_or("empty clip")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let part = path.with_extension("part");
    let result = (|| -> Result<(), String> {
        let file = BufWriter::with_capacity(1 << 20, File::create(&part).map_err(|e| format!("cannot create {}: {e}", part.display()))?);
        if mp4 {
            let samples: Vec<Mp4Sample> = packets
                .iter()
                .filter(|p| p.track_index() < tracks.len())
                .map(|p| Mp4Sample { track: p.track_index(), pts: p.pts - origin, keyframe: p.keyframe, size: p.data.len() as u32 })
                .collect();
            let kept: Vec<&Packet> = packets.iter().filter(|p| p.track_index() < tracks.len()).collect();
            let mut fetch = |i: usize, buf: &mut Vec<u8>| -> std::io::Result<()> {
                buf.clear();
                if let Some(p) = kept.get(i) {
                    buf.extend_from_slice(&p.data);
                }
                Ok(())
            };
            write_mp4(file, tracks, &samples, &mut fetch).map_err(|e| e.to_string())?;
        } else {
            let opts =
                MkvOptions { title: "FluxCapture replay".into(), writing_app: format!("FluxCapture {}", env!("CARGO_PKG_VERSION")), ..MkvOptions::default() };
            let mut mkv = MkvWriter::new(file, tracks, opts).map_err(|e| e.to_string())?;
            for p in packets {
                if p.track_index() < tracks.len() {
                    mkv.write(p.track_index(), p.pts - origin, p.keyframe, &p.data).map_err(|e| e.to_string())?;
                }
            }
            mkv.finish().map_err(|e| e.to_string())?;
        }
        Ok(())
    })();
    match result {
        Ok(()) => std::fs::rename(&part, path).map_err(|e| format!("cannot finish {}: {e}", path.display())),
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn pkt(kind: StreamKind, ms: i64, key: bool, len: usize) -> Packet {
        Packet { kind, pts: Hns::from_millis(ms), keyframe: key, data: Arc::from(vec![0u8; len].as_slice()) }
    }

    #[test]
    fn window_and_memory_are_bounded() {
        let mut r = ReplayBuffer::new(ReplayConfig { seconds: 5, max_bytes: 2 << 20 });
        for i in 0..60 * 60 {
            let ms = i * 1000 / 60;
            r.push(pkt(StreamKind::Video, ms, i % 120 == 0, 1000));
            if i % 3 == 0 {
                r.push(pkt(StreamKind::Audio(0), ms, true, 300));
            }
        }
        assert!(r.seconds_held() <= 16.0, "{}", r.seconds_held());
        let clip = r.snapshot();
        assert!(clip[0].keyframe && clip[0].kind == StreamKind::Video);
        let span = (clip.last().unwrap().pts - clip[0].pts).as_secs_f64();
        assert!((5.0..=7.1).contains(&span), "{span}");
        // Memory cap wins over duration.
        let mut small = ReplayBuffer::new(ReplayConfig { seconds: 60, max_bytes: 1 << 20 });
        for i in 0..10_000 {
            small.push(pkt(StreamKind::Video, i * 16, i % 60 == 0, 10_000));
        }
        assert!(small.bytes() <= 1 << 20);
        assert!(ReplayBuffer::new(ReplayConfig { seconds: 5, max_bytes: 0 }).snapshot().is_empty());
    }
}
