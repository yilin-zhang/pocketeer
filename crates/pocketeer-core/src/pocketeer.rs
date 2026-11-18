//! Everything Pocketeer does, in one place for every front end.

use std::path::{Path, PathBuf};

use fs_err as fs;

use crate::{
    cores::{
        CoreIndex, CoreList, CoreRow, InstallCoreError, InstallCoreOutcome, RemoteCore,
        RemoteCoreSource,
    },
    cover::{Cover, CoverIndex, RemoteCoverSource},
    dat::{DatMirror, RemoteDatSource, SyncReport},
    device::{
        DeleteSaveOutcome, Device, DeviceEntry, DeviceKey, EjectDeviceError, OpenDeviceError,
        OrphanDeviceSave, ScanReport,
    },
    error::BasicError,
    game::Game,
    library::{
        AddGameError, AddGameOutcome, Library, LibraryKey, LocalEntry, OpenLibraryError,
        TrashSaveAllOutcome,
    },
    os,
    platform::Platform,
    sources::{libretro::Libretro, openfpga::OpenFpga},
    transfer::{self, PullSaveOutcome, PushGameOutcome, PushSaveError, PushSaveOutcome},
};

// ============================================================================
// Commands
// ============================================================================

/// What a front end asks Pocketeer to do, to one game, save, core or file.
/// Front ends run a batch as one command after another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    PullGame(DeviceKey),
    PushGame(LibraryKey),
    /// The file, and the platform it is for.
    ImportGame(PathBuf, Platform),
    /// The game, and its new name. The extension stays.
    RenameInLibrary(LibraryKey, String),
    /// The game, and its new name. The extension and folder stay.
    RenameOnDevice(DeviceKey, String),
    TrashGame(LibraryKey),
    DeleteGame(DeviceKey),
    PullSave(DeviceKey),
    /// The game on the device, and the name of the library's version to put
    /// there.
    PushSave(DeviceKey, String),
    /// The game, and the save file to keep as a new version.
    ImportSave(LibraryKey, PathBuf),
    /// Every version of the game's save.
    TrashSaves(LibraryKey),
    /// The game, and the name of the version.
    TrashSave(LibraryKey, String),
    DeleteSave(DeviceKey),
    /// The platform, and the stem its saves are kept under.
    TrashOrphanSave(Platform, String),
    /// The platform, and the save's path under `Saves/<platform>/common/`.
    DeleteOrphanSave(Platform, PathBuf),
    FetchCover(LibraryKey),
    /// The game, and the image file.
    ImportCover(LibraryKey, PathBuf),
    InstallCore(String),
    RemoveCore(String),
    /// Where a core loads the file from, and the file to copy there.
    PutBios(DeviceKey, PathBuf),
}

/// How one command went, in words for a person. Finding nothing to do is
/// fine, so it is `ok`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// What it was done to, such as `gba/Tetris.gba`.
    pub subject: String,
    pub ok: bool,
    pub message: String,
}

impl Report {
    /// `message` is `Ok` when the command did its job.
    fn new(subject: impl ToString, message: Result<String, String>) -> Self {
        let ok = message.is_ok();
        let message = message.unwrap_or_else(|error| error);
        Self {
            subject: subject.to_string(),
            ok,
            message,
        }
    }

    /// `<subject>: <message>`.
    pub fn to_line(&self) -> String {
        format!("{}: {}", self.subject, self.message)
    }
}

// ============================================================================
// Game rows
// ============================================================================

/// One game, in the library, on the device, or both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GameRow {
    InLibrary(LocalEntry),
    OnDevice(DeviceEntry),
    Both {
        local_entry: LocalEntry,
        device_entry: DeviceEntry,
    },
}

impl GameRow {
    pub fn local_entry(&self) -> Option<&LocalEntry> {
        match self {
            GameRow::InLibrary(local_entry) | GameRow::Both { local_entry, .. } => {
                Some(local_entry)
            }
            GameRow::OnDevice(_) => None,
        }
    }

    pub fn device_entry(&self) -> Option<&DeviceEntry> {
        match self {
            GameRow::OnDevice(device_entry) | GameRow::Both { device_entry, .. } => {
                Some(device_entry)
            }
            GameRow::InLibrary(_) => None,
        }
    }

    pub fn game(&self) -> &Game {
        match self {
            GameRow::InLibrary(local_entry) | GameRow::Both { local_entry, .. } => {
                &local_entry.game
            }
            GameRow::OnDevice(device_entry) => &device_entry.game,
        }
    }

    /// How many versions of its save the library has.
    pub fn saves_kept(&self) -> usize {
        let saves = self
            .local_entry()
            .and_then(|local_entry| local_entry.save.as_ref());
        saves.map_or(0, |saves| saves.iter().count())
    }

    /// Whether the device has a save the library doesn't.
    pub fn has_new_save_on_device(&self) -> bool {
        let saves = self
            .local_entry()
            .and_then(|local_entry| local_entry.save.as_ref());
        let device_save = self
            .device_entry()
            .and_then(|device_entry| device_entry.save.as_ref());
        device_save.is_some_and(|save| !saves.is_some_and(|saves| saves.contains(save)))
    }
}

// ============================================================================
// Pocketeer
// ============================================================================

/// The library, the DATs, the known covers and cores, and the open device if
/// there is one. `D`, `C` and `I` are the sources of DATs, covers and cores.
/// Only tests use anything other than [`Libretro`] and [`OpenFpga`].
pub struct Pocketeer<D = Libretro, C = Libretro, I = OpenFpga> {
    library: Library,
    dats: DatMirror<D>,
    covers: CoverIndex<C>,
    cores: CoreIndex<I>,
    device: Option<Device>,
    /// Whether commands since the last [`Pocketeer::settle`] changed which
    /// files on the device are games.
    device_changed: bool,
}

