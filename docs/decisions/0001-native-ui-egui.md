# 0001 — Native egui interface instead of Tauri + React

**Status:** accepted · **Plan reference:** §3, §4 ("Interface: Tauri + React")

## Context

The plan proposed Tauri 2 + React for the interface, with the media path kept in Rust. The plan's
own rule is that the UI must never be on the media path and that resource use is the product's
main differentiator.

## Decision

The interface is a native egui (eframe/glow) window in the same process.

## Why

- Tauri renders through WebView2: an extra browser engine with several processes and typically
  100–200 MB of memory before the app does anything, plus a Node/npm toolchain to build.
- A recorder's UI is mostly idle; egui repaints only when asked (here: at the preview rate, or
  10 Hz for status), so its cost is a few milliseconds per repaint, on demand.
- One language, one binary (~11 MB, no installer runtime), the same conventions as FilmCraft.
- The UI still never touches frames: it drives the engine through `Command`s and reads snapshots;
  the only pixels it receives are the downscaled preview.

## Consequences

- Rich web-style layouts are harder; the UI favours plain panels.
- Every widget is drawn by us (icons are font glyphs or painted shapes: no bundled artwork).
