//! ROM files on the card: what is in `Assets/<platform>/<common|core id>/`
//! and `GB Studio/` (the scan), and the disposable identity cache that
//! remembers their hashes between runs (the index). Listing reads metadata
//! only; verification is explicit so callers can schedule one file at a time
//! without blocking a UI.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

use crate::fsutil::hash;
use crate::{
    Error, Pocket, Result,
    card::CardIdentity,
    card::bios,
    fsutil::{self, Stamp},
};

// ---------------------------------------------------------------------------
// Scan

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RomEntry {
    /// Path relative to the owner folder, including any nested directories.
    pub name: String,
    pub size: u64,
}

/// One folder under a platform: `common` or a core's own folder.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RomDir {
    /// `common` or `<author>.<core>`.
    pub owner: String,
    /// Relative to the SD root.
    pub path: PathBuf,
    /// Files recursively, sorted by relative path (case-insensitive). Hidden files skipped.
    pub files: Vec<RomEntry>,
    pub bytes: u64,
    /// Number of visible descendant directories.
    pub subdirs: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PlatformRoms {
    pub platform: String,
    pub dirs: Vec<RomDir>,
    pub files: usize,
    pub bytes: u64,
}

/// Lists one directory as a [`RomDir`]; `None` if it doesn't exist.
fn scan_dir(pocket: &Pocket, rel: &Path, owner: &str, recursive: bool) -> Result<Option<RomDir>> {
    let full = pocket.root().join(rel);
    match std::fs::symlink_metadata(&full) {
        Ok(meta) if meta.is_symlink() => return Ok(None),
        Ok(meta) if !meta.is_dir() => return Err(Error::NotADirectory { path: full }),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io(&full, e)),
    }
    let mut files = Vec::new();
    let mut subdirs = 0;
    for entry in walkdir::WalkDir::new(&full)
        .min_depth(1)
        .max_depth(if recursive { usize::MAX } else { 1 })
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !fsutil::is_hidden(e.file_name()))
    {
        let entry = entry.map_err(|e| {
            Error::io(
                e.path().unwrap_or(&full),
                std::io::Error::other(e.to_string()),
            )
        })?;
        if entry.file_type().is_dir() {
            subdirs += 1;
        } else if entry.file_type().is_file() {
            let meta = entry
                .metadata()
                .map_err(|e| Error::io(entry.path(), std::io::Error::other(e.to_string())))?;
            files.push(RomEntry {
                name: entry
                    .path()
                    .strip_prefix(&full)
                    .expect("descendant")
                    .to_string_lossy()
                    .into_owned(),
                size: meta.len(),
            });
        }
    }
    files.sort_by_key(|f| f.name.to_lowercase());
    Ok(Some(RomDir {
        owner: owner.to_string(),
        path: rel.to_path_buf(),
        bytes: files.iter().map(|f| f.size).sum(),
        files,
        subdirs,
    }))
}

/// The `common` and per-core folders of one platform (empty if none exist).
pub fn platform_dirs(pocket: &Pocket, platform: &str) -> Result<Vec<RomDir>> {
    let base = pocket.assets_dir().join(platform);
    let entries = match std::fs::read_dir(&base) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::io(&base, e)),
    };
    let mut owners = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| Error::io(&base, e))?;
        if entry
            .file_type()
            .map_err(|e| Error::io(entry.path(), e))?
            .is_dir()
            && !fsutil::is_hidden(&entry.file_name())
        {
            owners.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    // `common` first, then cores alphabetically.
    owners.sort_by_key(|o| (o != "common", o.to_lowercase()));
    let mut dirs = Vec::new();
    for owner in owners {
        if let Some(dir) = scan_dir(
            pocket,
            &crate::card::assets_rel(platform, &owner),
            &owner,
            true,
        )? {
            dirs.push(dir);
        }
    }
    Ok(dirs)
}

