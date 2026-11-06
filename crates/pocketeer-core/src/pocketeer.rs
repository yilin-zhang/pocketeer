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
    dat::{DatEntry, DatMirror, RemoteDatSource, SyncReport},
    device::{
        DeleteSaveError, Device, DeviceEntry, EjectDeviceError, OpenDeviceError, OrphanDeviceSave,
        RenameOnDeviceError, ScanReport,
    },
    error::SharedError,
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

/// How a batch ended: whether what it was given broke, or the error if the
/// log can't be written.
pub type BatchResult = Result<ControlFlow<()>, SharedError>;

// ============================================================================
// What operations return
// ============================================================================

/// Why the library's saves weren't put in the trash.
#[derive(Debug, Clone, thiserror::Error)]
pub enum TrashSaveError {
    /// The library has no such save.
    #[error("no such save in the library")]
    NoSave,
    /// The save has no version written then, or not any more.
    #[error("the library has no such save")]
    NoSuchVersion,
    /// Several versions of its save were written in the second asked for.
    #[error("more than one version of the save was written at {at}")]
    AmbiguousVersion { at: String },
    #[error(transparent)]
    Shared(#[from] SharedError),
}

/// Why a file wasn't made a game's cover.
#[derive(Debug, thiserror::Error)]
pub enum ImportCoverError {
    #[error("not a PNG, JPEG or WebP file")]
    NotAnImage,
    #[error(transparent)]
    Shared(#[from] SharedError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchCoverOutcome {
    Fetched,
    AlreadyHad,
}

/// Why no cover was downloaded.
#[derive(Debug, thiserror::Error)]
pub enum FetchCoverError {
    /// The source has no cover for it.
    #[error("no cover found for it")]
    NoCover,
    #[error(transparent)]
    Shared(#[from] SharedError),
}

/// What importing a save did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportSaveOutcome {
    /// The version it was kept as.
    Imported(Save),
    /// The library has a version with the same contents already.
    AlreadyHad,
}

/// What importing a game did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportGameOutcome {
    /// The game as it is in the library now.
    Imported(LocalEntry),
    /// The library has the game already, under this name.
    AlreadyHad(LocalEntry),
}

/// Why importing a game imported nothing.
#[derive(Debug, thiserror::Error)]
pub enum ImportGameError {
    /// No platform was given, or one that isn't named, and the file's extension
    /// doesn't tell.
    #[error("can't tell which platform it is for")]
    UnknownPlatform,
    #[error(transparent)]
    Add(#[from] AddGameError),
    #[error(transparent)]
    Shared(#[from] SharedError),
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
        let history = self.local_entry().and_then(|l| l.save.as_ref());
        history.map_or(0, |h| h.iter().count())
    }

    /// Whether the device has a save the library doesn't.
    pub fn has_new_save_on_device(&self) -> bool {
        let history = self.local_entry().and_then(|l| l.save.as_ref());
        let on_device = self.device_entry().and_then(|d| d.save.as_ref());
        on_device.is_some_and(|s| !history.is_some_and(|h| h.contains(s)))
    }
}

// ============================================================================
// Pocketeer
// ============================================================================

/// The library, the DATs, what covers and cores there are, and the device when
/// one is open. DATs come from `D`, covers from `C` and cores from `I`; only
/// tests use others than [`Libretro`] and [`OpenFpga`].
pub struct Pocketeer<D = Libretro, C = Libretro, I = OpenFpga> {
    library: Library,
    dats: DatMirror<D>,
    covers: CoverIndex<C>,
    cores: CoreInventory<I>,
    device: Option<Device>,
    logger: Logger,
}

impl Pocketeer {
    /// The library at `library`, with the DATs it keeps, without going
    /// online. No device is open. What is done is logged as done through
    /// `front_end`.
    pub fn open(library: &Path, front_end: FrontEnd) -> Result<Self, OpenLibraryError> {
        Pocketeer::open_with(library, front_end, Libretro, Libretro, OpenFpga)
    }

    /// `Pocketeer Library` in the home folder, made if there is none: the only
    /// library that is.
    pub fn default_library_path() -> Result<PathBuf, OpenLibraryError> {
        let home = std::env::home_dir().ok_or(OpenLibraryError::NoHome)?;
        let library = home.join("Pocketeer Library");
        fs::create_dir_all(&library).map_err(SharedError::from)?;
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

    /// Opens the device at `root`, in place of any open one. It is not
    /// scanned yet.
    pub fn open_device(&mut self, root: &Path) -> Result<(), OpenDeviceError> {
        self.device = Some(Device::open(root)?);
        Ok(())
    }

    /// Opens the device mounted now, if there is one, in place of any open
    /// one; whether there was. It is not scanned yet. With more than one
    /// mounted, none is opened: which one is not for pocketeer to guess.
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

    pub fn scan_device(&mut self) -> Result<ScanReport, SharedError> {
        self.device.as_mut().ok_or(SharedError::NoDevice)?.scan()
    }

    /// Ejects the open device and closes it; if the system won't eject it, it
    /// stays open.
    pub fn eject_device(&mut self) -> Result<(), EjectDeviceError> {
        let root = self.device.as_ref().map(|d| d.root().to_path_buf());
        let ejected = match &self.device {
            Some(device) => device.eject(),
            None => Err(SharedError::NoDevice.into()),
        };
        if ejected.is_ok() {
            self.device = None;
        }
        self.log_result(ejected, |ejected| OperationReport::Eject(root, ejected))
    }

    // ---- Reload -----------------------------------------------------------------

    /// Reads the library's folder again, and the open device's, for changes
    /// made outside pocketeer.
    pub fn reload(&mut self) -> Result<ScanReport, SharedError> {
        let read = self.reload_library().and_then(|()| match self.device {
            Some(_) => self.scan_device(),
            None => Ok(ScanReport::default()),
        });
        self.log_result(read, |read| OperationReport::Reload(read.map(|_| ())))
    }

    fn reload_library(&mut self) -> Result<(), SharedError> {
        self.library.reload()
    }

    // ---- Listing games ----------------------------------------------------------

    /// Every game: the library's in order, each with its copy on the device
    /// if it has one, then those only on the device.
    pub fn game_rows(&self) -> Vec<GameRow> {
        let device = self.device.as_ref();
        let mut rows: Vec<GameRow> = self
            .library
            .entries()
            .iter()
            .map(
                |local| match device.and_then(|d| d.entry_for(&local.game)) {
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
        let on_device = self.device.as_ref().map_or(&[][..], |d| d.entries());
        on_device
            .iter()
            .filter(|d| self.library.entry_for(&d.game).is_none())
    }

    /// The games in the library without a cover.
    pub fn games_without_covers(&self) -> Vec<LocalEntry> {
        let entries = self.library.entries().iter();
        entries
            .filter(|e| e.cover_path.is_none())
            .cloned()
            .collect()
    }

    /// The games on the device the library doesn't have.
    pub fn games_only_on_device(&self) -> Vec<DeviceEntry> {
        self.entries_only_on_device().cloned().collect()
    }

    /// The games on the device the library has too.
    pub fn games_on_device_in_library(&self) -> Vec<DeviceEntry> {
        let library = &self.library;
        let mut games = self.games_on_device();
        games.retain(|g| library.entry_for(&g.game).is_some());
        games
    }

    /// Every game on the device.
    pub(crate) fn games_on_device(&self) -> Vec<DeviceEntry> {
        let entries = self.device.as_ref().map_or(&[][..], |d| d.entries());
        entries.to_vec()
    }

    // ---- DATs -------------------------------------------------------------------

    pub fn sync_dats(&mut self) -> Result<SyncReport, SharedError> {
        let synced = self.dats.sync();
        self.log_result(synced, OperationReport::SyncDats)
    }

    /// What the DATs call `game`.
    pub fn find_dat_entry(&self, game: &Game) -> Option<&DatEntry> {
        self.dats.lookup(game)
    }

    /// The name pulling gives a game on the device in the library.
    pub fn name_for_pull(&self, on_device: &DeviceEntry) -> String {
        self.name_in_library(&on_device.game, &on_device.path)
    }

    /// The name in the library of `game`, from `file`: its name in the DATs,
    /// with the extension of the file, or the file's name if the DATs don't
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

    /// Each of `games` whose name isn't what the DATs call it, with that name.
    /// Games the DATs don't know are left out.
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
        on_entry_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            games,
            |p, game| OperationReport::PullGame(game.clone(), p.pull_game(game)),
            on_entry_logged,
        )
    }

    /// Copies `on_device` into the library, under its name in the DATs if
    /// they know it, and under its file name if not.
    fn pull_game(&mut self, on_device: &DeviceEntry) -> Result<PullGameOutcome, AddGameError> {
        let on_device = self.current_device_entry(on_device)?;
        let name = self.name_for_pull(&on_device);
        let device = self.device.as_ref().ok_or(SharedError::NoDevice)?;
        transfer::pull_game(&mut self.library, device, &on_device, &name)
    }

    pub fn push_games(
        &mut self,
        games: &[LocalEntry],
        on_entry_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            games,
            |p, game| OperationReport::PushGame(game.clone(), p.push_game(game)),
            on_entry_logged,
        )
    }

