//! Finding ROM candidates in a folder (zips included) and hashing them.
//! Nothing here downloads anything; it only looks at files you already have.

use std::path::{Path, PathBuf};

use super::{
    hash::{HashCache, Hashes, Stamp, hash_reader},
    is_junk, is_sidecar, open_zip, size_of, visible_files, with_zip_entry,
};
use crate::{Error, Result};

/// Where a ROM's bytes are: a plain file, or one entry inside a zip. Paths
/// are relative or absolute depending on who built the reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRef {
    /// The file, or the zip holding it.
    pub path: PathBuf,
    /// Entry name inside the zip, if any.
    pub inner: Option<String>,
}

impl FileRef {
    pub fn plain(path: impl Into<PathBuf>) -> Self {
        FileRef {
            path: path.into(),
            inner: None,
        }
    }

    /// The name a DAT would know this file by.
    pub fn display_name(&self) -> String {
        match &self.inner {
            Some(inner) => inner.rsplit('/').next().unwrap_or(inner).to_string(),
            None => self
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        }
    }

    /// `path` or `path::inner`, for messages and skip reasons.
    pub fn location(&self) -> String {
        match &self.inner {
            Some(inner) => format!("{}::{inner}", self.path.display()),
            None => self.path.display().to_string(),
        }
    }

    /// The same reference with `path` resolved under `dir`.
    pub fn under(&self, dir: &Path) -> FileRef {
        FileRef {
            path: dir.join(&self.path),
            inner: self.inner.clone(),
        }
    }
}

/// A file that might be a ROM: on disk, or inside a zip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// Relative to the scanned folder (for zip entries, the zip's path).
    pub file: FileRef,
    pub size: u64,
}

/// Lists ROM candidates under `dir`, recursively. Hidden files, macOS junk
/// and DAT/text files are skipped; zips are looked inside (not extracted).
pub fn scan(dir: &Path) -> Result<Vec<Candidate>> {
    if !dir.is_dir() {
        return Err(Error::NotADirectory {
            path: dir.to_path_buf(),
        });
    }
    let mut out = Vec::new();
    for full in visible_files(dir) {
        let rel = full.strip_prefix(dir).unwrap_or(&full).to_path_buf();
        if is_junk(&rel) || is_sidecar(&rel) {
            continue;
        }
        out.extend(candidates_in(&full, rel));
    }
    out.sort_by(|a, b| {
        a.file
            .path
            .cmp(&b.file.path)
            .then_with(|| a.file.inner.cmp(&b.file.inner))
    });
    Ok(out)
}

/// The candidates one file contributes: its zip entries, or itself.
fn candidates_in(full: &Path, rel: PathBuf) -> Vec<Candidate> {
    let size = size_of(full);
    if rel
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("zip"))
        && let Ok(entries) = zip_entries(full)
    {
        // An unreadable zip falls through and is treated as an opaque file.
        return entries
            .into_iter()
            .map(|(inner, size)| Candidate {
                file: FileRef {
                    path: rel.clone(),
                    inner: Some(inner),
                },
                size,
            })
            .collect();
    }
    vec![Candidate {
        file: FileRef::plain(rel),
        size,
    }]
}

/// A folder (scanned recursively) or a single ROM / zip: the folder the
/// candidates are relative to, and the candidates.
pub fn scan_source(source: &Path) -> Result<(PathBuf, Vec<Candidate>)> {
    if source.is_file() {
        let dir = source.parent().unwrap_or(Path::new(".")).to_path_buf();
        let rel = PathBuf::from(source.file_name().unwrap_or_default());
        return Ok((dir, candidates_in(source, rel)));
    }
    Ok((source.to_path_buf(), scan(source)?))
}

fn zip_entries(path: &Path) -> Result<Vec<(String, u64)>> {
    let mut archive = open_zip(path)?;
    let mut out = Vec::new();
    for i in 0..archive.len() {
        let f = archive
            .by_index(i)
            .map_err(|e| Error::zip(e, path.display().to_string()))?;
        if f.is_dir() {
            continue;
        }
        if let Ok(name) = f.name() {
            let name = name.into_owned();
            if is_junk(Path::new(&name)) {
                continue;
            }
            out.push((name, f.size()));
        }
    }
    Ok(out)
}

/// Hashes a candidate (plain file or zip entry) under `dir`, through the cache.
pub fn hash_candidate(dir: &Path, c: &Candidate, cache: &mut HashCache) -> Result<Hashes> {
    hash_ref(&c.file.under(dir), cache)
}

/// Hashes a file or zip entry at an absolute location, through the cache.
/// Zip entries are stamped with the zip's size and mtime.
fn hash_ref(file: &FileRef, cache: &mut HashCache) -> Result<Hashes> {
    let full = &file.path;
    match &file.inner {
        None => cache.file(full),
        Some(inner) => {
            let stamp = Stamp::of(full)?;
            let key = format!("{}::{inner}", full.to_string_lossy());
            cache.cached(&key, stamp, || {
                with_zip_entry(full, inner, |entry| {
                    hash_reader(entry).map_err(|e| Error::io(full, e))
                })
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{build_zip, write};
    use std::fs;

    fn library() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let w = |rel: &str, body: &[u8]| write(tmp.path(), rel, body);
        w("Hello World (USA).fkh", b"hello world"); // matched
        w(
            "sub/quickfox.fkh",
            b"The quick brown fox jumps over the lazy dog",
        ); // misnamed
        w("random.fkh", b"not in dat"); // unknown
        w(
            "discs.zip",
            &build_zip(&[("Two Discs (Japan) (Disc 1).bin", b"abc")]),
        ); // matched, zipped
        w("hello copy.fkh", b"hello world"); // misnamed duplicate
        w(".DS_Store", b"x");
        w("notes.txt", b"x");
        w("Hello World (USA)._fkh", b"x"); // not junk: ._ only applies to file name start
        tmp
    }

    #[test]
    fn scan_skips_junk_and_expands_zips() {
        let lib = library();
        let c = scan(lib.path()).unwrap();
        let names: Vec<String> = c.iter().map(|c| c.file.location()).collect();
        assert_eq!(
            names,
            [
                "Hello World (USA)._fkh",
                "Hello World (USA).fkh",
                "discs.zip::Two Discs (Japan) (Disc 1).bin",
                "hello copy.fkh",
                "random.fkh",
                "sub/quickfox.fkh",
            ]
        );
        assert_eq!(c[2].file.display_name(), "Two Discs (Japan) (Disc 1).bin");
        assert_eq!(c[5].file.display_name(), "quickfox.fkh");
    }

    #[test]
    fn scan_rejects_non_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("file");
        fs::write(&f, "x").unwrap();
        assert_eq!(scan(&f).unwrap_err().code(), "NOT_A_DIRECTORY");
    }
}
