//! Everything pocketeer does, in one place for every front end.

use std::{
    ops::ControlFlow,
    path::{Path, PathBuf},
};

use fs_err as fs;

use crate::{
    cores::{
        BiosRow, CoreInventory, CoreRow, CoreZip, InstallCoreError, InstallCoreOutcome,
        PutBiosError, RemoteCoreSource, RemoveCoreError, SUPPORTED,
    },
    cover::{Cover, CoverIndex, RemoteCoverSource},
    dat::{DatMirror, RemoteDatSource, SyncReport},
    device::{
        DeleteOrphanSaveError, DeleteSaveOutcome, Device, DeviceEntry, EjectDeviceError,
        OpenDeviceError, OrphanDeviceSave, RenameOnDeviceError, ScanReport,
    },
    error::BasicError,
    files::list_files_recursive,
    game::{Game, Save},
    library::{
        AddGameError, Library, LocalEntry, OpenLibraryError, OrphanLocalSave, RenameInLibraryError,
    },
    log::{FrontEnd, LogEntry, Logger, OperationReport},
    platform::Platform,
    sources::{libretro::Libretro, openfpga::OpenFpga},
    transfer::{
        self, PullGameOutcome, PullSaveError, PullSaveOutcome, PushGameError, PushGameOutcome,
        PushSaveError, PushSaveOutcome,
    },
};

// ============================================================================
// Batches
// ============================================================================

/// How a batch ended. `Break` means the callback stopped it, and an error means
/// the log couldn't be written.
pub type BatchResult = Result<ControlFlow<()>, BasicError>;

// ============================================================================
// What operations return
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrashSaveAllOutcome {
    Trashed,
    /// The library has no save of the game.
    NoSave,
}

/// Why a version of a save wasn't trashed.
#[derive(Debug, Clone, thiserror::Error)]
pub enum TrashSaveError {
    /// The save has no version written at that time.
    #[error("the library has no such save")]
    NoSuchVersion,
    /// Several versions of the save were written in the requested second.
    #[error("more than one version of the save was written at {at}")]
    AmbiguousVersion { at: String },
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
    Imported(Save),
    /// The library already has a version with the same contents.
    AlreadyHad,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportGameOutcome {
    /// The game as it is in the library now.
    Imported(LocalEntry),
    /// The library already has the game, under this name.
    AlreadyHad(LocalEntry),
}

#[derive(Debug, thiserror::Error)]
pub enum ImportGameError {
    /// No known platform was given and the file's extension doesn't indicate
    /// one.
    #[error("can't tell which platform it is for")]
    UnknownPlatform,
    #[error(transparent)]
    Add(#[from] AddGameError),
    #[error(transparent)]
    Other(#[from] BasicError),
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
        local: LocalEntry,
        on_device: DeviceEntry,
    },
}

impl GameRow {
    pub fn local_entry(&self) -> Option<&LocalEntry> {
        match self {
            GameRow::InLibrary(local) | GameRow::Both { local, .. } => Some(local),
            GameRow::OnDevice(_) => None,
        }
    }

    pub fn device_entry(&self) -> Option<&DeviceEntry> {
        match self {
            GameRow::OnDevice(on_device) | GameRow::Both { on_device, .. } => Some(on_device),
            GameRow::InLibrary(_) => None,
        }
    }

    pub fn game(&self) -> &Game {
        match self {
            GameRow::InLibrary(local) | GameRow::Both { local, .. } => &local.game,
            GameRow::OnDevice(on_device) => &on_device.game,
        }
    }

    /// How many versions of its save the library has.
    pub fn saves_kept(&self) -> usize {
        let history = self.local_entry().and_then(|local| local.save.as_ref());
        history.map_or(0, |history| history.iter().count())
    }

