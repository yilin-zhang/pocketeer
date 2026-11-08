//! A Pocket and the games on it.

use std::{
    collections::HashSet,
    fmt, io,
    path::{Path, PathBuf},
};

use bytesize::ByteSize;
use fs_err as fs;
use serde::Deserialize;

use crate::{
    cores::{
        self, BiosRow, BiosStatus, CoreFile, CoreZip, InstalledCore, PutBiosError, RemoveCoreError,
    },
    error::BasicError,
    game::{Game, RomCache, Save},
    os::{self, FileNameExt},
    platform::Platform,
};

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

/// As `<platform>/<path>`, the way the command line, the log and messages
/// name it.
impl fmt::Display for DeviceEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.game.platform.id(), self.path.display())
    }
}

/// What a scan found besides the games.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanReport {
    /// Copies of a game at a later path than its first copy. Everything else
    /// ignores them, since only the first copy's save is synced.
    pub duplicates: Vec<DeviceEntry>,
}

/// Why a game on the device wasn't renamed. Nothing has changed, unless the rename
/// failed halfway and putting the files back failed as well.
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
    Other(#[from] BasicError),
}

/// Every path Pocketeer reads or writes on a device is spelled out here.
#[derive(Debug, Clone)]
struct DeviceLayout {
    root: PathBuf,
}

impl DeviceLayout {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
        }
    }

    /// Whether the root has what every Pocket has there. A card gets these the
    /// first time it is in a Pocket that is turned on.
    fn is_device(&self) -> bool {
        [self.pocket_json(), self.assets(), self.cores()]
            .iter()
            .all(|path| path.exists())
    }

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

/// As `<platform>/<path>`, the way the command line, the log and messages
/// name it.
impl fmt::Display for OrphanDeviceSave {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.platform.id(), self.path.display())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteSaveOutcome {
    Deleted,
    /// The game has no save on the device.
    NoSave,
}

#[derive(Debug, thiserror::Error)]
pub enum DeleteOrphanSaveError {
    /// The orphan save is gone, or a game has taken it.
    #[error("the device has no such orphan save")]
    NoOrphanSave,
    #[error(transparent)]
    Other(#[from] BasicError),
}

#[derive(Debug, thiserror::Error)]
pub enum OpenDeviceError {
    #[error("{} is not an Analogue Pocket device", .0.display())]
    NotADevice(PathBuf),
    #[error("more than one device is mounted: {}", joined_paths(.0))]
    ManyDevices(Vec<PathBuf>),
}

fn joined_paths(paths: &[PathBuf]) -> String {
    let paths: Vec<_> = paths
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    paths.join(", ")
}

/// Why the device wasn't ejected.
#[derive(Debug, thiserror::Error)]
pub enum EjectDeviceError {
    /// The system didn't eject the device, and this is what it said.
    #[error("couldn't eject the device: {0}")]
    EjectFailed(String),
    #[error(transparent)]
    Other(#[from] BasicError),
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
    cache: RomCache,
    /// What the last scan found, sorted by platform id and path with each game
    /// once, plus what Pocketeer has put on the device since.
    entries: Vec<DeviceEntry>,
    scanned: bool,
}

impl Device {
    /// The roots of the mounted devices, sorted.
    pub fn find_mounted() -> Vec<PathBuf> {
        Self::roots_among(os::mounted_volumes())
    }

    /// The roots of the devices among `volumes`, sorted, each once.
    fn roots_among(volumes: Vec<PathBuf>) -> Vec<PathBuf> {
        let mut roots: Vec<PathBuf> = volumes
            .into_iter()
            .filter(|path| Self::is_device(path))
            .collect();
        roots.sort();
        roots.dedup();
        roots
    }

    /// Whether the folder at `root` looks like a device. Reads no more than
    /// that.
    pub fn is_device(root: &Path) -> bool {
        DeviceLayout::new(root).is_device()
    }

