//! FluxCapture desktop application.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

fn main() {
    // Last-resort guard (AGENTS.md §0.7): every panic is logged; worker threads already turn their
    // own panics into errors, so a panic here means a UI bug, and recordings are finalized by
    // the engine's Drop while unwinding.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!("panic: {info}");
        default_hook(info);
    }));
    if let Err(e) = fluxcapture_ui::run() {
        log::error!("FluxCapture could not start: {e}");
        eprintln!("FluxCapture could not start: {e}");
    }
}