impl Pocketeer {
    /// Opens the library at `library` with the DATs it keeps, without going
    /// online. No device is open yet.
    pub fn open(library: &Path) -> Result<Self, OpenLibraryError> {
        Pocketeer::open_with(library, Libretro, Libretro, OpenFpga)
    }

    /// The `Pocketeer Library` folder in the home folder, created if missing.
    /// This is the only library Pocketeer creates.
    pub fn default_library_path() -> Result<PathBuf, OpenLibraryError> {
        let home = std::env::home_dir().ok_or(OpenLibraryError::NoHome)?;
        let library = home.join("Pocketeer Library");
        fs::create_dir_all(&library).map_err(BasicError::from)?;
        Ok(library)
    }
}

impl<D: RemoteDatSource, C: RemoteCoverSource, I: RemoteCoreSource> Pocketeer<D, C, I> {
    // ---- Opening ----------------------------------------------------------------

    /// Like [`Pocketeer::open`], with DATs from `dats`, covers from `covers`
    /// and cores from `cores`.
    fn open_with(library: &Path, dats: D, covers: C, cores: I) -> Result<Self, OpenLibraryError> {
        let library = Library::open(library)?;
        let dats = DatMirror::open(&library.layout().dats(), dats)?;
        let covers = CoverIndex::open(&library.layout().cover_index(), covers);
        let cores = CoreIndex::open(&library.layout().core_index(), cores);
        Ok(Self {
            library,
            dats,
            covers,
            cores,
            device: None,
            device_changed: false,
        })
    }

    // ---- Accessors --------------------------------------------------------------

    pub fn library(&self) -> &Library {
        &self.library
    }

    pub fn device(&self) -> Option<&Device> {
        self.device.as_ref()
    }

    // ---- Device connection ------------------------------------------------------

    /// Opens the device at `root`, replacing any open one, without scanning it.
    pub fn open_device(&mut self, root: &Path) -> Result<(), OpenDeviceError> {
        self.device = Some(Device::open(root)?);
        Ok(())
    }

    /// Opens the mounted device, if there is one, replacing any open one, and
    /// returns whether there was. It isn't scanned yet.
    ///
    /// With more than one mounted none is opened, because Pocketeer shouldn't
    /// guess which one is meant.
    pub fn open_mounted_device(&mut self) -> Result<bool, OpenDeviceError> {
        match &Device::find_mounted()[..] {
            [] => Ok(false),
            [root] => self.open_device(root).map(|()| true),
            found => Err(OpenDeviceError::ManyDevices(found.to_vec())),
        }
    }

    pub fn close_device(&mut self) {
        self.device = None;
    }

    pub fn scan_device(&mut self) -> Result<ScanReport, BasicError> {
        self.device.as_mut().ok_or(BasicError::NoDevice)?.scan()
    }

    /// Ejects the open device and closes it, returning its root. If the system
    /// won't eject it, it stays open.
    pub fn eject_device(&mut self) -> Result<PathBuf, EjectDeviceError> {
        let device = self.device.as_ref().ok_or(BasicError::NoDevice)?;
        device.eject()?;
        let root = device.root().to_path_buf();
        self.close_device();
        Ok(root)
    }

    /// Reads the library's folder again, and the open device's, for changes
    /// made outside Pocketeer.
    pub fn reload(&mut self) -> Result<ScanReport, BasicError> {
        self.library.reload()?;
        match self.device {
            Some(_) => self.scan_device(),
            None => Ok(ScanReport::default()),
        }
    }

    /// Scans the device again if commands since the last call changed which
    /// files on it are games: deleting the first copy of a game makes the next
    /// one count, and the files a core loads aren't games. A device that
    /// hasn't been scanned stays that way.
    ///
    /// Front ends call this once a batch of commands is done, however it
    /// ended.
    pub fn settle(&mut self) -> Result<(), BasicError> {
        if !std::mem::take(&mut self.device_changed) {
            return Ok(());
        }
        match &mut self.device {
            Some(device) if device.has_been_scanned() => device.scan().map(|_| ()),
            _ => Ok(()),
        }
    }

    // ---- Listing games ----------------------------------------------------------

    /// Every game, starting with the library's in order, each with its device
    /// copy if it has one, followed by those only on the device.
    pub fn game_rows(&self) -> Vec<GameRow> {
        let device = self.device.as_ref();
        let mut rows: Vec<GameRow> = self
            .library
            .entries()
            .iter()
            .map(|local_entry| {
                match device.and_then(|device| device.entry_for(&local_entry.game)) {
                    Some(device_entry) => GameRow::Both {
                        local_entry: local_entry.clone(),
                        device_entry: device_entry.clone(),
                    },
                    None => GameRow::InLibrary(local_entry.clone()),
                }
            })
            .collect();
        rows.extend(
            self.entries_only_on_device()
                .cloned()
                .map(GameRow::OnDevice),
        );
        rows
    }

    fn entries_only_on_device(&self) -> impl Iterator<Item = &DeviceEntry> {
        self.entries_on_device()
            .iter()
            .filter(|device_entry| self.library.entry_for(&device_entry.game).is_none())
    }

    fn entries_on_device(&self) -> &[DeviceEntry] {
        self.device
            .as_ref()
            .map_or(&[][..], |device| device.entries())
    }

    pub fn games_without_covers(&self) -> Vec<LibraryKey> {
        let entries = self.library.entries().iter();
        let without = entries.filter(|entry| entry.cover_path.is_none());
        without.map(LocalEntry::key).collect()
    }

    pub fn games_only_on_device(&self) -> Vec<DeviceEntry> {
        self.entries_only_on_device().cloned().collect()
    }

    pub fn games_on_device_in_library(&self) -> Vec<DeviceKey> {
        let entries = self.entries_on_device().iter();
        let in_library = entries.filter(|entry| self.library.entry_for(&entry.game).is_some());
        in_library.map(DeviceEntry::key).collect()
    }

