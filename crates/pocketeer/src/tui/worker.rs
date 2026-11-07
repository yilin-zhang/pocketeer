//! The worker thread, which alone holds the `Pocketeer` and runs one command at
//! a time.

use std::{
    ops::ControlFlow,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use super::path_input::fuzzy_match;
use pocketeer_core::{
    Pocketeer,
    cores::{BiosRow, CoreRow},
    device::{Device, DeviceEntry, DiskSpace, Firmware, OrphanDeviceSave},
    library::{LibrarySave, LocalEntry, OrphanLocalSave},
    log::{FrontEnd, OperationReport, device_ref},
    platform::Platform,
    pocketeer::GameRow,
};

/// How often it checks for a device being plugged in or removed while idle.
const DEVICE_CHECK_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq)]
pub enum WorkerCommand {
    PullGames(Vec<DeviceEntry>),
    PullSaves(Vec<DeviceEntry>),
    PushGames(Vec<LocalEntry>),
    PushSaveVersion(DeviceEntry, LibrarySave),
    FetchCovers(Vec<LocalEntry>),
    TrashGames(Vec<LocalEntry>),
    /// The file, and the platform if one was chosen. Otherwise its extension
    /// decides.
    ImportGame(PathBuf, Option<Platform>),
    ImportSave(LocalEntry, PathBuf),
    ImportCover(LocalEntry, PathBuf),
    TrashSaveVersion(LocalEntry, LibrarySave),
    TrashOrphanSaves(Vec<OrphanLocalSave>),
    DeleteGames(Vec<DeviceEntry>),
    DeleteSaves(Vec<DeviceEntry>),
    DeleteOrphanSaves(Vec<OrphanDeviceSave>),
    SyncDats,
    Reload,
    Eject,
    /// The Cores page was opened, so the worker starts sending the cores.
    ShowCores,
    InstallCores(Vec<String>),
    RemoveCores(Vec<String>),
    /// The file to copy to where the row says.
    PutBios(BiosRow, PathBuf),
    /// Asks what the DATs would rename these games in the library to. The answer
    /// comes as `WorkerMessage::RenamesProposed`.
    ProposeRenames(Vec<LocalEntry>),
    /// Games in the library, and what to call them.
    RenameGamesInLibrary(Vec<(LocalEntry, String)>),
    /// The game on the device, and what to call it.
    RenameOnDevice(DeviceEntry, String),
}

