//! The library on the computer, where pocketeer keeps games, their covers
//! and their saves.

use std::{
    collections::{HashMap, HashSet, hash_map},
    fs,
    path::{Path, PathBuf},
};

use image::ImageFormat;

use crate::{
    cover::{COVER_FORMATS, Cover},
    error::{BaseError, io_at},
    files::{Cache, copy, list, write},
    game::{Game, Save, SaveHistory},
    platform::Platform,
};

/// One game in the library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalEntry {
    /// The game's name, which is also its file name: on the device a save is
    /// found by the name of the game's file.
    pub name: String,
    pub game: Game,
    /// Where its cover is, if it has one. A path rather than the cover:
    /// covers are large and only a few are shown at a time, so each is read
    /// when it is needed, by whoever needs it, without going through the
    /// library.
    pub cover_path: Option<PathBuf>,
    pub save: Option<SaveHistory>,
}

impl LocalEntry {
    fn is_called(&self, platform: &Platform, name: &str) -> bool {
        &self.game.platform == platform && self.name == name
    }

    /// The name without its extension.
    pub fn stem(&self) -> &str {
        stem(&self.name)
    }
}

/// Saves in the library with no game of that name: a game put in the trash
/// leaves them, and a game put there under that name has them again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanLocalSave {
    pub platform: Platform,
    /// The name its game had, without the extension.
    pub stem: String,
    pub save: SaveHistory,
}

/// Why a game wasn't added to the library.
#[derive(Debug, thiserror::Error)]
pub enum AddGameError {
    /// Another game of the platform has the name, whatever its extension or
    /// case: saves and covers are kept by the name without the extension, and
    /// a file system may not tell cases apart.
    #[error("another game in the library has its name")]
    NameTaken,
    /// The name can't be a file's in the library: it would be put elsewhere,
    /// or hidden and so not found again.
    #[error("the library can't hold a game of that name")]
    BadName,
    #[error(transparent)]
    Other(#[from] BaseError),
}

/// Where everything is in a library folder: every path in it is spelled
/// out here.
#[derive(Debug, Clone)]
pub(crate) struct LibraryLayout {
    root: PathBuf,
}

impl LibraryLayout {
    pub(crate) fn dats(&self) -> PathBuf {
        self.root.join("dats")
    }

    /// Which cores can be downloaded.
    pub(crate) fn core_inventory(&self) -> PathBuf {
        self.root.join("cores.json")
    }

    /// Which covers can be downloaded.
    pub(crate) fn cover_index(&self) -> PathBuf {
        self.root.join("covers.json")
    }

    fn roms(&self) -> PathBuf {
        self.root.join("roms")
    }

    /// The folder of a platform's games.
    fn platform_roms(&self, platform: &Platform) -> PathBuf {
        self.roms().join(platform.id())
    }

    fn rom(&self, platform: &Platform, name: &str) -> PathBuf {
        self.platform_roms(platform).join(name)
    }

    fn cover(&self, platform: &Platform, stem: &str, format: ImageFormat) -> PathBuf {
        let file = format!("{stem}.{}", format.extensions_str()[0]);
        self.root.join("covers").join(platform.id()).join(file)
    }

    fn all_saves(&self) -> PathBuf {
        self.root.join("saves")
    }

    /// The folder of the saves of a platform's games.
    fn platform_saves(&self, platform: &Platform) -> PathBuf {
        self.all_saves().join(platform.id())
    }

    /// The folder of a game's save versions, by its name without the
    /// extension as on the Pocket.
    fn saves(&self, platform: &Platform, stem: &str) -> PathBuf {
        self.platform_saves(platform).join(stem)
    }

    /// The `n`th file for `save`, a version of a game's save: they are named
    /// after when the Pocket wrote them, and a second written in the same
    /// second is `-2`.
    fn save_version(&self, platform: &Platform, stem: &str, save: &Save, n: u32) -> PathBuf {
        let file = match n {
            1 => format!("{}.sav", save.timestamp()),
            n => format!("{}-{n}.sav", save.timestamp()),
        };
        self.saves(platform, stem).join(file)
    }

