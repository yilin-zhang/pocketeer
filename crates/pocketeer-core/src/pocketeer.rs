//! Everything Pocketeer does, in one place for every front end.

use std::{
    ops::ControlFlow,
    path::{Path, PathBuf},
};

use fs_err as fs;

use crate::{
    cores::{
        BiosRow, CoreIndex, CoreList, CoreRow, InstallCoreError, InstallCoreOutcome, PutBiosError,
        RemoteCore, RemoteCoreSource, RemoveCoreError,
    },
    cover::{Cover, CoverIndex, RemoteCoverSource},
    dat::{DatMirror, RemoteDatSource, SyncReport},
    device::{
        DeleteOrphanSaveError, DeleteSaveOutcome, Device, DeviceEntry, EjectDeviceError,
        OpenDeviceError, OrphanDeviceSave, RenameOnDeviceError, ScanReport,
    },
    error::BasicError,
    game::Game,
    library::{
        AddGameError, AddGameOutcome, Library, LibrarySave, LocalEntry, OpenLibraryError,
        OrphanLocalSave, RenameInLibraryError, TrashSaveAllOutcome, TrashSaveError,
    },
    log::{FrontEnd, LogEntry, LoggedError, Logger, OperationReport},
    os,
    platform::Platform,
    sources::{libretro::Libretro, openfpga::OpenFpga},
    transfer::{
        self, PullSaveOutcome, PushGameError, PushGameOutcome, PushSaveError, PushSaveOutcome,
    },
};

// ============================================================================
// Batches
// ============================================================================

/// How a batch ended. `Break` means the callback stopped it. An error means the
/// log couldn't be written, or the device couldn't be scanned again after it;
/// each item's own failure is in its report.
pub type BatchResult = Result<ControlFlow<()>, LoggedError<BasicError>>;

// ============================================================================
// What operations return
// ============================================================================

