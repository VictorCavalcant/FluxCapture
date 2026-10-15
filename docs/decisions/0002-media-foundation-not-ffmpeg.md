# 0002 — Windows encoders through Media Foundation; containers in Rust; no FFmpeg

**Status:** accepted · **Plan reference:** §4 ("FFmpeg/libav"), §11 (licensing risk)

## Context

The plan listed FFmpeg/libav for codecs and muxing, and flagged its licensing as a risk.

## Decision

- Video and AAC encoding use the operating system's Media Foundation transforms: the GPU vendors'
  hardware encoders (NVENC, AMF, Quick Sync) are installed as MFTs; Microsoft's software H.264
  encoder is the fallback.
- Matroska and MP4 are written (and Matroska read back) by `fluxcapture-mux`, implemented from the
  public specifications.
- `mfplat.dll` is loaded at run time so the app starts (and explains what is missing) on Windows N.

## Why

- No GPL/LGPL obligations or build complexity; nothing to ship besides the executable.
- Hardware MFTs accept Direct3D 11 textures (via `IMFDXGIDeviceManager`), which keeps the whole path
  on the GPU — the main performance goal.
- Owning the muxer gives exactly the behaviour a recorder needs: crash-safe Matroska, one encode
  feeding several outputs, a replay buffer of encoded packets, recovery of interrupted files.

## Consequences

- Codecs are limited to what Windows offers (H.264, HEVC; AV1 where the GPU has an AV1 MFT — the
  container side for AV1 is not implemented yet).
- No B-frames for now (pts = dts; MP4 has no `ctts`), which also keeps encoder latency low.
- ffmpeg/ffprobe are used only as test oracles.