    /// What the front ends did.
    pub(crate) fn log(&self) -> PathBuf {
        self.root.join("pocketeer.log")
    }

    fn cache(&self) -> PathBuf {
        self.root.join("cache.json")
    }
}

pub struct Library {
    layout: LibraryLayout,
    cache: Cache,
    /// By platform and name.
    entries: Vec<LocalEntry>,
}

impl Library {
    /// The library at `root`, a folder that must exist, with every game in
    /// it, each once.
    pub fn open(root: &Path) -> Result<Self, BaseError> {
        if !root.is_dir() {
            return Err(BaseError::NoLibrary {
                path: root.to_path_buf(),
            });
        }
        let layout = LibraryLayout {
            root: root.to_path_buf(),
        };
        let roms = layout.roms();
        fs::create_dir_all(&roms).map_err(io_at(&roms))?;
        let cache = Cache::load(&layout.root, &layout.cache());
        let mut library = Self {
            layout,
            cache,
            entries: Vec::new(),
        };
        library.reload()?;
        Ok(library)
    }

    pub fn root(&self) -> &Path {
        &self.layout.root
    }

    pub fn entries(&self) -> &[LocalEntry] {
        &self.entries
    }

    /// The game of `platform` called `name`.
    pub fn entry(&self, platform: &Platform, name: &str) -> Option<&LocalEntry> {
        self.entries.iter().find(|e| e.is_called(platform, name))
    }

    /// The library's copy of `game`, under whatever name.
    pub fn entry_for(&self, game: &Game) -> Option<&LocalEntry> {
        self.entries.iter().find(|e| &e.game == game)
    }

    pub(crate) fn layout(&self) -> &LibraryLayout {
        &self.layout
    }

    /// Reads the folder again, for changes made outside pocketeer. A game
    /// whose file is unchanged since the last time is not read again;
    /// covers are not read at all.
    pub fn reload(&mut self) -> Result<(), BaseError> {
        self.entries = self.scan()?;
        Ok(())
    }

    /// Where `entry`'s file is.
    pub fn game_path(&self, entry: &LocalEntry) -> PathBuf {
        self.layout.rom(&entry.game.platform, &entry.name)
    }

    /// The files of `entry` as a game: its file and its cover, if it has
    /// one. Not its saves, which are kept by its name apart from it.
    pub fn files_of(&self, entry: &LocalEntry) -> Vec<PathBuf> {
        let mut files = vec![self.game_path(entry)];
        files.extend(entry.cover_path.clone());
        files
    }

    /// The saves with no game of their name, as the folders are now.
    pub fn orphan_saves(&self) -> Result<Vec<OrphanLocalSave>, BaseError> {
        let all = self.layout.all_saves();
        let mut orphans = Vec::new();
        for id in list(&all).map_err(io_at(&all))?.dirs {
            let platform = Platform::from_id(&id);
            let folder = self.layout.platform_saves(&platform);
            for name in list(&folder).map_err(io_at(&folder))?.dirs {
                let has_game = |e: &LocalEntry| e.game.platform == platform && e.stem() == name;
                if self.entries.iter().any(has_game) {
                    continue;
                }
                if let Some(save) = self.saves(&platform, &name)? {
                    orphans.push(OrphanLocalSave {
                        platform: platform.clone(),
                        stem: name,
                        save,
                    });
                }
            }
        }
        Ok(orphans)
    }

    /// The file of every version of `entry`'s save.
    pub fn all_save_files(&self, entry: &LocalEntry) -> Result<Vec<PathBuf>, BaseError> {
        let files = self.save_files(&entry.game.platform, entry.stem())?;
        Ok(files.into_iter().map(|(path, _)| path).collect())
    }

    /// The file of every version of `orphan`'s save, if it is still an orphan:
    /// a game may have been put under its name since. None if it isn't.
    pub(crate) fn orphan_save_files(
        &self,
        orphan: &OrphanLocalSave,
    ) -> Result<Vec<PathBuf>, BaseError> {
        let is_it = |o: &OrphanLocalSave| o.platform == orphan.platform && o.stem == orphan.stem;
        if !self.orphan_saves()?.iter().any(is_it) {
            return Ok(Vec::new());
        }
        let files = self.save_files(&orphan.platform, &orphan.stem)?;
        Ok(files.into_iter().map(|(path, _)| path).collect())
    }

