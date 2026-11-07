//! A Pocket and the games on it.

use std::{
    collections::HashSet,
    fmt, io,
    path::{Path, PathBuf},
    process::Command,
};

use bytesize::ByteSize;
use fs_err as fs;
use serde::Deserialize;

use crate::{
    cores::{
        BiosRow, BiosStatus, CoreFile, CoreZip, InstalledCore, PutBiosError, RemoveCoreError,
        parse_core_files, parse_core_json,
    },
    error::SharedError,
    files::{
        Cache, copy_atomically, list_dir, list_files_recursive, safe_rename_all, write_atomically,
    },
    game::{Game, Save, is_valid_fat_name, renamed_file_name},
    platform::Platform,
};

/// What a folder must have at its root to be a Pocket.
const MARKERS: [&str; 3] = ["Analogue_Pocket.json", "Assets", "Cores"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceEntry {
    /// Its path under `Assets/<platform>/common/`, e.g. `RPG/Pokemon.gba`. Its
    /// save is at the same path under `Saves/<platform>/common/`, with the
    /// extension `.sav`.
    pub path: PathBuf,
    pub game: Game,
    pub save: Option<Save>,
}

impl DeviceEntry {
    fn is_at(&self, platform: &Platform, path: &Path) -> bool {
        &self.game.platform == platform && self.path == path
    }
}

/// What a scan found besides the games.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanReport {
    /// Copies of a game at a later path than its first copy. Everything else
    /// ignores them, since only the first copy's save is synced.
    pub duplicates: Vec<DeviceEntry>,
}

/// Why a game on the device wasn't renamed, in which case nothing has changed.
#[derive(Debug, thiserror::Error)]
pub enum RenameOnDeviceError {
    #[error("the device can't hold a game of that name")]
    BadName,
    #[error("it has that name already")]
    SameName,
    /// A file beside it has that name, whatever its extension or case. On the
    /// device they would share a save.
    #[error("another file on the device has that name")]
    NameTaken,
    /// The Pocket would give this game the save already at that name.
    #[error("the device has a save under that name")]
    LeftoversThere,
    /// A file beside it has the same name and therefore the same save, so
    /// moving the save would take it from that file.
    #[error("another file beside it shares its save")]
    SharesSave,
    /// A core loads a file of that name, so it isn't a game.
    #[error("a core on the device loads a file of that name")]
    BiosName,
    #[error(transparent)]
    Shared(#[from] SharedError),
}

/// Every path pocketeer reads or writes on a device is spelled out here.
#[derive(Debug, Clone)]
struct DeviceLayout {
    root: PathBuf,
}

impl DeviceLayout {
    fn pocket_json(&self) -> PathBuf {
        self.root.join("Analogue_Pocket.json")
    }

    fn assets(&self) -> PathBuf {
        self.root.join("Assets")
    }

    /// The folder of the games every core of a platform shares.
    fn common(&self, platform: &Platform) -> PathBuf {
        self.assets().join(platform.id()).join("common")
    }

    fn game(&self, platform: &Platform, path: &Path) -> PathBuf {
        self.common(platform).join(path)
    }

    fn saves(&self) -> PathBuf {
        self.root.join("Saves")
    }

    /// The folder of the saves of a platform's games under `common/`.
    fn saves_common(&self, platform: &Platform) -> PathBuf {
        self.saves().join(platform.id()).join("common")
    }

    /// The save of the game at `path` under `common/`, which is the same path
    /// under `Saves/` with the extension `.sav`. This is the Pocket's own rule,
    /// and it means `Foo.pce` and `Foo.sgx` in one folder share `Foo.sav`.
    fn save(&self, platform: &Platform, path: &Path) -> PathBuf {
        self.saves_common(platform).join(path.with_extension("sav"))
    }

    fn cores(&self) -> PathBuf {
        self.root.join("Cores")
    }

    fn core(&self, id: &str) -> PathBuf {
        self.cores().join(id)
    }

    /// Pocketeer's own folder, apart from what the Pocket uses.
    fn pocketeer(&self) -> PathBuf {
        self.root.join(".pocketeer")
    }

    fn cache(&self) -> PathBuf {
        self.pocketeer().join("cache.json")
    }

    /// Holds what is being installed until all of it is there. It is on the
    /// device so that each file only has to be renamed.
    fn staging(&self) -> PathBuf {
        self.pocketeer().join("staging")
    }

    /// Where a platform that has been set aside goes. Being there keeps it set
    /// aside.
    fn archived_platform(&self, file: &Path) -> PathBuf {
        self.root.join("Platforms").join("_archive").join(file)
    }
}

/// A save on the device with no game at its path. The Pocket gives it to any
/// game put at that path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanDeviceSave {
    pub platform: Platform,
    /// Where it is under `Saves/<platform>/common/`, e.g. `RPG/Pokemon.sav`.
    pub path: PathBuf,
    pub save: Save,
}

#[derive(Debug, thiserror::Error)]
pub enum DeleteSaveError {
    #[error("no save on the device")]
    NoSave,
    #[error(transparent)]
    Shared(#[from] SharedError),
}

#[derive(Debug, thiserror::Error)]
pub enum OpenDeviceError {
    /// `path` lacks some of the files and folders every Pocket has.
    #[error("{} is not a Pocket: no {}", .path.display(), .missing.join(", "))]
    NotADevice {
        path: PathBuf,
        missing: Vec<&'static str>,
    },
    #[error("more than one device is mounted: {}", joined_paths(.found))]
    ManyDevices { found: Vec<PathBuf> },
}

fn joined_paths(paths: &[PathBuf]) -> String {
    let paths: Vec<_> = paths
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    paths.join(", ")
}

/// Why the device wasn't ejected. This derives `Clone` because operation
/// results also go in the log.
#[derive(Debug, Clone, thiserror::Error)]
pub enum EjectDeviceError {
    /// The system didn't eject the device, and `message` is what it said.
    #[error("couldn't eject the device: {message}")]
    EjectFailed { message: String },
    #[error(transparent)]
    Shared(#[from] SharedError),
}

/// The firmware the Pocket reports in `Analogue_Pocket.json`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Firmware {
    #[serde(rename = "name")]
    pub version: Option<String>,
    pub build_date: Option<String>,
}

impl fmt::Display for Firmware {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.version.as_deref().unwrap_or("unknown"))?;
        match &self.build_date {
            Some(build_date) => write!(f, " (built {build_date})"),
            None => Ok(()),
        }
    }
}

/// The layout of `Analogue_Pocket.json`, down to the firmware.
#[derive(Deserialize)]
struct PocketJson {
    firmware: PocketJsonFirmware,
}

#[derive(Deserialize)]
struct PocketJsonFirmware {
    runtime: Firmware,
}

/// The space on the whole drive, which also counts files that aren't the
/// Pocket's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskSpace {
    pub total_bytes: u64,
    pub available_bytes: u64,
}

impl fmt::Display for DiskSpace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let available = ByteSize::b(self.available_bytes);
        let total = ByteSize::b(self.total_bytes);
        write!(
            f,
            "{} free of {}",
            available.display().si(),
            total.display().si()
        )
    }
}

/// A Pocket as a mounted drive, either the Pocket itself or its SD card in a
/// reader.
#[derive(Debug, Clone)]
pub struct Device {
    layout: DeviceLayout,
    cache: Cache,
    /// What the last scan found, sorted by platform id and path with each game
    /// once, plus what pocketeer has put on the device since.
    entries: Vec<DeviceEntry>,
    scanned: bool,
}

impl Device {
    /// The roots of the mounted devices, sorted.
    pub fn find_mounted() -> Vec<PathBuf> {
        devices_among(mounted_volumes())
    }

    /// Whether the folder at `root` looks like a device. Reads no more than
    /// that.
    pub fn is_device(root: &Path) -> bool {
        missing_markers(root).is_empty()
    }

