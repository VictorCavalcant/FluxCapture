//! Writer → reader round trips, crash/truncation behaviour and mutation tests (no ffmpeg needed).

mod common;

use std::io::{self, Cursor, Seek, SeekFrom, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};

use common::*;
use fluxcapture_mux::{FrameRef, Hns, MkvIndex, MkvOptions, MkvWriter, MuxError, TrackSpec, recover_mkv, remux_mkv_to_mp4, scan_mkv};

fn mux(seconds: u64, finish: bool) -> (Vec<u8>, Vec<(usize, Hns, bool, Vec<u8>)>) {
    let ev = synthetic_events(seconds);
    let mut w = MkvWriter::new(Cursor::new(Vec::new()), &synthetic_tracks(), MkvOptions { title: "Teste".into(), ..MkvOptions::default() }).unwrap();
    for (i, (t, p, k, d)) in ev.iter().enumerate() {
        w.write(*t, *p, *k, d).unwrap();
        if i % 50 == 49 {
            w.flush().unwrap();
        }
    }
    let bytes = if finish {
        w.finish().unwrap().into_inner()
    } else {
        w.flush().unwrap();
        w.get_ref().get_ref().clone()
    };
    (bytes, ev)
}

fn scan(bytes: &[u8]) -> Result<MkvIndex, MuxError> {
    scan_mkv(Cursor::new(bytes), bytes.len() as u64, 1_000_000)
}

fn check_frames(bytes: &[u8], idx: &MkvIndex, ev: &[(usize, Hns, bool, Vec<u8>)]) {
    assert_eq!(idx.frames.len(), ev.len());
    for (f, (t, p, k, d)) in idx.frames.iter().zip(ev) {
        assert_eq!(f.track, *t);
        assert_eq!(f.pts, Hns::from_millis(p.as_millis_rounded()));
        assert_eq!(f.keyframe, *k || *t != 0);
        assert_eq!(&bytes[f.offset as usize..f.offset as usize + f.size as usize], &d[..]);
    }
}

#[test]
fn finished_file_round_trips() {
    let (bytes, ev) = mux(3, true);
    let idx = scan(&bytes).unwrap();
    assert_eq!(idx.tracks, synthetic_tracks());
    assert!(idx.finalized);
    assert_eq!(idx.valid_until, bytes.len() as u64);
    check_frames(&bytes, &idx, &ev);
    // Video ends at 3 s; the last AAC frame (140 * 1024 samples) at 2.9867 s; PCM at 3.0 s.
    assert!((idx.duration.as_secs_f64() - 3.0).abs() < 0.002, "{}", idx.duration);
}

#[test]
fn unfinished_file_is_complete_up_to_last_flush() {
    let (bytes, ev) = mux(3, false);
    let idx = scan(&bytes).unwrap();
    assert!(!idx.finalized);
    assert_eq!(idx.valid_until, bytes.len() as u64);
    check_frames(&bytes, &idx, &ev);
}

fn prefix_ok(full: &[FrameRef], part: &MkvIndex, len: usize) {
    assert!(part.frames.len() <= full.len());
    assert_eq!(&full[..part.frames.len()], &part.frames[..]);
    assert!(part.valid_until <= len as u64);
    for f in &part.frames {
        assert!(f.offset + u64::from(f.size) <= len as u64);
    }
}