    /// Reads the saves of the game called `name` again, once some are gone.
    pub(crate) fn reread_saves(
        &mut self,
        platform: &Platform,
        name: &str,
    ) -> Result<(), BaseError> {
        let saves = self.saves(platform, stem(name))?;
        let i = self.index_of(platform, name)?;
        self.entries[i].save = saves;
        Ok(())
    }

    /// Copies the file at `from`, which holds `game`, into the library as the
    /// game called `name`, unless another game of the platform has that name,
    /// whatever its extension or case, or a file is there already: this never
    /// replaces one. The copy isn't read: `game` is what is noted of it.
    pub(crate) fn add_game(
        &mut self,
        game: &Game,
        name: &str,
        from: &Path,
    ) -> Result<LocalEntry, AddGameError> {
        if name.is_empty() || name.starts_with('.') || name.contains(['/', '\\']) {
            return Err(AddGameError::BadName);
        }
        let platform = &game.platform;
        let lowercase = stem(name).to_lowercase();
        let taken =
            |e: &LocalEntry| &e.game.platform == platform && e.stem().to_lowercase() == lowercase;
        let to = self.layout.rom(platform, name);
        if self.entries.iter().any(taken) || to.exists() {
            return Err(AddGameError::NameTaken);
        }
        copy(from, &to)?;
        self.cache.record(&to, game).map_err(io_at(&to))?;
        self.cache.save();
        let entry = LocalEntry {
            name: name.into(),
            cover_path: self.cover_path(platform, stem(name)),
            save: self.saves(platform, stem(name))?,
            game: game.clone(),
        };
        let place = self.entries.binary_search_by(|e| {
            (e.game.platform.id(), &e.name).cmp(&(entry.game.platform.id(), &entry.name))
        });
        let (Ok(at) | Err(at)) = place;
        self.entries.insert(at, entry.clone());
        Ok(entry)
    }

    /// Adds the save at `from` to the game called `name`, unless it has one
    /// with the same contents already; the new version, if it was added.
    pub(crate) fn add_save(
        &mut self,
        platform: &Platform,
        name: &str,
        from: &Path,
    ) -> Result<Option<Save>, BaseError> {
        let i = self.index_of(platform, name)?;
        let modified = fs::metadata(from)
            .and_then(|m| m.modified())
            .map_err(io_at(from))?;
        let bytes = fs::read(from).map_err(io_at(from))?;
        let save = Save::of(&bytes, modified);
        if self.entries[i].save.as_ref().is_some_and(|h| h.has(&save)) {
            return Ok(None);
        }
        let path = (1..)
            .map(|n| self.layout.save_version(platform, stem(name), &save, n))
            .find(|path| !path.exists())
            .expect("some name is free");
        write(&path, &bytes, Some(modified))?;
        let entry = &mut self.entries[i];
        let mut saves: Vec<Save> = entry.save.iter().flat_map(|h| h.iter().cloned()).collect();
        saves.push(save.clone());
        entry.save = SaveHistory::from_saves(saves);
        Ok(Some(save))
    }

    /// Leaves the game called `name` out of the entries and the cache, once
    /// its files are gone.
    pub(crate) fn forget(&mut self, platform: &Platform, name: &str) {
        self.cache.forget(&self.layout.rom(platform, name));
        self.cache.save();
        self.entries.retain(|e| !e.is_called(platform, name));
    }

    /// Makes `cover` the cover of the game called `name`, which has none.
    pub(crate) fn add_cover(
        &mut self,
        platform: &Platform,
        name: &str,
        cover: &Cover,
    ) -> Result<(), BaseError> {
        let i = self.index_of(platform, name)?;
        let path = self.layout.cover(platform, stem(name), cover.format());
        write(&path, cover.bytes(), None)?;
        self.entries[i].cover_path = Some(path);
        Ok(())
    }

