//! The log backend: rotating file + in-memory ring, with secret redaction.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use log::{Level, LevelFilter, Log, Metadata, Record};

/// Bytes per log file before rotating.
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// Rotated files kept (fluxcapture.1.log … fluxcapture.N.log).
const KEEP_FILES: usize = 4;
/// Lines kept in memory for the UI.
const RING_LINES: usize = 1000;

/// One stored log line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub level: Level,
    pub text: String,
}

struct Inner {
    file: Option<File>,
    path: Option<PathBuf>,
    written: u64,
    ring: VecDeque<Line>,
}

/// Handle to the installed logger (read the ring, export).
#[derive(Clone)]
pub struct LogHandle {
    inner: Arc<Mutex<Inner>>,
}

struct Logger {
    inner: Arc<Mutex<Inner>>,
    level: LevelFilter,
}

static HANDLE: OnceLock<LogHandle> = OnceLock::new();

/// Local wall-clock (hour, minute, second) provider; set by the application (the OS layer knows the
/// time zone). Without one, log times are UTC.
static LOCAL_TIME: OnceLock<fn() -> (u32, u32, u32)> = OnceLock::new();

/// Makes log timestamps local time.
pub fn set_local_time(f: fn() -> (u32, u32, u32)) {
    let _ = LOCAL_TIME.set(f);
}

/// Installs the logger (once per process; later calls return the first handle). `dir` gets
/// `fluxcapture.log`; `None` logs to memory only.
pub fn init(dir: Option<&Path>, verbose: bool) -> LogHandle {
    HANDLE
        .get_or_init(|| {
            let mut inner = Inner { file: None, path: None, written: 0, ring: VecDeque::with_capacity(RING_LINES) };
            if let Some(dir) = dir
                && std::fs::create_dir_all(dir).is_ok()
            {
                let path = dir.join("fluxcapture.log");
                rotate(&path);
                inner.file = OpenOptions::new().create(true).append(true).open(&path).ok();
                inner.path = Some(path);
            }
            let inner = Arc::new(Mutex::new(inner));
            let level = if verbose { LevelFilter::Debug } else { LevelFilter::Info };
            let logger = Logger { inner: Arc::clone(&inner), level };
            if log::set_boxed_logger(Box::new(logger)).is_ok() {
                log::set_max_level(level);
            }
            LogHandle { inner }
        })
        .clone()
}

fn rotated(path: &Path, n: usize) -> PathBuf {
    path.with_file_name(format!("fluxcapture.{n}.log"))
}

fn rotate(path: &Path) {
    let _ = std::fs::remove_file(rotated(path, KEEP_FILES));
    for n in (1..KEEP_FILES).rev() {
        let _ = std::fs::rename(rotated(path, n), rotated(path, n + 1));
    }
    let _ = std::fs::rename(path, rotated(path, 1));
}

fn timestamp() -> String {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let s = d.as_secs();
    let (h, m, sec) = match LOCAL_TIME.get() {
        Some(f) => f(),
        None => (((s / 3600) % 24) as u32, ((s / 60) % 60) as u32, (s % 60) as u32),
    };
    format!("{h:02}:{m:02}:{sec:02}.{:03}", d.subsec_millis())
}

impl Log for Logger {
    fn enabled(&self, m: &Metadata<'_>) -> bool {
        m.level() <= self.level && (m.level() <= Level::Info || m.target().starts_with("fluxcapture"))
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let text = redact(&format!("{} {:<5} [{}] {}", timestamp(), record.level(), record.target(), record.args()));
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if inner.ring.len() == RING_LINES {
            inner.ring.pop_front();
        }
        inner.ring.push_back(Line { level: record.level(), text: text.clone() });
        if inner.written > MAX_FILE_BYTES
            && let Some(path) = inner.path.clone()
        {
            inner.file = None;
            rotate(&path);
            inner.file = OpenOptions::new().create(true).append(true).open(&path).ok();
            inner.written = 0;
        }
        if let Some(f) = inner.file.as_mut()
            && writeln!(f, "{text}").is_ok()
        {
            inner.written += text.len() as u64 + 1;
        }
    }

