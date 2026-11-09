//! The worker thread, which alone holds the `Pocketeer` and runs one command at
//! a time.

use std::{
    ops::ControlFlow,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use super::{Event, path_input};
use pocketeer_core::{
    Pocketeer,
    cores::{BiosRow, CoreList},
    device::{Device, DeviceEntry, DiskSpace, Firmware, OrphanDeviceSave},
    library::{LibrarySave, LocalEntry, OrphanLocalSave},
    log::{FrontEnd, LogEntry, LoggedError, OperationReport},
    platform::Platform,
    pocketeer::{BatchResult, GameRow},
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
                GameRow::InLibrary(local_entry) | GameRow::Both { local_entry, .. } => {
                    local_entry.name.clone()
                }
                GameRow::OnDevice(device_entry) => device_entry.path.display().to_string(),
            },
            GamesTableRow::OrphanSaveInLibrary(orphan) => orphan.stem.clone(),
            GamesTableRow::OrphanSaveOnDevice(orphan) => orphan.path.display().to_string(),
        }
    }

    /// Whether a search for `query` finds the row, by its name or by its path
    /// on the device.
    pub fn matches(&self, query: &str) -> bool {
        let device_path = self
            .device_entry()
            .map(|device_entry| device_entry.path.display().to_string())
            .unwrap_or_default();
        path_input::fuzzy_match(query, &self.name()).is_some()
            || path_input::fuzzy_match(query, &device_path).is_some()
    }
}

/// What the Games page draws, as the worker last read it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GamesPageData {
    pub rows: Vec<GamesTableRow>,
    pub device_root: Option<PathBuf>,
}