    /// The device at `root`, if it looks like one. Reads no games.
    pub fn open(root: &Path) -> Result<Self, OpenDeviceError> {
        let missing = missing_markers(root);
        if !missing.is_empty() {
            return Err(OpenDeviceError::NotADevice {
                path: root.to_path_buf(),
                missing,
            });
        }
        let layout = DeviceLayout {
            root: root.to_path_buf(),
        };
        let cache = Cache::load(&layout.root, &layout.cache());
        Ok(Self {
            layout,
            cache,
            entries: Vec::new(),
            scanned: false,
        })
    }

    pub fn root(&self) -> &Path {
        &self.layout.root
    }

    pub fn entries(&self) -> &[DeviceEntry] {
        &self.entries
    }

    pub(crate) fn has_been_scanned(&self) -> bool {
        self.scanned
    }

    /// The game of `platform` at `path` under `common/`, as of the last scan.
    pub fn entry_at(&self, platform: &Platform, path: &Path) -> Option<&DeviceEntry> {
        self.entries
            .iter()
            .find(|entry| entry.is_at(platform, path))
    }

    /// The device's copy of `game`, wherever it is.
    pub fn entry_for(&self, game: &Game) -> Option<&DeviceEntry> {
        self.entries.iter().find(|entry| &entry.game == game)
    }

    /// Deletes `entry`'s file for good. Its save stays.
    pub(crate) fn remove_game(&mut self, entry: &DeviceEntry) -> Result<(), SharedError> {
        let path = self.game_path(&entry.game.platform, &entry.path);
        fs::remove_file(&path)?;
        self.cache.forget(&path);
        self.cache.save();
        self.entries
            .retain(|existing| !existing.is_at(&entry.game.platform, &entry.path));
        Ok(())
    }

