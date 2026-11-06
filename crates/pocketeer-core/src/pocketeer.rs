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
    cover::{CoverIndex, RemoteCoverSource},
    dat::{DatEntry, DatMirror, RemoteDatSource, SyncReport},
    device::{
        DeleteSaveError, Device, DeviceEntry, OrphanDeviceSave, RenameOnDeviceError, ScanReport,
    },
    error::{BaseError, trash_failed},
    files::list_files_recursive,
    game::{Game, Save},
    library::{AddGameError, Library, LocalEntry, OrphanLocalSave, RenameInLibraryError},
    log::{FrontEnd, LogEntry, LogEvent, Logger},
    platform::Platform,
    sources::{libretro::Libretro, openfpga::OpenFpga},
    transfer::{
        self, PullGameOutcome, PullSaveError, PullSaveOutcome, PushGameError, PushGameOutcome,
        PushSaveError, PushSaveOutcome,
    },
};

/// How a batch ended: whether what it was given broke, or the error if the
/// log can't be written.
pub type BatchResult = Result<ControlFlow<()>, BaseError>;

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
    Other(#[from] BaseError),
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
    Other(#[from] BaseError),
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
}

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
    pub fn open(library: &Path, front_end: FrontEnd) -> Result<Self, BaseError> {
        Pocketeer::open_with(library, front_end, Libretro, Libretro, OpenFpga)
    }

    /// `Pocketeer Library` in the home folder, made if there is none: the only
    /// library that is.
    pub fn default_library_path() -> Result<PathBuf, BaseError> {
        let home = std::env::home_dir().ok_or(BaseError::NoHome)?;
        let library = home.join("Pocketeer Library");
        fs::create_dir_all(&library)?;
        Ok(library)
    }
}