    // ---- DATs -------------------------------------------------------------------

    pub fn sync_dats(&mut self) -> Result<SyncReport, BasicError> {
        self.dats.sync()
    }

    /// The games among `games` whose name differs from their DAT name, each
    /// paired with that name. Games the DATs don't know, or the library
    /// doesn't have, are left out.
    pub fn renames_to_dat_names(&self, games: &[LibraryKey]) -> Vec<(LibraryKey, String)> {
        games
            .iter()
            .filter_map(|key| {
                let local_entry = self.library.entry_named(&key.platform, &key.name)?;
                let dat = self.dats.lookup(&local_entry.game)?;
                (local_entry.stem() != dat.name).then(|| (key.clone(), dat.name.clone()))
            })
            .collect()
    }

    // ---- Orphan saves, cores and BIOS files ------------------------------------

    /// The saves on the device with no game, or none if no device is open.
    pub fn find_orphan_saves_on_device(&self) -> Result<Vec<OrphanDeviceSave>, BasicError> {
        self.device
            .as_ref()
            .map_or(Ok(Vec::new()), Device::find_orphan_saves)
    }

    /// Every core, starting with those that can be installed, then the others
    /// on the device. If the index can't be fetched, the list says why and
    /// holds the cores on the device alone.
    pub fn load_core_list(&mut self) -> Result<CoreList, BasicError> {
        let installed = match &self.device {
            Some(device) => device.installed_cores()?,
            None => Vec::new(),
        };
        let (latest, index_failure) = match self.cores.supported_cores() {
            Ok(latest) => (latest, None),
            Err(error) => (Vec::new(), Some(error)),
        };
        Ok(CoreList {
            rows: CoreRow::pair(installed, latest),
            index_failure,
        })
    }

    /// The cores on the device with another version in the index.
    pub fn find_cores_with_updates(&mut self) -> Result<Vec<String>, BasicError> {
        let core_list = self.load_core_list()?;
        if let Some(failure) = core_list.index_failure {
            return Err(failure);
        }
        let with_updates = core_list.rows.iter().filter(|row| row.has_update());
        Ok(with_updates.map(|row| row.id().to_string()).collect())
    }

    pub fn read_bios_rows(&self) -> Result<Vec<crate::cores::BiosRow>, BasicError> {
        self.device
            .as_ref()
            .ok_or(BasicError::NoDevice)?
            .read_bios_rows()
    }

    // ---- Running commands -------------------------------------------------------

    /// Looks up what `command` names as things are now, since they may have
    /// changed or gone since the front end saw them, does it, and says how it
    /// went.
    pub fn run(&mut self, command: &Command) -> Report {
        match command {
            Command::PullGame(key) => Report::new(key, self.pull_game(key)),
            Command::PushGame(key) => Report::new(key, self.push_game(key)),
            Command::ImportGame(from, platform) => {
                let name = from.file_name().unwrap_or(from.as_os_str());
                Report::new(name.to_string_lossy(), self.import_game(from, platform))
            }
            Command::RenameInLibrary(key, new) => {
                Report::new(key, self.rename_in_library(key, new))
            }
            Command::RenameOnDevice(key, new) => {
                self.device_changed = true;
                Report::new(key, self.rename_on_device(key, new))
            }
            Command::TrashGame(key) => Report::new(key, self.trash_game(key)),
            Command::DeleteGame(key) => {
                self.device_changed = true;
                Report::new(key, self.delete_game(key))
            }
            Command::PullSave(key) => Report::new(key, self.pull_save(key)),
            Command::PushSave(key, version) => Report::new(key, self.push_save(key, version)),
            Command::ImportSave(key, from) => Report::new(key, self.import_save(key, from)),
            Command::TrashSaves(key) => Report::new(key, self.trash_saves(key)),
            Command::TrashSave(key, version) => Report::new(key, self.trash_save(key, version)),
            Command::DeleteSave(key) => Report::new(key, self.delete_save(key)),
            Command::TrashOrphanSave(platform, stem) => Report::new(
                format!("{}/{stem}", platform.id()),
                self.trash_orphan_save(platform, stem),
            ),
            Command::DeleteOrphanSave(platform, path) => Report::new(
                format!("{}/{}", platform.id(), path.display()),
                self.delete_orphan_save(platform, path),
            ),
            Command::FetchCover(key) => Report::new(key, self.fetch_cover(key)),
            Command::ImportCover(key, from) => Report::new(key, self.import_cover(key, from)),
            Command::InstallCore(id) => {
                self.device_changed = true;
                Report::new(id, self.install_core(id))
            }
            Command::RemoveCore(id) => {
                self.device_changed = true;
                Report::new(id, self.remove_core(id))
            }
            Command::PutBios(key, from) => Report::new(key, self.put_bios(key, from)),
        }
    }

    // Each command below returns its message: `Ok` when it did its job, `Err`
    // when it didn't. A refusal says why; anything else starts with `error: `.

    // ---- Games ------------------------------------------------------------------

    fn pull_game(&mut self, key: &DeviceKey) -> Result<String, String> {
        let pulled = self
            .device_entry(key)
            .map_err(AddGameError::from)
            .and_then(|entry| {
                let device = self.device.as_ref().ok_or(BasicError::NoDevice)?;
                transfer::pull_game(&mut self.library, device, &entry)
            });
        added_message(pulled, "pulled")
    }

    fn import_game(&mut self, from: &Path, platform: &Platform) -> Result<String, String> {
        added_message(self.library.import_game(from, platform.clone()), "imported")
    }

