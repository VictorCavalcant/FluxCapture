//! Embeds the application icon in `fluxcapture.exe` (Explorer, shortcuts, pinned taskbar
//! buttons); see `res.rs`.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod res;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // Other targets: no embedded icon (the window still sets its own at run time).
    res::embed("fluxcapture-icon", None);
}