#[derive(Debug, thiserror::Error)]
pub enum ImportCoverError {
    #[error("not a PNG, JPEG or WebP file")]
    NotAnImage,
    #[error(transparent)]
    Other(#[from] BasicError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchCoverOutcome {
    Fetched,
    AlreadyHad,
    /// The source has no cover for it.
    NoCover,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportSaveOutcome {
    /// The version it was kept as.
    Imported(LibrarySave),
    /// The library already has a version with the same contents.
    AlreadyHad,
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
    logger: Logger,
}

impl Pocketeer {
    /// Opens the library at `library` with the DATs it keeps, without going
    /// online. No device is open yet. Operations are logged under `front_end`.
    pub fn open(library: &Path, front_end: FrontEnd) -> Result<Self, OpenLibraryError> {
        Pocketeer::open_with(library, front_end, Libretro, Libretro, OpenFpga)
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
    fn open_with(
        library: &Path,
        front_end: FrontEnd,
        dats: D,
        covers: C,
        cores: I,
    ) -> Result<Self, OpenLibraryError> {
        let library = Library::open(library)?;
        let dats = DatMirror::open(&library.layout().dats(), dats)?;
        let covers = CoverIndex::open(&library.layout().cover_index(), covers);
        let cores = CoreIndex::open(&library.layout().core_index(), cores);
        let logger = Logger::new(library.layout().log(), front_end);
        Ok(Self {
            library,
            dats,
            covers,
            cores,
            device: None,
            logger,
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

    /// Ejects the open device and closes it. If the system won't eject it, it
    /// stays open.
    /// Returns the root of the device that was ejected.
    pub fn eject_device(
        &mut self,
        on_logged: impl FnOnce(&Self, LogEntry),
    ) -> Result<PathBuf, LoggedError<EjectDeviceError>> {
        let ejected = match &self.device {
            Some(device) => device.eject().map(|()| device.root().to_path_buf()),
            None => Err(BasicError::NoDevice.into()),
        };
        if ejected.is_ok() {
            self.close_device();
        }
        self.log_one(ejected, OperationReport::eject, on_logged)
    }

    // ---- Reload -----------------------------------------------------------------

    /// Reads the library's folder again, and the open device's, for changes
    /// made outside Pocketeer.
    pub fn reload(
        &mut self,
        on_logged: impl FnOnce(&Self, LogEntry),
    ) -> Result<ScanReport, LoggedError<BasicError>> {
        let read = self.library.reload().and_then(|()| match self.device {
            Some(_) => self.scan_device(),
            None => Ok(ScanReport::default()),
        });
        self.log_one(read, OperationReport::reload, on_logged)
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
        let device_entries = self
            .device
            .as_ref()
            .map_or(&[][..], |device| device.entries());
        device_entries
            .iter()
            .filter(|device_entry| self.library.entry_for(&device_entry.game).is_none())
    }

    pub fn games_without_covers(&self) -> Vec<LocalEntry> {
        let entries = self.library.entries().iter();
        entries
            .filter(|entry| entry.cover_path.is_none())
            .cloned()
            .collect()
    }

    pub fn games_only_on_device(&self) -> Vec<DeviceEntry> {
        self.entries_only_on_device().cloned().collect()
    }

    pub fn games_on_device_in_library(&self) -> Vec<DeviceEntry> {
        let library = &self.library;
        let mut games = self.games_on_device();
        games.retain(|game| library.entry_for(&game.game).is_some());
        games
    }

    fn games_on_device(&self) -> Vec<DeviceEntry> {
        let entries = self
            .device
            .as_ref()
            .map_or(&[][..], |device| device.entries());
        entries.to_vec()
    }

    // ---- DATs -------------------------------------------------------------------

    pub fn sync_dats(
        &mut self,
        on_logged: impl FnOnce(&Self, LogEntry),
    ) -> Result<SyncReport, LoggedError<BasicError>> {
        let synced = self.dats.sync();
        self.log_one(synced, OperationReport::sync_dats, on_logged)
    }

    /// The games among `games` whose name differs from their DAT name, each
    /// paired with that name. Games the DATs don't know are left out.
    pub fn renames_to_dat_names(&self, games: &[LocalEntry]) -> Vec<(LocalEntry, String)> {
        games
            .iter()
            .filter_map(|local_entry| {
                let dat = self.dats.lookup(&local_entry.game)?;
                (local_entry.stem() != dat.name).then(|| (local_entry.clone(), dat.name.clone()))
            })
            .collect()
    }

    // ---- Games ------------------------------------------------------------------

    pub fn pull_games(
        &mut self,
        games: &[DeviceEntry],
        on_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> (BatchResult, Vec<LocalEntry>) {
        let mut added_entries = Vec::new();
        let batch_result = self.for_each_item(
            games,
            |pocketeer, game| {
                let outcome = pocketeer.pull_game(game);
                if let Ok(AddGameOutcome::Added(local_entry)) = &outcome {
                    added_entries.push(local_entry.clone());
                }
                OperationReport::pull_game(game, &outcome)
            },
            on_logged,
        );
        (batch_result, added_entries)
    }

    fn pull_game(&mut self, device_entry: &DeviceEntry) -> Result<AddGameOutcome, AddGameError> {
        let device_entry = self.still_on_device(device_entry)?;
        let device = self.device.as_ref().ok_or(BasicError::NoDevice)?;
        transfer::pull_game(&mut self.library, device, &device_entry)
    }

    pub fn push_games(
        &mut self,
        games: &[LocalEntry],
        on_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> (BatchResult, Vec<DeviceEntry>) {
        let mut pushed_entries = Vec::new();
        let batch_result = self.for_each_item(
            games,
            |pocketeer, game| {
                let outcome = pocketeer.push_game(game);
                if let Ok(PushGameOutcome::Pushed(device_entry)) = &outcome {
                    pushed_entries.push(device_entry.clone());
                }
                OperationReport::push_game(game, &outcome)
            },
            on_logged,
        );
        (batch_result, pushed_entries)
    }

    fn push_game(&mut self, local_entry: &LocalEntry) -> Result<PushGameOutcome, PushGameError> {
        let local_entry = self.still_in_library(local_entry)?;
        let device = self.device.as_mut().ok_or(BasicError::NoDevice)?;
        transfer::push_game(&self.library, device, &local_entry)
    }

    /// Imports each file as a game of the platform paired with it, under its
    /// file name. The files stay where they are.
    pub fn import_games(
        &mut self,
        files: &[(PathBuf, Platform)],
        on_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> (BatchResult, Vec<LocalEntry>) {
        let mut added_entries = Vec::new();
        let batch_result = self.for_each_item(
            files,
            |pocketeer, (from, platform)| {
                let outcome = pocketeer.library.import_game(from, platform.clone());
                if let Ok(AddGameOutcome::Added(local_entry)) = &outcome {
                    added_entries.push(local_entry.clone());
                }
                // The log outlives the folder the command was run in.
                let from = std::path::absolute(from).unwrap_or_else(|_| from.clone());
                OperationReport::import_game(&from, &outcome)
            },
            on_logged,
        );
        (batch_result, added_entries)
    }

    /// Renames each game in the library to the name paired with it, keeping its
    /// extension. The device isn't touched.
    pub fn rename_games_in_library(
        &mut self,
        games: &[(LocalEntry, String)],
        on_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            games,
            |pocketeer, (local_entry, new)| {
                OperationReport::rename_in_library(
                    local_entry,
                    &pocketeer.rename_game_in_library(local_entry, new),
                )
            },
            on_logged,
        )
    }

    fn rename_game_in_library(
        &mut self,
        local_entry: &LocalEntry,
        new: &str,
    ) -> Result<LocalEntry, RenameInLibraryError> {
        let local_entry = self.still_in_library(local_entry)?;
        self.library
            .rename_game(&local_entry.game.platform, &local_entry.name, new)
    }

    /// Renames each game on the device to the name paired with it, keeping its
    /// extension and folder. The library isn't touched. Which copy of a game
    /// comes first depends on its path, so the device is read again afterward.
    pub fn rename_games_on_device(
        &mut self,
        games: &[(DeviceEntry, String)],
        on_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        let batch_result = self.for_each_item(
            games,
            |pocketeer, (device_entry, new)| {
                OperationReport::rename_on_device(
                    device_entry,
                    &pocketeer.rename_game_on_device(device_entry, new),
                )
            },
            on_logged,
        );
        self.rescan_device_after(batch_result)
    }

    fn rename_game_on_device(
        &mut self,
        device_entry: &DeviceEntry,
        new: &str,
    ) -> Result<DeviceEntry, RenameOnDeviceError> {
        let device_entry = self.still_on_device(device_entry)?;
        let device = self.device.as_mut().ok_or(BasicError::NoDevice)?;
        device.rename_game(&device_entry, new)
    }

    pub fn fetch_covers(
        &mut self,
        games: &[LocalEntry],
        on_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            games,
            |pocketeer, game| OperationReport::fetch_cover(game, &pocketeer.fetch_cover(game)),
            on_logged,
        )
    }

    /// Downloads a cover for `local_entry` if it has none. It looks under the DAT
    /// name if known and otherwise under the file name without its extension.
    fn fetch_cover(&mut self, local_entry: &LocalEntry) -> Result<FetchCoverOutcome, BasicError> {
        let local_entry = self.still_in_library(local_entry)?;
        if local_entry.cover_path.is_some() {
            return Ok(FetchCoverOutcome::AlreadyHad);
        }
        let name = match self.dats.lookup(&local_entry.game) {
            Some(dat) => dat.name.clone(),
            None => local_entry.stem().to_string(),
        };
        let platform = &local_entry.game.platform;
        let Some(cover) = self.covers.fetch(platform, &name)? else {
            return Ok(FetchCoverOutcome::NoCover);
        };
        self.library
            .set_cover(platform, &local_entry.name, &cover)?;
        Ok(FetchCoverOutcome::Fetched)
    }

    /// Makes each file the cover of the game paired with it. Any existing cover
    /// goes to the system's trash.
    pub fn import_covers(
        &mut self,
        covers: &[(LocalEntry, PathBuf)],
        on_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            covers,
            |pocketeer, (local_entry, from)| {
                let result = pocketeer.import_cover(local_entry, from);
                // The log outlives the folder the command was run in.
                let from = std::path::absolute(from).unwrap_or_else(|_| from.clone());
                OperationReport::import_cover(local_entry, &from, &result)
            },
            on_logged,
        )
    }

    fn import_cover(
        &mut self,
        local_entry: &LocalEntry,
        from: &Path,
    ) -> Result<(), ImportCoverError> {
        let local_entry = self.still_in_library(local_entry)?;
        let bytes = fs::read(from).map_err(BasicError::from)?;
        let cover = Cover::from_bytes(bytes).ok_or(ImportCoverError::NotAnImage)?;
        self.library
            .set_cover(&local_entry.game.platform, &local_entry.name, &cover)?;
        Ok(())
    }

    pub fn trash_games_in_library(
        &mut self,
        games: &[LocalEntry],
        on_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            games,
            |pocketeer, game| {
                OperationReport::trash_game(game, &pocketeer.trash_game_in_library(game))
            },
            on_logged,
        )
    }

    fn trash_game_in_library(&mut self, local_entry: &LocalEntry) -> Result<(), BasicError> {
        let local_entry = self.still_in_library(local_entry)?;
        self.library.trash_game(&local_entry)
    }

    /// Once the first copy of a game is deleted the next one takes its place,
    /// so the device is read again afterward, however the batch ended.
    pub fn delete_games_on_device(
        &mut self,
        games: &[DeviceEntry],
        on_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        let batch_result = self.for_each_item(
            games,
            |pocketeer, game| {
                OperationReport::delete_game(game, &pocketeer.delete_game_on_device(game))
            },
            on_logged,
        );
        self.rescan_device_after(batch_result)
    }

    /// Deletes the file of `device_entry` from the device for good. Its save
    /// stays there.
    fn delete_game_on_device(&mut self, device_entry: &DeviceEntry) -> Result<(), BasicError> {
        let device_entry = self.still_on_device(device_entry)?;
        let device = self.device.as_mut().ok_or(BasicError::NoDevice)?;
        device.remove_game(&device_entry)
    }

    // ---- Saves ------------------------------------------------------------------

    pub fn pull_saves(
        &mut self,
        games: &[DeviceEntry],
        on_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            games,
            |pocketeer, game| OperationReport::pull_save(game, &pocketeer.pull_save(game)),
            on_logged,
        )
    }

    fn pull_save(&mut self, device_entry: &DeviceEntry) -> Result<PullSaveOutcome, BasicError> {
        let device_entry = self.still_on_device(device_entry)?;
        let device = self.device.as_ref().ok_or(BasicError::NoDevice)?;
        transfer::pull_save(&mut self.library, device, &device_entry)
    }

    /// Pushes each version to the game paired with it.
    pub fn push_save_versions(
        &mut self,
        versions: &[(DeviceEntry, LibrarySave)],
        on_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            versions,
            |pocketeer, (game, version)| {
                OperationReport::push_save(game, &pocketeer.push_save(game, version))
            },
            on_logged,
        )
    }

    fn push_save(
        &mut self,
        device_entry: &DeviceEntry,
        version: &LibrarySave,
    ) -> Result<PushSaveOutcome, PushSaveError> {
        let device_entry = self.still_on_device(device_entry)?;
        let device = self.device.as_mut().ok_or(BasicError::NoDevice)?;
        transfer::push_save(&self.library, device, &device_entry, version)
    }

    /// Imports each file as a version of the save of the game paired with it.
    pub fn import_saves(
        &mut self,
        saves: &[(LocalEntry, PathBuf)],
        on_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            saves,
            |pocketeer, (local_entry, from)| {
                let result = pocketeer.import_save(local_entry, from);
                // The log outlives the folder the command was run in.
                let from = std::path::absolute(from).unwrap_or_else(|_| from.clone());
                OperationReport::import_save(local_entry, &from, &result)
            },
            on_logged,
        )
    }

    /// Keeps the file at `from` as a new version of `local_entry`'s save, dated when
    /// the file was written. The file stays where it is.
    fn import_save(
        &mut self,
        local_entry: &LocalEntry,
        from: &Path,
    ) -> Result<ImportSaveOutcome, BasicError> {
        let local_entry = self.still_in_library(local_entry)?;
        let added = self
            .library
            .add_save(&local_entry.game.platform, &local_entry.name, from)?;
        Ok(added.map_or(ImportSaveOutcome::AlreadyHad, ImportSaveOutcome::Imported))
    }

    /// Trashes every version of each game's save.
    pub fn trash_saves_in_library(
        &mut self,
        games: &[LocalEntry],
        on_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            games,
            |pocketeer, game| {
                OperationReport::trash_every_version(
                    game,
                    &pocketeer.trash_every_version_in_library(game),
                )
            },
            on_logged,
        )
    }

    fn trash_every_version_in_library(
        &mut self,
        local_entry: &LocalEntry,
    ) -> Result<TrashSaveAllOutcome, BasicError> {
        let local_entry = self.still_in_library(local_entry)?;
        self.library.trash_all_saves(&local_entry)
    }

    /// Trashes each version of the save of the game paired with it.
    pub fn trash_save_versions_in_library(
        &mut self,
        versions: &[(LocalEntry, LibrarySave)],
        on_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            versions,
            |pocketeer, (game, version)| {
                let result = pocketeer.trash_save_version(game, &version.name);
                OperationReport::trash_save_version(game, &version.name, &result)
            },
            on_logged,
        )
    }