    fn push_game(&mut self, local: &LocalEntry) -> Result<PushGameOutcome, PushGameError> {
        let local = self.current_local_entry(local)?;
        let device = self.device.as_mut().ok_or(SharedError::NoDevice)?;
        transfer::push_game(&self.library, device, &local)
    }

    /// Imports each file as a game, for `platform` or else what its extension
    /// says.
    pub fn import_games(
        &mut self,
        files: &[PathBuf],
        platform: Option<&Platform>,
        on_entry_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            files,
            |p, from| {
                let result = p.import_game(from, platform);
                // The log outlives the folder the command was run in.
                let from = std::path::absolute(from).unwrap_or_else(|_| from.clone());
                OperationReport::ImportGame(from, result)
            },
            on_entry_logged,
        )
    }

    /// Copies the file at `from` into the library, unless it has the game, for
    /// `platform` or else what the file's extension says, under its name in
    /// the DATs if they know it, and under its file name if not. The file
    /// stays where it is.
    fn import_game(
        &mut self,
        from: &Path,
        platform: Option<&Platform>,
    ) -> Result<ImportGameOutcome, ImportGameError> {
        let by_extension = || {
            let extension = from.extension()?.to_str()?;
            Platform::from_extension(extension)
        };
        // Only a named platform: an id can be anything, and it makes a path.
        let named = platform.filter(|p| !matches!(p, Platform::Other(_)));
        let platform = named.cloned().or_else(by_extension);
        let platform = platform.ok_or(ImportGameError::UnknownPlatform)?;
        let game = Game::read(platform, from).map_err(SharedError::from)?;
        if let Some(entry) = self.library.entry_for(&game) {
            return Ok(ImportGameOutcome::AlreadyHad(entry.clone()));
        }
        let name = self.name_in_library(&game, from);
        let added = self.library.add_game(&game, &name, from)?;
        Ok(ImportGameOutcome::Imported(added))
    }

    /// Calls each game in the library what it comes with, keeping its extension.
    /// The device isn't touched.
    pub fn rename_games_in_library(
        &mut self,
        games: &[(LocalEntry, String)],
        on_entry_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            games,
            |p, (local, new)| {
                OperationReport::RenameInLibrary(
                    local.clone(),
                    p.rename_game_in_library(local, new),
                )
            },
            on_entry_logged,
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

    /// Calls each game on the device what it comes with, keeping its extension
    /// and its folder. The library isn't touched. Which copy of a game comes
    /// first goes by path, so the device is read again at the end.
    pub fn rename_games_on_device(
        &mut self,
        games: &[(DeviceEntry, String)],
        on_entry_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        let batch_result = self.for_each_item(
            games,
            |p, (on_device, new)| {
                OperationReport::RenameOnDevice(
                    on_device.clone(),
                    p.rename_game_on_device(on_device, new),
                )
            },
            on_entry_logged,
        );
        self.rescan_device_after(batch_result)
    }

    fn rename_game_on_device(
        &mut self,
        on_device: &DeviceEntry,
        new: &str,
    ) -> Result<DeviceEntry, RenameOnDeviceError> {
        let on_device = self.current_device_entry(on_device)?;
        let device = self.device.as_mut().ok_or(SharedError::NoDevice)?;
        device.rename_game(&on_device, new)
    }

    pub fn fetch_covers(
        &mut self,
        games: &[LocalEntry],
        on_entry_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            games,
            |p, game| OperationReport::FetchCover(game.clone(), p.fetch_cover(game)),
            on_entry_logged,
        )
    }

    /// Downloads a cover for `local` if it has none: the cover of its name
    /// in the DATs if they know it, and of its file name without the
    /// extension if not.
    fn fetch_cover(&mut self, local: &LocalEntry) -> Result<FetchCoverOutcome, FetchCoverError> {
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
            return Err(FetchCoverError::NoCover);
        };
        self.library.add_cover(platform, &local.name, &cover)?;
        Ok(FetchCoverOutcome::Fetched)
    }