impl GamesPageData {
    fn new(pocketeer: &Pocketeer, orphan_saves: &OrphanSaves) -> Self {
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
        Self {
            rows,
            device_root: pocketeer.device().map(|device| device.root().to_path_buf()),
        }
    }
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
#[derive(Debug)]
pub enum WorkerMessage {
    GamesPageChanged(GamesPageData),
    /// The cores, once their page has been opened.
    CoreListChanged(CoreList),
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
    tx: Option<Sender<QueuedCommand>>,
    /// The flag every command sent until the next cancel carries. Once true it
    /// stays true, and a cancel creates a new one for later commands.
    is_canceled: Arc<AtomicBool>,
    /// None once it is dropped, or for a worker with no thread.
    thread: Option<JoinHandle<()>>,
}

impl WorkerHandle {
    /// Starts the thread, which opens the library at `library` and the device
    /// at `device`, or else any device mounted now or later. Its messages go
    /// to `events`.
    pub fn spawn(library: PathBuf, device: Option<PathBuf>, events: Sender<Event>) -> Self {
        let send_message = move |message| {
            let _ = events.send(Event::Worker(Box::new(message)));
        };
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
    pub fn recording() -> (WorkerHandle, Receiver<QueuedCommand>) {
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
    rx: &Receiver<QueuedCommand>,
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
        (self.send_message)(WorkerMessage::GamesPageChanged(GamesPageData::new(
            pocketeer,
            &self.orphan_saves,
        )));
    }

    /// Looks for orphans only when a command ends, because that reads every
    /// save and the games page may be sent for each game.
    fn refresh_orphans(&mut self, pocketeer: &Pocketeer) {
        self.orphan_saves.in_library = pocketeer.library().find_orphan_saves().unwrap_or_default();
        self.orphan_saves.on_device = pocketeer.find_orphan_saves_on_device().unwrap_or_default();
    }

    fn send_cores_page_data(&self, pocketeer: &mut Pocketeer) {
        if !self.is_cores_page_opened {
            return;
        }
        let core_list = pocketeer.load_core_list().unwrap_or_else(|error| CoreList {
            rows: Vec::new(),
            index_failure: Some(error),
        });
        (self.send_message)(WorkerMessage::CoreListChanged(core_list));
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
            [] if open.is_none() => {}
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
                send_duplicates(self.send_message, &report.duplicates);
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
        let mut progress = CommandProgress {
            send_message: self.send_message,
            orphan_saves: &self.orphan_saves,
            is_canceled,
            target_count: command.target_count(),
            reports: Vec::new(),
        };
        let label = command.label();
        (self.send_message)(WorkerMessage::CommandStarted {
            label: label.to_string(),
            target_count: progress.target_count,
        });
        let batch_result = match command {
            WorkerCommand::PullGames(games) => pocketeer.pull_games(&games, progress.on_logged()).0,
            WorkerCommand::PullSaves(games) => pocketeer.pull_saves(&games, progress.on_logged()),
            WorkerCommand::PushGames(games) => pocketeer.push_games(&games, progress.on_logged()).0,
            WorkerCommand::PushSaveVersion(game, version) => {
                pocketeer.push_save_versions(&[(game, version)], progress.on_logged())
            }
            WorkerCommand::FetchCovers(games) => {
                pocketeer.fetch_covers(&games, progress.on_logged())
            }
            WorkerCommand::ImportGame(path, platform) => {
                let files = pocketeer_core::pocketeer::expand_import_paths(&[path]);
                files
                    .map_err(LoggedError::OperationFailed)
                    .and_then(|files| {
                        // The app asks for the platform when the extension doesn't
                        // tell it, so every file has one.
                        let files: Vec<_> = files
                            .into_iter()
                            .filter_map(|(file, by_extension)| {
                                Some((file, platform.clone().or(by_extension)?))
                            })
                            .collect();
                        progress.target_count = files.len();
                        pocketeer.import_games(&files, progress.on_logged()).0
                    })
            }
            WorkerCommand::ImportSave(game, from) => {
                pocketeer.import_saves(&[(game, from)], progress.on_logged())
            }
            WorkerCommand::ImportCover(game, from) => {
                pocketeer.import_covers(&[(game, from)], progress.on_logged())
            }
            WorkerCommand::TrashGames(games) => {
                pocketeer.trash_games_in_library(&games, progress.on_logged())
            }
            WorkerCommand::TrashSaveVersion(game, version) => {
                pocketeer.trash_save_versions_in_library(&[(game, version)], progress.on_logged())
            }
            WorkerCommand::TrashOrphanSaves(orphan_saves) => {
                pocketeer.trash_orphan_saves_in_library(&orphan_saves, progress.on_logged())
            }
            WorkerCommand::DeleteGames(games) => {
                pocketeer.delete_games_on_device(&games, progress.on_logged())
            }
            WorkerCommand::DeleteSaves(games) => {
                pocketeer.delete_saves_on_device(&games, progress.on_logged())
            }
            WorkerCommand::DeleteOrphanSaves(orphan_saves) => {
                pocketeer.delete_orphan_saves_on_device(&orphan_saves, progress.on_logged())
            }
            // Handled above, since they aren't commands to report.
            WorkerCommand::ShowCores | WorkerCommand::ProposeRenames(_) => {
                Ok(ControlFlow::Continue(()))
            }
            WorkerCommand::InstallCores(ids) => pocketeer.install_cores(&ids, progress.on_logged()),
            WorkerCommand::RemoveCores(ids) => pocketeer.remove_cores(&ids, progress.on_logged()),
            WorkerCommand::PutBios(row, from) => {
                pocketeer.put_bios_files(&[(row, from)], progress.on_logged())
            }
            WorkerCommand::RenameGamesInLibrary(games) => {
                pocketeer.rename_games_in_library(&games, progress.on_logged())
            }
            WorkerCommand::RenameOnDevice(game, new) => {
                pocketeer.rename_games_on_device(&[(game, new)], progress.on_logged())
            }
            // These do one thing, so they have no batch to stop.
            WorkerCommand::SyncDats => {
                let synced = pocketeer.sync_dats(|pocketeer, entry| {
                    let _ = progress.record_finished_target(pocketeer, entry.report);
                });
                only_log_failure(synced)
            }
            WorkerCommand::Reload => {
                let read = pocketeer.reload(|pocketeer, entry| {
                    let _ = progress.record_finished_target(pocketeer, entry.report);
                });
                if let Ok(report) = &read {
                    send_duplicates(self.send_message, &report.duplicates);
                }
                only_log_failure(read)
            }
            WorkerCommand::Eject => {
                let ejected = pocketeer.eject_device(|pocketeer, entry| {
                    let _ = progress.record_finished_target(pocketeer, entry.report);
                });
                only_log_failure(ejected)
            }
        };
        let (mut summary, mut failed) =
            command_summary(label, progress.target_count, &progress.reports);
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
    /// The callback for a batch, which records each game as it finishes.
    fn on_logged(&mut self) -> impl FnMut(&Pocketeer, LogEntry) -> ControlFlow<()> + '_ {
        move |pocketeer: &Pocketeer, log_entry: LogEntry| {
            self.record_finished_target(pocketeer, log_entry.report)
        }
    }

    /// Records how one game went and sends the current state. Breaks if the
    /// command was canceled.
    fn record_finished_target(
        &mut self,
        pocketeer: &Pocketeer,
        report: OperationReport,
    ) -> ControlFlow<()> {
        let target = report.subject.clone();
        self.reports.push(report);
        (self.send_message)(WorkerMessage::GamesPageChanged(GamesPageData::new(
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
    let failed = reports.iter().filter(|report| !report.is_ok).count();
    let first_failure = reports.iter().find(|report| !report.is_ok);
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

/// The part of a single operation's result that the summary still has to
/// tell: its own failure is in its report already, but a failed log write isn't.
fn only_log_failure<T, E>(result: Result<T, LoggedError<E>>) -> BatchResult {
    match result {
        Err(LoggedError::LogFailed(error)) => Err(LoggedError::LogFailed(error)),
        Ok(_) | Err(LoggedError::OperationFailed(_)) => Ok(ControlFlow::Continue(())),
    }
}

/// Tells the UI about `duplicates`, the copies a scan left out, if there are
/// any.
fn send_duplicates(send_message: &dyn Fn(WorkerMessage), duplicates: &[DeviceEntry]) {
    if duplicates.is_empty() {
        return;
    }
    let lines = duplicates.iter().map(ToString::to_string);
    send_message(WorkerMessage::DuplicateGamesSkipped(lines.collect()));
}

#[cfg(test)]
mod tests {
    use std::fs;

    use pocketeer_core::{
        cores::testing::index_file, device::testing::make_empty_device, os::testing::put,
    };

    use super::*;

    /// The worker on `library` and `device`, and the receiver of its messages.
    fn worker_on(
        library: PathBuf,
        device: Option<PathBuf>,
    ) -> (WorkerHandle, Receiver<WorkerMessage>) {
        let (events, worker_events) = mpsc::channel();
        let worker_handle = WorkerHandle::spawn(library, device, events);
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for event in worker_events {
                if let Event::Worker(message) = event {
                    let _ = tx.send(*message);
                }
            }
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
        make_empty_device(&device);
        (dir, library, device)
    }

    /// A library and a device holding these files, and the worker on them.
    fn started_with(
        library_files: &[(&str, &[u8])],
        device_files: &[(&str, &[u8])],
    ) -> (tempfile::TempDir, WorkerHandle, Receiver<WorkerMessage>) {
        let (dir, library, device) = empty_library_and_device();
        for (path, bytes) in library_files {
            put(&library, path, bytes);
        }
        for (path, bytes) in device_files {
            put(&device, path, bytes);
        }
        let (worker_handle, rx) = worker_on(library, Some(device));
        (dir, worker_handle, rx)
    }

    /// A library with two games and a device with one, the worker on them.
    fn started() -> (tempfile::TempDir, WorkerHandle, Receiver<WorkerMessage>) {
        started_with(
            &[("roms/gba/A.gba", b"a"), ("roms/gba/B.gba", b"b")],
            &[
                ("Assets/gba/common/C.gba", b"c"),
                ("Saves/gba/common/Gone.sav", b"orphan"),
            ],
        )
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
        messages_until(rx, |message| {
            matches!(message, WorkerMessage::DeviceReadFinished)
        })
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

    /// The ids of the cores last sent in `messages`.
    fn cores_sent(messages: &[WorkerMessage]) -> Option<Vec<String>> {
        messages.iter().rev().find_map(|message| match message {
            WorkerMessage::CoreListChanged(core_list) => Some(
                core_list
                    .rows
                    .iter()
                    .map(|row| row.id().to_string())
                    .collect(),
            ),
            _ => None,
        })
    }

    /// Every message up to the cores being sent.
    fn until_cores(rx: &Receiver<WorkerMessage>) -> Vec<WorkerMessage> {
        messages_until(rx, |message| {
            matches!(message, WorkerMessage::CoreListChanged(_))
        })
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

    /// Runs `after_device_opened` on a Pocketeer with the device open, then
    /// checks for the device once it has disappeared. Returns the messages
    /// that check sent.
    fn messages_after_device_disappears(
        after_device_opened: impl FnOnce(&mut Pocketeer),
    ) -> Vec<WorkerMessage> {
        let (_dir, library, device) = empty_library_and_device();
        let mut pocketeer = Pocketeer::open(&library, FrontEnd::Tui).unwrap();
        let messages = std::cell::RefCell::new(Vec::new());
        let send_message = |message| messages.borrow_mut().push(message);
        let mut worker_state = WorkerState {
            send_message: &send_message,
            device_from_command_line: Some(device.clone()),
            last_found_devices: Vec::new(),
            orphan_saves: OrphanSaves::default(),
            is_cores_page_opened: false,
        };
        worker_state.sync_mounted_device(&mut pocketeer);
        after_device_opened(&mut pocketeer);
        messages.borrow_mut().clear();
        fs::remove_dir_all(&device).unwrap();
        worker_state.sync_mounted_device(&mut pocketeer);
        messages.into_inner()
    }

    /// Unplugging the open device says so. Ejecting closes the device before
    /// it goes, so then nothing is said.
    #[test]
    fn reports_device_gone_only_if_it_was_open() {
        let messages = messages_after_device_disappears(|_| ());
        assert_eq!(finished(&messages), ("device gone", false));
        let messages = messages_after_device_disappears(Pocketeer::close_device);
        assert!(messages.is_empty(), "{messages:?}");
    }

    mod opening {
        use super::*;

        #[test]
        fn reports_duplicates_on_open_and_reload() {
            let (_dir, mut worker_handle, rx) = started_with(
                &[],
                &[
                    ("Assets/gba/common/One.gba", b"c"),
                    ("Assets/gba/common/RPG/Two.gba", b"c"),
                ],
            );
            let lines = ["gba/RPG/Two.gba".to_string()];
            let opening = until_device_read(&rx);
            assert_eq!(duplicates(&opening), [&lines[..]]);
            assert_eq!(last_games_page_data(&opening).rows.len(), 1);
            worker_handle.send(WorkerCommand::Reload);
            assert_eq!(duplicates(&until_finished(&rx)), [&lines[..]]);
        }

        #[test]
        fn reports_only_failure_when_library_cannot_open() {
            let dir = tempfile::tempdir().unwrap();
            let (_worker, rx) =
                worker_on(dir.path().join("typo"), Some(dir.path().join("no device")));
            let told = rx.recv_timeout(Duration::from_secs(10)).unwrap();
            assert!(
                matches!(told, WorkerMessage::LibraryOpenFailed(_)),
                "{told:?}"
            );
            // The worker has gone, and its end of the channel with it.
            assert!(rx.recv_timeout(Duration::from_secs(10)).is_err());
        }

        #[test]
        fn reports_why_device_cannot_be_read() {
            // A folder is where the game's save goes.
            let (_dir, _worker, rx) = started_with(
                &[],
                &[
                    ("Assets/gba/common/One.gba", b"c"),
                    ("Saves/gba/common/One.sav/x", b""),
                ],
            );
            let messages = until_finished(&rx);
            let (summary, failed) = finished(&messages);
            assert!(failed);
            assert!(
                summary.starts_with("couldn't read the device: "),
                "{summary}"
            );
            assert!(
                messages
                    .iter()
                    .any(|message| matches!(message, WorkerMessage::DeviceReadFinished))
            );
            assert_eq!(last_games_page_data(&messages).device_root, None);
        }
    }

    mod commands {
        use super::*;

        /// Opens the device, pushes two games and deletes an orphan save,
        /// checking what the UI is told at each step.
        #[test]
        fn worker_opens_device_runs_command_and_refreshes() {
            let (dir, mut worker_handle, rx) = started();
            let opening = until_device_read(&rx);
            assert!(matches!(opening[1], WorkerMessage::DeviceReadStarted(_)));
            let games_page_data = last_games_page_data(&opening);
            assert!(games_page_data.device_root.is_some());
            // A, B, C and the orphan save.
            assert_eq!(games_page_data.rows.len(), 4);
            let GamesTableRow::OrphanSaveOnDevice(orphan) = games_page_data.rows[3].clone() else {
                panic!()
            };
            // Reading it successfully isn't a command to report.
            let is_command_message = |message: &WorkerMessage| {
                matches!(
                    message,
                    WorkerMessage::CommandStarted { .. } | WorkerMessage::CommandFinished { .. }
                )
            };
            assert!(!opening.iter().any(is_command_message));
            let device_page = device_page_sent(&opening).unwrap();
            assert_eq!(device_page.root, dir.path().join("device"));
            assert_eq!(device_page.firmware, None);
            assert_eq!(device_page.core_count, Some(0));
            assert_eq!(device_page.game_count, 1);
            assert!(device_page.disk_space.unwrap().total_bytes > 0);

            worker_handle.send(WorkerCommand::PushGames(locals(games_page_data)));
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
            let on_device = last_games_page_data(&messages)
                .rows
                .iter()
                .filter(|row| row.device_entry().is_some());
            assert_eq!(on_device.count(), 3);
            assert_eq!(device_page_sent(&messages).unwrap().game_count, 3);

            worker_handle.send(WorkerCommand::DeleteOrphanSaves(vec![orphan]));
            let messages = until_finished(&rx);
            assert_eq!(
                finished(&messages),
                ("gba/Gone.sav: save deleted from the device", false)
            );
            assert_eq!(last_games_page_data(&messages).rows.len(), 3);
        }

        /// A file is imported for the platform chosen, and a folder counts
        /// each game in it once it is read.
        #[test]
        fn import_counts_files_and_uses_chosen_platform() {
            let (dir, mut worker_handle, rx) = started();
            until_device_read(&rx);
            let from = put(dir.path(), "file/New.bin", b"new");
            worker_handle.send(WorkerCommand::ImportGame(from, Some(Platform::Gb)));
            let messages = until_finished(&rx);
            assert!(!finished(&messages).1);
            let imported: Vec<(String, Platform)> = locals(last_games_page_data(&messages))
                .into_iter()
                .filter(|local_entry| local_entry.name == "New.bin")
                .map(|local_entry| (local_entry.name, local_entry.game.platform))
                .collect();
            assert_eq!(imported, [("New.bin".to_string(), Platform::Gb)]);

            put(dir.path(), "folder/Fresh.gb", b"other new");
            put(dir.path(), "folder/deep/Other.gb", b"other");
            worker_handle.send(WorkerCommand::ImportGame(dir.path().join("folder"), None));
            let messages = until_finished(&rx);
            assert_eq!(finished(&messages), ("importing games: 2 done", false));
        }

        #[test]
        fn proposes_dat_names_without_reporting_command() {
            let (_dir, mut worker_handle, rx) =
                started_with(&[("roms/gba/Old.gba", b"hello")], &[]);
            let opening = until_device_read(&rx);
            worker_handle.send(WorkerCommand::ProposeRenames(locals(last_games_page_data(
                &opening,
            ))));
            let messages = messages_until(&rx, |message| {
                matches!(message, WorkerMessage::RenamesProposed(_))
            });
            assert!(
                matches!(messages[..], [WorkerMessage::RenamesProposed(_)]),
                "{messages:?}"
            );
        }

        /// Nothing goes online for the cores until their page opens. From
        /// then on they are sent again after each command.
        #[test]
        fn sends_cores_only_after_page_opens() {
            // The index was written just now, so it isn't fetched again.
            let (_dir, mut worker_handle, rx) = started_with(
                &[("cores.json", &index_file("http://127.0.0.1:1/never"))],
                &[("Cores/a.B/core.json", b"{}")],
            );
            let opening = until_device_read(&rx);
            assert_eq!(cores_sent(&opening), None);
            worker_handle.send(WorkerCommand::Reload);
            assert_eq!(cores_sent(&until_finished(&rx)), None);
            worker_handle.send(WorkerCommand::ShowCores);
            let told = cores_sent(&until_cores(&rx)).unwrap();
            assert_eq!(told, ["Spiritualized.GB", "a.B"]);
            worker_handle.send(WorkerCommand::RemoveCores(vec!["a.B".to_string()]));
            let messages = until_finished(&rx);
            assert_eq!(finished(&messages), ("a.B: removed from the device", false));
            assert_eq!(cores_sent(&messages).unwrap(), ["Spiritualized.GB"]);
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
            let ok = || OperationReport::reload(&Ok(()));
            let bad =
                || OperationReport::reload::<()>(&Err(pocketeer_core::error::BasicError::NoDevice));
            assert_eq!(
                command_summary("reading", 1, &[ok()]),
                ("everything: read again".to_string(), false)
            );
            assert_eq!(
                command_summary("reading", 1, &[bad()]),
                ("everything: error: no device is open".to_string(), true)
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

        /// The worker checks for a cancel after each game, so the game under
        /// way when the handle is dropped is finished and logged, and the
        /// thread has ended once the drop returns. If this thread is slow to
        /// drop it, the second game may be done as well.
        #[test]
        fn finishes_game_under_way_when_dropped() {
            let (dir, mut worker_handle, rx) = started();
            let games = locals(last_games_page_data(&until_device_read(&rx)));
            worker_handle.send(WorkerCommand::PushGames(games));
            messages_until(&rx, |message| {
                matches!(message, WorkerMessage::CommandStarted { .. })
            });
            drop(worker_handle);
            // The thread held the other end. Its messages pass through
            // `worker_on`'s thread, so the end goes once they have arrived.
            while rx.recv_timeout(Duration::from_secs(10)).is_ok() {}
            assert!(matches!(
                rx.try_recv(),
                Err(mpsc::TryRecvError::Disconnected)
            ));
            let on_device = ["A.gba", "B.gba"]
                .iter()
                .filter(|name| {
                    dir.path()
                        .join("device/Assets/gba/common")
                        .join(name)
                        .exists()
                })
                .count();
            assert!((1..=2).contains(&on_device), "{on_device}");
            let log = fs::read_to_string(dir.path().join("library/pocketeer.log")).unwrap();
            assert_eq!(log.lines().count(), on_device);
        }
    }
}