#[test]
fn every_truncation_scans_without_panicking() {
    let (bytes, _) = mux(2, false);
    let full = scan(&bytes).unwrap().frames;
    let header_len = 400; // well past Tracks
    // Every length in the header, a stride over the rest, and every length around each block boundary.
    let stride = (bytes.len() / 3000).max(7);
    let mut lens: Vec<usize> =
        (0..header_len.min(bytes.len())).chain((header_len..bytes.len()).step_by(stride)).chain(bytes.len().saturating_sub(64)..=bytes.len()).collect();
    for f in &full {
        let (a, b) = (f.offset as usize, (f.offset + u64::from(f.size)) as usize);
        lens.extend([a.saturating_sub(5), a - 1, a, a + 1, b - 1, b, (b + 1).min(bytes.len())]);
    }
    lens.sort_unstable();
    lens.dedup();
    for len in lens {
        let part = &bytes[..len];
        let r = catch_unwind(AssertUnwindSafe(|| scan(part)));
        let r = r.unwrap_or_else(|_| panic!("panic at truncation {len}"));
        if let Ok(idx) = r {
            prefix_ok(&full, &idx, len);
        }
    }
    // The finished file truncated anywhere behaves the same.
    let (bytes, _) = mux(2, true);
    let full = scan(&bytes).unwrap().frames;
    for len in (0..bytes.len()).step_by((bytes.len() / 3000).max(13)) {
        let r = catch_unwind(AssertUnwindSafe(|| scan(&bytes[..len]))).unwrap_or_else(|_| panic!("panic at truncation {len}"));
        if let Ok(idx) = r {
            assert!(!idx.finalized || len == bytes.len());
            prefix_ok(&full, &idx, len);
        }
    }
}

#[test]
fn random_byte_flips_never_panic() {
    let (bytes, _) = mux(2, true);
    let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = || {
        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        (seed >> 33) as usize
    };
    for round in 0..3000 {
        let mut m = bytes.clone();
        let flips = 1 + next() % 4;
        for _ in 0..flips {
            // Bias towards the header, where structure is dense.
            let pos = if next() % 2 == 0 { next() % 600.min(m.len()) } else { next() % m.len() };
            m[pos] ^= 1 << (next() % 8);
        }
        if round % 5 == 0 {
            let cut = next() % m.len();
            m.truncate(cut);
        }
        let len = m.len();
        let r = catch_unwind(AssertUnwindSafe(|| scan(&m))).unwrap_or_else(|_| panic!("panic in round {round}"));
        if let Ok(idx) = r {
            for f in &idx.frames {
                assert!(f.offset + u64::from(f.size) <= len as u64);
                assert!(f.track < idx.tracks.len());
            }
            assert!(idx.valid_until <= len as u64);
        }
    }
}

#[test]
fn hostile_sizes_are_refused_or_skipped() {
    let (bytes, _) = mux(1, true);
    // Overwrite every possible vint size field with the largest 8-byte size: no allocation blowups.
    for pos in 0..300.min(bytes.len() - 8) {
        let mut m = bytes.clone();
        m[pos..pos + 8].copy_from_slice(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFE]);
        let r = catch_unwind(AssertUnwindSafe(|| scan(&m))).unwrap_or_else(|_| panic!("panic with huge size at {pos}"));
        if let Ok(idx) = r {
            assert!(idx.valid_until <= m.len() as u64);
        }
    }
    // A file_len larger than the data is treated as truncation, not trusted.
    let r = scan_mkv(Cursor::new(&bytes[..bytes.len() / 2]), bytes.len() as u64 * 10, 1_000_000).unwrap();
    assert!(!r.frames.is_empty());
    // The frame cap is an error, not a silent truncation.
    assert!(matches!(scan_mkv(Cursor::new(&bytes), bytes.len() as u64, 10), Err(MuxError::TooLarge(_))));
    assert!(scan(b"not a matroska file at all").is_err());
    assert!(scan(&[]).is_err());
}

/// A writer that fails after `budget` bytes, like a full disk.
struct FailingWriter {
    inner: Cursor<Vec<u8>>,
    budget: usize,
}