/// Every platform folder under `Assets/`, with its `common` and per-core
/// folders. Platforms whose folders hold no files at all are still listed
/// (they exist because a core was installed), so the user sees the gap.
pub fn scan(pocket: &Pocket) -> Result<Vec<PlatformRoms>> {
    let assets = pocket.assets_dir();
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&assets).map_err(|e| Error::io(&assets, e))? {
        let entry = entry.map_err(|e| Error::io(&assets, e))?;
        if fsutil::is_hidden(&entry.file_name())
            || !entry
                .file_type()
                .map_err(|e| Error::io(entry.path(), e))?
                .is_dir()
        {
            continue;
        }
        let platform = entry.file_name().to_string_lossy().into_owned();
        let dirs = platform_dirs(pocket, &platform)?;
        out.push(PlatformRoms {
            files: dirs.iter().map(|d| d.files.len()).sum(),
            bytes: dirs.iter().map(|d| d.bytes).sum(),
            platform,
            dirs,
        });
    }
    if let Some(mut dir) = scan_dir(pocket, Path::new(super::GB_STUDIO_DIR), "", false)? {
        dir.files.retain(|f| {
            Path::new(&f.name)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("pocket"))
        });
        dir.bytes = dir.files.iter().map(|f| f.size).sum();
        out.push(PlatformRoms {
            platform: super::GB_STUDIO_PLATFORM.into(),
            files: dir.files.len(),
            bytes: dir.bytes,
            dirs: vec![dir],
        });
    }
    out.sort_by_key(|p| p.platform.to_lowercase());
    Ok(out)
}

// ---------------------------------------------------------------------------
// Index

const VERSION: u8 = 1;
const CACHE_PATH: &str = ".pocketeer/rom-index.msgpack";
const SAMPLE_BYTES: u64 = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Pending,
    Cached,
    Sampled,
    Full,
    Error,
}

#[derive(Debug, Clone, Serialize)]
pub struct CardRom {
    pub platform: String,
    pub owner: String,
    #[serde(rename = "path")]
    pub rel_path: PathBuf,
    pub name: String,
    pub size: u64,
    pub sha1: Option<String>,
    pub confidence: Confidence,
    pub error: Option<String>,
    #[serde(skip)]
    stamp: Stamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Cached {
    stamp: Stamp,
    #[serde(with = "serde_bytes")]
    sha1: [u8; 20],
    #[serde(with = "serde_bytes")]
    sample: [u8; 20],
}

#[derive(Serialize, Deserialize)]
struct Store {
    version: u8,
    entries: BTreeMap<PathBuf, Cached>,
}

pub struct Index {
    identity: CardIdentity,
    pub roms: Vec<CardRom>,
    /// Cache failures do not prevent listing or operating on ROM files.
    pub warning: Option<String>,
    entries: BTreeMap<PathBuf, Cached>,
    dirty: bool,
}

impl Index {
    pub fn scan(pocket: &Pocket) -> Result<Self> {
        Self::scan_with_protected(pocket, &bios::protected_paths(pocket)?)
    }