    /// The file of a version of `entry`'s save; none if it has no such
    /// version.
    pub fn save_file(&self, entry: &LocalEntry, save: &Save) -> Result<Option<PathBuf>, BaseError> {
        Ok(self
            .save_files(&entry.game.platform, entry.stem())?
            .into_iter()
            .find(|(_, s)| s == save)
            .map(|(path, _)| path))
    }

    fn index_of(&self, platform: &Platform, name: &str) -> Result<usize, BaseError> {
        self.entries
            .iter()
            .position(|e| e.is_called(platform, name))
            .ok_or(BaseError::NotInLibrary)
    }

    fn scan(&mut self) -> Result<Vec<LocalEntry>, BaseError> {
        let mut seen = HashSet::new();
        let mut entries = Vec::new();
        let roms = self.layout.roms();
        for id in list(&roms).map_err(io_at(&roms))?.dirs {
            let platform = Platform::from_id(&id);
            let dir = self.layout.platform_roms(&platform);
            for name in list(&dir).map_err(io_at(&dir))?.files {
                let path = self.layout.rom(&platform, &name);
                let game = self.cache.read(&platform, &path).map_err(io_at(&path))?;
                seen.insert(path);
                let cover_path = self.cover_path(&platform, stem(&name));
                let save = self.saves(&platform, stem(&name))?;
                entries.push(LocalEntry {
                    name,
                    game,
                    cover_path,
                    save,
                });
            }
        }
        self.cache.keep_only(&seen);
        self.cache.save();
        self.refuse_same_game_twice(&entries)?;
        Ok(entries)
    }

    /// A game's save is kept under one name, so a second file of it would
    /// never get one. Only files put in the folder by hand get this far.
    fn refuse_same_game_twice(&self, entries: &[LocalEntry]) -> Result<(), BaseError> {
        let mut firsts = HashMap::new();
        let mut copies = Vec::new();
        for entry in entries {
            match firsts.entry(&entry.game) {
                hash_map::Entry::Vacant(first) => {
                    first.insert(entry);
                }
                hash_map::Entry::Occupied(first) => {
                    copies.push((self.game_path(first.get()), self.game_path(entry)));
                }
            }
        }
        if !copies.is_empty() {
            return Err(BaseError::SameGameInLibrary { copies });
        }
        Ok(())
    }

    fn cover_path(&self, platform: &Platform, stem: &str) -> Option<PathBuf> {
        COVER_FORMATS
            .into_iter()
            .map(|format| self.layout.cover(platform, stem, format))
            .find(|path| path.is_file())
    }

    /// Every version of the game's save, newest first; none if it has none.
    fn saves(&self, platform: &Platform, stem: &str) -> Result<Option<SaveHistory>, BaseError> {
        let saves = self.save_files(platform, stem)?.into_iter().map(|(_, s)| s);
        Ok(SaveHistory::from_saves(saves.collect()))
    }

    /// The game's save files, each with what it holds.
    fn save_files(
        &self,
        platform: &Platform,
        stem: &str,
    ) -> Result<Vec<(PathBuf, Save)>, BaseError> {
        let dir = self.layout.saves(platform, stem);
        let mut files = Vec::new();
        for name in list(&dir).map_err(io_at(&dir))?.files {
            if Path::new(&name).extension().is_some_and(|e| e == "sav") {
                let path = dir.join(&name);
                let save = Save::read(&path).map_err(io_at(&path))?;
                files.push((path, save));
            }
        }
        Ok(files)
    }
}

fn stem(name: &str) -> &str {
    Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::testing::{at, put, put_at};
    use crate::game::{Files, Rom};

    /// An empty library; tests put files in it and then reload.
    fn library() -> (tempfile::TempDir, Library) {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path()).unwrap();
        (dir, library)
    }

    /// A 1×1 PNG.
    const PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    /// Adds the file at `from`, read to tell what game it holds.
    fn add(
        library: &mut Library,
        platform: &Platform,
        name: &str,
        from: &Path,
    ) -> Result<LocalEntry, AddGameError> {
        let game = Game::read(platform.clone(), from).unwrap();
        library.add_game(&game, name, from)
    }

