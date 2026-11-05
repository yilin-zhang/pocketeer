//! Helpers for files on disk, shared by the device, the library and the DAT
//! mirror.

use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    time::SystemTime,
};

use serde::{Deserialize, Serialize};

use crate::{
    error::{BaseError, io_at},
    game::{Files, Game, Rom},
    platform::Platform,
};

/// What is in a folder, by name, sorted.
#[derive(Default)]
pub(crate) struct Listing {
    pub(crate) dirs: Vec<String>,
    pub(crate) files: Vec<String>,
}

/// What is in `dir`; nothing if it doesn't exist. Hidden entries (macOS
/// leaves `._` files on devices) and symlinks are left out.
pub(crate) fn list(dir: &Path) -> io::Result<Listing> {
    let read = match fs::read_dir(dir) {
        Ok(read) => read,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Listing::default()),
        Err(e) => return Err(e),
    };
    let (mut dirs, mut files) = (Vec::new(), Vec::new());
    for entry in read {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let file_type = entry.file_type()?;
        if name.starts_with('.') {
            continue;
        }
        if file_type.is_dir() {
            dirs.push(name);
        } else if file_type.is_file() {
            files.push(name);
        }
    }
    dirs.sort();
    files.sort();
    Ok(Listing { dirs, files })
}

/// Replaces the file at `path` with `bytes`, last written at `modified` if
/// given.
pub(crate) fn write(
    path: &Path,
    bytes: &[u8],
    modified: Option<SystemTime>,
) -> Result<(), BaseError> {
    replace(
        path,
        |tmp| fs::write(tmp, bytes).map_err(io_at(tmp)),
        modified,
    )
}

/// Replaces the file at `to` with a copy of `from`, without holding all of
/// it in memory.
pub(crate) fn copy(from: &Path, to: &Path) -> Result<(), BaseError> {
    replace(
        to,
        |tmp| fs::copy(from, tmp).map(drop).map_err(io_at(from)),
        None,
    )
}

/// Replaces the file at `path` with what `fill` writes to a hidden file beside
/// it, so that `path` is never half written. If this fails the hidden file is
/// removed; if the process dies first, [`list`] doesn't see it and the next
/// write to `path` replaces it.
fn replace(
    path: &Path,
    fill: impl FnOnce(&Path) -> Result<(), BaseError>,
    modified: Option<SystemTime>,
) -> Result<(), BaseError> {
    let dir = path.parent().expect("files are written into a folder");
    fs::create_dir_all(dir).map_err(io_at(dir))?;
    let mut name = OsString::from(".");
    name.push(path.file_name().expect("files have a name"));
    name.push(".part");
    let tmp = path.with_file_name(name);
    let replaced = fill(&tmp)
        .and_then(|()| match modified {
            Some(modified) => fs::File::options()
                .write(true)
                .open(&tmp)
                .and_then(|f| f.set_modified(modified))
                .map_err(io_at(&tmp)),
            None => Ok(()),
        })
        .and_then(|()| fs::rename(&tmp, path).map_err(io_at(path)));
    if replaced.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    replaced
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Cached {
    /// The file's size; a NES ROM's [`Rom`] is smaller, without its header.
    size: u64,
    modified: SystemTime,
    rom: Rom,
}

/// What was read of a device's or library's games: each file's [`Rom`], kept
/// while its size and time stay the same. It only saves time, so a cache that
/// can't be read is empty and one that can't be written is not written.
#[derive(Debug, Clone)]
pub(crate) struct Cache {
    /// The root of the device or library.
    root: PathBuf,
    /// The file it is kept in.
    path: PathBuf,
    /// Path from `root` → what was read, so that it holds wherever the device
    /// is mounted.
    files: HashMap<PathBuf, Cached>,
}

impl Cache {
    /// The cache of the files under `root`, kept in the file at `path`.
    pub(crate) fn load(root: &Path, path: &Path) -> Self {
        let files = fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            root: root.to_path_buf(),
            path: path.to_path_buf(),
            files,
        }
    }

    pub(crate) fn save(&self) {
        if let Ok(json) = serde_json::to_vec(&self.files) {
            let _ = write(&self.path, &json, None);
        }
    }

    /// The game at `path`: from this cache if its size and time are what they
    /// were, otherwise read, and then noted here.
    pub(crate) fn read(&mut self, platform: &Platform, path: &Path) -> io::Result<Game> {
        let meta = fs::metadata(path)?;
        let (size, modified) = (meta.len(), meta.modified()?);
        let key = key(&self.root, path);
        let rom = match self.files.get(key) {
            Some(c) if c.size == size && c.modified == modified => c.rom,
            _ => {
                let Files::Single(rom) = Game::read(platform.clone(), path)?.files;
                rom
            }
        };
        self.insert(key, size, modified, rom);
        Ok(Game {
            platform: platform.clone(),
            files: Files::Single(rom),
        })
    }

    /// Notes that the file at `path` holds `game` as it is now: for a file
    /// just written from a game whose hash is known.
    pub(crate) fn record(&mut self, path: &Path, game: &Game) -> io::Result<()> {
        let meta = fs::metadata(path)?;
        let Files::Single(rom) = game.files;
        self.insert(key(&self.root, path), meta.len(), meta.modified()?, rom);
        Ok(())
    }

    /// Notes that the file at `path` is gone.
    pub(crate) fn forget(&mut self, path: &Path) {
        self.files.remove(key(&self.root, path));
    }

    /// Forgets every file but those at `paths`, the ones a scan just found.
    pub(crate) fn keep_only(&mut self, paths: &HashSet<PathBuf>) {
        self.files
            .retain(|file, _| paths.contains(&self.root.join(file)));
    }

    fn insert(&mut self, path: &Path, size: u64, modified: SystemTime, rom: Rom) {
        let cached = Cached {
            size,
            modified,
            rom,
        };
        self.files.insert(path.to_path_buf(), cached);
    }
}

