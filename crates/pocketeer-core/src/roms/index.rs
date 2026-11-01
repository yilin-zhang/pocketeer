//! Disposable content index for a card. Listing reads metadata only; verification
//! is explicit so callers can schedule one file at a time without blocking a UI.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

use super::{card, hash};
use crate::{Error, Pocket, Result, fsutil, required, sd::CardIdentity};

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
struct Stamp {
    size: u64,
    seconds: u64,
    nanos: u32,
}

impl Stamp {
    fn read(path: &Path) -> Result<Self> {
        let meta = fs::metadata(path).map_err(|e| Error::io(path, e))?;
        let modified = meta
            .modified()
            .map_err(|e| Error::io(path, e))?
            .duration_since(UNIX_EPOCH)
            .map_err(|e| Error::io(path, io::Error::other(e)))?;
        Ok(Self {
            size: meta.len(),
            seconds: modified.as_secs(),
            nanos: modified.subsec_nanos(),
        })
    }
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
        Self::scan_with_protected(pocket, &required::protected_paths(pocket)?)
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
        for platform in card::scan(pocket)? {
            for dir in platform.dirs {
                for file in dir.files {
                    let path = dir.path.join(file.name);
                    if declared.contains(&path)
                        || fsutil::is_sidecar(&path)
                        || fsutil::is_junk(&path)
                    {
                        continue;
                    }
                    let stamp = Stamp::read(&pocket.root().join(&path))?;
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
                        sha1: cached.map(|c| hex(&c.sha1)),
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
        if identity != CardIdentity::read(pocket)? {
            return Err(Error::io(
                pocket.root(),
                io::Error::other("card changed during scan"),
            ));
        }
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
        let identity_matches = CardIdentity::read(pocket).is_ok_and(|id| id == self.identity);
        let rom = &mut self.roms[i];
        let path = pocket.root().join(&rom.rel_path);
        let result = (|| -> Result<(Cached, Confidence)> {
            if !identity_matches {
                return Err(Error::io(
                    pocket.root(),
                    io::Error::other("card disconnected or replaced"),
                ));
            }
            let before = Stamp::read(&path)?;
            let mut file = File::open(&path).map_err(|e| Error::io(&path, e))?;
            let sample =
                sample(&mut file, before.size, cancelled).map_err(|e| Error::io(&path, e))?;
            let cached = self
                .entries
                .get(&rom.rel_path)
                .filter(|c| c.stamp == before && c.sample == sample);
            let (sha1, confidence) = if let Some(cached) = cached.filter(|_| !full) {
                (cached.sha1, Confidence::Sampled)
            } else {
                file.rewind().map_err(|e| Error::io(&path, e))?;
                let mut digest = Sha1::new();
                hash::for_each_chunk(&mut file, |chunk| {
                    check_cancel(cancelled)?;
                    digest.update(chunk);
                    Ok(())
                })
                .map_err(|e| Error::io(&path, e))?;
                (digest.finalize().into(), Confidence::Full)
            };
            if before != Stamp::read(&path)? || self.identity != CardIdentity::read(pocket)? {
                return Err(Error::io(
                    &path,
                    io::Error::other("file changed during verification"),
                ));
            }
            Ok((
                Cached {
                    stamp: before,
                    sha1,
                    sample,
                },
                confidence,
            ))
        })();
        if cancelled() {
            return false;
        }
        match result {
            Ok((entry, confidence)) => {
                rom.sha1 = Some(hex(&entry.sha1));
                rom.stamp = entry.stamp.clone();
                rom.size = entry.stamp.size;
                rom.confidence = confidence;
                rom.error = None;
                self.dirty |= self.entries.get(&rom.rel_path) != Some(&entry);
                self.entries.insert(rom.rel_path.clone(), entry);
            }
            Err(e) => {
                self.dirty |= self.entries.remove(&rom.rel_path).is_some();
                rom.sha1 = None;
                rom.confidence = Confidence::Error;
                rom.error = Some(e.to_string());
            }
        }
        true
    }

    pub fn relocate(&mut self, pocket: &Pocket, from: &Path, to: &Path) -> Result<()> {
        if self.identity != CardIdentity::read(pocket)? {
            return Err(Error::io(pocket.root(), io::Error::other("card changed")));
        }
        if let Some(entry) = self.entries.remove(from) {
            self.dirty = true;
            if entry.stamp == Stamp::read(&pocket.root().join(to))? {
                self.entries.insert(to.to_path_buf(), entry);
            }
        }
        Ok(())
    }

    /// The caller has just copied and fully hashed these bytes. Reading only
    /// the sample here seeds future checks without rereading the whole ROM.
    pub fn record_copy(&mut self, pocket: &Pocket, path: &Path, sha1: &str) -> Result<()> {
        if self.identity != CardIdentity::read(pocket)? {
            return Err(Error::io(
                pocket.root(),
                io::Error::other("card disconnected or replaced"),
            ));
        }
        let full = pocket.root().join(path);
        let before = Stamp::read(&full)?;
        let mut file = File::open(&full).map_err(|e| Error::io(&full, e))?;
        let sample = sample(&mut file, before.size, &|| false).map_err(|e| Error::io(&full, e))?;
        let mut digest = [0u8; 20];
        if sha1.len() != 40 || !sha1.is_ascii() {
            return Err(Error::io(&full, io::Error::other("invalid SHA-1")));
        }
        for (i, byte) in digest.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&sha1[i * 2..i * 2 + 2], 16)
                .map_err(|e| Error::io(&full, io::Error::other(e)))?;
        }
        if before != Stamp::read(&full)? {
            return Err(Error::io(&full, io::Error::other("copied file changed")));
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
        if self.identity != CardIdentity::read(pocket)? {
            return Err(Error::io(
                pocket.root(),
                io::Error::other("card disconnected or replaced"),
            ));
        }
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

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
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
mod tests {
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