    mod opening {
        use super::*;

        #[test]
        fn a_folder_that_does_not_exist_is_not_made() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("typo");
            assert!(matches!(
                Library::open(&root),
                Err(BaseError::NoLibrary { .. })
            ));
            assert!(!root.exists());
        }

        #[test]
        fn the_same_game_twice_is_refused() {
            let dir = tempfile::tempdir().unwrap();
            let first = put(dir.path(), "roms/gba/Mine.gba", b"same");
            let second = put(dir.path(), "roms/gba/Other.gba", b"same");
            let Err(BaseError::SameGameInLibrary { copies }) = Library::open(dir.path()) else {
                panic!()
            };
            assert_eq!(copies, [(first, second)]);
        }

        #[test]
        fn a_reload_that_finds_it_twice_keeps_what_it_had() {
            let (_dir, mut library) = library();
            put(library.root(), "roms/gba/Mine.gba", b"same");
            library.reload().unwrap();
            put(library.root(), "roms/gba/Other.gba", b"same");
            assert!(matches!(
                library.reload(),
                Err(BaseError::SameGameInLibrary { .. })
            ));
            assert_eq!(library.entries().len(), 1);
        }

        #[test]
        fn the_same_contents_on_another_platform_are_another_game() {
            let dir = tempfile::tempdir().unwrap();
            put(dir.path(), "roms/gb/Tetris.gb", b"same");
            put(dir.path(), "roms/gbc/Tetris.gbc", b"same");
            assert_eq!(Library::open(dir.path()).unwrap().entries().len(), 2);
        }

        #[test]
        fn a_game_is_found_with_its_cover_and_saves() {
            let (_dir, mut library) = library();
            put(library.root(), "roms/gba/Tetris.gba", b"hello");
            put(library.root(), "covers/gba/Tetris.png", PNG);
            put_at(library.root(), "saves/gba/Tetris/old.sav", b"a", 1_000);
            put_at(library.root(), "saves/gba/Tetris/new.sav", b"bb", 2_000);
            library.reload().unwrap();
            let entries = library.entries();
            assert_eq!(entries.len(), 1);
            let entry = &entries[0];
            assert_eq!(entry.name, "Tetris.gba");
            assert_eq!(
                entry.game,
                Game {
                    platform: Platform::Gba,
                    files: Files::Single(Rom {
                        size: 5,
                        crc32: 0x3610a686
                    }),
                }
            );
            let cover_path = entry.cover_path.as_ref().unwrap();
            assert_eq!(cover_path, &library.root().join("covers/gba/Tetris.png"));
            assert_eq!(fs::read(cover_path).unwrap(), PNG);
            let history = entry.save.as_ref().unwrap();
            assert_eq!(history.current.size, 2);
            assert_eq!(history.versions.len(), 1);
            assert_eq!(history.versions[0].size, 1);
        }

        #[test]
        fn only_sav_files_are_saves() {
            let (_dir, mut library) = library();
            put(library.root(), "roms/gba/Tetris.gba", b"hello");
            put(library.root(), "saves/gba/Tetris/notes.txt", b"x");
            put(library.root(), "saves/gba/Tetris/._a.sav", b"x");
            library.reload().unwrap();
            assert_eq!(library.entries()[0].save, None);
        }

