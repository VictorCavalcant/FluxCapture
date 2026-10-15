# FluxCapture — rules for agents and contributors

These rules apply to every human and AI contributor. When `CLAUDE.md` or any other document disagrees
with this file, this file wins.

## 0. Never crash, never lose a recording

People record things that cannot be repeated. A missing device, an unplugged monitor, a full disk, a
corrupt profile, an encoder that refuses a setting or a bad CLI parameter must give an error the user can
act on — never a crash, never a silently broken file. **This outranks feature work.**

1. **Fail with `Result<T, E>`.** Propagate with `?`, add context (`map_err`, an error variant). The UI
   reports the error (status bar / toast / dialog) and carries on.
2. **No panicking shortcuts outside tests:** no `unwrap()`, `expect()`, `panic!`, `unreachable!`, `todo!`,
   `unimplemented!`. Every clean crate carries
   `#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]`.
   `clippy.toml` allows them in tests: a failing test should fail loudly.
3. **No `unsafe`** (`unsafe_code = "forbid"` for the workspace). The single exception is
   `crates/platform`, which calls the operating system's capture, graphics, audio and media APIs. It uses
   `unsafe_code = "deny"` with `#[allow(unsafe_code)]` only on FFI modules, a `// SAFETY:` comment on every
   `unsafe` block (`clippy::undocumented_unsafe_blocks = "deny"`), and a safe, `Result`-returning public API.
4. **Every input-derived number is hostile.** Profiles, settings files, media files being recovered or
   remuxed, CLI / control parameters, device-reported formats and sizes: index with `get()`, use
   `checked_*` / `saturating_*`, never divide by a value that can be zero (frame rates, sample rates,
   sizes), never cast NaN/negative floats to integers without checking, never call `clamp` with bounds that
   can cross, cap allocations sized by input.
5. **Bound every queue and buffer.** Each channel between threads has an explicit capacity; the replay
   buffer has a byte and a duration cap. A full queue is a *counted, reported* event (dropped frame with a
   cause), never unbounded growth and never a silent drop.
6. **Don't cascade.** Lock poisoning is not fatal: `lock().unwrap_or_else(PoisonError::into_inner)`. Every
   worker thread (capture, render, encode, mux, audio) runs under `catch_unwind` and reports a failure as an
   error state; a dead worker must never leave the UI showing "recording" forever.
7. **Finish the file.** Any recording that stops — by the user, by an error, by low disk space, by a lost
   source — finalizes its file. Files are written so that a hard crash still leaves a playable file
   (Matroska with complete clusters), and the session journal lets the next start offer recovery without
   touching the original.
8. **Prove it.** Every crash fix comes with a regression test that failed before. Parsers (the Matroska
   reader used by recovery and remux, codec-config parsers) get mutation tests (truncation, bit flips,
   corrupt sizes) under `catch_unwind`.

## 1. Measure, don't promise

Performance claims need numbers. Changes to the capture → render → encode → mux path come with a
before/after measurement (`fluxcapture-cli bench`, see `docs/performance.md`). Never silently change the
output resolution, frame rate or quality the user picked; automatic adjustments are limited to the preview
and are always visible.

## 2. Licences and assets

- Dependencies must use permissive licences: MIT / Apache-2.0 / BSD / ISC / Zlib / Unicode / CC0 / BSL-1.0,
  or MPL-2.0 used unmodified.
- Never copy GPL/LGPL/AGPL code (FFmpeg, x264, x265, OBS Studio…). Codecs are the operating system's
  (Media Foundation encoder MFTs); containers are implemented here from the public specifications
  (Matroska / EBML RFC 8794 + RFC 9559, ISO/IEC 14496-12 / -14 / -15).
- ffmpeg / ffprobe may be used **only** as external test oracles and fixture generators. They are never
  linked, bundled or shipped.
- No bundled third-party artwork or fonts: icons are drawn in code; text sources use fonts already
  installed on the user's system.

## 3. Engineering rules

- **Layering** (enforced by `cargo xtask layers`): L0 `time` → L1 `mux`, `audio`, `telemetry` → L2
  `platform` → L3 `settings`, `pipeline` → L4 `engine` → L5 `ui` → apps. A crate may depend only on
  lower layers. Nothing below L5 depends on egui/eframe/rfd.
- **One clock:** all media time is `fluxcapture_time::Hns` (100 ns ticks, the QPC domain). Frame `k`'s
  time is computed from `k` (`FrameRate::frame_time`), never accumulated.
- **The UI never carries frames.** UI ↔ engine traffic is commands, state snapshots and metrics. The only
  pixels that reach the UI are the downscaled preview, rate-limited.
- **Everything is a command** (`fluxcapture_engine::Command`): UI buttons, global hotkeys, the tray menu,
  the CLI and the control channel all dispatch the same commands.
- **Quality gates** before every commit: `cargo xtask ci` (fmt check, clippy `-D warnings`, tests,
  layer check).
