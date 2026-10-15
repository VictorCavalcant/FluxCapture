//! Project automation: `cargo xtask <task>`.
//!
//! - `ci`: everything below, in order (what must pass before a commit).
//! - `fmt`: `cargo fmt --all --check`.
//! - `clippy`: `cargo clippy --workspace --all-targets -- -D warnings`.
//! - `test`: `cargo test --workspace`.
//! - `layers`: checks the crate dependency layering (AGENTS.md §3).
//! - `dist`: release build of both binaries into `dist/`, plus the setup program
//!   `dist/FluxCapture-Setup-<version>.exe` that installs them.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

#[allow(dead_code)]
#[path = "../../apps/fluxcapture-setup/src/payload.rs"]
mod payload;

/// Layer of each workspace crate; a crate may depend only on strictly lower layers.
const LAYERS: &[(&str, u8)] = &[
    ("fluxcapture-time", 0),
    ("fluxcapture-mux", 1),
    ("fluxcapture-audio", 1),
    ("fluxcapture-telemetry", 1),
    ("fluxcapture-platform", 2),
    ("fluxcapture-settings", 3),
    ("fluxcapture-pipeline", 3),
    ("fluxcapture-engine", 4),
    ("fluxcapture-ui", 5),
    ("fluxcapture", 6),
    ("fluxcapture-cli", 6),
    ("fluxcapture-setup", 6),
];

/// Dependencies only the UI layer (and apps) may use.
const UI_ONLY: &[&str] = &["egui", "eframe", "rfd", "tray-icon", "winit"];

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."))
}

fn run(args: &[&str]) -> bool {
    println!("$ cargo {}", args.join(" "));
    Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into())).args(args).current_dir(root()).status().is_ok_and(|s| s.success())
}

/// Names of `[dependencies]` (not dev-dependencies) in a manifest.
fn dependencies(manifest: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_deps = false;
    for line in manifest.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_deps = t == "[dependencies]" || (t.starts_with("[target.") && t.ends_with(".dependencies]"));
            continue;
        }
        if in_deps
            && let Some((name, _)) = t.split_once('=')
            && !t.starts_with('#')
        {
            out.push(name.trim().trim_matches('"').to_string());
        }
    }
    out
}

fn package_name(manifest: &str) -> Option<String> {
    let mut in_pkg = false;
    for line in manifest.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_pkg = t == "[package]";
            continue;
        }
        if in_pkg && let Some(v) = t.strip_prefix("name") {
            return Some(v.trim_start_matches([' ', '=']).trim().trim_matches('"').to_string());
        }
    }
    None
}

fn layers() -> bool {
    let mut ok = true;
    let mut manifests = Vec::new();
    for dir in ["crates", "apps"] {
        if let Ok(rd) = std::fs::read_dir(root().join(dir)) {
            for e in rd.flatten() {
                let m = e.path().join("Cargo.toml");
                if m.exists() {
                    manifests.push(m);
                }
            }
        }
    }
    for m in manifests {
        let Ok(text) = std::fs::read_to_string(&m) else { continue };
        let Some(name) = package_name(&text) else { continue };
        let Some(&(_, layer)) = LAYERS.iter().find(|(n, _)| *n == name) else {
            println!("✗ {name}: not in the layer table (xtask/src/main.rs)");
            ok = false;
            continue;
        };
        for dep in dependencies(&text) {
            if let Some(&(_, dl)) = LAYERS.iter().find(|(n, _)| *n == dep)
                && dl >= layer
            {
                println!("✗ {name} (L{layer}) depends on {dep} (L{dl})");
                ok = false;
            }
            if UI_ONLY.contains(&dep.as_str()) && layer < 5 {
                println!("✗ {name} (L{layer}) depends on UI crate {dep}");
                ok = false;
            }
        }
    }
    if ok {
        println!("✓ layering");
    }
    ok
}

/// Packs the release binaries into the setup program: `dist/FluxCapture-Setup-<version>.exe`.
fn setup(dist: &Path) -> bool {
    let release = root().join("target/release");
    let read = |name: &str| std::fs::read(release.join(name)).map_err(|e| println!("✗ cannot read target/release/{name}: {e}")).ok();
    let (Some(program), Some(app), Some(cli)) = (read("fluxcapture-setup.exe"), read("fluxcapture.exe"), read("fluxcapture-cli.exe")) else { return false };
    let name = format!("FluxCapture-Setup-{}.exe", env!("CARGO_PKG_VERSION"));
    let mut out = Vec::with_capacity(program.len() + app.len() + cli.len() + 1024);
    let packed = payload::write(&mut out, &program, &[("fluxcapture.exe", &app), ("fluxcapture-cli.exe", &cli)]);
    // Written beside and renamed, so a failed write never leaves a broken setup program in dist/.
    let tmp = dist.join(format!("{name}.tmp"));
    let r = packed.and_then(|()| std::fs::write(&tmp, &out)).and_then(|()| std::fs::rename(&tmp, dist.join(&name)));
    match &r {
        Ok(()) => println!("→ dist/{name} ({} MB)", out.len() >> 20),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            println!("✗ {name}: {e}");
        }
    }
    r.is_ok()
}

fn main() -> ExitCode {
    let task = std::env::args().nth(1).unwrap_or_else(|| "ci".into());
    let ok = match task.as_str() {
        "fmt" => run(&["fmt", "--all", "--check"]),
        "clippy" => run(&["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"]),
        "test" => run(&["test", "--workspace"]),
        "layers" => layers(),
        "dist" => {
            let built = run(&["build", "--release", "-p", "fluxcapture", "-p", "fluxcapture-cli", "-p", "fluxcapture-setup"]);
            let dist = root().join("dist");
            built
                && std::fs::create_dir_all(&dist).is_ok()
                && ["fluxcapture.exe", "fluxcapture-cli.exe"].iter().all(|b| {
                    let r = std::fs::copy(root().join("target/release").join(b), dist.join(b));
                    println!("{} {b}", if r.is_ok() { "→ dist/" } else { "✗ missing" });
                    r.is_ok()
                })
                && setup(&dist)
        }
        "ci" => {
            layers()
                && run(&["fmt", "--all", "--check"])
                && run(&["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"])
                && run(&["test", "--workspace"])
        }
        other => {
            eprintln!("unknown task {other:?}: ci | fmt | clippy | test | layers | dist");
            false
        }
    };
    if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}
