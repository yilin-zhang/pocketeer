//! The library on the computer, where Pocketeer keeps games, their covers
//! and their saves.

use std::{
    collections::{HashMap, HashSet, hash_map},
    fmt,
    path::{Path, PathBuf},
};

use fs_err as fs;
use image::ImageFormat;

use crate::{
    cover::Cover,
    error::BasicError,
    game::{Game, RomCache, Save},
    os::{self, FileNameExt},
    platform::Platform,
};

/// One version of a game's save in the library. Its name is its file name
/// without `.sav`, which the front ends show and take to pick a version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibrarySave {
    pub name: String,
    pub save: Save,
}

/// Every version of a game's save in the library, of which there is at least
/// one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibrarySaves {
    /// Newest first.
    versions: Vec<LibrarySave>,
}

impl LibrarySaves {
    /// Sorts `saves` newest first. Of versions written in the same second the
    /// one with the higher number was added later, so it counts as newer.
    /// Returns none if there are none.
    pub fn from_saves(mut saves: Vec<LibrarySave>) -> Option<Self> {
        if saves.is_empty() {
            return None;
        }
        saves.sort_by(|a, b| {
            let newest_first = b.save.modified.cmp(&a.save.modified);
            // Their names differ only in the number, so a longer name has a
            // higher number.
            newest_first
                .then_with(|| b.name.len().cmp(&a.name.len()))
                .then_with(|| b.name.cmp(&a.name))
        });
        Some(Self { versions: saves })
    }

    pub fn newest(&self) -> &LibrarySave {
        &self.versions[0]
    }

    /// Every version, newest first.
    pub fn iter(&self) -> impl Iterator<Item = &LibrarySave> {
        self.versions.iter()
    }

    pub fn version_named(&self, name: &str) -> Option<&LibrarySave> {
        self.iter().find(|version| version.name == name)
    }

    pub fn contains(&self, save: &Save) -> bool {
        self.iter()
            .any(|version| version.save.has_same_contents_as(save))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalEntry {
    /// The game's name, which is also its file name, because on the device a
    /// save is found by the game file's name.
    pub name: String,
    pub game: Game,
    /// Where its cover is, if it has one.
    ///
    /// This is a path rather than the cover because covers are large and only a
    /// few are shown at a time. Whoever needs one reads it when needed, without
    /// going through the library.
    pub cover_path: Option<PathBuf>,
    pub save: Option<LibrarySaves>,
}

impl LocalEntry {
    fn is_called(&self, platform: &Platform, name: &str) -> bool {
        &self.game.platform == platform && self.name == name
    }

    pub fn stem(&self) -> &str {
        self.name.stem()
    }
}

/// As `<platform>/<name>`, the way the command line, the log and messages
/// name it.
impl fmt::Display for LocalEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.game.platform.id(), self.name)
    }
}

/// Saves in the library with no game of that name. Trashing a game leaves them
/// behind, and adding a game under that name picks them up again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanLocalSave {
    pub platform: Platform,
    /// The name its game had, without the extension.
    pub stem: String,
    pub save: LibrarySaves,
}

/// As `<platform>/<name>`, the way the command line, the log and messages
/// name it.
impl fmt::Display for OrphanLocalSave {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.platform.id(), self.stem)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OpenLibraryError {
    #[error("no home folder; give a --library")]
    NoHome,
    #[error("no library at {}", .0.display())]
    NoLibrary(PathBuf),
    #[error(transparent)]
    Other(#[from] BasicError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddGameOutcome {
    /// The game as it is in the library now.
    Added(LocalEntry),
    /// The library already has the game, under this name.
    AlreadyHad(LocalEntry),
}

#[derive(Debug, thiserror::Error)]
pub enum AddGameError {
    /// Another game of the platform has this name, whatever its extension or
    /// case. Saves and covers are kept by the name without the extension, and a
    /// file system may not tell cases apart.
    #[error("another game in the library has its name")]
    NameTaken,
    /// The library can't hold a file with this name, or a folder with its
    /// platform's id, because it would end up elsewhere or be hidden and not
    /// found again.
    #[error("the library can't hold a game of that name")]
    BadName,
    #[error(transparent)]
    Other(#[from] BasicError),
}

/// Why a game in the library wasn't renamed. Nothing has changed, unless the rename
/// failed halfway and putting the files back failed as well.
#[derive(Debug, thiserror::Error)]
pub enum RenameInLibraryError {
    #[error("the library can't hold a game of that name")]
    BadName,
    #[error("it has that name already")]
    SameName,
    /// Another game of the platform has this name, whatever its extension or
    /// case. Saves and covers are kept by the name without the extension.
    #[error("another game in the library has that name")]
    NameTaken,
    /// Saves or a cover with no game have this name and would become this
    /// game's.
    #[error("the library has saves or a cover under that name")]
    LeftoversThere,
    #[error(transparent)]
    Other(#[from] BasicError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrashSaveAllOutcome {
    Trashed,
    /// The library has no save of the game.
    NoSave,
}

/// Why a version of a save wasn't trashed.
#[derive(Debug, thiserror::Error)]
pub enum TrashSaveError {
    /// The save has no version of that name.
    #[error("the library has no such save")]
    NoSuchVersion,
    #[error(transparent)]
    Other(#[from] BasicError),
}

#[derive(Debug, thiserror::Error)]
pub enum TrashOrphanSaveError {
    /// The orphan save is gone, or a game has taken it.
    #[error("the library has no such orphan save")]
    NoOrphanSave,
    #[error(transparent)]
    Other(#[from] BasicError),
}

/// Every path in a library folder is spelled out here.
#[derive(Debug, Clone)]
pub(crate) struct LibraryLayout {
    root: PathBuf,
}

impl LibraryLayout {
    pub(crate) fn dats(&self) -> PathBuf {
        self.root.join("dats")
    }