    fn push_game(&mut self, key: &LibraryKey) -> Result<String, String> {
        let local_entry = self.local_entry(key).map_err(error_message)?;
        let device = self.device.as_mut().ok_or(BasicError::NoDevice);
        let device = device.map_err(error_message)?;
        match transfer::push_game(&self.library, device, &local_entry) {
            Ok(PushGameOutcome::Pushed(entry)) => Ok(format!("pushed to {entry}")),
            Ok(PushGameOutcome::AlreadyThere(entry)) => {
                Ok(format!("on the device already, at {entry}"))
            }
            Err(transfer::PushGameError::Other(error)) => Err(error_message(error)),
            Err(error) => Err(error.to_string()),
        }
    }

    fn rename_in_library(&mut self, key: &LibraryKey, new: &str) -> Result<String, String> {
        let local_entry = self.local_entry(key).map_err(error_message)?;
        let renamed = self
            .library
            .rename_game(&local_entry.game.platform, &local_entry.name, new);
        match renamed {
            Ok(entry) => Ok(format!("renamed to {entry}")),
            Err(crate::library::RenameInLibraryError::Other(error)) => Err(error_message(error)),
            Err(error) => Err(format!("not renamed: {error}")),
        }
    }

    fn rename_on_device(&mut self, key: &DeviceKey, new: &str) -> Result<String, String> {
        let device_entry = self.device_entry(key).map_err(error_message)?;
        let device = self
            .device
            .as_mut()
            .expect("the entry is on the open device");
        match device.rename_game(&device_entry, new) {
            Ok(entry) => Ok(format!("renamed to {entry}")),
            Err(crate::device::RenameOnDeviceError::Other(error)) => Err(error_message(error)),
            Err(error) => Err(format!("not renamed: {error}")),
        }
    }

    fn trash_game(&mut self, key: &LibraryKey) -> Result<String, String> {
        let local_entry = self.local_entry(key).map_err(error_message)?;
        self.library
            .trash_game(&local_entry)
            .map_err(error_message)?;
        Ok("put in the trash".into())
    }

    /// Deletes the game's file from the device for good. Its save stays there.
    fn delete_game(&mut self, key: &DeviceKey) -> Result<String, String> {
        let device_entry = self.device_entry(key).map_err(error_message)?;
        let device = self
            .device
            .as_mut()
            .expect("the entry is on the open device");
        device.remove_game(&device_entry).map_err(error_message)?;
        Ok("deleted from the device".into())
    }

    // ---- Saves ------------------------------------------------------------------

    fn pull_save(&mut self, key: &DeviceKey) -> Result<String, String> {
        let device_entry = self.device_entry(key).map_err(error_message)?;
        let device = self
            .device
            .as_ref()
            .expect("the entry is on the open device");
        match transfer::pull_save(&mut self.library, device, &device_entry) {
            Ok(PullSaveOutcome::Pulled) => Ok("save kept".into()),
            Ok(PullSaveOutcome::AlreadyHad) => Ok("the library has this save already".into()),
            Ok(PullSaveOutcome::NoSave) => Ok("no save on the device".into()),
            Err(error @ BasicError::NotInLibrary) => Err(format!("{error}; pull the game first")),
            Err(error) => Err(error_message(error)),
        }
    }

    fn push_save(&mut self, key: &DeviceKey, version: &str) -> Result<String, String> {
        let pushed = self
            .device_entry(key)
            .map_err(PushSaveError::from)
            .and_then(|entry| {
                let local_entry = self.library.entry_for(&entry.game);
                let local_entry = local_entry.ok_or(BasicError::NotInLibrary)?;
                let saves = local_entry.save.as_ref();
                let version = saves.and_then(|saves| saves.version_named(version));
                let version = version.ok_or(PushSaveError::NoSuchVersion)?.clone();
                let device = self
                    .device
                    .as_mut()
                    .expect("the entry is on the open device");
                transfer::push_save(&self.library, device, &entry, &version)
            });
        match pushed {
            Ok(PushSaveOutcome::Pushed(save)) => Ok(format!("save {} pushed", save.name)),
            Ok(PushSaveOutcome::AlreadyThere) => Ok("the device has this save already".into()),
            Err(error @ PushSaveError::NotBackedUp) => Err(format!("{error}; back it up first")),
            Err(PushSaveError::Other(error)) => Err(error_message(error)),
            Err(error) => Err(error.to_string()),
        }
    }

    /// Keeps the file at `from` as a new version of the game's save, dated when
    /// the file was written. The file stays where it is.
    fn import_save(&mut self, key: &LibraryKey, from: &Path) -> Result<String, String> {
        let local_entry = self.local_entry(key).map_err(error_message)?;
        let added = self
            .library
            .add_save(&local_entry.game.platform, &local_entry.name, from);
        match added.map_err(error_message)? {
            Some(save) => Ok(format!("save {} imported", save.name)),
            None => Ok("the library has this save already".into()),
        }
    }

    fn trash_saves(&mut self, key: &LibraryKey) -> Result<String, String> {
        let local_entry = self.local_entry(key).map_err(error_message)?;
        match self
            .library
            .trash_all_saves(&local_entry)
            .map_err(error_message)?
        {
            TrashSaveAllOutcome::Trashed => Ok("save put in the trash".into()),
            TrashSaveAllOutcome::NoSave => Ok("no save in the library".into()),
        }
    }

    fn trash_save(&mut self, key: &LibraryKey, version: &str) -> Result<String, String> {
        let local_entry = self.local_entry(key).map_err(error_message)?;
        match self.library.trash_save_version(&local_entry, version) {
            Ok(()) => Ok("save put in the trash".into()),
            Err(crate::library::TrashSaveError::Other(error)) => Err(error_message(error)),
            Err(error) => Err(error.to_string()),
        }
    }