impl Write for FailingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.len() > self.budget {
            return Err(io::Error::other("disk full"));
        }
        self.budget -= buf.len();
        self.inner.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for FailingWriter {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

#[test]
fn write_failure_poisons_the_writer_but_keeps_earlier_clusters() {
    let w = FailingWriter { inner: Cursor::new(Vec::new()), budget: 250_000 };
    let mut w = MkvWriter::new(w, &synthetic_tracks(), MkvOptions::default()).unwrap();
    let mut failed = false;
    for (t, p, k, d) in synthetic_events(3) {
        if let Err(e) = w.write(t, p, k, &d) {
            assert!(matches!(e, MuxError::Io(_)));
            failed = true;
            break;
        }
    }
    assert!(failed);
    assert!(matches!(w.write(1, Hns::from_secs(9), true, &[1]), Err(MuxError::Failed)));
    assert!(matches!(w.flush(), Err(MuxError::Failed)));
    let bytes = w.get_ref().inner.get_ref().clone();
    let idx = scan(&bytes).unwrap();
    assert!(!idx.frames.is_empty());
}

#[test]
fn recover_finalizes_a_truncated_file_and_mp4_refuses_pcm() {
    let dir = scratch("recover_synthetic");
    let (bytes, ev) = mux(3, false);
    let cut = bytes.len() - 777; // mid-cluster
    let src = dir.join("crashed.mkv");
    std::fs::write(&src, &bytes[..cut]).unwrap();
    let dst = dir.join("recovered.mkv");
    let report = recover_mkv(&src, &dst).unwrap();
    assert_eq!(std::fs::read(&src).unwrap().len(), cut, "source untouched");
    assert!(!dir.join("recovered.mkv.part").exists());
    let out = std::fs::read(&dst).unwrap();
    let idx = scan(&out).unwrap();
    assert!(idx.finalized);
    assert_eq!(idx.frames.len(), report.frames);
    assert_eq!(report.dropped_frames, 0);
    assert!(report.frames < ev.len() && report.frames > ev.len() - 100);
    assert!(report.dropped_tail_bytes > 0);
    assert_eq!(report.bytes, out.len() as u64);
    assert_eq!(idx.tracks, synthetic_tracks().into_iter().collect::<Vec<TrackSpec>>());

    assert!(matches!(remux_mkv_to_mp4(&src, &dir.join("x.mp4")), Err(MuxError::Unsupported(_))));
    assert!(!dir.join("x.mp4.part").exists());
    assert!(recover_mkv(&src, &src).is_err(), "never overwrite the source");
}

/// A seekable sink that only counts (measures the muxer, not the disk).
#[derive(Default)]
struct CountingSink {
    pos: u64,
    len: u64,
}

impl Write for CountingSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.pos += buf.len() as u64;
        self.len = self.len.max(self.pos);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for CountingSink {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.pos = match pos {
            SeekFrom::Start(p) => p,
            SeekFrom::End(d) => self.len.saturating_add_signed(d),
            SeekFrom::Current(d) => self.pos.saturating_add_signed(d),
        };
        Ok(self.pos)
    }
}

#[test]
fn mkv_write_throughput() {
    // 2 minutes of 60 fps video at ~50 Mbit/s plus AAC: the muxer must be far faster than real time.
    let tracks = synthetic_tracks();
    let mut w = MkvWriter::new(CountingSink::default(), &tracks[..2], MkvOptions::default()).unwrap();
    let frame = vec![0x5Au8; 100_000];
    let aac = vec![0x21u8; 400];
    let start = std::time::Instant::now();
    let frames = 120 * 60;
    for k in 0..frames {
        let pts = fluxcapture_mux::FrameRate::FPS_60.frame_time(k);
        w.write(0, pts, k % 120 == 0, &frame).unwrap();
        if k % 4 < 3 {
            w.write(1, pts, true, &aac).unwrap();
        }
        if k % 60 == 59 {
            w.flush().unwrap();
        }
    }
    let sink = w.finish().unwrap();
    let secs = start.elapsed().as_secs_f64();
    let mb = sink.len as f64 / 1e6;
    println!("mkv write: {mb:.0} MB in {secs:.3} s = {:.0} MB/s, {:.0}x real time", mb / secs, 120.0 / secs);
    assert!(120.0 / secs > 20.0, "muxer too slow: {secs:.3} s for 2 minutes");
}
