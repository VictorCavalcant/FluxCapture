//! Session journal: a small JSON file per running recording. A file left behind after a crash
//! means the recording was interrupted; the next start offers to recover it.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub started_unix: u64,
    pub files: Vec<PathBuf>,
    pub app_version: String,
}

pub struct Journal {
    path: PathBuf,
    entry: JournalEntry,
}

impl Journal {
    pub fn create(dir: &Path, files: &[PathBuf]) -> Result<Journal, String> {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let started_unix = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let path = dir.join(format!("session-{started_unix}-{}.json", std::process::id()));
        let j = Journal { path, entry: JournalEntry { started_unix, files: files.to_vec(), app_version: env!("CARGO_PKG_VERSION").into() } };
        j.write()?;
        Ok(j)
    }

    fn write(&self) -> Result<(), String> {
        let text = serde_json::to_string_pretty(&self.entry).map_err(|e| e.to_string())?;
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &self.path).map_err(|e| e.to_string())
    }

    pub fn update(&mut self, files: &[PathBuf]) -> Result<(), String> {
        self.entry.files = files.to_vec();
        self.write()
    }

    /// The recording finished cleanly.
    pub fn complete(self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Interrupted sessions whose files still exist: (journal file, entry).
pub fn pending(dir: &Path) -> Vec<(PathBuf, JournalEntry)> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out = Vec::new();
    for e in rd.flatten().take(1000) {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&p) else { continue };
        if text.len() > 1_000_000 {
            continue;
        }
        match serde_json::from_str::<JournalEntry>(&text) {
            Ok(entry) if entry.files.iter().any(|f| f.exists()) => out.push((p, entry)),
            _ => {
                // Unreadable, or nothing left to recover: discard.
                let _ = std::fs::remove_file(&p);
            }
        }
    }
    out.sort_by_key(|(_, e)| e.started_unix);
    out
}

/// Forgets an interrupted session (after recovery, or when the user declines).
pub fn dismiss(journal_file: &Path) {
    let _ = std::fs::remove_file(journal_file);
}

/// `name (recuperado).mkv` next to the original.
pub fn recovered_path(src: &Path) -> PathBuf {
    let stem = src.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "gravação".into());
    let dir = src.parent().unwrap_or(Path::new("."));
    let mut p = dir.join(format!("{stem} (recuperado).mkv"));
    let mut n = 2;
    while p.exists() && n < 1000 {
        p = dir.join(format!("{stem} (recuperado {n}).mkv"));
        n += 1;
    }
    p
}
