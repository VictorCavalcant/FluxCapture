# Performance

FluxCapture does not claim to be lighter than anything without numbers (AGENTS.md §1). This page
says how to measure, what was measured, and how to compare fairly with OBS Studio.

## The benchmark

```bash
cargo xtask dist
```

```bash
dist\fluxcapture-cli.exe bench --seconds 60 --json --codec h264 --res 1080p --fps 60 --rate vbr:20000
```

`bench` records the configured scene (default: the primary monitor) to a temporary file with audio,
samples the process every second and prints: frames submitted / encoded, dropped frames by cause
(render lag, encoder backlog), CPU time per frame of composition, GPU time per frame (timestamp
queries), encoder latency, average process CPU (% of the whole machine) and peak private memory.
Use `--config DIR` with a dedicated configuration folder to keep runs identical.

Rules for a fair run:

- Same content on screen (a looping video is a good stress test: every frame changes).
- Same source, resolution, frame rate, encoder, rate control and bitrate, audio sources and filters.
- Preview off (`bench` disables it) or equally configured in both programs.
- Repeat each scenario at least five times; report the median and the spread.
- Record OS build, GPU driver, and anything else running.

## Results (development machine)

Windows 11 build 26200, Ryzen 5 4600G (6 cores / 12 threads), RTX 3050 (driver 32.0.16.1074),
16 GB, release build, primary monitor 1920×1080 @ 120 Hz playing a video, system audio + microphone
(3 AAC tracks), 20 s per run.

| Scenario | CPU (process) | Composition CPU | Composition GPU | Encoder latency | Dropped | Private memory |
|---|---|---|---|---|---|---|
| 1080p60 H.264 NVENC VBR | 1.07 % | 0.15 ms | 0.21 ms | 5.4 ms | 0 | 173 MB |
| 1080p60 HEVC NVENC VBR | 1.08 % | 0.18 ms | 0.80 ms | 7.3 ms | 0 | 171 MB |
| GUI idle, preview 15 fps (Gaming profile) | 3.4 % | 0.23 ms | 0.24 ms | — | — | 159 MB |

What the numbers say: per frame, the whole capture → compose → convert path costs about 0.2 ms of
CPU and well under 1 ms of GPU. The encoder runs on the GPU's dedicated video engine. Most of the
remaining process CPU is the audio pipeline (three AAC encoders and the DSP chain), the WASAPI threads
and statistics. The GUI costs a few percent more because of the preview (read back and drawn 15 times
per second) — the Gaming profile pauses the preview while recording.

## Comparing with OBS Studio

1. In OBS: Settings › Output › Recording: NVIDIA NVENC H.264, the same rate control and bitrate,
   MKV; Video: base and output 1920×1080, 60 fps; the same audio devices; disable the preview
   (right-click the preview › uncheck *Enable preview*).
2. Run both against the same looping video, one at a time, 60 s each, five times.
3. Measure the process CPU with the same tool for both (e.g. `typeperf "\Process(obs64)\% Processor Time"`
   and `typeperf "\Process(fluxcapture-cli)\% Processor Time"`, divided by the number of logical CPUs),
   and the dropped frames each program reports.
4. Compare output quality too (same bitrate → compare VMAF/SSIM against a lossless capture), not CPU
   alone.

The target percentage reduction is set only after this baseline exists.

## Optimisations in place

- GPU-only recording path (capture texture → canvas → NV12 → encoder); no CPU pixel access.
- Tracked samples return encoder surfaces without polling; no per-frame texture allocations.
- One encode shared by the recording and the replay buffer; packets shared by reference.
- Mip chains only for sources shrunk ≥ 2× (generated on capture, not per render).
- 1:1 layers use the bilinear shader whatever the configured filter (identical result, less work).
- Variable frame rate skips encoding unchanged frames; static desktops produce no capture frames at all.
- Preview readback through a 3-texture staging ring with `DO_NOT_WAIT`: the render thread never waits
  for the GPU; the preview refreshes only while something changes.
- MMCSS scheduling for render ("Capture") and audio ("Pro Audio") threads, GPU thread priority 7,
  above-normal process priority by default; high-resolution waitable timers instead of spinning.
- Release profile: fat LTO, one codegen unit, x86-64-v2 target CPU.
