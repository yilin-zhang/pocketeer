//! Helpers for files on disk, shared by the device, the library and the DAT
//! mirror.

use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    time::SystemTime,
};

use fs_err as fs;
use serde::{Deserialize, Serialize};

use crate::{
    error::SharedError,
    game::{Files, Game, Rom},
    platform::Platform,
};

/// What is in a folder, by name, sorted.
#[derive(Default)]
pub(crate) struct Listing {
    pub(crate) dirs: Vec<String>,
    pub(crate) files: Vec<String>,
}

/// What is in `dir`, or nothing if it doesn't exist. Hidden entries (macOS
/// leaves `._` files on devices) and symlinks are left out.
pub(crate) fn list_dir(dir: &Path) -> Result<Listing, SharedError> {
    let read = match fs::read_dir(dir) {
        Ok(read) => read,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Listing::default()),
        Err(error) => return Err(error.into()),
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

/// Every visible file under `dir`, at any depth, as a path from `dir`.
pub(crate) fn list_files_recursive(dir: &Path) -> Result<Vec<PathBuf>, SharedError> {
    let Listing { dirs, files } = list_dir(dir)?;
    let mut paths: Vec<PathBuf> = files.into_iter().map(PathBuf::from).collect();
    for sub in dirs {
        for path in list_files_recursive(&dir.join(&sub))? {
            paths.push(Path::new(&sub).join(path));
        }
    }
    paths.sort();
    Ok(paths)
}

/// Replaces the file at `path` with `bytes`, last written at `modified` if
/// given.
pub(crate) fn write_atomically(
    path: &Path,
    bytes: &[u8],
    modified: Option<SystemTime>,
) -> Result<(), SharedError> {
    replace_atomically(path, |tmp| fs::write(tmp, bytes), modified)
}

/// Replaces the file at `to` with a copy of `from`, without holding all of
/// it in memory.
pub(crate) fn copy_atomically(from: &Path, to: &Path) -> Result<(), SharedError> {
    replace_atomically(to, |tmp| fs::copy(from, tmp).map(drop), None)
}

/// Renames each pair of old and new paths in turn. If one fails, the earlier
/// ones are put back, so either all are renamed or none, unless the process
/// dies on the way. Nothing is copied, so each path stays on its volume.
pub(crate) fn safe_rename_all(renames: &[(PathBuf, PathBuf)]) -> Result<(), SharedError> {
    for (i, (from, to)) in renames.iter().enumerate() {
        if let Err(error) = fs::rename(from, to) {
            for (from, to) in renames[..i].iter().rev() {
                let _ = fs::rename(to, from);
            }
            return Err(error.into());
        }
    }
    Ok(())
}

/// Replaces the file at `path` with what `fill` writes to a hidden file beside
/// it, so `path` is never half written.
///
/// If this fails the hidden file is removed. If the process dies first,
/// [`list_dir`] doesn't see it and the next write to `path` replaces it.
fn replace_atomically(
    path: &Path,
    fill: impl FnOnce(&Path) -> io::Result<()>,
    modified: Option<SystemTime>,
) -> Result<(), SharedError> {
    let dir = path.parent().expect("files are written into a folder");
    fs::create_dir_all(dir)?;
    let mut name = OsString::from(".");
    name.push(path.file_name().expect("files have a name"));
    name.push(".part");
    let tmp = path.with_file_name(name);
    let replaced = fill(&tmp)
        .and_then(|()| match modified {
            Some(modified) => fs::File::options()
                .write(true)
                .open(&tmp)
                .and_then(|file| file.set_modified(modified)),
            None => Ok(()),
        })
        .and_then(|()| fs::rename(&tmp, path));
    if replaced.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    Ok(replaced?)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Cached {
    /// The file's size. A NES ROM's [`Rom`] is smaller because it leaves out
    /// the header.
    size: u64,
    modified: SystemTime,
    rom: Rom,
}

/// What was read of a device's or library's games. Each file's [`Rom`] is kept
/// while its size and time stay the same. The cache only saves time, so one
/// that can't be read counts as empty and a failed write is ignored.
#[derive(Debug, Clone)]
pub(crate) struct Cache {
    /// The root of the device or library.
    root: PathBuf,
    /// The file it is kept in.
    path: PathBuf,
    /// Paths are relative to `root`, so entries hold wherever the device is
    /// mounted.
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
            let _ = write_atomically(&self.path, &json, None);
        }
    }

    /// The game at `path`, taken from this cache if its size and time are
    /// unchanged, otherwise read and recorded.
    pub(crate) fn read_game(
        &mut self,
        platform: &Platform,
        path: &Path,
    ) -> Result<Game, SharedError> {
        let meta = fs::metadata(path)?;
        let (size, modified) = (meta.len(), meta.modified()?);
        let key = key(&self.root, path);
        let rom = match self.files.get(key) {
            Some(cached) if cached.size == size && cached.modified == modified => cached.rom,
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

    /// Records that the file at `path` holds `game`. This is for a file just
    /// written from a game whose hash is known.
    pub(crate) fn record_game(&mut self, path: &Path, game: &Game) -> Result<(), SharedError> {
        let meta = fs::metadata(path)?;
        let Files::Single(rom) = game.files;
        self.insert(key(&self.root, path), meta.len(), meta.modified()?, rom);
        Ok(())
    }

    /// Records that the file at `path` is gone.
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

/// Helpers shared by the tests of modules that keep files on disk.
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
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn leaves_no_temp_file_after_replacing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Foo.gba");
        write_atomically(&path, b"old", None).unwrap();
        write_atomically(&path, b"new", None).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(all_in(dir.path()), ["Foo.gba"]);
    }

    #[test]
    fn hides_and_replaces_temp_file_of_dead_process() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".Foo.gba.part"), b"half").unwrap();
        assert_eq!(list_dir(dir.path()).unwrap().files, Vec::<String>::new());
        write_atomically(&dir.path().join("Foo.gba"), b"whole", None).unwrap();
        assert_eq!(all_in(dir.path()), ["Foo.gba"]);
    }

    #[test]
    fn keeps_old_file_and_removes_temp_after_failed_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Foo.gba");
        fs::write(&path, b"old").unwrap();
        let failed = replace_atomically(
            &path,
            |tmp| {
                fs::write(tmp, b"half").unwrap();
                Err(io::Error::other("full"))
            },
            None,
        );
        assert!(matches!(failed, Err(SharedError::Io(_))));
        assert_eq!(fs::read(&path).unwrap(), b"old");
        assert_eq!(all_in(dir.path()), ["Foo.gba"]);
    }
}
