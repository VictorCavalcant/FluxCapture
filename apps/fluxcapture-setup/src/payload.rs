//! The files a setup program installs, appended to the setup executable itself.
//!
//! ```text
//! [setup program][file 0]…[file n-1][index][footer]
//! index:  per file: name length (u16 LE), name (UTF-8), size (u64 LE), FNV-1a 64 of the bytes (u64 LE)
//! footer: MAGIC (16 bytes) | program length (u64 LE) | index length (u64 LE)
//! ```
//!
//! Windows ignores bytes after the last section of an executable, so the result still runs. The
//! program part alone (the first `program_len` bytes) is a setup program without files: installed
//! as the uninstaller. Plain `std` on purpose: `xtask` includes this file to write packages.
//!
//! Everything read here comes from a download and is hostile (AGENTS.md §0.4): sizes are checked
//! against the file length, names are restricted to plain file names, every file is verified.

use std::io::{self, Read, Seek, SeekFrom, Write};

pub const MAGIC: &[u8; 16] = b"FLUXCAPTURE-PKG1";
const FOOTER_LEN: u64 = 32;
/// More than any package has: a sanity limit for the index.
pub const MAX_FILES: usize = 16;
const MAX_INDEX_LEN: u64 = 64 * 1024;
const FNV_START: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a 64 of `bytes`, continuing from `hash`.
pub fn fnv1a(mut hash: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        hash = (hash ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash
}

/// A plain file name: 1–64 ASCII letters, digits, `.`, `-`, `_`, not starting with a dot (no
/// folders, no `..`, nothing Windows treats specially).
pub fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len()) && !name.starts_with('.') && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

/// One file of a package.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    /// Offset of the file's bytes from the start of the package.
    pub offset: u64,
    pub size: u64,
    pub hash: u64,
}

/// The index of a package.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Package {
    /// Length of the setup program in front of the files.
    pub program_len: u64,
    pub files: Vec<Entry>,
}

impl Package {
    pub fn total_size(&self) -> u64 {
        self.files.iter().fold(0u64, |a, f| a.saturating_add(f.size))
    }
}

/// Writes a package: `program` followed by `files` (name, bytes), the index and the footer.
#[allow(dead_code)] // used by xtask, which packs the setup program
pub fn write(out: &mut impl Write, program: &[u8], files: &[(&str, &[u8])]) -> io::Result<()> {
    let invalid = |m: &str| io::Error::new(io::ErrorKind::InvalidInput, m.to_string());
    if files.len() > MAX_FILES {
        return Err(invalid("too many files"));
    }
    out.write_all(program)?;
    let mut index = Vec::new();
    for (name, bytes) in files {
        if !valid_name(name) {
            return Err(invalid(&format!("invalid file name {name:?}")));
        }
        out.write_all(bytes)?;
        index.extend_from_slice(&(name.len() as u16).to_le_bytes());
        index.extend_from_slice(name.as_bytes());
        index.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        index.extend_from_slice(&fnv1a(FNV_START, bytes).to_le_bytes());
    }
    out.write_all(&index)?;
    out.write_all(MAGIC)?;
    out.write_all(&(program.len() as u64).to_le_bytes())?;
    out.write_all(&(index.len() as u64).to_le_bytes())?;
    Ok(())
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    b.get(at..at.checked_add(8)?)?.try_into().ok().map(u64::from_le_bytes)
}

/// Reads a package index. `Ok(None)`: no package at all (a setup program without files, i.e. the
/// uninstaller); `Err`: a damaged package.
pub fn read(r: &mut (impl Read + Seek)) -> Result<Option<Package>, String> {
    let io = |e: io::Error| format!("cannot read the setup file: {e}");
    let len = r.seek(SeekFrom::End(0)).map_err(io)?;
    if len < FOOTER_LEN {
        return Ok(None);
    }
    let mut footer = [0u8; FOOTER_LEN as usize];
    r.seek(SeekFrom::Start(len - FOOTER_LEN)).map_err(io)?;
    r.read_exact(&mut footer).map_err(io)?;
    if footer.get(..16) != Some(&MAGIC[..]) {
        return Ok(None);
    }
    let corrupt = |what: &str| format!("damaged setup file ({what})");
    let program_len = u64_at(&footer, 16).ok_or_else(|| corrupt("footer"))?;
    let index_len = u64_at(&footer, 24).ok_or_else(|| corrupt("footer"))?;
    if index_len > MAX_INDEX_LEN {
        return Err(corrupt("index size"));
    }
    let index_at = (len - FOOTER_LEN).checked_sub(index_len).ok_or_else(|| corrupt("index size"))?;
    if program_len > index_at {
        return Err(corrupt("program size"));
    }
    let mut index = vec![0u8; index_len as usize];
    r.seek(SeekFrom::Start(index_at)).map_err(io)?;
    r.read_exact(&mut index).map_err(io)?;

    let mut files = Vec::new();
    let mut at = 0usize;
    let mut offset = program_len;
    while at < index.len() {
        if files.len() >= MAX_FILES {
            return Err(corrupt("too many files"));
        }
        let name_len = index.get(at..at + 2).and_then(|b| b.try_into().ok()).map(u16::from_le_bytes).ok_or_else(|| corrupt("index"))? as usize;
        at += 2;
        let name = index.get(at..at + name_len).and_then(|b| std::str::from_utf8(b).ok()).ok_or_else(|| corrupt("file name"))?;
        if !valid_name(name) {
            return Err(corrupt("file name"));
        }
        at += name_len;
        let size = u64_at(&index, at).ok_or_else(|| corrupt("index"))?;
        let hash = u64_at(&index, at + 8).ok_or_else(|| corrupt("index"))?;
        at += 16;
        if files.iter().any(|f: &Entry| f.name.eq_ignore_ascii_case(name)) {
            return Err(corrupt("duplicate file"));
        }
        files.push(Entry { name: name.to_string(), offset, size, hash });
        offset = offset.checked_add(size).filter(|&end| end <= index_at).ok_or_else(|| corrupt("file size"))?;
    }
    if offset != index_at {
        return Err(corrupt("file sizes"));
    }
    Ok(Some(Package { program_len, files }))
}