    /// Makes each file the cover of the game it comes with. A cover the game
    /// has already goes in the system's trash.
    pub fn import_covers(
        &mut self,
        covers: &[(LocalEntry, PathBuf)],
        on_entry_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            covers,
            |p, (local, from)| {
                let result = p.import_cover(local, from);
                // The log outlives the folder the command was run in.
                let from = std::path::absolute(from).unwrap_or_else(|_| from.clone());
                OperationReport::ImportCover(local.clone(), from, result)
            },
            on_entry_logged,
        )
    }

    /// A cover of another format is trashed once the new one is written, so
    /// the game has one if writing fails; one of the same format has its file
    /// name, so it has to go first.
    fn import_cover(&mut self, local: &LocalEntry, from: &Path) -> Result<(), ImportCoverError> {
        let local = self.current_local_entry(local)?;
        let bytes = fs::read(from).map_err(SharedError::from)?;
        let cover = Cover::from_bytes(bytes).ok_or(ImportCoverError::NotAnImage)?;
        let platform = &local.game.platform;
        let new_path = self
            .library
            .layout()
            .cover(platform, local.stem(), cover.format());
        let old_path = local.cover_path.as_ref();
        if old_path == Some(&new_path) {
            trash::delete(&new_path).map_err(SharedError::from)?;
        }
        self.library.add_cover(platform, &local.name, &cover)?;
        if let Some(old_path) = old_path.filter(|path| **path != new_path) {
            trash::delete(old_path).map_err(SharedError::from)?;
        }
        Ok(())
    }

    pub fn trash_games_in_library(
        &mut self,
        games: &[LocalEntry],
        on_entry_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            games,
            |p, game| OperationReport::TrashGame(game.clone(), p.trash_game_in_library(game)),
            on_entry_logged,
        )
    }

    /// Puts `local`'s file and cover in the system's trash, from where they
    /// can be put back. Its saves stay, for a game put there under its name.
    fn trash_game_in_library(&mut self, local: &LocalEntry) -> Result<(), SharedError> {
        let local = self.current_local_entry(local)?;
        let files = self.library.files_of(&local);
        trash::delete_all(&files)?;
        self.library.forget_game(&local.game.platform, &local.name);
        Ok(())
    }

    /// Once the first copy of a game is deleted, the next takes its place, so
    /// the device is read again at the end, however it ended.
    pub fn delete_games_on_device(
        &mut self,
        games: &[DeviceEntry],
        on_entry_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        let batch_result = self.for_each_item(
            games,
            |p, game| OperationReport::DeleteGame(game.clone(), p.delete_game_on_device(game)),
            on_entry_logged,
        );
        self.rescan_device_after(batch_result)
    }

    /// Deletes the file of `on_device` from the device for good. Its save
    /// stays there.
    fn delete_game_on_device(&mut self, on_device: &DeviceEntry) -> Result<(), SharedError> {
        let on_device = self.current_device_entry(on_device)?;
        let device = self.device.as_mut().ok_or(SharedError::NoDevice)?;
        device.remove_game(&on_device)
    }

    // ---- Saves ------------------------------------------------------------------

    pub fn pull_saves(
        &mut self,
        games: &[DeviceEntry],
        on_entry_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            games,
            |p, game| OperationReport::PullSave(game.clone(), p.pull_save(game)),
            on_entry_logged,
        )
    }

    fn pull_save(&mut self, on_device: &DeviceEntry) -> Result<PullSaveOutcome, PullSaveError> {
        let on_device = self.current_device_entry(on_device)?;
        let device = self.device.as_ref().ok_or(SharedError::NoDevice)?;
        transfer::pull_save(&mut self.library, device, &on_device)
    }

    /// Pushes the newest version of each game's save.
    pub fn push_saves(
        &mut self,
        games: &[DeviceEntry],
        on_entry_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            games,
            |p, game| OperationReport::PushSave(game.clone(), p.push_newest_save(game)),
            on_entry_logged,
        )
    }

    /// Pushes the newest version of `game`'s save in the library.
    fn push_newest_save(&mut self, game: &DeviceEntry) -> Result<PushSaveOutcome, PushSaveError> {
        let version = self.newest_save(&game.game)?;
        self.push_save(game, &version)
    }

    /// The newest version of `game`'s save in the library.
    fn newest_save(&self, game: &Game) -> Result<Save, PushSaveError> {
        let entry = self.library.entry_for(game);
        let entry = entry.ok_or(PushSaveError::NotInLibrary)?;
        let history = entry.save.as_ref().ok_or(PushSaveError::NoSuchVersion)?;
        Ok(history.current.clone())
    }

    /// Pushes the version of each game's save written at `at`, as
    /// [`Save::timestamp`] writes it.
    pub fn push_saves_at(
        &mut self,
        games: &[DeviceEntry],
        at: &str,
        on_entry_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            games,
            |p, game| OperationReport::PushSave(game.clone(), p.push_save_written_at(game, at)),
            on_entry_logged,
        )
    }

    /// Pushes the version of `game`'s save written at `at`.
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

    /// Pushes each version to the game it comes with.
    pub fn push_save_versions(
        &mut self,
        versions: &[(DeviceEntry, Save)],
        on_entry_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            versions,
            |p, (game, version)| {
                OperationReport::PushSave(game.clone(), p.push_save(game, version))
            },
            on_entry_logged,
        )
    }

    fn push_save(
        &mut self,
        on_device: &DeviceEntry,
        version: &Save,
    ) -> Result<PushSaveOutcome, PushSaveError> {
        let on_device = self.current_device_entry(on_device)?;
        let device = self.device.as_mut().ok_or(SharedError::NoDevice)?;
        transfer::push_save(&self.library, device, &on_device, version)
    }

    /// Imports each file as a version of the save of the game it comes with.
    pub fn import_saves(
        &mut self,
        saves: &[(LocalEntry, PathBuf)],
        on_entry_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            saves,
            |p, (local, from)| {
                let result = p.import_save(local, from);
                // The log outlives the folder the command was run in.
                let from = std::path::absolute(from).unwrap_or_else(|_| from.clone());
                OperationReport::ImportSave(local.clone(), from, result)
            },
            on_entry_logged,
        )
    }

    /// Keeps the file at `from` as a new version of `local`'s save, written
    /// when the file was. The file stays where it is.
    fn import_save(
        &mut self,
        local: &LocalEntry,
        from: &Path,
    ) -> Result<ImportSaveOutcome, SharedError> {
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
        on_entry_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            games,
            |p, game| {
                OperationReport::TrashSaveAll(game.clone(), p.trash_every_version_in_library(game))
            },
            on_entry_logged,
        )
    }

    fn trash_every_version_in_library(&mut self, local: &LocalEntry) -> Result<(), TrashSaveError> {
        let local = self.current_local_entry(local)?;
        let files = self.library.files_of_all_saves(&local)?;
        if files.is_empty() {
            return Err(TrashSaveError::NoSave);
        }
        self.trash_save_files(&local, &files)
    }

    /// Trashes the version of each game's save written at `at`, as
    /// [`Save::timestamp`] writes it.
    pub fn trash_saves_at(
        &mut self,
        games: &[LocalEntry],
        at: &str,
        on_entry_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            games,
            |p, game| {
                let result = p.trash_save_written_at(game, at);
                OperationReport::TrashSaveVersion(game.clone(), at.to_string(), result)
            },
            on_entry_logged,
        )
    }

    /// Trashes the version of `game`'s save written at `at`.
    fn trash_save_written_at(&mut self, game: &LocalEntry, at: &str) -> Result<(), TrashSaveError> {
        let local = self.current_local_entry(game)?;
        match &saves_written_at(&local, at)[..] {
            [] => Err(TrashSaveError::NoSuchVersion),
            [version] => self.trash_save_version(&local, version),
            _ => Err(TrashSaveError::AmbiguousVersion { at: at.to_string() }),
        }
    }

    /// Trashes each version of the save of the game it comes with.
    pub fn trash_save_versions_in_library(
        &mut self,
        versions: &[(LocalEntry, Save)],
        on_entry_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            versions,
            |p, (game, version)| {
                let result = p.trash_save_version(game, version);
                OperationReport::TrashSaveVersion(game.clone(), version.timestamp(), result)
            },
            on_entry_logged,
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
        self.trash_save_files(&local, &[file])
    }

    fn trash_save_files(
        &mut self,
        local: &LocalEntry,
        files: &[PathBuf],
    ) -> Result<(), TrashSaveError> {
        trash::delete_all(files).map_err(SharedError::from)?;
        self.library
            .reread_saves(&local.game.platform, &local.name)?;
        Ok(())
    }

    pub fn delete_saves_on_device(
        &mut self,
        games: &[DeviceEntry],
        on_entry_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            games,
            |p, game| OperationReport::DeleteSave(game.clone(), p.delete_save_on_device(game)),
            on_entry_logged,
        )
    }

    /// Deletes the save of `on_device` from the device for good, whether or
    /// not the library has it.
    fn delete_save_on_device(&mut self, on_device: &DeviceEntry) -> Result<(), DeleteSaveError> {
        let on_device = self.current_device_entry(on_device)?;
        let device = self.device.as_mut().ok_or(SharedError::NoDevice)?;
        device.remove_save(&on_device)
    }

    // ---- Orphan saves -----------------------------------------------------------

    /// The saves in the library with no game.
    pub fn find_orphan_saves_in_library(&self) -> Result<Vec<OrphanLocalSave>, SharedError> {
        self.library.find_orphan_saves()
    }

    /// The saves on the device with no game; none if no device is open.
    pub fn find_orphan_saves_on_device(&self) -> Result<Vec<OrphanDeviceSave>, SharedError> {
        self.device
            .as_ref()
            .map_or(Ok(Vec::new()), Device::find_orphan_saves)
    }

    /// Trashes every version of each orphan's save.
    pub fn trash_orphan_saves_in_library(
        &mut self,
        orphans: &[OrphanLocalSave],
        on_entry_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            orphans,
            |p, orphan| {
                OperationReport::TrashOrphanSave(
                    orphan.clone(),
                    p.trash_orphan_save_in_library(orphan),
                )
            },
            on_entry_logged,
        )
    }

    fn trash_orphan_save_in_library(
        &mut self,
        orphan: &OrphanLocalSave,
    ) -> Result<(), TrashSaveError> {
        let files = self.library.files_of_orphan_save(orphan)?;
        if files.is_empty() {
            return Err(TrashSaveError::NoSave);
        }
        trash::delete_all(&files).map_err(SharedError::from)?;
        Ok(())
    }

    pub fn delete_orphan_saves_on_device(
        &mut self,
        orphans: &[OrphanDeviceSave],
        on_entry_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            orphans,
            |p, orphan| {
                OperationReport::DeleteOrphanSave(
                    orphan.clone(),
                    p.delete_orphan_save_on_device(orphan),
                )
            },
            on_entry_logged,
        )
    }

    fn delete_orphan_save_on_device(
        &mut self,
        orphan: &OrphanDeviceSave,
    ) -> Result<(), DeleteSaveError> {
        let device = self.device.as_ref().ok_or(SharedError::NoDevice)?;
        device.remove_orphan_save(orphan)
    }

    // ---- Cores ------------------------------------------------------------------

    // What a core loads is no game, so the games change with the cores.

    /// Every core: those that can be installed, then the others on the device.
    /// If the inventory can't be had that is told too, with the cores on the
    /// device alone.
    pub fn load_core_rows(&mut self) -> Result<(Vec<CoreRow>, Option<SharedError>), SharedError> {
        let installed = match &self.device {
            Some(device) => device.installed_cores()?,
            None => Vec::new(),
        };
        let (latest, failure) = match self.cores.supported_cores() {
            Ok(latest) => (latest, None),
            Err(e) => (Vec::new(), Some(e)),
        };
        Ok((CoreRow::pair(installed, latest), failure))
    }

    /// The cores on the device with another version in the inventory.
    pub fn find_cores_with_updates(&mut self) -> Result<Vec<String>, SharedError> {
        let (rows, failure) = self.load_core_rows()?;
        if let Some(failure) = failure {
            return Err(failure);
        }
        let with_updates = rows.iter().filter(|r| r.has_update());
        Ok(with_updates.map(|r| r.id().to_string()).collect())
    }

    /// Puts the newest release of each core on the device.
    pub fn install_cores(
        &mut self,
        ids: &[String],
        on_entry_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        let batch_result = self.for_each_item(
            ids,
            |p, id| OperationReport::InstallCore(id.clone(), p.install_core(id)),
            on_entry_logged,
        );
        self.rescan_device_after(batch_result)
    }

    /// Puts the newest release of the core on the device, unless it is there.
    fn install_core(&mut self, id: &str) -> Result<InstallCoreOutcome, InstallCoreError> {
        if !SUPPORTED.contains(&id) {
            return Err(InstallCoreError::NotSupported);
        }
        let device = self.device.as_ref().ok_or(SharedError::NoDevice)?;
        let installed = device.installed_cores()?;
        let latest = self
            .cores
            .supported_cores()?
            .into_iter()
            .find(|c| c.id == id);
        let latest = latest.ok_or(InstallCoreError::NotSupported)?;
        let on_device = installed.iter().find(|c| c.id == id);
        if on_device.is_some_and(|c| c.is_at_version(&latest.version)) {
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
        on_entry_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        let batch_result = self.for_each_item(
            ids,
            |p, id| OperationReport::RemoveCore(id.clone(), p.remove_core(id)),
            on_entry_logged,
        );
        self.rescan_device_after(batch_result)
    }

    fn remove_core(&mut self, id: &str) -> Result<(), RemoveCoreError> {
        let device = self.device.as_ref().ok_or(SharedError::NoDevice)?;
        device.remove_core(id)
    }

    // ---- BIOS files -------------------------------------------------------------

    /// The files the cores on the device load.
    pub fn read_bios_rows(&self) -> Result<Vec<BiosRow>, SharedError> {
        self.device
            .as_ref()
            .ok_or(SharedError::NoDevice)?
            .read_bios_rows()
    }

    /// Copies each file to where the row it comes with says.
    pub fn put_bios_files(
        &mut self,
        files: &[(BiosRow, PathBuf)],
        on_entry_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_item(
            files,
            |p, (row, from)| {
                let result = p.put_bios_file(row, from);
                // The log outlives the folder the command was run in.
                let from = std::path::absolute(from).unwrap_or_else(|_| from.clone());
                OperationReport::PutBios(row.clone(), from, result)
            },
            on_entry_logged,
        )
    }

    /// Copies the file at `from` to where `row` says. The file stays where it
    /// is.
    fn put_bios_file(&mut self, row: &BiosRow, from: &Path) -> Result<(), PutBiosError> {
        let device = self.device.as_ref().ok_or(SharedError::NoDevice)?;
        device.put_bios(&row.platform, &row.path, from)
    }

    // ---- Logging and checks -----------------------------------------------------

    // What the front ends call notes what it did in the log. A call that acts
    // once goes through `log_result`, a batch through `for_each_item`.

    /// Notes how `result` went, and returns it. If it went well but can't be
    /// noted, that is the error.
    fn log_result<T: Clone, E: Clone + From<SharedError>>(
        &self,
        result: Result<T, E>,
        to_report: impl FnOnce(Result<T, E>) -> OperationReport,
    ) -> Result<T, E> {
        let (_, log_write_result) = self.logger.record(to_report(result.clone()));
        let value = result?;
        log_write_result?;
        Ok(value)
    }

    // The batch methods below go through their items with `for_each_item`, one
    // at a time: each item is done, and its report noted in the log and handed
    // to `on_entry_logged` before the next one starts. An item that fails doesn't
    // stop the others. `on_entry_logged` can stop them by returning `Break`, once
    // the current item is finished, and it can read how things stand now. They
    // return whether it broke, or the error if the log can't be written, which
    // also stops them.

    fn for_each_item<Item>(
        &mut self,
        items: &[Item],
        op: impl Fn(&mut Self, &Item) -> OperationReport,
        mut on_entry_logged: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        for item in items {
            let report = op(self, item);
            let (entry, log_write_result) = self.logger.record(report);
            let control_flow = on_entry_logged(self, entry);
            log_write_result?;
            if control_flow.is_break() {
                return Ok(control_flow);
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Reads the device's games again after a batch, however it ended, if they
    /// have been read at all.
    fn rescan_device_after(&mut self, batch_result: BatchResult) -> BatchResult {
        let scan_result = match &mut self.device {
            Some(device) if device.has_been_scanned() => device.scan().map(|_| ()),
            _ => Ok(()),
        };
        let control_flow = batch_result?;
        scan_result?;
        Ok(control_flow)
    }

    // What is done takes entries as the caller saw them, and looks them up again:
    // they may have changed or gone since.

    /// `entry` as the device has it now, which may not be what the caller saw.
    fn current_device_entry(&self, entry: &DeviceEntry) -> Result<DeviceEntry, SharedError> {
        let device = self.device.as_ref().ok_or(SharedError::NoDevice)?;
        let current = device.entry_at(&entry.game.platform, &entry.path);
        current.cloned().ok_or(SharedError::NotOnDevice)
    }

    /// `entry` as the library has it now, which may not be what the caller
    /// saw.
    fn current_local_entry(&self, entry: &LocalEntry) -> Result<LocalEntry, SharedError> {
        let current = self.library.entry_named(&entry.game.platform, &entry.name);
        current.cloned().ok_or(SharedError::NotInLibrary)
    }
}

// ============================================================================
// Helpers
// ============================================================================

/// `paths` with each folder replaced by every file under it whose extension
/// tells its platform; a file stays as it is.
pub fn expand_import_paths(paths: &[PathBuf]) -> Result<Vec<PathBuf>, SharedError> {
    let tells_platform = |file: &PathBuf| {
        let extension = file.extension().and_then(|e| e.to_str());
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

/// The versions of `local`'s save written at `at`.
fn saves_written_at(local: &LocalEntry, at: &str) -> Vec<Save> {
    let written = local.save.iter().flat_map(|h| h.written_at(at));
    written.cloned().collect()
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, fs, rc::Rc};

    use super::*;
    use crate::cores::{
        BiosStatus, InventoryCore,
        testing::{core_json, data_json, zip_of},
    };
    use crate::cover::Cover;
    use crate::files::testing::{put, put_at};

    /// A DAT that knows the GBA game "hello".
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

        fn version(&self, _: &str) -> Result<String, SharedError> {
            Ok("1".into())
        }

        fn names(&self, _: &str, _: &str) -> Result<Vec<String>, SharedError> {
            Ok(vec!["Tetris (World)".into(), "My Hack".into()])
        }

        fn cover_name(&self, game_name: &str) -> String {
            game_name.into()
        }

        fn fetch(&self, _: &str, _: &str) -> Result<Cover, SharedError> {
            let mut png = Vec::new();
            image::RgbaImage::new(1, 1)
                .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
                .unwrap();
            Ok(Cover::from_bytes(png).unwrap())
        }
    }

    /// A source of cores, with `Spiritualized.GB` at 2.0; it counts what it
    /// downloads, shared with the test.
    #[derive(Clone, Default)]
    struct CoreSource {
        downloaded: Rc<Cell<u32>>,
    }

    impl RemoteCoreSource for CoreSource {
        fn inventory(&self) -> Result<Vec<InventoryCore>, SharedError> {
            Ok(vec![InventoryCore {
                id: "Spiritualized.GB".into(),
                version: "2.0".into(),
                platforms: vec![Platform::Gb],
                download_url: "core.zip".into(),
            }])
        }

        fn download(&self, url: &str) -> Result<Vec<u8>, SharedError> {
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

    /// DATs only from the library's folder: the tests never sync.
    type Tested = Pocketeer<Libretro, Covers, CoreSource>;

    struct Setup {
        _dir: tempfile::TempDir,
        pocketeer: Tested,
    }

    /// A library, a mirror holding [`GBA_DAT`], and an open, scanned device;
    /// `device` and `library` are the files each has.
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
        let mut pocketeer = Pocketeer::open_with(
            &root.join("library"),
            FrontEnd::Cli,
            Libretro,
            Covers,
            CoreSource::default(),
        )
        .unwrap();
        pocketeer.open_device(&root.join("device")).unwrap();
        pocketeer.scan_device().unwrap();
        Setup {
            _dir: dir,
            pocketeer,
        }
    }

    fn names(p: &Tested) -> Vec<String> {
        p.library()
            .entries()
            .iter()
            .map(|e| e.name.clone())
            .collect()
    }

    /// One game with a save written at 1,000 seconds and one without, on the
    /// device and in the library.
    fn with_versions() -> (Setup, Vec<DeviceEntry>, Vec<LocalEntry>) {
        let mut s = setup(
            &[
                ("Assets/gba/common/A.gba", b"a"),
                ("Assets/gba/common/B.gba", b"b"),
            ],
            &[("roms/gba/A.gba", b"a"), ("roms/gba/B.gba", b"b")],
        );
        let root = s.pocketeer.library().root().to_path_buf();
        put_at(&root, "saves/gba/A/one.sav", b"one", 1_000);
        s.pocketeer.reload_library().unwrap();
        let on_device = s.pocketeer.games_on_device();
        let local = s.pocketeer.library().entries().to_vec();
        (s, on_device, local)
    }

    /// The library's log, one value for each line.
    fn log_lines(s: &Setup) -> Vec<serde_json::Value> {
        let log = s.pocketeer.library().layout().log();
        fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// What `run` hands its `on_entry_logged`, as the lines a front end would show.
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
            let s = setup(
                &[
                    ("Assets/gba/common/Tetris.gba", b"hello"),
                    ("Assets/gba/common/Only Here.gba", b"device"),
                ],
                &[
                    ("roms/gba/Tetris (World).gba", b"hello"),
                    ("roms/gba/Only There.gba", b"library"),
                ],
            );
            let rows: Vec<_> = s
                .pocketeer
                .game_rows()
                .iter()
                .map(|r| {
                    let local = r.local_entry().map(|l| l.name.clone());
                    let on_device = r.device_entry().map(|d| d.path.display().to_string());
                    (local, on_device)
                })
                .collect();
            let row = |l: Option<&str>, d: Option<&str>| (l.map(String::from), d.map(String::from));
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
            let mut s = setup(
                &[
                    ("Assets/gba/common/Tetris.gba", b"hello"),
                    ("Saves/gba/common/Tetris.sav", b"save"),
                ],
                &[("roms/gba/Tetris (World).gba", b"hello")],
            );
            let row = s.pocketeer.game_rows().remove(0);
            assert_eq!(row.saves_kept(), 0);
            assert!(row.has_new_save_on_device());
            let on_device = s.pocketeer.games_on_device().remove(0);
            s.pocketeer.pull_save(&on_device).unwrap();
            let row = s.pocketeer.game_rows().remove(0);
            assert_eq!(row.saves_kept(), 1);
            assert!(!row.has_new_save_on_device());
        }

        #[test]
        fn lists_only_device_games_in_library() {
            let s = setup(
                &[
                    ("Assets/gba/common/Both.gba", b"both"),
                    ("Assets/gba/common/Here.gba", b"here"),
                ],
                &[("roms/gba/Mine.gba", b"both")],
            );
            let games = s.pocketeer.games_on_device_in_library();
            assert_eq!(games.len(), 1);
            assert_eq!(games[0].path, Path::new("Both.gba"));
        }
    }

    mod pull_game_names {
        use super::*;

        #[test]
        fn uses_dat_name_for_known_game() {
            let mut s = setup(&[("Assets/gba/common/tetris.gba", b"hello")], &[]);
            let on_device = s.pocketeer.games_on_device().remove(0);
            let pulled = s.pocketeer.pull_game(&on_device).unwrap();
            assert!(matches!(pulled, PullGameOutcome::Pulled(_)));
            assert_eq!(names(&s.pocketeer), ["Tetris (World).gba"]);
        }

        #[test]
        fn keeps_file_name_for_unknown_game() {
            let mut s = setup(&[("Assets/gba/common/RPG/My Hack.gba", b"hack")], &[]);
            let on_device = s.pocketeer.games_on_device().remove(0);
            s.pocketeer.pull_game(&on_device).unwrap();
            assert_eq!(names(&s.pocketeer), ["My Hack.gba"]);
        }
    }

    mod missing_targets {
        use super::*;

        #[test]
        fn returns_no_device_for_transfers() {
            let mut s = setup(
                &[("Assets/gba/common/Tetris.gba", b"hello")],
                &[("roms/gba/Tetris (World).gba", b"hello")],
            );
            let on_device = s.pocketeer.games_on_device().remove(0);
            let local = s.pocketeer.library().entries()[0].clone();
            s.pocketeer.close_device();
            assert_eq!(s.pocketeer.game_rows().len(), 1);
            assert!(matches!(
                s.pocketeer.push_game(&local),
                Err(PushGameError::Shared(SharedError::NoDevice))
            ));
            assert!(matches!(
                s.pocketeer.pull_game(&on_device),
                Err(AddGameError::Shared(SharedError::NoDevice))
            ));
        }

        #[test]
        fn returns_error_for_entry_no_longer_present() {
            let mut s = setup(
                &[("Assets/gba/common/Tetris.gba", b"hello")],
                &[("roms/gba/Tetris (World).gba", b"hello")],
            );
            let on_device = s.pocketeer.games_on_device().remove(0);
            let local = s.pocketeer.library().entries()[0].clone();
            let device = s.pocketeer.device().unwrap().root().to_path_buf();
            fs::remove_file(device.join("Assets/gba/common/Tetris.gba")).unwrap();
            s.pocketeer.scan_device().unwrap();
            assert!(matches!(
                s.pocketeer.pull_save(&on_device),
                Err(PullSaveError::Shared(SharedError::NotOnDevice))
            ));
            fs::remove_file(s.pocketeer.library().game_path(&local)).unwrap();
            s.pocketeer.reload_library().unwrap();
            assert!(matches!(
                s.pocketeer.fetch_cover(&local),
                Err(FetchCoverError::Shared(SharedError::NotInLibrary))
            ));
        }

        #[test]
        fn returns_error_when_required_save_is_missing() {
            let mut s = setup(
                &[("Assets/gba/common/Tetris.gba", b"hello")],
                &[("roms/gba/Tetris (World).gba", b"hello")],
            );
            let on_device = s.pocketeer.games_on_device().remove(0);
            let local = s.pocketeer.library().entries()[0].clone();
            assert!(matches!(
                s.pocketeer.trash_every_version_in_library(&local),
                Err(TrashSaveError::NoSave)
            ));
            let gone = Save::from_bytes(b"gone", std::time::UNIX_EPOCH);
            assert!(matches!(
                s.pocketeer.trash_save_version(&local, &gone),
                Err(TrashSaveError::NoSuchVersion)
            ));
            assert!(matches!(
                s.pocketeer.delete_save_on_device(&on_device),
                Err(DeleteSaveError::NoSave)
            ));
            let lines =
                lines_of(|on_entry_logged| s.pocketeer.push_saves(&[on_device], on_entry_logged));
            assert_eq!(lines, ["gba/Tetris.gba: the library has no such save"]);
        }
    }

    mod batches {
        use super::*;

        #[test]
        fn continues_after_failed_game() {
            let mut s = setup(&[], &[("roms/gba/Tetris (World).gba", b"hello")]);
            let local = s.pocketeer.library().entries()[0].clone();
            let unknown = LocalEntry {
                name: "Gone.gba".into(),
                ..local.clone()
            };
            let mut seen = Vec::new();
            let _ = s
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
            let mut s = setup(&[], &[("roms/gba/A.gba", b"a"), ("roms/gba/B.gba", b"b")]);
            let games = s.pocketeer.library().entries().to_vec();
            let mut seen = Vec::new();
            let control_flow = s.pocketeer.push_games(&games, |_, entry| {
                seen.push(entry.report.subject());
                ControlFlow::Break(())
            });
            assert!(control_flow.unwrap().is_break());
            assert_eq!(seen, ["gba/A.gba"]);
            assert_eq!(s.pocketeer.games_on_device().len(), 1);
            // What was finished is in the log, and nothing else.
            assert_eq!(log_lines(&s).len(), 1);
        }
    }

    mod covers {
        use super::*;

        #[test]
        fn fetches_cover_by_dat_name() {
            let mut s = setup(&[], &[("roms/gba/tetris.gba", b"hello")]);
            let local = s.pocketeer.library().entries()[0].clone();
            let fetched = s.pocketeer.fetch_cover(&local).unwrap();
            assert_eq!(fetched, FetchCoverOutcome::Fetched);
            let entry = &s.pocketeer.library().entries()[0];
            assert!(
                entry
                    .cover_path
                    .as_ref()
                    .unwrap()
                    .ends_with("covers/gba/tetris.png")
            );
            let again = s.pocketeer.fetch_cover(&local).unwrap();
            assert_eq!(again, FetchCoverOutcome::AlreadyHad);
        }

        #[test]
        fn fetches_cover_by_file_name_when_dats_do_not_know_game() {
            let mut s = setup(
                &[],
                &[
                    ("roms/gba/My Hack.gba", b"hack"),
                    ("roms/gba/Other.gba", b"other"),
                ],
            );
            let games = s.pocketeer.games_without_covers();
            let lines =
                lines_of(|on_entry_logged| s.pocketeer.fetch_covers(&games, on_entry_logged));
            assert_eq!(
                lines,
                [
                    "gba/My Hack.gba: cover downloaded",
                    "gba/Other.gba: no cover found for it"
                ]
            );
            assert_eq!(s.pocketeer.games_without_covers().len(), 1);
        }
    }

    mod versions {
        use super::*;

        #[test]
        fn pushes_given_version() {
            let (mut s, on_device, local) = with_versions();
            let version = local[0].save.as_ref().unwrap().current.clone();
            let versions = [(on_device[0].clone(), version)];
            let lines = lines_of(|on_entry_logged| {
                s.pocketeer.push_save_versions(&versions, on_entry_logged)
            });
            assert_eq!(lines, ["gba/A.gba: save 1970-01-01T00-16-40 pushed"]);
        }

        #[test]
        fn reports_trashing_version_library_does_not_have() {
            let (mut s, _, local) = with_versions();
            let gone = Save::from_bytes(b"gone", std::time::UNIX_EPOCH);
            let versions = [(local[0].clone(), gone)];
            let lines = lines_of(|on_entry_logged| {
                s.pocketeer
                    .trash_save_versions_in_library(&versions, on_entry_logged)
            });
            assert_eq!(lines, ["gba/A.gba: the library has no such save"]);
        }

        #[test]
        fn pushes_newest_by_default() {
            let (mut s, on_device, _) = with_versions();
            let root = s.pocketeer.library().root().to_path_buf();
            put_at(&root, "saves/gba/A/two.sav", b"two", 2_000);
            s.pocketeer.reload_library().unwrap();
            let lines = lines_of(|on_entry_logged| {
                s.pocketeer.push_saves(&on_device[..1], on_entry_logged)
            });
            assert_eq!(lines, ["gba/A.gba: save 1970-01-01T00-33-20 pushed"]);
        }

        #[test]
        fn pushes_version_written_at_given_time() {
            let (mut s, on_device, _) = with_versions();
            let at = "1970-01-01T00-16-40";
            let lines = lines_of(|on_entry_logged| {
                s.pocketeer
                    .push_saves_at(&on_device[..1], at, on_entry_logged)
            });
            assert_eq!(lines, ["gba/A.gba: save 1970-01-01T00-16-40 pushed"]);
        }

        #[test]
        fn reports_game_without_version_at_given_time() {
            let (mut s, on_device, _) = with_versions();
            let at = "1970-01-01T00-16-40";
            let lines = lines_of(|on_entry_logged| {
                s.pocketeer
                    .push_saves_at(&on_device[1..], at, on_entry_logged)
            });
            assert_eq!(lines, ["gba/B.gba: the library has no such save"]);
        }

        #[test]
        fn acts_on_neither_when_two_versions_share_time() {
            let (mut s, on_device, local) = with_versions();
            let root = s.pocketeer.library().root().to_path_buf();
            let file = put_at(&root, "saves/gba/A/two.sav", b"two", 1_000);
            let first = root.join("saves/gba/A/one.sav");
            s.pocketeer.reload_library().unwrap();
            let at = "1970-01-01T00-16-40";
            let refused = [
                "gba/A.gba: error: more than one version of the save was written \
                            at 1970-01-01T00-16-40",
            ];
            let lines = lines_of(|on_entry_logged| {
                s.pocketeer
                    .push_saves_at(&on_device[..1], at, on_entry_logged)
            });
            assert_eq!(lines, refused);
            let lines = lines_of(|on_entry_logged| {
                s.pocketeer.trash_saves_at(&local[..1], at, on_entry_logged)
            });
            assert_eq!(lines, refused);
            assert!(file.exists() && first.exists());
        }

        #[test]
        fn logs_missing_version_when_trashing() {
            let (mut s, _, local) = with_versions();
            let lines = lines_of(|on_entry_logged| {
                s.pocketeer.trash_saves_at(&local, "never", on_entry_logged)
            });
            assert_eq!(
                lines,
                [
                    "gba/A.gba: the library has no such save",
                    "gba/B.gba: the library has no such save"
                ]
            );
            let logged = log_lines(&s);
            assert_eq!(logged[0]["version"], "never");
            assert_eq!(logged[0]["ok"], false);
        }
    }

    mod import_covers {
        use super::*;

        /// The start of a PNG file: enough to be told as one.
        const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";

        /// A library with the game `A.gba`, and a folder apart from it.
        fn to_import_into() -> (Setup, tempfile::TempDir, LocalEntry) {
            let s = setup(&[], &[("roms/gba/A.gba", b"a")]);
            let local = s.pocketeer.library().entries()[0].clone();
            (s, tempfile::tempdir().unwrap(), local)
        }

        #[test]
        fn imports_cover_for_game_without_one() {
            let (mut s, elsewhere, local) = to_import_into();
            let from = put(elsewhere.path(), "mine.dat", PNG);
            let covers = [(local, from.clone())];
            let lines =
                lines_of(|on_entry_logged| s.pocketeer.import_covers(&covers, on_entry_logged));
            assert_eq!(lines, ["gba/A.gba: cover imported"]);
            let root = s.pocketeer.library().root();
            let cover = root.join("covers/gba/A.png");
            assert_eq!(fs::read(&cover).unwrap(), PNG);
            let entry = &s.pocketeer.library().entries()[0];
            assert_eq!(entry.cover_path, Some(cover));
            assert!(from.exists());
            assert_eq!(log_lines(&s)[0]["from"], from.display().to_string());
        }

        #[test]
        fn rejects_file_that_is_not_image() {
            let (mut s, elsewhere, local) = to_import_into();
            let covers = [(local, put(elsewhere.path(), "mine.png", b"not an image"))];
            let lines =
                lines_of(|on_entry_logged| s.pocketeer.import_covers(&covers, on_entry_logged));
            assert_eq!(lines, ["gba/A.gba: not a PNG, JPEG or WebP file"]);
            assert!(!s.pocketeer.library().root().join("covers").exists());
        }

        #[test]
        fn rejects_non_image_and_keeps_existing_cover() {
            let mut s = setup(&[], &[("roms/gba/A.gba", b"a"), ("covers/gba/A.png", PNG)]);
            let local = s.pocketeer.library().entries()[0].clone();
            let elsewhere = tempfile::tempdir().unwrap();
            let covers = [(local, put(elsewhere.path(), "mine.png", b"not an image"))];
            lines_of(|on_entry_logged| s.pocketeer.import_covers(&covers, on_entry_logged));
            let cover = s.pocketeer.library().root().join("covers/gba/A.png");
            assert_eq!(fs::read(cover).unwrap(), PNG);
        }

        #[test]
        fn reports_game_no_longer_in_library() {
            let (mut s, elsewhere, local) = to_import_into();
            fs::remove_file(s.pocketeer.library().game_path(&local)).unwrap();
            s.pocketeer.reload_library().unwrap();
            let covers = [(local, put(elsewhere.path(), "mine.png", PNG))];
            let lines =
                lines_of(|on_entry_logged| s.pocketeer.import_covers(&covers, on_entry_logged));
            assert_eq!(
                lines,
                ["gba/A.gba: error: the library doesn't have the game"]
            );
        }
    }

    mod import_saves {
        use super::*;

        /// A library with the game `A.gba`, and a folder apart from it.
        fn to_import_into() -> (Setup, tempfile::TempDir, LocalEntry) {
            let s = setup(&[], &[("roms/gba/A.gba", b"a")]);
            let local = s.pocketeer.library().entries()[0].clone();
            (s, tempfile::tempdir().unwrap(), local)
        }

        #[test]
        fn keeps_version_named_after_write_time_and_leaves_source() {
            let (mut s, elsewhere, local) = to_import_into();
            let from = put_at(elsewhere.path(), "Mine.sav", b"kept", 1_000);
            let saves = [(local, from.clone())];
            let lines =
                lines_of(|on_entry_logged| s.pocketeer.import_saves(&saves, on_entry_logged));
            assert_eq!(lines, ["gba/A.gba: save 1970-01-01T00-16-40 imported"]);
            let root = s.pocketeer.library().root();
            let kept = root.join("saves/gba/A/1970-01-01T00-16-40.sav");
            assert_eq!(fs::read(kept).unwrap(), b"kept");
            assert_eq!(fs::read(from).unwrap(), b"kept");
        }

        #[test]
        fn skips_save_in_library() {
            let (mut s, elsewhere, local) = to_import_into();
            let first = put_at(elsewhere.path(), "first.sav", b"kept", 1_000);
            let again = put_at(elsewhere.path(), "again.sav", b"kept", 2_000);
            let saves = [(local.clone(), first), (local, again)];
            let lines =
                lines_of(|on_entry_logged| s.pocketeer.import_saves(&saves, on_entry_logged));
            assert_eq!(lines[1], "gba/A.gba: the library has this save already");
            let entry = &s.pocketeer.library().entries()[0];
            assert_eq!(entry.save.as_ref().unwrap().iter().count(), 1);
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
            let mut s = setup(&[], &[]);
            let (_dir, from) = apart("tetris.gba", b"hello");
            let files = [from.clone()];
            let lines =
                lines_of(|on_entry_logged| s.pocketeer.import_games(&files, None, on_entry_logged));
            assert_eq!(lines, ["tetris.gba: imported as gba/Tetris (World).gba"]);
            assert_eq!(names(&s.pocketeer), ["Tetris (World).gba"]);
            assert!(from.exists());
        }

        #[test]
        fn skips_game_in_library_and_reports_its_name() {
            let mut s = setup(&[], &[("roms/gba/Mine.gba", b"hello")]);
            let (_dir, from) = apart("other name.gba", b"hello");
            let files = [from];
            let lines =
                lines_of(|on_entry_logged| s.pocketeer.import_games(&files, None, on_entry_logged));
            assert_eq!(
                lines,
                ["other name.gba: the library has it already, as gba/Mine.gba"]
            );
            assert_eq!(names(&s.pocketeer), ["Mine.gba"]);
        }

        #[test]
        fn skips_file_with_same_contents_as_earlier_file() {
            let mut s = setup(&[], &[]);
            let dir = tempfile::tempdir().unwrap();
            let files = [
                put(dir.path(), "A.gba", b"same"),
                put(dir.path(), "B.gba", b"same"),
            ];
            let lines =
                lines_of(|on_entry_logged| s.pocketeer.import_games(&files, None, on_entry_logged));
            assert_eq!(
                lines,
                [
                    "A.gba: imported as gba/A.gba",
                    "B.gba: the library has it already, as gba/A.gba",
                ]
            );
            assert_eq!(names(&s.pocketeer), ["A.gba"]);
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
            let mut s = setup(&[], &[]);
            let (_dir, from) = apart("x.gba", b"x");
            let files = [from];
            lines_of(|on_entry_logged| {
                s.pocketeer
                    .import_games(&files, Some(&Platform::Gb), on_entry_logged)
            });
            let root = s.pocketeer.library().root();
            assert!(root.join("roms/gb/x.gba").exists());
            assert!(!root.join("roms/gba").exists());
        }

        #[test]
        fn requires_platform_for_unknown_extension() {
            let mut s = setup(&[], &[]);
            let (_dir, from) = apart("x.bin", b"x");
            let files = [from.clone()];
            let lines =
                lines_of(|on_entry_logged| s.pocketeer.import_games(&files, None, on_entry_logged));
            assert_eq!(lines, ["x.bin: can't tell which platform it is for"]);
            assert_eq!(names(&s.pocketeer), Vec::<String>::new());
            lines_of(|on_entry_logged| {
                s.pocketeer
                    .import_games(&files, Some(&Platform::Genesis), on_entry_logged)
            });
            let root = s.pocketeer.library().root();
            assert!(root.join("roms/genesis/x.bin").exists());
        }

        #[test]
        fn rejects_unlisted_platform_id() {
            let mut s = setup(&[], &[]);
            let (_dir, from) = apart("x.bin", b"x");
            let files = [from];
            for id in ["../../escaped", "/absolute", "arduboy"] {
                let platform = Platform::from_id(id);
                let lines = lines_of(|on_entry_logged| {
                    s.pocketeer
                        .import_games(&files, Some(&platform), on_entry_logged)
                });
                assert_eq!(lines, ["x.bin: can't tell which platform it is for"]);
            }
            assert_eq!(names(&s.pocketeer), Vec::<String>::new());
            let root = s.pocketeer.library().root();
            assert!(!root.parent().unwrap().join("escaped").exists());
        }

        #[test]
        fn rejects_hidden_file() {
            let mut s = setup(&[], &[]);
            let (_dir, from) = apart(".hidden.gba", b"x");
            let files = [from];
            let lines =
                lines_of(|on_entry_logged| s.pocketeer.import_games(&files, None, on_entry_logged));
            assert_eq!(
                lines,
                [".hidden.gba: not imported: the library can't hold a game of that name"]
            );
            let root = s.pocketeer.library().root();
            assert!(!root.join("roms/gba/.hidden.gba").exists());
        }

        #[test]
        fn reports_read_failure_of_missing_file() {
            let mut s = setup(&[], &[]);
            let dir = tempfile::tempdir().unwrap();
            let from = dir.path().join("Gone.gba");
            let files = [from.clone()];
            let lines =
                lines_of(|on_entry_logged| s.pocketeer.import_games(&files, None, on_entry_logged));
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
            let mut s = setup(&[], &[("roms/gba/A.gba", b"a"), ("roms/gba/B.gba", b"b")]);
            let games = s.pocketeer.library().entries().to_vec();
            fs::create_dir(s.pocketeer.library().layout().log()).unwrap();
            let mut seen = Vec::new();
            let batch_result = s.pocketeer.push_games(&games, |_, entry| {
                seen.push(entry.report.subject());
                ControlFlow::Continue(())
            });
            assert!(matches!(batch_result, Err(SharedError::LogFailed { .. })));
            assert_eq!(seen, ["gba/A.gba"]);
            assert_eq!(s.pocketeer.games_on_device().len(), 1);
        }

        #[test]
        fn returns_failure_when_success_cannot_be_logged() {
            let (mut s, _, _) = with_versions();
            let root = s.pocketeer.library().root().to_path_buf();
            put(&root, "roms/gba/C.gba", b"c");
            fs::create_dir(s.pocketeer.library().layout().log()).unwrap();
            assert!(matches!(
                s.pocketeer.reload(),
                Err(SharedError::LogFailed { .. })
            ));
            assert_eq!(s.pocketeer.library().entries().len(), 3);
        }

        #[test]
        fn returns_original_failure_when_log_unwritable() {
            let mut s = setup(&[], &[]);
            s.pocketeer.close_device();
            fs::create_dir(s.pocketeer.library().layout().log()).unwrap();
            assert!(matches!(
                s.pocketeer.eject_device(),
                Err(EjectDeviceError::Shared(SharedError::NoDevice))
            ));
        }
    }

    mod renaming {
        use super::*;

        /// A game in the library and on the device, under the same name.
        fn on_both() -> (Setup, LocalEntry, DeviceEntry) {
            let s = setup(
                &[
                    ("Assets/gba/common/Old.gba", b"hello"),
                    ("Saves/gba/common/Old.sav", b"save"),
                ],
                &[("roms/gba/Old.gba", b"hello")],
            );
            let local = s.pocketeer.library().entries()[0].clone();
            let on_device = s.pocketeer.games_on_device().remove(0);
            (s, local, on_device)
        }

        /// What `renames_to_dat_names` makes of the one game in the library.
        fn renames_of(s: &Setup) -> Vec<(String, String)> {
            s.pocketeer
                .renames_to_dat_names(s.pocketeer.library().entries())
                .into_iter()
                .map(|(local, new)| (local.name, new))
                .collect()
        }

        #[test]
        fn finds_dat_name_of_game_called_something_else() {
            let s = setup(&[], &[("roms/gba/Old.gba", b"hello")]);
            assert_eq!(
                renames_of(&s),
                [("Old.gba".to_string(), "Tetris (World)".to_string())]
            );
        }

        #[test]
        fn finds_dat_name_differing_only_in_case() {
            let s = setup(&[], &[("roms/gba/tetris (world).gba", b"hello")]);
            assert_eq!(
                renames_of(&s),
                [(
                    "tetris (world).gba".to_string(),
                    "Tetris (World)".to_string()
                )]
            );
        }

        #[test]
        fn leaves_out_game_already_called_dat_name() {
            let s = setup(&[], &[("roms/gba/Tetris (World).gba", b"hello")]);
            assert_eq!(renames_of(&s), []);
        }

        #[test]
        fn leaves_out_game_unknown_to_dats() {
            let s = setup(&[], &[("roms/gba/Mine.gba", b"not in the DAT")]);
            assert_eq!(renames_of(&s), []);
        }

        #[test]
        fn renames_to_dat_name_with_save_and_cover_and_leaves_device() {
            let (mut s, _, _) = on_both();
            let root = s.pocketeer.library().root().to_path_buf();
            put(&root, "saves/gba/Old/x.sav", b"save");
            put(&root, "covers/gba/Old.png", b"cover");
            s.pocketeer.reload_library().unwrap();
            let renames = s
                .pocketeer
                .renames_to_dat_names(s.pocketeer.library().entries());
            let lines = lines_of(|on_entry_logged| {
                s.pocketeer
                    .rename_games_in_library(&renames, on_entry_logged)
            });
            assert_eq!(lines, ["gba/Old.gba: renamed to gba/Tetris (World).gba"]);
            assert!(root.join("roms/gba/Tetris (World).gba").exists());
            assert!(root.join("saves/gba/Tetris (World)/x.sav").exists());
            assert!(root.join("covers/gba/Tetris (World).png").exists());
            let row = s.pocketeer.game_rows().remove(0);
            assert_eq!(row.device_entry().unwrap().path, Path::new("Old.gba"));
        }

        #[test]
        fn renames_in_library_and_leaves_device() {
            let (mut s, local, _) = on_both();
            let games = [(local, "New".to_string())];
            let lines = lines_of(|on_entry_logged| {
                s.pocketeer.rename_games_in_library(&games, on_entry_logged)
            });
            assert_eq!(lines, ["gba/Old.gba: renamed to gba/New.gba"]);
            let logged = log_lines(&s);
            assert_eq!(logged[0]["library"], "gba/New.gba");
            let row = s.pocketeer.game_rows().remove(0);
            assert_eq!(row.local_entry().unwrap().name, "New.gba");
            assert_eq!(row.device_entry().unwrap().path, Path::new("Old.gba"));
        }

        #[test]
        fn renames_on_device_and_leaves_library() {
            let (mut s, _, on_device) = on_both();
            let games = [(on_device, "New".to_string())];
            let lines = lines_of(|on_entry_logged| {
                s.pocketeer.rename_games_on_device(&games, on_entry_logged)
            });
            assert_eq!(lines, ["gba/Old.gba: renamed to gba/New.gba"]);
            let logged = log_lines(&s);
            assert_eq!(logged[0]["device"], "gba/New.gba");
            let row = s.pocketeer.game_rows().remove(0);
            assert_eq!(row.local_entry().unwrap().name, "Old.gba");
            let on_device = row.device_entry().unwrap();
            assert_eq!(on_device.path, Path::new("New.gba"));
            assert_eq!(on_device.save.as_ref().unwrap().size, 4);
        }

        #[test]
        fn logs_refused_rename_as_failure() {
            let (mut s, local, _) = on_both();
            let games = [(local, "Old".to_string())];
            let lines = lines_of(|on_entry_logged| {
                s.pocketeer.rename_games_in_library(&games, on_entry_logged)
            });
            assert_eq!(
                lines,
                ["gba/Old.gba: not renamed: it has that name already"]
            );
            assert_eq!(log_lines(&s)[0]["ok"], false);
        }

        #[test]
        fn reports_game_gone_since_listing() {
            let (mut s, local, on_device) = on_both();
            let games = [(local, "New".to_string())];
            lines_of(|on_entry_logged| {
                s.pocketeer.rename_games_in_library(&games, on_entry_logged)
            });
            let lines = lines_of(|on_entry_logged| {
                s.pocketeer.rename_games_in_library(&games, on_entry_logged)
            });
            assert_eq!(
                lines,
                ["gba/Old.gba: error: the library doesn't have the game"]
            );
            s.pocketeer.close_device();
            let games = [(on_device, "New".to_string())];
            let lines = lines_of(|on_entry_logged| {
                s.pocketeer.rename_games_on_device(&games, on_entry_logged)
            });
            assert_eq!(lines, ["gba/Old.gba: error: no device is open"]);
        }
    }

    mod bios {
        use super::*;

        const GB: &str = "Spiritualized.GB";

        /// Its `core.zip` declares `gba_bios.bin`.
        fn install(s: &mut Setup) {
            let ids = [GB.to_string()];
            lines_of(|on_entry_logged| s.pocketeer.install_cores(&ids, on_entry_logged));
        }

        fn remove(s: &mut Setup) {
            let ids = [GB.to_string()];
            lines_of(|on_entry_logged| s.pocketeer.remove_cores(&ids, on_entry_logged));
        }

        fn games(s: &Setup) -> Vec<PathBuf> {
            let games = s.pocketeer.games_on_device();
            games.into_iter().map(|g| g.path).collect()
        }

        #[test]
        fn returns_error_without_device() {
            let mut s = setup(&[], &[]);
            s.pocketeer.close_device();
            assert!(matches!(
                s.pocketeer.read_bios_rows(),
                Err(SharedError::NoDevice)
            ));
        }

        #[test]
        fn puts_file_and_logs_source() {
            let mut s = setup(&[], &[]);
            install(&mut s);
            let row = s.pocketeer.read_bios_rows().unwrap().remove(0);
            let elsewhere = tempfile::tempdir().unwrap();
            let from = put(elsewhere.path(), "mine.bin", b"four");
            let files = [(row, from.clone())];
            let lines =
                lines_of(|on_entry_logged| s.pocketeer.put_bios_files(&files, on_entry_logged));
            assert_eq!(lines, ["gba/gba_bios.bin: put on the device"]);
            let logged = log_lines(&s);
            let last = logged.last().unwrap();
            assert_eq!(last["ok"], true);
            assert_eq!(last["from"], from.display().to_string());
            let row = s.pocketeer.read_bios_rows().unwrap().remove(0);
            assert_eq!(row.status, BiosStatus::Present);
        }

        #[test]
        fn logs_refused_file_as_failure() {
            let mut s = setup(&[], &[]);
            install(&mut s);
            let row = s.pocketeer.read_bios_rows().unwrap().remove(0);
            let elsewhere = tempfile::tempdir().unwrap();
            let files = [(row, put(elsewhere.path(), "mine.bin", b"five!"))];
            let lines =
                lines_of(|on_entry_logged| s.pocketeer.put_bios_files(&files, on_entry_logged));
            assert_eq!(
                lines,
                ["gba/gba_bios.bin: the file has 5 bytes, and the core takes 4"]
            );
            assert_eq!(log_lines(&s).last().unwrap()["ok"], false);
        }

        #[test]
        fn updates_game_list_when_cores_change() {
            let mut s = setup(&[("Assets/gba/common/gba_bios.bin", b"four")], &[]);
            assert_eq!(games(&s), [PathBuf::from("gba_bios.bin")]);
            install(&mut s);
            assert_eq!(games(&s), [] as [PathBuf; 0]);
            remove(&mut s);
            assert_eq!(games(&s), [PathBuf::from("gba_bios.bin")]);
        }

        #[test]
        fn does_not_scan_unread_device_for_cores() {
            let mut s = setup(&[("Assets/gba/common/Tetris.gba", b"hello")], &[]);
            let device = s.pocketeer.device().unwrap().root().to_path_buf();
            s.pocketeer.open_device(&device).unwrap();
            install(&mut s);
            assert_eq!(games(&s), [] as [PathBuf; 0]);
        }
    }

    mod cores {
        use super::*;

        const GB: &str = "Spiritualized.GB";

        fn ids(ids: &[&str]) -> Vec<String> {
            ids.iter().map(ToString::to_string).collect()
        }

        fn downloads_counted() -> (Setup, Rc<Cell<u32>>) {
            let mut s = setup(&[], &[]);
            let source = CoreSource::default();
            let downloaded = source.downloaded.clone();
            let library = s.pocketeer.library().root().to_path_buf();
            let device = s.pocketeer.device().unwrap().root().to_path_buf();
            s.pocketeer =
                Pocketeer::open_with(&library, FrontEnd::Cli, Libretro, Covers, source).unwrap();
            s.pocketeer.open_device(&device).unwrap();
            (s, downloaded)
        }

        #[test]
        fn lists_inventory_cores_then_others() {
            let mut s = setup(
                &[
                    (
                        "Cores/Spiritualized.GB/core.json",
                        core_json("1.0").as_bytes(),
                    ),
                    ("Cores/someone.Else/core.json", core_json("1.0").as_bytes()),
                ],
                &[],
            );
            let (rows, failure) = s.pocketeer.load_core_rows().unwrap();
            assert!(failure.is_none());
            let found: Vec<_> = rows.iter().map(|r| (r.id(), r.has_update())).collect();
            assert_eq!(found, [(GB, true), ("someone.Else", false)]);
            assert_eq!(s.pocketeer.find_cores_with_updates().unwrap(), [GB]);
        }

        #[test]
        fn lists_every_core_as_available_without_device() {
            let mut s = setup(&[("Cores/Spiritualized.GB/core.json", b"{}")], &[]);
            s.pocketeer.close_device();
            let (rows, _) = s.pocketeer.load_core_rows().unwrap();
            assert!(matches!(rows[..], [CoreRow::Available(_)]));
        }

        #[test]
        fn installs_and_logs_core() {
            let mut s = setup(&[], &[]);
            let lines =
                lines_of(|on_entry_logged| s.pocketeer.install_cores(&ids(&[GB]), on_entry_logged));
            assert_eq!(lines, ["Spiritualized.GB: 2.0 installed"]);
            let logged = log_lines(&s);
            assert_eq!(logged[0]["game"], GB);
            assert_eq!(logged[0]["version"], "2.0");
            let (rows, _) = s.pocketeer.load_core_rows().unwrap();
            assert!(!rows[0].has_update());
            assert!(rows[0].installed().is_some());
        }

        #[test]
        fn does_not_download_core_at_newest_version() {
            let (mut s, downloaded) = downloads_counted();
            lines_of(|on_entry_logged| s.pocketeer.install_cores(&ids(&[GB]), on_entry_logged));
            assert_eq!(downloaded.get(), 1);
            let lines =
                lines_of(|on_entry_logged| s.pocketeer.install_cores(&ids(&[GB]), on_entry_logged));
            assert_eq!(
                lines,
                ["Spiritualized.GB: the device has the newest version"]
            );
            assert_eq!(downloaded.get(), 1);
        }

        #[test]
        fn continues_after_unsupported_core() {
            let mut s = setup(&[], &[]);
            let lines = lines_of(|on_entry_logged| {
                s.pocketeer
                    .install_cores(&ids(&["no.Such", GB]), on_entry_logged)
            });
            assert_eq!(
                lines,
                [
                    "no.Such: not a core that can be installed",
                    "Spiritualized.GB: 2.0 installed"
                ]
            );
        }

        #[test]
        fn downloads_nothing_without_device() {
            let (mut s, downloaded) = downloads_counted();
            s.pocketeer.close_device();
            let lines =
                lines_of(|on_entry_logged| s.pocketeer.install_cores(&ids(&[GB]), on_entry_logged));
            assert_eq!(lines, ["Spiritualized.GB: error: no device is open"]);
            assert_eq!(downloaded.get(), 0);
        }

        #[test]
        fn removes_core_from_device() {
            let mut s = setup(&[("Cores/a.B/core.json", b"{}")], &[]);
            let lines = lines_of(|on_entry_logged| {
                s.pocketeer.remove_cores(&ids(&["a.B"]), on_entry_logged)
            });
            assert_eq!(lines, ["a.B: removed from the device"]);
        }

        #[test]
        fn reports_core_not_on_device() {
            let mut s = setup(&[], &[]);
            let lines = lines_of(|on_entry_logged| {
                s.pocketeer.remove_cores(&ids(&["a.B"]), on_entry_logged)
            });
            assert_eq!(lines, ["a.B: not on the device"]);
        }
    }

    mod rereading {
        use super::*;

        #[test]
        fn finds_changes_on_both_sides_and_logs() {
            let mut s = setup(
                &[("Assets/gba/common/One.gba", b"one")],
                &[("roms/gba/A.gba", b"a")],
            );
            let device = s.pocketeer.device().unwrap().root().to_path_buf();
            let library = s.pocketeer.library().root().to_path_buf();
            put(&device, "Assets/gba/common/RPG/Two.gba", b"one");
            put(&library, "roms/gba/B.gba", b"b");
            let report = s.pocketeer.reload().unwrap();
            assert_eq!(report.duplicates.len(), 1);
            assert_eq!(s.pocketeer.library().entries().len(), 2);
            assert_eq!(log_lines(&s)[0]["message"], "read again");
        }

        #[test]
        fn uses_next_copy_after_deleting_game() {
            let mut s = setup(
                &[
                    ("Assets/gba/common/One.gba", b"copy"),
                    ("Assets/gba/common/RPG/Two.gba", b"copy"),
                ],
                &[],
            );
            let games = s.pocketeer.games_on_device();
            lines_of(|on_entry_logged| s.pocketeer.delete_games_on_device(&games, on_entry_logged));
            let left = s.pocketeer.games_on_device();
            assert_eq!(left[0].path, Path::new("RPG/Two.gba"));
        }
    }

    mod orphan_saves {
        use super::*;

        #[test]
        fn deletes_and_logs_orphan_on_device() {
            let mut s = setup(&[("Saves/gba/common/RPG/Gone.sav", b"save")], &[]);
            let orphans = s.pocketeer.find_orphan_saves_on_device().unwrap();
            let lines = lines_of(|on_entry_logged| {
                s.pocketeer
                    .delete_orphan_saves_on_device(&orphans, on_entry_logged)
            });
            assert_eq!(lines, ["gba/RPG/Gone.sav: save deleted from the device"]);
            assert_eq!(s.pocketeer.find_orphan_saves_on_device().unwrap(), []);
        }

        #[test]
        fn reports_orphan_gone_from_library() {
            let mut s = setup(&[], &[("saves/gba/Gone/a.sav", b"save")]);
            let orphans = s.pocketeer.find_orphan_saves_in_library().unwrap();
            let root = s.pocketeer.library().root().to_path_buf();
            fs::remove_file(root.join("saves/gba/Gone/a.sav")).unwrap();
            let lines = lines_of(|on_entry_logged| {
                s.pocketeer
                    .trash_orphan_saves_in_library(&orphans, on_entry_logged)
            });
            assert_eq!(lines, ["gba/Gone: no such save in the library"]);
        }
    }
}
