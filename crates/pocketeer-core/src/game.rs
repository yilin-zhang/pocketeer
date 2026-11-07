//! A game and its saves.

use std::{
    collections::{HashMap, HashSet},
    io::{self, Read, Seek},
    path::{Path, PathBuf},
    time::SystemTime,
};

use fs_err::{self as fs, File};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::{error::BasicError, os::write_atomically, platform::Platform};

/// The file name after renaming `name` to `new`. The extension stays, even if
/// `new` already has it.
pub(crate) fn renamed_file_name(name: &str, new: &str) -> String {
    let new = new.trim();
    let Some(extension) = Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
    else {
        return new.to_string();
    };
    let dotted = format!(".{extension}").to_lowercase();
    let without = match new.to_lowercase().ends_with(&dotted) {
        true => &new[..new.len() - dotted.len()],
        false => new,
    };
    format!("{without}.{extension}")
}

/// One file's contents, as ROM catalogs (No-Intro and Redump DATs) list
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rom {
    pub size: u64,
    pub crc32: u32,
}

impl Rom {
    /// Hashes the rest of `file`.
    fn read(mut file: File) -> io::Result<Self> {
        let mut crc = crc32fast::Hasher::new();
        let mut size = 0;
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            match file.read(&mut buf)? {
                0 => break,
                read_count => {
                    crc.update(&buf[..read_count]);
                    size += read_count as u64;
                }
            }
        }
        Ok(Self {
            size,
            crc32: crc.finalize(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Files {
    Single(Rom),
    // Games made of several files (an instance JSON and its data files, as
    // CD games are) will be another variant.
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Game {
    pub platform: Platform,
    pub files: Files,
}

impl Game {
    /// Hashes the ROM at `path` without holding all of it in memory.
    pub fn read(platform: Platform, path: &Path) -> io::Result<Self> {
        let mut file = File::open(path)?;
        // No-Intro lists NES ROMs without the 16-byte iNES header the cores
        // need, so the file keeps it and the hash leaves it out.
        if platform == Platform::Nes && !skip_ines_header(&mut file)? {
            file.rewind()?;
        }
        let rom = Rom::read(file)?;
        Ok(Self {
            platform,
            files: Files::Single(rom),
        })
    }
}

/// One save file's contents and when it was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Save {
    pub size: u64,
    pub crc32: u32,
    pub modified: SystemTime,
}

impl Save {
    /// Reads the save at `path`. Saves are small, so this reads all of it.
    pub fn read(path: &Path) -> io::Result<Self> {
        let bytes = fs::read(path)?;
        Ok(Save::from_bytes(&bytes, fs::metadata(path)?.modified()?))
    }

    /// Reads the save at `path`, or gives none if there is no file there.
    pub fn read_if_exists(path: &Path) -> io::Result<Option<Self>> {
        match Save::read(path) {
            Ok(save) => Ok(Some(save)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn from_bytes(bytes: &[u8], modified: SystemTime) -> Self {
        Self {
            size: bytes.len() as u64,
            crc32: crc32fast::hash(bytes),
            modified,
        }
    }

    /// When it was written, in UTC, in a form a file name can hold, e.g.
    /// `2026-11-03T14-20-05`.
    pub fn timestamp(&self) -> String {
        let time = Timestamp::try_from(self.modified).unwrap_or(Timestamp::UNIX_EPOCH);
        time.strftime("%Y-%m-%dT%H-%M-%S").to_string()
    }

    /// Whether the two hold the same save, whenever each was written.
    pub fn has_same_contents_as(&self, other: &Self) -> bool {
        (self.size, self.crc32) == (other.size, other.crc32)
    }
}

/// Skips the 16 bytes of an iNES header and returns whether that is what they
/// were.
fn skip_ines_header(file: &mut File) -> io::Result<bool> {
    let mut start = Vec::new();
    file.take(16).read_to_end(&mut start)?;
    Ok(start.len() == 16 && start.starts_with(b"NES\x1a"))
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
pub(crate) struct RomCache {
    /// The root of the device or library.
    root: PathBuf,
    /// The file it is kept in.
    path: PathBuf,
    /// Paths are relative to `root`, so entries hold wherever the device is
    /// mounted.
    files: HashMap<PathBuf, Cached>,
}

impl RomCache {
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
    ) -> Result<Game, BasicError> {
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
    pub(crate) fn record_game(&mut self, path: &Path, game: &Game) -> Result<(), BasicError> {
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

#[cfg(test)]
mod tests {
    use super::*;

    mod renaming {
        use super::*;
        use crate::os::is_valid_file_name;

        #[test]
        fn keeps_old_extension_in_new_file_name() {
            for (old, new, expected) in [
                ("Old.gba", "New", "New.gba"),
                ("Old", "New", "New"),
                ("Old.gba", "New.gba", "New.gba"),
                ("Old.gba", "New.GBA", "New.gba"),
                ("Old.gb", "Dr. Mario", "Dr. Mario.gb"),
                ("Old.gb", "Dr. Mario.gb", "Dr. Mario.gb"),
                ("Old.gba", "  New  ", "New.gba"),
            ] {
                assert_eq!(renamed_file_name(old, new), expected, "{old} to {new}");
            }
        }

        #[test]
        fn rejects_empty_name_after_removing_extension() {
            for new in ["", "  ", ".gba", ".GBA"] {
                assert!(
                    !is_valid_file_name(&renamed_file_name("Old.gba", new)),
                    "{new}"
                );
            }
        }
    }

    fn file(bytes: &[u8]) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(file.path(), bytes).unwrap();
        file
    }

    fn rom(game: &Game) -> Rom {
        match game.files {
            Files::Single(rom) => rom,
        }
    }

    #[test]
    fn reads_rom_size_and_crc32() {
        let file = file(b"hello");
        let game = Game::read(Platform::Gba, file.path()).unwrap();
        assert_eq!(
            rom(&game),
            Rom {
                size: 5,
                crc32: 0x3610a686
            }
        );
    }

    #[test]
    fn hashes_nes_rom_without_ines_header() {
        let mut bytes = b"NES\x1a".to_vec();
        bytes.extend([0u8; 12]);
        bytes.extend(b"hello");
        let hello = Rom {
            size: 5,
            crc32: 0x3610a686,
        };
        let headed = file(&bytes);
        assert_eq!(
            rom(&Game::read(Platform::Nes, headed.path()).unwrap()),
            hello
        );
        // Without a header, all of it is the ROM.
        let bare = file(b"hello");
        assert_eq!(rom(&Game::read(Platform::Nes, bare.path()).unwrap()), hello);
    }
}