    /// Deletes `entry`'s save for good.
    pub(crate) fn remove_save(&mut self, entry: &DeviceEntry) -> Result<(), DeleteSaveError> {
        let path = self.save_path(entry);
        let removed = match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Err(DeleteSaveError::NoSave),
            Err(error) => return Err(SharedError::from(error).into()),
        };
        if let Some(stored_entry) = self.entry_mut(entry) {
            stored_entry.save = None;
        }
        removed
    }

    /// Deletes `orphan` for good if it is still an orphan, since a game may
    /// have been put at its path.
    pub(crate) fn remove_orphan_save(
        &self,
        orphan: &OrphanDeviceSave,
    ) -> Result<(), DeleteSaveError> {
        let is_it = |candidate: &OrphanDeviceSave| {
            candidate.platform == orphan.platform && candidate.path == orphan.path
        };
        if !self.find_orphan_saves()?.iter().any(is_it) {
            return Err(DeleteSaveError::NoSave);
        }
        let path = self
            .layout
            .saves_common(&orphan.platform)
            .join(&orphan.path);
        fs::remove_file(&path).map_err(SharedError::from)?;
        Ok(())
    }

    /// Finds every game on the device, keeping the first copy of each. A game
    /// whose file is unchanged since the last scan is not read again, and files
    /// a core loads don't count as games.
    pub fn scan(&mut self) -> Result<ScanReport, SharedError> {
        let declared = self.declared_files()?;
        let layout = &self.layout;
        let mut seen = HashSet::new();
        let mut games = HashSet::new();
        let mut entries = Vec::new();
        let mut duplicates = Vec::new();
        let assets = layout.assets();
        for id in list_dir(&assets)?.dirs {
            let platform = Platform::from_id(&id);
            for path in list_files_recursive(&layout.common(&platform))? {
                let full = layout.game(&platform, &path);
                if declared.contains(&full) {
                    continue;
                }
                let game = self.cache.read_game(&platform, &full)?;
                seen.insert(full);
                let save = read_save(&layout.save(&platform, &path))?;
                let entry = DeviceEntry { path, game, save };
                if games.insert(entry.game.clone()) {
                    entries.push(entry);
                } else {
                    duplicates.push(entry);
                }
            }
        }
        self.cache.keep_only(&seen);
        self.cache.save();
        self.entries = entries;
        self.scanned = true;
        Ok(ScanReport { duplicates })
    }

    /// Where a game of `platform` at `path` under `common/` would be.
    pub fn game_path(&self, platform: &Platform, path: &Path) -> PathBuf {
        self.layout.game(platform, path)
    }

    /// Copies the file at `from`, which holds `game`, to `path` under
    /// `common/`, replacing what is there. The cache records its hash, so the
    /// next scan doesn't read the file back.
    pub(crate) fn put_game(
        &mut self,
        game: &Game,
        path: &Path,
        from: &Path,
    ) -> Result<DeviceEntry, SharedError> {
        let to = self.layout.game(&game.platform, path);
        copy_atomically(from, &to)?;
        self.cache.record_game(&to, game)?;
        self.cache.save();
        let entry = DeviceEntry {
            path: path.to_path_buf(),
            game: game.clone(),
            save: read_save(&self.layout.save(&game.platform, path))?,
        };
        self.insert(entry.clone());
        Ok(entry)
    }

    /// Inserts `entry` so that `entries` stays sorted by platform id and path,
    /// replacing any entry already at that path.
    fn insert(&mut self, entry: DeviceEntry) {
        let place = self.entries.binary_search_by(|existing| {
            (existing.game.platform.id(), &existing.path)
                .cmp(&(entry.game.platform.id(), &entry.path))
        });
        match place {
            Ok(at) => self.entries[at] = entry,
            Err(at) => self.entries.insert(at, entry),
        }
    }

    /// Renames `entry` to `new` in the same folder, keeping its extension.
    /// Every file beside its save with the same name moves too, because a core
    /// may keep more than the save there. The contents aren't read again.
    pub(crate) fn rename_game(
        &mut self,
        entry: &DeviceEntry,
        new: &str,
    ) -> Result<DeviceEntry, RenameOnDeviceError> {
        let platform = &entry.game.platform;
        let file = entry.path.file_name().unwrap_or_default().to_string_lossy();
        let new_name = renamed_file_name(&file, new);
        if !is_valid_fat_name(&new_name) {
            return Err(RenameOnDeviceError::BadName);
        }
        if new_name == file {
            return Err(RenameOnDeviceError::SameName);
        }
        let new_path = entry.path.with_file_name(&new_name);
        let (from, to) = (
            self.layout.game(platform, &entry.path),
            self.layout.game(platform, &new_path),
        );
        let declared = self.declared_files()?;
        let lowercase_to = to.to_string_lossy().to_lowercase();
        if declared
            .iter()
            .any(|file| file.to_string_lossy().to_lowercase() == lowercase_to)
        {
            return Err(RenameOnDeviceError::BiosName);
        }
        let stem_of = |name: &str| {
            let stem = Path::new(name).file_stem().unwrap_or_default();
            stem.to_string_lossy().to_lowercase()
        };
        let (old_stem, new_stem) = (stem_of(&file), stem_of(&new_name));
        let dir = from.parent().expect("a game is in a folder");
        let beside = list_dir(dir)?.files;
        let others = || beside.iter().filter(|name| **name != *file);
        if others().any(|name| stem_of(name) == new_stem) {
            return Err(RenameOnDeviceError::NameTaken);
        }
        if others().any(|name| stem_of(name) == old_stem) {
            return Err(RenameOnDeviceError::SharesSave);
        }
        let saves_dir = self
            .layout
            .saves_common(platform)
            .join(entry.path.parent().unwrap_or(Path::new("")));
        let saves = list_dir(&saves_dir)?.files;
        // If only the case changed, those files are the game's own.
        if old_stem != new_stem && saves.iter().any(|name| stem_of(name) == new_stem) {
            return Err(RenameOnDeviceError::LeftoversThere);
        }
        let new_file_stem = Path::new(&new_name).file_stem().unwrap_or_default();
        let mut renames: Vec<(PathBuf, PathBuf)> = saves
            .iter()
            .filter(|name| stem_of(name) == old_stem)
            .map(|name| {
                let name = Path::new(name);
                let mut renamed = new_file_stem.to_os_string();
                if let Some(extension) = name.extension() {
                    renamed.push(".");
                    renamed.push(extension);
                }
                (saves_dir.join(name), saves_dir.join(renamed))
            })
            .collect();
        renames.push((from.clone(), to.clone()));
        safe_rename_all(&renames)?;
        self.cache.forget(&from);
        self.cache.record_game(&to, &entry.game)?;
        self.cache.save();
        self.entries
            .retain(|existing| !existing.is_at(platform, &entry.path));
        let renamed = DeviceEntry {
            path: new_path,
            game: entry.game.clone(),
            save: entry.save.clone(),
        };
        self.insert(renamed.clone());
        Ok(renamed)
    }

    /// Where `entry`'s save is, even if it has none.
    pub fn save_path(&self, entry: &DeviceEntry) -> PathBuf {
        self.layout.save(&entry.game.platform, &entry.path)
    }

    /// The saves on the device with no game, as the folders are now. They are
    /// looked up each time, so the device needn't be scanned.
    pub fn find_orphan_saves(&self) -> Result<Vec<OrphanDeviceSave>, SharedError> {
        let declared = self.declared_files()?;
        let saves = self.layout.saves();
        let mut orphans = Vec::new();
        for id in list_dir(&saves)?.dirs {
            let platform = Platform::from_id(&id);
            let games = list_files_recursive(&self.layout.common(&platform))?;
            let games: Vec<PathBuf> = games
                .iter()
                .filter(|path| !declared.contains(&self.layout.game(&platform, path)))
                .map(|path| path.with_extension(""))
                .collect();
            let dir = self.layout.saves_common(&platform);
            for path in list_files_recursive(&dir)? {
                let is_save = path.extension().is_some_and(|extension| extension == "sav");
                if !is_save || games.contains(&path.with_extension("")) {
                    continue;
                }
                let full = dir.join(&path);
                orphans.push(OrphanDeviceSave {
                    platform: platform.clone(),
                    save: Save::read(&full)?,
                    path,
                });
            }
        }
        Ok(orphans)
    }

    /// `entry`'s save as it is now, which may differ from what the scan saw, or
    /// none if it has none.
    pub fn read_save(&self, entry: &DeviceEntry) -> Result<Option<Save>, SharedError> {
        read_save(&self.save_path(entry))
    }

    /// Makes the file at `from` into `entry`'s save, keeping `from`'s write
    /// time so that it counts as the same version.
    pub(crate) fn put_save(&mut self, entry: &DeviceEntry, from: &Path) -> Result<(), SharedError> {
        let modified = fs::metadata(from).and_then(|metadata| metadata.modified())?;
        let bytes = fs::read(from)?;
        write_atomically(&self.save_path(entry), &bytes, Some(modified))?;
        if let Some(stored_entry) = self.entry_mut(entry) {
            stored_entry.save = Some(Save::from_bytes(&bytes, modified));
        }
        Ok(())
    }

    fn entry_mut(&mut self, entry: &DeviceEntry) -> Option<&mut DeviceEntry> {
        self.entries
            .iter_mut()
            .find(|existing| existing.is_at(&entry.game.platform, &entry.path))
    }

    /// The cores on the device, as the folders are now. One whose `core.json`
    /// can't be read has no version.
    pub fn installed_cores(&self) -> Result<Vec<InstalledCore>, SharedError> {
        let dir = self.layout.cores();
        let ids = list_dir(&dir)?.dirs;
        Ok(ids
            .into_iter()
            .map(|id| {
                let json = fs::read(self.layout.core(&id).join("core.json")).ok();
                let (version, platforms) = json
                    .and_then(|bytes| parse_core_json(&bytes))
                    .map_or((None, Vec::new()), |(version, path)| (Some(version), path));
                InstalledCore {
                    id,
                    version,
                    platforms,
                }
            })
            .collect())
    }

    /// Installs `zip`.
    ///
    /// Everything is written beside the device's files before any of them
    /// changes, so an unreadable zip or a full device changes nothing. Each
    /// file is then renamed into place with `core.json` last, so a core stopped
    /// halfway still reports its old version and installing it again finishes
    /// the job.
    pub(crate) fn install_core(&self, zip: &mut CoreZip) -> Result<(), SharedError> {
        let staging = self.layout.staging();
        match fs::remove_dir_all(&staging) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.into()),
            _ => {}
        }
        let installed = self.stage_and_place(zip, &staging);
        let _ = fs::remove_dir_all(&staging);
        installed
    }

    fn stage_and_place(&self, zip: &mut CoreZip, staging: &Path) -> Result<(), SharedError> {
        let files: Vec<_> = zip
            .files()
            .into_iter()
            .filter(|(path, _)| self.can_install_at(path))
            .collect();
        for (path, name) in &files {
            let staged = staging.join(path);
            zip.extract(name, &staged)?;
        }
        for (path, _) in &files {
            let to = self.root().join(path);
            if let Some(dir) = to.parent() {
                fs::create_dir_all(dir)?;
            }
            fs::rename(staging.join(path), &to)?;
        }
        Ok(())
    }

    /// Whether a file from a core's zip should be written at `path`. What the
    /// device already has in `Platforms/` is kept, because cores share it and
    /// it may have been changed by hand. A platform that was set aside stays
    /// set aside.
    fn can_install_at(&self, path: &Path) -> bool {
        let Ok(in_platforms) = path.strip_prefix("Platforms") else {
            return true;
        };
        !self.root().join(path).exists() && !self.layout.archived_platform(in_platforms).exists()
    }

    /// Deletes the core's own folder for good. Anything else it uses stays.
    pub(crate) fn remove_core(&self, id: &str) -> Result<(), RemoveCoreError> {
        if !self.installed_cores()?.iter().any(|core| core.id == id) {
            return Err(RemoveCoreError::NotInstalled);
        }
        let path = self.layout.core(id);
        fs::remove_dir_all(&path).map_err(SharedError::from)?;
        Ok(())
    }

    /// The files the cores on the device load, and the core that loads each.
    fn core_files(&self) -> Result<Vec<(String, CoreFile)>, SharedError> {
        let mut files = Vec::new();
        for core in self.installed_cores()? {
            let json = fs::read(self.layout.core(&core.id).join("data.json")).unwrap_or_default();
            let declared = parse_core_files(&core.platforms, &json);
            files.extend(declared.into_iter().map(|file| (core.id.clone(), file)));
        }
        Ok(files)
    }

    /// Where the files the cores on the device load are, even if they
    /// aren't there.
    fn declared_files(&self) -> Result<HashSet<PathBuf>, SharedError> {
        let files = self.core_files()?;
        Ok(files
            .iter()
            .map(|(_, file)| self.layout.game(&file.platform, &file.path))
            .collect())
    }

    /// The files the cores on the device load, by platform and path, as the
    /// folders are now.
    pub fn read_bios_rows(&self) -> Result<Vec<BiosRow>, SharedError> {
        let mut rows: Vec<BiosRow> = Vec::new();
        for (core, file) in self.core_files()? {
            let same = |row: &&mut BiosRow| row.platform == file.platform && row.path == file.path;
            if let Some(row) = rows.iter_mut().find(same) {
                row.needed_by.push((core, file.required));
                row.size_exact = row.size_exact.filter(|_| row.size_exact == file.size_exact);
                continue;
            }
            rows.push(BiosRow {
                platform: file.platform,
                path: file.path,
                needed_by: vec![(core, file.required)],
                size_exact: file.size_exact,
                status: BiosStatus::Missing,
            });
        }
        for row in &mut rows {
            let path = self.layout.game(&row.platform, &row.path);
            row.status = match fs::metadata(&path) {
                Ok(meta) if row.size_exact.is_some_and(|size| size != meta.len()) => {
                    BiosStatus::WrongSize { size: meta.len() }
                }
                Ok(_) => BiosStatus::Present,
                Err(error) if error.kind() == io::ErrorKind::NotFound => BiosStatus::Missing,
                Err(error) => return Err(error.into()),
            };
        }
        rows.sort_by(|a, b| (a.platform.id(), &a.path).cmp(&(b.platform.id(), &b.path)));
        Ok(rows)
    }

    /// Copies the file at `from` to where a core loads the file at `path` from.
    /// Nothing else can be written this way, and nothing is replaced.
    pub(crate) fn put_bios(
        &self,
        platform: &Platform,
        path: &Path,
        from: &Path,
    ) -> Result<(), PutBiosError> {
        let rows = self.read_bios_rows()?;
        let row = rows
            .iter()
            .find(|row| &row.platform == platform && row.path == path);
        let row = row.ok_or(PutBiosError::NotDeclared)?;
        if row.status != BiosStatus::Missing {
            return Err(PutBiosError::AlreadyThere);
        }
        let actual = fs::metadata(from).map_err(SharedError::from)?.len();
        match row.size_exact {
            Some(expected) if expected != actual => {
                Err(PutBiosError::WrongSize { expected, actual })
            }
            _ => Ok(copy_atomically(from, &self.layout.game(platform, path))?),
        }
    }

    /// None if `Analogue_Pocket.json` can't be read or doesn't have the firmware.
    pub fn read_firmware(&self) -> Option<Firmware> {
        let bytes = fs::read(self.layout.pocket_json()).ok()?;
        let json: PocketJson = serde_json::from_slice(&bytes).ok()?;
        Some(json.firmware.runtime)
    }

    pub fn read_disk_space(&self) -> Result<DiskSpace, SharedError> {
        let root = &self.layout.root;
        Ok(DiskSpace {
            total_bytes: fs4::total_space(root)?,
            available_bytes: fs4::available_space(root)?,
        })
    }

    /// Has the system eject the device, as its file manager would.
    pub fn eject(&self) -> Result<(), EjectDeviceError> {
        let root = self.root();
        match std::env::consts::OS {
            "macos" => run_command(Command::new("diskutil").arg("eject").arg(root)),
            "linux" => {
                let mut findmnt = Command::new("findmnt");
                findmnt.args(["-n", "-o", "SOURCE", "--target"]).arg(root);
                let block = run_command(&mut findmnt)?;
                let block = block.trim();
                run_command(Command::new("udisksctl").args(["unmount", "-b", block]))?;
                run_command(Command::new("udisksctl").args(["power-off", "-b", block]))
            }
            "windows" => {
                // The drive, e.g. `E:`, as Explorer's Eject wants it.
                let drive = root.to_string_lossy();
                let drive = drive.trim_end_matches(['\\', '/']);
                let script = format!(
                    "(New-Object -ComObject Shell.Application).Namespace(17).ParseName('{drive}').InvokeVerb('Eject')"
                );
                run_command(Command::new("powershell").args(["-NoProfile", "-Command", &script]))
            }
            os => Err(EjectDeviceError::EjectFailed {
                message: format!("ejecting a device is not supported on {os}"),
            }),
        }
        .map(drop)
    }
}