    /// Deletes the game's save from the device for good, even if the library
    /// doesn't have it.
    fn delete_save(&mut self, key: &DeviceKey) -> Result<String, String> {
        let device_entry = self.device_entry(key).map_err(error_message)?;
        let device = self
            .device
            .as_mut()
            .expect("the entry is on the open device");
        match device.remove_save(&device_entry).map_err(error_message)? {
            DeleteSaveOutcome::Deleted => Ok("save deleted from the device".into()),
            DeleteSaveOutcome::NoSave => Ok("no save on the device".into()),
        }
    }

    fn trash_orphan_save(&mut self, platform: &Platform, stem: &str) -> Result<String, String> {
        match self.library.trash_orphan_save(platform, stem) {
            Ok(()) => Ok("save put in the trash".into()),
            Err(crate::library::TrashOrphanSaveError::Other(error)) => Err(error_message(error)),
            Err(error) => Err(error.to_string()),
        }
    }

    fn delete_orphan_save(&mut self, platform: &Platform, path: &Path) -> Result<String, String> {
        let device = self.device.as_ref().ok_or(BasicError::NoDevice);
        match device
            .map_err(error_message)?
            .remove_orphan_save(platform, path)
        {
            Ok(()) => Ok("save deleted from the device".into()),
            Err(crate::device::DeleteOrphanSaveError::Other(error)) => Err(error_message(error)),
            Err(error) => Err(error.to_string()),
        }
    }

    // ---- Covers -----------------------------------------------------------------

    /// Downloads a cover for the game if it has none. It looks under the DAT
    /// name if known and otherwise under the file name without its extension.
    fn fetch_cover(&mut self, key: &LibraryKey) -> Result<String, String> {
        let local_entry = self.local_entry(key).map_err(error_message)?;
        if local_entry.cover_path.is_some() {
            return Ok("it has a cover already".into());
        }
        let name = match self.dats.lookup(&local_entry.game) {
            Some(dat) => dat.name.clone(),
            None => local_entry.stem().to_string(),
        };
        let platform = &local_entry.game.platform;
        let Some(cover) = self.covers.fetch(platform, &name).map_err(error_message)? else {
            return Ok("no cover found for it".into());
        };
        let set = self.library.set_cover(platform, &local_entry.name, &cover);
        set.map_err(error_message)?;
        Ok("cover downloaded".into())
    }

    /// Makes the image at `from` the game's cover. Any cover it had goes to the
    /// system's trash.
    fn import_cover(&mut self, key: &LibraryKey, from: &Path) -> Result<String, String> {
        let local_entry = self.local_entry(key).map_err(error_message)?;
        let bytes = fs::read(from).map_err(|error| error_message(error.into()))?;
        let Some(cover) = Cover::from_bytes(bytes) else {
            return Err("not a PNG, JPEG or WebP file".into());
        };
        let set = self
            .library
            .set_cover(&local_entry.game.platform, &local_entry.name, &cover);
        set.map_err(error_message)?;
        Ok("cover imported".into())
    }

    // ---- Cores and BIOS files ---------------------------------------------------

    /// Installs the newest release of the core, unless it is already there.
    fn install_core(&mut self, id: &str) -> Result<String, String> {
        match self.install_core_release(id) {
            Ok(InstallCoreOutcome::Installed(version)) => Ok(format!("{version} installed")),
            Ok(InstallCoreOutcome::AlreadyCurrent) => {
                Ok("the device has the newest version".into())
            }
            Err(InstallCoreError::Other(error)) => Err(error_message(error)),
            Err(error) => Err(error.to_string()),
        }
    }

    fn install_core_release(&mut self, id: &str) -> Result<InstallCoreOutcome, InstallCoreError> {
        if !RemoteCore::is_supported(id) {
            return Err(InstallCoreError::NotSupported);
        }
        let device = self.device.as_ref().ok_or(BasicError::NoDevice)?;
        let installed = device.installed_cores()?;
        let latest = self
            .cores
            .supported_cores()?
            .into_iter()
            .find(|core| core.id == id);
        let latest = latest.ok_or(InstallCoreError::NotSupported)?;
        let installed_core = installed.iter().find(|core| core.id == id);
        if installed_core.is_some_and(|core| core.is_at_version(&latest.version)) {
            return Ok(InstallCoreOutcome::AlreadyCurrent);
        }
        let mut zip = self.cores.download(&latest)?;
        device.install_core(&mut zip)?;
        Ok(InstallCoreOutcome::Installed(zip.version().to_string()))
    }

    /// Deletes the core from the device for good.
    fn remove_core(&mut self, id: &str) -> Result<String, String> {
        let device = self.device.as_ref().ok_or(BasicError::NoDevice);
        match device.map_err(error_message)?.remove_core(id) {
            Ok(()) => Ok("removed from the device".into()),
            Err(crate::cores::RemoveCoreError::Other(error)) => Err(error_message(error)),
            Err(error) => Err(error.to_string()),
        }
    }

    /// Copies the file at `from` to where a core loads it. The file stays
    /// where it is.
    fn put_bios(&mut self, key: &DeviceKey, from: &Path) -> Result<String, String> {
        let device = self.device.as_ref().ok_or(BasicError::NoDevice);
        match device
            .map_err(error_message)?
            .put_bios(&key.platform, &key.path, from)
        {
            Ok(()) => Ok("put on the device".into()),
            Err(crate::cores::PutBiosError::Other(error)) => Err(error_message(error)),
            Err(error) => Err(error.to_string()),
        }
    }

    // ---- Looking up -------------------------------------------------------------

    /// The game `key` names, as the library has it now.
    fn local_entry(&self, key: &LibraryKey) -> Result<LocalEntry, BasicError> {
        let current = self.library.entry_named(&key.platform, &key.name);
        current.cloned().ok_or(BasicError::NotInLibrary)
    }

    /// The game `key` names, as the open device has it now.
    fn device_entry(&self, key: &DeviceKey) -> Result<DeviceEntry, BasicError> {
        let device = self.device.as_ref().ok_or(BasicError::NoDevice)?;
        let current = device.entry_at(&key.platform, &key.path);
        current.cloned().ok_or(BasicError::NotOnDevice)
    }
}