    /// Reuses declarations already read for a BIOS report in the same load.
    pub fn scan_with_protected(pocket: &Pocket, declared: &BTreeSet<PathBuf>) -> Result<Self> {
        let identity = CardIdentity::read(pocket)?;
        let cache_path = pocket.root().join(CACHE_PATH);
        let (mut entries, warning) = match fs::read(&cache_path) {
            Ok(bytes) => match rmp_serde::from_slice::<Store>(&bytes) {
                Ok(store) if store.version == VERSION => (store.entries, None),
                _ => (
                    BTreeMap::new(),
                    Some("ROM cache unreadable or obsolete; rebuilding".into()),
                ),
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => (BTreeMap::new(), None),
            Err(e) => (BTreeMap::new(), Some(format!("Cannot read ROM cache: {e}"))),
        };
        let mut roms = Vec::new();
        for platform in scan(pocket)? {
            for dir in platform.dirs {
                for file in dir.files {
                    let path = dir.path.join(file.name);
                    if declared.contains(&path)
                        || fsutil::is_sidecar(&path)
                        || fsutil::is_junk(&path)
                    {
                        continue;
                    }
                    let stamp = Stamp::of(&pocket.root().join(&path))?;
                    let cached = entries.get(&path).filter(|c| c.stamp == stamp);
                    roms.push(CardRom {
                        platform: platform.platform.clone(),
                        owner: dir.owner.clone(),
                        name: path
                            .file_name()
                            .expect("file")
                            .to_string_lossy()
                            .into_owned(),
                        size: stamp.size,
                        sha1: cached.map(|c| hash::hex(&c.sha1)),
                        confidence: if cached.is_some() {
                            Confidence::Cached
                        } else {
                            Confidence::Pending
                        },
                        rel_path: path,
                        stamp,
                        error: None,
                    });
                }
            }
        }
        // Only prune after a complete, successful directory scan.
        let before = entries.len();
        let stamps: BTreeMap<_, _> = roms.iter().map(|r| (&r.rel_path, &r.stamp)).collect();
        entries.retain(|path, entry| stamps.get(path).is_some_and(|stamp| **stamp == entry.stamp));
        let dirty = before != entries.len();
        identity.check(pocket)?;
        Ok(Self {
            identity,
            roms,
            warning,
            entries,
            dirty,
        })
    }

    /// Returns false when cancelled. Failures invalidate this file's cached
    /// identity; other files can still be verified. Full verification bypasses
    /// both the metadata shortcut and the sample check.
    pub fn verify(
        &mut self,
        pocket: &Pocket,
        i: usize,
        full: bool,
        cancelled: &dyn Fn() -> bool,
    ) -> bool {
        if cancelled() {
            return false;
        }
        let rel = self.roms[i].rel_path.clone();
        let result = self.read_identity(pocket, &rel, full, cancelled);
        if cancelled() {
            return false;
        }
        let rom = &mut self.roms[i];
        match result {
            Ok((entry, confidence)) => {
                rom.sha1 = Some(hash::hex(&entry.sha1));
                rom.stamp = entry.stamp;
                rom.size = entry.stamp.size;
                rom.confidence = confidence;
                rom.error = None;
                self.dirty |= self.entries.get(&rel) != Some(&entry);
                self.entries.insert(rel, entry);
            }
            Err(e) => {
                self.dirty |= self.entries.remove(&rel).is_some();
                rom.sha1 = None;
                rom.confidence = Confidence::Error;
                rom.error = Some(e.to_string());
            }
        }
        true
    }

    /// Hashes one card file, or trusts the cache when its stamp and sample
    /// still match (unless `full`). Fails if the card was swapped or the file
    /// changed while it was read.
    fn read_identity(
        &self,
        pocket: &Pocket,
        rel: &Path,
        full: bool,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(Cached, Confidence)> {
        self.identity.check(pocket)?;
        let path = pocket.root().join(rel);
        let before = Stamp::of(&path)?;
        let mut file = File::open(&path).map_err(|e| Error::io(&path, e))?;
        let sample = sample(&mut file, before.size, cancelled).map_err(|e| Error::io(&path, e))?;
        let cached = self
            .entries
            .get(rel)
            .filter(|c| !full && c.stamp == before && c.sample == sample);
        let (sha1, confidence) = match cached {
            Some(cached) => (cached.sha1, Confidence::Sampled),
            None => {
                file.rewind().map_err(|e| Error::io(&path, e))?;
                let mut digest = Sha1::new();
                hash::for_each_chunk(&mut file, |chunk| {
                    check_cancel(cancelled)?;
                    digest.update(chunk);
                    Ok(())
                })
                .map_err(|e| Error::io(&path, e))?;
                (digest.finalize().into(), Confidence::Full)
            }
        };
        self.identity.check(pocket)?;
        if before != Stamp::of(&path)? {
            return Err(Error::Changed { path });
        }
        let entry = Cached {
            stamp: before,
            sha1,
            sample,
        };
        Ok((entry, confidence))
    }

    pub fn relocate(&mut self, pocket: &Pocket, from: &Path, to: &Path) -> Result<()> {
        self.identity.check(pocket)?;
        if let Some(entry) = self.entries.remove(from) {
            self.dirty = true;
            if entry.stamp == Stamp::of(&pocket.root().join(to))? {
                self.entries.insert(to.to_path_buf(), entry);
            }
        }
        Ok(())
    }

    /// The caller has just copied and fully hashed these bytes. Reading only
    /// the sample here seeds future checks without rereading the whole ROM.
    pub fn record_copy(&mut self, pocket: &Pocket, path: &Path, sha1: &str) -> Result<()> {
        self.identity.check(pocket)?;
        let full = pocket.root().join(path);
        let before = Stamp::of(&full)?;
        let mut file = File::open(&full).map_err(|e| Error::io(&full, e))?;
        let sample = sample(&mut file, before.size, &|| false).map_err(|e| Error::io(&full, e))?;
        let digest = hash::parse_sha1(sha1).ok_or_else(|| Error::Mismatch {
            path: full.clone(),
            what: format!("not a SHA-1: {sha1:?}"),
        })?;
        if before != Stamp::of(&full)? {
            return Err(Error::Changed { path: full });
        }
        self.entries.insert(
            path.to_path_buf(),
            Cached {
                stamp: before,
                sha1: digest,
                sample,
            },
        );
        self.dirty = true;
        Ok(())
    }

    /// Batch persistence is separate from file operations: a failed cache write
    /// must never turn a successful copy or rename into an apparent failure.
    pub fn save(&mut self, pocket: &Pocket) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        self.identity.check(pocket)?;
        let path = pocket.root().join(CACHE_PATH);
        fs::create_dir_all(path.parent().expect("parent")).map_err(|e| Error::io(&path, e))?;
        let lock_path = pocket.root().join(".pocketeer/rom-index.lock");
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|e| Error::io(&lock_path, e))?;
        fs2::FileExt::lock_exclusive(&lock).map_err(|e| Error::io(&lock_path, e))?;
        let bytes = rmp_serde::to_vec(&(VERSION, &self.entries))
            .map_err(|e| Error::io(&path, io::Error::other(e)))?;
        fsutil::write_atomic(&path, &bytes)?;
        self.dirty = false;
        Ok(())
    }
}

fn check_cancel(cancelled: &dyn Fn() -> bool) -> io::Result<()> {
    if cancelled() {
        Err(io::Error::other("verification cancelled"))
    } else {
        Ok(())
    }
}

fn sample(file: &mut File, size: u64, cancelled: &dyn Fn() -> bool) -> io::Result<[u8; 20]> {
    let mut digest = Sha1::new();
    digest.update(size.to_le_bytes());
    let len = size.min(SAMPLE_BYTES);
    let mut offsets = vec![0, (size - len) / 2, size - len];
    offsets.dedup();
    let mut buf = vec![0; len as usize];
    for offset in offsets {
        check_cancel(cancelled)?;
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut buf)?;
        digest.update(offset.to_le_bytes());
        digest.update(&buf);
    }
    Ok(digest.finalize().into())
}