/// Every volume mounted now, by each system's conventions. On Windows these are
/// the drives, and elsewhere the folders inside the folders that volumes are
/// mounted in.
fn mounted_volumes() -> Vec<PathBuf> {
    let in_dirs = |dirs: &[PathBuf]| -> Vec<PathBuf> {
        dirs.iter()
            .filter_map(|dir| fs::read_dir(dir).ok())
            .flatten()
            .filter_map(|entry| entry.ok().map(|dir_entry| dir_entry.path()))
            .collect()
    };
    match std::env::consts::OS {
        "macos" => in_dirs(&[PathBuf::from("/Volumes")]),
        "linux" => {
            let user = std::env::var("USER").unwrap_or_default();
            in_dirs(&[
                Path::new("/media").join(&user),
                Path::new("/run/media").join(&user),
                PathBuf::from("/media"),
                PathBuf::from("/mnt"),
            ])
        }
        "windows" => (b'A'..=b'Z')
            .map(|drive_letter| PathBuf::from(format!("{}:\\", drive_letter as char)))
            .collect(),
        _ => Vec::new(),
    }
}

fn devices_among(volumes: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut devices: Vec<PathBuf> = volumes
        .into_iter()
        .filter(|path| missing_markers(path).is_empty())
        .collect();
    devices.sort();
    devices.dedup();
    devices
}

fn missing_markers(root: &Path) -> Vec<&'static str> {
    MARKERS
        .into_iter()
        .filter(|marker| !root.join(marker).exists())
        .collect()
}