    /// The device at `root`, if it looks like one. Reads no games.
    pub fn open(root: &Path) -> Result<Self, OpenDeviceError> {
        let layout = DeviceLayout::new(root);
        if !layout.is_device() {
            return Err(OpenDeviceError::NotADevice(layout.root));
        }
        let cache = RomCache::load(&layout.root, &layout.cache());
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
    pub(crate) fn remove_game(&mut self, entry: &DeviceEntry) -> Result<(), BasicError> {
        let path = self.game_path(&entry.game.platform, &entry.path);
        fs::remove_file(&path)?;
        self.cache.forget(&path);
        self.cache.save();
        self.entries
            .retain(|existing| !existing.is_at(&entry.game.platform, &entry.path));
        Ok(())
    }

    /// Deletes `entry`'s save for good.
    pub(crate) fn remove_save(
        &mut self,
        entry: &DeviceEntry,
    ) -> Result<DeleteSaveOutcome, BasicError> {
        let path = self.save_path(entry);
        let removed = match fs::remove_file(&path) {
            Ok(()) => DeleteSaveOutcome::Deleted,
            Err(error) if error.kind() == io::ErrorKind::NotFound => DeleteSaveOutcome::NoSave,
            Err(error) => return Err(error.into()),
        };
        if let Some(stored_entry) = self.entry_mut(entry) {
            stored_entry.save = None;
        }
        Ok(removed)
    }

    /// Deletes `orphan` for good if it is still an orphan, since a game may
    /// have been put at its path.
    pub(crate) fn remove_orphan_save(
        &self,
        orphan: &OrphanDeviceSave,
    ) -> Result<(), DeleteOrphanSaveError> {
        let is_it = |candidate: &OrphanDeviceSave| {
            candidate.platform == orphan.platform && candidate.path == orphan.path
        };
        if !self.find_orphan_saves()?.iter().any(is_it) {
            return Err(DeleteOrphanSaveError::NoOrphanSave);
        }
        let path = self
            .layout
            .saves_common(&orphan.platform)
            .join(&orphan.path);
        fs::remove_file(&path).map_err(BasicError::from)?;
        Ok(())
    }

    /// Finds every game on the device, keeping the first copy of each. A game
    /// whose file is unchanged since the last scan is not read again, and files
    /// a core loads don't count as games.
    pub fn scan(&mut self) -> Result<ScanReport, BasicError> {
        let declared = self.declared_files()?;
        let layout = &self.layout;
        let mut seen = HashSet::new();
        let mut games = HashSet::new();
        let mut entries = Vec::new();
        let mut duplicates = Vec::new();
        let assets = layout.assets();
        for id in os::list_dir(&assets)?.dirs {
            let platform = Platform::from_id(&id);
            for path in os::list_files_recursive(&layout.common(&platform))? {
                let full = layout.game(&platform, &path);
                if declared.contains(&full) {
                    continue;
                }
                let game = self.cache.read_game(&platform, &full)?;
                seen.insert(full);
                let save = Save::read_if_exists(&layout.save(&platform, &path))?;
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
    ) -> Result<DeviceEntry, BasicError> {
        let to = self.layout.game(&game.platform, path);
        os::copy_atomically(from, &to)?;
        self.cache.record_game(&to, game)?;
        self.cache.save();
        let entry = DeviceEntry {
            path: path.to_path_buf(),
            game: game.clone(),
            save: Save::read_if_exists(&self.layout.save(&game.platform, path))?,
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
        let new_name = self.check_rename(entry, new)?;
        let platform = &entry.game.platform;
        let new_path = entry.path.with_file_name(&new_name);
        let (from, to) = (
            self.layout.game(platform, &entry.path),
            self.layout.game(platform, &new_path),
        );
        let file = entry.path.file_name().unwrap_or_default().to_string_lossy();
        let (old_stem, new_stem) = (file.stem().to_lowercase(), new_name.stem());
        let save = self.save_path(entry);
        let saves_dir = save.parent().expect("a save is in a folder");
        let mut renames: Vec<(PathBuf, PathBuf)> = os::list_dir(saves_dir)?
            .files
            .iter()
            .filter(|name| name.stem().to_lowercase() == old_stem)
            .map(|name| {
                let renamed = match Path::new(name).extension() {
                    Some(extension) => format!("{new_stem}.{}", extension.to_string_lossy()),
                    None => new_stem.to_string(),
                };
                (saves_dir.join(name), saves_dir.join(renamed))
            })
            .collect();
        renames.push((from.clone(), to.clone()));
        os::safe_rename_all(&renames)?;
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

    /// The file name `entry` gets if it is renamed to `new`, or why it can't
    /// be. Nothing is changed.
    fn check_rename(&self, entry: &DeviceEntry, new: &str) -> Result<String, RenameOnDeviceError> {
        let file = entry.path.file_name().unwrap_or_default().to_string_lossy();
        let new_name = file.with_stem(new);
        if !new_name.is_valid_fat_name() {
            return Err(RenameOnDeviceError::BadName);
        }
        if new_name == file {
            return Err(RenameOnDeviceError::SameName);
        }
        let from = self.layout.game(&entry.game.platform, &entry.path);
        // A core loads a file at the new path, whatever its case.
        let lowercase_to = from
            .with_file_name(&new_name)
            .to_string_lossy()
            .to_lowercase();
        if self
            .declared_files()?
            .iter()
            .any(|file| file.to_string_lossy().to_lowercase() == lowercase_to)
        {
            return Err(RenameOnDeviceError::BiosName);
        }
        // The Pocket finds a save by the game's name without its extension, in
        // any case, so another file beside it with that name shares the save.
        let (old_stem, new_stem) = (file.stem().to_lowercase(), new_name.stem().to_lowercase());
        let beside = os::list_dir(from.parent().expect("a game is in a folder"))?.files;
        let others = || beside.iter().filter(|name| **name != *file);
        if others().any(|name| name.stem().to_lowercase() == new_stem) {
            return Err(RenameOnDeviceError::NameTaken);
        }
        if others().any(|name| name.stem().to_lowercase() == old_stem) {
            return Err(RenameOnDeviceError::SharesSave);
        }
        // Saves already at the new name. If only the case changed, those files
        // are the game's own.
        let save = self.save_path(entry);
        let saves = os::list_dir(save.parent().expect("a save is in a folder"))?.files;
        if old_stem != new_stem
            && saves
                .iter()
                .any(|name| name.stem().to_lowercase() == new_stem)
        {
            return Err(RenameOnDeviceError::LeftoversThere);
        }
        Ok(new_name)
    }

    /// Where `entry`'s save is, even if it has none.
    pub fn save_path(&self, entry: &DeviceEntry) -> PathBuf {
        self.layout.save(&entry.game.platform, &entry.path)
    }

    /// The saves on the device with no game, as the folders are now. They are
    /// looked up each time, so the device needn't be scanned.
    pub fn find_orphan_saves(&self) -> Result<Vec<OrphanDeviceSave>, BasicError> {
        let declared = self.declared_files()?;
        let saves = self.layout.saves();
        let mut orphans = Vec::new();
        for id in os::list_dir(&saves)?.dirs {
            let platform = Platform::from_id(&id);
            let games = os::list_files_recursive(&self.layout.common(&platform))?;
            let games: Vec<PathBuf> = games
                .iter()
                .filter(|path| !declared.contains(&self.layout.game(&platform, path)))
                .map(|path| path.with_extension(""))
                .collect();
            let dir = self.layout.saves_common(&platform);
            for path in os::list_files_recursive(&dir)? {
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
    pub fn read_save(&self, entry: &DeviceEntry) -> Result<Option<Save>, BasicError> {
        Ok(Save::read_if_exists(&self.save_path(entry))?)
    }

    /// Makes the file at `from` into `entry`'s save, keeping `from`'s write
    /// time so that it counts as the same version.
    pub(crate) fn put_save(&mut self, entry: &DeviceEntry, from: &Path) -> Result<(), BasicError> {
        let modified = fs::metadata(from).and_then(|metadata| metadata.modified())?;
        let bytes = fs::read(from)?;
        os::write_atomically(&self.save_path(entry), &bytes, Some(modified))?;
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
    pub fn installed_cores(&self) -> Result<Vec<InstalledCore>, BasicError> {
        let dir = self.layout.cores();
        let ids = os::list_dir(&dir)?.dirs;
        Ok(ids
            .into_iter()
            .map(|id| {
                let json = fs::read(self.layout.core(&id).join("core.json")).ok();
                let (version, platforms) = json
                    .and_then(|bytes| cores::parse_core_json(&bytes))
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
    pub(crate) fn install_core(&self, zip: &mut CoreZip) -> Result<(), BasicError> {
        let staging = self.layout.staging();
        match fs::remove_dir_all(&staging) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.into()),
            _ => {}
        }
        let installed = self.install_core_files(zip, &staging);
        let _ = fs::remove_dir_all(&staging);
        installed
    }

    /// Extracts the files of `zip` to `staging`, then moves each into place.
    ///
    /// The zip holds two kinds of files:
    /// - `Cores/<id>/`: the core itself, such as its bitstream, `core.json`
    ///   and `data.json`.
    /// - `Platforms/`: the platforms it runs, such as `gba.json` with the name
    ///   the menu shows. Other cores of the same platform share these.
    fn install_core_files(&self, zip: &mut CoreZip, staging: &Path) -> Result<(), BasicError> {
        let files: Vec<_> = zip
            .files()
            .into_iter()
            .filter(|(path, _)| match path.strip_prefix("Platforms") {
                // A platform file is written only if the device has none yet,
                // in `Platforms/` or in `Platforms/_archive/`.
                Ok(in_platforms) => {
                    !self.root().join(path).exists()
                        && !self.layout.archived_platform(in_platforms).exists()
                }
                // The core's own files are always written.
                Err(_) => true,
            })
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

    /// Deletes the core's own folder for good. Anything else it uses stays.
    pub(crate) fn remove_core(&self, id: &str) -> Result<(), RemoveCoreError> {
        if !self.installed_cores()?.iter().any(|core| core.id == id) {
            return Err(RemoveCoreError::NotInstalled);
        }
        let path = self.layout.core(id);
        fs::remove_dir_all(&path).map_err(BasicError::from)?;
        Ok(())
    }

    /// The files the cores on the device load, and the core that loads each.
    fn core_files(&self) -> Result<Vec<(String, CoreFile)>, BasicError> {
        let mut files = Vec::new();
        for core in self.installed_cores()? {
            let json = fs::read(self.layout.core(&core.id).join("data.json")).unwrap_or_default();
            let declared = cores::parse_core_files(&core.platforms, &json);
            files.extend(declared.into_iter().map(|file| (core.id.clone(), file)));
        }
        Ok(files)
    }

    /// Where the files the cores on the device load are, even if they
    /// aren't there.
    fn declared_files(&self) -> Result<HashSet<PathBuf>, BasicError> {
        let files = self.core_files()?;
        Ok(files
            .iter()
            .map(|(_, file)| self.layout.game(&file.platform, &file.path))
            .collect())
    }

    /// The files the cores on the device load, by platform and path, as the
    /// folders are now.
    pub fn read_bios_rows(&self) -> Result<Vec<BiosRow>, BasicError> {
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
                    BiosStatus::WrongSize(meta.len())
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
        let actual = fs::metadata(from).map_err(BasicError::from)?.len();
        match row.size_exact {
            Some(expected) if expected != actual => {
                Err(PutBiosError::WrongSize { expected, actual })
            }
            _ => Ok(os::copy_atomically(
                from,
                &self.layout.game(platform, path),
            )?),
        }
    }

    /// None if `Analogue_Pocket.json` can't be read or doesn't have the firmware.
    pub fn read_firmware(&self) -> Option<Firmware> {
        let bytes = fs::read(self.layout.pocket_json()).ok()?;
        let json: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        Firmware::deserialize(json.pointer("/firmware/runtime")?).ok()
    }

    pub fn read_disk_space(&self) -> Result<DiskSpace, BasicError> {
        let root = &self.layout.root;
        Ok(DiskSpace {
            total_bytes: fs4::total_space(root)?,
            available_bytes: fs4::available_space(root)?,
        })
    }

    /// Has the system eject the device, as its file manager would.
    pub fn eject(&self) -> Result<(), EjectDeviceError> {
        os::eject_volume(self.root()).map_err(EjectDeviceError::EjectFailed)
    }
}

/// Helpers shared by the tests of modules that use a device.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use std::path::{Path, PathBuf};

    use fs_err as fs;

    use super::DeviceEntry;

    /// Makes `root` the smallest folder that counts as a device, with no
    /// games and no cores.
    pub fn make_empty_device(root: &Path) {
        fs::create_dir_all(root.join("Assets")).unwrap();
        fs::create_dir_all(root.join("Cores")).unwrap();
        fs::write(root.join("Analogue_Pocket.json"), b"{}").unwrap();
    }

    /// Where each of `entries` is under its platform's `common/`.
    pub fn entry_paths(entries: &[DeviceEntry]) -> Vec<PathBuf> {
        entries.iter().map(|entry| entry.path.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{testing::*, *};
    use crate::cores::testing::{core_json, data_json, release, zip_of};
    use crate::game::Files;
    use crate::os::testing::{put, put_at};

    fn scanned(device: &mut Device) -> Vec<DeviceEntry> {
        device.scan().unwrap();
        device.entries().to_vec()
    }

    fn empty_device() -> (tempfile::TempDir, Device) {
        let dir = tempfile::tempdir().unwrap();
        make_empty_device(dir.path());
        let device = Device::open(dir.path()).unwrap();
        (dir, device)
    }

    fn crc32s(entries: &[DeviceEntry]) -> Vec<u32> {
        entries
            .iter()
            .map(|entry| match entry.game.files {
                Files::Single(rom) => rom.crc32,
            })
            .collect()
    }

    /// Core `id` on the device, for `platform`, with these slots.
    fn core(device: &Device, id: &str, platform: &str, slots: &str) {
        let root = device.root();
        put(
            root,
            &format!("Cores/{id}/core.json"),
            core_json("1", platform).as_bytes(),
        );
        let data = data_json(slots);
        put(root, &format!("Cores/{id}/data.json"), data.as_bytes());
    }

    /// A slot for a GBA BIOS of four bytes.
    const BIOS: &str = r#"{"filename":"gba_bios.bin","required":true,"size_exact":4}"#;

    mod scan {
        use super::*;

        #[test]
        fn rejects_dir_that_is_not_a_pocket() {
            let dir = tempfile::tempdir().unwrap();
            fs::create_dir(dir.path().join("Assets")).unwrap();
            assert!(matches!(
                Device::open(dir.path()),
                Err(OpenDeviceError::NotADevice(_))
            ));
        }

        #[test]
        fn device_scan_finds_games_and_saves() {
            let (dir, mut device) = empty_device();
            let root = dir.path();
            put(root, "Assets/gba/common/Tetris.gba", b"hello");
            put(root, "Assets/gba/common/RPG/Pokemon.gba", b"hi");
            put(root, "Saves/gba/common/RPG/Pokemon.sav", b"save");
            // A second copy of Tetris, at a later path.
            put(root, "Assets/gba/common/Z/Tetris.gba", b"hello");
            // The same contents on another platform are another game.
            put(root, "Assets/gb/common/Tetris.gb", b"hello");
            // None of these are games.
            put(root, "Assets/gba/common/._Tetris.gba", b"x");
            put(root, "Assets/gba/common/.Trash/Lost.gba", b"x");
            put(root, "Assets/gba/agg23.GBA/Lost.gba", b"x");
            put(root, "Lost.gba", b"x");
            let report = device.scan().unwrap();
            let entries = device.entries();
            assert_eq!(
                entry_paths(entries),
                [
                    PathBuf::from("Tetris.gb"),
                    PathBuf::from("RPG/Pokemon.gba"),
                    PathBuf::from("Tetris.gba"),
                ]
            );
            let platforms: Vec<_> = entries.iter().map(|entry| &entry.game.platform).collect();
            assert_eq!(platforms, [&Platform::Gb, &Platform::Gba, &Platform::Gba]);
            assert_eq!(crc32s(entries), [0x3610a686, 0xd8932aac, 0x3610a686]);
            assert_eq!(entries[1].save.as_ref().unwrap().size, 4);
            assert_eq!(entries[2].save, None);
            assert_eq!(
                entry_paths(&report.duplicates),
                [PathBuf::from("Z/Tetris.gba")]
            );
        }

        #[test]
        fn device_cache_rereads_changed_and_replaced_files() {
            let (dir, mut device) = empty_device();
            let path = "Assets/gba/common/Tetris.gba";
            let file = put_at(dir.path(), path, b"hello", 1_000_000);
            scanned(&mut device);
            // Same size and time with other contents, so the cached game is
            // used, even after reopening.
            put_at(dir.path(), path, b"jello", 1_000_000);
            let mut device = Device::open(dir.path()).unwrap();
            assert_eq!(crc32s(&scanned(&mut device)), [0x3610a686]);
            // A new time, so it is read again.
            put_at(dir.path(), path, b"jello", 1_000_002);
            assert_eq!(crc32s(&scanned(&mut device)), [0x4cd0f5e6]);
            // A file gone at the last scan, replaced by one of the same size
            // and time.
            fs::remove_file(&file).unwrap();
            assert_eq!(scanned(&mut device), []);
            put_at(dir.path(), path, b"hello", 1_000_002);
            assert_eq!(crc32s(&scanned(&mut device)), [0x3610a686]);
            // The same after Pocketeer deletes the game.
            let entry = device.entries()[0].clone();
            device.remove_game(&entry).unwrap();
            put_at(dir.path(), path, b"jello", 1_000_002);
            assert_eq!(crc32s(&scanned(&mut device)), [0x4cd0f5e6]);
            // A corrupt cache counts as empty.
            put(dir.path(), ".pocketeer/cache.json", b"{ not json");
            let mut device = Device::open(dir.path()).unwrap();
            assert_eq!(crc32s(&scanned(&mut device)), [0x4cd0f5e6]);
        }
    }

    #[test]
    fn device_deletes_game_save_and_orphan() {
        let (dir, mut device) = empty_device();
        let root = dir.path();
        put(root, "Assets/gba/common/One.gba", b"hello");
        put(root, "Assets/gba/common/Two.gba", b"other");
        let one_save = put(root, "Saves/gba/common/One.sav", b"save");
        let two_save = put(root, "Saves/gba/common/Two.sav", b"two");
        let [one, two]: [DeviceEntry; 2] = scanned(&mut device).try_into().unwrap();
        // Deleting a game leaves its save and the other games.
        device.remove_game(&one).unwrap();
        assert!(!root.join("Assets/gba/common/One.gba").exists());
        assert!(root.join("Assets/gba/common/Two.gba").exists());
        assert!(one_save.exists());
        assert_eq!(entry_paths(device.entries()), [PathBuf::from("Two.gba")]);
        // Its save is now an orphan.
        let orphans = device.find_orphan_saves().unwrap();
        let found: Vec<_> = orphans.iter().map(|orphan| orphan.path.clone()).collect();
        assert_eq!(found, [PathBuf::from("One.sav")]);
        // A game put back at its path since takes the save, so it stays.
        let game = put(root, "Assets/gba/common/One.gba", b"hello");
        assert!(matches!(
            device.remove_orphan_save(&orphans[0]),
            Err(DeleteOrphanSaveError::NoOrphanSave)
        ));
        assert!(one_save.exists());
        fs::remove_file(game).unwrap();
        device.remove_orphan_save(&orphans[0]).unwrap();
        assert!(!one_save.exists());
        // A game's save is deleted once.
        assert!(matches!(
            device.remove_save(&two),
            Ok(DeleteSaveOutcome::Deleted)
        ));
        assert!(!two_save.exists());
        assert_eq!(device.entries()[0].save, None);
        assert!(matches!(
            device.remove_save(&two),
            Ok(DeleteSaveOutcome::NoSave)
        ));
    }

    #[test]
    fn lists_saves_without_game() {
        let (_dir, device) = empty_device();
        let root = device.root();
        put(root, "Assets/gba/common/RPG/Kept.gba", b"hello");
        put(root, "Saves/gba/common/RPG/Kept.sav", b"has a game");
        put(root, "Saves/gba/common/RPG/Gone.sav", b"none");
        put(root, "Saves/gba/common/._Gone.sav", b"hidden");
        put(root, "Saves/gba/common/notes.txt", b"not a save");
        // A file a core loads isn't a game, so its save has none.
        core(&device, "a.GBA", "gba", BIOS);
        put(root, "Assets/gba/common/gba_bios.bin", b"four");
        put(root, "Saves/gba/common/gba_bios.sav", b"save");
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
                (Platform::Gba, PathBuf::from("gba_bios.sav"), 4),
            ]
        );
    }

    mod cores {
        use super::*;

        fn install(device: &Device, bytes: Vec<u8>) -> Result<(), BasicError> {
            let mut zip = CoreZip::parse(bytes, "a.B").unwrap();
            device.install_core(&mut zip)
        }

        fn version(device: &Device) -> Option<String> {
            device.installed_cores().unwrap().remove(0).version
        }

        #[test]
        fn device_installs_reinstalls_and_removes_core() {
            let (_dir, device) = empty_device();
            let root = device.root();
            // A core without a readable `core.json`, as an unfinished install
            // leaves it, has no version.
            put(root, "Cores/a.B/core.rbf_r", b"bits");
            assert_eq!(version(&device), None);
            put(root, "Cores/a.B/core.json", b"{ not json");
            assert_eq!(version(&device), None);
            // Only the core's and the platform's files are written, and not a
            // platform the user has set aside.
            put(root, "Platforms/_archive/gba.json", b"set aside");
            install(&device, release("1.0")).unwrap();
            assert_eq!(
                device.installed_cores().unwrap(),
                [InstalledCore {
                    id: "a.B".into(),
                    version: Some("1.0".into()),
                    platforms: vec![Platform::Gba],
                }]
            );
            assert_eq!(fs::read(root.join("Cores/a.B/core.rbf_r")).unwrap(), b"1.0");
            assert!(root.join("Platforms/_images/gba.bin").exists());
            for left_out in [
                "Platforms/gba.json",
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
            // A new version replaces the core and keeps the user's files.
            let image = put(root, "Platforms/_images/gba.bin", b"my own image");
            let settings = put(root, "Settings/a.B/mine.json", b"mine");
            let other = put(root, "Cores/other.C/core.json", b"{}");
            install(&device, release("2.0")).unwrap();
            assert_eq!(version(&device).unwrap(), "2.0");
            assert_eq!(fs::read(root.join("Cores/a.B/core.rbf_r")).unwrap(), b"2.0");
            assert_eq!(fs::read(&image).unwrap(), b"my own image");
            assert!(!root.join("Platforms/gba.json").exists());
            // Removing it deletes only its own folder.
            device.remove_core("a.B").unwrap();
            assert!(!root.join("Cores/a.B").exists());
            assert!(settings.exists() && other.exists() && image.exists());
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
            assert!(matches!(
                install(&device, release("2.0")),
                Err(BasicError::Io(_))
            ));
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
                ("Cores/a.B/core.json", core_json("2.0", "gba").as_bytes()),
                ("Cores/a.B/core.rbf_r", b"new bits"),
                ("Cores/a.B/both", b"file"),
                ("Cores/a.B/both/inside", b"file"),
            ]);
            assert!(matches!(install(&device, bytes), Err(BasicError::Io(_))));
            assert_eq!(version(&device).unwrap(), "1.0");
            let core_file = device.root().join("Cores/a.B/core.rbf_r");
            assert_eq!(fs::read(core_file).unwrap(), b"1.0");
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
            let mut files = os::list_files_recursive(&root.join("Assets")).unwrap();
            files.extend(os::list_files_recursive(&root.join("Saves")).unwrap());
            files
        }

        #[test]
        fn device_rename_moves_game_with_saves() {
            let (dir, mut device, entry) = with_game();
            let root = dir.path();
            // Every file beside its save with its name moves too, and nothing in
            // another folder.
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
            // The entries are as a fresh scan finds them.
            let listed = device.entries().to_vec();
            assert_eq!(scanned(&mut device), listed);
            assert!(listed.contains(&renamed));
            // Only the case can change, though FAT doesn't tell cases apart.
            let renamed = device.rename_game(&renamed, "NEW").unwrap();
            assert_eq!(renamed.path, Path::new("RPG/NEW.gba"));
            let saves = os::list_dir(&root.join("Saves/gba/common/RPG"))
                .unwrap()
                .files;
            assert_eq!(saves, ["NEW.rtc", "NEW.sav"]);
        }

        #[test]
        fn moves_nothing_when_refused() {
            use RenameOnDeviceError::*;
            for (new, extra, refusal) in [
                ("a:b", None, BadName),
                ("", None, BadName),
                (".gba", None, BadName),
                ("Old", None, SameName),
                (
                    "Neighbor",
                    Some("Assets/gba/common/RPG/Neighbor.gba"),
                    NameTaken,
                ),
                (
                    "neighbor",
                    Some("Assets/gba/common/RPG/Neighbor.pce"),
                    NameTaken,
                ),
                ("New", Some("Saves/gba/common/RPG/New.rtc"), LeftoversThere),
                ("New", Some("Assets/gba/common/RPG/Old.sgx"), SharesSave),
            ] {
                let (dir, mut device, entry) = with_game();
                let root = dir.path();
                if let Some(extra) = extra {
                    put(root, extra, b"in the way");
                }
                let before = all_files(root);
                let refused = device.rename_game(&entry, new).unwrap_err();
                assert_eq!(
                    std::mem::discriminant(&refused),
                    std::mem::discriminant(&refusal),
                    "{new}: {refused:?}"
                );
                assert_eq!(all_files(root), before, "{new}");
            }
        }

        #[test]
        fn rejects_name_of_file_core_loads() {
            let (dir, mut device, _) = with_game();
            let root = dir.path();
            let data = data_json(r#"{"filename":"Bios.gba"}"#);
            put(
                root,
                "Cores/a.B/core.json",
                core_json("1", "gba").as_bytes(),
            );
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
                Err(RenameOnDeviceError::Other(_))
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

        fn gba_status(device: &Device) -> BiosStatus {
            let rows = device.read_bios_rows().unwrap();
            let gba = rows.into_iter().find(|row| row.platform == Platform::Gba);
            gba.unwrap().status
        }

        #[test]
        fn device_lists_and_puts_bios_files() {
            let (_dir, device) = empty_device();
            let root = device.root();
            core(&device, "a.GBA", "gba", BIOS);
            core(
                &device,
                "b.GBA",
                "gba",
                r#"{"filename":"gba_bios.bin","size_exact":4}"#,
            );
            core(&device, "a.GB", "gb", r#"{"filename":"gb_bios.bin"}"#);
            core(&device, "a.GG", "gg", r#"{"name":"Cartridge"}"#);
            // Cores that can't be read load nothing.
            put(root, "Cores/broken.A/core.json", b"{ not json");
            core(&device, "broken.B", "gba", BIOS);
            put(root, "Cores/broken.B/data.json", b"{ not json");
            // One row for each file, whichever cores load it.
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
            let path = Path::new("gba_bios.bin");
            let to = root.join("Assets/gba/common/gba_bios.bin");
            let elsewhere = tempfile::tempdir().unwrap();
            // A file of another size than the core takes isn't put there.
            let five = put(elsewhere.path(), "five.bin", b"five!");
            assert!(matches!(
                device.put_bios(&Platform::Gba, path, &five),
                Err(PutBiosError::WrongSize {
                    expected: 4,
                    actual: 5
                })
            ));
            assert!(!root.join("Assets/gba").exists());
            // A file of the right size is copied, and the original stays.
            let four = put(elsewhere.path(), "my copy.bin", b"four");
            device.put_bios(&Platform::Gba, path, &four).unwrap();
            assert_eq!(fs::read(&to).unwrap(), b"four");
            assert_eq!(fs::read(&four).unwrap(), b"four");
            assert_eq!(gba_status(&device), BiosStatus::Present);
            // A file on the device is never replaced.
            let other = put(elsewhere.path(), "other.bin", b"four");
            fs::write(&to, b"mine").unwrap();
            assert!(matches!(
                device.put_bios(&Platform::Gba, path, &other),
                Err(PutBiosError::AlreadyThere)
            ));
            assert_eq!(fs::read(&to).unwrap(), b"mine");
            // One the core won't load is reported.
            fs::write(&to, b"five!").unwrap();
            assert_eq!(gba_status(&device), BiosStatus::WrongSize(5));
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
        fn rejects_file_no_core_declares() {
            let (_dir, device) = empty_device();
            core(&device, "a.GBA", "gba", BIOS);
            let elsewhere = tempfile::tempdir().unwrap();
            let from = put(elsewhere.path(), "my copy.bin", b"four");
            for (platform, path) in [
                (Platform::Gba, "other.bin"),
                (Platform::Gba, "common/gba_bios.bin"),
                (Platform::Gba, "../../Analogue_Pocket.json"),
                (Platform::Gb, "gba_bios.bin"),
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
                entry_paths(&scanned(&mut device)),
                [
                    PathBuf::from("RPG/gba_bios.bin"),
                    PathBuf::from("Sonic.bin"),
                ]
            );
        }
    }
}
