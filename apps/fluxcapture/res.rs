//! Windows resources for the executables (shared by the build scripts of `fluxcapture` and
//! `fluxcapture-setup`): the application icon, drawn in code by the UI's `app_icon` module, packed
//! into a compiled resource file (`.res`, written directly — no resource compiler needed) that the
//! MSVC linker takes as an input.

use std::path::PathBuf;

#[allow(dead_code)]
#[path = "../../crates/ui/src/app_icon.rs"]
mod app_icon;

/// Sizes Windows picks from (100 % to 300 % scaling, plus the 256 px "jumbo" view).
const SIZES: [u32; 9] = [16, 20, 24, 32, 40, 48, 64, 128, 256];
const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
const RT_MANIFEST: u16 = 24;

/// One entry of a `.res` file with numeric type and name (header: 32 bytes, DWORD-aligned data).
fn entry(out: &mut Vec<u8>, kind: u16, id: u16, flags: u16, data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&32u32.to_le_bytes());
    out.extend_from_slice(&[0xFF, 0xFF]);
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&[0xFF, 0xFF]);
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // data version
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&0x0409u16.to_le_bytes()); // en-US, like rc.exe's default
    out.extend_from_slice(&0u32.to_le_bytes()); // version
    out.extend_from_slice(&0u32.to_le_bytes()); // characteristics
    out.extend_from_slice(data);
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
}

/// A compiled resource file with the application icon (group 1) and, if given, an application
/// manifest (`RT_MANIFEST` 1).
pub fn res(manifest: Option<&[u8]>) -> Vec<u8> {
    // A .res file starts with an empty entry.
    let mut out = vec![0, 0, 0, 0, 0x20, 0, 0, 0, 0xFF, 0xFF, 0, 0, 0xFF, 0xFF, 0, 0];
    out.extend_from_slice(&[0u8; 16]);
    let mut group = Vec::new();
    group.extend_from_slice(&0u16.to_le_bytes());
    group.extend_from_slice(&1u16.to_le_bytes()); // icon
    group.extend_from_slice(&(SIZES.len() as u16).to_le_bytes());
    for (i, n) in SIZES.into_iter().enumerate() {
        let id = i as u16 + 1;
        let image = app_icon::dib(n);
        entry(&mut out, RT_ICON, id, 0x1010, &image);
        let dim = if n >= 256 { 0 } else { n as u8 };
        group.extend_from_slice(&[dim, dim, 0, 0]);
        group.extend_from_slice(&1u16.to_le_bytes()); // planes
        group.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        group.extend_from_slice(&(image.len() as u32).to_le_bytes());
        group.extend_from_slice(&id.to_le_bytes());
    }
    entry(&mut out, RT_GROUP_ICON, 1, 0x1030, &group);
    if let Some(m) = manifest {
        entry(&mut out, RT_MANIFEST, 1, 0x1030, m);
    }
    out
}

/// Writes the resources into `OUT_DIR` and links them into the package's binaries (MSVC targets
/// only; elsewhere the binaries simply have no embedded icon).
pub fn embed(name: &str, manifest: Option<&[u8]>) {
    println!("cargo:rerun-if-changed=../fluxcapture/res.rs");
    println!("cargo:rerun-if-changed=../../crates/ui/src/app_icon.rs");
    let windows = std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|v| v == "windows");
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").is_ok_and(|v| v == "msvc");
    if !(windows && msvc) {
        return;
    }
    let Some(out_dir) = std::env::var_os("OUT_DIR").map(PathBuf::from) else { return };
    let path = out_dir.join(format!("{name}.res"));
    match std::fs::write(&path, res(manifest)) {
        Ok(()) => println!("cargo:rustc-link-arg-bins={}", path.display()),
        Err(e) => println!("cargo:warning=cannot write the resource file: {e}"),
    }
}