    /// The cached list of cores that can be downloaded.
    pub(crate) fn core_index(&self) -> PathBuf {
        self.root.join("cores.json")
    }

    /// The cached list of covers that can be downloaded.
    pub(crate) fn cover_index(&self) -> PathBuf {
        self.root.join("covers.json")
    }

    fn roms(&self) -> PathBuf {
        self.root.join("roms")
    }

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

    fn platform_saves(&self, platform: &Platform) -> PathBuf {
        self.all_saves().join(platform.id())
    }

    /// The folder of a game's save versions, by its name without the
    /// extension as on the Pocket.
    fn saves(&self, platform: &Platform, stem: &str) -> PathBuf {
        self.platform_saves(platform).join(stem)
    }

    /// The file of the version called `version_name` of the game's save.
    fn save_version(&self, platform: &Platform, stem: &str, version_name: &str) -> PathBuf {
        self.saves(platform, stem)
            .join(format!("{version_name}.sav"))
    }

    pub(crate) fn log(&self) -> PathBuf {
        self.root.join("pocketeer.log")
    }

    fn cache(&self) -> PathBuf {
        self.root.join("cache.json")
    }
}

pub struct Library {
    layout: LibraryLayout,
    cache: RomCache,
    /// Sorted by platform id and name.
    entries: Vec<LocalEntry>,
}

impl Library {
    /// The library at `root`, a folder that must exist, with every game in
    /// it, each once.
    pub fn open(root: &Path) -> Result<Self, OpenLibraryError> {
        if !root.is_dir() {
            return Err(OpenLibraryError::NoLibrary(root.to_path_buf()));
        }
        let layout = LibraryLayout {
            root: root.to_path_buf(),
        };
        let roms = layout.roms();
        fs::create_dir_all(&roms).map_err(BasicError::from)?;
        let cache = RomCache::load(&layout.root, &layout.cache());
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

    pub fn entry_named(&self, platform: &Platform, name: &str) -> Option<&LocalEntry> {
        self.entries
            .iter()
            .find(|entry| entry.is_called(platform, name))
    }

    /// The library's copy of `game`, under whatever name.
    pub fn entry_for(&self, game: &Game) -> Option<&LocalEntry> {
        self.entries.iter().find(|entry| &entry.game == game)
    }

    pub(crate) fn layout(&self) -> &LibraryLayout {
        &self.layout
    }

    /// Reads the folder again to pick up changes made outside pocketeer. Games
    /// whose files are unchanged aren't read again, and covers aren't read at
    /// all.
    pub fn reload(&mut self) -> Result<(), BasicError> {
        self.entries = self.scan()?;
        Ok(())
    }

    pub fn game_path(&self, entry: &LocalEntry) -> PathBuf {
        self.layout.rom(&entry.game.platform, &entry.name)
    }

    /// Moves `entry`'s file and cover to the system's trash, where they can be
    /// restored, and forgets the game. Its saves stay, in case a game is added
    /// under that name again.
    pub(crate) fn trash_game(&mut self, entry: &LocalEntry) -> Result<(), BasicError> {
        trash::delete_all(self.files_of(entry))?;
        self.forget_game(&entry.game.platform, &entry.name);
        Ok(())
    }

    /// The files of `entry` as a game, which are its file and its cover if it
    /// has one. Its saves are kept apart under its name.
    fn files_of(&self, entry: &LocalEntry) -> Vec<PathBuf> {
        let mut files = vec![self.game_path(entry)];
        files.extend(entry.cover_path.clone());
        files
    }

    /// The saves with no game of their name, as the folders are now.
    pub fn find_orphan_saves(&self) -> Result<Vec<OrphanLocalSave>, BasicError> {
        let all = self.layout.all_saves();
        let mut orphans = Vec::new();
        for id in os::list_dir(&all)?.dirs {
            let platform = Platform::from_id(&id);
            let dir = self.layout.platform_saves(&platform);
            for name in os::list_dir(&dir)?.dirs {
                let has_game =
                    |entry: &LocalEntry| entry.game.platform == platform && entry.stem() == name;
                if self.entries.iter().any(has_game) {
                    continue;
                }
                if let Some(save) = self.read_save_history(&platform, &name)? {
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

    /// Moves every version of `entry`'s save to the system's trash.
    pub(crate) fn trash_all_saves(
        &mut self,
        entry: &LocalEntry,
    ) -> Result<TrashSaveAllOutcome, BasicError> {
        let files = self.files_of_all_saves(entry)?;
        if files.is_empty() {
            return Ok(TrashSaveAllOutcome::NoSave);
        }
        trash::delete_all(&files)?;
        self.reread_saves(&entry.game.platform, &entry.name)?;
        Ok(TrashSaveAllOutcome::Trashed)
    }

    /// Moves the version called `version_name` of `entry`'s save to the
    /// system's trash.
    pub(crate) fn trash_save_version(
        &mut self,
        entry: &LocalEntry,
        version_name: &str,
    ) -> Result<(), TrashSaveError> {
        let path = self
            .layout
            .save_version(&entry.game.platform, entry.stem(), version_name);
        if !path.is_file() {
            return Err(TrashSaveError::NoSuchVersion);
        }
        trash::delete(&path).map_err(BasicError::from)?;
        self.reread_saves(&entry.game.platform, &entry.name)?;
        Ok(())
    }

    /// Moves every version of `orphan`'s save to the system's trash, unless it
    /// is no longer an orphan because a game has been put under its name.
    pub(crate) fn trash_orphan_save(
        &self,
        orphan: &OrphanLocalSave,
    ) -> Result<(), TrashOrphanSaveError> {
        let files = self.files_of_orphan_save(orphan)?;
        if files.is_empty() {
            return Err(TrashOrphanSaveError::NoOrphanSave);
        }
        trash::delete_all(&files).map_err(BasicError::from)?;
        Ok(())
    }

    fn files_of_all_saves(&self, entry: &LocalEntry) -> Result<Vec<PathBuf>, BasicError> {
        let files = self.files_of_saves(&entry.game.platform, entry.stem())?;
        Ok(files.into_iter().map(|(path, _)| path).collect())
    }

    /// The files of every version of `orphan`'s save, or none if it is no
    /// longer an orphan because a game has been put under its name.
    fn files_of_orphan_save(&self, orphan: &OrphanLocalSave) -> Result<Vec<PathBuf>, BasicError> {
        let is_it = |candidate: &OrphanLocalSave| {
            candidate.platform == orphan.platform && candidate.stem == orphan.stem
        };
        if !self.find_orphan_saves()?.iter().any(is_it) {
            return Ok(Vec::new());
        }
        let files = self.files_of_saves(&orphan.platform, &orphan.stem)?;
        Ok(files.into_iter().map(|(path, _)| path).collect())
    }

    /// Reads the saves of the game called `name` again, once some are gone.
    fn reread_saves(&mut self, platform: &Platform, name: &str) -> Result<(), BasicError> {
        let saves = self.read_save_history(platform, name.stem())?;
        let i = self.index_of(platform, name)?;
        self.entries[i].save = saves;
        Ok(())
    }

    /// Copies the file at `from` into the library as a game of `platform`,
    /// under its file name, unless the library has the game already. The file
    /// stays where it is.
    pub(crate) fn import_game(
        &mut self,
        from: &Path,
        platform: Platform,
    ) -> Result<AddGameOutcome, AddGameError> {
        let game = Game::read(platform, from).map_err(BasicError::from)?;
        let name = from.file_name().unwrap_or_default().to_string_lossy();
        self.add_game(&game, &name, from)
    }

    /// Copies the file at `from`, which holds `game`, into the library as the
    /// game called `name`, unless the library has the game already, under
    /// whatever name.
    ///
    /// This never replaces a file, so it fails if one is already there or if
    /// another game of the platform has that name, whatever its extension or
    /// case. The copy isn't read, as `game` is what gets recorded.
    pub(crate) fn add_game(
        &mut self,
        game: &Game,
        name: &str,
        from: &Path,
    ) -> Result<AddGameOutcome, AddGameError> {
        if let Some(entry) = self.entry_for(game) {
            return Ok(AddGameOutcome::AlreadyHad(entry.clone()));
        }
        // The platform's id is a folder of the library, and an id can be
        // anything.
        if !name.is_valid_file_name() || !game.platform.id().is_valid_file_name() {
            return Err(AddGameError::BadName);
        }
        let platform = &game.platform;
        let lowercase = name.stem().to_lowercase();
        let taken = |entry: &LocalEntry| {
            &entry.game.platform == platform && entry.stem().to_lowercase() == lowercase
        };
        let to = self.layout.rom(platform, name);
        if self.entries.iter().any(taken) || to.exists() {
            return Err(AddGameError::NameTaken);
        }
        os::copy_atomically(from, &to)?;
        self.cache.record_game(&to, game)?;
        self.cache.save();
        let entry = LocalEntry {
            name: name.into(),
            cover_path: self.cover_path(platform, name.stem()),
            save: self.read_save_history(platform, name.stem())?,
            game: game.clone(),
        };
        self.insert(entry.clone());
        Ok(AddGameOutcome::Added(entry))
    }

    /// Inserts `entry` so that `entries` stays sorted by platform id and name.
    fn insert(&mut self, entry: LocalEntry) {
        let place = self.entries.binary_search_by(|existing| {
            (existing.game.platform.id(), &existing.name)
                .cmp(&(entry.game.platform.id(), &entry.name))
        });
        let (Ok(at) | Err(at)) = place;
        self.entries.insert(at, entry);
    }

    /// Renames the game called `name` to `new`, keeping its extension and
    /// moving its saves and cover. The contents aren't read again.
    pub(crate) fn rename_game(
        &mut self,
        platform: &Platform,
        name: &str,
        new: &str,
    ) -> Result<LocalEntry, RenameInLibraryError> {
        let i = self.index_of(platform, name)?;
        let new_name = self.check_rename(i, new)?;
        let (old_stem, new_stem) = (name.stem(), new_name.stem());
        let (from, to) = (
            self.layout.rom(platform, name),
            self.layout.rom(platform, &new_name),
        );
        let mut renames = Vec::new();
        let saves = self.layout.saves(platform, old_stem);
        if saves.exists() {
            renames.push((saves, self.layout.saves(platform, new_stem)));
        }
        if let Some(cover) = &self.entries[i].cover_path {
            let extension = cover.extension().unwrap_or_default().to_string_lossy();
            let file = format!("{new_stem}.{extension}");
            renames.push((cover.clone(), cover.with_file_name(file)));
        }
        renames.push((from.clone(), to.clone()));
        os::safe_rename_all(&renames)?;
        let cover_path = self.cover_path(platform, new_stem);
        let mut entry = self.entries.remove(i);
        entry.name = new_name;
        entry.cover_path = cover_path;
        self.cache.forget(&from);
        self.cache.record_game(&to, &entry.game)?;
        self.cache.save();
        self.insert(entry.clone());
        Ok(entry)
    }

    /// The file name the game at `i` gets if it is renamed to `new`, or why it
    /// can't be. Nothing is changed.
    fn check_rename(&self, i: usize, new: &str) -> Result<String, RenameInLibraryError> {
        let LocalEntry { name, game, .. } = &self.entries[i];
        let platform = &game.platform;
        let new_name = name.with_stem(new);
        if !new_name.is_valid_fat_name() {
            return Err(RenameInLibraryError::BadName);
        }
        if new_name == *name {
            return Err(RenameInLibraryError::SameName);
        }
        let (old_stem, new_stem) = (name.stem(), new_name.stem());
        let lowercase = new_stem.to_lowercase();
        // Another game of the platform has the name, whatever its case.
        let taken = |(other_index, entry): (usize, &LocalEntry)| {
            other_index != i
                && &entry.game.platform == platform
                && entry.stem().to_lowercase() == lowercase
        };
        if self.entries.iter().enumerate().any(taken) {
            return Err(RenameInLibraryError::NameTaken);
        }
        // Files at the new name that aren't a game's. If only the case changed,
        // on a file system that doesn't tell cases apart, whatever is at the new
        // name is the game's own.
        if old_stem.to_lowercase() != lowercase {
            if self.layout.rom(platform, &new_name).exists() {
                return Err(RenameInLibraryError::NameTaken);
            }
            if self.layout.saves(platform, new_stem).exists()
                || self.cover_path(platform, new_stem).is_some()
            {
                return Err(RenameInLibraryError::LeftoversThere);
            }
        }
        Ok(new_name)
    }

    /// Adds the save at `from` to the game called `name`, unless it already has
    /// one with the same contents. Returns the new version if one was added.
    ///
    /// The version is named after when the save was written, and a second one
    /// from the same second gets `-2`, and so on.
    pub(crate) fn add_save(
        &mut self,
        platform: &Platform,
        name: &str,
        from: &Path,
    ) -> Result<Option<LibrarySave>, BasicError> {
        let i = self.index_of(platform, name)?;
        let modified = fs::metadata(from).and_then(|metadata| metadata.modified())?;
        let bytes = fs::read(from)?;
        let save = Save::from_bytes(&bytes, modified);
        if self.entries[i]
            .save
            .as_ref()
            .is_some_and(|saves| saves.contains(&save))
        {
            return Ok(None);
        }
        let timestamp = save.timestamp();
        let (version_name, path) = (1..)
            .map(|version_number| match version_number {
                1 => timestamp.clone(),
                version_number => format!("{timestamp}-{version_number}"),
            })
            .map(|version_name| {
                let path = self
                    .layout
                    .save_version(platform, name.stem(), &version_name);
                (version_name, path)
            })
            .find(|(_, path)| !path.exists())
            .expect("some name is free");
        os::write_atomically(&path, &bytes, Some(modified))?;
        let version = LibrarySave {
            name: version_name,
            save,
        };
        let entry = &mut self.entries[i];
        let mut saves: Vec<LibrarySave> = entry
            .save
            .iter()
            .flat_map(|saves| saves.iter().cloned())
            .collect();
        saves.push(version.clone());
        entry.save = LibrarySaves::from_saves(saves);
        Ok(Some(version))
    }

    /// Removes the game called `name` from the entries and the cache, once its
    /// files are gone.
    fn forget_game(&mut self, platform: &Platform, name: &str) {
        self.cache.forget(&self.layout.rom(platform, name));
        self.cache.save();
        self.entries
            .retain(|entry| !entry.is_called(platform, name));
    }

    /// Makes `cover` the cover of the game called `name`. Any cover it had goes
    /// to the system's trash.
    ///
    /// One of another format is trashed after the new one is written, so the
    /// game keeps a cover if writing fails. One of the same format has the same
    /// file name, so it has to go first.
    pub(crate) fn set_cover(
        &mut self,
        platform: &Platform,
        name: &str,
        cover: &Cover,
    ) -> Result<(), BasicError> {
        let i = self.index_of(platform, name)?;
        let new_path = self.layout.cover(platform, name.stem(), cover.format());
        let old_path = self.entries[i].cover_path.clone();
        if old_path.as_ref() == Some(&new_path) {
            trash::delete(&new_path)?;
        }
        os::write_atomically(&new_path, cover.bytes(), None)?;
        self.entries[i].cover_path = Some(new_path.clone());
        if let Some(old_path) = old_path.filter(|path| *path != new_path) {
            trash::delete(&old_path)?;
        }
        Ok(())
    }

    /// The file of `version`, or none if `entry`'s save no longer has it.
    pub(crate) fn file_of_version(
        &self,
        entry: &LocalEntry,
        version: &LibrarySave,
    ) -> Option<PathBuf> {
        let path = self
            .layout
            .save_version(&entry.game.platform, entry.stem(), &version.name);
        path.is_file().then_some(path)
    }

    fn index_of(&self, platform: &Platform, name: &str) -> Result<usize, BasicError> {
        self.entries
            .iter()
            .position(|entry| entry.is_called(platform, name))
            .ok_or(BasicError::NotInLibrary)
    }

    fn scan(&mut self) -> Result<Vec<LocalEntry>, BasicError> {
        let mut seen = HashSet::new();
        let mut entries = Vec::new();
        let roms = self.layout.roms();
        for id in os::list_dir(&roms)?.dirs {
            let platform = Platform::from_id(&id);
            let dir = self.layout.platform_roms(&platform);
            for name in os::list_dir(&dir)?.files {
                let path = self.layout.rom(&platform, &name);
                let game = self.cache.read_game(&platform, &path)?;
                seen.insert(path);
                let cover_path = self.cover_path(&platform, name.stem());
                let save = self.read_save_history(&platform, name.stem())?;
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
    fn refuse_same_game_twice(&self, entries: &[LocalEntry]) -> Result<(), BasicError> {
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
            return Err(BasicError::SameGameInLibrary { copies });
        }
        Ok(())
    }

    fn cover_path(&self, platform: &Platform, stem: &str) -> Option<PathBuf> {
        Cover::FORMATS
            .into_iter()
            .map(|format| self.layout.cover(platform, stem, format))
            .find(|path| path.is_file())
    }

    /// Every version of the game's save, newest first, or none if it has none.
    fn read_save_history(
        &self,
        platform: &Platform,
        stem: &str,
    ) -> Result<Option<LibrarySaves>, BasicError> {
        let saves = self
            .files_of_saves(platform, stem)?
            .into_iter()
            .map(|(_, version)| version);
        Ok(LibrarySaves::from_saves(saves.collect()))
    }

    /// The game's save files, each with the version it holds.
    fn files_of_saves(
        &self,
        platform: &Platform,
        stem: &str,
    ) -> Result<Vec<(PathBuf, LibrarySave)>, BasicError> {
        let dir = self.layout.saves(platform, stem);
        let mut files = Vec::new();
        for file_name in os::list_dir(&dir)?.files {
            let Some(name) = file_name.strip_suffix(".sav") else {
                continue;
            };
            let path = dir.join(&file_name);
            let version = LibrarySave {
                name: name.to_string(),
                save: Save::read(&path)?,
            };
            files.push((path, version));
        }
        Ok(files)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cover::testing::PNG;
    use crate::game::{Files, Rom};
    use crate::os::testing::{at, put, put_at};

    /// A library holding `files`.
    fn library_with(files: &[(&str, &[u8])]) -> (tempfile::TempDir, Library) {
        let dir = tempfile::tempdir().unwrap();
        for (path, bytes) in files {
            put(dir.path(), path, bytes);
        }
        let library = Library::open(dir.path()).unwrap();
        (dir, library)
    }

    fn names(library: &Library) -> Vec<String> {
        library
            .entries()
            .iter()
            .map(|entry| entry.to_string())
            .collect()
    }

    mod renaming {
        use super::*;

        /// A library with `Old.gba`, its cover, two versions of its save, and
        /// `Other.gba`.
        fn with_game() -> (tempfile::TempDir, Library) {
            let (dir, mut library) = library_with(&[]);
            let root = dir.path();
            put_at(root, "roms/gba/Old.gba", b"old", 1_000);
            put(root, "roms/gba/Other.gba", b"other");
            put(root, "covers/gba/Old.png", b"cover");
            put_at(root, "saves/gba/Old/a.sav", b"one", 1_000);
            put_at(root, "saves/gba/Old/b.sav", b"two", 2_000);
            library.reload().unwrap();
            (dir, library)
        }

        fn rename(library: &mut Library, new: &str) -> Result<LocalEntry, RenameInLibraryError> {
            library.rename_game(&Platform::Gba, "Old.gba", new)
        }

        /// What is under the library's `roms`, `covers` and `saves`.
        fn all_files(root: &Path) -> Vec<String> {
            let mut files = Vec::new();
            for dir in ["roms", "covers", "saves"] {
                for file in os::list_files_recursive(&root.join(dir)).unwrap() {
                    files.push(format!("{dir}/{}", file.display()));
                }
            }
            files.sort();
            files
        }

        #[test]
        fn library_rename_moves_game_cover_and_saves() {
            let (dir, mut library) = with_game();
            let root = dir.path();
            // Its file, cover and saves move, keeping their contents and times.
            let entry = rename(&mut library, "New").unwrap();
            assert_eq!(entry.name, "New.gba");
            assert_eq!(entry.cover_path, Some(root.join("covers/gba/New.png")));
            assert_eq!(entry.save.as_ref().unwrap().iter().count(), 2);
            assert_eq!(
                all_files(root),
                [
                    "covers/gba/New.png",
                    "roms/gba/New.gba",
                    "roms/gba/Other.gba",
                    "saves/gba/New/a.sav",
                    "saves/gba/New/b.sav",
                ]
            );
            assert_eq!(fs::read(root.join("roms/gba/New.gba")).unwrap(), b"old");
            let save = fs::metadata(root.join("saves/gba/New/a.sav")).unwrap();
            assert_eq!(save.modified().unwrap(), at(1_000));
            // A game without a cover or saves is renamed too.
            let other = library
                .rename_game(&Platform::Gba, "Other.gba", "Zzz")
                .unwrap();
            assert_eq!((other.cover_path, other.save), (None, None));
            assert!(root.join("roms/gba/Zzz.gba").exists());
            // The entries stay sorted.
            assert_eq!(names(&library), ["gba/New.gba", "gba/Zzz.gba"]);
            library
                .rename_game(&Platform::Gba, "Zzz.gba", "Aaa")
                .unwrap();
            assert_eq!(names(&library), ["gba/Aaa.gba", "gba/New.gba"]);
            // Only the case can change, though the file system may not tell
            // cases apart.
            let entry = library
                .rename_game(&Platform::Gba, "New.gba", "NEW")
                .unwrap();
            assert_eq!(entry.name, "NEW.gba");
            assert_eq!(entry.save.unwrap().iter().count(), 2);
            let roms = os::list_dir(&root.join("roms/gba")).unwrap().files;
            assert!(roms.contains(&"NEW.gba".to_string()), "{roms:?}");
        }

        #[test]
        fn moves_nothing_when_refused() {
            use RenameInLibraryError::*;
            for (new, leftover, refusal) in [
                ("a:b", None, BadName),
                ("", None, BadName),
                (".gba", None, BadName),
                ("Old", None, SameName),
                ("Other", None, NameTaken),
                ("other", None, NameTaken),
                // A file the library hasn't scanned yet.
                ("New", Some("roms/gba/New.gba"), NameTaken),
                ("New", Some("saves/gba/New/x.sav"), LeftoversThere),
                ("New", Some("covers/gba/New.jpg"), LeftoversThere),
            ] {
                let (dir, mut library) = with_game();
                if let Some(leftover) = leftover {
                    put(dir.path(), leftover, b"not its own");
                }
                let before = all_files(dir.path());
                let refused = rename(&mut library, new).unwrap_err();
                assert_eq!(
                    std::mem::discriminant(&refused),
                    std::mem::discriminant(&refusal),
                    "{new}: {refused:?}"
                );
                assert_eq!(all_files(dir.path()), before, "{new}");
                assert_eq!(names(&library), ["gba/Old.gba", "gba/Other.gba"]);
            }
        }

        #[test]
        fn restores_everything_on_failure() {
            let (dir, mut library) = with_game();
            let root = dir.path();
            // With its extension, the name is too long for a file. Its save
            // folder, which has no extension, is renamed before its cover
            // fails.
            let new = "x".repeat(252);
            let before = all_files(root);
            assert!(matches!(
                rename(&mut library, &new),
                Err(RenameInLibraryError::Other(_))
            ));
            assert_eq!(all_files(root), before);
            assert_eq!(library.entries()[0].name, "Old.gba");
        }
    }

    mod opening {
        use super::*;

        #[test]
        fn does_not_create_missing_dir() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("typo");
            assert!(matches!(
                Library::open(&root),
                Err(OpenLibraryError::NoLibrary(_))
            ));
            assert!(!root.exists());
        }

        #[test]
        fn refuses_same_game_twice() {
            let dir = tempfile::tempdir().unwrap();
            let first = put(dir.path(), "roms/gba/Mine.gba", b"same");
            let second = put(dir.path(), "roms/gba/Other.gba", b"same");
            let Err(OpenLibraryError::Other(BasicError::SameGameInLibrary { copies })) =
                Library::open(dir.path())
            else {
                panic!()
            };
            assert_eq!(copies, [(first, second.clone())]);
            // A reload that finds a copy keeps what was there before.
            fs::remove_file(&second).unwrap();
            let mut library = Library::open(dir.path()).unwrap();
            put(dir.path(), "roms/gba/Other.gba", b"same");
            assert!(matches!(
                library.reload(),
                Err(BasicError::SameGameInLibrary { .. })
            ));
            assert_eq!(names(&library), ["gba/Mine.gba"]);
            // The same contents on another platform are another game.
            fs::remove_file(&second).unwrap();
            put(dir.path(), "roms/gb/Mine.gb", b"same");
            library.reload().unwrap();
            assert_eq!(names(&library), ["gb/Mine.gb", "gba/Mine.gba"]);
        }

        #[test]
        fn reads_games_covers_and_saves_from_folder() {
            let (_dir, mut library) = library_with(&[
                ("roms/gba/Tetris.gba", b"hello"),
                ("covers/gba/Tetris.png", PNG),
                ("saves/gba/Tetris/notes.txt", b"x"),
                ("saves/gba/Tetris/._a.sav", b"x"),
            ]);
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
            let versions: Vec<_> = entry.save.iter().flat_map(LibrarySaves::iter).collect();
            let versions: Vec<_> = versions
                .iter()
                .map(|version| (version.name.as_str(), version.save.size))
                .collect();
            assert_eq!(versions, [("new", 2), ("old", 1)]);
        }

        #[test]
        fn rereads_forgotten_game_instead_of_using_cache() {
            let (_dir, mut library) = library_with(&[]);
            let path = "roms/gba/Tetris.gba";
            let file = put_at(library.root(), path, b"hello", 1_000_000);
            library.reload().unwrap();
            fs::remove_file(&file).unwrap();
            library.forget_game(&Platform::Gba, "Tetris.gba");
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

    mod adding {
        use super::*;

        #[test]
        fn library_imports_game_once() {
            let (_dir, mut library) = library_with(&[("roms/gba/Tetris.gba", b"kept")]);
            let root = library.root().to_path_buf();
            let elsewhere = tempfile::tempdir().unwrap();
            // A game the library has, under another name, isn't copied.
            let copy = put(elsewhere.path(), "Copy.gba", b"kept");
            let imported = library.import_game(&copy, Platform::Gba).unwrap();
            let AddGameOutcome::AlreadyHad(entry) = imported else {
                panic!("{imported:?}")
            };
            assert_eq!(entry.name, "Tetris.gba");
            assert!(!root.join("roms/gba/Copy.gba").exists());
            // Another game's name, whatever its extension or case, is taken.
            for name in ["Tetris.agb", "tetris.gba"] {
                let from = put(elsewhere.path(), name, b"other");
                let imported = library.import_game(&from, Platform::Gba);
                assert!(matches!(imported, Err(AddGameError::NameTaken)), "{name}");
            }
            assert!(!root.join("roms/gba/Tetris.agb").exists());
            assert_eq!(fs::read(root.join("roms/gba/Tetris.gba")).unwrap(), b"kept");
            // So is the name of a file put in the folder by hand.
            let by_hand = put(&root, "roms/gba/Hand.gba", b"by hand");
            let from = put(elsewhere.path(), "Hand.gba", b"other hand");
            let imported = library.import_game(&from, Platform::Gba);
            assert!(matches!(imported, Err(AddGameError::NameTaken)));
            assert_eq!(fs::read(&by_hand).unwrap(), b"by hand");
            fs::remove_file(&by_hand).unwrap();
            // A game is kept under its file name for the platform given,
            // whatever its extension and whatever another platform has. The
            // file stays where it was.
            let apart = tempfile::tempdir().unwrap();
            let from = put(apart.path(), "Tetris.gba", b"hello");
            let imported = library.import_game(&from, Platform::Gb).unwrap();
            assert_eq!(
                imported,
                AddGameOutcome::Added(LocalEntry {
                    name: "Tetris.gba".into(),
                    game: Game {
                        platform: Platform::Gb,
                        files: Files::Single(Rom {
                            size: 5,
                            crc32: 0x3610a686
                        }),
                    },
                    cover_path: None,
                    save: None,
                })
            );
            assert_eq!(fs::read(root.join("roms/gb/Tetris.gba")).unwrap(), b"hello");
            assert_eq!(fs::read(&from).unwrap(), b"hello");
            // The entries stay sorted, as the folder reads.
            let from = put(apart.path(), "A.gba", b"a");
            library.import_game(&from, Platform::Gba).unwrap();
            assert_eq!(
                names(&library),
                ["gb/Tetris.gba", "gba/A.gba", "gba/Tetris.gba"]
            );
            let in_memory = library.entries().to_vec();
            library.reload().unwrap();
            assert_eq!(library.entries(), in_memory);
        }

        #[test]
        fn refuses_names_outside_library() {
            let (dir, mut library) = library_with(&[]);
            let elsewhere = tempfile::tempdir().unwrap();
            let from = put(elsewhere.path(), "x.gba", b"other");
            let game = Game::read(Platform::Gba, &from).unwrap();
            for name in ["../x.gba", "a/x.gba", "a\\x.gba", ".x.gba", "..", ""] {
                let added = library.add_game(&game, name, &from);
                assert!(matches!(added, Err(AddGameError::BadName)), "{name}");
            }
            // A platform's id is a folder's name too.
            for id in ["../../escaped", "/absolute"] {
                let imported = library.import_game(&from, Platform::from_id(id));
                assert!(matches!(imported, Err(AddGameError::BadName)), "{id}");
            }
            assert_eq!(library.entries(), []);
            assert!(!dir.path().join("roms/gba").exists());
            assert!(!dir.path().join("x.gba").exists());
            assert!(!dir.path().parent().unwrap().join("escaped").exists());
        }
    }

    mod saves {
        use super::*;

        #[test]
        fn library_keeps_save_versions() {
            let (_dir, mut library) = library_with(&[("roms/gba/Tetris.gba", b"hello")]);
            let dir = library.root().join("saves/gba/Tetris");
            let elsewhere = tempfile::tempdir().unwrap();
            let add = |library: &mut Library, name: &str, bytes: &[u8]| {
                let from = put_at(elsewhere.path(), name, bytes, 1_762_179_605);
                library.add_save(&Platform::Gba, "Tetris.gba", &from)
            };
            // A version is named after when it was written, in UTC, and keeps
            // that time. The file stays where it was.
            let first = add(&mut library, "Tetris.sav", b"save").unwrap().unwrap();
            assert_eq!(first.name, "2025-11-03T14-20-05");
            let file = dir.join("2025-11-03T14-20-05.sav");
            assert_eq!(fs::read(&file).unwrap(), b"save");
            assert_eq!(
                fs::metadata(&file).unwrap().modified().unwrap(),
                at(1_762_179_605)
            );
            assert_eq!(
                fs::read(elsewhere.path().join("Tetris.sav")).unwrap(),
                b"save"
            );
            // The same contents again add nothing.
            assert_eq!(add(&mut library, "Again.sav", b"save").unwrap(), None);
            // Another version from the same second gets a number.
            let second = add(&mut library, "Second.sav", b"two").unwrap().unwrap();
            assert_eq!(second.name, "2025-11-03T14-20-05-2");
            let second_file = dir.join("2025-11-03T14-20-05-2.sav");
            assert_eq!(fs::read(&second_file).unwrap(), b"two");
            // Each version's file is found by its name.
            let entry = &library.entries()[0];
            assert_eq!(library.file_of_version(entry, &first), Some(file));
            assert_eq!(library.file_of_version(entry, &second), Some(second_file));
            let other = LibrarySave {
                name: "other".to_string(),
                save: first.save.clone(),
            };
            assert_eq!(library.file_of_version(entry, &other), None);
            // In memory as it is on disk.
            let in_memory = library.entries()[0].save.clone();
            library.reload().unwrap();
            assert_eq!(library.entries()[0].save, in_memory);
        }

        #[test]
        fn sorts_versions_newest_first() {
            let version = |name: &str, secs| LibrarySave {
                name: name.to_string(),
                save: Save::from_bytes(name.as_bytes(), at(secs)),
            };
            let saves = LibrarySaves::from_saves(vec![
                version("1970-01-01T00-00-01", 1),
                version("1970-01-01T00-16-40-9", 1_000),
                version("1970-01-01T00-16-40", 1_000),
                version("1970-01-01T00-00-03", 3),
                version("1970-01-01T00-16-40-10", 1_000),
            ])
            .unwrap();
            let names: Vec<_> = saves.iter().map(|version| version.name.as_str()).collect();
            assert_eq!(
                names,
                [
                    "1970-01-01T00-16-40-10",
                    "1970-01-01T00-16-40-9",
                    "1970-01-01T00-16-40",
                    "1970-01-01T00-00-03",
                    "1970-01-01T00-00-01",
                ]
            );
        }

        #[test]
        fn library_trash_touches_only_its_files() {
            let (_dir, mut library) = library_with(&[
                ("roms/gba/Tetris.gba", b"hello"),
                ("covers/gba/Tetris.png", PNG),
                ("saves/gba/Tetris/old.sav", b"a"),
                ("saves/gba/Tetris/new.sav", b"b"),
                ("saves/gba/Tetris/notes.txt", b"x"),
                ("roms/gba/Other.gba", b"other"),
            ]);
            let root = library.root().to_path_buf();
            let saves = root.join("saves/gba/Tetris");
            let rom = root.join("roms/gba/Tetris.gba");
            let [other, tetris] = [library.entries()[0].clone(), library.entries()[1].clone()];
            // Trashing a game takes its file and its cover, not its saves.
            assert_eq!(
                library.files_of(&tetris),
                [rom.clone(), root.join("covers/gba/Tetris.png")]
            );
            assert_eq!(library.files_of(&other), [root.join("roms/gba/Other.gba")]);
            // Trashing its saves takes every version and nothing else.
            assert_eq!(
                library.files_of_all_saves(&tetris).unwrap(),
                [saves.join("new.sav"), saves.join("old.sav")]
            );
            assert!(matches!(
                library.trash_all_saves(&other),
                Ok(TrashSaveAllOutcome::NoSave)
            ));
            assert!(matches!(
                library.trash_save_version(&tetris, "never"),
                Err(TrashSaveError::NoSuchVersion)
            ));
            assert_eq!(
                library.entries()[1].save.as_ref().unwrap().iter().count(),
                2
            );
            assert!(saves.join("old.sav").exists());
            // Once the game is gone, its saves are an orphan's, every version.
            fs::remove_file(&rom).unwrap();
            library.reload().unwrap();
            let orphan = library.find_orphan_saves().unwrap().remove(0);
            assert_eq!(orphan.stem, "Tetris");
            assert_eq!(
                library.files_of_orphan_save(&orphan).unwrap(),
                [saves.join("new.sav"), saves.join("old.sav")]
            );
            // A game back under the name takes them, so they stay.
            put(&root, "roms/gba/Tetris.gba", b"hello");
            library.reload().unwrap();
            assert!(matches!(
                library.trash_orphan_save(&orphan),
                Err(TrashOrphanSaveError::NoOrphanSave)
            ));
            assert!(saves.join("old.sav").exists());
            // Saves gone since they were listed aren't there to trash.
            fs::remove_file(&rom).unwrap();
            library.reload().unwrap();
            fs::remove_file(saves.join("old.sav")).unwrap();
            fs::remove_file(saves.join("new.sav")).unwrap();
            assert!(matches!(
                library.trash_orphan_save(&orphan),
                Err(TrashOrphanSaveError::NoOrphanSave)
            ));
        }

        #[test]
        fn lists_saves_without_game() {
            let (_dir, library) = library_with(&[
                ("roms/gba/Kept.gba", b"hello"),
                ("saves/gba/Kept/a.sav", b"has a game"),
                ("saves/gba/Gone/a.sav", b"none"),
                ("saves/gb/Kept/a.sav", b"another platform"),
                ("saves/gba/Empty/notes.txt", b"not a save"),
            ]);
            let orphans = library.find_orphan_saves().unwrap();
            let found: Vec<_> = orphans
                .iter()
                .map(|orphan| (orphan.platform.clone(), orphan.stem.as_str()))
                .collect();
            assert_eq!(found, [(Platform::Gb, "Kept"), (Platform::Gba, "Gone")]);
        }
    }

    #[test]
    fn keeps_old_cover_when_writing_new_one_fails() {
        let (dir, mut library) = library_with(&[
            ("roms/gba/Tetris.gba", b"hello"),
            ("covers/gba/Tetris.jpg", b"old cover"),
        ]);
        let old = dir.path().join("covers/gba/Tetris.jpg");
        assert_eq!(library.entries()[0].cover_path, Some(old.clone()));
        // A folder is where the new cover would go, so it can't be written.
        put(dir.path(), "covers/gba/Tetris.png/in the way", b"x");
        let cover = Cover::from_bytes(PNG.to_vec()).unwrap();
        assert!(matches!(
            library.set_cover(&Platform::Gba, "Tetris.gba", &cover),
            Err(BasicError::Io(_))
        ));
        assert_eq!(fs::read(&old).unwrap(), b"old cover");
        assert_eq!(library.entries()[0].cover_path, Some(old));
    }
}