#[cfg(test)]
mod index_tests {
    use super::*;
    use crate::testutil::{sd, write};

    #[test]
    fn nested_duplicates_cache_roundtrip_and_external_rename() {
        let (tmp, pocket) = sd();
        write(tmp.path(), "Assets/gba/common/folder/one.gba", "same bytes");
        write(tmp.path(), "Assets/gba/common/two.gba", "same bytes");
        let mut index = Index::scan(&pocket).unwrap();
        assert_eq!(index.roms.len(), 2);
        for i in 0..2 {
            assert!(index.verify(&pocket, i, false, &|| false));
        }
        assert_eq!(index.roms[0].sha1, index.roms[1].sha1);
        assert_eq!(
            index.roms[0].sha1.as_ref().unwrap(),
            &hash::hash_file(&tmp.path().join(&index.roms[0].rel_path))
                .unwrap()
                .sha1
        );
        index.save(&pocket).unwrap();
        let mut index = Index::scan(&pocket).unwrap();
        assert!(
            index
                .roms
                .iter()
                .all(|r| r.confidence == Confidence::Cached)
        );
        index.verify(&pocket, 0, false, &|| false);
        assert_eq!(index.roms[0].confidence, Confidence::Sampled);
        fs::rename(
            tmp.path().join("Assets/gba/common/two.gba"),
            tmp.path().join("Assets/gba/common/renamed.gba"),
        )
        .unwrap();
        let mut index = Index::scan(&pocket).unwrap();
        let i = index
            .roms
            .iter()
            .position(|r| r.name == "renamed.gba")
            .unwrap();
        assert_eq!(index.roms[i].confidence, Confidence::Pending);
        index.verify(&pocket, i, false, &|| false);
        assert_eq!(index.roms[0].sha1, index.roms[1].sha1);
        index.save(&pocket).unwrap();
        let store: Store =
            rmp_serde::from_slice(&fs::read(tmp.path().join(CACHE_PATH)).unwrap()).unwrap();
        assert_eq!(store.entries.len(), 2);
        assert!(
            !store
                .entries
                .contains_key(Path::new("Assets/gba/common/two.gba"))
        );
    }