    fn flush(&self) {
        if let Some(f) = self.inner.lock().unwrap_or_else(PoisonError::into_inner).file.as_mut() {
            let _ = f.flush();
        }
    }
}

impl LogHandle {
    /// The last lines (oldest first).
    pub fn lines(&self) -> Vec<Line> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner).ring.iter().cloned().collect()
    }

    pub fn path(&self) -> Option<PathBuf> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner).path.clone()
    }

    /// Writes a shareable report: `header` (system information) followed by the current log file
    /// and the in-memory lines. Everything is already redacted.
    pub fn export(&self, dest: &Path, header: &str) -> std::io::Result<()> {
        let (path, ring) = {
            let inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            (inner.path.clone(), inner.ring.iter().map(|l| l.text.clone()).collect::<Vec<_>>())
        };
        let mut out = File::create(dest)?;
        writeln!(out, "{}", redact(header))?;
        writeln!(out, "\n==== log ====")?;
        match path.and_then(|p| std::fs::read_to_string(p).ok()) {
            Some(text) => out.write_all(redact(&text).as_bytes())?,
            None => {
                for l in ring {
                    writeln!(out, "{l}")?;
                }
            }
        }
        Ok(())
    }
}

/// Masks secrets: values after `key=`, `token=`, `password=`, `secret=`, `streamkey=`, Twitch/YouTube
/// style stream keys (`live_…`) and the last path segment of `rtmp(s)://` URLs.
pub fn redact(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let lower = s.to_ascii_lowercase();
    let bytes = s.as_bytes();
    let mut i = 0usize;
    let keys = ["key=", "token=", "password=", "secret=", "streamkey=", "stream_key=", "pwd="];
    while i < bytes.len() {
        let rest = lower.get(i..).unwrap_or("");
        if let Some(k) = keys.iter().find(|k| rest.starts_with(**k)) {
            out.push_str(s.get(i..i + k.len()).unwrap_or(""));
            i += k.len();
            let end = s.get(i..).and_then(|r| r.find(|c: char| c.is_whitespace() || c == '&' || c == '"' || c == '\'')).map_or(s.len(), |e| i + e);
            if end > i {
                out.push_str("***");
            }
            i = end;
            continue;
        }
        if rest.starts_with("live_") && (i == 0 || !bytes.get(i - 1).is_some_and(|b| b.is_ascii_alphanumeric())) {
            let end = s.get(i..).and_then(|r| r.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))).map_or(s.len(), |e| i + e);
            if end - i > 12 {
                out.push_str("live_***");
                i = end;
                continue;
            }
        }
        if rest.starts_with("rtmp://") || rest.starts_with("rtmps://") {
            let end = s.get(i..).and_then(|r| r.find(char::is_whitespace)).map_or(s.len(), |e| i + e);
            let url = s.get(i..end).unwrap_or("");
            match url.rfind('/') {
                Some(slash) if slash + 1 < url.len() && url.get(..slash).is_some_and(|p| p.matches('/').count() >= 3) => {
                    out.push_str(url.get(..=slash).unwrap_or(""));
                    out.push_str("***");
                }
                _ => out.push_str(url),
            }
            i = end;
            continue;
        }
        let ch = s.get(i..).and_then(|r| r.chars().next()).unwrap_or('?');
        out.push(ch);
        i += ch.len_utf8().max(1);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_secrets() {
        assert_eq!(redact("connect key=abc123 ok"), "connect key=*** ok");
        assert_eq!(redact("url rtmp://live.twitch.tv/app/live_123456789_abcdefghijklmn"), "url rtmp://live.twitch.tv/app/***");
        assert_eq!(redact("using live_123456789_abcdefghijklmnop now"), "using live_*** now");
        assert_eq!(redact("Password=hunter2&x=1"), "Password=***&x=1");
        assert_eq!(redact("olá — nothing secret"), "olá — nothing secret");
        assert_eq!(redact("key="), "key=");
    }

    #[test]
    fn logger_ring() {
        let h = init(None, false);
        log::info!(target: "fluxcapture_test", "hello token=SECRET");
        let lines = h.lines();
        assert!(lines.iter().any(|l| l.text.contains("hello token=***")));
        assert!(!lines.iter().any(|l| l.text.contains("SECRET")));
    }
}