    /// Whether the device has a save the library doesn't.
    pub fn has_new_save_on_device(&self) -> bool {
        let history = self.local_entry().and_then(|local| local.save.as_ref());
        let on_device = self
            .device_entry()
            .and_then(|on_device| on_device.save.as_ref());
        on_device.is_some_and(|save| !history.is_some_and(|history| history.contains(save)))
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
    cores: CoreInventory<I>,
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
    /// This is the only library pocketeer creates.
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
        let cores = CoreInventory::open(&library.layout().core_inventory(), cores);
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
    /// With more than one mounted none is opened, because pocketeer shouldn't
    /// guess which one is meant.
    pub fn open_mounted_device(&mut self) -> Result<bool, OpenDeviceError> {
        match &Device::find_mounted()[..] {
            [] => Ok(false),
            [root] => self.open_device(root).map(|()| true),
            found => Err(OpenDeviceError::ManyDevices {
                found: found.to_vec(),
            }),
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
    pub fn eject_device(&mut self) -> Result<(), EjectDeviceError> {
        let root = self
            .device
            .as_ref()
            .map(|device| device.root().to_path_buf());
        let ejected = match &self.device {
            Some(device) => device.eject(),
            None => Err(BasicError::NoDevice.into()),
        };
        if ejected.is_ok() {
            self.device = None;
        }
        self.log_result(ejected, |ejected| OperationReport::Eject(root, ejected))
    }

    // ---- Reload -----------------------------------------------------------------

    /// Reads the library's folder again, and the open device's, for changes
    /// made outside pocketeer.
    pub fn reload(&mut self) -> Result<ScanReport, BasicError> {
        let read = self.reload_library().and_then(|()| match self.device {
            Some(_) => self.scan_device(),
            None => Ok(ScanReport::default()),
        });
        self.log_result(read, |read| OperationReport::Reload(read.map(|_| ())))
    }

    fn reload_library(&mut self) -> Result<(), BasicError> {
        self.library.reload()
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
            .map(
                |local| match device.and_then(|device| device.entry_for(&local.game)) {
                    Some(on_device) => GameRow::Both {
                        local: local.clone(),
                        on_device: on_device.clone(),
                    },
                    None => GameRow::InLibrary(local.clone()),
                },
            )
            .collect();
        rows.extend(
            self.entries_only_on_device()
                .cloned()
                .map(GameRow::OnDevice),
        );
        rows
    }

    fn entries_only_on_device(&self) -> impl Iterator<Item = &DeviceEntry> {
        let on_device = self
            .device
            .as_ref()
            .map_or(&[][..], |device| device.entries());
        on_device
            .iter()
            .filter(|on_device| self.library.entry_for(&on_device.game).is_none())
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

    pub(crate) fn games_on_device(&self) -> Vec<DeviceEntry> {
        let entries = self
            .device
            .as_ref()
            .map_or(&[][..], |device| device.entries());
        entries.to_vec()
    }

    // ---- DATs -------------------------------------------------------------------

    pub fn sync_dats(&mut self) -> Result<SyncReport, BasicError> {
        let synced = self.dats.sync();
        self.log_result(synced, OperationReport::SyncDats)
    }

    /// The name a game on the device gets in the library when pulled.
    pub fn name_for_pull(&self, on_device: &DeviceEntry) -> String {
        self.name_in_library(&on_device.game, &on_device.path)
    }

    /// The name in the library for `game` read from `file`, which is its DAT
    /// name with the file's extension, or the file's name if the DATs don't
    /// know it.
    fn name_in_library(&self, game: &Game, file: &Path) -> String {
        let file_name = file.file_name().unwrap_or_default();
        let Some(dat) = self.dats.lookup(game) else {
            return file_name.to_string_lossy().into_owned();
        };
        match file.extension() {
            Some(ext) => format!("{}.{}", dat.name, ext.to_string_lossy()),
            None => dat.name.clone(),
        }
    }

    /// The games among `games` whose name differs from their DAT name, each
    /// paired with that name. Games the DATs don't know are left out.
    pub fn renames_to_dat_names(&self, games: &[LocalEntry]) -> Vec<(LocalEntry, String)> {
        games
            .iter()
            .filter_map(|local| {
                let dat = self.dats.lookup(&local.game)?;
                (local.stem() != dat.name).then(|| (local.clone(), dat.name.clone()))
            })
            .collect()
    }

    // ---- Games ------------------------------------------------------------------

    pub fn pull_games(
        &mut self,
        games: &[DeviceEntry],
        on_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            games,
            |pocketeer, game| OperationReport::PullGame(game.clone(), pocketeer.pull_game(game)),
            on_logged,
        )
    }

    /// Copies `on_device` into the library, under its DAT name if known and its
    /// file name otherwise.
    fn pull_game(&mut self, on_device: &DeviceEntry) -> Result<PullGameOutcome, AddGameError> {
        let on_device = self.current_device_entry(on_device)?;
        let name = self.name_for_pull(&on_device);
        let device = self.device.as_ref().ok_or(BasicError::NoDevice)?;
        transfer::pull_game(&mut self.library, device, &on_device, &name)
    }

    pub fn push_games(
        &mut self,
        games: &[LocalEntry],
        on_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            games,
            |pocketeer, game| OperationReport::PushGame(game.clone(), pocketeer.push_game(game)),
            on_logged,
        )
    }

    fn push_game(&mut self, local: &LocalEntry) -> Result<PushGameOutcome, PushGameError> {
        let local = self.current_local_entry(local)?;
        let device = self.device.as_mut().ok_or(BasicError::NoDevice)?;
        transfer::push_game(&self.library, device, &local)
    }

    /// Imports each file as a game, for `platform` or otherwise the platform
    /// its extension indicates.
    pub fn import_games(
        &mut self,
        files: &[PathBuf],
        platform: Option<&Platform>,
        on_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            files,
            |pocketeer, from| {
                let result = pocketeer.import_game(from, platform);
                // The log outlives the folder the command was run in.
                let from = std::path::absolute(from).unwrap_or_else(|_| from.clone());
                OperationReport::ImportGame(from, result)
            },
            on_logged,
        )
    }

    /// Copies the file at `from` into the library, unless it already has the
    /// game. The platform is `platform` or otherwise what the extension
    /// indicates. The name is the DAT name if known and the file name
    /// otherwise. The file stays where it is.
    fn import_game(
        &mut self,
        from: &Path,
        platform: Option<&Platform>,
    ) -> Result<ImportGameOutcome, ImportGameError> {
        let by_extension = || {
            let extension = from.extension()?.to_str()?;
            Platform::from_extension(extension)
        };
        // Only a named platform counts, because an id can be anything and it
        // becomes part of a path.
        let named = platform.filter(|platform| !matches!(platform, Platform::Other(_)));
        let platform = named.cloned().or_else(by_extension);
        let platform = platform.ok_or(ImportGameError::UnknownPlatform)?;
        let game = Game::read(platform, from).map_err(BasicError::from)?;
        if let Some(entry) = self.library.entry_for(&game) {
            return Ok(ImportGameOutcome::AlreadyHad(entry.clone()));
        }
        let name = self.name_in_library(&game, from);
        let added = self.library.add_game(&game, &name, from)?;
        Ok(ImportGameOutcome::Imported(added))
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
            |pocketeer, (local, new)| {
                OperationReport::RenameInLibrary(
                    local.clone(),
                    pocketeer.rename_game_in_library(local, new),
                )
            },
            on_logged,
        )
    }

    fn rename_game_in_library(
        &mut self,
        local: &LocalEntry,
        new: &str,
    ) -> Result<LocalEntry, RenameInLibraryError> {
        let local = self.current_local_entry(local)?;
        self.library
            .rename_game(&local.game.platform, &local.name, new)
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
            |pocketeer, (on_device, new)| {
                OperationReport::RenameOnDevice(
                    on_device.clone(),
                    pocketeer.rename_game_on_device(on_device, new),
                )
            },
            on_logged,
        );
        self.rescan_device_after(batch_result)
    }

    fn rename_game_on_device(
        &mut self,
        on_device: &DeviceEntry,
        new: &str,
    ) -> Result<DeviceEntry, RenameOnDeviceError> {
        let on_device = self.current_device_entry(on_device)?;
        let device = self.device.as_mut().ok_or(BasicError::NoDevice)?;
        device.rename_game(&on_device, new)
    }

    pub fn fetch_covers(
        &mut self,
        games: &[LocalEntry],
        on_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            games,
            |pocketeer, game| {
                OperationReport::FetchCover(game.clone(), pocketeer.fetch_cover(game))
            },
            on_logged,
        )
    }

