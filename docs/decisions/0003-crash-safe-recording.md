# 0003 — Crash-safe Matroska, session journal, MP4 by remux

**Status:** accepted · **Plan reference:** §7 (recovery), §8 (session recovery)

## Decision

- Recordings are always written as Matroska: an unknown-size Segment followed by complete Clusters,
  each cluster written in one piece and flushed every second. At any instant the file on disk is a
  valid, playable Matroska file; a hard crash loses at most about one second.
- `MkvWriter::finish` adds Cues, the duration and the segment size on a clean stop.
- A journal file per recording lives in `<config>/sessions/`; it is deleted after a clean stop. On
  start-up, a remaining journal means the session was interrupted: the UI offers to recover it, which
  writes `name (recuperado).mkv` (finalized) and never modifies the original.
- MP4 is produced after the recording by remuxing (no re-encoding, faststart), and the MKV is deleted
  only if the user asked and the MP4 was completed.
- The writer finalizes the file before the disk fills (`min_free_mb`) and warns earlier.

## Verified

A recording killed with `taskkill /F` after 7 s decodes with no errors; `fluxcapture-cli recover`
detects the journal and writes a finalized copy (241 frames, 3 audio tracks, duration set); the
journal is removed and the original is untouched.
