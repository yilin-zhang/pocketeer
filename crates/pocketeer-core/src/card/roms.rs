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
    card::bios,
    fsutil::{self, Stamp},
    plan::{Checked, Pinned, Plan, Progress, Resources},
};

// ---------------------------------------------------------------------------
// Scan

/// The visible files under one folder, relative to the SD root, sorted by
/// path (case-insensitive); empty if the folder doesn't exist.
fn files_in(pocket: &Pocket, rel: &Path, recursive: bool) -> Result<Vec<PathBuf>> {
    let full = pocket.root().join(rel);
    match fs::symlink_metadata(&full) {
        Ok(meta) if meta.is_symlink() => return Ok(Vec::new()),
        Ok(meta) if !meta.is_dir() => return Err(Error::NotADirectory { path: full }),
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::io(&full, e)),
    }
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(&full)
        .min_depth(1)
        .max_depth(if recursive { usize::MAX } else { 1 })
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !fsutil::is_hidden(e.file_name()))
    {
        let entry = entry
            .map_err(|e| Error::io(e.path().unwrap_or(&full), io::Error::other(e.to_string())))?;
        if entry.file_type().is_file() {
            files.push(rel.join(entry.path().strip_prefix(&full).expect("descendant")));
        }
    }
    files.sort_by_key(|f| f.to_string_lossy().to_lowercase());
    Ok(files)
}

/// The visible subfolder names of `dir`, sorted case-insensitively; empty if
/// `dir` doesn't exist.
fn subfolders(dir: &Path) -> Result<Vec<String>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::io(dir, e)),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| Error::io(dir, e))?;
        if entry
            .file_type()
            .map_err(|e| Error::io(entry.path(), e))?
            .is_dir()
            && !fsutil::is_hidden(&entry.file_name())
        {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    names.sort_by_key(|n| n.to_lowercase());
    Ok(names)
}

