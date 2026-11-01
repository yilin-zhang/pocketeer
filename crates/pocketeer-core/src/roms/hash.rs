//! CRC32 + SHA-1 of files, with a cache keyed on (path, size, mtime) so
//! re-verifying a large library doesn't re-read every file.

use std::{
    collections::HashMap,
    io::Read,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha1::Digest;

use crate::{
    Error, Result,
    fsutil::{self, write_atomic},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hashes {
    pub size: u64,
    pub crc32: u32,
    pub sha1: String,
}

impl Hashes {
    pub fn crc32_hex(&self) -> String {
        format!("{:08x}", self.crc32)
    }
}

/// Feeds `reader` to `chunk` in 256 KiB pieces until it is exhausted. The
/// one read loop every hash in the crate goes through.
pub(super) fn for_each_chunk(
    reader: &mut (impl Read + ?Sized),
    mut chunk: impl FnMut(&[u8]) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        chunk(&buf[..n])?;
    }
}

/// Hashes everything the reader yields, in one pass.
pub fn hash_reader(reader: &mut (impl Read + ?Sized)) -> std::io::Result<Hashes> {
    copy_and_hash(reader, &mut std::io::sink())
}

/// Copies `reader` into `out` and hashes the bytes on the way: one read of
/// the source instead of one for hashing and one for copying.
pub fn copy_and_hash(
    reader: &mut (impl Read + ?Sized),
    out: &mut impl std::io::Write,
) -> std::io::Result<Hashes> {
    let mut crc = crc32fast::Hasher::new();
    let mut sha = sha1::Sha1::new();
    let mut size = 0u64;
    for_each_chunk(reader, |chunk| {
        out.write_all(chunk)?;
        crc.update(chunk);
        sha.update(chunk);
        size += chunk.len() as u64;
        Ok(())
    })?;
    Ok(Hashes {
        size,
        crc32: crc.finalize(),
        sha1: format!("{:x}", sha.finalize()),
    })
}

/// Lowercase hex digest of everything the reader yields, for any
/// [`Digest`] (md5 for BIOS checks, sha1 elsewhere).
pub fn digest_hex<D: Digest>(reader: &mut (impl Read + ?Sized)) -> std::io::Result<String> {
    let mut digest = D::new();
    for_each_chunk(reader, |chunk| {
        digest.update(chunk);
        Ok(())
    })?;
    Ok(digest
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

pub fn hash_file(path: &Path) -> Result<Hashes> {
    let mut file = std::fs::File::open(path).map_err(|e| Error::io(path, e))?;
    hash_reader(&mut file).map_err(|e| Error::io(path, e))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    /// Validity stamp of the file this came from (for zip entries: the zip).
    stamp_size: u64,
    mtime: u64,
    /// Size of the hashed content itself.
    size: u64,
    crc32: u32,
    sha1: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    entries: HashMap<String, Entry>,
}

/// Persistent hash cache. `None` path = in-memory only.
#[derive(Debug, Default)]
pub struct HashCache {
    path: Option<PathBuf>,
    store: Store,
    dirty: bool,
}

/// Validity stamp of a file: changes if it's rewritten.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub size: u64,
    pub mtime: u64,
}

impl Stamp {
    pub fn of(path: &Path) -> Result<Stamp> {
        let meta = std::fs::metadata(path).map_err(|e| Error::io(path, e))?;
        Ok(Stamp {
            size: meta.len(),
            mtime: fsutil::mtime_of(&meta).max(0) as u64,
        })
    }
}

impl HashCache {
    /// Opens `<cache_dir>/rom-hashes.json`, ignoring a missing or corrupt file.
    pub fn open(cache_dir: Option<&Path>) -> Self {
        let path = cache_dir.map(|d| d.join("rom-hashes.json"));
        let store = path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        HashCache {
            path,
            store,
            dirty: false,
        }
    }

    pub fn in_memory() -> Self {
        Self::default()
    }

    /// Hashes a plain file, reusing the cached result if it hasn't changed.
    pub fn file(&mut self, path: &Path) -> Result<Hashes> {
        let stamp = Stamp::of(path)?;
        let key = path.to_string_lossy().into_owned();
        self.cached(&key, stamp, || hash_file(path))
    }

    /// Hashes something identified by `key` whose validity follows `stamp`
    /// (e.g. an entry inside a zip, stamped with the zip's size/mtime).
    pub fn cached(
        &mut self,
        key: &str,
        stamp: Stamp,
        compute: impl FnOnce() -> Result<Hashes>,
    ) -> Result<Hashes> {
        if let Some(e) = self.store.entries.get(key)
            && e.stamp_size == stamp.size
            && e.mtime == stamp.mtime
        {
            return Ok(Hashes {
                size: e.size,
                crc32: e.crc32,
                sha1: e.sha1.clone(),
            });
        }
        let hashes = compute()?;
        self.store.entries.insert(
            key.to_string(),
            Entry {
                stamp_size: stamp.size,
                mtime: stamp.mtime,
                size: hashes.size,
                crc32: hashes.crc32,
                sha1: hashes.sha1.clone(),
            },
        );
        self.dirty = true;
        Ok(hashes)
    }

    pub fn save(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        if let Some(path) = &self.path {
            let bytes = serde_json::to_vec(&self.store).expect("serializable");
            write_atomic(path, &bytes)?;
        }
        self.dirty = false;
        Ok(())
    }

    /// [`HashCache::save`] for the end of an operation whose result matters
    /// more than the cache: a cache that can't be written is just slower next
    /// time, never an error.
    pub fn save_best_effort(&mut self) {
        let _ = self.save();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_answers() {
        // CRC32 and SHA-1 of "hello world" and the pangram are widely published.
        let h = hash_reader(&mut &b"hello world"[..]).unwrap();
        assert_eq!(h.size, 11);
        assert_eq!(h.crc32_hex(), "0d4a1185");
        assert_eq!(h.sha1, "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed");
        let h = hash_reader(&mut &b"The quick brown fox jumps over the lazy dog"[..]).unwrap();
        assert_eq!(h.crc32_hex(), "414fa339");
        assert_eq!(h.sha1, "2fd4e1c67a2d28fced849ee1bb76e7391b93eb12");
        let h = hash_reader(&mut &b""[..]).unwrap();
        assert_eq!(h.crc32, 0);
        assert_eq!(h.sha1, "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    }

    /// Zip entries are stamped with the zip's size but hashed by their own.
    #[test]
    fn cached_entry_keeps_content_size_separate_from_stamp() {
        let mut cache = HashCache::in_memory();
        let stamp = Stamp {
            size: 1000,
            mtime: 1,
        };
        let first = cache
            .cached("z.zip::inner", stamp, || {
                hash_reader(&mut &b"abc"[..]).map_err(|e| Error::io("z", e))
            })
            .unwrap();
        let second = cache
            .cached("z.zip::inner", stamp, || unreachable!("must be cached"))
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(second.size, 3);
    }

    #[test]
    fn cache_reuses_until_file_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("rom.bin");
        std::fs::write(&file, "hello world").unwrap();
        let mut cache = HashCache::open(Some(tmp.path()));
        assert_eq!(cache.file(&file).unwrap().crc32_hex(), "0d4a1185");
        cache.save().unwrap();

        let mut reopened = HashCache::open(Some(tmp.path()));
        assert_eq!(reopened.store.entries.len(), 1);
        // Same stamp → cached value even though we never read the file.
        let mut calls = 0;
        let stamp = Stamp::of(&file).unwrap();
        let h = reopened
            .cached(&file.to_string_lossy(), stamp, || {
                calls += 1;
                hash_file(&file)
            })
            .unwrap();
        assert_eq!(calls, 0);
        assert_eq!(h.sha1, "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed");

        // Different size → recomputed.
        std::fs::write(&file, "hello world!").unwrap();
        assert_eq!(reopened.file(&file).unwrap().size, 12);
    }
}
