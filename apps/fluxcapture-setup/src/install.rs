//! Installing and uninstalling, without any UI (the window and the silent mode both use this).
//!
//! Per-user install in `%LOCALAPPDATA%\Programs\FluxCapture`: no administrator rights, no UAC
//! prompt. Only files this program knows by name are ever deleted, so a folder chosen by the user
//! can never lose anything else; recordings (in Videos) are never touched.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use fluxcapture_platform::install as os;

use crate::payload::{self, Package};

pub const APP_ID: &str = "FluxCapture";
pub const APP_NAME: &str = "FluxCapture";
pub const MAIN_EXE: &str = "fluxcapture.exe";
pub const UNINSTALLER: &str = "uninstall.exe";
/// Every file an install may contain (the uninstaller removes exactly these).
pub const FILES: [&str; 3] = [MAIN_EXE, "fluxcapture-cli.exe", UNINSTALLER];
const SHORTCUT: &str = "FluxCapture.lnk";
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Why an install or uninstall stopped. The UI turns these into sentences for the user.
#[derive(Debug, PartialEq, Eq)]
pub enum Failure {
    /// FluxCapture (or its CLI) is running from the install folder — possibly recording, so it is
    /// never closed for the user.
    InUse,
    /// The setup file is damaged or incomplete (a broken download).
    Damaged(String),
    /// Not enough space: (needed, available) bytes.
    NoSpace(u64, u64),
    /// Anything else the OS refused (permissions, a read-only folder…).
    Io(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::InUse => write!(f, "FluxCapture is running"),
            Failure::Damaged(e) | Failure::Io(e) => write!(f, "{e}"),
            Failure::NoSpace(need, have) => write!(f, "not enough disk space ({} MB needed, {} MB free)", need >> 20, have >> 20),
        }
    }
}

/// What to install and where.
#[derive(Clone, Debug)]
pub struct Plan {
    pub dir: PathBuf,
    pub desktop_shortcut: bool,
    /// Start menu shortcut and the Settings › Apps entry. Off only in tests.
    pub register: bool,
}

/// The installed version, if FluxCapture is installed for this user.
pub fn installed_version() -> Option<String> {
    os::read_uninstall_value(APP_ID, "DisplayVersion")
}

/// The folder of an existing install, else `%LOCALAPPDATA%\Programs\FluxCapture`.
pub fn default_dir() -> PathBuf {
    if let Some(dir) = os::read_uninstall_value(APP_ID, "InstallLocation").map(PathBuf::from).filter(|d| d.is_absolute()) {
        return dir;
    }
    os::user_programs_dir()
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Programs")))
        .unwrap_or_else(|| PathBuf::from(r"C:\FluxCapture"))
        .join(APP_NAME)
}

/// A folder picked by the user: installs go into a `FluxCapture` folder inside it.
pub fn dir_from_pick(picked: &Path) -> PathBuf {
    if picked.file_name().is_some_and(|n| n.eq_ignore_ascii_case(APP_NAME)) { picked.to_path_buf() } else { picked.join(APP_NAME) }
}

/// True if `path` is an executable that is running (Windows refuses to open a running image for
/// writing with a sharing violation).
fn in_use(path: &Path) -> bool {
    const ERROR_SHARING_VIOLATION: i32 = 32;
    path.exists() && fs::OpenOptions::new().write(true).open(path).is_err_and(|e| e.raw_os_error() == Some(ERROR_SHARING_VIOLATION))
}

fn io(what: &str, path: &Path, e: std::io::Error) -> Failure {
    Failure::Io(format!("{what} {}: {e}", path.display()))
}

