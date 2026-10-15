//! `FluxCapture-Setup.exe` — installs FluxCapture for the current user, and uninstalls it.
//!
//! `cargo xtask dist` builds this program and appends the files to install to it (see
//! [`payload`]). Installed, the same program without the files is `uninstall.exe`, which Settings ›
//! Apps runs to remove FluxCapture.
//!
//! ```text
//! FluxCapture-Setup.exe [--silent] [--dir DIR] [--no-desktop-shortcut]
//! uninstall.exe         [--silent] [--remove-settings]
//! ```
//!
//! Without `--silent` a small window walks the user through it; with it, the exit code says
//! whether it worked (0) or not (1).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod install;
mod payload;
mod ui;

#[allow(dead_code)]
#[path = "../../../crates/ui/src/app_icon.rs"]
mod app_icon;

use std::path::PathBuf;
use std::process::ExitCode;

use install::{Plan, UNINSTALLER};

/// What this run does.
#[derive(Clone)]
pub enum Mode {
    /// Install the files packed into this program.
    Install,
    /// Remove the install in this folder.
    Uninstall(PathBuf),
    /// A setup program without files that is not an installed uninstaller (e.g. the bare build
    /// output): nothing to do but say so.
    Empty,
}

fn mode(exe: &std::path::Path, uninstall_flag: bool) -> Mode {
    let has_files = std::fs::File::open(exe).ok().and_then(|mut f| payload::read(&mut f).ok()).flatten().is_some();
    let named_uninstaller = exe.file_name().is_some_and(|n| n.eq_ignore_ascii_case(UNINSTALLER));
    match exe.parent() {
        // The setup program asked to uninstall: the existing install, wherever it is.
        _ if uninstall_flag && has_files => Mode::Uninstall(install::default_dir()),
        Some(dir) if uninstall_flag || (!has_files && named_uninstaller) => Mode::Uninstall(dir.to_path_buf()),
        _ if has_files => Mode::Install,
        _ => Mode::Empty,
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |f: &str| args.iter().any(|a| a.eq_ignore_ascii_case(f));
    let value = |f: &str| args.iter().position(|a| a.eq_ignore_ascii_case(f)).and_then(|i| args.get(i + 1)).map(PathBuf::from);

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => return ui::fatal(&format!("cannot find the setup program: {e}")),
    };
    let mode = mode(&exe, flag("--uninstall"));
    if !flag("--silent") {
        return ui::run(exe, mode);
    }
    let ok = match mode {
        Mode::Install => {
            let dir = value("--dir").filter(|d| d.is_absolute()).unwrap_or_else(install::default_dir);
            let plan = Plan { dir, desktop_shortcut: !flag("--no-desktop-shortcut"), register: true };
            install::install(&exe, &plan, &mut |_| {}).is_ok()
        }
        Mode::Uninstall(dir) => match install::uninstall(&dir, &exe, flag("--remove-settings"), true) {
            Ok(pending) => {
                if pending {
                    install::delete_after_exit(&exe);
                }
                true
            }
            Err(_) => false,
        },
        Mode::Empty => false,
    };
    if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}