    /// Downloads a cover for `local` if it has none. It looks under the DAT
    /// name if known and otherwise under the file name without its extension.
    fn fetch_cover(&mut self, local: &LocalEntry) -> Result<FetchCoverOutcome, BasicError> {
        let local = self.current_local_entry(local)?;
        if local.cover_path.is_some() {
            return Ok(FetchCoverOutcome::AlreadyHad);
        }
        let name = match self.dats.lookup(&local.game) {
            Some(dat) => dat.name.clone(),
            None => local.stem().to_string(),
        };
        let platform = &local.game.platform;
        let Some(cover) = self.covers.fetch(platform, &name)? else {
            return Ok(FetchCoverOutcome::NoCover);
        };
        self.library.add_cover(platform, &local.name, &cover)?;
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
            |pocketeer, (local, from)| {
                let result = pocketeer.import_cover(local, from);
                // The log outlives the folder the command was run in.
                let from = std::path::absolute(from).unwrap_or_else(|_| from.clone());
                OperationReport::ImportCover(local.clone(), from, result)
            },
            on_logged,
        )
    }

    /// A cover of another format is trashed after the new one is written, so
    /// the game keeps a cover if writing fails. One of the same format has the
    /// same file name, so it has to go first.
    fn import_cover(&mut self, local: &LocalEntry, from: &Path) -> Result<(), ImportCoverError> {
        let local = self.current_local_entry(local)?;
        let bytes = fs::read(from).map_err(BasicError::from)?;
        let cover = Cover::from_bytes(bytes).ok_or(ImportCoverError::NotAnImage)?;
        let platform = &local.game.platform;
        let new_path = self
            .library
            .layout()
            .cover(platform, local.stem(), cover.format());
        let old_path = local.cover_path.as_ref();
        if old_path == Some(&new_path) {
            trash::delete(&new_path).map_err(BasicError::from)?;
        }
        self.library.add_cover(platform, &local.name, &cover)?;
        if let Some(old_path) = old_path.filter(|path| **path != new_path) {
            trash::delete(old_path).map_err(BasicError::from)?;
        }
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
                OperationReport::TrashGame(game.clone(), pocketeer.trash_game_in_library(game))
            },
            on_logged,
        )
    }

    /// Moves `local`'s file and cover to the system's trash, where they can be
    /// restored. Its saves stay, in case a game is added under that name again.
    fn trash_game_in_library(&mut self, local: &LocalEntry) -> Result<(), BasicError> {
        let local = self.current_local_entry(local)?;
        let files = self.library.files_of(&local);
        trash::delete_all(&files)?;
        self.library.forget_game(&local.game.platform, &local.name);
        Ok(())
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
                OperationReport::DeleteGame(game.clone(), pocketeer.delete_game_on_device(game))
            },
            on_logged,
        );
        self.rescan_device_after(batch_result)
    }

    /// Deletes the file of `on_device` from the device for good. Its save
    /// stays there.
    fn delete_game_on_device(&mut self, on_device: &DeviceEntry) -> Result<(), BasicError> {
        let on_device = self.current_device_entry(on_device)?;
        let device = self.device.as_mut().ok_or(BasicError::NoDevice)?;
        device.remove_game(&on_device)
    }

    // ---- Saves ------------------------------------------------------------------

    pub fn pull_saves(
        &mut self,
        games: &[DeviceEntry],
        on_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            games,
            |pocketeer, game| OperationReport::PullSave(game.clone(), pocketeer.pull_save(game)),
            on_logged,
        )
    }

    fn pull_save(&mut self, on_device: &DeviceEntry) -> Result<PullSaveOutcome, PullSaveError> {
        let on_device = self.current_device_entry(on_device)?;
        let device = self.device.as_ref().ok_or(BasicError::NoDevice)?;
        transfer::pull_save(&mut self.library, device, &on_device)
    }

    /// Pushes the newest version of each game's save.
    pub fn push_saves(
        &mut self,
        games: &[DeviceEntry],
        on_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            games,
            |pocketeer, game| {
                OperationReport::PushSave(game.clone(), pocketeer.push_newest_save(game))
            },
            on_logged,
        )
    }

    fn push_newest_save(&mut self, game: &DeviceEntry) -> Result<PushSaveOutcome, PushSaveError> {
        let version = self.newest_save(&game.game)?;
        self.push_save(game, &version)
    }

    fn newest_save(&self, game: &Game) -> Result<Save, PushSaveError> {
        let entry = self.library.entry_for(game);
        let entry = entry.ok_or(PushSaveError::NotInLibrary)?;
        let history = entry.save.as_ref().ok_or(PushSaveError::NoSuchVersion)?;
        Ok(history.current.clone())
    }

    /// Pushes the version of each game's save written at `at`, in the format of
    /// [`Save::timestamp`].
    pub fn push_saves_at(
        &mut self,
        games: &[DeviceEntry],
        at: &str,
        on_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            games,
            |pocketeer, game| {
                OperationReport::PushSave(game.clone(), pocketeer.push_save_written_at(game, at))
            },
            on_logged,
        )
    }

    fn push_save_written_at(
        &mut self,
        game: &DeviceEntry,
        at: &str,
    ) -> Result<PushSaveOutcome, PushSaveError> {
        let local = self.library.entry_for(&game.game);
        let local = local.ok_or(PushSaveError::NotInLibrary)?;
        match &saves_written_at(local, at)[..] {
            [] => Err(PushSaveError::NoSuchVersion),
            [version] => self.push_save(game, version),
            _ => Err(PushSaveError::AmbiguousVersion { at: at.to_string() }),
        }
    }

    /// Pushes each version to the game paired with it.
    pub fn push_save_versions(
        &mut self,
        versions: &[(DeviceEntry, Save)],
        on_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            versions,
            |pocketeer, (game, version)| {
                OperationReport::PushSave(game.clone(), pocketeer.push_save(game, version))
            },
            on_logged,
        )
    }

    fn push_save(
        &mut self,
        on_device: &DeviceEntry,
        version: &Save,
    ) -> Result<PushSaveOutcome, PushSaveError> {
        let on_device = self.current_device_entry(on_device)?;
        let device = self.device.as_mut().ok_or(BasicError::NoDevice)?;
        transfer::push_save(&self.library, device, &on_device, version)
    }

    /// Imports each file as a version of the save of the game paired with it.
    pub fn import_saves(
        &mut self,
        saves: &[(LocalEntry, PathBuf)],
        on_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            saves,
            |pocketeer, (local, from)| {
                let result = pocketeer.import_save(local, from);
                // The log outlives the folder the command was run in.
                let from = std::path::absolute(from).unwrap_or_else(|_| from.clone());
                OperationReport::ImportSave(local.clone(), from, result)
            },
            on_logged,
        )
    }

    /// Keeps the file at `from` as a new version of `local`'s save, dated when
    /// the file was written. The file stays where it is.
    fn import_save(
        &mut self,
        local: &LocalEntry,
        from: &Path,
    ) -> Result<ImportSaveOutcome, BasicError> {
        let local = self.current_local_entry(local)?;
        let added = self
            .library
            .add_save(&local.game.platform, &local.name, from)?;
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
                OperationReport::TrashSaveAll(
                    game.clone(),
                    pocketeer.trash_every_version_in_library(game),
                )
            },
            on_logged,
        )
    }

    fn trash_every_version_in_library(
        &mut self,
        local: &LocalEntry,
    ) -> Result<TrashSaveAllOutcome, BasicError> {
        let local = self.current_local_entry(local)?;
        let files = self.library.files_of_all_saves(&local)?;
        if files.is_empty() {
            return Ok(TrashSaveAllOutcome::NoSave);
        }
        self.trash_save_files(&local, &files)?;
        Ok(TrashSaveAllOutcome::Trashed)
    }

    /// Trashes the version of each game's save written at `at`, in the format
    /// of [`Save::timestamp`].
    pub fn trash_saves_at(
        &mut self,
        games: &[LocalEntry],
        at: &str,
        on_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            games,
            |pocketeer, game| {
                let result = pocketeer.trash_save_written_at(game, at);
                OperationReport::TrashSaveVersion(game.clone(), at.to_string(), result)
            },
            on_logged,
        )
    }

    fn trash_save_written_at(&mut self, game: &LocalEntry, at: &str) -> Result<(), TrashSaveError> {
        let local = self.current_local_entry(game)?;
        match &saves_written_at(&local, at)[..] {
            [] => Err(TrashSaveError::NoSuchVersion),
            [version] => self.trash_save_version(&local, version),
            _ => Err(TrashSaveError::AmbiguousVersion { at: at.to_string() }),
        }
    }

    /// Trashes each version of the save of the game paired with it.
    pub fn trash_save_versions_in_library(
        &mut self,
        versions: &[(LocalEntry, Save)],
        on_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            versions,
            |pocketeer, (game, version)| {
                let result = pocketeer.trash_save_version(game, version);
                OperationReport::TrashSaveVersion(game.clone(), version.timestamp(), result)
            },
            on_logged,
        )
    }

    fn trash_save_version(
        &mut self,
        local: &LocalEntry,
        version: &Save,
    ) -> Result<(), TrashSaveError> {
        let local = self.current_local_entry(local)?;
        let Some(file) = self.library.file_of_save(&local, version)? else {
            return Err(TrashSaveError::NoSuchVersion);
        };
        Ok(self.trash_save_files(&local, &[file])?)
    }

    fn trash_save_files(
        &mut self,
        local: &LocalEntry,
        files: &[PathBuf],
    ) -> Result<(), BasicError> {
        trash::delete_all(files)?;
        self.library.reread_saves(&local.game.platform, &local.name)
    }

    pub fn delete_saves_on_device(
        &mut self,
        games: &[DeviceEntry],
        on_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            games,
            |pocketeer, game| {
                OperationReport::DeleteSave(game.clone(), pocketeer.delete_save_on_device(game))
            },
            on_logged,
        )
    }

    /// Deletes the save of `on_device` from the device for good, even if the
    /// library doesn't have it.
    fn delete_save_on_device(
        &mut self,
        on_device: &DeviceEntry,
    ) -> Result<DeleteSaveOutcome, BasicError> {
        let on_device = self.current_device_entry(on_device)?;
        let device = self.device.as_mut().ok_or(BasicError::NoDevice)?;
        device.remove_save(&on_device)
    }

    // ---- Orphan saves -----------------------------------------------------------

    pub fn find_orphan_saves_in_library(&self) -> Result<Vec<OrphanLocalSave>, BasicError> {
        self.library.find_orphan_saves()
    }

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
                OperationReport::TrashOrphanSave(
                    orphan.clone(),
                    pocketeer.trash_orphan_save_in_library(orphan),
                )
            },
            on_logged,
        )
    }

    fn trash_orphan_save_in_library(
        &mut self,
        orphan: &OrphanLocalSave,
    ) -> Result<(), TrashOrphanSaveError> {
        let files = self.library.files_of_orphan_save(orphan)?;
        if files.is_empty() {
            return Err(TrashOrphanSaveError::NoOrphanSave);
        }
        trash::delete_all(&files).map_err(BasicError::from)?;
        Ok(())
    }

    pub fn delete_orphan_saves_on_device(
        &mut self,
        orphans: &[OrphanDeviceSave],
        on_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            orphans,
            |pocketeer, orphan| {
                OperationReport::DeleteOrphanSave(
                    orphan.clone(),
                    pocketeer.delete_orphan_save_on_device(orphan),
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
    /// on the device. If the inventory can't be fetched, the error comes back
    /// with the cores on the device alone.
    pub fn load_core_rows(&mut self) -> Result<(Vec<CoreRow>, Option<BasicError>), BasicError> {
        let installed = match &self.device {
            Some(device) => device.installed_cores()?,
            None => Vec::new(),
        };
        let (latest, failure) = match self.cores.supported_cores() {
            Ok(latest) => (latest, None),
            Err(error) => (Vec::new(), Some(error)),
        };
        Ok((CoreRow::pair(installed, latest), failure))
    }

    /// The cores on the device with another version in the inventory.
    pub fn find_cores_with_updates(&mut self) -> Result<Vec<String>, BasicError> {
        let (rows, failure) = self.load_core_rows()?;
        if let Some(failure) = failure {
            return Err(failure);
        }
        let with_updates = rows.iter().filter(|row| row.has_update());
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
            |pocketeer, id| OperationReport::InstallCore(id.clone(), pocketeer.install_core(id)),
            on_logged,
        );
        self.rescan_device_after(batch_result)
    }

    /// Installs the newest release of the core, unless it is already there.
    fn install_core(&mut self, id: &str) -> Result<InstallCoreOutcome, InstallCoreError> {
        if !SUPPORTED.contains(&id) {
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
        let on_device = installed.iter().find(|core| core.id == id);
        if on_device.is_some_and(|core| core.is_at_version(&latest.version)) {
            return Ok(InstallCoreOutcome::AlreadyCurrent);
        }
        let mut zip = CoreZip::parse(self.cores.download(&latest.download_url)?, id)?;
        device.install_core(&mut zip)?;
        Ok(InstallCoreOutcome::Installed {
            version: zip.version().to_string(),
        })
    }

    /// Deletes each core from the device for good.
    pub fn remove_cores(
        &mut self,
        ids: &[String],
        on_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        let batch_result = self.for_each_item(
            ids,
            |pocketeer, id| OperationReport::RemoveCore(id.clone(), pocketeer.remove_core(id)),
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
                OperationReport::PutBios(row.clone(), from, result)
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
    // operations go through `log_result`, and batches through `for_each_item`.

    /// Logs `result` and returns it. If it succeeded but can't be logged, that
    /// is the error.
    fn log_result<T: Clone, E: Clone + From<BasicError>>(
        &self,
        result: Result<T, E>,
        to_report: impl FnOnce(Result<T, E>) -> OperationReport,
    ) -> Result<T, E> {
        let (_, log_write_result) = self.logger.record(to_report(result.clone()));
        let value = result?;
        log_write_result?;
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
        op: impl Fn(&mut Self, &Item) -> OperationReport,
        mut on_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        for item in items {
            let report = op(self, item);
            let (entry, log_write_result) = self.logger.record(report);
            let control_flow = on_logged(self, entry);
            log_write_result?;
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
        scan_result?;
        Ok(control_flow)
    }

    // Operations look up the entries they are given again, because they may
    // have changed or gone since the caller saw them.

    /// `entry` as the device has it now.
    fn current_device_entry(&self, entry: &DeviceEntry) -> Result<DeviceEntry, BasicError> {
        let device = self.device.as_ref().ok_or(BasicError::NoDevice)?;
        let current = device.entry_at(&entry.game.platform, &entry.path);
        current.cloned().ok_or(BasicError::NotOnDevice)
    }

    /// `entry` as the library has it now.
    fn current_local_entry(&self, entry: &LocalEntry) -> Result<LocalEntry, BasicError> {
        let current = self.library.entry_named(&entry.game.platform, &entry.name);
        current.cloned().ok_or(BasicError::NotInLibrary)
    }
}

// ============================================================================
// Helpers
// ============================================================================

/// `paths` with each folder replaced by every file under it whose extension
/// indicates a platform. Files stay as they are.
pub fn expand_import_paths(paths: &[PathBuf]) -> Result<Vec<PathBuf>, BasicError> {
    let tells_platform = |file: &PathBuf| {
        let extension = file.extension().and_then(|extension| extension.to_str());
        extension.and_then(Platform::from_extension).is_some()
    };
    let mut files = Vec::new();
    for path in paths {
        if path.is_dir() {
            let under = list_files_recursive(path)?;
            let games = under.into_iter().filter(tells_platform);
            files.extend(games.map(|file| path.join(file)));
        } else {
            files.push(path.clone());
        }
    }
    Ok(files)
}

fn saves_written_at(local: &LocalEntry, at: &str) -> Vec<Save> {
    let written = local.save.iter().flat_map(|history| history.written_at(at));
    written.cloned().collect()
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, fs, rc::Rc};

    use super::*;
    use crate::cores::{
        InventoryCore,
        testing::{core_json, data_json, zip_of},
    };
    use crate::cover::{Cover, testing::PNG};
    use crate::files::testing::{put, put_at};

    /// A DAT that lists the GBA game with the contents `hello`.
    const GBA_DAT: &str = r#"clrmamepro ( name "Nintendo - Game Boy Advance" ) game ( name "Tetris (World)" rom ( name "Tetris (World).gba" size 5 crc 3610A686 ) )"#;

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
            let mut png = Vec::new();
            image::RgbaImage::new(1, 1)
                .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
                .unwrap();
            Ok(Cover::from_bytes(png).unwrap())
        }
    }

    /// A source of cores with `Spiritualized.GB` at 2.0. It counts its
    /// downloads in a counter shared with the test.
    #[derive(Clone, Default)]
    struct CoreSource {
        downloaded: Rc<Cell<u32>>,
    }

    impl RemoteCoreSource for CoreSource {
        fn inventory(&self) -> Result<Vec<InventoryCore>, BasicError> {
            Ok(vec![InventoryCore {
                id: "Spiritualized.GB".into(),
                version: "2.0".into(),
                platforms: vec![Platform::Gb],
                download_url: "core.zip".into(),
            }])
        }

        fn download(&self, url: &str) -> Result<Vec<u8>, BasicError> {
            self.downloaded.set(self.downloaded.get() + 1);
            match url {
                "core.zip" => Ok(zip_of(&[
                    (
                        "Cores/Spiritualized.GB/core.json",
                        core_json("2.0").as_bytes(),
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
        put(root, "device/Analogue_Pocket.json", b"{}");
        fs::create_dir_all(root.join("device/Cores")).unwrap();
        fs::create_dir_all(root.join("device/Assets")).unwrap();
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

    fn names(pocketeer: &Tested) -> Vec<String> {
        pocketeer
            .library()
            .entries()
            .iter()
            .map(|entry| entry.name.clone())
            .collect()
    }

    /// One game in the library, and a folder to import from.
    fn to_import_into() -> (Setup, tempfile::TempDir, LocalEntry) {
        let setup = setup(&[], &[("roms/gba/A.gba", b"a")]);
        let local = setup.first_local();
        (setup, tempfile::tempdir().unwrap(), local)
    }

    /// One game with a save written at 1,000 seconds and one without, on the
    /// device and in the library.
    fn with_versions() -> (Setup, Vec<DeviceEntry>, Vec<LocalEntry>) {
        let mut setup = setup(
            &[
                ("Assets/gba/common/A.gba", b"a"),
                ("Assets/gba/common/B.gba", b"b"),
            ],
            &[("roms/gba/A.gba", b"a"), ("roms/gba/B.gba", b"b")],
        );
        let root = setup.pocketeer.library().root().to_path_buf();
        put_at(&root, "saves/gba/A/one.sav", b"one", 1_000);
        setup.pocketeer.reload_library().unwrap();
        let on_device = setup.pocketeer.games_on_device();
        let local = setup.pocketeer.library().entries().to_vec();
        (setup, on_device, local)
    }

    /// The library's log, one value for each line.
    fn log_lines(setup: &Setup) -> Vec<serde_json::Value> {
        let log = setup.pocketeer.library().layout().log();
        fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
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

    mod rows {
        use super::*;

        #[test]
        fn pairs_library_with_device_by_contents() {
            let setup = setup(
                &[
                    ("Assets/gba/common/Tetris.gba", b"hello"),
                    ("Assets/gba/common/Only Here.gba", b"device"),
                ],
                &[
                    ("roms/gba/Tetris (World).gba", b"hello"),
                    ("roms/gba/Only There.gba", b"library"),
                ],
            );
            let rows: Vec<_> = setup
                .pocketeer
                .game_rows()
                .iter()
                .map(|row| {
                    let local = row.local_entry().map(|local| local.name.clone());
                    let on_device = row
                        .device_entry()
                        .map(|on_device| on_device.path.display().to_string());
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
        }

        #[test]
        fn reports_saves_on_each_side() {
            let mut setup = setup(
                &[
                    ("Assets/gba/common/Tetris.gba", b"hello"),
                    ("Saves/gba/common/Tetris.sav", b"save"),
                ],
                &[("roms/gba/Tetris (World).gba", b"hello")],
            );
            let row = setup.pocketeer.game_rows().remove(0);
            assert_eq!(row.saves_kept(), 0);
            assert!(row.has_new_save_on_device());
            let on_device = setup.first_on_device();
            setup.pocketeer.pull_save(&on_device).unwrap();
            let row = setup.pocketeer.game_rows().remove(0);
            assert_eq!(row.saves_kept(), 1);
            assert!(!row.has_new_save_on_device());
        }

        #[test]
        fn lists_only_device_games_in_library() {
            let setup = setup(
                &[
                    ("Assets/gba/common/Both.gba", b"both"),
                    ("Assets/gba/common/Here.gba", b"here"),
                ],
                &[("roms/gba/Mine.gba", b"both")],
            );
            let games = setup.pocketeer.games_on_device_in_library();
            assert_eq!(games.len(), 1);
            assert_eq!(games[0].path, Path::new("Both.gba"));
        }
    }

    mod pull_game_names {
        use super::*;

        #[test]
        fn uses_dat_name_for_known_game() {
            let mut setup = setup(&[("Assets/gba/common/tetris.gba", b"hello")], &[]);
            let on_device = setup.first_on_device();
            let pulled = setup.pocketeer.pull_game(&on_device).unwrap();
            assert!(matches!(pulled, PullGameOutcome::Pulled(_)));
            assert_eq!(names(&setup.pocketeer), ["Tetris (World).gba"]);
        }

        #[test]
        fn keeps_file_name_for_unknown_game() {
            let mut setup = setup(&[("Assets/gba/common/RPG/My Hack.gba", b"hack")], &[]);
            let on_device = setup.first_on_device();
            setup.pocketeer.pull_game(&on_device).unwrap();
            assert_eq!(names(&setup.pocketeer), ["My Hack.gba"]);
        }
    }

    mod missing_targets {
        use super::*;

        #[test]
        fn returns_error_without_device() {
            let mut setup = setup(&[], &[]);
            setup.pocketeer.close_device();
            assert!(matches!(
                setup.pocketeer.read_bios_rows(),
                Err(BasicError::NoDevice)
            ));
        }

        #[test]
        fn returns_no_device_for_transfers() {
            let mut setup = setup(
                &[("Assets/gba/common/Tetris.gba", b"hello")],
                &[("roms/gba/Tetris (World).gba", b"hello")],
            );
            let on_device = setup.first_on_device();
            let local = setup.first_local();
            setup.pocketeer.close_device();
            assert!(matches!(
                setup.pocketeer.push_game(&local),
                Err(PushGameError::Other(BasicError::NoDevice))
            ));
            assert!(matches!(
                setup.pocketeer.pull_game(&on_device),
                Err(AddGameError::Other(BasicError::NoDevice))
            ));
        }

        #[test]
        fn returns_error_for_entry_no_longer_present() {
            let mut setup = setup(
                &[("Assets/gba/common/Tetris.gba", b"hello")],
                &[("roms/gba/Tetris (World).gba", b"hello")],
            );
            let on_device = setup.first_on_device();
            let local = setup.first_local();
            let device = setup.pocketeer.device().unwrap().root().to_path_buf();
            fs::remove_file(device.join("Assets/gba/common/Tetris.gba")).unwrap();
            setup.pocketeer.scan_device().unwrap();
            assert!(matches!(
                setup.pocketeer.pull_save(&on_device),
                Err(PullSaveError::Other(BasicError::NotOnDevice))
            ));
            fs::remove_file(setup.pocketeer.library().game_path(&local)).unwrap();
            setup.pocketeer.reload_library().unwrap();
            assert!(matches!(
                setup.pocketeer.fetch_cover(&local),
                Err(BasicError::NotInLibrary)
            ));
        }

        #[test]
        fn reports_no_save_and_rejects_missing_version() {
            let mut setup = setup(
                &[("Assets/gba/common/Tetris.gba", b"hello")],
                &[("roms/gba/Tetris (World).gba", b"hello")],
            );
            let on_device = setup.first_on_device();
            let local = setup.first_local();
            assert!(matches!(
                setup.pocketeer.trash_every_version_in_library(&local),
                Ok(TrashSaveAllOutcome::NoSave)
            ));
            let gone = Save::from_bytes(b"gone", std::time::UNIX_EPOCH);
            assert!(matches!(
                setup.pocketeer.trash_save_version(&local, &gone),
                Err(TrashSaveError::NoSuchVersion)
            ));
            let lines = lines_of(|on_logged| setup.pocketeer.push_saves(&[on_device], on_logged));
            assert_eq!(lines, ["gba/Tetris.gba: the library has no such save"]);
        }
    }

    mod batches {
        use super::*;

        #[test]
        fn continues_after_failed_game() {
            let mut setup = setup(&[], &[("roms/gba/Tetris (World).gba", b"hello")]);
            let local = setup.first_local();
            let unknown = LocalEntry {
                name: "Gone.gba".into(),
                ..local.clone()
            };
            let mut seen = Vec::new();
            let _ = setup
                .pocketeer
                .push_games(&[unknown, local], |_, entry| {
                    seen.push((entry.report.subject(), entry.report.is_ok()));
                    ControlFlow::Continue(())
                })
                .unwrap();
            assert_eq!(
                seen,
                [
                    ("gba/Gone.gba".to_string(), false),
                    ("gba/Tetris (World).gba".to_string(), true)
                ]
            );
        }

        #[test]
        fn stops_before_next_game_on_break() {
            let mut setup = setup(&[], &[("roms/gba/A.gba", b"a"), ("roms/gba/B.gba", b"b")]);
            let games = setup.pocketeer.library().entries().to_vec();
            let mut seen = Vec::new();
            let control_flow = setup.pocketeer.push_games(&games, |_, entry| {
                seen.push(entry.report.subject());
                ControlFlow::Break(())
            });
            assert!(control_flow.unwrap().is_break());
            assert_eq!(seen, ["gba/A.gba"]);
            assert_eq!(setup.pocketeer.games_on_device().len(), 1);
            // What was finished is in the log, and nothing else.
            assert_eq!(log_lines(&setup).len(), 1);
        }
    }

    mod covers {
        use super::*;

        #[test]
        fn fetches_cover_by_dat_name() {
            let mut setup = setup(&[], &[("roms/gba/tetris.gba", b"hello")]);
            let local = setup.first_local();
            let fetched = setup.pocketeer.fetch_cover(&local).unwrap();
            assert_eq!(fetched, FetchCoverOutcome::Fetched);
            let entry = &setup.pocketeer.library().entries()[0];
            assert!(
                entry
                    .cover_path
                    .as_ref()
                    .unwrap()
                    .ends_with("covers/gba/tetris.png")
            );
            let again = setup.pocketeer.fetch_cover(&local).unwrap();
            assert_eq!(again, FetchCoverOutcome::AlreadyHad);
        }

        #[test]
        fn fetches_cover_by_file_name_when_dats_do_not_know_game() {
            let mut setup = setup(
                &[],
                &[
                    ("roms/gba/My Hack.gba", b"hack"),
                    ("roms/gba/Other.gba", b"other"),
                ],
            );
            let games = setup.pocketeer.games_without_covers();
            let lines = lines_of(|on_logged| setup.pocketeer.fetch_covers(&games, on_logged));
            assert_eq!(
                lines,
                [
                    "gba/My Hack.gba: cover downloaded",
                    "gba/Other.gba: no cover found for it"
                ]
            );
            assert_eq!(setup.pocketeer.games_without_covers().len(), 1);
        }
    }

    mod versions {
        use super::*;

        #[test]
        fn pushes_given_version() {
            let (mut setup, on_device, local) = with_versions();
            let version = local[0].save.as_ref().unwrap().current.clone();
            let versions = [(on_device[0].clone(), version)];
            let lines =
                lines_of(|on_logged| setup.pocketeer.push_save_versions(&versions, on_logged));
            assert_eq!(lines, ["gba/A.gba: save 1970-01-01T00-16-40 pushed"]);
        }

        #[test]
        fn pushes_newest_by_default() {
            let (mut setup, on_device, _) = with_versions();
            let root = setup.pocketeer.library().root().to_path_buf();
            put_at(&root, "saves/gba/A/two.sav", b"two", 2_000);
            setup.pocketeer.reload_library().unwrap();
            let lines =
                lines_of(|on_logged| setup.pocketeer.push_saves(&on_device[..1], on_logged));
            assert_eq!(lines, ["gba/A.gba: save 1970-01-01T00-33-20 pushed"]);
        }

        #[test]
        fn pushes_version_written_at_given_time() {
            let (mut setup, on_device, _) = with_versions();
            let at = "1970-01-01T00-16-40";
            let lines = lines_of(|on_logged| {
                setup
                    .pocketeer
                    .push_saves_at(&on_device[..1], at, on_logged)
            });
            assert_eq!(lines, ["gba/A.gba: save 1970-01-01T00-16-40 pushed"]);
        }

        #[test]
        fn logs_missing_version_when_trashing() {
            let (mut setup, _, local) = with_versions();
            let lines =
                lines_of(|on_logged| setup.pocketeer.trash_saves_at(&local, "never", on_logged));
            assert_eq!(
                lines,
                [
                    "gba/A.gba: the library has no such save",
                    "gba/B.gba: the library has no such save"
                ]
            );
            let logged = log_lines(&setup);
            assert_eq!(logged[0]["version"], "never");
            assert_eq!(logged[0]["ok"], false);
        }

        #[test]
        fn acts_on_neither_when_two_versions_share_time() {
            let (mut setup, on_device, local) = with_versions();
            let root = setup.pocketeer.library().root().to_path_buf();
            let file = put_at(&root, "saves/gba/A/two.sav", b"two", 1_000);
            let first = root.join("saves/gba/A/one.sav");
            setup.pocketeer.reload_library().unwrap();
            let at = "1970-01-01T00-16-40";
            let refused = [
                "gba/A.gba: error: more than one version of the save was written \
                            at 1970-01-01T00-16-40",
            ];
            let lines = lines_of(|on_logged| {
                setup
                    .pocketeer
                    .push_saves_at(&on_device[..1], at, on_logged)
            });
            assert_eq!(lines, refused);
            let lines =
                lines_of(|on_logged| setup.pocketeer.trash_saves_at(&local[..1], at, on_logged));
            assert_eq!(lines, refused);
            assert!(file.exists() && first.exists());
        }
    }

    mod import_covers {
        use super::*;

        #[test]
        fn imports_cover_for_game_without_one() {
            let (mut setup, elsewhere, local) = to_import_into();
            let from = put(elsewhere.path(), "mine.dat", PNG);
            let covers = [(local, from.clone())];
            let lines = lines_of(|on_logged| setup.pocketeer.import_covers(&covers, on_logged));
            assert_eq!(lines, ["gba/A.gba: cover imported"]);
            let root = setup.pocketeer.library().root();
            let cover = root.join("covers/gba/A.png");
            assert_eq!(fs::read(&cover).unwrap(), PNG);
            let entry = &setup.pocketeer.library().entries()[0];
            assert_eq!(entry.cover_path, Some(cover));
            assert!(from.exists());
            assert_eq!(log_lines(&setup)[0]["from"], from.display().to_string());
        }

        #[test]
        fn rejects_file_that_is_not_image() {
            let (mut setup, elsewhere, local) = to_import_into();
            let covers = [(local, put(elsewhere.path(), "mine.png", b"not an image"))];
            let lines = lines_of(|on_logged| setup.pocketeer.import_covers(&covers, on_logged));
            assert_eq!(lines, ["gba/A.gba: not a PNG, JPEG or WebP file"]);
            assert!(!setup.pocketeer.library().root().join("covers").exists());
        }

        #[test]
        fn rejects_non_image_and_keeps_existing_cover() {
            let mut setup = setup(&[], &[("roms/gba/A.gba", b"a"), ("covers/gba/A.png", PNG)]);
            let local = setup.first_local();
            let elsewhere = tempfile::tempdir().unwrap();
            let covers = [(local, put(elsewhere.path(), "mine.png", b"not an image"))];
            lines_of(|on_logged| setup.pocketeer.import_covers(&covers, on_logged));
            let cover = setup.pocketeer.library().root().join("covers/gba/A.png");
            assert_eq!(fs::read(cover).unwrap(), PNG);
        }
    }

    mod import_saves {
        use super::*;

        #[test]
        fn keeps_version_named_after_write_time_and_leaves_source() {
            let (mut setup, elsewhere, local) = to_import_into();
            let from = put_at(elsewhere.path(), "Mine.sav", b"kept", 1_000);
            let saves = [(local, from.clone())];
            let lines = lines_of(|on_logged| setup.pocketeer.import_saves(&saves, on_logged));
            assert_eq!(lines, ["gba/A.gba: save 1970-01-01T00-16-40 imported"]);
            let root = setup.pocketeer.library().root();
            let kept = root.join("saves/gba/A/1970-01-01T00-16-40.sav");
            assert_eq!(fs::read(kept).unwrap(), b"kept");
            assert_eq!(fs::read(from).unwrap(), b"kept");
        }
    }

    mod import_games {
        use super::*;

        /// The file `name` holding `bytes`, in a folder apart from the library.
        fn apart(name: &str, bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
            let dir = tempfile::tempdir().unwrap();
            let file = put(dir.path(), name, bytes);
            (dir, file)
        }

        #[test]
        fn uses_dat_name_and_leaves_source_file() {
            let mut setup = setup(&[], &[]);
            let (_dir, from) = apart("tetris.gba", b"hello");
            let files = [from.clone()];
            let lines = lines_of(|on_logged| setup.pocketeer.import_games(&files, None, on_logged));
            assert_eq!(lines, ["tetris.gba: imported as gba/Tetris (World).gba"]);
            assert_eq!(names(&setup.pocketeer), ["Tetris (World).gba"]);
            assert!(from.exists());
        }

        #[test]
        fn skips_file_with_same_contents_as_earlier_file() {
            let mut setup = setup(&[], &[]);
            let dir = tempfile::tempdir().unwrap();
            let files = [
                put(dir.path(), "A.gba", b"same"),
                put(dir.path(), "B.gba", b"same"),
            ];
            let lines = lines_of(|on_logged| setup.pocketeer.import_games(&files, None, on_logged));
            assert_eq!(
                lines,
                [
                    "A.gba: imported as gba/A.gba",
                    "B.gba: the library has it already, as gba/A.gba",
                ]
            );
            assert_eq!(names(&setup.pocketeer), ["A.gba"]);
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
            assert_eq!(expand_import_paths(&paths).unwrap(), [game, deep, lone]);
        }

        #[test]
        fn prefers_given_platform_over_extension() {
            let mut setup = setup(&[], &[]);
            let (_dir, from) = apart("x.gba", b"x");
            let files = [from];
            lines_of(|on_logged| {
                setup
                    .pocketeer
                    .import_games(&files, Some(&Platform::Gb), on_logged)
            });
            let root = setup.pocketeer.library().root();
            assert!(root.join("roms/gb/x.gba").exists());
            assert!(!root.join("roms/gba").exists());
        }

        #[test]
        fn rejects_unlisted_platform_id() {
            let mut setup = setup(&[], &[]);
            let (_dir, from) = apart("x.bin", b"x");
            let files = [from];
            for id in ["../../escaped", "/absolute", "arduboy"] {
                let platform = Platform::from_id(id);
                let lines = lines_of(|on_logged| {
                    setup
                        .pocketeer
                        .import_games(&files, Some(&platform), on_logged)
                });
                assert_eq!(lines, ["x.bin: can't tell which platform it is for"]);
            }
            assert_eq!(names(&setup.pocketeer), Vec::<String>::new());
            let root = setup.pocketeer.library().root();
            assert!(!root.parent().unwrap().join("escaped").exists());
        }

        #[test]
        fn reports_read_failure_of_missing_file() {
            let mut setup = setup(&[], &[]);
            let dir = tempfile::tempdir().unwrap();
            let from = dir.path().join("Gone.gba");
            let files = [from.clone()];
            let lines = lines_of(|on_logged| setup.pocketeer.import_games(&files, None, on_logged));
            assert_eq!(lines.len(), 1);
            let expected_start =
                format!("Gone.gba: error: failed to open file `{}`", from.display());
            assert!(lines[0].starts_with(&expected_start), "{}", lines[0]);
        }
    }

    mod logging {
        use super::*;

        #[test]
        fn stops_batch_after_reporting_game_when_log_unwritable() {
            let mut setup = setup(&[], &[("roms/gba/A.gba", b"a"), ("roms/gba/B.gba", b"b")]);
            let games = setup.pocketeer.library().entries().to_vec();
            fs::create_dir(setup.pocketeer.library().layout().log()).unwrap();
            let mut seen = Vec::new();
            let batch_result = setup.pocketeer.push_games(&games, |_, entry| {
                seen.push(entry.report.subject());
                ControlFlow::Continue(())
            });
            assert!(matches!(batch_result, Err(BasicError::LogFailed { .. })));
            assert_eq!(seen, ["gba/A.gba"]);
            assert_eq!(setup.pocketeer.games_on_device().len(), 1);
        }

        #[test]
        fn returns_failure_when_success_cannot_be_logged() {
            let (mut setup, _, _) = with_versions();
            let root = setup.pocketeer.library().root().to_path_buf();
            put(&root, "roms/gba/C.gba", b"c");
            fs::create_dir(setup.pocketeer.library().layout().log()).unwrap();
            assert!(matches!(
                setup.pocketeer.reload(),
                Err(BasicError::LogFailed { .. })
            ));
            assert_eq!(setup.pocketeer.library().entries().len(), 3);
        }

        #[test]
        fn returns_original_failure_when_log_unwritable() {
            let mut setup = setup(&[], &[]);
            setup.pocketeer.close_device();
            fs::create_dir(setup.pocketeer.library().layout().log()).unwrap();
            assert!(matches!(
                setup.pocketeer.eject_device(),
                Err(EjectDeviceError::Other(BasicError::NoDevice))
            ));
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
            let local = setup.first_local();
            let on_device = setup.first_on_device();
            (setup, local, on_device)
        }

        /// What `renames_to_dat_names` makes of the one game in the library.
        fn renames_of(setup: &Setup) -> Vec<(String, String)> {
            setup
                .pocketeer
                .renames_to_dat_names(setup.pocketeer.library().entries())
                .into_iter()
                .map(|(local, new)| (local.name, new))
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
            let (mut setup, _, on_device) = on_both();
            let games = [(on_device, "New".to_string())];
            let lines =
                lines_of(|on_logged| setup.pocketeer.rename_games_on_device(&games, on_logged));
            assert_eq!(lines, ["gba/Old.gba: renamed to gba/New.gba"]);
            let logged = log_lines(&setup);
            assert_eq!(logged[0]["device"], "gba/New.gba");
            let row = setup.pocketeer.game_rows().remove(0);
            assert_eq!(row.local_entry().unwrap().name, "Old.gba");
            let on_device = row.device_entry().unwrap();
            assert_eq!(on_device.path, Path::new("New.gba"));
            assert_eq!(on_device.save.as_ref().unwrap().size, 4);
        }
    }

    mod cores {
        use super::*;

        const GB: &str = "Spiritualized.GB";

        fn ids(ids: &[&str]) -> Vec<String> {
            ids.iter().map(ToString::to_string).collect()
        }

        #[test]
        fn lists_every_core_as_available_without_device() {
            let mut setup = setup(&[("Cores/Spiritualized.GB/core.json", b"{}")], &[]);
            setup.pocketeer.close_device();
            let (rows, _) = setup.pocketeer.load_core_rows().unwrap();
            assert!(matches!(rows[..], [CoreRow::Available(_)]));
        }

        #[test]
        fn does_not_download_core_at_newest_version() {
            let mut setup = setup(&[], &[]);
            lines_of(|on_logged| setup.pocketeer.install_cores(&ids(&[GB]), on_logged));
            assert_eq!(setup.downloaded.get(), 1);
            let lines = lines_of(|on_logged| setup.pocketeer.install_cores(&ids(&[GB]), on_logged));
            assert_eq!(
                lines,
                ["Spiritualized.GB: the device has the newest version"]
            );
            assert_eq!(setup.downloaded.get(), 1);
        }

        #[test]
        fn rejects_unsupported_core() {
            let mut setup = setup(&[], &[]);
            let lines =
                lines_of(|on_logged| setup.pocketeer.install_cores(&ids(&["no.Such"]), on_logged));
            assert_eq!(lines, ["no.Such: not a core that can be installed"]);
        }

        #[test]
        fn downloads_nothing_without_device() {
            let mut setup = setup(&[], &[]);
            setup.pocketeer.close_device();
            let lines = lines_of(|on_logged| setup.pocketeer.install_cores(&ids(&[GB]), on_logged));
            assert_eq!(lines, ["Spiritualized.GB: error: no device is open"]);
            assert_eq!(setup.downloaded.get(), 0);
        }
    }

    mod rereading {
        use super::*;

        const GB: &str = "Spiritualized.GB";

        /// Its `core.zip` declares `gba_bios.bin`.
        fn install(setup: &mut Setup) {
            let ids = [GB.to_string()];
            lines_of(|on_logged| setup.pocketeer.install_cores(&ids, on_logged));
        }

        fn remove(setup: &mut Setup) {
            let ids = [GB.to_string()];
            lines_of(|on_logged| setup.pocketeer.remove_cores(&ids, on_logged));
        }

        fn games(setup: &Setup) -> Vec<PathBuf> {
            let games = setup.pocketeer.games_on_device();
            games.into_iter().map(|game| game.path).collect()
        }

        #[test]
        fn updates_game_list_when_cores_change() {
            let mut setup = setup(&[("Assets/gba/common/gba_bios.bin", b"four")], &[]);
            assert_eq!(games(&setup), [PathBuf::from("gba_bios.bin")]);
            install(&mut setup);
            assert_eq!(games(&setup), [] as [PathBuf; 0]);
            remove(&mut setup);
            assert_eq!(games(&setup), [PathBuf::from("gba_bios.bin")]);
        }

        #[test]
        fn does_not_scan_unread_device_for_cores() {
            let mut setup = setup(&[("Assets/gba/common/Tetris.gba", b"hello")], &[]);
            let device = setup.pocketeer.device().unwrap().root().to_path_buf();
            setup.pocketeer.open_device(&device).unwrap();
            install(&mut setup);
            assert_eq!(games(&setup), [] as [PathBuf; 0]);
        }

        #[test]
        fn finds_changes_on_both_sides_and_logs() {
            let mut setup = setup(
                &[("Assets/gba/common/One.gba", b"one")],
                &[("roms/gba/A.gba", b"a")],
            );
            let device = setup.pocketeer.device().unwrap().root().to_path_buf();
            let library = setup.pocketeer.library().root().to_path_buf();
            put(&device, "Assets/gba/common/RPG/Two.gba", b"one");
            put(&library, "roms/gba/B.gba", b"b");
            let report = setup.pocketeer.reload().unwrap();
            assert_eq!(report.duplicates.len(), 1);
            assert_eq!(setup.pocketeer.library().entries().len(), 2);
            assert_eq!(log_lines(&setup)[0]["message"], "read again");
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
            assert_eq!(left[0].path, Path::new("RPG/Two.gba"));
        }
    }

    mod orphan_saves {
        use super::*;

        #[test]
        fn reports_orphan_gone_from_library() {
            let mut setup = setup(&[], &[("saves/gba/Gone/a.sav", b"save")]);
            let orphans = setup.pocketeer.find_orphan_saves_in_library().unwrap();
            let root = setup.pocketeer.library().root().to_path_buf();
            fs::remove_file(root.join("saves/gba/Gone/a.sav")).unwrap();
            let lines = lines_of(|on_logged| {
                setup
                    .pocketeer
                    .trash_orphan_saves_in_library(&orphans, on_logged)
            });
            assert_eq!(lines, ["gba/Gone: the library has no such orphan save"]);
        }
    }
}