    fn trash_save_version(
        &mut self,
        local_entry: &LocalEntry,
        version_name: &str,
    ) -> Result<(), TrashSaveError> {
        let local_entry = self.still_in_library(local_entry)?;
        self.library.trash_save_version(&local_entry, version_name)
    }

    pub fn delete_saves_on_device(
        &mut self,
        games: &[DeviceEntry],
        on_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            games,
            |pocketeer, game| {
                OperationReport::delete_save(game, &pocketeer.delete_save_on_device(game))
            },
            on_logged,
        )
    }

    /// Deletes the save of `device_entry` from the device for good, even if the
    /// library doesn't have it.
    fn delete_save_on_device(
        &mut self,
        device_entry: &DeviceEntry,
    ) -> Result<DeleteSaveOutcome, BasicError> {
        let device_entry = self.still_on_device(device_entry)?;
        let device = self.device.as_mut().ok_or(BasicError::NoDevice)?;
        device.remove_save(&device_entry)
    }

    // ---- Orphan saves -----------------------------------------------------------

    /// The saves on the device with no game, or none if no device is open.
    pub fn find_orphan_saves_on_device(&self) -> Result<Vec<OrphanDeviceSave>, BasicError> {
        self.device
            .as_ref()
            .map_or(Ok(Vec::new()), Device::find_orphan_saves)
    }

    /// Trashes every version of each orphan's save.
    pub fn trash_orphan_saves_in_library(
        &mut self,
        orphans: &[OrphanLocalSave],
        on_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            orphans,
            |pocketeer, orphan| {
                OperationReport::trash_orphan_save(
                    orphan,
                    &pocketeer.library.trash_orphan_save(orphan),
                )
            },
            on_logged,
        )
    }

    pub fn delete_orphan_saves_on_device(
        &mut self,
        orphans: &[OrphanDeviceSave],
        on_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            orphans,
            |pocketeer, orphan| {
                OperationReport::delete_orphan_save(
                    orphan,
                    &pocketeer.delete_orphan_save_on_device(orphan),
                )
            },
            on_logged,
        )
    }

    fn delete_orphan_save_on_device(
        &mut self,
        orphan: &OrphanDeviceSave,
    ) -> Result<(), DeleteOrphanSaveError> {
        let device = self.device.as_ref().ok_or(BasicError::NoDevice)?;
        device.remove_orphan_save(orphan)
    }

    // ---- Cores ------------------------------------------------------------------

    // Files a core loads aren't games, so the list of games changes with the
    // cores.

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

    /// Installs the newest release of each core on the device.
    pub fn install_cores(
        &mut self,
        ids: &[String],
        on_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        let batch_result = self.for_each_item(
            ids,
            |pocketeer, id| OperationReport::install_core(id, &pocketeer.install_core(id)),
            on_logged,
        );
        self.rescan_device_after(batch_result)
    }

    /// Installs the newest release of the core, unless it is already there.
    fn install_core(&mut self, id: &str) -> Result<InstallCoreOutcome, InstallCoreError> {
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

    /// Deletes each core from the device for good.
    pub fn remove_cores(
        &mut self,
        ids: &[String],
        on_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        let batch_result = self.for_each_item(
            ids,
            |pocketeer, id| OperationReport::remove_core(id, &pocketeer.remove_core(id)),
            on_logged,
        );
        self.rescan_device_after(batch_result)
    }

    fn remove_core(&mut self, id: &str) -> Result<(), RemoveCoreError> {
        let device = self.device.as_ref().ok_or(BasicError::NoDevice)?;
        device.remove_core(id)
    }

    // ---- BIOS files -------------------------------------------------------------

    pub fn read_bios_rows(&self) -> Result<Vec<BiosRow>, BasicError> {
        self.device
            .as_ref()
            .ok_or(BasicError::NoDevice)?
            .read_bios_rows()
    }

    /// Copies each file to where the row paired with it says.
    pub fn put_bios_files(
        &mut self,
        files: &[(BiosRow, PathBuf)],
        on_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            files,
            |pocketeer, (row, from)| {
                let result = pocketeer.put_bios_file(row, from);
                // The log outlives the folder the command was run in.
                let from = std::path::absolute(from).unwrap_or_else(|_| from.clone());
                OperationReport::put_bios(row, &from, &result)
            },
            on_logged,
        )
    }

    /// Copies the file at `from` to where `row` says. The file stays where it
    /// is.
    fn put_bios_file(&mut self, row: &BiosRow, from: &Path) -> Result<(), PutBiosError> {
        let device = self.device.as_ref().ok_or(BasicError::NoDevice)?;
        device.put_bios(&row.platform, &row.path, from)
    }

    // ---- Logging and checks -----------------------------------------------------

    // Every call from a front end records what it did in the log. Single
    // operations go through `log_one`, and batches through `for_each_item`.

    /// Logs `result`, passes the entry to `on_logged`, and returns `result`. If
    /// it succeeded but can't be logged, that is the error.
    fn log_one<T, E>(
        &self,
        result: Result<T, E>,
        to_report: impl FnOnce(&Result<T, E>) -> OperationReport,
        on_logged: impl FnOnce(&Self, LogEntry),
    ) -> Result<T, LoggedError<E>> {
        let (entry, log_write_result) = self.logger.record(to_report(&result));
        on_logged(self, entry);
        let value = result.map_err(LoggedError::OperationFailed)?;
        log_write_result.map_err(LoggedError::LogFailed)?;
        Ok(value)
    }

    // The batch methods below use `for_each_item` to handle one item at a time.
    // Each item's report is logged and passed to `on_logged` before the next
    // item starts.
    //
    // An item that fails doesn't stop the others. `on_logged` can stop the
    // batch by returning `Break`, which takes effect after the current item,
    // and it can read the current state. A failed log write also stops the
    // batch. The methods return whether it was stopped, or the log error.

    fn for_each_item<Item>(
        &mut self,
        items: &[Item],
        mut op: impl FnMut(&mut Self, &Item) -> OperationReport,
        mut on_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        for item in items {
            let report = op(self, item);
            let (entry, log_write_result) = self.logger.record(report);
            let control_flow = on_logged(self, entry);
            log_write_result.map_err(LoggedError::LogFailed)?;
            if control_flow.is_break() {
                return Ok(control_flow);
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Scans the device again after a batch, however it ended, if it had been
    /// scanned before.
    fn rescan_device_after(&mut self, batch_result: BatchResult) -> BatchResult {
        let scan_result = match &mut self.device {
            Some(device) if device.has_been_scanned() => device.scan().map(|_| ()),
            _ => Ok(()),
        };
        let control_flow = batch_result?;
        scan_result.map_err(LoggedError::OperationFailed)?;
        Ok(control_flow)
    }

    // Operations look up the entries they are given again, because they may
    // have changed or gone since the caller saw them.

    /// `entry` as the device has it now.
    fn still_on_device(&self, entry: &DeviceEntry) -> Result<DeviceEntry, BasicError> {
        let device = self.device.as_ref().ok_or(BasicError::NoDevice)?;
        let current = device.entry_at(&entry.game.platform, &entry.path);
        current.cloned().ok_or(BasicError::NotOnDevice)
    }

    /// `entry` as the library has it now.
    fn still_in_library(&self, entry: &LocalEntry) -> Result<LocalEntry, BasicError> {
        let current = self.library.entry_named(&entry.game.platform, &entry.name);
        current.cloned().ok_or(BasicError::NotInLibrary)
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
    use crate::log::testing::logged_lines;
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
        fn first_local(&self) -> LocalEntry {
            self.pocketeer.library().entries()[0].clone()
        }

        fn first_on_device(&self) -> DeviceEntry {
            self.pocketeer.games_on_device().remove(0)
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
        let mut pocketeer = Pocketeer::open_with(
            &root.join("library"),
            FrontEnd::Cli,
            Libretro,
            Covers,
            source,
        )
        .unwrap();
        pocketeer.open_device(&root.join("device")).unwrap();
        pocketeer.scan_device().unwrap();
        Setup {
            _dir: dir,
            pocketeer,
            downloaded,
        }
    }

    /// The library's log, one value for each line.
    fn log_lines(setup: &Setup) -> Vec<serde_json::Value> {
        logged_lines(&setup.pocketeer.library().layout().log())
    }

    /// What `run` passes to `on_logged`, as the lines a front end would show.
    fn lines_of(
        run: impl FnOnce(&mut dyn FnMut(&Tested, LogEntry) -> ControlFlow<()>) -> BatchResult,
    ) -> Vec<String> {
        let mut lines = Vec::new();
        let _ = run(&mut |_, entry| {
            lines.push(entry.report.to_line());
            ControlFlow::Continue(())
        })
        .unwrap();
        lines
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
        let in_library = setup.pocketeer.games_on_device_in_library();
        assert_eq!(entry_paths(&in_library), [PathBuf::from("Tetris.gba")]);
        // A save on the device is new until the library has it.
        let row = setup.pocketeer.game_rows().remove(1);
        assert_eq!(row.saves_kept(), 0);
        assert!(row.has_new_save_on_device());
        let tetris = in_library[0].clone();
        setup.pocketeer.pull_save(&tetris).unwrap();
        let row = setup.pocketeer.game_rows().remove(1);
        assert_eq!(row.saves_kept(), 1);
        assert!(!row.has_new_save_on_device());
    }

    #[test]
    fn refuses_stale_entries() {
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
        let device_entry = setup.first_on_device();
        let [a, tetris] = [0, 1].map(|i| setup.pocketeer.library().entries()[i].clone());
        let version = a.save.as_ref().unwrap().newest().clone();
        // A game gone from the device since it was listed.
        let device = setup.pocketeer.device().unwrap().root().to_path_buf();
        fs::remove_file(device.join("Assets/gba/common/Tetris.gba")).unwrap();
        setup.pocketeer.scan_device().unwrap();
        assert!(matches!(
            setup.pocketeer.pull_save(&device_entry),
            Err(BasicError::NotOnDevice)
        ));
        // A game gone from the library.
        fs::remove_file(library.join("roms/gba/Tetris (World).gba")).unwrap();
        setup.pocketeer.library.reload().unwrap();
        assert!(matches!(
            setup.pocketeer.fetch_cover(&tetris),
            Err(BasicError::NotInLibrary)
        ));
        // A version of a save gone from the library.
        fs::remove_file(library.join("saves/gba/A/one.sav")).unwrap();
        let versions = [(a, version)];
        let lines = lines_of(|on_logged| {
            setup
                .pocketeer
                .trash_save_versions_in_library(&versions, on_logged)
        });
        assert_eq!(lines, ["gba/A.gba: the library has no such save"]);
        let logged = log_lines(&setup);
        assert_eq!(logged[0]["version"], "one");
        assert_eq!(logged[0]["ok"], false);
    }

    #[test]
    fn batch_logs_each_item_and_stops() {
        let mut setup = setup(
            &[],
            &[
                ("roms/gba/A.gba", b"a"),
                ("roms/gba/B.gba", b"b"),
                ("roms/gba/C.gba", b"c"),
            ],
        );
        let [a, b, c] = [0, 1, 2].map(|i| setup.pocketeer.library().entries()[i].clone());
        let gone = LocalEntry {
            name: "Gone.gba".into(),
            ..a.clone()
        };
        // A failed game doesn't stop the batch. The callback stops it after
        // the game it was given, and only what was done is in the log.
        let mut seen = Vec::new();
        let (control_flow, _) = setup
            .pocketeer
            .push_games(&[gone, a, b.clone()], |_, entry| {
                seen.push((entry.report.subject.clone(), entry.report.is_ok));
                match seen.len() {
                    2 => ControlFlow::Break(()),
                    _ => ControlFlow::Continue(()),
                }
            });
        assert!(control_flow.unwrap().is_break());
        assert_eq!(
            seen,
            [
                ("gba/Gone.gba".to_string(), false),
                ("gba/A.gba".to_string(), true)
            ]
        );
        assert_eq!(
            entry_paths(&setup.pocketeer.games_on_device()),
            [PathBuf::from("A.gba")]
        );
        assert_eq!(log_lines(&setup).len(), 2);
        // If the log can't be written, the batch stops after reporting the
        // game.
        let log = setup.pocketeer.library().layout().log();
        fs::remove_file(&log).unwrap();
        fs::create_dir(&log).unwrap();
        let mut seen = Vec::new();
        let (batch_result, _) = setup.pocketeer.push_games(&[b, c], |_, entry| {
            seen.push(entry.report.subject.clone());
            ControlFlow::Continue(())
        });
        assert!(matches!(batch_result, Err(LoggedError::LogFailed(_))));
        assert_eq!(seen, ["gba/B.gba"]);
        assert_eq!(setup.pocketeer.games_on_device().len(), 2);
    }

    #[test]
    fn reload_rereads_both_sides_and_logs() {
        let mut setup = setup(
            &[("Assets/gba/common/One.gba", b"one")],
            &[("roms/gba/A.gba", b"a")],
        );
        let device = setup.pocketeer.device().unwrap().root().to_path_buf();
        let library = setup.pocketeer.library().root().to_path_buf();
        put(&device, "Assets/gba/common/RPG/Two.gba", b"one");
        put(&library, "roms/gba/B.gba", b"b");
        let report = setup.pocketeer.reload(|_, _| {}).unwrap();
        assert_eq!(
            entry_paths(&report.duplicates),
            [PathBuf::from("RPG/Two.gba")]
        );
        assert_eq!(setup.pocketeer.library().entries().len(), 2);
        assert_eq!(log_lines(&setup)[0]["message"], "read again");
        // If the log can't be written, the reload is still done and shown,
        // and the log's failure is returned.
        let log = setup.pocketeer.library().layout().log();
        fs::remove_file(&log).unwrap();
        fs::create_dir(&log).unwrap();
        put(&library, "roms/gba/C.gba", b"c");
        let mut reported = None;
        let read = setup
            .pocketeer
            .reload(|_, entry| reported = Some(entry.report.is_ok));
        assert!(matches!(read, Err(LoggedError::LogFailed(_))));
        assert_eq!(reported, Some(true));
        assert_eq!(setup.pocketeer.library().entries().len(), 3);
        // An operation's own failure comes before the log's.
        setup.pocketeer.close_device();
        assert!(matches!(
            setup.pocketeer.eject_device(|_, _| {}),
            Err(LoggedError::OperationFailed(EjectDeviceError::Other(
                BasicError::NoDevice
            )))
        ));
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
        // name, and kept under the game's name.
        let games = setup.pocketeer.games_without_covers();
        let lines = lines_of(|on_logged| setup.pocketeer.fetch_covers(&games, on_logged));
        assert_eq!(
            lines,
            [
                "gba/My Hack.gba: cover downloaded",
                "gba/Other.gba: no cover found for it",
                "gba/tetris.gba: cover downloaded",
            ]
        );
        assert_eq!(fs::read(root.join("covers/gba/tetris.png")).unwrap(), PNG);
        assert_eq!(fs::read(root.join("covers/gba/My Hack.png")).unwrap(), PNG);
        assert_eq!(setup.pocketeer.games_without_covers().len(), 1);
        // A game with a cover gets no other.
        let entry = |setup: &Setup, name: &str| {
            let library = setup.pocketeer.library();
            library.entry_named(&Platform::Gba, name).unwrap().clone()
        };
        let tetris = entry(&setup, "tetris.gba");
        let fetched = setup.pocketeer.fetch_cover(&tetris).unwrap();
        assert_eq!(fetched, FetchCoverOutcome::AlreadyHad);
        // A file that isn't a PNG, JPEG or WebP image isn't imported, and the
        // cover stays.
        let elsewhere = tempfile::tempdir().unwrap();
        let text = put(elsewhere.path(), "mine.png", b"not an image");
        let gif = put(elsewhere.path(), "mine.gif", b"GIF89a....");
        let covers = [(tetris.clone(), text), (tetris, gif)];
        let lines = lines_of(|on_logged| setup.pocketeer.import_covers(&covers, on_logged));
        assert_eq!(
            lines,
            [
                "gba/tetris.gba: not a PNG, JPEG or WebP file",
                "gba/tetris.gba: not a PNG, JPEG or WebP file",
            ]
        );
        assert_eq!(fs::read(root.join("covers/gba/tetris.png")).unwrap(), PNG);
        // A cover is known by its contents. The file stays, and the log keeps
        // where it came from.
        let from = put(elsewhere.path(), "mine.dat", PNG);
        let covers = [(entry(&setup, "Other.gba"), from.clone())];
        lines_of(|on_logged| setup.pocketeer.import_covers(&covers, on_logged));
        assert_eq!(fs::read(root.join("covers/gba/Other.png")).unwrap(), PNG);
        assert_eq!(fs::read(&from).unwrap(), PNG);
        let logged = log_lines(&setup);
        assert_eq!(logged.last().unwrap()["from"], from.display().to_string());
        // In memory as on disk.
        let in_memory = setup.pocketeer.library().entries().to_vec();
        setup.pocketeer.library.reload().unwrap();
        assert_eq!(setup.pocketeer.library().entries(), in_memory);
    }

    mod import_games {
        use super::*;

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
    }
    mod renaming {
        use super::*;

        /// A game in the library and on the device, under the same name.
        fn on_both() -> (Setup, LocalEntry, DeviceEntry) {
            let setup = setup(
                &[
                    ("Assets/gba/common/Old.gba", b"hello"),
                    ("Saves/gba/common/Old.sav", b"save"),
                ],
                &[("roms/gba/Old.gba", b"hello")],
            );
            let local_entry = setup.first_local();
            let device_entry = setup.first_on_device();
            (setup, local_entry, device_entry)
        }

        /// What `renames_to_dat_names` makes of the one game in the library.
        fn renames_of(setup: &Setup) -> Vec<(String, String)> {
            setup
                .pocketeer
                .renames_to_dat_names(setup.pocketeer.library().entries())
                .into_iter()
                .map(|(local_entry, new)| (local_entry.name, new))
                .collect()
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
                let expected: Vec<_> = expected
                    .iter()
                    .map(|name| (name.to_string(), "Tetris (World)".to_string()))
                    .collect();
                assert_eq!(renames_of(&setup), expected, "{path}");
            }
        }

        #[test]
        fn renames_on_device_and_leaves_library() {
            let (mut setup, _, device_entry) = on_both();
            let games = [(device_entry, "New".to_string())];
            let lines =
                lines_of(|on_logged| setup.pocketeer.rename_games_on_device(&games, on_logged));
            assert_eq!(lines, ["gba/Old.gba: renamed to gba/New.gba"]);
            let logged = log_lines(&setup);
            assert_eq!(logged[0]["device"], "gba/New.gba");
            let row = setup.pocketeer.game_rows().remove(0);
            assert_eq!(row.local_entry().unwrap().name, "Old.gba");
            let device_entry = row.device_entry().unwrap();
            assert_eq!(device_entry.path, Path::new("New.gba"));
            assert_eq!(device_entry.save.as_ref().unwrap().size, 4);
        }
    }

    mod cores {
        use super::*;

        const GB: &str = "Spiritualized.GB";

        fn install(setup: &mut Setup, id: &str) -> Vec<String> {
            let ids = [id.to_string()];
            lines_of(|on_logged| setup.pocketeer.install_cores(&ids, on_logged))
        }

        fn games(setup: &Setup) -> Vec<PathBuf> {
            entry_paths(&setup.pocketeer.games_on_device())
        }

        #[test]
        fn install_core_flow() {
            // Its `core.zip` declares `gba_bios.bin`.
            let mut setup = setup(&[("Assets/gba/common/gba_bios.bin", b"four")], &[]);
            let device = setup.pocketeer.device().unwrap().root().to_path_buf();
            assert_eq!(games(&setup), [PathBuf::from("gba_bios.bin")]);
            // Without a device every core is only available, and nothing is
            // downloaded.
            setup.pocketeer.close_device();
            let core_list = setup.pocketeer.load_core_list().unwrap();
            assert!(matches!(core_list.rows[..], [CoreRow::Available(_)]));
            assert_eq!(
                install(&mut setup, GB),
                ["Spiritualized.GB: error: no device is open"]
            );
            assert_eq!(setup.downloaded.get(), 0);
            // A device that hasn't been read isn't read after installing.
            setup.pocketeer.open_device(&device).unwrap();
            assert_eq!(install(&mut setup, GB), ["Spiritualized.GB: 2.0 installed"]);
            assert_eq!(setup.downloaded.get(), 1);
            assert_eq!(games(&setup), [] as [PathBuf; 0]);
            // A file the core loads isn't a game.
            setup.pocketeer.scan_device().unwrap();
            assert_eq!(games(&setup), [] as [PathBuf; 0]);
            let core_list = setup.pocketeer.load_core_list().unwrap();
            assert!(matches!(core_list.rows[..], [CoreRow::Both { .. }]));
            let updates = setup.pocketeer.find_cores_with_updates().unwrap();
            assert_eq!(updates, [] as [String; 0]);
            // The newest version isn't downloaded again.
            assert_eq!(
                install(&mut setup, GB),
                ["Spiritualized.GB: the device has the newest version"]
            );
            assert_eq!(setup.downloaded.get(), 1);
            assert_eq!(
                install(&mut setup, "no.Such"),
                ["no.Such: not a core that can be installed"]
            );
            // Without the core, the file is a game again.
            let ids = [GB.to_string()];
            lines_of(|on_logged| setup.pocketeer.remove_cores(&ids, on_logged));
            assert_eq!(games(&setup), [PathBuf::from("gba_bios.bin")]);
        }
    }

    #[test]
    fn uses_next_copy_after_deleting_game() {
        let mut setup = setup(
            &[
                ("Assets/gba/common/One.gba", b"copy"),
                ("Assets/gba/common/RPG/Two.gba", b"copy"),
            ],
            &[],
        );
        let games = setup.pocketeer.games_on_device();
        lines_of(|on_logged| setup.pocketeer.delete_games_on_device(&games, on_logged));
        let left = setup.pocketeer.games_on_device();
        assert_eq!(entry_paths(&left), [PathBuf::from("RPG/Two.gba")]);
    }
}