/// Copies `n` bytes from `at` in `r` to `out` (64 KiB at a time), calling `progress` with the bytes
/// copied so far; returns their FNV-1a hash.
pub fn copy(r: &mut (impl Read + Seek), at: u64, n: u64, out: &mut impl Write, progress: &mut dyn FnMut(u64)) -> io::Result<u64> {
    r.seek(SeekFrom::Start(at))?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut hash = FNV_START;
    let mut done = 0u64;
    while done < n {
        let want = usize::try_from((n - done).min(buf.len() as u64)).unwrap_or(buf.len());
        let chunk = buf.get_mut(..want).ok_or_else(|| io::Error::other("buffer"))?;
        r.read_exact(chunk)?;
        hash = fnv1a(hash, chunk);
        out.write_all(chunk)?;
        done += want as u64;
        progress(done);
    }
    Ok(hash)
}

/// Extracts one file, verifying its hash.
pub fn extract(r: &mut (impl Read + Seek), f: &Entry, out: &mut impl Write, progress: &mut dyn FnMut(u64)) -> Result<(), String> {
    let hash = copy(r, f.offset, f.size, out, progress).map_err(|e| format!("{}: {e}", f.name))?;
    if hash == f.hash { Ok(()) } else { Err(format!("damaged setup file ({} does not match its checksum)", f.name)) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn sample() -> Vec<u8> {
        let mut out = Vec::new();
        write(&mut out, b"MZprogram", &[("a.exe", b"first file"), ("b-cli.exe", &[7u8; 100_000])]).unwrap();
        out
    }

    #[test]
    fn round_trip() {
        let bytes = sample();
        let p = read(&mut Cursor::new(&bytes)).unwrap().unwrap();
        assert_eq!(p.program_len, 9);
        assert_eq!(p.files.len(), 2);
        assert_eq!(p.total_size(), 100_010);
        for (f, want) in p.files.iter().zip([b"first file".to_vec(), vec![7u8; 100_000]]) {
            let mut out = Vec::new();
            let mut last = 0;
            extract(&mut Cursor::new(&bytes), f, &mut out, &mut |n| last = n).unwrap();
            assert_eq!(out, want);
            assert_eq!(last, want.len() as u64);
        }
        // The program alone is not a package.
        assert_eq!(read(&mut Cursor::new(&bytes[..9])).unwrap(), None);
        assert_eq!(read(&mut Cursor::new(Vec::new())).unwrap(), None);
    }

    #[test]
    fn refuses_bad_names() {
        for bad in ["", "..", ".hidden", "a/b.exe", r"a\b.exe", "C:x", "ç.exe", &"x".repeat(65)] {
            assert!(!valid_name(bad), "{bad:?}");
            assert!(write(&mut Vec::new(), b"", &[(bad, b"x")]).is_err());
        }
        assert!(valid_name("fluxcapture-cli.exe"));
    }

    /// AGENTS.md §0.8: a damaged download never panics and never extracts wrong bytes silently.
    #[test]
    fn damaged_packages_fail_cleanly() {
        let good = sample();
        let check = |bytes: Vec<u8>| {
            std::panic::catch_unwind(|| {
                if let Ok(Some(p)) = read(&mut Cursor::new(&bytes)) {
                    for f in &p.files {
                        let _ = extract(&mut Cursor::new(&bytes), f, &mut Vec::new(), &mut |_| {});
                    }
                }
            })
            .is_ok()
        };
        for cut in 0..good.len() {
            assert!(check(good[..cut].to_vec()), "truncated at {cut}");
        }
        let tail = good.len() - 300;
        for i in tail..good.len() {
            for bit in 0..8 {
                let mut b = good.clone();
                b[i] ^= 1 << bit;
                assert!(check(b), "bit {bit} of byte {i}");
            }
        }
        // A flipped bit in a file's bytes is caught by the checksum.
        let mut b = good.clone();
        b[20] ^= 1;
        let p = read(&mut Cursor::new(&b)).unwrap().unwrap();
        assert!(extract(&mut Cursor::new(&b), &p.files[1], &mut Vec::new(), &mut |_| {}).is_err());
        // Hostile sizes.
        let mut b = good.clone();
        let n = b.len();
        b[n - 8..].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(read(&mut Cursor::new(&b)).is_err());
        let mut b = good;
        b[n - 16..n - 8].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(read(&mut Cursor::new(&b)).is_err());
    }
}