/// `path` from `root`, as the cache keeps it.
fn key<'a>(root: &Path, path: &'a Path) -> &'a Path {
    path.strip_prefix(root).unwrap_or(path)
}

/// What the tests of the modules that keep files on disk share.
#[cfg(test)]
pub(crate) mod testing {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;

    pub(crate) fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    /// Writes `bytes` to `path` under `root`, making its folders.
    pub(crate) fn put(root: &Path, path: &str, bytes: &[u8]) -> PathBuf {
        let full = root.join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(&full, bytes).unwrap();
        full
    }

    /// Like [`put`], last written at `secs`.
    pub(crate) fn put_at(root: &Path, path: &str, bytes: &[u8], secs: u64) -> PathBuf {
        let full = put(root, path, bytes);
        fs::File::options()
            .write(true)
            .open(&full)
            .unwrap()
            .set_modified(at(secs))
            .unwrap();
        full
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything in `dir`, hidden or not, sorted.
    fn all_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_replaced_file_leaves_nothing_beside_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Foo.gba");
        write(&path, b"old", None).unwrap();
        write(&path, b"new", None).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(all_in(dir.path()), ["Foo.gba"]);
    }

    #[test]
    fn a_file_left_by_a_dead_process_is_not_listed_and_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".Foo.gba.part"), b"half").unwrap();
        assert_eq!(list(dir.path()).unwrap().files, Vec::<String>::new());
        write(&dir.path().join("Foo.gba"), b"whole", None).unwrap();
        assert_eq!(all_in(dir.path()), ["Foo.gba"]);
    }

    #[test]
    fn a_failed_fill_leaves_nothing_and_keeps_the_old_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Foo.gba");
        fs::write(&path, b"old").unwrap();
        let failed = replace(
            &path,
            |tmp| {
                fs::write(tmp, b"half").unwrap();
                Err(BaseError::NoDevice)
            },
            None,
        );
        assert!(matches!(failed, Err(BaseError::NoDevice)));
        assert_eq!(fs::read(&path).unwrap(), b"old");
        assert_eq!(all_in(dir.path()), ["Foo.gba"]);
    }

    #[test]
    fn a_failed_rename_leaves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        // A folder with something in it can't be replaced by a file.
        let path = dir.path().join("Foo.gba");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("inside"), b"x").unwrap();
        assert!(write(&path, b"new", None).is_err());
        assert_eq!(all_in(dir.path()), ["Foo.gba"]);
    }
}