impl WorkerCommand {
    fn label(&self) -> &'static str {
        match self {
            WorkerCommand::PullGames(_) => "pulling games",
            WorkerCommand::PullSaves(_) => "backing up saves",
            WorkerCommand::PushGames(_) => "pushing games",
            WorkerCommand::PushSaveVersion(..) => "pushing a save",
            WorkerCommand::FetchCovers(_) => "downloading covers",
            WorkerCommand::ImportGame(..) => "importing games",
            WorkerCommand::ImportSave(..) => "importing a save",
            WorkerCommand::ImportCover(..) => "importing a cover",
            WorkerCommand::TrashGames(_) => "putting games in the trash",
            WorkerCommand::TrashSaveVersion(..) => "putting a save in the trash",
            WorkerCommand::TrashOrphanSaves(_) => "putting saves in the trash",
            WorkerCommand::DeleteGames(_) => "deleting games",
            WorkerCommand::DeleteSaves(_) | WorkerCommand::DeleteOrphanSaves(_) => "deleting saves",
            WorkerCommand::SyncDats => "updating the DATs",
            WorkerCommand::Reload => "reading everything again",
            WorkerCommand::Eject => "ejecting the device",
            WorkerCommand::ShowCores => "reading the cores",
            WorkerCommand::ProposeRenames(_) => "finding standard names",
            WorkerCommand::InstallCores(_) => "installing cores",
            WorkerCommand::RemoveCores(_) => "removing cores",
            WorkerCommand::PutBios(..) => "putting a file on the device",
            WorkerCommand::RenameGamesInLibrary(_) => "renaming games",
            WorkerCommand::RenameOnDevice(..) => "renaming a game",
        }
    }

    fn target_count(&self) -> usize {
        match self {
            // These hold a list, and each item is one target. They take several
            // arms only because the lists hold different types.
            WorkerCommand::PullGames(games)
            | WorkerCommand::PullSaves(games)
            | WorkerCommand::DeleteGames(games)
            | WorkerCommand::DeleteSaves(games) => games.len(),
            WorkerCommand::PushGames(games)
            | WorkerCommand::FetchCovers(games)
            | WorkerCommand::TrashGames(games) => games.len(),
            WorkerCommand::TrashOrphanSaves(orphan_saves) => orphan_saves.len(),
            WorkerCommand::DeleteOrphanSaves(orphan_saves) => orphan_saves.len(),
            WorkerCommand::InstallCores(ids) | WorkerCommand::RemoveCores(ids) => ids.len(),
            WorkerCommand::RenameGamesInLibrary(games) => games.len(),

            // A folder can hold many games, but that isn't known until it is
            // read. `run_command` replaces this count after reading it.
            WorkerCommand::ImportGame(..) => 1,

            // These act on one thing, so they report one step.
            WorkerCommand::PushSaveVersion(..)
            | WorkerCommand::PutBios(..)
            | WorkerCommand::RenameOnDevice(..)
            | WorkerCommand::ImportSave(..)
            | WorkerCommand::ImportCover(..)
            | WorkerCommand::TrashSaveVersion(..)
            | WorkerCommand::Eject => 1,

            // These go through many files or platforms, but report one result
            // when they end.
            WorkerCommand::SyncDats | WorkerCommand::Reload => 1,

            // `run_command` returns before it needs a count for these.
            WorkerCommand::ShowCores | WorkerCommand::ProposeRenames(_) => 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum GamesTableRow {
    Game(GameRow),
    OrphanSaveInLibrary(OrphanLocalSave),
    OrphanSaveOnDevice(OrphanDeviceSave),
}

impl GamesTableRow {
    /// The game's copy in the library, if this is a game and it has one.
    pub fn local_entry(&self) -> Option<&LocalEntry> {
        match self {
            GamesTableRow::Game(row) => row.local_entry(),
            _ => None,
        }
    }

    /// The game's copy on the device, if this is a game and it has one.
    pub fn device_entry(&self) -> Option<&DeviceEntry> {
        match self {
            GamesTableRow::Game(row) => row.device_entry(),
            _ => None,
        }
    }

    pub fn orphan_in_library(&self) -> Option<&OrphanLocalSave> {
        match self {
            GamesTableRow::OrphanSaveInLibrary(orphan) => Some(orphan),
            _ => None,
        }
    }

    pub fn orphan_on_device(&self) -> Option<&OrphanDeviceSave> {
        match self {
            GamesTableRow::OrphanSaveOnDevice(orphan) => Some(orphan),
            _ => None,
        }
    }

    /// The row's name in the table, which is also what a search matches.
    pub fn name(&self) -> String {
        match self {
            GamesTableRow::Game(game_row) => match game_row {
                GameRow::InLibrary(local) | GameRow::Both { local, .. } => local.name.clone(),
                GameRow::OnDevice(on_device) => on_device.path.display().to_string(),
            },
            GamesTableRow::OrphanSaveInLibrary(orphan) => orphan.stem.clone(),
            GamesTableRow::OrphanSaveOnDevice(orphan) => orphan.path.display().to_string(),
        }
    }

    /// Whether a search for `query` finds the row, by its name or by its path
    /// on the device.
    pub fn matches(&self, query: &str) -> bool {
        let on_device = self.device_entry().map_or(String::new(), |on_device| {
            on_device.path.display().to_string()
        });
        fuzzy_match(query, &self.name()).is_some() || fuzzy_match(query, &on_device).is_some()
    }
}

/// What the Games page draws, as the worker last read it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GamesPageData {
    pub rows: Vec<GamesTableRow>,
    pub device_root: Option<PathBuf>,
}

/// What the Device page draws, as the worker last read it. A value that can't
/// be read is left out, and the page says so.
#[derive(Debug, Clone, PartialEq)]
pub struct DevicePageData {
    pub root: PathBuf,
    pub firmware: Option<Firmware>,
    pub disk_space: Option<DiskSpace>,
    pub core_count: Option<usize>,
    pub game_count: usize,
}

/// What the worker has read and how a command is going, sent to the UI.
#[derive(Debug, Clone, PartialEq)]
pub enum WorkerMessage {
    GamesPageChanged(GamesPageData),
    /// The cores, once their page has been opened, and why the inventory
    /// couldn't be fetched, if it couldn't.
    CoreListChanged {
        rows: Vec<CoreRow>,
        failure: Option<String>,
    },
    /// The files the cores on the device load, or none without a device.
    BiosFilesChanged(Vec<BiosRow>),
    /// None without a device.
    DevicePageChanged(Option<DevicePageData>),
    DeviceReadStarted(PathBuf),
    /// The read ended, and if it failed `CommandFinished` says why.
    DeviceReadFinished,
    CommandStarted {
        label: String,
        target_count: usize,
    },
    /// One target of the command under way is finished.
    CommandProgressed {
        finished_count: usize,
        target_count: usize,
        target: String,
    },
    /// The command is over. `failed` is set if part of it failed.
    CommandFinished {
        summary: String,
        failed: bool,
    },
    /// The answer to `WorkerCommand::ProposeRenames`: the games whose name differs
    /// from their DAT name, each with that name.
    RenamesProposed(Vec<(LocalEntry, String)>),
    /// The copies a scan left out, one line each. Only sent when there are some
    /// and the device was opened or reloaded on request.
    DuplicateGamesSkipped(Vec<String>),
    /// The library couldn't be opened, for this reason, and the worker has
    /// stopped.
    LibraryOpenFailed(String),
}

pub struct QueuedCommand {
    /// Whether it has been canceled. Commands sent between two cancels share
    /// this.
    pub is_canceled: Arc<AtomicBool>,
    pub command: WorkerCommand,
}

/// The UI's end of the worker. Dropping it cancels as [`WorkerHandle::cancel`]
/// does and waits for the thread, so that what it writes gets finished and
/// logged.
pub struct WorkerHandle {
    /// None once it is dropped.
    tx: Option<mpsc::Sender<QueuedCommand>>,
    /// The flag every command sent until the next cancel carries. Once true it
    /// stays true, and a cancel creates a new one for later commands.
    is_canceled: Arc<AtomicBool>,
    /// None once it is dropped, or for a worker with no thread.
    thread: Option<JoinHandle<()>>,
}

impl WorkerHandle {
    /// Starts the thread, which opens the library at `library` and the device
    /// at `device`, or else any device mounted now or later.
    pub fn spawn(
        library: PathBuf,
        device: Option<PathBuf>,
        send_message: impl Fn(WorkerMessage) + Send + 'static,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let thread = thread::spawn(move || run_worker_loop(&library, device, &rx, &send_message));
        Self {
            tx: Some(tx),
            is_canceled: Arc::default(),
            thread: Some(thread),
        }
    }

    /// A worker with no thread, whose commands can be read back.
    #[cfg(test)]
    pub fn recording() -> (WorkerHandle, mpsc::Receiver<QueuedCommand>) {
        let (tx, rx) = mpsc::channel();
        let worker_handle = WorkerHandle {
            tx: Some(tx),
            is_canceled: Arc::default(),
            thread: None,
        };
        (worker_handle, rx)
    }

    pub fn send(&mut self, command: WorkerCommand) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(QueuedCommand {
                is_canceled: self.is_canceled.clone(),
                command,
            });
        }
    }

    /// Stops the running command after its current game and drops the commands
    /// waiting.
    pub fn cancel(&mut self) {
        self.is_canceled.store(true, Ordering::Relaxed);
        self.is_canceled = Arc::default();
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        self.cancel();
        // With nothing left to come, the thread returns.
        drop(self.tx.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run_worker_loop(
    library: &Path,
    device_from_command_line: Option<PathBuf>,
    rx: &mpsc::Receiver<QueuedCommand>,
    send_message: &dyn Fn(WorkerMessage),
) {
    let mut pocketeer = match Pocketeer::open(library, FrontEnd::Tui) {
        Ok(pocketeer) => pocketeer,
        Err(error) => {
            send_message(WorkerMessage::LibraryOpenFailed(error.to_string()));
            return;
        }
    };
    let mut worker_state = WorkerState {
        send_message,
        device_from_command_line,
        last_found_devices: Vec::new(),
        orphan_saves: OrphanSaves::default(),
        is_cores_page_opened: false,
    };
    worker_state.refresh_orphans(&pocketeer);
    worker_state.send_games_page_data(&pocketeer);
    worker_state.sync_mounted_device(&mut pocketeer);
    loop {
        match rx.recv_timeout(DEVICE_CHECK_INTERVAL) {
            Ok(QueuedCommand {
                is_canceled,
                command,
            }) => {
                if !is_canceled.load(Ordering::Relaxed) {
                    worker_state.run_command(&mut pocketeer, command, &is_canceled);
                }
            }
            Err(RecvTimeoutError::Timeout) => worker_state.sync_mounted_device(&mut pocketeer),
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[derive(Default)]
struct OrphanSaves {
    in_library: Vec<OrphanLocalSave>,
    on_device: Vec<OrphanDeviceSave>,
}

struct WorkerState<'a> {
    send_message: &'a dyn Fn(WorkerMessage),
    /// The device given on the command line, if any.
    device_from_command_line: Option<PathBuf>,
    /// The devices found by the last check, to notice changes.
    last_found_devices: Vec<PathBuf>,
    orphan_saves: OrphanSaves,
    /// Whether the Cores page has been opened. Until then nothing goes online
    /// for it.
    is_cores_page_opened: bool,
}

impl WorkerState<'_> {
    fn send_games_page_data(&self, pocketeer: &Pocketeer) {
        (self.send_message)(WorkerMessage::GamesPageChanged(games_page_data(
            pocketeer,
            &self.orphan_saves,
        )));
    }

    /// Looks for orphans only when a command ends, because that reads every
    /// save and the games page may be sent for each game.
    fn refresh_orphans(&mut self, pocketeer: &Pocketeer) {
        self.orphan_saves.in_library = pocketeer.find_orphan_saves_in_library().unwrap_or_default();
        self.orphan_saves.on_device = pocketeer.find_orphan_saves_on_device().unwrap_or_default();
    }

    fn send_cores_page_data(&self, pocketeer: &mut Pocketeer) {
        if !self.is_cores_page_opened {
            return;
        }
        let (rows, failure) = match pocketeer.load_core_rows() {
            Ok((rows, failure)) => (rows, failure.map(|error| error.to_string())),
            Err(error) => (Vec::new(), Some(error.to_string())),
        };
        (self.send_message)(WorkerMessage::CoreListChanged { rows, failure });
    }

    fn send_bios_page_data(&self, pocketeer: &Pocketeer) {
        (self.send_message)(WorkerMessage::BiosFilesChanged(
            pocketeer.read_bios_rows().unwrap_or_default(),
        ));
    }

    fn send_device_page_data(&self, pocketeer: &Pocketeer) {
        let data = pocketeer.device().map(|device| DevicePageData {
            root: device.root().to_path_buf(),
            firmware: device.read_firmware(),
            disk_space: device.read_disk_space().ok(),
            core_count: device.installed_cores().ok().map(|cores| cores.len()),
            game_count: device.entries().len(),
        });
        (self.send_message)(WorkerMessage::DevicePageChanged(data));
    }

    /// Opens a device that appeared and closes one that disappeared.
    fn sync_mounted_device(&mut self, pocketeer: &mut Pocketeer) {
        let found = match &self.device_from_command_line {
            Some(path) if Device::is_device(path) => vec![path.clone()],
            Some(_) => Vec::new(),
            None => Device::find_mounted(),
        };
        let open = pocketeer.device().map(|device| device.root().to_path_buf());
        if found == self.last_found_devices
            && open
                .as_ref()
                .is_none_or(|open_root| found.contains(open_root))
        {
            return;
        }
        self.last_found_devices = found.clone();
        if open
            .as_ref()
            .is_some_and(|open_root| found.contains(open_root))
        {
            return;
        }
        pocketeer.close_device();
        match &found[..] {
            [] => {
                self.finish_with_refresh(pocketeer, "device gone".to_string(), false);
            }
            [root] => self.open_device(pocketeer, root),
            many => {
                let list: Vec<_> = many
                    .iter()
                    .map(|device_root| device_root.display().to_string())
                    .collect();
                let summary = format!("more than one device mounted: {}", list.join(", "));
                self.finish_with_refresh(pocketeer, summary, true);
            }
        }
    }

    fn open_device(&mut self, pocketeer: &mut Pocketeer, root: &Path) {
        (self.send_message)(WorkerMessage::DeviceReadStarted(root.to_path_buf()));
        let opened = pocketeer
            .open_device(root)
            .map_err(|error| error.to_string())
            .and_then(|()| pocketeer.scan_device().map_err(|error| error.to_string()));
        let failure = match opened {
            Ok(report) => {
                send_duplicates(self.send_message, pocketeer, &report.duplicates);
                None
            }
            Err(error) => {
                pocketeer.close_device();
                Some(format!("couldn't read the device: {error}"))
            }
        };
        self.refresh_orphans(pocketeer);
        self.send_games_page_data(pocketeer);
        self.send_cores_page_data(pocketeer);
        self.send_bios_page_data(pocketeer);
        self.send_device_page_data(pocketeer);
        (self.send_message)(WorkerMessage::DeviceReadFinished);
        if let Some(summary) = failure {
            (self.send_message)(WorkerMessage::CommandFinished {
                summary,
                failed: true,
            });
        }
    }

    fn finish_with_refresh(&mut self, pocketeer: &mut Pocketeer, summary: String, failed: bool) {
        self.refresh_orphans(pocketeer);
        self.send_games_page_data(pocketeer);
        self.send_cores_page_data(pocketeer);
        self.send_bios_page_data(pocketeer);
        self.send_device_page_data(pocketeer);
        (self.send_message)(WorkerMessage::CommandFinished { summary, failed });
    }

    fn run_command(
        &mut self,
        pocketeer: &mut Pocketeer,
        command: WorkerCommand,
        is_canceled: &AtomicBool,
    ) {
        if command == WorkerCommand::ShowCores {
            self.is_cores_page_opened = true;
            self.send_cores_page_data(pocketeer);
            return;
        }
        if let WorkerCommand::ProposeRenames(games) = &command {
            (self.send_message)(WorkerMessage::RenamesProposed(
                pocketeer.renames_to_dat_names(games),
            ));
            return;
        }
        let mut command_progress = CommandProgress {
            send_message: self.send_message,
            orphan_saves: &self.orphan_saves,
            is_canceled,
            target_count: command.target_count(),
            reports: Vec::new(),
        };
        let label = command.label();
        (self.send_message)(WorkerMessage::CommandStarted {
            label: label.to_string(),
            target_count: command_progress.target_count,
        });
        // A non-batch command reports through `command_progress` by hand,
        // because it has no log entry, only its result.
        let batch_result = match command {
            WorkerCommand::PullGames(games) => {
                pocketeer.pull_games(&games, |pocketeer, log_entry| {
                    command_progress.record_finished_target(pocketeer, log_entry.report)
                })
            }
            WorkerCommand::PullSaves(games) => {
                pocketeer.pull_saves(&games, |pocketeer, log_entry| {
                    command_progress.record_finished_target(pocketeer, log_entry.report)
                })
            }
            WorkerCommand::PushGames(games) => {
                pocketeer.push_games(&games, |pocketeer, log_entry| {
                    command_progress.record_finished_target(pocketeer, log_entry.report)
                })
            }
            WorkerCommand::PushSaveVersion(game, version) => {
                pocketeer.push_save_versions(&[(game, version)], |pocketeer, log_entry| {
                    command_progress.record_finished_target(pocketeer, log_entry.report)
                })
            }
            WorkerCommand::FetchCovers(games) => {
                pocketeer.fetch_covers(&games, |pocketeer, log_entry| {
                    command_progress.record_finished_target(pocketeer, log_entry.report)
                })
            }
            WorkerCommand::ImportGame(path, platform) => {
                pocketeer_core::pocketeer::expand_import_paths(&[path]).and_then(|files| {
                    command_progress.target_count = files.len();
                    pocketeer.import_games(&files, platform.as_ref(), |pocketeer, log_entry| {
                        command_progress.record_finished_target(pocketeer, log_entry.report)
                    })
                })
            }
            WorkerCommand::ImportSave(game, from) => {
                pocketeer.import_saves(&[(game, from)], |pocketeer, log_entry| {
                    command_progress.record_finished_target(pocketeer, log_entry.report)
                })
            }
            WorkerCommand::ImportCover(game, from) => {
                pocketeer.import_covers(&[(game, from)], |pocketeer, log_entry| {
                    command_progress.record_finished_target(pocketeer, log_entry.report)
                })
            }
            WorkerCommand::TrashGames(games) => {
                pocketeer.trash_games_in_library(&games, |pocketeer, log_entry| {
                    command_progress.record_finished_target(pocketeer, log_entry.report)
                })
            }
            WorkerCommand::TrashSaveVersion(game, version) => pocketeer
                .trash_save_versions_in_library(&[(game, version)], |pocketeer, log_entry| {
                    command_progress.record_finished_target(pocketeer, log_entry.report)
                }),
            WorkerCommand::TrashOrphanSaves(orphan_saves) => pocketeer
                .trash_orphan_saves_in_library(&orphan_saves, |pocketeer, log_entry| {
                    command_progress.record_finished_target(pocketeer, log_entry.report)
                }),
            WorkerCommand::DeleteGames(games) => {
                pocketeer.delete_games_on_device(&games, |pocketeer, log_entry| {
                    command_progress.record_finished_target(pocketeer, log_entry.report)
                })
            }
            WorkerCommand::DeleteSaves(games) => {
                pocketeer.delete_saves_on_device(&games, |pocketeer, log_entry| {
                    command_progress.record_finished_target(pocketeer, log_entry.report)
                })
            }
            WorkerCommand::DeleteOrphanSaves(orphan_saves) => pocketeer
                .delete_orphan_saves_on_device(&orphan_saves, |pocketeer, log_entry| {
                    command_progress.record_finished_target(pocketeer, log_entry.report)
                }),
            // Handled above, since they aren't commands to report.
            WorkerCommand::ShowCores | WorkerCommand::ProposeRenames(_) => {
                Ok(ControlFlow::Continue(()))
            }
            WorkerCommand::InstallCores(ids) => {
                pocketeer.install_cores(&ids, |pocketeer, log_entry| {
                    command_progress.record_finished_target(pocketeer, log_entry.report)
                })
            }
            WorkerCommand::RemoveCores(ids) => {
                pocketeer.remove_cores(&ids, |pocketeer, log_entry| {
                    command_progress.record_finished_target(pocketeer, log_entry.report)
                })
            }
            WorkerCommand::PutBios(row, from) => {
                pocketeer.put_bios_files(&[(row, from)], |pocketeer, log_entry| {
                    command_progress.record_finished_target(pocketeer, log_entry.report)
                })
            }
            WorkerCommand::RenameGamesInLibrary(games) => {
                pocketeer.rename_games_in_library(&games, |pocketeer, log_entry| {
                    command_progress.record_finished_target(pocketeer, log_entry.report)
                })
            }
            WorkerCommand::RenameOnDevice(game, new) => {
                pocketeer.rename_games_on_device(&[(game, new)], |pocketeer, log_entry| {
                    command_progress.record_finished_target(pocketeer, log_entry.report)
                })
            }
            WorkerCommand::SyncDats => {
                let synced = pocketeer.sync_dats();
                Ok(command_progress
                    .record_finished_target(pocketeer, OperationReport::SyncDats(synced)))
            }
            WorkerCommand::Reload => {
                let read = pocketeer.reload();
                let read = read.map(|report| {
                    send_duplicates(self.send_message, pocketeer, &report.duplicates)
                });
                Ok(command_progress
                    .record_finished_target(pocketeer, OperationReport::Reload(read)))
            }
            WorkerCommand::Eject => {
                let root = pocketeer.device().map(|device| device.root().to_path_buf());
                let ejected = pocketeer.eject_device();
                Ok(command_progress
                    .record_finished_target(pocketeer, OperationReport::Eject(root, ejected)))
            }
        };
        let (mut summary, mut failed) = command_summary(
            label,
            command_progress.target_count,
            &command_progress.reports,
        );
        if let Err(error) = batch_result {
            summary.push_str(&format!("; {error}"));
            failed = true;
        }
        self.finish_with_refresh(pocketeer, summary, failed);
    }
}

struct CommandProgress<'a> {
    send_message: &'a dyn Fn(WorkerMessage),
    orphan_saves: &'a OrphanSaves,
    is_canceled: &'a AtomicBool,
    target_count: usize,
    reports: Vec<OperationReport>,
}

impl CommandProgress<'_> {
    /// Records how one game went and sends the current state. Breaks if the
    /// command was canceled.
    fn record_finished_target(
        &mut self,
        pocketeer: &Pocketeer,
        report: OperationReport,
    ) -> ControlFlow<()> {
        let target = report.subject();
        self.reports.push(report);
        (self.send_message)(WorkerMessage::GamesPageChanged(games_page_data(
            pocketeer,
            self.orphan_saves,
        )));
        (self.send_message)(WorkerMessage::CommandProgressed {
            finished_count: self.reports.len(),
            target_count: self.target_count,
            target,
        });
        if self.is_canceled.load(Ordering::Relaxed) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    }
}

/// Summarizes a command labeled `label` meant for `target_count` targets, and
/// says whether any of it failed.
fn command_summary(
    label: &str,
    target_count: usize,
    reports: &[OperationReport],
) -> (String, bool) {
    let finished_count = reports.len();
    let failed = reports.iter().filter(|report| !report.is_ok()).count();
    let first_failure = reports.iter().find(|report| !report.is_ok());
    let summary = match (target_count, first_failure, reports.last()) {
        (1, _, Some(last)) => last.to_line(),
        (_, Some(first), _) => format!(
            "{label}: {finished_count} of {target_count} done, {failed} failed; first: {}",
            first.to_line()
        ),
        _ if finished_count < target_count => {
            format!("{label}: stopped after {finished_count} of {target_count}")
        }
        _ => format!("{label}: {finished_count} done"),
    };
    (summary, failed > 0)
}

fn games_page_data(pocketeer: &Pocketeer, orphan_saves: &OrphanSaves) -> GamesPageData {
    let mut rows: Vec<GamesTableRow> = pocketeer
        .game_rows()
        .into_iter()
        .map(GamesTableRow::Game)
        .collect();
    rows.extend(
        orphan_saves
            .in_library
            .iter()
            .cloned()
            .map(GamesTableRow::OrphanSaveInLibrary),
    );
    rows.extend(
        orphan_saves
            .on_device
            .iter()
            .cloned()
            .map(GamesTableRow::OrphanSaveOnDevice),
    );
    GamesPageData {
        rows,
        device_root: pocketeer.device().map(|device| device.root().to_path_buf()),
    }
}

/// Tells the UI about `duplicates`, the copies a scan left out, if there are
/// any.
fn send_duplicates(
    send_message: &dyn Fn(WorkerMessage),
    pocketeer: &Pocketeer,
    duplicates: &[DeviceEntry],
) {
    if duplicates.is_empty() {
        return;
    }
    let lines = duplicates.iter().map(|on_device| {
        let first = pocketeer
            .device()
            .and_then(|device| device.entry_for(&on_device.game));
        let first = first.map_or(String::new(), device_ref);
        format!("{} is the same game as {first}", device_ref(on_device))
    });
    send_message(WorkerMessage::DuplicateGamesSkipped(lines.collect()));
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::mpsc::Receiver};

    use pocketeer_core::cores::BiosStatus;

    use super::*;

    fn put(root: &Path, path: &str, bytes: &[u8]) -> PathBuf {
        let full = root.join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(&full, bytes).unwrap();
        full
    }

    /// The worker on `library` and `device`, and the receiver of its messages.
    fn worker_on(
        library: PathBuf,
        device: Option<PathBuf>,
    ) -> (WorkerHandle, Receiver<WorkerMessage>) {
        let (tx, rx) = mpsc::channel();
        let worker_handle = WorkerHandle::spawn(library, device, move |message| {
            let _ = tx.send(message);
        });
        (worker_handle, rx)
    }

    /// The summary, and whether it failed, of the command `messages` end with.
    fn finished(messages: &[WorkerMessage]) -> (&str, bool) {
        let Some(WorkerMessage::CommandFinished { summary, failed }) = messages.last() else {
            panic!("{messages:?}")
        };
        (summary, *failed)
    }

    /// An empty library and an empty device in a new folder, with their paths.
    fn empty_library_and_device() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let (library, device) = (dir.path().join("library"), dir.path().join("device"));
        fs::create_dir(&library).unwrap();
        put(&device, "Analogue_Pocket.json", b"{}");
        fs::create_dir(device.join("Cores")).unwrap();
        fs::create_dir(device.join("Assets")).unwrap();
        (dir, library, device)
    }

    /// A library with two games and a device with one, the worker on them.
    fn started() -> (tempfile::TempDir, WorkerHandle, Receiver<WorkerMessage>) {
        let (dir, library, device) = empty_library_and_device();
        put(&library, "roms/gba/A.gba", b"a");
        put(&library, "roms/gba/B.gba", b"b");
        put(&device, "Assets/gba/common/C.gba", b"c");
        put(&device, "Saves/gba/common/Gone.sav", b"orphan");
        let (worker_handle, rx) = worker_on(library, Some(device));
        (dir, worker_handle, rx)
    }

    /// Every message up to the first that `is_last`.
    fn messages_until(
        rx: &Receiver<WorkerMessage>,
        is_last: impl Fn(&WorkerMessage) -> bool,
    ) -> Vec<WorkerMessage> {
        let mut messages = Vec::new();
        loop {
            let message = rx.recv_timeout(Duration::from_secs(10)).unwrap();
            let last = is_last(&message);
            messages.push(message);
            if last {
                return messages;
            }
        }
    }

    /// Every message up to the end of the next command.
    fn until_finished(rx: &Receiver<WorkerMessage>) -> Vec<WorkerMessage> {
        messages_until(rx, |message| {
            matches!(message, WorkerMessage::CommandFinished { .. })
        })
    }

    /// Every message up to the device having been read.
    fn until_device_read(rx: &Receiver<WorkerMessage>) -> Vec<WorkerMessage> {
        messages_until(rx, |message| *message == WorkerMessage::DeviceReadFinished)
    }

    fn last_games_page_data(messages: &[WorkerMessage]) -> &GamesPageData {
        messages
            .iter()
            .rev()
            .find_map(|message| match message {
                WorkerMessage::GamesPageChanged(games_page_data) => Some(games_page_data),
                _ => None,
            })
            .unwrap()
    }

    fn locals(games_page_data: &GamesPageData) -> Vec<LocalEntry> {
        games_page_data
            .rows
            .iter()
            .filter_map(GamesTableRow::local_entry)
            .cloned()
            .collect()
    }

    /// A device with two copies of a game, the worker on it.
    fn started_with_two_copies() -> (tempfile::TempDir, WorkerHandle, Receiver<WorkerMessage>) {
        let (dir, library, device) = empty_library_and_device();
        put(&device, "Assets/gba/common/One.gba", b"c");
        put(&device, "Assets/gba/common/RPG/Two.gba", b"c");
        let (worker_handle, rx) = worker_on(library, Some(device));
        (dir, worker_handle, rx)
    }

    /// A device with the core `a.B` and a freshly written inventory, so it
    /// isn't fetched again, that offers `Spiritualized.GB`.
    fn started_with_cores() -> (tempfile::TempDir, WorkerHandle, Receiver<WorkerMessage>) {
        let (dir, library, device) = empty_library_and_device();
        put(
            &library,
            "cores.json",
            br#"[{"id":"Spiritualized.GB","version":"2.0","platforms":["gb"],"download_url":"http://127.0.0.1:1/never"}]"#,
        );
        put(&device, "Cores/a.B/core.json", b"{}");
        let (worker_handle, rx) = worker_on(library, Some(device));
        (dir, worker_handle, rx)
    }

    /// The ids of the cores last sent in `messages`.
    fn cores_sent(messages: &[WorkerMessage]) -> Option<Vec<String>> {
        messages.iter().rev().find_map(|message| match message {
            WorkerMessage::CoreListChanged { rows, .. } => {
                Some(rows.iter().map(|row| row.id().to_string()).collect())
            }
            _ => None,
        })
    }

    /// Every message up to the cores being sent.
    fn until_cores(rx: &Receiver<WorkerMessage>) -> Vec<WorkerMessage> {
        messages_until(rx, |message| {
            matches!(message, WorkerMessage::CoreListChanged { .. })
        })
    }

    /// A device with a game that can't be read because a folder is where its
    /// save goes, and the worker on it.
    fn started_with_unreadable_device() -> (tempfile::TempDir, WorkerHandle, Receiver<WorkerMessage>)
    {
        let (dir, library, device) = empty_library_and_device();
        put(&device, "Assets/gba/common/One.gba", b"c");
        put(&device, "Saves/gba/common/One.sav/x", b"");
        let (worker_handle, rx) = worker_on(library, Some(device));
        (dir, worker_handle, rx)
    }

    fn duplicates(messages: &[WorkerMessage]) -> Vec<&[String]> {
        messages
            .iter()
            .filter_map(|message| match message {
                WorkerMessage::DuplicateGamesSkipped(lines) => Some(&lines[..]),
                _ => None,
            })
            .collect()
    }

    fn device_page_sent(messages: &[WorkerMessage]) -> Option<&DevicePageData> {
        messages
            .iter()
            .rev()
            .find_map(|message| match message {
                WorkerMessage::DevicePageChanged(data) => Some(data.as_ref()),
                _ => None,
            })
            .flatten()
    }

    /// The files last sent in `messages`.
    fn bios_sent(messages: &[WorkerMessage]) -> &[BiosRow] {
        let told = messages.iter().rev().find_map(|message| match message {
            WorkerMessage::BiosFilesChanged(rows) => Some(rows),
            _ => None,
        });
        told.unwrap()
    }

    mod opening {
        use super::*;

        #[test]
        fn reports_duplicates_on_open_and_reload() {
            let (_dir, mut worker_handle, rx) = started_with_two_copies();
            let lines = ["gba/RPG/Two.gba is the same game as gba/One.gba".to_string()];
            let opening = until_device_read(&rx);
            assert_eq!(duplicates(&opening), [&lines[..]]);
            assert_eq!(last_games_page_data(&opening).rows.len(), 1);
            worker_handle.send(WorkerCommand::Reload);
            assert_eq!(duplicates(&until_finished(&rx)), [&lines[..]]);
        }

        #[test]
        fn sends_device_page_after_reading_device() {
            let (dir, _worker, rx) = started();
            let messages = until_device_read(&rx);
            let data = device_page_sent(&messages).unwrap();
            assert_eq!(data.root, dir.path().join("device"));
            assert_eq!(data.firmware, None);
            assert_eq!(data.core_count, Some(0));
            assert_eq!(data.game_count, 1);
            assert!(data.disk_space.unwrap().total_bytes > 0);
        }

        #[test]
        fn sends_device_page_after_command() {
            let (dir, mut worker_handle, rx) = started();
            until_device_read(&rx);
            put(&dir.path().join("device"), "Assets/gba/common/D.gba", b"d");
            worker_handle.send(WorkerCommand::Reload);
            let messages = until_finished(&rx);
            assert_eq!(device_page_sent(&messages).unwrap().game_count, 2);
        }

        #[test]
        fn reports_only_failure_when_library_cannot_open() {
            let dir = tempfile::tempdir().unwrap();
            let (_worker, rx) = worker_on(dir.path().join("typo"), None);
            let told = rx.recv_timeout(Duration::from_secs(10)).unwrap();
            assert!(
                matches!(told, WorkerMessage::LibraryOpenFailed(_)),
                "{told:?}"
            );
            // The worker has gone, and its end of the channel with it.
            assert!(rx.recv_timeout(Duration::from_secs(10)).is_err());
        }

        #[test]
        fn opens_device_and_sends_every_row() {
            let (_dir, _worker, rx) = started();
            let opening = until_device_read(&rx);
            let games_page_data = last_games_page_data(&opening);
            assert!(games_page_data.device_root.is_some());
            // A, B, C and the orphan save.
            assert_eq!(games_page_data.rows.len(), 4);
            assert!(matches!(
                games_page_data.rows[3],
                GamesTableRow::OrphanSaveOnDevice(_)
            ));
            assert!(matches!(opening[1], WorkerMessage::DeviceReadStarted(_)));
            // Reading it successfully isn't a command to report.
            let is_command_message = |message: &WorkerMessage| {
                matches!(
                    message,
                    WorkerMessage::CommandStarted { .. } | WorkerMessage::CommandFinished { .. }
                )
            };
            assert!(!opening.iter().any(is_command_message));
        }

        #[test]
        fn reports_why_device_cannot_be_read() {
            let (_dir, _worker, rx) = started_with_unreadable_device();
            let messages = until_finished(&rx);
            let (summary, failed) = finished(&messages);
            assert!(failed);
            assert!(
                summary.starts_with("couldn't read the device: "),
                "{summary}"
            );
            assert!(messages.contains(&WorkerMessage::DeviceReadFinished));
            assert_eq!(last_games_page_data(&messages).device_root, None);
        }
    }

    mod commands {
        use super::*;

        #[test]
        fn reports_progress_for_each_game() {
            let (_dir, mut worker_handle, rx) = started();
            let opening = until_device_read(&rx);
            let games = locals(last_games_page_data(&opening));
            worker_handle.send(WorkerCommand::PushGames(games));
            let messages = until_finished(&rx);
            let progress: Vec<_> = messages
                .iter()
                .filter_map(|message| match message {
                    WorkerMessage::CommandProgressed {
                        finished_count,
                        target_count,
                        target,
                    } => Some((*finished_count, *target_count, target.as_str())),
                    _ => None,
                })
                .collect();
            assert_eq!(progress, [(1, 2, "gba/A.gba"), (2, 2, "gba/B.gba")]);
            assert_eq!(finished(&messages), ("pushing games: 2 done", false));
            let games_page_data = last_games_page_data(&messages);
            let on_device = games_page_data
                .rows
                .iter()
                .filter(|row| row.device_entry().is_some());
            assert_eq!(on_device.count(), 3);
        }

        #[test]
        fn passes_chosen_platform_to_import() {
            let (dir, mut worker_handle, rx) = started();
            let opening = until_device_read(&rx);
            let before = locals(last_games_page_data(&opening)).len();
            let from = put(dir.path(), "elsewhere/New.bin", b"new");
            worker_handle.send(WorkerCommand::ImportGame(from, Some(Platform::Gb)));
            let messages = until_finished(&rx);
            assert!(!finished(&messages).1);
            assert_eq!(locals(last_games_page_data(&messages)).len(), before + 1);
        }

        #[test]
        fn counts_games_in_imported_dir() {
            let (dir, mut worker_handle, rx) = started();
            until_device_read(&rx);
            put(dir.path(), "elsewhere/New.gb", b"new");
            put(dir.path(), "elsewhere/deep/Other.gb", b"other");
            worker_handle.send(WorkerCommand::ImportGame(
                dir.path().join("elsewhere"),
                None,
            ));
            let messages = until_finished(&rx);
            assert_eq!(finished(&messages), ("importing games: 2 done", false));
        }

        #[test]
        fn refreshes_orphan_saves_after_deleting_one() {
            let (_dir, mut worker_handle, rx) = started();
            let opening = until_device_read(&rx);
            let GamesTableRow::OrphanSaveOnDevice(orphan) =
                last_games_page_data(&opening).rows[3].clone()
            else {
                panic!()
            };
            worker_handle.send(WorkerCommand::DeleteOrphanSaves(vec![orphan]));
            let messages = until_finished(&rx);
            assert_eq!(
                finished(&messages),
                ("gba/Gone.sav: save deleted from the device", false)
            );
            assert_eq!(last_games_page_data(&messages).rows.len(), 3);
        }

        #[test]
        fn proposes_dat_names_without_reporting_command() {
            let (_dir, library, device) = empty_library_and_device();
            put(
                &library,
                "dats/Nintendo - Game Boy Advance.dat",
                br#"clrmamepro ( name "Nintendo - Game Boy Advance" )
game ( name "Tetris (World)" rom ( name "Tetris (World).gba" size 5 crc 3610A686 ) )"#,
            );
            put(&library, "roms/gba/Old.gba", b"hello");
            put(&library, "roms/gba/Mine.gba", b"unknown");
            let (mut worker_handle, rx) = worker_on(library, Some(device));
            let opening = until_device_read(&rx);
            worker_handle.send(WorkerCommand::ProposeRenames(locals(last_games_page_data(
                &opening,
            ))));
            let messages = messages_until(&rx, |message| {
                matches!(message, WorkerMessage::RenamesProposed(_))
            });
            let [WorkerMessage::RenamesProposed(renames)] = &messages[..] else {
                panic!("{messages:?}")
            };
            let [(game, new)] = &renames[..] else {
                panic!("{renames:?}")
            };
            assert_eq!(
                (game.name.as_str(), new.as_str()),
                ("Old.gba", "Tetris (World)")
            );
        }

        #[test]
        fn reports_bios_file_and_puts_it_on_device() {
            let (dir, library, device) = empty_library_and_device();
            put(
                &device,
                "Cores/a.B/core.json",
                br#"{"core":{"metadata":{"version":"1","platform_ids":["gba"]}}}"#,
            );
            put(
                &device,
                "Cores/a.B/data.json",
                br#"{"data":{"data_slots":[{"filename":"gba_bios.bin","required":true}]}}"#,
            );
            let from = put(dir.path(), "mine.bin", b"bios");
            let (mut worker_handle, rx) = worker_on(library, Some(device.clone()));
            let opening = until_device_read(&rx);
            let told = bios_sent(&opening);
            assert_eq!(told[0].status, BiosStatus::Missing);
            worker_handle.send(WorkerCommand::PutBios(told[0].clone(), from));
            let messages = until_finished(&rx);
            assert_eq!(
                finished(&messages),
                ("gba/gba_bios.bin: put on the device", false)
            );
            assert_eq!(bios_sent(&messages)[0].status, BiosStatus::Present);
            assert!(device.join("Assets/gba/common/gba_bios.bin").exists());
        }

        #[test]
        fn sends_cores_only_after_page_opens() {
            let (_dir, mut worker_handle, rx) = started_with_cores();
            let opening = until_device_read(&rx);
            assert_eq!(cores_sent(&opening), None);
            worker_handle.send(WorkerCommand::Reload);
            assert_eq!(cores_sent(&until_finished(&rx)), None);
            worker_handle.send(WorkerCommand::ShowCores);
            let told = cores_sent(&until_cores(&rx)).unwrap();
            assert_eq!(told, ["Spiritualized.GB", "a.B"]);
        }

        #[test]
        fn drops_removed_core_from_cores_update() {
            let (_dir, mut worker_handle, rx) = started_with_cores();
            until_device_read(&rx);
            worker_handle.send(WorkerCommand::ShowCores);
            until_cores(&rx);
            worker_handle.send(WorkerCommand::RemoveCores(vec!["a.B".to_string()]));
            let messages = until_finished(&rx);
            assert_eq!(finished(&messages), ("a.B: removed from the device", false));
            assert_eq!(cores_sent(&messages).unwrap(), ["Spiritualized.GB"]);
        }

        #[test]
        fn counts_games_renamed_in_library() {
            let (_dir, mut worker_handle, rx) = started();
            let opening = until_device_read(&rx);
            let renames = locals(last_games_page_data(&opening))
                .into_iter()
                .map(|local| {
                    let new = format!("New {}", local.stem());
                    (local, new)
                })
                .collect();
            worker_handle.send(WorkerCommand::RenameGamesInLibrary(renames));
            let messages = until_finished(&rx);
            assert_eq!(finished(&messages), ("renaming games: 2 done", false));
            let names: Vec<String> = locals(last_games_page_data(&messages))
                .into_iter()
                .map(|local| local.name)
                .collect();
            assert_eq!(names, ["New A.gba", "New B.gba"]);
        }

        #[test]
        fn logs_commands_with_tui_front_end() {
            let (dir, mut worker_handle, rx) = started();
            let opening = until_device_read(&rx);
            let games = locals(last_games_page_data(&opening));
            worker_handle.send(WorkerCommand::PushGames(games));
            until_finished(&rx);
            let log = fs::read_to_string(dir.path().join("library/pocketeer.log")).unwrap();
            assert!(log.contains(r#""front_end":"tui""#), "{log}");
        }

        #[test]
        fn stops_command_and_reports_when_log_cannot_be_written() {
            let (dir, mut worker_handle, rx) = started();
            let opening = until_device_read(&rx);
            let games = locals(last_games_page_data(&opening));
            fs::create_dir(dir.path().join("library/pocketeer.log")).unwrap();
            worker_handle.send(WorkerCommand::PushGames(games));
            let messages = until_finished(&rx);
            let done = messages
                .iter()
                .filter(|message| matches!(message, WorkerMessage::CommandProgressed { .. }))
                .count();
            assert_eq!(done, 1);
            let (summary, failed) = finished(&messages);
            assert!(failed);
            assert!(summary.contains("couldn't write the log"), "{summary}");
        }

        #[test]
        fn summarizes_how_command_went() {
            let ok = || OperationReport::Reload(Ok(()));
            let bad = || OperationReport::Reload(Err(pocketeer_core::error::BasicError::NoDevice));
            assert_eq!(
                command_summary("reading", 1, &[ok()]),
                ("everything: read again".to_string(), false)
            );
            assert_eq!(
                command_summary("reading", 1, &[bad()]),
                ("everything: error: no device is open".to_string(), true)
            );
            assert_eq!(
                command_summary("reading", 2, &[ok(), ok()]),
                ("reading: 2 done".to_string(), false)
            );
            assert_eq!(
                command_summary("reading", 3, &[ok()]),
                ("reading: stopped after 1 of 3".to_string(), false)
            );
            assert_eq!(
                command_summary("reading", 2, &[ok(), bad()]),
                (
                    "reading: 2 of 2 done, 1 failed; first: everything: error: no device is open"
                        .to_string(),
                    true
                )
            );
        }
    }

    mod canceling {
        use super::*;

        #[test]
        fn skips_canceled_commands() {
            let (dir, library, _) = empty_library_and_device();
            let (tx, commands) = mpsc::channel();
            let canceled = Arc::new(AtomicBool::new(true));
            tx.send(QueuedCommand {
                is_canceled: canceled,
                command: WorkerCommand::PullSaves(Vec::new()),
            })
            .unwrap();
            tx.send(QueuedCommand {
                is_canceled: Arc::default(),
                command: WorkerCommand::Reload,
            })
            .unwrap();
            // With nothing left to come, `run_worker_loop` returns.
            drop(tx);
            let labels = std::cell::RefCell::new(Vec::new());
            // A device that isn't there, so this can't read one that is mounted.
            let no_device = Some(dir.path().join("no device"));
            run_worker_loop(&library, no_device, &commands, &|message| {
                if let WorkerMessage::CommandStarted { label, .. } = message {
                    labels.borrow_mut().push(label);
                }
            });
            assert_eq!(*labels.borrow(), ["reading everything again"]);
        }

        #[test]
        fn stops_command_at_next_game_after_cancel() {
            let (_dir, library, device) = empty_library_and_device();
            put(&library, "roms/gba/A.gba", b"a");
            put(&library, "roms/gba/B.gba", b"b");
            let games = pocketeer_core::library::Library::open(&library)
                .unwrap()
                .entries()
                .to_vec();
            let (tx, commands) = mpsc::channel();
            let is_canceled = Arc::new(AtomicBool::new(false));
            tx.send(QueuedCommand {
                is_canceled: is_canceled.clone(),
                command: WorkerCommand::PushGames(games),
            })
            .unwrap();
            // With nothing left to come, `run_worker_loop` returns.
            drop(tx);
            let finished = std::cell::RefCell::new(String::new());
            run_worker_loop(
                &library,
                Some(device.clone()),
                &commands,
                &|message| match message {
                    WorkerMessage::CommandProgressed { .. } => {
                        is_canceled.store(true, Ordering::Relaxed)
                    }
                    WorkerMessage::CommandFinished { summary, .. } => {
                        *finished.borrow_mut() = summary;
                    }
                    _ => {}
                },
            );
            assert_eq!(*finished.borrow(), "pushing games: stopped after 1 of 2");
            assert!(device.join("Assets/gba/common/A.gba").exists());
            assert!(!device.join("Assets/gba/common/B.gba").exists());
        }

        #[test]
        fn cancels_only_commands_sent_before_cancel() {
            let (mut worker_handle, rx) = WorkerHandle::recording();
            worker_handle.send(WorkerCommand::Reload);
            worker_handle.send(WorkerCommand::SyncDats);
            worker_handle.cancel();
            worker_handle.send(WorkerCommand::Eject);
            let flags: Vec<bool> = rx
                .try_iter()
                .map(|queued| queued.is_canceled.load(Ordering::Relaxed))
                .collect();
            assert_eq!(flags, [true, true, false]);
        }

        #[test]
        fn ends_thread_when_dropped() {
            let (_dir, worker_handle, rx) = started();
            until_device_read(&rx);
            drop(worker_handle);
            // The thread held the other end.
            while rx.try_recv().is_ok() {}
            assert_eq!(rx.try_recv(), Err(mpsc::TryRecvError::Disconnected));
        }

        #[test]
        fn finishes_at_most_current_game_when_dropped() {
            let (dir, mut worker_handle, rx) = started();
            let games = locals(last_games_page_data(&until_device_read(&rx)));
            worker_handle.send(WorkerCommand::PushGames(games));
            drop(worker_handle);
            let on_device = ["A.gba", "B.gba"]
                .iter()
                .filter(|name| {
                    dir.path()
                        .join("device/Assets/gba/common")
                        .join(name)
                        .exists()
                })
                .count();
            assert!(on_device <= 1, "{on_device}");
            let log =
                fs::read_to_string(dir.path().join("library/pocketeer.log")).unwrap_or_default();
            assert_eq!(log.lines().count(), on_device);
        }
    }
}