/// Runs `command` and returns what it printed if it succeeded.
fn run_command(command: &mut Command) -> Result<String, EjectDeviceError> {
    let eject_failed = |message: String| EjectDeviceError::EjectFailed { message };
    let out = command
        .output()
        .map_err(|error| eject_failed(error.to_string()))?;
    if !out.status.success() {
        let said = [&out.stderr[..], &out.stdout[..]].concat();
        return Err(eject_failed(
            String::from_utf8_lossy(&said).trim().to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn read_save(path: &Path) -> Result<Option<Save>, SharedError> {
    match Save::read(path) {
        Ok(save) => Ok(Some(save)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::testing::{at, put, put_at};
    use crate::game::Files;

    fn scanned(device: &mut Device) -> Vec<DeviceEntry> {
        device.scan().unwrap();
        device.entries().to_vec()
    }

    fn empty_device() -> (tempfile::TempDir, Device) {
        let dir = tempfile::tempdir().unwrap();
        put(dir.path(), "Analogue_Pocket.json", b"{}");
        fs::create_dir(dir.path().join("Assets")).unwrap();
        fs::create_dir(dir.path().join("Cores")).unwrap();
        let device = Device::open(dir.path()).unwrap();
        (dir, device)
    }

    fn paths(entries: &[DeviceEntry]) -> Vec<PathBuf> {
        entries.iter().map(|entry| entry.path.clone()).collect()
    }

    fn crc32s(entries: &[DeviceEntry]) -> Vec<u32> {
        entries
            .iter()
            .map(|entry| match entry.game.files {
                Files::Single(rom) => rom.crc32,
            })
            .collect()
    }

    mod scan {
        use super::*;

        #[test]
        fn rejects_dir_without_device_markers() {
            let dir = tempfile::tempdir().unwrap();
            fs::create_dir(dir.path().join("Assets")).unwrap();
            match Device::open(dir.path()) {
                Err(OpenDeviceError::NotADevice { missing, .. }) => {
                    assert_eq!(missing, ["Analogue_Pocket.json", "Cores"])
                }
                other => panic!("{other:?}"),
            }
        }

        #[test]
        fn finds_only_volumes_that_are_devices() {
            let (root, _) = empty_device();
            let other = tempfile::tempdir().unwrap();
            let volumes = vec![
                other.path().to_path_buf(),
                root.path().to_path_buf(),
                root.path().join("Assets"),
                PathBuf::from("/not/mounted"),
                root.path().to_path_buf(),
            ];
            assert_eq!(devices_among(volumes), [root.path().to_path_buf()]);
        }

        #[test]
        fn finds_games_at_any_depth_with_saves() {
            let (dir, mut device) = empty_device();
            put(dir.path(), "Assets/gba/common/Tetris.gba", b"hello");
            put(dir.path(), "Assets/gba/common/RPG/Pokemon.gba", b"hi");
            put(dir.path(), "Saves/gba/common/RPG/Pokemon.sav", b"save");
            let entries = scanned(&mut device);
            assert_eq!(
                paths(&entries),
                [
                    PathBuf::from("RPG/Pokemon.gba"),
                    PathBuf::from("Tetris.gba")
                ]
            );
            assert_eq!(entries[0].game.platform, Platform::Gba);
            assert_eq!(crc32s(&entries), [0xd8932aac, 0x3610a686]);
            assert_eq!(entries[0].save.as_ref().unwrap().size, 4);
            assert_eq!(entries[1].save, None);
        }

        #[test]
        fn ignores_hidden_files_and_other_dirs() {
            let (dir, mut device) = empty_device();
            put(dir.path(), "Assets/gba/common/._Tetris.gba", b"x");
            put(dir.path(), "Assets/gba/common/.Trash/Tetris.gba", b"x");
            put(dir.path(), "Assets/gba/agg23.GBA/Tetris.gba", b"x");
            put(dir.path(), "Tetris.gba", b"x");
            assert_eq!(scanned(&mut device), []);
        }

        #[test]
        fn keeps_first_copy_and_reports_duplicates() {
            let (dir, mut device) = empty_device();
            put(dir.path(), "Assets/gba/common/One.gba", b"hello");
            put(dir.path(), "Assets/gba/common/RPG/Two.gba", b"hello");
            let report = device.scan().unwrap();
            assert_eq!(paths(device.entries()), [PathBuf::from("One.gba")]);
            assert_eq!(paths(&report.duplicates), [PathBuf::from("RPG/Two.gba")]);
        }

        #[test]
        fn treats_same_contents_on_other_platform_as_other_game() {
            let (dir, mut device) = empty_device();
            put(dir.path(), "Assets/gb/common/Tetris.gb", b"hello");
            put(dir.path(), "Assets/gbc/common/Tetris.gbc", b"hello");
            assert_eq!(device.scan().unwrap(), ScanReport::default());
            assert_eq!(device.entries().len(), 2);
        }
    }

    mod cache {
        use super::*;

        #[test]
        fn reuses_unchanged_file_after_reopening() {
            let (dir, mut device) = empty_device();
            let path = "Assets/gba/common/Tetris.gba";
            put_at(dir.path(), path, b"hello", 1_000_000);
            scanned(&mut device);
            // Same size and time with other contents, so the cached game is
            // used.
            put_at(dir.path(), path, b"jello", 1_000_000);
            let mut reopened = Device::open(dir.path()).unwrap();
            assert_eq!(crc32s(&scanned(&mut reopened)), [0x3610a686]);
            // A new time, so it is read again.
            put_at(dir.path(), path, b"jello", 1_000_002);
            assert_eq!(crc32s(&scanned(&mut reopened)), [0x4cd0f5e6]);
        }

        #[test]
        fn rereads_file_at_path_gone_at_last_scan() {
            let (dir, mut device) = empty_device();
            let path = "Assets/gba/common/Tetris.gba";
            let file = put_at(dir.path(), path, b"hello", 1_000_000);
            scanned(&mut device);
            fs::remove_file(&file).unwrap();
            assert_eq!(scanned(&mut device), []);
            // The same size and time as the file gone.
            put_at(dir.path(), path, b"jello", 1_000_000);
            assert_eq!(crc32s(&scanned(&mut device)), [0x4cd0f5e6]);
        }

        #[test]
        fn rereads_file_at_path_of_removed_game() {
            let (dir, mut device) = empty_device();
            let path = "Assets/gba/common/Tetris.gba";
            put_at(dir.path(), path, b"hello", 1_000_000);
            let entry = scanned(&mut device).remove(0);
            device.remove_game(&entry).unwrap();
            // The same size and time as the file removed.
            put_at(dir.path(), path, b"jello", 1_000_000);
            assert_eq!(crc32s(&scanned(&mut device)), [0x4cd0f5e6]);
        }

        #[test]
        fn ignores_corrupt_cache_file() {
            let (dir, mut device) = empty_device();
            put(dir.path(), "Assets/gba/common/Tetris.gba", b"hello");
            put(dir.path(), ".pocketeer/cache.json", b"{ not json");
            assert_eq!(crc32s(&scanned(&mut device)), [0x3610a686]);
        }
    }

    mod writing {
        use super::*;

        #[test]
        fn puts_save_where_scan_finds_it() {
            let (_dir, mut device) = empty_device();
            put(device.root(), "Assets/gba/common/RPG/Pokemon.gba", b"hi");
            let entry = scanned(&mut device).remove(0);
            assert_eq!(
                device.save_path(&entry),
                device.root().join("Saves/gba/common/RPG/Pokemon.sav")
            );
            let elsewhere = tempfile::tempdir().unwrap();
            let from = put_at(elsewhere.path(), "x.sav", b"save", 1_000);
            device.put_save(&entry, &from).unwrap();
            let save = device.read_save(&entry).unwrap().unwrap();
            assert_eq!((save.size, save.modified), (4, at(1_000)));
            assert_eq!(device.entries()[0].save, Some(save.clone()));
            assert_eq!(scanned(&mut device)[0].save, Some(save));
        }

        #[test]
        fn lists_put_game_without_rereading() {
            let (_dir, mut device) = empty_device();
            let elsewhere = tempfile::tempdir().unwrap();
            let from = put(elsewhere.path(), "Tetris.gba", b"hello");
            let game = Game::read(Platform::Gba, &from).unwrap();
            let put_there = device
                .put_game(&game, Path::new("Tetris.gba"), &from)
                .unwrap();
            let path = device.root().join("Assets/gba/common/Tetris.gba");
            assert_eq!(fs::read(&path).unwrap(), b"hello");
            // Listed at once, without a scan.
            assert_eq!(device.entries(), std::slice::from_ref(&put_there));
            // Other contents of the same size and time, so the cached game is
            // used.
            let time = fs::metadata(&path).unwrap().modified().unwrap();
            fs::write(&path, b"jello").unwrap();
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(time)
                .unwrap();
            assert_eq!(scanned(&mut device), [put_there]);
        }

        /// A device with `One.gba`, its save, and `Two.gba`, along with
        /// `One.gba`'s entry.
        fn with_two_games() -> (tempfile::TempDir, Device, DeviceEntry) {
            let (dir, mut device) = empty_device();
            let root = dir.path();
            put(root, "Assets/gba/common/One.gba", b"hello");
            put(root, "Assets/gba/common/Two.gba", b"other");
            put(root, "Saves/gba/common/One.sav", b"save");
            let entry = scanned(&mut device).remove(0);
            (dir, device, entry)
        }

        #[test]
        fn removes_save_from_device() {
            let (dir, mut device, entry) = with_two_games();
            device.remove_save(&entry).unwrap();
            assert!(!dir.path().join("Saves/gba/common/One.sav").exists());
            assert_eq!(device.entries()[0].save, None);
        }

        #[test]
        fn rejects_removing_save_already_gone() {
            let (_dir, mut device, entry) = with_two_games();
            device.remove_save(&entry).unwrap();
            assert!(matches!(
                device.remove_save(&entry),
                Err(DeleteSaveError::NoSave)
            ));
        }

        #[test]
        fn removes_game_and_leaves_its_save_and_other_games() {
            let (dir, mut device, entry) = with_two_games();
            let save = put(dir.path(), "Saves/gba/common/One.sav", b"save");
            device.remove_game(&entry).unwrap();
            assert!(!dir.path().join("Assets/gba/common/One.gba").exists());
            assert!(dir.path().join("Assets/gba/common/Two.gba").exists());
            assert!(save.exists());
            assert_eq!(paths(device.entries()), [PathBuf::from("Two.gba")]);
        }
    }

    mod cores {
        use super::*;
        use crate::cores::testing::{core_json, release, zip_of};

        fn install(device: &Device, bytes: Vec<u8>) -> Result<(), SharedError> {
            let mut zip = CoreZip::parse(bytes, "a.B").unwrap();
            device.install_core(&mut zip)
        }

        fn version(device: &Device) -> Option<String> {
            device.installed_cores().unwrap().remove(0).version
        }

        #[test]
        fn lists_core_from_core_json() {
            let (_dir, device) = empty_device();
            put(
                device.root(),
                "Cores/a.B/core.json",
                core_json("1.2").as_bytes(),
            );
            assert_eq!(
                device.installed_cores().unwrap(),
                [InstalledCore {
                    id: "a.B".into(),
                    version: Some("1.2".into()),
                    platforms: vec![Platform::Gba],
                }]
            );
        }

        #[test]
        fn reports_no_version_without_readable_core_json() {
            let (_dir, device) = empty_device();
            put(device.root(), "Cores/a.B/core.rbf_r", b"bits");
            assert_eq!(version(&device), None);
            put(device.root(), "Cores/a.B/core.json", b"{ not json");
            assert_eq!(version(&device), None);
        }

        #[test]
        fn writes_only_core_and_platform_files() {
            let (_dir, device) = empty_device();
            install(&device, release("1.0")).unwrap();
            let root = device.root();
            assert_eq!(version(&device).unwrap(), "1.0");
            assert_eq!(fs::read(root.join("Cores/a.B/core.rbf_r")).unwrap(), b"1.0");
            assert!(root.join("Platforms/gba.json").exists());
            assert!(root.join("Platforms/_images/gba.bin").exists());
            for left_out in [
                "README.md",
                "docs",
                "Saves",
                "Assets/gba",
                "Cores/other.C",
                "Cores/a.B/.DS_Store",
                "__MACOSX",
                ".pocketeer/staging",
            ] {
                assert!(!root.join(left_out).exists(), "{left_out}");
            }
        }

        #[test]
        fn replaces_core_and_keeps_platform_files() {
            let (_dir, device) = empty_device();
            install(&device, release("1.0")).unwrap();
            let root = device.root();
            let mine = put(root, "Platforms/_images/gba.bin", b"my own image");
            install(&device, release("2.0")).unwrap();
            assert_eq!(version(&device).unwrap(), "2.0");
            assert_eq!(fs::read(root.join("Cores/a.B/core.rbf_r")).unwrap(), b"2.0");
            assert_eq!(fs::read(mine).unwrap(), b"my own image");
        }

        #[test]
        fn skips_platform_json_archived_by_user() {
            let (_dir, device) = empty_device();
            let root = device.root();
            put(root, "Platforms/_archive/gba.json", b"set aside");
            install(&device, release("1.0")).unwrap();
            assert!(!root.join("Platforms/gba.json").exists());
            assert!(root.join("Platforms/_images/gba.bin").exists());
        }

        #[test]
        fn keeps_old_version_until_interrupted_install_is_retried() {
            let (_dir, device) = empty_device();
            install(&device, release("1.0")).unwrap();
            let root = device.root();
            // A folder with something in it can't be replaced by a file.
            let bits = root.join("Cores/a.B/core.rbf_r");
            fs::remove_file(&bits).unwrap();
            put(root, "Cores/a.B/core.rbf_r/in the way", b"x");
            assert!(install(&device, release("2.0")).is_err());
            assert_eq!(version(&device).unwrap(), "1.0");
            assert!(!root.join(".pocketeer/staging").exists());
            fs::remove_dir_all(&bits).unwrap();
            install(&device, release("2.0")).unwrap();
            assert_eq!(version(&device).unwrap(), "2.0");
            assert_eq!(fs::read(bits).unwrap(), b"2.0");
        }

        #[test]
        fn keeps_old_version_when_staging_fails() {
            let (_dir, device) = empty_device();
            install(&device, release("1.0")).unwrap();
            // One path is both a file and a folder, so the new version can't be
            // written out.
            let bytes = zip_of(&[
                ("Cores/a.B/core.json", core_json("2.0").as_bytes()),
                ("Cores/a.B/core.rbf_r", b"new bits"),
                ("Cores/a.B/both", b"file"),
                ("Cores/a.B/both/inside", b"file"),
            ]);
            let stayed = install(&device, bytes);
            assert!(stayed.is_err());
            assert_eq!(version(&device).unwrap(), "1.0");
            let core_file = device.root().join("Cores/a.B/core.rbf_r");
            assert_eq!(fs::read(core_file).unwrap(), b"1.0");
        }

        #[test]
        fn removes_only_core_folder() {
            let (_dir, device) = empty_device();
            let root = device.root();
            install(&device, release("1.0")).unwrap();
            let settings = put(root, "Settings/a.B/mine.json", b"mine");
            let other = put(root, "Cores/other.C/core.json", b"{}");
            device.remove_core("a.B").unwrap();
            assert!(!root.join("Cores/a.B").exists());
            assert!(settings.exists() && other.exists());
            assert!(root.join("Platforms/gba.json").exists());
        }

        #[test]
        fn rejects_removing_path_that_is_not_installed_core() {
            let (_dir, device) = empty_device();
            let root = device.root();
            let marker = root.join("Analogue_Pocket.json");
            for id in ["a.B", "..", "../Assets", "../Analogue_Pocket.json"] {
                assert!(
                    matches!(device.remove_core(id), Err(RemoveCoreError::NotInstalled)),
                    "{id}"
                );
            }
            assert!(marker.exists() && root.join("Assets").exists());
        }
    }

    mod renaming {
        use super::*;

        /// A device with `RPG/Old.gba`, its save and clock, and others.
        fn with_game() -> (tempfile::TempDir, Device, DeviceEntry) {
            let (dir, mut device) = empty_device();
            let root = dir.path();
            put_at(root, "Assets/gba/common/RPG/Old.gba", b"old", 1_000);
            put(root, "Assets/gba/common/Other.gba", b"other");
            put_at(root, "Saves/gba/common/RPG/Old.sav", b"save", 1_000);
            put(root, "Saves/gba/common/RPG/Old.rtc", b"clock");
            put(root, "Saves/gba/common/Old.sav", b"another folder's");
            let entry = scanned(&mut device)
                .into_iter()
                .find(|entry| entry.path == Path::new("RPG/Old.gba"))
                .unwrap();
            (dir, device, entry)
        }

        fn all_files(root: &Path) -> Vec<PathBuf> {
            let mut files = list_files_recursive(&root.join("Assets")).unwrap();
            files.extend(list_files_recursive(&root.join("Saves")).unwrap());
            files
        }

        #[test]
        fn moves_game_and_files_beside_save_within_folder() {
            let (dir, mut device, entry) = with_game();
            let root = dir.path();
            let renamed = device.rename_game(&entry, "New").unwrap();
            assert_eq!(renamed.path, Path::new("RPG/New.gba"));
            assert_eq!(renamed.save, entry.save);
            assert_eq!(
                all_files(root),
                [
                    PathBuf::from("gba/common/Other.gba"),
                    PathBuf::from("gba/common/RPG/New.gba"),
                    PathBuf::from("gba/common/Old.sav"),
                    PathBuf::from("gba/common/RPG/New.rtc"),
                    PathBuf::from("gba/common/RPG/New.sav"),
                ]
            );
            assert_eq!(
                fs::read(root.join("Saves/gba/common/RPG/New.rtc")).unwrap(),
                b"clock"
            );
        }

        #[test]
        fn matches_fresh_scan_after_renaming() {
            let (_dir, mut device, entry) = with_game();
            let renamed = device.rename_game(&entry, "New").unwrap();
            let listed = device.entries().to_vec();
            assert_eq!(scanned(&mut device), listed);
            assert!(listed.contains(&renamed));
        }

        #[test]
        fn does_not_reread_renamed_game() {
            let (dir, mut device, entry) = with_game();
            device.rename_game(&entry, "New").unwrap();
            // Same size and time with other contents, so the cached game is
            // used.
            put_at(dir.path(), "Assets/gba/common/RPG/New.gba", b"new", 1_000);
            let found = scanned(&mut device);
            let found = found
                .iter()
                .find(|entry| entry.path == Path::new("RPG/New.gba"));
            assert_eq!(found.unwrap().game, entry.game);
        }

        #[test]
        fn allows_case_only_change() {
            let (dir, mut device, entry) = with_game();
            let renamed = device.rename_game(&entry, "OLD").unwrap();
            assert_eq!(renamed.path, Path::new("RPG/OLD.gba"));
            let root = dir.path();
            let saves = list_dir(&root.join("Saves/gba/common/RPG")).unwrap().files;
            assert_eq!(saves, ["OLD.rtc", "OLD.sav"]);
        }

        #[test]
        fn moves_nothing_when_refused() {
            for (new, extra, refusal) in [
                ("a:b", None, "the device can't hold a game of that name"),
                ("Old", None, "it has that name already"),
                (
                    "Neighbor",
                    Some("Assets/gba/common/RPG/Neighbor.gba"),
                    "another file on the device has that name",
                ),
                (
                    "neighbor",
                    Some("Assets/gba/common/RPG/Neighbor.pce"),
                    "another file on the device has that name",
                ),
                (
                    "New",
                    Some("Saves/gba/common/RPG/New.rtc"),
                    "the device has a save under that name",
                ),
                (
                    "New",
                    Some("Assets/gba/common/RPG/Old.sgx"),
                    "another file beside it shares its save",
                ),
            ] {
                let (dir, mut device, entry) = with_game();
                let root = dir.path();
                if let Some(extra) = extra {
                    put(root, extra, b"in the way");
                }
                let before = all_files(root);
                let refused = device.rename_game(&entry, new).unwrap_err();
                assert_eq!(refused.to_string(), refusal, "{new}");
                assert_eq!(all_files(root), before, "{new}");
            }
        }

        #[test]
        fn rejects_name_of_file_core_loads() {
            let (dir, mut device, _) = with_game();
            let root = dir.path();
            let core_json = r#"{"core":{"metadata":{"version":"1","platform_ids":["gba"]}}}"#;
            let data = r#"{"data":{"data_slots":[{"filename":"Bios.gba"}]}}"#;
            put(root, "Cores/a.B/core.json", core_json.as_bytes());
            put(root, "Cores/a.B/data.json", data.as_bytes());
            // A file a core loads sits at the top of `common/`.
            put(root, "Assets/gba/common/Mine.gba", b"mine");
            let entry = scanned(&mut device)
                .into_iter()
                .find(|entry| entry.path == Path::new("Mine.gba"))
                .unwrap();
            let before = all_files(root);
            let refused = device.rename_game(&entry, "Bios").unwrap_err();
            assert!(matches!(refused, RenameOnDeviceError::BiosName));
            assert_eq!(all_files(root), before);
        }

        #[test]
        fn restores_everything_on_failure() {
            let (dir, mut device, entry) = with_game();
            let root = dir.path();
            let before = all_files(root);
            // The new name fits a file ending in `.a` but is too long with
            // `.sav`. The `.a` file moves first, so this checks that it is
            // moved back.
            put(root, "Saves/gba/common/RPG/Old.a", b"short");
            let new = "x".repeat(253);
            assert!(matches!(
                device.rename_game(&entry, &new),
                Err(RenameOnDeviceError::Shared(_))
            ));
            let mut expected = before;
            expected.push(PathBuf::from("gba/common/RPG/Old.a"));
            expected.sort();
            let mut after = all_files(root);
            after.sort();
            assert_eq!(after, expected);
            assert!(device.entries().contains(&entry));
        }
    }

    mod bios {
        use super::*;
        use crate::cores::testing::data_json;

        const BIOS: &str = r#"{"filename":"gba_bios.bin","required":true,"size_exact":4}"#;

        /// Core `id` on the device, for `platform`, with these slots.
        fn core(device: &Device, id: &str, platform: &str, slots: &str) {
            let core_json = format!(
                r#"{{"core":{{"metadata":{{"version":"1","platform_ids":["{platform}"]}}}}}}"#
            );
            let root = device.root();
            put(root, &format!("Cores/{id}/core.json"), core_json.as_bytes());
            let data = data_json(slots);
            put(root, &format!("Cores/{id}/data.json"), data.as_bytes());
        }

        fn status(device: &Device) -> BiosStatus {
            device.read_bios_rows().unwrap().remove(0).status
        }

        #[test]
        fn merges_file_loaded_by_several_cores_into_one_row() {
            let (_dir, device) = empty_device();
            core(&device, "a.GBA", "gba", BIOS);
            core(
                &device,
                "b.GBA",
                "gba",
                r#"{"filename":"gba_bios.bin","size_exact":4}"#,
            );
            core(&device, "a.GB", "gb", r#"{"filename":"gb_bios.bin"}"#);
            core(&device, "a.GG", "gg", r#"{"name":"Cartridge"}"#);
            assert_eq!(
                device.read_bios_rows().unwrap(),
                [
                    BiosRow {
                        platform: Platform::Gb,
                        path: PathBuf::from("gb_bios.bin"),
                        needed_by: vec![("a.GB".into(), false)],
                        size_exact: None,
                        status: BiosStatus::Missing,
                    },
                    BiosRow {
                        platform: Platform::Gba,
                        path: PathBuf::from("gba_bios.bin"),
                        needed_by: vec![("a.GBA".into(), true), ("b.GBA".into(), false)],
                        size_exact: Some(4),
                        status: BiosStatus::Missing,
                    },
                ]
            );
        }

        #[test]
        fn requires_no_size_when_cores_disagree() {
            for sizes in [[4, 8, 4], [8, 4, 4], [4, 4, 8]] {
                let (_dir, device) = empty_device();
                for (core_id, size) in ["a.A", "b.B", "c.C"].iter().zip(sizes) {
                    let slot = format!(r#"{{"filename":"x.bin","size_exact":{size}}}"#);
                    core(&device, core_id, "gba", &slot);
                }
                assert_eq!(
                    device.read_bios_rows().unwrap()[0].size_exact,
                    None,
                    "{sizes:?}"
                );
            }
        }

        #[test]
        fn leaves_out_core_that_cannot_be_read() {
            let (_dir, device) = empty_device();
            core(&device, "a.GBA", "gba", BIOS);
            put(device.root(), "Cores/broken.A/core.json", b"{ not json");
            core(&device, "broken.B", "gba", BIOS);
            put(device.root(), "Cores/broken.B/data.json", b"{ not json");
            let rows = device.read_bios_rows().unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].needed_by, [("a.GBA".to_string(), true)]);
        }

        #[test]
        fn reports_missing_present_or_wrong_size() {
            let (_dir, device) = empty_device();
            core(&device, "a.GBA", "gba", BIOS);
            assert_eq!(status(&device), BiosStatus::Missing);
            let file = put(device.root(), "Assets/gba/common/gba_bios.bin", b"four");
            assert_eq!(status(&device), BiosStatus::Present);
            fs::write(file, b"five!").unwrap();
            assert_eq!(status(&device), BiosStatus::WrongSize { size: 5 });
        }

        mod putting {
            use super::*;

            /// A device with a core that loads `gba_bios.bin`, and a file of
            /// `bytes` to put there.
            fn ready(bytes: &[u8]) -> (tempfile::TempDir, Device, tempfile::TempDir, PathBuf) {
                let (dir, device) = empty_device();
                core(&device, "a.GBA", "gba", BIOS);
                let elsewhere = tempfile::tempdir().unwrap();
                let from = put(elsewhere.path(), "my copy.bin", bytes);
                (dir, device, elsewhere, from)
            }

            const PATH: &str = "gba_bios.bin";

            #[test]
            fn copies_file_where_core_looks_and_leaves_source() {
                let (_dir, device, _elsewhere, from) = ready(b"four");
                device
                    .put_bios(&Platform::Gba, Path::new(PATH), &from)
                    .unwrap();
                let to = device.root().join("Assets/gba/common/gba_bios.bin");
                assert_eq!(fs::read(to).unwrap(), b"four");
                assert_eq!(fs::read(from).unwrap(), b"four");
                assert_eq!(status(&device), BiosStatus::Present);
            }

            #[test]
            fn rejects_file_already_on_device() {
                let (_dir, device, _elsewhere, from) = ready(b"four");
                let there = put(device.root(), "Assets/gba/common/gba_bios.bin", b"mine!");
                assert!(matches!(
                    device.put_bios(&Platform::Gba, Path::new(PATH), &from),
                    Err(PutBiosError::AlreadyThere)
                ));
                assert_eq!(fs::read(there).unwrap(), b"mine!");
            }

            #[test]
            fn rejects_file_of_wrong_size() {
                let (_dir, device, _elsewhere, from) = ready(b"five!");
                assert!(matches!(
                    device.put_bios(&Platform::Gba, Path::new(PATH), &from),
                    Err(PutBiosError::WrongSize {
                        expected: 4,
                        actual: 5
                    })
                ));
                assert!(!device.root().join("Assets/gba").exists());
            }

            #[test]
            fn rejects_file_no_core_declares() {
                let (_dir, device, _elsewhere, from) = ready(b"four");
                for (platform, path) in [
                    (Platform::Gba, "other.bin"),
                    (Platform::Gba, "common/gba_bios.bin"),
                    (Platform::Gba, "../../Analogue_Pocket.json"),
                    (Platform::Gb, PATH),
                ] {
                    assert!(
                        matches!(
                            device.put_bios(&platform, Path::new(path), &from),
                            Err(PutBiosError::NotDeclared)
                        ),
                        "{path}"
                    );
                }
                assert_eq!(
                    fs::read(device.root().join("Analogue_Pocket.json")).unwrap(),
                    b"{}"
                );
                assert!(!device.root().join("Assets/gba").exists());
            }
        }

        mod scanning {
            use super::*;

            #[test]
            fn excludes_only_declared_bios_path_from_games() {
                let (dir, mut device) = empty_device();
                core(&device, "a.GBA", "gba", BIOS);
                put(dir.path(), "Assets/gba/common/gba_bios.bin", b"four");
                put(dir.path(), "Assets/gba/common/Sonic.bin", b"game");
                // The same file name in another folder isn't where the core
                // looks, so it counts as a game.
                put(dir.path(), "Assets/gba/common/RPG/gba_bios.bin", b"other");
                assert_eq!(
                    paths(&scanned(&mut device)),
                    [
                        PathBuf::from("RPG/gba_bios.bin"),
                        PathBuf::from("Sonic.bin"),
                    ]
                );
            }

            #[test]
            fn treats_save_of_bios_file_as_orphan() {
                let (_dir, device) = empty_device();
                core(&device, "a.GBA", "gba", BIOS);
                let root = device.root();
                put(root, "Assets/gba/common/gba_bios.bin", b"four");
                put(root, "Assets/gba/common/Kept.gba", b"game");
                put(root, "Saves/gba/common/gba_bios.sav", b"save");
                put(root, "Saves/gba/common/Kept.sav", b"save");
                let orphans = device.find_orphan_saves().unwrap();
                let found: Vec<_> = orphans.iter().map(|orphan| orphan.path.clone()).collect();
                assert_eq!(found, [PathBuf::from("gba_bios.sav")]);
            }
        }
    }

    mod firmware {
        use super::*;

        fn firmware_in(json: &[u8]) -> Option<Firmware> {
            let (dir, device) = empty_device();
            put(dir.path(), "Analogue_Pocket.json", json);
            device.read_firmware()
        }

        #[test]
        fn reads_version_and_build_date() {
            let firmware = firmware_in(
                br#"{"product": "Analogue Pocket", "firmware": {"runtime":
                    {"name": "2.6", "byte": 38, "build_date": "2026-06-16"}}}"#,
            );
            assert_eq!(
                firmware,
                Some(Firmware {
                    version: Some("2.6".to_string()),
                    build_date: Some("2026-06-16".to_string()),
                })
            );
        }

        #[test]
        fn reads_firmware_without_build_date() {
            let firmware = firmware_in(br#"{"firmware": {"runtime": {"name": "2.6"}}}"#);
            assert_eq!(firmware.unwrap().to_string(), "2.6");
        }

        #[test]
        fn returns_none_without_readable_firmware_section() {
            assert_eq!(firmware_in(b"{}"), None);
            assert_eq!(firmware_in(b"{ not json"), None);
        }

        #[test]
        fn shows_version_and_build_date() {
            let firmware = Firmware {
                version: Some("2.6".to_string()),
                build_date: Some("2026-06-16".to_string()),
            };
            assert_eq!(firmware.to_string(), "2.6 (built 2026-06-16)");
        }
    }

    mod disk_space {
        use super::*;

        #[test]
        fn shows_space_in_decimal_units() {
            let space = DiskSpace {
                total_bytes: 128_000_000_000,
                available_bytes: 12_300_000_000,
            };
            assert_eq!(space.to_string(), "12.3 GB free of 128.0 GB");
        }
    }

    mod orphan_saves {
        use super::*;

        #[test]
        fn lists_saves_without_game() {
            let (_dir, device) = empty_device();
            let root = device.root();
            put(root, "Assets/gba/common/RPG/Kept.gba", b"hello");
            put(root, "Saves/gba/common/RPG/Kept.sav", b"has a game");
            put(root, "Saves/gba/common/RPG/Gone.sav", b"none");
            put(root, "Saves/gba/common/._Gone.sav", b"hidden");
            put(root, "Saves/gba/common/notes.txt", b"not a save");
            // This platform has no folder under `Assets/`, and the device
            // hasn't been scanned.
            put(root, "Saves/gb/common/Gone Too.sav", b"none");
            let orphans = device.find_orphan_saves().unwrap();
            let found: Vec<_> = orphans
                .iter()
                .map(|orphan| {
                    (
                        orphan.platform.clone(),
                        orphan.path.clone(),
                        orphan.save.size,
                    )
                })
                .collect();
            assert_eq!(
                found,
                [
                    (Platform::Gb, PathBuf::from("Gone Too.sav"), 4),
                    (Platform::Gba, PathBuf::from("RPG/Gone.sav"), 4),
                ]
            );
        }

        #[test]
        fn removes_orphan_save() {
            let (dir, device) = empty_device();
            let save = put(dir.path(), "Saves/gba/common/RPG/Gone.sav", b"save");
            let orphan = device.find_orphan_saves().unwrap().remove(0);
            device.remove_orphan_save(&orphan).unwrap();
            assert!(!save.exists());
        }

        #[test]
        fn rejects_removing_save_with_game() {
            let (dir, device) = empty_device();
            let save = put(dir.path(), "Saves/gba/common/Tetris.sav", b"save");
            let orphan = device.find_orphan_saves().unwrap().remove(0);
            put(dir.path(), "Assets/gba/common/Tetris.gba", b"hello");
            assert!(matches!(
                device.remove_orphan_save(&orphan),
                Err(DeleteSaveError::NoSave)
            ));
            assert!(save.exists());
        }
    }
}