/// Every ROM file on the card as (platform, owner, path from the SD root):
/// `Assets/<platform>/<common|core id>/**` and the `.pocket` files in
/// `GB Studio/`. Platforms in order, `common` before the cores' own folders.
fn walk(pocket: &Pocket) -> Result<Vec<(String, String, PathBuf)>> {
    let assets = pocket.assets_dir();
    // A card without `Assets/` is not a card; every other folder may be missing.
    fs::metadata(&assets).map_err(|e| Error::io(&assets, e))?;
    let mut platforms: Vec<(String, Vec<(String, PathBuf)>)> = Vec::new();
    for platform in subfolders(&assets)? {
        let mut owners = subfolders(&assets.join(&platform))?;
        owners.sort_by_key(|o| o != super::COMMON);
        let mut files = Vec::new();
        for owner in owners {
            for path in files_in(pocket, &super::assets_rel(&platform, &owner), true)? {
                files.push((owner.clone(), path));
            }
        }
        platforms.push((platform, files));
    }
    let gb_studio: Vec<_> = files_in(pocket, Path::new(super::GB_STUDIO_DIR), false)?
        .into_iter()
        .filter(|f| {
            f.extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("pocket"))
        })
        .map(|f| (String::new(), f))
        .collect();
    if !gb_studio.is_empty() {
        platforms.push((super::GB_STUDIO_PLATFORM.into(), gb_studio));
    }
    platforms.sort_by_key(|(p, _)| p.to_lowercase());
    Ok(platforms
        .into_iter()
        .flat_map(|(platform, files)| {
            files
                .into_iter()
                .map(move |(owner, path)| (platform.clone(), owner, path))
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Index

const VERSION: u8 = 1;
/// Inside the card's `.pocketeer/` folder.
const CACHE_FILE: &str = "rom-index.msgpack";
const LOCK_FILE: &str = "rom-index.lock";
const SAMPLE_BYTES: u64 = 16 * 1024;

/// What is known about a card ROM's contents. pocketeer reads card files
/// only when asked, so most start out `Pending` or `Cached`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "identity", rename_all = "snake_case")]
pub enum Identity {
    /// Not read yet.
    Pending,
    /// From the cache: the file looks unchanged since it was last hashed.
    Cached { sha1: String },
    /// Confirmed by reading a sample of the file.
    Sampled { sha1: String },
    /// Confirmed by reading every byte.
    Full { sha1: String },
    /// Reading it failed.
    Error { error: String },
}

impl Identity {
    pub fn sha1(&self) -> Option<&str> {
        match self {
            Identity::Cached { sha1 } | Identity::Sampled { sha1 } | Identity::Full { sha1 } => {
                Some(sha1)
            }
            Identity::Pending | Identity::Error { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CardRom {
    pub platform: String,
    pub owner: String,
    #[serde(rename = "path")]
    pub rel_path: PathBuf,
    pub name: String,
    pub size: u64,
    #[serde(flatten)]
    pub identity: Identity,
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
    /// The card this index describes; every write checks it is still mounted.
    pinned: Pinned,
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
        let pinned = Pinned::card(pocket)?;
        let cache_path = pocket.pocketeer_dir().join(CACHE_FILE);
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
        for (platform, owner, path) in walk(pocket)? {
            if declared.contains(&path) || fsutil::is_sidecar(&path) || fsutil::is_junk(&path) {
                continue;
            }
            let stamp = Stamp::of(&pocket.root().join(&path))?;
            let cached = entries.get(&path).filter(|c| c.stamp == stamp);
            roms.push(CardRom {
                platform,
                owner,
                name: path
                    .file_name()
                    .expect("file")
                    .to_string_lossy()
                    .into_owned(),
                size: stamp.size,
                identity: match cached {
                    Some(c) => Identity::Cached {
                        sha1: hash::hex(&c.sha1),
                    },
                    None => Identity::Pending,
                },
                rel_path: path,
                stamp,
            });
        }
        // Only prune after a complete, successful directory scan.
        let before = entries.len();
        let stamps: BTreeMap<_, _> = roms.iter().map(|r| (&r.rel_path, &r.stamp)).collect();
        entries.retain(|path, entry| stamps.get(path).is_some_and(|stamp| **stamp == entry.stamp));
        let dirty = before != entries.len();
        pinned.check_card(pocket)?;
        Ok(Self {
            pinned,
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
            Ok((entry, identity)) => {
                rom.identity = identity;
                rom.stamp = entry.stamp;
                rom.size = entry.stamp.size;
                self.dirty |= self.entries.get(&rel) != Some(&entry);
                self.entries.insert(rel, entry);
            }
            Err(e) => {
                self.dirty |= self.entries.remove(&rel).is_some();
                rom.identity = Identity::Error {
                    error: e.to_string(),
                };
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
    ) -> Result<(Cached, Identity)> {
        self.pinned.check_card(pocket)?;
        let path = pocket.root().join(rel);
        let before = Stamp::of(&path)?;
        let mut file = File::open(&path).map_err(|e| Error::io(&path, e))?;
        let sample = sample(&mut file, before.size, cancelled).map_err(|e| Error::io(&path, e))?;
        let cached = self
            .entries
            .get(rel)
            .filter(|c| !full && c.stamp == before && c.sample == sample);
        let (sha1, sampled) = match cached {
            Some(cached) => (cached.sha1, true),
            None => {
                file.rewind().map_err(|e| Error::io(&path, e))?;
                let mut digest = Sha1::new();
                hash::for_each_chunk(&mut file, |chunk| {
                    check_cancel(cancelled)?;
                    digest.update(chunk);
                    Ok(())
                })
                .map_err(|e| Error::io(&path, e))?;
                (digest.finalize().into(), false)
            }
        };
        self.pinned.check_card(pocket)?;
        if before != Stamp::of(&path)? {
            return Err(Error::Changed { path });
        }
        let entry = Cached {
            stamp: before,
            sha1,
            sample,
        };
        let sha1 = hash::hex(&sha1);
        let identity = match sampled {
            true => Identity::Sampled { sha1 },
            false => Identity::Full { sha1 },
        };
        Ok((entry, identity))
    }

    pub fn relocate(&mut self, pocket: &Pocket, from: &Path, to: &Path) -> Result<()> {
        self.pinned.check_card(pocket)?;
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
        self.pinned.check_card(pocket)?;
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
        self.pinned.check_card(pocket)?;
        let path = pocket.pocketeer_dir().join(CACHE_FILE);
        fs::create_dir_all(path.parent().expect("parent")).map_err(|e| Error::io(&path, e))?;
        let lock_path = pocket.pocketeer_dir().join(LOCK_FILE);
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

// ---------------------------------------------------------------------------
// Delete

/// Deleting one ROM file from the card (one the library has no copy of).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DeletePlan {
    /// Relative to the card root.
    pub path: PathBuf,
    #[serde(skip)]
    pinned: Pinned,
}

impl Plan for DeletePlan {
    type Output = crate::card::install::Changes;

    fn changes(&self) -> usize {
        1
    }

    fn pinned(&self) -> &Pinned {
        &self.pinned
    }

    fn execute(
        &mut self,
        res: &mut Resources,
        progress: Progress<'_>,
        _: Checked,
    ) -> Result<Self::Output> {
        let actions = [crate::card::install::Action::Remove {
            path: self.path.clone(),
            reason: crate::card::install::plan::RemoveReason::Uninstall,
        }];
        crate::card::install::apply::apply(
            res.pocket()?,
            "delete-rom",
            &actions,
            &mut |_, _| unreachable!("a delete installs nothing"),
            progress,
        )
    }
}

pub fn plan_delete(pocket: &Pocket, path: &Path) -> Result<DeletePlan> {
    Ok(DeletePlan {
        path: path.to_path_buf(),
        pinned: Pinned::card(pocket)?,
    })
}

#[cfg(test)]
mod index_tests {
    use super::*;
    use crate::testutil::{sd, write};

    #[test]
    fn deleting_from_a_swapped_card_is_refused() {
        let (_tmp, pocket) = sd();
        write(pocket.root(), "Assets/gba/common/a.gba", "rom");
        let mut plan = plan_delete(&pocket, Path::new("Assets/gba/common/a.gba")).unwrap();
        let aside = tempfile::tempdir().unwrap();
        crate::testutil::swap_card(&pocket, aside.path());
        let err = crate::testutil::apply(&mut plan, Some(&pocket), None).unwrap_err();
        assert_eq!(err.code(), "STALE_PLAN");
        assert!(pocket.root().join("Assets/gba/common/a.gba").exists());
    }

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
        assert_eq!(index.roms[0].identity.sha1(), index.roms[1].identity.sha1());
        // `shasum` of "same bytes".
        assert_eq!(
            index.roms[0].identity.sha1(),
            Some("9ad949ae6c2f37319cfa43f410db66843000a047")
        );
        index.save(&pocket).unwrap();
        let mut index = Index::scan(&pocket).unwrap();
        assert!(
            index
                .roms
                .iter()
                .all(|r| matches!(r.identity, Identity::Cached { .. }))
        );
        index.verify(&pocket, 0, false, &|| false);
        assert!(matches!(index.roms[0].identity, Identity::Sampled { .. }));
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
        assert!(matches!(index.roms[i].identity, Identity::Pending));
        index.verify(&pocket, i, false, &|| false);
        assert_eq!(index.roms[0].identity.sha1(), index.roms[1].identity.sha1());
        index.save(&pocket).unwrap();
        let store: Store =
            rmp_serde::from_slice(&fs::read(pocket.pocketeer_dir().join(CACHE_FILE)).unwrap())
                .unwrap();
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
        let original = index.roms[0].identity.sha1().map(str::to_owned);
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
        assert!(matches!(index.roms[0].identity, Identity::Sampled { .. }));
        assert_eq!(index.roms[0].identity.sha1(), original.as_deref());
        index.verify(&pocket, 0, true, &|| false);
        assert_ne!(index.roms[0].identity.sha1(), original.as_deref());
        let previous = index.roms[0].identity.sha1().map(str::to_owned);
        bytes[0] = b'c';
        fs::write(&path, bytes).unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(time)
            .unwrap();
        index.verify(&pocket, 0, false, &|| false);
        assert!(matches!(index.roms[0].identity, Identity::Full { .. }));
        assert_ne!(index.roms[0].identity.sha1(), previous.as_deref());
    }

    #[test]
    fn corrupt_cache_cancellation_and_missing_file_are_recoverable() {
        let (tmp, pocket) = sd();
        write(&pocket.pocketeer_dir(), CACHE_FILE, "broken");
        write(tmp.path(), "Assets/gb/common/test.gb", "rom");
        let mut index = Index::scan(&pocket).unwrap();
        assert!(index.warning.is_some());
        assert!(!index.verify(&pocket, 0, false, &|| true));
        assert!(matches!(index.roms[0].identity, Identity::Pending));
        index.verify(&pocket, 0, false, &|| false);
        assert!(matches!(index.roms[0].identity, Identity::Full { .. }));
        fs::remove_file(tmp.path().join(&index.roms[0].rel_path)).unwrap();
        index.verify(&pocket, 0, false, &|| false);
        assert!(matches!(index.roms[0].identity, Identity::Error { .. }));
    }
}

#[cfg(test)]
mod scan_tests {
    use super::*;
    use crate::testutil::{sd, write};

    #[test]
    fn lists_every_rom_on_the_card_common_folder_first() {
        let (tmp, pocket) = sd();
        let w = |rel: &str| write(tmp.path(), rel, "x");
        w("Assets/gba/z.Core/sub/inner.gba");
        w("Assets/gba/common/b.gba");
        w("Assets/gba/common/A.gba");
        w("Assets/gba/common/.DS_Store");
        w("Assets/.hidden/common/c.gba");
        w("Assets/arduboy/common/game.hex");
        w("GB Studio/demo.pocket");
        w("GB Studio/readme.txt");

        let index = Index::scan(&pocket).unwrap();
        let listed: Vec<_> = index
            .roms
            .iter()
            .map(|r| {
                (
                    r.platform.as_str(),
                    r.owner.as_str(),
                    r.rel_path.to_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            listed,
            [
                ("arduboy", "common", "Assets/arduboy/common/game.hex"),
                ("gba", "common", "Assets/gba/common/A.gba"),
                ("gba", "common", "Assets/gba/common/b.gba"),
                ("gba", "z.Core", "Assets/gba/z.Core/sub/inner.gba"),
                ("gbstudio", "", "GB Studio/demo.pocket"),
            ]
        );
    }
}
