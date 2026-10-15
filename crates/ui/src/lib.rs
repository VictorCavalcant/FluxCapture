//! The FluxCapture desktop interface (egui).
//!
//! The UI never touches frames: it drives the [`Engine`] with commands and settings changes, and
//! draws state snapshots, meters and the rate-limited preview image. Engine work runs on its own
//! threads, so a busy UI cannot disturb a recording; the UI repaints only as often as the preview
//! (or 10 Hz) needs.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod app;
mod app_icon;
mod diagnostics;
mod mixer;
mod preview;
mod settings_window;
mod sources;
mod theme;
mod tray;
mod widgets;
mod windows;

pub use app::{FluxApp, run};
