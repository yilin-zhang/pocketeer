//! A game and its saves.

use std::{
    fs::{self, File},
    io::{self, Read, Seek},
    path::Path,
    time::SystemTime,
};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::platform::Platform;

/// One file's contents, as ROM catalogues (No-Intro and Redump DATs) list
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rom {
    pub size: u64,
    pub crc32: u32,
}

/// What a game's files are.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Files {
    /// One ROM file.
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
    /// Reads the ROM at `path` once.
    pub fn read(platform: Platform, path: &Path) -> io::Result<Self> {
        let mut file = File::open(path)?;
        // No-Intro lists NES ROMs without the 16-byte iNES header the cores
        // need, so the file keeps it and the hash leaves it out.
        if platform == Platform::Nes && !skip_ines_header(&mut file)? {
            file.rewind()?;
        }
        let rom = hash(file)?;
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
        Ok(Save::of(&bytes, fs::metadata(path)?.modified()?))
    }

    /// A save with these contents, written at `modified`.
    pub fn of(bytes: &[u8], modified: SystemTime) -> Self {
        Self {
            size: bytes.len() as u64,
            crc32: crc32fast::hash(bytes),
            modified,
        }
    }

    /// When it was written, in UTC, as a file name can hold it, e.g.
    /// `2026-11-03T14-20-05`.
    pub fn timestamp(&self) -> String {
        let time = Timestamp::try_from(self.modified).unwrap_or(Timestamp::UNIX_EPOCH);
        time.strftime("%Y-%m-%dT%H-%M-%S").to_string()
    }

    /// Whether the two hold the same save, whenever each was written.
    pub fn same_contents(&self, other: &Self) -> bool {
        (self.size, self.crc32) == (other.size, other.crc32)
    }
}

/// A save as the library keeps it: the current backup and the ones it
/// replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveHistory {
    pub current: Save,
    /// Newest first.
    pub versions: Vec<Save>,
}

impl SaveHistory {
    /// `saves` in order, newest first; none if there are none.
    pub fn from_saves(mut saves: Vec<Save>) -> Option<Self> {
        saves.sort_by_key(|s| std::cmp::Reverse(s.modified));
        let mut saves = saves.into_iter();
        Some(Self {
            current: saves.next()?,
            versions: saves.collect(),
        })
    }

    /// Every version, newest first.
    pub fn iter(&self) -> impl Iterator<Item = &Save> {
        std::iter::once(&self.current).chain(&self.versions)
    }

    /// The versions written at `timestamp`, as [`Save::timestamp`] writes
    /// it: one, or none, or rarely several written in the same second.
    pub fn written_at(&self, timestamp: &str) -> Vec<&Save> {
        self.iter().filter(|s| s.timestamp() == timestamp).collect()
    }

    /// Whether some version holds the same save as `save`.
    pub fn has(&self, save: &Save) -> bool {
        self.iter().any(|s| s.same_contents(save))
    }
}

/// Reads the 16 bytes of an iNES header; whether that is what they were.
fn skip_ines_header(file: &mut File) -> io::Result<bool> {
    let mut start = Vec::new();
    file.take(16).read_to_end(&mut start)?;
    Ok(start.len() == 16 && start.starts_with(b"NES\x1a"))
}

/// The rest of `file`.
fn hash(mut file: File) -> io::Result<Rom> {
    let mut crc = crc32fast::Hasher::new();
    let mut size = 0;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        match file.read(&mut buf)? {
            0 => break,
            n => {
                crc.update(&buf[..n]);
                size += n as u64;
            }
        }
    }
    Ok(Rom {
        size,
        crc32: crc.finalize(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::testing::at;

    fn file(bytes: &[u8]) -> tempfile::NamedTempFile {
        let f = tempfile::NamedTempFile::new().unwrap();
        fs::write(f.path(), bytes).unwrap();
        f
    }

    fn rom(game: &Game) -> Rom {
        match game.files {
            Files::Single(rom) => rom,
        }
    }

    #[test]
    fn a_rom_is_its_size_and_crc32() {
        let f = file(b"hello");
        let game = Game::read(Platform::Gba, f.path()).unwrap();
        assert_eq!(
            rom(&game),
            Rom {
                size: 5,
                crc32: 0x3610a686
            }
        );
    }

    #[test]
    fn a_nes_rom_is_hashed_without_its_ines_header() {
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

    #[test]
    fn versions_are_found_by_when_they_were_written() {
        let history = SaveHistory::from_saves(vec![
            Save::of(b"a", at(1)),
            Save::of(b"b", at(2)),
            Save::of(b"c", at(2)),
        ])
        .unwrap();
        assert_eq!(
            history.written_at("1970-01-01T00-00-01"),
            [&Save::of(b"a", at(1))]
        );
        assert_eq!(history.written_at("1970-01-01T00-00-02").len(), 2);
        assert_eq!(
            history.written_at("1970-01-01T00-00-09"),
            Vec::<&Save>::new()
        );
    }

    #[test]
    fn a_history_is_newest_first() {
        let history = SaveHistory::from_saves(vec![
            Save::of(b"a", at(1)),
            Save::of(b"b", at(3)),
            Save::of(b"c", at(2)),
        ])
        .unwrap();
        let times: Vec<_> = history.iter().map(|s| s.modified).collect();
        assert_eq!(times, [at(3), at(2), at(1)]);
        assert!(history.has(&Save::of(b"c", at(9))));
        assert!(!history.has(&Save::of(b"d", at(2))));
        assert_eq!(SaveHistory::from_saves(Vec::new()), None);
    }

    #[test]
    fn a_save_time_is_written_in_utc() {
        let save = Save::of(b"", at(1_762_179_605));
        assert_eq!(save.timestamp(), "2025-11-03T14-20-05");
    }
}