impl<D: RemoteDatSource, C: RemoteCoverSource, I: RemoteCoreSource> Pocketeer<D, C, I> {
    /// Like [`Pocketeer::open`], with DATs from `dats`, covers from `covers`
    /// and cores from `cores`.
    fn open_with(
        library: &Path,
        front_end: FrontEnd,
        dats: D,
        covers: C,
        cores: I,
    ) -> Result<Self, BaseError> {
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

    pub fn library(&self) -> &Library {
        &self.library
    }

    pub fn device(&self) -> Option<&Device> {
        self.device.as_ref()
    }

    fn reload_library(&mut self) -> Result<(), BaseError> {
        self.library.reload()
    }

    /// Opens the device at `root`, in place of any open one. It is not
    /// scanned yet.
    pub fn open_device(&mut self, root: &Path) -> Result<(), BaseError> {
        self.device = Some(Device::open(root)?);
        Ok(())
    }

    /// Opens the device mounted now, if there is one, in place of any open
    /// one; whether there was. It is not scanned yet. With more than one
    /// mounted, none is opened: which one is not for pocketeer to guess.
    pub fn open_mounted_device(&mut self) -> Result<bool, BaseError> {
        match &Device::find_mounted()[..] {
            [] => Ok(false),
            [root] => self.open_device(root).map(|()| true),
            found => Err(BaseError::ManyDevices {
                found: found.to_vec(),
            }),
        }
    }

    pub fn close_device(&mut self) {
        self.device = None;
    }

    pub fn scan_device(&mut self) -> Result<ScanReport, BaseError> {
        self.device.as_mut().ok_or(BaseError::NoDevice)?.scan()
    }

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

    /// What the DATs call `game`.
    pub fn dat_entry_for(&self, game: &Game) -> Option<&DatEntry> {
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

    // What the front ends call notes what it did in the log. What it calls
    // for one game goes through `log_result`, the others through
    // `for_each_game`.

    /// Notes how `done` went, and returns it. If it went well but can't be
    /// noted, that is the error.
    fn log_result<T: Clone, E: Clone + From<BaseError>>(
        &self,
        done: Result<T, E>,
        event: impl FnOnce(Result<T, E>) -> LogEvent,
    ) -> Result<T, E> {
        let (_, written) = self.logger.record(event(done.clone()));
        let value = done?;
        written?;
        Ok(value)
    }

    pub fn sync_dats(&mut self) -> Result<SyncReport, BaseError> {
        let synced = self.dats.sync();
        self.log_result(synced, LogEvent::SyncDats)
    }

    /// Reads the library's folder again, and the open device's, for changes
    /// made outside pocketeer.
    pub fn reload(&mut self) -> Result<ScanReport, BaseError> {
        let read = self.reload_library().and_then(|()| match self.device {
            Some(_) => self.scan_device(),
            None => Ok(ScanReport::default()),
        });
        self.log_result(read, |read| LogEvent::Reload(read.map(|_| ())))
    }

    /// Ejects the open device and closes it; if the system won't eject it, it
    /// stays open.
    pub fn eject_device(&mut self) -> Result<(), BaseError> {
        let root = self.device.as_ref().map(|d| d.root().to_path_buf());
        let ejected = self
            .device
            .as_ref()
            .ok_or(BaseError::NoDevice)
            .and_then(Device::eject);
        if ejected.is_ok() {
            self.device = None;
        }
        self.log_result(ejected, |ejected| LogEvent::Eject(root, ejected))
    }

    /// Makes `version` of the library's save for the game `on_device` its
    /// save on the device.
    pub fn push_save_version(
        &mut self,
        on_device: &DeviceEntry,
        version: &Save,
    ) -> Result<PushSaveOutcome, PushSaveError> {
        let pushed = self.push_save(on_device, version);
        self.log_result(pushed, |pushed| {
            LogEvent::PushSave(on_device.clone(), pushed)
        })
    }

    /// Puts `version` of `local`'s save in the system's trash.
    pub fn trash_save_version_in_library(
        &mut self,
        local: &LocalEntry,
        version: &Save,
    ) -> Result<(), TrashSaveError> {
        let trashed = self.trash_save_version(local, version);
        self.log_result(trashed, |trashed| {
            LogEvent::TrashSaveVersion(local.clone(), version.timestamp(), trashed)
        })
    }

    // These take entries as the caller saw them, and look them up again: they
    // may have changed or gone since.

    /// Copies `on_device` into the library, under its name in the DATs if
    /// they know it, and under its file name if not.
    fn pull_game(&mut self, on_device: &DeviceEntry) -> Result<PullGameOutcome, AddGameError> {
        let on_device = self.current_device_entry(on_device)?;
        let name = self.name_for_pull(&on_device);
        let device = self.device.as_ref().ok_or(BaseError::NoDevice)?;
        transfer::pull_game(&mut self.library, device, &on_device, &name)
    }

    fn pull_save(&mut self, on_device: &DeviceEntry) -> Result<PullSaveOutcome, PullSaveError> {
        let on_device = self.current_device_entry(on_device)?;
        let device = self.device.as_ref().ok_or(BaseError::NoDevice)?;
        transfer::pull_save(&mut self.library, device, &on_device)
    }

    fn push_game(&mut self, local: &LocalEntry) -> Result<PushGameOutcome, PushGameError> {
        let local = self.current_local_entry(local)?;
        let device = self.device.as_mut().ok_or(BaseError::NoDevice)?;
        transfer::push_game(&self.library, device, &local)
    }

    fn push_save(
        &mut self,
        on_device: &DeviceEntry,
        version: &Save,
    ) -> Result<PushSaveOutcome, PushSaveError> {
        let on_device = self.current_device_entry(on_device)?;
        let device = self.device.as_mut().ok_or(BaseError::NoDevice)?;
        transfer::push_save(&self.library, device, &on_device, version)
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
        let game = Game::read(platform, from).map_err(|e| AddGameError::Other(e.into()))?;
        if let Some(entry) = self.library.entry_for(&game) {
            return Ok(ImportGameOutcome::AlreadyHad(entry.clone()));
        }
        let name = self.name_in_library(&game, from);
        let added = self.library.add_game(&game, &name, from)?;
        Ok(ImportGameOutcome::Imported(added))
    }

    /// Keeps the file at `from` as a new version of `local`'s save, written
    /// when the file was. The file stays where it is.
    fn import_save(
        &mut self,
        local: &LocalEntry,
        from: &Path,
    ) -> Result<ImportSaveOutcome, BaseError> {
        let local = self.current_local_entry(local)?;
        let added = self
            .library
            .add_save(&local.game.platform, &local.name, from)?;
        Ok(added.map_or(ImportSaveOutcome::AlreadyHad, ImportSaveOutcome::Imported))
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

    fn rename_game_on_device(
        &mut self,
        on_device: &DeviceEntry,
        new: &str,
    ) -> Result<DeviceEntry, RenameOnDeviceError> {
        let on_device = self.current_device_entry(on_device)?;
        let device = self.device.as_mut().ok_or(BaseError::NoDevice)?;
        device.rename_game(&on_device, new)
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

    /// Puts `local`'s file and cover in the system's trash, from where they
    /// can be put back. Its saves stay, for a game put there under its name.
    fn trash_game_in_library(&mut self, local: &LocalEntry) -> Result<(), BaseError> {
        let local = self.current_local_entry(local)?;
        let files = self.library.files_of(&local);
        trash::delete_all(&files).map_err(trash_failed)?;
        self.library.forget_game(&local.game.platform, &local.name);
        Ok(())
    }

    fn trash_every_version_in_library(&mut self, local: &LocalEntry) -> Result<(), TrashSaveError> {
        let local = self.current_local_entry(local)?;
        let files = self.library.files_of_all_saves(&local)?;
        if files.is_empty() {
            return Err(TrashSaveError::NoSave);
        }
        self.trash_save_files(&local, &files)
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
        trash::delete_all(files).map_err(trash_failed)?;
        self.library
            .reread_saves(&local.game.platform, &local.name)?;
        Ok(())
    }

    fn trash_orphan_save_in_library(
        &mut self,
        orphan: &OrphanLocalSave,
    ) -> Result<(), TrashSaveError> {
        let files = self.library.files_of_orphan_save(orphan)?;
        if files.is_empty() {
            return Err(TrashSaveError::NoSave);
        }
        trash::delete_all(&files).map_err(trash_failed)?;
        Ok(())
    }

    /// Deletes the file of `on_device` from the device for good. Its save
    /// stays there.
    fn delete_game_on_device(&mut self, on_device: &DeviceEntry) -> Result<(), BaseError> {
        let on_device = self.current_device_entry(on_device)?;
        let device = self.device.as_mut().ok_or(BaseError::NoDevice)?;
        device.remove_game(&on_device)
    }

    /// Deletes the save of `on_device` from the device for good, whether or
    /// not the library has it.
    fn delete_save_on_device(&mut self, on_device: &DeviceEntry) -> Result<(), DeleteSaveError> {
        let on_device = self.current_device_entry(on_device)?;
        let device = self.device.as_mut().ok_or(BaseError::NoDevice)?;
        device.remove_save(&on_device)
    }

    fn delete_orphan_save_on_device(
        &mut self,
        orphan: &OrphanDeviceSave,
    ) -> Result<(), DeleteSaveError> {
        let device = self.device.as_ref().ok_or(BaseError::NoDevice)?;
        device.remove_orphan_save(orphan)
    }

    /// Every core: those that can be installed, then the others on the device.
    /// If the inventory can't be had that is told too, with the cores on the
    /// device alone.
    pub fn core_rows(&mut self) -> Result<(Vec<CoreRow>, Option<BaseError>), BaseError> {
        let mut installed = match &self.device {
            Some(device) => device.installed_cores()?,
            None => Vec::new(),
        };
        let (latest, failure) = match self.cores.supported_cores() {
            Ok(latest) => (latest, None),
            Err(e) => (Vec::new(), Some(e)),
        };
        let mut rows = Vec::new();
        for latest in latest {
            let on_device = installed.iter().position(|c| c.id == latest.id);
            rows.push(match on_device {
                Some(i) => CoreRow::Both {
                    installed: installed.remove(i),
                    latest,
                },
                None => CoreRow::Available(latest),
            });
        }
        rows.extend(installed.into_iter().map(CoreRow::Installed));
        Ok((rows, failure))
    }

    /// The cores on the device with another version in the inventory.
    pub fn cores_with_updates(&mut self) -> Result<Vec<String>, BaseError> {
        let (rows, failure) = self.core_rows()?;
        if let Some(failure) = failure {
            return Err(failure);
        }
        let with_updates = rows.iter().filter(|r| r.has_update());
        Ok(with_updates.map(|r| r.id().to_string()).collect())
    }

    /// Puts the newest release of the core on the device, unless it is there.
    fn install_core(&mut self, id: &str) -> Result<InstallCoreOutcome, InstallCoreError> {
        if !SUPPORTED.contains(&id) {
            return Err(InstallCoreError::NotSupported);
        }
        let device = self.device.as_ref().ok_or(BaseError::NoDevice)?;
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

    fn remove_core(&mut self, id: &str) -> Result<(), RemoveCoreError> {
        let device = self.device.as_ref().ok_or(BaseError::NoDevice)?;
        device.remove_core(id)
    }

    /// The files the cores on the device load.
    pub fn bios_rows(&self) -> Result<Vec<BiosRow>, BaseError> {
        self.device.as_ref().ok_or(BaseError::NoDevice)?.bios_rows()
    }

    /// Copies the file at `from` to where `row` says. The file stays where it
    /// is.
    fn put_bios_file(&mut self, row: &BiosRow, from: &Path) -> Result<(), PutBiosError> {
        let device = self.device.as_ref().ok_or(BaseError::NoDevice)?;
        device.put_bios(&row.platform, &row.path, from)
    }

    /// The saves in the library with no game.
    pub fn orphan_saves_in_library(&self) -> Result<Vec<OrphanLocalSave>, BaseError> {
        self.library.orphan_saves()
    }

    /// The saves on the device with no game; none if no device is open.
    pub fn orphan_saves_on_device(&self) -> Result<Vec<OrphanDeviceSave>, BaseError> {
        self.device
            .as_ref()
            .map_or(Ok(Vec::new()), Device::orphan_saves)
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
    pub fn games_on_device(&self) -> Vec<DeviceEntry> {
        let entries = self.device.as_ref().map_or(&[][..], |d| d.entries());
        entries.to_vec()
    }

    // Each of these does one game at a time, notes it in the log and hands
    // the entry to `each` before going on to the next: an error for one doesn't
    // stop the others, but `each` can, by breaking: the game under way is
    // always finished. `each` also sees how things stand now. They return
    // whether it broke, or the error if the log can't be written, which stops
    // them too.

    fn for_each_game<G: Clone, T, E>(
        &mut self,
        games: &[G],
        mut op: impl FnMut(&mut Self, &G) -> Result<T, E>,
        event: impl Fn(G, Result<T, E>) -> LogEvent,
        mut each: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        for game in games {
            let done = op(self, game);
            let (entry, written) = self.logger.record(event(game.clone(), done));
            let flow = each(self, entry);
            written?;
            if flow.is_break() {
                return Ok(flow);
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Reads the device's games again after `done`, however it ended, if they
    /// have been read at all.
    fn rescan_device_after(&mut self, done: BatchResult) -> BatchResult {
        let read = match &mut self.device {
            Some(device) if device.has_been_scanned() => device.scan().map(|_| ()),
            _ => Ok(()),
        };
        let flow = done?;
        read?;
        Ok(flow)
    }

    // What a core loads is no game, so the games change with the cores.

    /// Puts the newest release of each core on the device.
    pub fn install_cores(
        &mut self,
        ids: &[String],
        each: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        let done = self.for_each_game(ids, |p, id| p.install_core(id), LogEvent::InstallCore, each);
        self.rescan_device_after(done)
    }

    /// Deletes each core from the device for good.
    pub fn remove_cores(
        &mut self,
        ids: &[String],
        each: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        let done = self.for_each_game(ids, |p, id| p.remove_core(id), LogEvent::RemoveCore, each);
        self.rescan_device_after(done)
    }

    /// Calls each game in the library what it comes with, keeping its extension.
    /// The device isn't touched.
    pub fn rename_games_in_library(
        &mut self,
        games: &[(LocalEntry, String)],
        each: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_game(
            games,
            |p, (local, new)| p.rename_game_in_library(local, new),
            |(local, _), done| LogEvent::RenameInLibrary(local, done),
            each,
        )
    }

    /// Calls each game on the device what it comes with, keeping its extension
    /// and its folder. The library isn't touched. Which copy of a game comes
    /// first goes by path, so the device is read again at the end.
    pub fn rename_games_on_device(
        &mut self,
        games: &[(DeviceEntry, String)],
        each: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        let done = self.for_each_game(
            games,
            |p, (on_device, new)| p.rename_game_on_device(on_device, new),
            |(on_device, _), done| LogEvent::RenameOnDevice(on_device, done),
            each,
        );
        self.rescan_device_after(done)
    }

    /// Copies each file to where the row it comes with says.
    pub fn put_bios_files(
        &mut self,
        files: &[(BiosRow, PathBuf)],
        each: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_game(
            files,
            |p, (row, from)| p.put_bios_file(row, from),
            // The log outlives the folder the command was run in.
            |(row, from), done| {
                let from = std::path::absolute(&from).unwrap_or(from);
                LogEvent::PutBios(row, from, done)
            },
            each,
        )
    }

    pub fn pull_games(
        &mut self,
        games: &[DeviceEntry],
        each: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_game(games, Self::pull_game, LogEvent::PullGame, each)
    }

    pub fn pull_saves(
        &mut self,
        games: &[DeviceEntry],
        each: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_game(games, Self::pull_save, LogEvent::PullSave, each)
    }

    pub fn push_games(
        &mut self,
        games: &[LocalEntry],
        each: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_game(games, Self::push_game, LogEvent::PushGame, each)
    }

    pub fn fetch_covers(
        &mut self,
        games: &[LocalEntry],
        each: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_game(games, Self::fetch_cover, LogEvent::FetchCover, each)
    }

    pub fn trash_games_in_library(
        &mut self,
        games: &[LocalEntry],
        each: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_game(
            games,
            Self::trash_game_in_library,
            LogEvent::TrashGame,
            each,
        )
    }

    /// `paths` with each folder replaced by every file under it whose extension
    /// tells its platform; a file stays as it is.
    pub fn expand_import_paths(&self, paths: &[PathBuf]) -> Result<Vec<PathBuf>, BaseError> {
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

    /// Imports each file as a game, for `platform` or else what its extension
    /// says.
    pub fn import_games(
        &mut self,
        files: &[PathBuf],
        platform: Option<&Platform>,
        each: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_game(
            files,
            |p, from| p.import_game(from, platform),
            // The log outlives the folder the command was run in.
            |from, done| {
                let from = std::path::absolute(&from).unwrap_or(from);
                LogEvent::ImportGame(from, done)
            },
            each,
        )
    }

    /// Imports each file as a version of the save of the game it comes with.
    pub fn import_saves(
        &mut self,
        saves: &[(LocalEntry, PathBuf)],
        each: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_game(
            saves,
            |p, (local, from)| p.import_save(local, from),
            // The log outlives the folder the command was run in.
            |(local, from), done| {
                let from = std::path::absolute(&from).unwrap_or(from);
                LogEvent::ImportSave(local, from, done)
            },
            each,
        )
    }

    /// Trashes every version of each game's save.
    pub fn trash_saves_in_library(
        &mut self,
        games: &[LocalEntry],
        each: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_game(
            games,
            Self::trash_every_version_in_library,
            LogEvent::TrashSaveAll,
            each,
        )
    }

    /// Trashes the version of each game's save written at `at`, as
    /// [`Save::timestamp`] writes it.
    pub fn trash_saves_at(
        &mut self,
        games: &[LocalEntry],
        at: &str,
        each: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_game(
            games,
            |p, game| {
                let local = p.current_local_entry(game)?;
                match &saves_written_at(&local, at)[..] {
                    [] => Err(TrashSaveError::NoSuchVersion),
                    [version] => p.trash_save_version(&local, version),
                    _ => Err(TrashSaveError::AmbiguousVersion { at: at.to_string() }),
                }
            },
            |game, done| LogEvent::TrashSaveVersion(game, at.to_string(), done),
            each,
        )
    }

    /// Trashes every version of each orphan's save.
    pub fn trash_orphan_saves_in_library(
        &mut self,
        orphans: &[OrphanLocalSave],
        each: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_game(
            orphans,
            Self::trash_orphan_save_in_library,
            LogEvent::TrashOrphanSave,
            each,
        )
    }

    /// Once the first copy of a game is deleted, the next takes its place, so
    /// the device is read again at the end, however it ended.
    pub fn delete_games_on_device(
        &mut self,
        games: &[DeviceEntry],
        each: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        let done = self.for_each_game(
            games,
            Self::delete_game_on_device,
            LogEvent::DeleteGame,
            each,
        );
        self.rescan_device_after(done)
    }

    pub fn delete_saves_on_device(
        &mut self,
        games: &[DeviceEntry],
        each: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_game(
            games,
            Self::delete_save_on_device,
            LogEvent::DeleteSave,
            each,
        )
    }

    pub fn delete_orphan_saves_on_device(
        &mut self,
        orphans: &[OrphanDeviceSave],
        each: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_game(
            orphans,
            Self::delete_orphan_save_on_device,
            LogEvent::DeleteOrphanSave,
            each,
        )
    }

    /// Pushes the newest version of each game's save.
    pub fn push_saves(
        &mut self,
        games: &[DeviceEntry],
        each: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_game(
            games,
            |p, game| {
                let version = p.newest_save(&game.game)?;
                p.push_save(game, &version)
            },
            LogEvent::PushSave,
            each,
        )
    }

    /// Pushes the version of each game's save written at `at`, as
    /// [`Save::timestamp`] writes it.
    pub fn push_saves_at(
        &mut self,
        games: &[DeviceEntry],
        at: &str,
        each: impl FnMut(&Self, LogEntry) -> ControlFlow<()>,
    ) -> BatchResult {
        self.for_each_game(
            games,
            |p, game| {
                let local = p.library.entry_for(&game.game);
                let local = local.ok_or(BaseError::NotInLibrary)?;
                match &saves_written_at(local, at)[..] {
                    [] => Err(PushSaveError::NoSuchVersion),
                    [version] => p.push_save(game, version),
                    _ => Err(PushSaveError::AmbiguousVersion { at: at.to_string() }),
                }
            },
            LogEvent::PushSave,
            each,
        )
    }

    /// The newest version of `game`'s save in the library.
    fn newest_save(&self, game: &Game) -> Result<Save, PushSaveError> {
        let entry = self.library.entry_for(game);
        let entry = entry.ok_or(BaseError::NotInLibrary)?;
        let history = entry.save.as_ref().ok_or(PushSaveError::NoSuchVersion)?;
        Ok(history.current.clone())
    }

    /// `entry` as the device has it now, which may not be what the caller saw.
    fn current_device_entry(&self, entry: &DeviceEntry) -> Result<DeviceEntry, BaseError> {
        let device = self.device.as_ref().ok_or(BaseError::NoDevice)?;
        let current = device.entry_at(&entry.game.platform, &entry.path);
        current.cloned().ok_or(BaseError::NotOnDevice)
    }

    /// `entry` as the library has it now, which may not be what the caller
    /// saw.
    fn current_local_entry(&self, entry: &LocalEntry) -> Result<LocalEntry, BaseError> {
        let current = self.library.entry_named(&entry.game.platform, &entry.name);
        current.cloned().ok_or(BaseError::NotInLibrary)
    }
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

        fn version(&self, _: &str) -> Result<String, BaseError> {
            Ok("1".into())
        }

        fn names(&self, _: &str, _: &str) -> Result<Vec<String>, BaseError> {
            Ok(vec!["Tetris (World)".into(), "My Hack".into()])
        }

        fn cover_name(&self, game_name: &str) -> String {
            game_name.into()
        }

        fn fetch(&self, _: &str, _: &str) -> Result<Cover, BaseError> {
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
        fn inventory(&self) -> Result<Vec<InventoryCore>, BaseError> {
            Ok(vec![InventoryCore {
                id: "Spiritualized.GB".into(),
                version: "2.0".into(),
                platforms: vec![Platform::Gb],
                download_url: "core.zip".into(),
            }])
        }

        fn download(&self, url: &str) -> Result<Vec<u8>, BaseError> {
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

    /// What `run` hands its `each`, as the lines a front end would show.
    fn lines_of(
        run: impl FnOnce(&mut dyn FnMut(&Tested, LogEntry) -> ControlFlow<()>) -> BatchResult,
    ) -> Vec<String> {
        let mut lines = Vec::new();
        let _ = run(&mut |_, entry| {
            lines.push(entry.event.to_line());
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
                Err(PushGameError::Other(BaseError::NoDevice))
            ));
            assert!(matches!(
                s.pocketeer.pull_game(&on_device),
                Err(AddGameError::Other(BaseError::NoDevice))
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
                Err(PullSaveError::Other(BaseError::NotOnDevice))
            ));
            fs::remove_file(s.pocketeer.library().game_path(&local)).unwrap();
            s.pocketeer.reload_library().unwrap();
            assert!(matches!(
                s.pocketeer.fetch_cover(&local),
                Err(FetchCoverError::Other(BaseError::NotInLibrary))
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
                s.pocketeer.trash_save_version_in_library(&local, &gone),
                Err(TrashSaveError::NoSuchVersion)
            ));
            assert!(matches!(
                s.pocketeer.delete_save_on_device(&on_device),
                Err(DeleteSaveError::NoSave)
            ));
            let lines = lines_of(|each| s.pocketeer.push_saves(&[on_device], each));
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
                    seen.push((entry.event.subject(), entry.event.is_ok()));
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
            let flow = s.pocketeer.push_games(&games, |_, entry| {
                seen.push(entry.event.subject());
                ControlFlow::Break(())
            });
            assert!(flow.unwrap().is_break());
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
            let lines = lines_of(|each| s.pocketeer.fetch_covers(&games, each));
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
        fn pushes_newest_by_default() {
            let (mut s, on_device, _) = with_versions();
            let root = s.pocketeer.library().root().to_path_buf();
            put_at(&root, "saves/gba/A/two.sav", b"two", 2_000);
            s.pocketeer.reload_library().unwrap();
            let lines = lines_of(|each| s.pocketeer.push_saves(&on_device[..1], each));
            assert_eq!(lines, ["gba/A.gba: save 1970-01-01T00-33-20 pushed"]);
        }

        #[test]
        fn pushes_version_written_at_given_time() {
            let (mut s, on_device, _) = with_versions();
            let at = "1970-01-01T00-16-40";
            let lines = lines_of(|each| s.pocketeer.push_saves_at(&on_device[..1], at, each));
            assert_eq!(lines, ["gba/A.gba: save 1970-01-01T00-16-40 pushed"]);
        }

        #[test]
        fn reports_game_without_version_at_given_time() {
            let (mut s, on_device, _) = with_versions();
            let at = "1970-01-01T00-16-40";
            let lines = lines_of(|each| s.pocketeer.push_saves_at(&on_device[1..], at, each));
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
            let lines = lines_of(|each| s.pocketeer.push_saves_at(&on_device[..1], at, each));
            assert_eq!(lines, refused);
            let lines = lines_of(|each| s.pocketeer.trash_saves_at(&local[..1], at, each));
            assert_eq!(lines, refused);
            assert!(file.exists() && first.exists());
        }

        #[test]
        fn logs_missing_version_when_trashing() {
            let (mut s, _, local) = with_versions();
            let lines = lines_of(|each| s.pocketeer.trash_saves_at(&local, "never", each));
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
            let lines = lines_of(|each| s.pocketeer.import_saves(&saves, each));
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
            let lines = lines_of(|each| s.pocketeer.import_saves(&saves, each));
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
            let lines = lines_of(|each| s.pocketeer.import_games(&files, None, each));
            assert_eq!(lines, ["tetris.gba: imported as gba/Tetris (World).gba"]);
            assert_eq!(names(&s.pocketeer), ["Tetris (World).gba"]);
            assert!(from.exists());
        }

        #[test]
        fn skips_game_in_library_and_reports_its_name() {
            let mut s = setup(&[], &[("roms/gba/Mine.gba", b"hello")]);
            let (_dir, from) = apart("other name.gba", b"hello");
            let files = [from];
            let lines = lines_of(|each| s.pocketeer.import_games(&files, None, each));
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
            let lines = lines_of(|each| s.pocketeer.import_games(&files, None, each));
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
            let s = setup(&[], &[]);
            let dir = tempfile::tempdir().unwrap();
            let game = put(dir.path(), "Mine.gba", b"mine");
            let deep = put(dir.path(), "old/gb/Deep.GB", b"deep");
            put(dir.path(), "README.txt", b"read me");
            put(dir.path(), "old/.Hidden.gba", b"hidden");
            let apart = tempfile::tempdir().unwrap();
            let lone = put(apart.path(), "lone.txt", b"lone");
            let paths = [dir.path().to_path_buf(), lone.clone()];
            assert_eq!(
                s.pocketeer.expand_import_paths(&paths).unwrap(),
                [game, deep, lone]
            );
        }

        #[test]
        fn prefers_given_platform_over_extension() {
            let mut s = setup(&[], &[]);
            let (_dir, from) = apart("x.gba", b"x");
            let files = [from];
            lines_of(|each| s.pocketeer.import_games(&files, Some(&Platform::Gb), each));
            let root = s.pocketeer.library().root();
            assert!(root.join("roms/gb/x.gba").exists());
            assert!(!root.join("roms/gba").exists());
        }

        #[test]
        fn requires_platform_for_unknown_extension() {
            let mut s = setup(&[], &[]);
            let (_dir, from) = apart("x.bin", b"x");
            let files = [from.clone()];
            let lines = lines_of(|each| s.pocketeer.import_games(&files, None, each));
            assert_eq!(lines, ["x.bin: can't tell which platform it is for"]);
            assert_eq!(names(&s.pocketeer), Vec::<String>::new());
            lines_of(|each| {
                s.pocketeer
                    .import_games(&files, Some(&Platform::Genesis), each)
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
                let lines =
                    lines_of(|each| s.pocketeer.import_games(&files, Some(&platform), each));
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
            let lines = lines_of(|each| s.pocketeer.import_games(&files, None, each));
            assert_eq!(
                lines,
                [".hidden.gba: not imported: the library can't hold a game of that name"]
            );
            let root = s.pocketeer.library().root();
            assert!(!root.join("roms/gba/.hidden.gba").exists());
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
            let done = s.pocketeer.push_games(&games, |_, entry| {
                seen.push(entry.event.subject());
                ControlFlow::Continue(())
            });
            assert!(matches!(done, Err(BaseError::LogFailed { .. })));
            assert_eq!(seen, ["gba/A.gba"]);
            assert_eq!(s.pocketeer.games_on_device().len(), 1);
        }

        #[test]
        fn returns_failure_when_success_cannot_be_logged() {
            let (mut s, on_device, local) = with_versions();
            let version = local[0].save.as_ref().unwrap().current.clone();
            fs::create_dir(s.pocketeer.library().layout().log()).unwrap();
            assert!(matches!(
                s.pocketeer.push_save_version(&on_device[0], &version),
                Err(PushSaveError::Other(BaseError::LogFailed { .. }))
            ));
            assert!(s.pocketeer.games_on_device()[0].save.is_some());
        }

        #[test]
        fn returns_original_failure_when_log_unwritable() {
            let mut s = setup(&[], &[]);
            s.pocketeer.close_device();
            fs::create_dir(s.pocketeer.library().layout().log()).unwrap();
            assert!(matches!(
                s.pocketeer.eject_device(),
                Err(BaseError::NoDevice)
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

        #[test]
        fn renames_in_library_and_leaves_device() {
            let (mut s, local, _) = on_both();
            let games = [(local, "New".to_string())];
            let lines = lines_of(|each| s.pocketeer.rename_games_in_library(&games, each));
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
            let lines = lines_of(|each| s.pocketeer.rename_games_on_device(&games, each));
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
            let lines = lines_of(|each| s.pocketeer.rename_games_in_library(&games, each));
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
            lines_of(|each| s.pocketeer.rename_games_in_library(&games, each));
            let lines = lines_of(|each| s.pocketeer.rename_games_in_library(&games, each));
            assert_eq!(
                lines,
                ["gba/Old.gba: error: the library doesn't have the game"]
            );
            s.pocketeer.close_device();
            let games = [(on_device, "New".to_string())];
            let lines = lines_of(|each| s.pocketeer.rename_games_on_device(&games, each));
            assert_eq!(lines, ["gba/Old.gba: error: no device is open"]);
        }
    }

    mod bios {
        use super::*;

        const GB: &str = "Spiritualized.GB";

        /// Its `core.zip` declares `gba_bios.bin`.
        fn install(s: &mut Setup) {
            let ids = [GB.to_string()];
            lines_of(|each| s.pocketeer.install_cores(&ids, each));
        }

        fn remove(s: &mut Setup) {
            let ids = [GB.to_string()];
            lines_of(|each| s.pocketeer.remove_cores(&ids, each));
        }

        fn games(s: &Setup) -> Vec<PathBuf> {
            let games = s.pocketeer.games_on_device();
            games.into_iter().map(|g| g.path).collect()
        }

        #[test]
        fn returns_error_without_device() {
            let mut s = setup(&[], &[]);
            s.pocketeer.close_device();
            assert!(matches!(s.pocketeer.bios_rows(), Err(BaseError::NoDevice)));
        }

        #[test]
        fn puts_file_and_logs_source() {
            let mut s = setup(&[], &[]);
            install(&mut s);
            let row = s.pocketeer.bios_rows().unwrap().remove(0);
            let elsewhere = tempfile::tempdir().unwrap();
            let from = put(elsewhere.path(), "mine.bin", b"four");
            let files = [(row, from.clone())];
            let lines = lines_of(|each| s.pocketeer.put_bios_files(&files, each));
            assert_eq!(lines, ["gba/gba_bios.bin: put on the device"]);
            let logged = log_lines(&s);
            let last = logged.last().unwrap();
            assert_eq!(last["ok"], true);
            assert_eq!(last["from"], from.display().to_string());
            let row = s.pocketeer.bios_rows().unwrap().remove(0);
            assert_eq!(row.status, BiosStatus::Present);
        }

        #[test]
        fn logs_refused_file_as_failure() {
            let mut s = setup(&[], &[]);
            install(&mut s);
            let row = s.pocketeer.bios_rows().unwrap().remove(0);
            let elsewhere = tempfile::tempdir().unwrap();
            let files = [(row, put(elsewhere.path(), "mine.bin", b"five!"))];
            let lines = lines_of(|each| s.pocketeer.put_bios_files(&files, each));
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
            let (rows, failure) = s.pocketeer.core_rows().unwrap();
            assert!(failure.is_none());
            let found: Vec<_> = rows.iter().map(|r| (r.id(), r.has_update())).collect();
            assert_eq!(found, [(GB, true), ("someone.Else", false)]);
            assert_eq!(s.pocketeer.cores_with_updates().unwrap(), [GB]);
        }

        #[test]
        fn lists_every_core_as_available_without_device() {
            let mut s = setup(&[("Cores/Spiritualized.GB/core.json", b"{}")], &[]);
            s.pocketeer.close_device();
            let (rows, _) = s.pocketeer.core_rows().unwrap();
            assert!(matches!(rows[..], [CoreRow::Available(_)]));
        }

        #[test]
        fn installs_and_logs_core() {
            let mut s = setup(&[], &[]);
            let lines = lines_of(|each| s.pocketeer.install_cores(&ids(&[GB]), each));
            assert_eq!(lines, ["Spiritualized.GB: 2.0 installed"]);
            let logged = log_lines(&s);
            assert_eq!(logged[0]["game"], GB);
            assert_eq!(logged[0]["version"], "2.0");
            let (rows, _) = s.pocketeer.core_rows().unwrap();
            assert!(!rows[0].has_update());
            assert!(rows[0].installed().is_some());
        }

        #[test]
        fn does_not_download_core_at_newest_version() {
            let (mut s, downloaded) = downloads_counted();
            lines_of(|each| s.pocketeer.install_cores(&ids(&[GB]), each));
            assert_eq!(downloaded.get(), 1);
            let lines = lines_of(|each| s.pocketeer.install_cores(&ids(&[GB]), each));
            assert_eq!(
                lines,
                ["Spiritualized.GB: the device has the newest version"]
            );
            assert_eq!(downloaded.get(), 1);
        }

        #[test]
        fn continues_after_unsupported_core() {
            let mut s = setup(&[], &[]);
            let lines = lines_of(|each| s.pocketeer.install_cores(&ids(&["no.Such", GB]), each));
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
            let lines = lines_of(|each| s.pocketeer.install_cores(&ids(&[GB]), each));
            assert_eq!(lines, ["Spiritualized.GB: error: no device is open"]);
            assert_eq!(downloaded.get(), 0);
        }

        #[test]
        fn removes_core_from_device() {
            let mut s = setup(&[("Cores/a.B/core.json", b"{}")], &[]);
            let lines = lines_of(|each| s.pocketeer.remove_cores(&ids(&["a.B"]), each));
            assert_eq!(lines, ["a.B: removed from the device"]);
        }

        #[test]
        fn reports_core_not_on_device() {
            let mut s = setup(&[], &[]);
            let lines = lines_of(|each| s.pocketeer.remove_cores(&ids(&["a.B"]), each));
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
            lines_of(|each| s.pocketeer.delete_games_on_device(&games, each));
            let left = s.pocketeer.games_on_device();
            assert_eq!(left[0].path, Path::new("RPG/Two.gba"));
        }
    }

    mod orphan_saves {
        use super::*;

        #[test]
        fn deletes_and_logs_orphan_on_device() {
            let mut s = setup(&[("Saves/gba/common/RPG/Gone.sav", b"save")], &[]);
            let orphans = s.pocketeer.orphan_saves_on_device().unwrap();
            let lines = lines_of(|each| s.pocketeer.delete_orphan_saves_on_device(&orphans, each));
            assert_eq!(lines, ["gba/RPG/Gone.sav: save deleted from the device"]);
            assert_eq!(s.pocketeer.orphan_saves_on_device().unwrap(), []);
        }

        #[test]
        fn reports_orphan_gone_from_library() {
            let mut s = setup(&[], &[("saves/gba/Gone/a.sav", b"save")]);
            let orphans = s.pocketeer.orphan_saves_in_library().unwrap();
            let root = s.pocketeer.library().root().to_path_buf();
            fs::remove_file(root.join("saves/gba/Gone/a.sav")).unwrap();
            let lines = lines_of(|each| s.pocketeer.trash_orphan_saves_in_library(&orphans, each));
            assert_eq!(lines, ["gba/Gone: no such save in the library"]);
        }
    }
}
