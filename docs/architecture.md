# Architecture

## Crates and layers

| Layer | Crate | Role |
|---|---|---|
| L0 | `time` | `Hns` (100 ns ticks — the QPC / WGC / WASAPI / Media Foundation clock), rational `FrameRate`, exact sample clocks. Frame `k`'s time is computed from `k`, never accumulated. |
| L1 | `mux` | Matroska writer (crash-safe) and tolerant reader, MP4 writer (faststart), H.264/HEVC parameter-set parsing (avcC/hvcC), remux and recovery. Pure Rust from the public specs. |
| L1 | `audio` | Mixer: per-source timeline placement with clock-drift correction, polyphase resampler, filters (high-pass, noise suppression, gate, compressor), limiter, meters, track routing. |
| L1 | `telemetry` | Rotating log with secret redaction, metric history, bottleneck diagnostics. |
| L2 | `platform` | Everything that talks to Windows (the only crate with `unsafe`): D3D11 device, WGC / Desktop Duplication / webcam capture, the compositor, NV12 conversion, Media Foundation encoders, WASAPI, AAC, hotkeys, system metrics. A stub backend keeps other OSes building. |
| L3 | `settings` | The configuration model, presets, validation (`sanitized`), atomic persistence, portable mode, file-name templates. |
| L3 | `pipeline` | Packet router (one encode, many outputs), file recording (pause, split, disk guard, journal, MP4 remux), replay buffer. |
| L4 | `engine` | Application core: commands, session lifecycle, scenes → layers (text/image caches), metrics, diagnostics, advisor, setup assistant, control channel. |
| L5 | `ui` | egui interface. |
| apps | `fluxcapture`, `fluxcapture-cli` | Binaries. |
| apps | `fluxcapture-setup` | Per-user installer / uninstaller; `cargo xtask dist` appends the binaries to it (decision 0005). |

`cargo xtask layers` enforces that a crate depends only on lower layers and that nothing below L5
uses UI crates.

## Threads and data flow

```
WGC thread pool ──copy──▶ source texture ─┐
DXGI duplication thread ─▶ source texture ─┤
webcam thread ──upload──▶ source texture ──┤
                                           ▼
            render thread (MMCSS "Capture", high-res waitable timer, GPU priority 7)
              ├─ compose layers → canvas (BGRA8, output size)          [GPU]
              ├─ canvas → NV12 pool surface (Y + UV passes)             [GPU]
              │      └─▶ encoder thread: tracked sample → Media Foundation MFT (NVENC/AMF/QSV)
              │                └─▶ Annex B packets ─┐
              └─ preview: downscale → staging ring → RGBA (no GPU waits) ─▶ UI texture
                                                     │
WASAPI threads (MMCSS "Pro Audio", QPC timestamps) ─▶ mixer thread (10 ms ticks, 80 ms behind)
                                                     └─▶ AAC / PCM packets per track ─┐
                                                                                      ▼
                                     router thread: Annex B → length-prefixed, codec config,
                                                    fan-out (Arc<[u8]>, no copies)
                                                     ├─▶ writer thread: Matroska file(s)
                                                     └─▶ replay ring (bounded by s and MB)
```

- **Timestamps.** Video frames carry their *scheduled* tick time (`epoch + frame_time(k)`), so pacing
  jitter never reaches the file. Audio chunks carry the device's QPC position; the mixer maps them onto
  a 48 kHz timeline and steers a tiny resampling correction (≤ ±0.5%) to follow device clock drift.
  Both live in the same QPC domain, so A/V sync needs no heuristics.
- **Zero-copy video.** Pool surfaces are wrapped with `MFCreateDXGISurfaceBuffer` in *tracked* samples;
  when the encoder releases a sample, a callback returns the surface to the pool. A full pool means the
  encoder is behind → the frame is counted as `dropped_encoder_backlog` (never silently).
- **Bounded everything.** Encoder pool (6), router input (512 video / 2048 audio packets), writer queue
  (8192), replay buffer (seconds + MB), mixer FIFOs (2 s), WGC pool (2 buffers). Overflows are counted
  and surface in diagnostics.
- **Idle cost.** With no preview and no encoder the render thread sleeps on its command channel and
  releases every capture session. A static desktop produces no WGC frames; in variable-frame-rate mode
  it also produces no encoded frames (at least one per second).

## Crash safety

- Every worker thread runs under `catch_unwind` (`platform::guarded`) and reports errors as state.
- Matroska clusters are written whole and flushed every second: after a hard crash, the file plays up
  to the last second. The session journal (`<config>/sessions/*.json`) is removed only after a clean
  stop; at start-up a remaining journal offers recovery (`mux::remux::recover_mkv` → a finalized copy).
- Settings are written atomically (temporary file + rename); a broken file is set aside, never
  overwritten.

## Control channel

`Settings › Advanced › Control channel` (or `advanced.control_port` in `config.toml`) opens a JSON-lines
server on `127.0.0.1`. Requests: `{"id":1,"method":"state"}`, `{"id":2,"method":"command","params":{"name":"record.toggle"}}`,
`settings`, `recoveries`, `log`. Command names: `record.toggle|start|stop|pause`, `replay.toggle|save`,
`screenshot`, `mic.toggle`, `profile.set` / `scene.set` / `advisor.apply` (with `arg`),
`folder.open`, `devices.refresh`, `audio.reset_clip`.
