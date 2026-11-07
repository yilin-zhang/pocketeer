//! The library on the computer, where pocketeer keeps games, their covers
//! and their saves.

use std::{
    collections::{HashMap, HashSet, hash_map},
    path::{Path, PathBuf},
};

use fs_err as fs;
use image::ImageFormat;

use crate::{
    cover::{COVER_FORMATS, Cover},
    error::BasicError,
    files::{Cache, copy_atomically, list_dir, safe_rename_all, write_atomically},
    game::{Game, Save, is_valid_fat_name, is_valid_file_name, renamed_file_name},
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
            newest_first.then_with(|| b.name.cmp(&a.name))
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
        stem(&self.name)
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

#[derive(Debug, thiserror::Error)]
pub enum OpenLibraryError {
    #[error("no home folder; give a --library")]
    NoHome,
    #[error("no library at {}", .path.display())]
    NoLibrary { path: PathBuf },
    #[error(transparent)]
    Other(#[from] BasicError),
}

#[derive(Debug, thiserror::Error)]
pub enum AddGameError {
    /// Another game of the platform has this name, whatever its extension or
    /// case. Saves and covers are kept by the name without the extension, and a
    /// file system may not tell cases apart.
    #[error("another game in the library has its name")]
    NameTaken,
    /// The library can't hold a file with this name, because it would end up
    /// elsewhere or be hidden and not found again.
    #[error("the library can't hold a game of that name")]
    BadName,
    #[error(transparent)]
    Other(#[from] BasicError),
}

/// Why a game in the library wasn't renamed, in which case nothing has changed.
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
#[derive(Debug, Clone, thiserror::Error)]
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
    pub(crate) fn core_inventory(&self) -> PathBuf {
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
    cache: Cache,
    /// Sorted by platform id and name.
    entries: Vec<LocalEntry>,
}

impl Library {
    /// The library at `root`, a folder that must exist, with every game in
    /// it, each once.
    pub fn open(root: &Path) -> Result<Self, OpenLibraryError> {
        if !root.is_dir() {
            return Err(OpenLibraryError::NoLibrary {
                path: root.to_path_buf(),
            });
        }
        let layout = LibraryLayout {
            root: root.to_path_buf(),
        };
        let roms = layout.roms();
        fs::create_dir_all(&roms).map_err(BasicError::from)?;
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
        for id in list_dir(&all)?.dirs {
            let platform = Platform::from_id(&id);
            let dir = self.layout.platform_saves(&platform);
            for name in list_dir(&dir)?.dirs {
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
        let saves = self.read_save_history(platform, stem(name))?;
        let i = self.index_of(platform, name)?;
        self.entries[i].save = saves;
        Ok(())
    }

    /// Copies the file at `from`, which holds `game`, into the library as the
    /// game called `name`.
    ///
    /// This never replaces a file, so it fails if one is already there or if
    /// another game of the platform has that name, whatever its extension or
    /// case. The copy isn't read, as `game` is what gets recorded.
    pub(crate) fn add_game(
        &mut self,
        game: &Game,
        name: &str,
        from: &Path,
    ) -> Result<LocalEntry, AddGameError> {
        if !is_valid_file_name(name) {
            return Err(AddGameError::BadName);
        }
        let platform = &game.platform;
        let lowercase = stem(name).to_lowercase();
        let taken = |entry: &LocalEntry| {
            &entry.game.platform == platform && entry.stem().to_lowercase() == lowercase
        };
        let to = self.layout.rom(platform, name);
        if self.entries.iter().any(taken) || to.exists() {
            return Err(AddGameError::NameTaken);
        }
        copy_atomically(from, &to)?;
        self.cache.record_game(&to, game)?;
        self.cache.save();
        let entry = LocalEntry {
            name: name.into(),
            cover_path: self.cover_path(platform, stem(name)),
            save: self.read_save_history(platform, stem(name))?,
            game: game.clone(),
        };
        self.insert(entry.clone());
        Ok(entry)
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
        let new_name = renamed_file_name(name, new);
        if !is_valid_fat_name(&new_name) {
            return Err(RenameInLibraryError::BadName);
        }
        if new_name == name {
            return Err(RenameInLibraryError::SameName);
        }
        let (old_stem, new_stem) = (stem(name), stem(&new_name));
        let lowercase = new_stem.to_lowercase();
        let taken = |(other_index, entry): (usize, &LocalEntry)| {
            other_index != i
                && &entry.game.platform == platform
                && entry.stem().to_lowercase() == lowercase
        };
        if self.entries.iter().enumerate().any(taken) {
            return Err(RenameInLibraryError::NameTaken);
        }
        let (from, to) = (
            self.layout.rom(platform, name),
            self.layout.rom(platform, &new_name),
        );
        let saves = self.layout.saves(platform, old_stem);
        let new_saves = self.layout.saves(platform, new_stem);
        // If only the case changed, on a file system that doesn't tell cases
        // apart, whatever is at the new name is the game's own.
        if old_stem.to_lowercase() != lowercase {
            if to.exists() {
                return Err(RenameInLibraryError::NameTaken);
            }
            if new_saves.exists() || self.cover_path(platform, new_stem).is_some() {
                return Err(RenameInLibraryError::LeftoversThere);
            }
        }
        let mut renames = Vec::new();
        if saves.exists() {
            renames.push((saves, new_saves));
        }
        if let Some(cover) = &self.entries[i].cover_path {
            let extension = cover.extension().unwrap_or_default().to_string_lossy();
            let file = format!("{new_stem}.{extension}");
            renames.push((cover.clone(), cover.with_file_name(file)));
        }
        renames.push((from.clone(), to.clone()));
        safe_rename_all(&renames)?;
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
                    .save_version(platform, stem(name), &version_name);
                (version_name, path)
            })
            .find(|(_, path)| !path.exists())
            .expect("some name is free");
        write_atomically(&path, &bytes, Some(modified))?;
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
        let new_path = self.layout.cover(platform, stem(name), cover.format());
        let old_path = self.entries[i].cover_path.clone();
        if old_path.as_ref() == Some(&new_path) {
            trash::delete(&new_path)?;
        }
        write_atomically(&new_path, cover.bytes(), None)?;
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
        for id in list_dir(&roms)?.dirs {
            let platform = Platform::from_id(&id);
            let dir = self.layout.platform_roms(&platform);
            for name in list_dir(&dir)?.files {
                let path = self.layout.rom(&platform, &name);
                let game = self.cache.read_game(&platform, &path)?;
                seen.insert(path);
                let cover_path = self.cover_path(&platform, stem(&name));
                let save = self.read_save_history(&platform, stem(&name))?;
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
        COVER_FORMATS
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
        for file_name in list_dir(&dir)?.files {
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

fn stem(name: &str) -> &str {
    Path::new(name)
        .file_stem()
        .and_then(|file_stem| file_stem.to_str())
        .unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cover::testing::PNG;
    use crate::files::testing::{at, put, put_at};
    use crate::game::{Files, Rom};

    mod renaming {
        use super::*;

        /// A library with `Old.gba`, its cover, two versions of its save, and
        /// `Other.gba`.
        fn with_game() -> (tempfile::TempDir, Library) {
            let (dir, mut library) = library();
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

        fn names(library: &Library) -> Vec<&str> {
            library
                .entries()
                .iter()
                .map(|entry| entry.name.as_str())
                .collect()
        }

        /// What is under the library's `roms`, `covers` and `saves`.
        fn all_files(root: &Path) -> Vec<String> {
            let mut files = Vec::new();
            for dir in [
                "roms/gba",
                "covers/gba",
                "saves/gba",
                "saves/gba/Old",
                "saves/gba/New",
            ] {
                for file in list_dir(&root.join(dir)).unwrap().files {
                    files.push(format!("{dir}/{file}"));
                }
            }
            files.sort();
            files
        }

        #[test]
        fn moves_file_cover_and_saves() {
            let (dir, mut library) = with_game();
            let root = dir.path();
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
        }

        #[test]
        fn renames_game_without_cover_or_saves() {
            let (dir, mut library) = library_with(&[("roms/gba/Old.gba", b"old")]);
            let entry = rename(&mut library, "New").unwrap();
            assert_eq!((entry.cover_path, entry.save), (None, None));
            assert!(dir.path().join("roms/gba/New.gba").exists());
        }

        #[test]
        fn keeps_entries_sorted() {
            let (_dir, mut library) = with_game();
            rename(&mut library, "Zzz").unwrap();
            assert_eq!(names(&library), ["Other.gba", "Zzz.gba"]);
            library
                .rename_game(&Platform::Gba, "Zzz.gba", "Aaa")
                .unwrap();
            assert_eq!(names(&library), ["Aaa.gba", "Other.gba"]);
        }

        #[test]
        fn does_not_reread_renamed_game() {
            let (dir, mut library) = with_game();
            let before = library.entries()[0].game.clone();
            rename(&mut library, "New").unwrap();
            // Same size and time with other contents, so the cached game is
            // used.
            put_at(dir.path(), "roms/gba/New.gba", b"new", 1_000);
            library.reload().unwrap();
            assert_eq!(library.entries()[0].game, before);
        }

        #[test]
        fn allows_case_only_change() {
            let (dir, mut library) = with_game();
            let entry = rename(&mut library, "OLD").unwrap();
            assert_eq!(entry.name, "OLD.gba");
            assert_eq!(entry.save.unwrap().iter().count(), 2);
            let names = list_dir(&dir.path().join("roms/gba")).unwrap().files;
            assert!(names.contains(&"OLD.gba".to_string()), "{names:?}");
        }

        #[test]
        fn moves_nothing_when_refused() {
            for (new, leftover, refusal) in [
                ("a:b", None, "the library can't hold a game of that name"),
                ("Old", None, "it has that name already"),
                ("Other", None, "another game in the library has that name"),
                ("other", None, "another game in the library has that name"),
                (
                    "New",
                    Some("saves/gba/New/x.sav"),
                    "the library has saves or a cover under that name",
                ),
                (
                    "New",
                    Some("covers/gba/New.jpg"),
                    "the library has saves or a cover under that name",
                ),
            ] {
                let (dir, mut library) = with_game();
                if let Some(leftover) = leftover {
                    put(dir.path(), leftover, b"not its own");
                }
                let before = all_files(dir.path());
                let refused = rename(&mut library, new).unwrap_err();
                assert_eq!(refused.to_string(), refusal, "{new}");
                assert_eq!(all_files(dir.path()), before, "{new}");
                assert_eq!(names(&library), ["Old.gba", "Other.gba"]);
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

    /// A library holding `files`.
    fn library_with(files: &[(&str, &[u8])]) -> (tempfile::TempDir, Library) {
        let dir = tempfile::tempdir().unwrap();
        for (path, bytes) in files {
            put(dir.path(), path, bytes);
        }
        let library = Library::open(dir.path()).unwrap();
        (dir, library)
    }

    /// An empty library. Tests put files in it and then reload.
    fn library() -> (tempfile::TempDir, Library) {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path()).unwrap();
        (dir, library)
    }

    /// Adds the file at `from`, hashing it to find out which game it holds.
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
        fn does_not_create_missing_dir() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("typo");
            assert!(matches!(
                Library::open(&root),
                Err(OpenLibraryError::NoLibrary { .. })
            ));
            assert!(!root.exists());
        }

        #[test]
        fn rejects_duplicate_game() {
            let dir = tempfile::tempdir().unwrap();
            let first = put(dir.path(), "roms/gba/Mine.gba", b"same");
            let second = put(dir.path(), "roms/gba/Other.gba", b"same");
            let Err(OpenLibraryError::Other(BasicError::SameGameInLibrary { copies })) =
                Library::open(dir.path())
            else {
                panic!()
            };
            assert_eq!(copies, [(first, second)]);
        }

        #[test]
        fn keeps_entries_when_reload_finds_duplicate() {
            let (_dir, mut library) = library_with(&[("roms/gba/Mine.gba", b"same")]);
            put(library.root(), "roms/gba/Other.gba", b"same");
            assert!(matches!(
                library.reload(),
                Err(BasicError::SameGameInLibrary { .. })
            ));
            assert_eq!(library.entries().len(), 1);
        }

        #[test]
        fn treats_same_contents_on_other_platform_as_other_game() {
            let dir = tempfile::tempdir().unwrap();
            put(dir.path(), "roms/gb/Tetris.gb", b"same");
            put(dir.path(), "roms/gbc/Tetris.gbc", b"same");
            assert_eq!(Library::open(dir.path()).unwrap().entries().len(), 2);
        }

        #[test]
        fn finds_game_with_cover_and_saves() {
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
            let sizes: Vec<_> = entry.save.iter().flat_map(LibrarySaves::iter).collect();
            let sizes: Vec<_> = sizes.iter().map(|version| version.save.size).collect();
            assert_eq!(sizes, [2, 1]);
        }

        #[test]
        fn counts_only_sav_files_as_saves() {
            let (_dir, library) = library_with(&[
                ("roms/gba/Tetris.gba", b"hello"),
                ("saves/gba/Tetris/notes.txt", b"x"),
                ("saves/gba/Tetris/._a.sav", b"x"),
            ]);
            assert_eq!(library.entries()[0].save, None);
        }

        #[test]
        fn rereads_forgotten_game_instead_of_using_cache() {
            let (_dir, mut library) = library();
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

    mod add_game {
        use super::*;

        #[test]
        fn rejects_name_differing_only_by_extension_or_case() {
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
        fn keeps_existing_file_not_yet_scanned() {
            let (_dir, mut library) = library();
            let kept = put(library.root(), "roms/gba/Tetris.gba", b"kept");
            let elsewhere = tempfile::tempdir().unwrap();
            let from = put(elsewhere.path(), "x.gba", b"other");
            let added = add(&mut library, &Platform::Gba, "Tetris.gba", &from);
            assert!(matches!(added, Err(AddGameError::NameTaken)));
            assert_eq!(fs::read(&kept).unwrap(), b"kept");
        }

        #[test]
        fn allows_same_name_on_other_platform() {
            let (_dir, mut library) = library_with(&[("roms/gba/Foo.gba", b"gba")]);
            let elsewhere = tempfile::tempdir().unwrap();
            let from = put(elsewhere.path(), "x.gb", b"gb");
            add(&mut library, &Platform::Gb, "Foo.gb", &from).unwrap();
            assert_eq!(library.entries().len(), 2);
        }

        #[test]
        fn rejects_path_or_hidden_name() {
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
        fn records_given_game_without_rereading_file() {
            let (_dir, mut library) = library();
            let elsewhere = tempfile::tempdir().unwrap();
            let from = put(elsewhere.path(), "x.gba", b"hello");
            // This isn't what the file holds, because only the given game is
            // recorded.
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
        fn inserts_game_in_sorted_position() {
            let (_dir, mut library) =
                library_with(&[("roms/gba/B.gba", b"b"), ("roms/gba/D.gba", b"d")]);
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
            let names = |library: &Library| {
                library
                    .entries()
                    .iter()
                    .map(|entry| entry.name.clone())
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
            let (dir, library) = library_with(&[("roms/gba/Tetris.gba", b"hello")]);
            (dir, library)
        }

        /// Adds a file holding `bytes`, written at `secs`, as a save of Tetris.
        fn add_save(
            library: &mut Library,
            bytes: &[u8],
            secs: u64,
        ) -> Result<Option<LibrarySave>, BasicError> {
            let elsewhere = tempfile::tempdir().unwrap();
            let from = put_at(elsewhere.path(), "Tetris.sav", bytes, secs);
            library.add_save(&Platform::Gba, "Tetris.gba", &from)
        }

        #[test]
        fn names_version_file_after_modified_time() {
            let (_dir, mut library) = with_tetris();
            let added = add_save(&mut library, b"save", 1_762_179_605).unwrap();
            assert_eq!(added.unwrap().save.size, 4);
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
            assert_eq!(in_memory.unwrap().newest().save.size, 4);
        }

        #[test]
        fn keeps_both_saves_from_same_second() {
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
        fn returns_error_for_game_not_in_library() {
            let (_dir, mut library) = library();
            assert!(matches!(
                add_save(&mut library, b"save", 1_000),
                Err(BasicError::NotInLibrary)
            ));
        }

        #[test]
        fn finds_version_file_by_name() {
            let (_dir, mut library) = with_tetris();
            let version = add_save(&mut library, b"save", 1_000).unwrap().unwrap();
            assert_eq!(version.name, "1970-01-01T00-16-40");
            let entry = &library.entries()[0];
            assert_eq!(
                library.file_of_version(entry, &version),
                Some(
                    library
                        .root()
                        .join("saves/gba/Tetris/1970-01-01T00-16-40.sav")
                )
            );
            let other = LibrarySave {
                name: "other".to_string(),
                save: version.save.clone(),
            };
            assert_eq!(library.file_of_version(entry, &other), None);
        }

        #[test]
        fn names_second_version_of_same_second_with_a_number() {
            let (_dir, mut library) = with_tetris();
            add_save(&mut library, b"one", 1_000).unwrap();
            let second = add_save(&mut library, b"two", 1_000).unwrap().unwrap();
            assert_eq!(second.name, "1970-01-01T00-16-40-2");
            let saves = library.entries()[0].save.as_ref().unwrap();
            assert_eq!(saves.version_named("1970-01-01T00-16-40-2"), Some(&second));
            assert_eq!(saves.version_named("never"), None);
        }

        #[test]
        fn sorts_history_newest_first() {
            let version = |name: &str, bytes: &[u8], secs: u64| LibrarySave {
                name: name.to_string(),
                save: Save::from_bytes(bytes, at(secs)),
            };
            let saves = LibrarySaves::from_saves(vec![
                version("a", b"a", 1),
                version("b", b"b", 3),
                version("c", b"c", 2),
            ])
            .unwrap();
            let names: Vec<_> = saves.iter().map(|version| version.name.as_str()).collect();
            assert_eq!(names, ["b", "c", "a"]);
            assert_eq!(saves.newest().name, "b");
            assert!(saves.contains(&Save::from_bytes(b"c", at(9))));
            assert!(!saves.contains(&Save::from_bytes(b"d", at(2))));
            assert_eq!(LibrarySaves::from_saves(Vec::new()), None);
        }
    }

    mod files {
        use super::*;

        #[test]
        fn adds_cover_where_reload_finds_it() {
            let (_dir, mut library) = library_with(&[("roms/gba/Tetris.gba", b"hello")]);
            let cover = Cover::from_bytes(PNG.to_vec()).unwrap();
            library
                .set_cover(&Platform::Gba, "Tetris.gba", &cover)
                .unwrap();
            let path = library.root().join("covers/gba/Tetris.png");
            assert_eq!(library.entries()[0].cover_path, Some(path.clone()));
            assert_eq!(fs::read(&path).unwrap(), PNG);
            let in_memory = library.entries().to_vec();
            library.reload().unwrap();
            assert_eq!(library.entries(), in_memory);
        }

        #[test]
        fn lists_rom_and_cover_as_game_files() {
            let (_dir, library) = library_with(&[
                ("roms/gba/Tetris.gba", b"hello"),
                ("covers/gba/Tetris.png", PNG),
                ("saves/gba/Tetris/notes.txt", b"x"),
                ("roms/gba/Other.gba", b"other"),
            ]);
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
        fn lists_every_save_version_file() {
            let (_dir, mut library) = library();
            put(library.root(), "roms/gba/Tetris.gba", b"hello");
            let old = put_at(library.root(), "saves/gba/Tetris/old.sav", b"a", 1_000);
            let new = put_at(library.root(), "saves/gba/Tetris/new.sav", b"b", 2_000);
            put(library.root(), "saves/gba/Tetris/notes.txt", b"x");
            library.reload().unwrap();
            let entry = library.entries()[0].clone();
            assert_eq!(library.files_of_all_saves(&entry).unwrap(), [new, old]);
        }

        #[test]
        fn reports_no_save_and_rejects_missing_version_without_trashing() {
            let (_dir, mut library) = library_with(&[("roms/gba/Tetris.gba", b"hello")]);
            let entry = library.entries()[0].clone();
            assert!(matches!(
                library.trash_all_saves(&entry),
                Ok(TrashSaveAllOutcome::NoSave)
            ));
            assert!(matches!(
                library.trash_save_version(&entry, "never"),
                Err(TrashSaveError::NoSuchVersion)
            ));
        }

        #[test]
        fn rereads_remaining_saves() {
            let (_dir, mut library) = library();
            put(library.root(), "roms/gba/Tetris.gba", b"hello");
            put_at(library.root(), "saves/gba/Tetris/old.sav", b"a", 1_000);
            let new = put_at(library.root(), "saves/gba/Tetris/new.sav", b"bb", 2_000);
            library.reload().unwrap();
            fs::remove_file(&new).unwrap();
            library.reread_saves(&Platform::Gba, "Tetris.gba").unwrap();
            let saves = library.entries()[0].save.clone().unwrap();
            assert_eq!(saves.iter().count(), 1);
            assert_eq!(saves.newest().save.size, 1);
        }
    }

    mod orphan_saves {
        use super::*;

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

        #[test]
        fn lists_every_version_file_of_orphan() {
            let (_dir, mut library) = library();
            let old = put_at(library.root(), "saves/gba/Gone/old.sav", b"a", 1_000);
            let new = put_at(library.root(), "saves/gba/Gone/new.sav", b"b", 2_000);
            library.reload().unwrap();
            let orphan = library.find_orphan_saves().unwrap().remove(0);
            assert_eq!(library.files_of_orphan_save(&orphan).unwrap(), [new, old]);
        }

        #[test]
        fn refuses_to_trash_orphan_after_game_comes_back() {
            let (_dir, mut library) = library();
            let save = put_at(library.root(), "saves/gba/Gone/old.sav", b"a", 1_000);
            library.reload().unwrap();
            let orphan = library.find_orphan_saves().unwrap().remove(0);
            put(library.root(), "roms/gba/Gone.gba", b"hello");
            library.reload().unwrap();
            assert!(matches!(
                library.trash_orphan_save(&orphan),
                Err(TrashOrphanSaveError::NoOrphanSave)
            ));
            assert!(save.exists());
        }
    }
}