/// Writes `dir/name` through `name.tmp` + rename, so an existing file is replaced only by a
/// complete one.
fn write_file(dir: &Path, name: &str, fill: &mut dyn FnMut(&mut BufWriter<File>) -> Result<(), Failure>) -> Result<(), Failure> {
    let tmp = dir.join(format!("{name}.tmp"));
    let dest = dir.join(name);
    let result = (|| {
        let mut out = BufWriter::with_capacity(256 * 1024, File::create(&tmp).map_err(|e| io("cannot create", &tmp, e))?);
        fill(&mut out)?;
        out.flush().map_err(|e| io("cannot write", &tmp, e))?;
        out.into_inner().map_err(|e| io("cannot write", &tmp, e.into_error()))?.sync_all().map_err(|e| io("cannot write", &tmp, e))?;
        fs::rename(&tmp, &dest).map_err(|e| if in_use(&dest) { Failure::InUse } else { io("cannot replace", &dest, e) })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Installs the files packed in `setup` (the running setup program) according to `plan`.
/// `progress` receives 0.0–1.0.
pub fn install(setup: &Path, plan: &Plan, progress: &mut dyn FnMut(f32)) -> Result<(), Failure> {
    let mut src = File::open(setup).map_err(|e| io("cannot open", setup, e))?;
    let package: Package = payload::read(&mut src).map_err(Failure::Damaged)?.ok_or_else(|| Failure::Damaged("this setup program contains no files".into()))?;
    if package.files.iter().any(|f| !FILES.contains(&f.name.as_str()) || f.name == UNINSTALLER) {
        return Err(Failure::Damaged("unexpected file in the setup program".into()));
    }
    let needed = package.total_size().saturating_add(package.program_len);
    let dir = &plan.dir;
    fs::create_dir_all(dir).map_err(|e| io("cannot create", dir, e))?;
    if FILES.iter().any(|f| in_use(&dir.join(f))) {
        return Err(Failure::InUse);
    }
    if let Ok(free) = fluxcapture_platform::system::disk_free(dir) {
        let margin = 16 << 20;
        if free < needed.saturating_add(margin) {
            return Err(Failure::NoSpace(needed.saturating_add(margin), free));
        }
    }

    let total = needed.max(1) as f32;
    let mut done = 0u64;
    for f in &package.files {
        write_file(dir, &f.name, &mut |out| payload::extract(&mut src, f, out, &mut |n| progress((done + n) as f32 / total)).map_err(Failure::Damaged))?;
        done += f.size;
    }
    // The uninstaller: this program without the files.
    write_file(dir, UNINSTALLER, &mut |out| {
        payload::copy(&mut src, 0, package.program_len, out, &mut |n| progress((done + n) as f32 / total)).map(|_| ()).map_err(|e| io("cannot read", setup, e))
    })?;
    progress(1.0);

    let exe = dir.join(MAIN_EXE);
    let desktop = os::desktop_dir().map(|d| d.join(SHORTCUT));
    if plan.desktop_shortcut {
        if let Some(link) = &desktop {
            os::create_shortcut(link, &exe, "FluxCapture").map_err(|e| Failure::Io(e.to_string()))?;
        }
    } else if plan.register
        && let Some(link) = &desktop
    {
        // Unticked on an update: the user no longer wants the shortcut.
        let _ = fs::remove_file(link);
    }
    if plan.register {
        if let Some(menu) = os::start_menu_dir() {
            os::create_shortcut(&menu.join(SHORTCUT), &exe, "FluxCapture").map_err(|e| Failure::Io(e.to_string()))?;
        }
        register(dir, needed)?;
    }
    os::refresh_shell();
    Ok(())
}

fn register(dir: &Path, size: u64) -> Result<(), Failure> {
    use os::RegValue::{Number, Text};
    let dir_s = dir.to_string_lossy();
    let exe = dir.join(MAIN_EXE).to_string_lossy().into_owned();
    let uninstaller = format!("\"{}\"", dir.join(UNINSTALLER).display());
    let quiet = format!("{uninstaller} --silent");
    let (y, m, d, ..) = fluxcapture_platform::system::local_time();
    let date = format!("{y:04}{m:02}{d:02}");
    os::write_uninstall_entry(
        APP_ID,
        &[
            ("DisplayName", Text(APP_NAME)),
            ("DisplayVersion", Text(VERSION)),
            ("Publisher", Text(APP_NAME)),
            ("DisplayIcon", Text(&exe)),
            ("InstallLocation", Text(&dir_s)),
            ("InstallDate", Text(&date)),
            ("UninstallString", Text(&uninstaller)),
            ("QuietUninstallString", Text(&quiet)),
            ("EstimatedSize", Number(u32::try_from(size >> 10).unwrap_or(u32::MAX))),
            ("NoModify", Number(1)),
            ("NoRepair", Number(1)),
        ],
    )
    .map_err(|e| Failure::Io(e.to_string()))
}

/// `%APPDATA%\FluxCapture`: settings, profiles, scenes, logs and the session journal.
pub fn settings_dir() -> Option<PathBuf> {
    fluxcapture_platform::system::config_dir().map(|d| d.join(APP_NAME))
}

/// Removes the install in `dir`. The running uninstaller (`self_exe`) cannot delete itself:
/// returns `true` when it must be deleted after the process exits ([`delete_after_exit`]).
pub fn uninstall(dir: &Path, self_exe: &Path, remove_settings: bool, register: bool) -> Result<bool, Failure> {
    if FILES.iter().filter(|f| **f != UNINSTALLER).any(|f| in_use(&dir.join(f))) {
        return Err(Failure::InUse);
    }
    if register {
        for link in [os::desktop_dir(), os::start_menu_dir()].into_iter().flatten().map(|d| d.join(SHORTCUT)) {
            let _ = fs::remove_file(link);
        }
        os::delete_uninstall_entry(APP_ID).map_err(|e| Failure::Io(e.to_string()))?;
    }
    let mut pending = false;
    for name in FILES {
        let path = dir.join(name);
        if same_file(&path, self_exe) {
            pending = true;
        } else if path.exists() {
            fs::remove_file(&path).map_err(|e| io("cannot remove", &path, e))?;
        }
    }
    if !pending {
        let _ = fs::remove_dir(dir); // only if empty: a portable `config\` stays
    }
    if remove_settings && let Some(settings) = settings_dir().filter(|d| d.is_dir()) {
        fs::remove_dir_all(&settings).map_err(|e| io("cannot remove", &settings, e))?;
    }
    os::refresh_shell();
    Ok(pending)
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Deletes the uninstaller and its (then empty) folder a few seconds after this process exits.
/// Paths go through environment variables expanded after cmd.exe parses the line, so no folder
/// name can change the command.
pub fn delete_after_exit(exe: &Path) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let dir = exe.parent().unwrap_or(Path::new(""));
        let r = std::process::Command::new("cmd.exe")
            .raw_arg(r#"/d /v:on /c ping -n 4 127.0.0.1 >nul & del /f /q "!FC_EXE!" & rmdir "!FC_DIR!""#)
            .env("FC_EXE", exe)
            .env("FC_DIR", dir)
            .current_dir(std::env::temp_dir())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
        if let Err(e) = r {
            log::info!("cannot schedule the removal of {}: {e}", exe.display());
        }
    }
    #[cfg(not(windows))]
    let _ = exe;
}

/// Starts the installed FluxCapture.
pub fn launch(dir: &Path) -> Result<(), String> {
    std::process::Command::new(dir.join(MAIN_EXE)).current_dir(dir).spawn().map(|_| ()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fluxcapture-setup-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn package(dir: &Path, files: &[(&str, &[u8])]) -> PathBuf {
        let path = dir.join("setup.exe");
        let mut out = Vec::new();
        payload::write(&mut out, b"MZ setup program", files).unwrap();
        fs::write(&path, out).unwrap();
        path
    }

    #[test]
    fn install_update_uninstall() {
        let root = temp("cycle");
        let setup = package(&root, &[(MAIN_EXE, b"app v1"), ("fluxcapture-cli.exe", b"cli v1")]);
        let plan = Plan { dir: root.join("FluxCapture"), desktop_shortcut: false, register: false };
        let mut last = 0.0;
        install(&setup, &plan, &mut |p| last = p).unwrap();
        assert_eq!(last, 1.0);
        assert_eq!(fs::read(plan.dir.join(MAIN_EXE)).unwrap(), b"app v1");
        assert_eq!(fs::read(plan.dir.join(UNINSTALLER)).unwrap(), b"MZ setup program");

        // Update over an existing install; a user file in the folder survives everything.
        fs::write(plan.dir.join("portable.txt"), b"").unwrap();
        let setup = package(&root, &[(MAIN_EXE, b"app v2"), ("fluxcapture-cli.exe", b"cli v2")]);
        install(&setup, &plan, &mut |_| {}).unwrap();
        assert_eq!(fs::read(plan.dir.join(MAIN_EXE)).unwrap(), b"app v2");
        assert!(fs::read_dir(&plan.dir).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".tmp")));

        let pending = uninstall(&plan.dir, &setup, false, false).unwrap();
        assert!(!pending);
        let left: Vec<_> = fs::read_dir(&plan.dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left, ["portable.txt"]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn running_uninstaller_is_left_for_later() {
        let root = temp("self");
        let setup = package(&root, &[(MAIN_EXE, b"app")]);
        let plan = Plan { dir: root.join("FluxCapture"), desktop_shortcut: false, register: false };
        install(&setup, &plan, &mut |_| {}).unwrap();
        assert!(uninstall(&plan.dir, &plan.dir.join(UNINSTALLER), false, false).unwrap());
        assert!(!plan.dir.join(MAIN_EXE).exists());
        assert!(plan.dir.join(UNINSTALLER).exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn bad_packages_are_refused() {
        let root = temp("bad");
        let plan = Plan { dir: root.join("FluxCapture"), desktop_shortcut: false, register: false };
        // No files at all (the uninstaller run as a setup program).
        let plain = root.join("plain.exe");
        fs::write(&plain, b"MZ program").unwrap();
        assert!(matches!(install(&plain, &plan, &mut |_| {}), Err(Failure::Damaged(_))));
        // A file the uninstaller would not know about.
        let odd = package(&root, &[("other.dll", b"x")]);
        assert!(matches!(install(&odd, &plan, &mut |_| {}), Err(Failure::Damaged(_))));
        // A damaged file: nothing replaces the installed copy.
        let setup = package(&root, &[(MAIN_EXE, b"good")]);
        install(&setup, &plan, &mut |_| {}).unwrap();
        let mut bytes = fs::read(&setup).unwrap();
        bytes[17] ^= 0xFF; // inside the file's bytes
        fs::write(&setup, bytes).unwrap();
        assert!(matches!(install(&setup, &plan, &mut |_| {}), Err(Failure::Damaged(_))));
        assert_eq!(fs::read(plan.dir.join(MAIN_EXE)).unwrap(), b"good");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn picked_folders() {
        assert_eq!(dir_from_pick(Path::new(r"D:\Apps")), Path::new(r"D:\Apps\FluxCapture"));
        assert_eq!(dir_from_pick(Path::new(r"D:\Apps\fluxcapture")), Path::new(r"D:\Apps\fluxcapture"));
    }

    #[cfg(windows)]
    #[test]
    fn running_program_is_detected() {
        // This test binary is running: Windows refuses to open it for writing.
        assert!(in_use(&std::env::current_exe().unwrap()));
        assert!(!in_use(Path::new(r"C:\does\not\exist.exe")));
    }
}
