//! A game and its saves.

use std::{
    io::{self, Read, Seek},
    path::Path,
    time::SystemTime,
};

use fs_err::{self as fs, File};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::platform::Platform;

/// Whether a game can be called `name` in a folder: it would otherwise be put
/// elsewhere, or be hidden and so not found again.
pub(crate) fn is_valid_file_name(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('.') && !name.contains(['/', '\\'])
}

/// Whether a game can be given `name` and be the same on a device: FAT file
/// systems don't take these characters, and macOS writes them as something else
/// rather than refusing them.
pub(crate) fn is_valid_fat_name(name: &str) -> bool {
    is_valid_file_name(name) && !name.contains([':', '?', '*', '"', '<', '>', '|'])
}

/// What the file `name` is called once renamed to `new`: it keeps its extension,
/// whether or not `new` has it already.
pub(crate) fn renamed_file_name(name: &str, new: &str) -> String {
    let new = new.trim();
    let Some(extension) = Path::new(name).extension().and_then(|e| e.to_str()) else {
        return new.to_string();
    };
    let dotted = format!(".{extension}").to_lowercase();
    let without = match new.to_lowercase().ends_with(&dotted) {
        true => &new[..new.len() - dotted.len()],
        false => new,
    };
    format!("{without}.{extension}")
}

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
        let rom = hash_rom(file)?;
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

    /// A save with these contents, written at `modified`.
    pub fn from_bytes(bytes: &[u8], modified: SystemTime) -> Self {
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
    pub fn has_same_contents_as(&self, other: &Self) -> bool {
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
    pub fn contains(&self, save: &Save) -> bool {
        self.iter().any(|s| s.has_same_contents_as(save))
    }
}

/// Reads the 16 bytes of an iNES header; whether that is what they were.
fn skip_ines_header(file: &mut File) -> io::Result<bool> {
    let mut start = Vec::new();
    file.take(16).read_to_end(&mut start)?;
    Ok(start.len() == 16 && start.starts_with(b"NES\x1a"))
}

/// The rest of `file`.
fn hash_rom(mut file: File) -> io::Result<Rom> {
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

    mod renaming {
        use super::*;

        #[test]
        fn keeps_extension() {
            assert_eq!(renamed_file_name("Old.gba", "New"), "New.gba");
            assert_eq!(renamed_file_name("Old", "New"), "New");
        }

        #[test]
        fn does_not_double_extension() {
            assert_eq!(renamed_file_name("Old.gba", "New.gba"), "New.gba");
            assert_eq!(renamed_file_name("Old.gba", "New.GBA"), "New.gba");
        }

        #[test]
        fn keeps_dots_in_name() {
            assert_eq!(renamed_file_name("Old.gb", "Dr. Mario"), "Dr. Mario.gb");
            assert_eq!(renamed_file_name("Old.gb", "Dr. Mario.gb"), "Dr. Mario.gb");
        }

        #[test]
        fn trims_spaces() {
            assert_eq!(renamed_file_name("Old.gba", "  New  "), "New.gba");
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

        #[test]
        fn rejects_names_with_path_or_leading_dot() {
            for name in ["../x.gba", "a/x.gba", "a\\x.gba", ".x.gba", ""] {
                assert!(!is_valid_file_name(name), "{name}");
            }
            assert!(is_valid_file_name("Dr. Mario.gb"));
        }

        #[test]
        fn rejects_characters_fat_cannot_hold() {
            for name in [
                "a:b.gba", "a?.gba", "a*.gba", "a\".gba", "a<.gba", "a>.gba", "a|.gba",
            ] {
                assert!(!is_valid_fat_name(name), "{name}");
            }
            assert!(!is_valid_fat_name("../x.gba"));
            assert!(is_valid_fat_name(
                "Legend of Zelda, The - It's (USA) [!].gba"
            ));
        }
    }

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
    fn reads_rom_size_and_crc32() {
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

    #[test]
    fn finds_versions_by_write_time() {
        let history = SaveHistory::from_saves(vec![
            Save::from_bytes(b"a", at(1)),
            Save::from_bytes(b"b", at(2)),
            Save::from_bytes(b"c", at(2)),
        ])
        .unwrap();
        assert_eq!(
            history.written_at("1970-01-01T00-00-01"),
            [&Save::from_bytes(b"a", at(1))]
        );
        assert_eq!(history.written_at("1970-01-01T00-00-02").len(), 2);
        assert_eq!(
            history.written_at("1970-01-01T00-00-09"),
            Vec::<&Save>::new()
        );
    }

    #[test]
    fn sorts_history_newest_first() {
        let history = SaveHistory::from_saves(vec![
            Save::from_bytes(b"a", at(1)),
            Save::from_bytes(b"b", at(3)),
            Save::from_bytes(b"c", at(2)),
        ])
        .unwrap();
        let times: Vec<_> = history.iter().map(|s| s.modified).collect();
        assert_eq!(times, [at(3), at(2), at(1)]);
        assert!(history.contains(&Save::from_bytes(b"c", at(9))));
        assert!(!history.contains(&Save::from_bytes(b"d", at(2))));
        assert_eq!(SaveHistory::from_saves(Vec::new()), None);
    }

    #[test]
    fn formats_save_time_in_utc() {
        let save = Save::from_bytes(b"", at(1_762_179_605));
        assert_eq!(save.timestamp(), "2025-11-03T14-20-05");
    }
}