    #[test]
    fn same_metadata_replacement_sample_check_and_full_verification() {
        let (tmp, pocket) = sd();
        let path = tmp.path().join("Assets/gba/common/test.gba");
        write(
            tmp.path(),
            "Assets/gba/common/test.gba",
            "a".repeat(256 * 1024),
        );
        let time = fs::metadata(&path).unwrap().modified().unwrap();
        let mut index = Index::scan(&pocket).unwrap();
        index.verify(&pocket, 0, false, &|| false);
        let original = index.roms[0].sha1.clone();
        index.save(&pocket).unwrap();
        // An edit outside the sample is deliberately NOT claimed as fully verified.
        let mut bytes = fs::read(&path).unwrap();
        bytes[32 * 1024] = b'b';
        fs::write(&path, &bytes).unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(time)
            .unwrap();
        let mut index = Index::scan(&pocket).unwrap();
        index.verify(&pocket, 0, false, &|| false);
        assert_eq!(index.roms[0].confidence, Confidence::Sampled);
        assert_eq!(index.roms[0].sha1, original);
        index.verify(&pocket, 0, true, &|| false);
        assert_ne!(index.roms[0].sha1, original);
        let previous = index.roms[0].sha1.clone();
        bytes[0] = b'c';
        fs::write(&path, bytes).unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(time)
            .unwrap();
        index.verify(&pocket, 0, false, &|| false);
        assert_eq!(index.roms[0].confidence, Confidence::Full);
        assert_ne!(index.roms[0].sha1, previous);
    }

    #[test]
    fn corrupt_cache_cancellation_and_missing_file_are_recoverable() {
        let (tmp, pocket) = sd();
        write(tmp.path(), CACHE_PATH, "broken");
        write(tmp.path(), "Assets/gb/common/test.gb", "rom");
        let mut index = Index::scan(&pocket).unwrap();
        assert!(index.warning.is_some());
        assert!(!index.verify(&pocket, 0, false, &|| true));
        assert_eq!(index.roms[0].confidence, Confidence::Pending);
        index.verify(&pocket, 0, false, &|| false);
        assert_eq!(index.roms[0].confidence, Confidence::Full);
        fs::remove_file(tmp.path().join(&index.roms[0].rel_path)).unwrap();
        index.verify(&pocket, 0, false, &|| false);
        assert_eq!(index.roms[0].confidence, Confidence::Error);
        assert!(index.roms[0].sha1.is_none());
        assert!(index.roms[0].error.is_some());
    }
}

#[cfg(test)]
mod scan_tests {
    use super::*;
    use crate::testutil::{sd, write};
    use std::fs;

    #[test]
    fn scans_platforms_common_first() {
        let (tmp, pocket) = sd();
        let w = |rel: &str, body: &str| write(tmp.path(), rel, body);
        w("Assets/gba/common/b.gba", "22");
        w("Assets/gba/common/A.gba", "1");
        w("Assets/gba/common/.DS_Store", "x");
        w("Assets/gba/z.Core/game.json", "{}");
        w("Assets/gba/z.Core/sub/inner.bin", "xxx");
        w("Assets/arduboy/common/.gitkeep", "");
        fs::create_dir_all(tmp.path().join("Assets/.hidden/common")).unwrap();

        let plats = scan(&pocket).unwrap();
        assert_eq!(plats.len(), 2);
        assert_eq!(plats[0].platform, "arduboy");
        assert_eq!(plats[0].files, 0);
        let gba = &plats[1];
        assert_eq!(gba.platform, "gba");
        assert_eq!(gba.files, 4);
        assert_eq!(gba.bytes, 8);
        assert_eq!(gba.dirs[0].owner, "common");
        let names: Vec<&str> = gba.dirs[0].files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["A.gba", "b.gba"]);
        assert_eq!(gba.dirs[1].owner, "z.Core");
        assert_eq!(gba.dirs[1].subdirs, 1);
        assert_eq!(gba.dirs[1].files[1].name, "sub/inner.bin");
        assert_eq!(gba.dirs[1].path, Path::new("Assets/gba/z.Core"));
    }
}