/// The message for a failure that isn't the command's own refusal.
fn error_message(error: BasicError) -> String {
    format!("error: {error}")
}

/// The message for adding a game to the library by `verb`ing it.
fn added_message(
    added: Result<AddGameOutcome, AddGameError>,
    verb: &str,
) -> Result<String, String> {
    match added {
        Ok(AddGameOutcome::Added(entry)) => Ok(format!("{verb} as {entry}")),
        Ok(AddGameOutcome::AlreadyHad(entry)) => {
            Ok(format!("the library has it already, as {entry}"))
        }
        Err(AddGameError::Other(error)) => Err(error_message(error)),
        Err(error) => Err(format!("not {verb}: {error}")),
    }
}

// ============================================================================
// Helpers
// ============================================================================

/// `paths` with each folder replaced by every file under it whose extension
/// indicates a platform, each file with the platform its extension indicates.
/// Files given directly stay, with none if the extension indicates none.
pub fn expand_import_paths(
    paths: &[PathBuf],
) -> Result<Vec<(PathBuf, Option<Platform>)>, BasicError> {
    let platform_of = |file: &Path| {
        let extension = file.extension().and_then(|extension| extension.to_str());
        extension.and_then(Platform::from_extension)
    };
    let mut files = Vec::new();
    for path in paths {
        if path.is_dir() {
            for file in os::list_files_recursive(path)? {
                if let Some(platform) = platform_of(&file) {
                    files.push((path.join(file), Some(platform)));
                }
            }
        } else {
            files.push((path.clone(), platform_of(path)));
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, fs, rc::Rc};

    use super::*;
    use crate::cores::{
        RemoteCore,
        testing::{core_json, data_json, remote_core, zip_of},
    };
    use crate::cover::{Cover, testing::PNG};
    use crate::dat::testing::GBA_DAT;
    use crate::device::testing::{entry_paths, make_empty_device};
    use crate::os::testing::{put, put_at};

    /// A cover source that has covers for two GBA games.
    struct Covers;

    impl RemoteCoverSource for Covers {
        fn systems(&self, platform: &Platform) -> &'static [&'static str] {
            match platform {
                Platform::Gba => &["gba"],
                _ => &[],
            }
        }

        fn version(&self, _: &str) -> Result<String, BasicError> {
            Ok("1".into())
        }

        fn names(&self, _: &str, _: &str) -> Result<Vec<String>, BasicError> {
            Ok(vec!["Tetris (World)".into(), "My Hack".into()])
        }

        fn cover_name(&self, game_name: &str) -> String {
            game_name.into()
        }

        fn fetch(&self, _: &str, _: &str) -> Result<Cover, BasicError> {
            Ok(Cover::from_bytes(PNG.to_vec()).unwrap())
        }
    }

    /// A source of cores with `Spiritualized.GB` at 2.0. It counts its
    /// downloads in a counter shared with the test.
    #[derive(Clone, Default)]
    struct CoreSource {
        downloaded: Rc<Cell<u32>>,
    }

    impl RemoteCoreSource for CoreSource {
        fn index(&self) -> Result<Vec<RemoteCore>, BasicError> {
            Ok(vec![remote_core("Spiritualized.GB", "2.0")])
        }

        fn download(&self, core: &RemoteCore) -> Result<Vec<u8>, BasicError> {
            self.downloaded.set(self.downloaded.get() + 1);
            match core.download_url.as_str() {
                "core.zip" => Ok(zip_of(&[
                    (
                        "Cores/Spiritualized.GB/core.json",
                        core_json("2.0", "gba").as_bytes(),
                    ),
                    (
                        "Cores/Spiritualized.GB/data.json",
                        data_json(r#"{"filename":"gba_bios.bin","size_exact":4}"#).as_bytes(),
                    ),
                ])),
                _ => unreachable!(),
            }
        }
    }

    /// DATs only come from the library's folder, because the tests never sync.
    type Tested = Pocketeer<Libretro, Covers, CoreSource>;

    struct Setup {
        _dir: tempfile::TempDir,
        pocketeer: Tested,
        /// How many core downloads have been asked for.
        downloaded: Rc<Cell<u32>>,
    }

    impl Setup {
        /// Runs each command, and returns the lines a front end would show.
        fn run(&mut self, commands: &[Command]) -> Vec<String> {
            let reports = commands.iter().map(|command| self.pocketeer.run(command));
            reports.map(|report| report.to_line()).collect()
        }

        fn games_on_device(&self) -> Vec<PathBuf> {
            entry_paths(self.pocketeer.entries_on_device())
        }
    }

    fn gba(name: &str) -> LibraryKey {
        LibraryKey {
            platform: Platform::Gba,
            name: name.into(),
        }
    }

    fn gba_on_device(path: &str) -> DeviceKey {
        DeviceKey {
            platform: Platform::Gba,
            path: path.into(),
        }
    }

    /// A library, a mirror holding [`GBA_DAT`], and an open, scanned device.
    /// `device` and `library` list the files each has.
    fn setup(device: &[(&str, &[u8])], library: &[(&str, &[u8])]) -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        put(
            root,
            "library/dats/Nintendo - Game Boy Advance.dat",
            GBA_DAT.as_bytes(),
        );
        make_empty_device(&root.join("device"));
        for (path, bytes) in device {
            put(&root.join("device"), path, bytes);
        }
        for (path, bytes) in library {
            put(&root.join("library"), path, bytes);
        }
        let source = CoreSource::default();
        let downloaded = source.downloaded.clone();
        let mut pocketeer =
            Pocketeer::open_with(&root.join("library"), Libretro, Covers, source).unwrap();
        pocketeer.open_device(&root.join("device")).unwrap();
        pocketeer.scan_device().unwrap();
        Setup {
            _dir: dir,
            pocketeer,
            downloaded,
        }
    }

    #[test]
    fn game_rows_pair_by_contents_and_track_saves() {
        let mut setup = setup(
            &[
                ("Assets/gba/common/Tetris.gba", b"hello"),
                ("Saves/gba/common/Tetris.sav", b"save"),
                ("Assets/gba/common/Only Here.gba", b"device"),
            ],
            &[
                ("roms/gba/Tetris (World).gba", b"hello"),
                ("roms/gba/Only There.gba", b"library"),
            ],
        );
        // The library's games come first, each with its copy on the device
        // whatever its name, then those only on the device.
        let rows: Vec<_> = setup
            .pocketeer
            .game_rows()
            .iter()
            .map(|row| {
                let local = row
                    .local_entry()
                    .map(|local_entry| local_entry.name.clone());
                let on_device = row
                    .device_entry()
                    .map(|device_entry| device_entry.path.display().to_string());
                (local, on_device)
            })
            .collect();
        let row = |local: Option<&str>, on_device: Option<&str>| {
            (local.map(String::from), on_device.map(String::from))
        };
        assert_eq!(
            rows,
            [
                row(Some("Only There.gba"), None),
                row(Some("Tetris (World).gba"), Some("Tetris.gba")),
                row(None, Some("Only Here.gba")),
            ]
        );
        assert_eq!(
            setup.pocketeer.games_on_device_in_library(),
            [gba_on_device("Tetris.gba")]
        );
        // A save on the device is new until the library has it.
        let row = setup.pocketeer.game_rows().remove(1);
        assert_eq!(row.saves_kept(), 0);
        assert!(row.has_new_save_on_device());
        setup.run(&[Command::PullSave(gba_on_device("Tetris.gba"))]);
        let row = setup.pocketeer.game_rows().remove(1);
        assert_eq!(row.saves_kept(), 1);
        assert!(!row.has_new_save_on_device());
    }

    #[test]
    fn refuses_what_is_gone_since_it_was_listed() {
        let mut setup = setup(
            &[("Assets/gba/common/Tetris.gba", b"hello")],
            &[
                ("roms/gba/A.gba", b"a"),
                ("roms/gba/Tetris (World).gba", b"hello"),
            ],
        );
        let library = setup.pocketeer.library().root().to_path_buf();
        put_at(&library, "saves/gba/A/one.sav", b"one", 1_000);
        setup.pocketeer.library.reload().unwrap();
        // A game gone from the device, a game gone from the library, and a
        // version of a save gone from the library.
        let device = setup.pocketeer.device().unwrap().root().to_path_buf();
        fs::remove_file(device.join("Assets/gba/common/Tetris.gba")).unwrap();
        setup.pocketeer.scan_device().unwrap();
        fs::remove_file(library.join("roms/gba/Tetris (World).gba")).unwrap();
        fs::remove_file(library.join("saves/gba/A/one.sav")).unwrap();
        setup.pocketeer.library.reload().unwrap();
        let reports: Vec<_> = [
            Command::PullSave(gba_on_device("Tetris.gba")),
            Command::FetchCover(gba("Tetris (World).gba")),
            Command::TrashSave(gba("A.gba"), "one".into()),
        ]
        .iter()
        .map(|command| setup.pocketeer.run(command))
        .collect();
        assert_eq!(
            reports.iter().map(Report::to_line).collect::<Vec<_>>(),
            [
                "gba/Tetris.gba: error: the device doesn't have the game",
                "gba/Tetris (World).gba: error: the library doesn't have the game",
                "gba/A.gba: the library has no such save",
            ]
        );
        assert!(reports.iter().all(|report| !report.ok));
    }

    #[test]
    fn covers_fetch_and_import() {
        let mut setup = setup(
            &[],
            &[
                ("roms/gba/tetris.gba", b"hello"),
                ("roms/gba/My Hack.gba", b"hack"),
                ("roms/gba/Other.gba", b"other"),
            ],
        );
        let root = setup.pocketeer.library().root().to_path_buf();
        // A cover is looked for under the DAT's name, or else under the file
        // name, and kept under the game's name. A game with a cover gets no
        // other.
        let mut fetch: Vec<_> = setup.pocketeer.games_without_covers();
        fetch.push(gba("tetris.gba"));
        let fetch: Vec<_> = fetch.into_iter().map(Command::FetchCover).collect();
        assert_eq!(
            setup.run(&fetch),
            [
                "gba/My Hack.gba: cover downloaded",
                "gba/Other.gba: no cover found for it",
                "gba/tetris.gba: cover downloaded",
                "gba/tetris.gba: it has a cover already",
            ]
        );
        assert_eq!(fs::read(root.join("covers/gba/tetris.png")).unwrap(), PNG);
        assert_eq!(fs::read(root.join("covers/gba/My Hack.png")).unwrap(), PNG);
        // A file that isn't a PNG, JPEG or WebP image isn't imported, and the
        // cover stays.
        let elsewhere = tempfile::tempdir().unwrap();
        let gif = put(elsewhere.path(), "mine.gif", b"GIF89a....");
        assert_eq!(
            setup.run(&[Command::ImportCover(gba("tetris.gba"), gif)]),
            ["gba/tetris.gba: not a PNG, JPEG or WebP file"]
        );
        assert_eq!(fs::read(root.join("covers/gba/tetris.png")).unwrap(), PNG);
        // A cover is known by its contents, and the file stays.
        let from = put(elsewhere.path(), "mine.dat", PNG);
        setup.run(&[Command::ImportCover(gba("Other.gba"), from.clone())]);
        assert_eq!(fs::read(root.join("covers/gba/Other.png")).unwrap(), PNG);
        assert_eq!(fs::read(&from).unwrap(), PNG);
        // In memory as on disk.
        let in_memory = setup.pocketeer.library().entries().to_vec();
        setup.pocketeer.library.reload().unwrap();
        assert_eq!(setup.pocketeer.library().entries(), in_memory);
    }

    #[test]
    fn expands_dirs_and_keeps_files_given_directly() {
        let dir = tempfile::tempdir().unwrap();
        let game = put(dir.path(), "Mine.gba", b"mine");
        let deep = put(dir.path(), "old/gb/Deep.GB", b"deep");
        put(dir.path(), "README.txt", b"read me");
        put(dir.path(), "old/.Hidden.gba", b"hidden");
        let apart = tempfile::tempdir().unwrap();
        let lone = put(apart.path(), "lone.txt", b"lone");
        let paths = [dir.path().to_path_buf(), lone.clone()];
        assert_eq!(
            expand_import_paths(&paths).unwrap(),
            [
                (game, Some(Platform::Gba)),
                (deep, Some(Platform::Gb)),
                (lone, None)
            ]
        );
    }

    #[test]
    fn proposes_dat_name_unless_game_has_it_or_dats_do_not_know_game() {
        for (path, bytes, expected) in [
            ("roms/gba/Old.gba", b"hello".as_slice(), Some("Old.gba")),
            (
                "roms/gba/tetris (world).gba",
                b"hello",
                Some("tetris (world).gba"),
            ),
            ("roms/gba/Tetris (World).gba", b"hello", None),
            ("roms/gba/Mine.gba", b"not in the DAT", None),
        ] {
            let setup = setup(&[], &[(path, bytes)]);
            let keys: Vec<_> = setup
                .pocketeer
                .library()
                .entries()
                .iter()
                .map(LocalEntry::key)
                .collect();
            let expected: Vec<_> = expected
                .iter()
                .map(|name| (gba(name), "Tetris (World)".to_string()))
                .collect();
            assert_eq!(
                setup.pocketeer.renames_to_dat_names(&keys),
                expected,
                "{path}"
            );
        }
    }

    #[test]
    fn renames_on_device_and_leaves_library() {
        let mut setup = setup(
            &[
                ("Assets/gba/common/Old.gba", b"hello"),
                ("Saves/gba/common/Old.sav", b"save"),
            ],
            &[("roms/gba/Old.gba", b"hello")],
        );
        let rename = Command::RenameOnDevice(gba_on_device("Old.gba"), "New".into());
        assert_eq!(
            setup.run(&[rename]),
            ["gba/Old.gba: renamed to gba/New.gba"]
        );
        setup.pocketeer.settle().unwrap();
        let row = setup.pocketeer.game_rows().remove(0);
        assert_eq!(row.local_entry().unwrap().name, "Old.gba");
        let device_entry = row.device_entry().unwrap();
        assert_eq!(device_entry.path, Path::new("New.gba"));
        assert_eq!(device_entry.save.as_ref().unwrap().size, 4);
    }

    #[test]
    fn install_core_flow() {
        const GB: &str = "Spiritualized.GB";
        let install = Command::InstallCore(GB.into());
        // Its `core.zip` declares `gba_bios.bin`.
        let mut setup = setup(&[("Assets/gba/common/gba_bios.bin", b"four")], &[]);
        let device = setup.pocketeer.device().unwrap().root().to_path_buf();
        assert_eq!(setup.games_on_device(), [PathBuf::from("gba_bios.bin")]);
        // Without a device every core is only available, and nothing is
        // downloaded.
        setup.pocketeer.close_device();
        let core_list = setup.pocketeer.load_core_list().unwrap();
        assert!(matches!(core_list.rows[..], [CoreRow::Available(_)]));
        assert_eq!(
            setup.run(std::slice::from_ref(&install)),
            ["Spiritualized.GB: error: no device is open"]
        );
        assert_eq!(setup.downloaded.get(), 0);
        // A device that hasn't been read isn't read when settling.
        setup.pocketeer.open_device(&device).unwrap();
        assert_eq!(
            setup.run(std::slice::from_ref(&install)),
            ["Spiritualized.GB: 2.0 installed"]
        );
        setup.pocketeer.settle().unwrap();
        assert_eq!(setup.downloaded.get(), 1);
        assert_eq!(setup.games_on_device(), [] as [PathBuf; 0]);
        // A file the core loads isn't a game.
        setup.pocketeer.scan_device().unwrap();
        assert_eq!(setup.games_on_device(), [] as [PathBuf; 0]);
        let updates = setup.pocketeer.find_cores_with_updates().unwrap();
        assert_eq!(updates, [] as [String; 0]);
        // The newest version isn't downloaded again.
        assert_eq!(
            setup.run(&[install, Command::InstallCore("no.Such".into())]),
            [
                "Spiritualized.GB: the device has the newest version",
                "no.Such: not a core that can be installed",
            ]
        );
        assert_eq!(setup.downloaded.get(), 1);
        // Without the core, the file is a game again once settled.
        setup.run(&[Command::RemoveCore(GB.into())]);
        assert_eq!(setup.games_on_device(), [] as [PathBuf; 0]);
        setup.pocketeer.settle().unwrap();
        assert_eq!(setup.games_on_device(), [PathBuf::from("gba_bios.bin")]);
    }

    #[test]
    fn uses_next_copy_after_deleting_game_once_settled() {
        let mut setup = setup(
            &[
                ("Assets/gba/common/One.gba", b"copy"),
                ("Assets/gba/common/RPG/Two.gba", b"copy"),
            ],
            &[],
        );
        setup.run(&[Command::DeleteGame(gba_on_device("One.gba"))]);
        setup.pocketeer.settle().unwrap();
        assert_eq!(setup.games_on_device(), [PathBuf::from("RPG/Two.gba")]);
    }
}
