# fluxcapture-mux

Containers for FluxCapture (layer L0; depends only on `fluxcapture-time`, `thiserror`, `log`).

| Module     | What it does |
|------------|--------------|
| `codec`    | Annex B start-code splitting, conversion to 4-byte length-prefixed NAL units, keyframe detection, `avcC` / `hvcC` builders, AAC-LC `AudioSpecificConfig`. |
| `mkv`      | Crash-resilient Matroska writer (`MkvWriter`). |
| `mkv_read` | Tolerant Matroska scanner (`scan_mkv`) for recovery and remux. |
| `mp4`      | Faststart ISO BMFF writer (`write_mp4`). |
| `remux`    | `remux_mkv_to_mp4` and `recover_mkv` (write `dst.part`, sync, rename; the source is never touched). |

## Specifications used

Everything is implemented from the public specifications; no code from FFmpeg, MKVToolNix, GPAC or
other projects was used. ffmpeg/ffprobe appear only as external test oracles and fixture generators.

- **RFC 8794** — Extensible Binary Meta Language (EBML), 2020.
- **RFC 9559** — Matroska Media Container Format Specification, 2024 (codec IDs per the Matroska codec
  mappings: `V_MPEG4/ISO/AVC`, `V_MPEGH/ISO/HEVC`, `A_AAC`, `A_PCM/IEEE_FLOAT`).
- **ISO/IEC 14496-12** (ISO base media file format), 7th edition (2022).
- **ISO/IEC 14496-14** (MP4 file format), 2nd edition (2020) — `esds` / `mp4a`.
- **ISO/IEC 14496-15** (NAL-unit-structured video in ISOBMFF), 6th edition (2022) —
  `AVCDecoderConfigurationRecord` §5.3.3, `HEVCDecoderConfigurationRecord` §8.3.3.
- **ISO/IEC 14496-1** (Systems) — descriptor framing used by `esds`.
- **ISO/IEC 14496-3** (Audio) — `AudioSpecificConfig`.
- **ITU-T H.264** and **ITU-T H.265** — Annex B byte streams, NAL unit headers, SPS syntax
  (`profile_tier_level`, Exp-Golomb, emulation prevention).

## Crash-resilience design

```text
EBML header
Segment            size = 8-byte "unknown" (patched by finish)
  Void             reserved for the SeekHead (written by finish)
  Info             Duration slot reserved as an 11-byte Void (replaced by finish)
  Tracks
  Cluster          empty (Timestamp 0) so even a file with no packets opens everywhere
  Cluster …        each written whole, with a known size
  Cues             (finish)
```

- The header is written and flushed in `MkvWriter::new`, so a valid file exists from the first moment.
- Clusters are assembled in a reused in-memory buffer and appended to the output as one complete element
  when they close (at each video keyframe, at `max_cluster_duration` / `max_cluster_bytes`, when a block
  timestamp would leave the i16 range, and on `flush`). The bytes on disk are therefore always
  "header + complete clusters", optionally followed by a partially flushed cluster after a crash.
- An unknown-size Segment followed by complete Clusters is exactly the shape of a live Matroska stream,
  so demuxers open an unfinished file directly; the tests check this with ffprobe/ffmpeg at every
  flush point and after a mid-cluster cut.
- `finish` only appends Cues and then overwrites same-sized reserved regions (SeekHead, Duration, Segment
  size). If it is interrupted, the file is still the valid unfinished form.
- A failed write poisons the writer (`MuxError::Failed`): nothing is appended after a possibly partial
  cluster, so the bytes before it stay valid.
- `recover_mkv` scans the unfinished file (`scan_mkv` stops at the last complete block), drops frames
  before the first video keyframe and rewrites a finalized copy with Cues and Duration.

## Hostile input

`scan_mkv` and the codec-config parsers never panic and never index out of bounds: element sizes are
checked against their parent and the file length, CodecPrivate is capped at 1 MiB, strings at 4 KiB,
tracks at 64, frames at the caller's `max_frames`. Mutation tests (every truncation length around block
boundaries, thousands of random bit flips, oversized size fields) run under `catch_unwind`.

## Limitations

- No B-frames (pts == dts); the MP4 writer has no `ctts`.
- MP4 refuses float PCM tracks (`MuxError::Unsupported`); Matroska carries them.
- The reader skips laced blocks (our writer never laces) and tracks with ContentEncodings.
- Block timestamps use a 1 ms TimestampScale, as recommended by RFC 9559.