        #[test]
        fn a_forgotten_game_is_not_answered_for_by_the_cache() {
            let (_dir, mut library) = library();
            let path = "roms/gba/Tetris.gba";
            let file = put_at(library.root(), path, b"hello", 1_000_000);
            library.reload().unwrap();
            fs::remove_file(&file).unwrap();
            library.forget(&Platform::Gba, "Tetris.gba");
            assert_eq!(library.entries(), []);
            // The same size and time as the file removed.
            put_at(library.root(), path, b"jello", 1_000_000);
            library.reload().unwrap();
            assert_eq!(
                library.entries()[0].game.files,
                Files::Single(Rom {
                    size: 5,
                    crc32: 0x4cd0f5e6
                })
            );
        }
    }

    mod add_game {
        use super::*;

        #[test]
        fn not_under_a_name_differing_only_by_its_extension_or_case() {
            let (_dir, mut library) = library();
            let kept = put(library.root(), "roms/gba/Tetris.gba", b"kept");
            library.reload().unwrap();
            let elsewhere = tempfile::tempdir().unwrap();
            let from = put(elsewhere.path(), "x.gba", b"other");
            for name in ["Tetris.agb", "tetris.gba"] {
                let added = add(&mut library, &Platform::Gba, name, &from);
                assert!(matches!(added, Err(AddGameError::NameTaken)), "{name}");
            }
            assert!(!library.root().join("roms/gba/Tetris.agb").exists());
            assert_eq!(fs::read(&kept).unwrap(), b"kept");
            assert_eq!(library.entries().len(), 1);
        }

        #[test]
        fn a_file_put_there_since_the_last_scan_is_not_replaced() {
            let (_dir, mut library) = library();
            let kept = put(library.root(), "roms/gba/Tetris.gba", b"kept");
            let elsewhere = tempfile::tempdir().unwrap();
            let from = put(elsewhere.path(), "x.gba", b"other");
            let added = add(&mut library, &Platform::Gba, "Tetris.gba", &from);
            assert!(matches!(added, Err(AddGameError::NameTaken)));
            assert_eq!(fs::read(&kept).unwrap(), b"kept");
        }

        #[test]
        fn another_platform_can_have_the_same_name() {
            let (_dir, mut library) = library();
            put(library.root(), "roms/gba/Foo.gba", b"gba");
            library.reload().unwrap();
            let elsewhere = tempfile::tempdir().unwrap();
            let from = put(elsewhere.path(), "x.gb", b"gb");
            add(&mut library, &Platform::Gb, "Foo.gb", &from).unwrap();
            assert_eq!(library.entries().len(), 2);
        }

        #[test]
        fn not_under_a_name_that_leads_elsewhere_or_is_hidden() {
            let (_dir, mut library) = library();
            let elsewhere = tempfile::tempdir().unwrap();
            let from = put(elsewhere.path(), "x.gba", b"other");
            for name in ["../x.gba", "a/x.gba", "a\\x.gba", ".x.gba", "..", ""] {
                let added = add(&mut library, &Platform::Gba, name, &from);
                assert!(matches!(added, Err(AddGameError::BadName)), "{name}");
            }
            assert!(!library.root().join("roms").join("gba").exists());
            assert!(!library.root().join("x.gba").exists());
        }

        #[test]
        fn it_is_not_read_back() {
            let (_dir, mut library) = library();
            let elsewhere = tempfile::tempdir().unwrap();
            let from = put(elsewhere.path(), "x.gba", b"hello");
            // Not what the file holds: only what is given can be noted.
            let game = Game {
                platform: Platform::Gba,
                files: Files::Single(Rom { size: 1, crc32: 1 }),
            };
            let added = library.add_game(&game, "Tetris.gba", &from).unwrap();
            assert_eq!(added.game, game);
            // Nor is it read when the library is opened again.
            let root = library.root().to_path_buf();
            let reopened = Library::open(&root).unwrap();
            assert_eq!(reopened.entries()[0].game, game);
        }

        #[test]
        fn in_order() {
            let (_dir, mut library) = library();
            put(library.root(), "roms/gba/B.gba", b"b");
            put(library.root(), "roms/gba/D.gba", b"d");
            library.reload().unwrap();
            let elsewhere = tempfile::tempdir().unwrap();
            let from = put(elsewhere.path(), "x.gba", b"hello");
            let added = add(&mut library, &Platform::Gba, "C.gba", &from).unwrap();
            assert_eq!(
                added.game.files,
                Files::Single(Rom {
                    size: 5,
                    crc32: 0x3610a686
                })
            );
            assert_eq!(
                library.game_path(&added),
                library.root().join("roms/gba/C.gba")
            );
            let names = |l: &Library| {
                l.entries()
                    .iter()
                    .map(|e| e.name.clone())
                    .collect::<Vec<_>>()
            };
            assert_eq!(names(&library), ["B.gba", "C.gba", "D.gba"]);
            let in_memory = library.entries().to_vec();
            library.reload().unwrap();
            assert_eq!(library.entries(), in_memory);
        }
    }

    mod add_save {
        use super::*;

        /// A library with the game Tetris.
        fn with_tetris() -> (tempfile::TempDir, Library) {
            let (dir, mut library) = library();
            put(library.root(), "roms/gba/Tetris.gba", b"hello");
            library.reload().unwrap();
            (dir, library)
        }

        /// Adds a file holding `bytes`, written at `secs`, as a save of Tetris.
        fn add_save(
            library: &mut Library,
            bytes: &[u8],
            secs: u64,
        ) -> Result<Option<Save>, BaseError> {
            let elsewhere = tempfile::tempdir().unwrap();
            let from = put_at(elsewhere.path(), "Tetris.sav", bytes, secs);
            library.add_save(&Platform::Gba, "Tetris.gba", &from)
        }

        #[test]
        fn under_its_time() {
            let (_dir, mut library) = with_tetris();
            let added = add_save(&mut library, b"save", 1_762_179_605).unwrap();
            assert_eq!(added.unwrap().size, 4);
            let file = library
                .root()
                .join("saves/gba/Tetris/2025-11-03T14-20-05.sav");
            assert_eq!(fs::read(&file).unwrap(), b"save");
            assert_eq!(
                fs::metadata(&file).unwrap().modified().unwrap(),
                at(1_762_179_605)
            );
            // In memory as it is on disk.
            let in_memory = library.entries()[0].save.clone();
            library.reload().unwrap();
            assert_eq!(library.entries()[0].save, in_memory);
            assert_eq!(in_memory.unwrap().current.size, 4);
        }

        #[test]
        fn one_already_kept_is_not_added_again() {
            let (_dir, mut library) = with_tetris();
            add_save(&mut library, b"save", 1_000).unwrap();
            assert_eq!(add_save(&mut library, b"save", 2_000).unwrap(), None);
            assert_eq!(library.entries()[0].save.as_ref().unwrap().versions, []);
        }

        #[test]
        fn two_of_the_same_second_are_both_kept() {
            let (_dir, mut library) = with_tetris();
            add_save(&mut library, b"one", 1_000).unwrap();
            add_save(&mut library, b"two", 1_000).unwrap();
            let dir = library.root().join("saves/gba/Tetris");
            assert_eq!(
                fs::read(dir.join("1970-01-01T00-16-40.sav")).unwrap(),
                b"one"
            );
            assert_eq!(
                fs::read(dir.join("1970-01-01T00-16-40-2.sav")).unwrap(),
                b"two"
            );
        }

        #[test]
        fn for_a_game_not_in_the_library_is_an_error() {
            let (_dir, mut library) = library();
            assert!(matches!(
                add_save(&mut library, b"save", 1_000),
                Err(BaseError::NotInLibrary)
            ));
        }

        #[test]
        fn a_kept_version_is_found_by_what_it_holds() {
            let (_dir, mut library) = with_tetris();
            let save = add_save(&mut library, b"save", 1_000).unwrap().unwrap();
            let entry = &library.entries()[0];
            assert_eq!(
                library.save_file(entry, &save).unwrap(),
                Some(
                    library
                        .root()
                        .join("saves/gba/Tetris/1970-01-01T00-16-40.sav")
                )
            );
            let other = Save::of(b"other", at(0));
            assert_eq!(library.save_file(entry, &other).unwrap(), None);
        }
    }

    mod files {
        use super::*;

        #[test]
        fn a_cover_is_added_where_a_reload_finds_it() {
            let (_dir, mut library) = library();
            put(library.root(), "roms/gba/Tetris.gba", b"hello");
            library.reload().unwrap();
            let cover = Cover::from_bytes(PNG.to_vec()).unwrap();
            library
                .add_cover(&Platform::Gba, "Tetris.gba", &cover)
                .unwrap();
            let path = library.root().join("covers/gba/Tetris.png");
            assert_eq!(library.entries()[0].cover_path, Some(path.clone()));
            assert_eq!(fs::read(&path).unwrap(), PNG);
            let in_memory = library.entries().to_vec();
            library.reload().unwrap();
            assert_eq!(library.entries(), in_memory);
        }

        #[test]
        fn a_games_files_are_its_rom_and_cover() {
            let (_dir, mut library) = library();
            put(library.root(), "roms/gba/Tetris.gba", b"hello");
            put(library.root(), "covers/gba/Tetris.png", PNG);
            put(library.root(), "saves/gba/Tetris/notes.txt", b"x");
            put(library.root(), "roms/gba/Other.gba", b"other");
            library.reload().unwrap();
            let root = library.root();
            let [tetris, other] = [&library.entries()[1], &library.entries()[0]];
            assert_eq!(
                library.files_of(tetris),
                [
                    root.join("roms/gba/Tetris.gba"),
                    root.join("covers/gba/Tetris.png"),
                ]
            );
            assert_eq!(library.files_of(other), [root.join("roms/gba/Other.gba")]);
        }

        #[test]
        fn all_save_files_are_every_version() {
            let (_dir, mut library) = library();
            put(library.root(), "roms/gba/Tetris.gba", b"hello");
            let old = put_at(library.root(), "saves/gba/Tetris/old.sav", b"a", 1_000);
            let new = put_at(library.root(), "saves/gba/Tetris/new.sav", b"b", 2_000);
            put(library.root(), "saves/gba/Tetris/notes.txt", b"x");
            library.reload().unwrap();
            let entry = library.entries()[0].clone();
            assert_eq!(library.all_save_files(&entry).unwrap(), [new, old]);
        }

        #[test]
        fn saves_read_again_are_those_still_there() {
            let (_dir, mut library) = library();
            put(library.root(), "roms/gba/Tetris.gba", b"hello");
            put_at(library.root(), "saves/gba/Tetris/old.sav", b"a", 1_000);
            let new = put_at(library.root(), "saves/gba/Tetris/new.sav", b"bb", 2_000);
            library.reload().unwrap();
            fs::remove_file(&new).unwrap();
            library.reread_saves(&Platform::Gba, "Tetris.gba").unwrap();
            let history = library.entries()[0].save.clone().unwrap();
            assert_eq!(history.current.size, 1);
            assert_eq!(history.versions, []);
        }
    }

    mod orphan_saves {
        use super::*;

        #[test]
        fn saves_without_a_game_are_orphans() {
            let (_dir, mut library) = library();
            put(library.root(), "roms/gba/Kept.gba", b"hello");
            put(library.root(), "saves/gba/Kept/a.sav", b"has a game");
            put(library.root(), "saves/gba/Gone/a.sav", b"none");
            put(library.root(), "saves/gb/Kept/a.sav", b"another platform");
            put(library.root(), "saves/gba/Empty/notes.txt", b"not a save");
            library.reload().unwrap();
            let orphans = library.orphan_saves().unwrap();
            let found: Vec<_> = orphans
                .iter()
                .map(|o| (o.platform.clone(), o.stem.as_str()))
                .collect();
            assert_eq!(found, [(Platform::Gb, "Kept"), (Platform::Gba, "Gone")]);
        }

        #[test]
        fn their_files_are_every_version() {
            let (_dir, mut library) = library();
            let old = put_at(library.root(), "saves/gba/Gone/old.sav", b"a", 1_000);
            let new = put_at(library.root(), "saves/gba/Gone/new.sav", b"b", 2_000);
            library.reload().unwrap();
            let orphan = library.orphan_saves().unwrap().remove(0);
            assert_eq!(library.orphan_save_files(&orphan).unwrap(), [new, old]);
        }

        #[test]
        fn one_with_a_game_again_has_no_files() {
            let (_dir, mut library) = library();
            put_at(library.root(), "saves/gba/Gone/old.sav", b"a", 1_000);
            library.reload().unwrap();
            let orphan = library.orphan_saves().unwrap().remove(0);
            put(library.root(), "roms/gba/Gone.gba", b"hello");
            library.reload().unwrap();
            assert_eq!(
                library.orphan_save_files(&orphan).unwrap(),
                Vec::<PathBuf>::new()
            );
        }
    }
}
