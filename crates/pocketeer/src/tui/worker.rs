//! The worker thread: it alone holds the `Pocketeer`, does one command at a
//! time, and tells the UI how it goes and how things stand.

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

use pocketeer_core::{
    Pocketeer,
    cores::{BiosRow, CoreRow},
    device::{Device, DeviceEntry, OrphanDeviceSave},
    game::Save,
    library::{LocalEntry, OrphanLocalSave},
    log::{FrontEnd, OperationReport, device_ref},
    platform::Platform,
    pocketeer::GameRow,
};

/// How often it looks for a device being plugged in or pulled, when idle.
const WATCH: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    PullGames(Vec<DeviceEntry>),
    PullSaves(Vec<DeviceEntry>),
    PushGames(Vec<LocalEntry>),
    /// The newest version of each game's save.
    PushSaves(Vec<DeviceEntry>),
    PushSaveVersion(DeviceEntry, Save),
    FetchCovers(Vec<LocalEntry>),
    TrashGames(Vec<LocalEntry>),
    /// The file, for this platform or else what its extension says.
    ImportGame(PathBuf, Option<Platform>),
    ImportSave(LocalEntry, PathBuf),
    ImportCover(LocalEntry, PathBuf),
    TrashSaveVersion(LocalEntry, Save),
    TrashOrphanSaves(Vec<OrphanLocalSave>),
    DeleteGames(Vec<DeviceEntry>),
    DeleteSaves(Vec<DeviceEntry>),
    DeleteOrphanSaves(Vec<OrphanDeviceSave>),
    SyncDats,
    Reload,
    Eject,
    /// The Cores page was opened: from now on the worker tells of the cores.
    ShowCores,
    InstallCores(Vec<String>),
    RemoveCores(Vec<String>),
    /// The file to copy to where the row says.
    PutBios(BiosRow, PathBuf),
    /// The game in the library, and what to call it.
    RenameGamesInLibrary(Vec<(LocalEntry, String)>),
    /// The game on the device, and what to call it.
    RenameOnDevice(DeviceEntry, String),
}

/// One line of the games table.
#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    Game {
        row: GameRow,
        /// The game's name in the DATs.
        standard_name: Option<String>,
        /// What putting it in the trash puts there.
        files: Vec<PathBuf>,
    },
    OrphanInLibrary(OrphanLocalSave),
    OrphanOnDevice(OrphanDeviceSave),
}

impl Row {
    /// The game's copy in the library, if this is a game and it has one.
    pub fn local_entry(&self) -> Option<&LocalEntry> {
        match self {
            Row::Game { row, .. } => row.local_entry(),
            _ => None,
        }
    }

    /// The game's copy on the device, if this is a game and it has one.
    pub fn device_entry(&self) -> Option<&DeviceEntry> {
        match self {
            Row::Game { row, .. } => row.device_entry(),
            _ => None,
        }
    }

    /// The save, if this is an orphan save in the library.
    pub fn orphan_in_library(&self) -> Option<&OrphanLocalSave> {
        match self {
            Row::OrphanInLibrary(orphan) => Some(orphan),
            _ => None,
        }
    }

    /// The save, if this is an orphan save on the device.
    pub fn orphan_on_device(&self) -> Option<&OrphanDeviceSave> {
        match self {
            Row::OrphanOnDevice(orphan) => Some(orphan),
            _ => None,
        }
    }
}

/// How things stand: what the screens draw from.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub rows: Vec<Row>,
    /// Where the device open is.
    pub device: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Update {
    Snapshot(Snapshot),
    /// The cores, once the page for them has been opened, and why the
    /// inventory couldn't be had, if it couldn't.
    Cores {
        rows: Vec<CoreRow>,
        failure: Option<String>,
    },
    /// The files the cores on the device load; none without a device.
    Bios(Vec<BiosRow>),
    /// The device at this path is being opened and read.
    ReadingDevice(PathBuf),
    /// It has been, well or not; if not, `Finished` says why.
    DeviceRead,
    Started {
        label: String,
        total: usize,
    },
    /// One game of the command under way is done.
    Progress {
        done: usize,
        total: usize,
        game: String,
    },
    /// The command is over; `failed` if some of it didn't go well.
    Finished {
        summary: String,
        failed: bool,
    },
    /// The copies a scan left out, one line for each. Only sent when the
    /// device is opened or read again on request, and there are some.
    Duplicates(Vec<String>),
    /// The library couldn't be opened, for this reason; the worker has gone.
    LibraryFailed(String),
}

/// A command, and the token that says whether it has been canceled.
pub type Sent = (Arc<AtomicBool>, Command);

/// The UI's end of the worker. When it goes, the thread is stopped as by
/// [`Worker::cancel`] and waited for, so that what it writes is finished and
/// logged.
pub struct Worker {
    /// None once it is going.
    tx: Option<mpsc::Sender<Sent>>,
    /// What every command sent from now on, until a cancel, goes with. Once
    /// true it stays so: a cancel makes a new one for the commands after it.
    cancel_token: Arc<AtomicBool>,
    /// None once it is going, or for a worker with no thread.
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    /// Starts the thread, which opens the library at `library` and the device
    /// at `device`, or else any device mounted now or later.
    pub fn spawn(
        library: PathBuf,
        device: Option<PathBuf>,
        send: impl Fn(Update) + Send + 'static,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let thread = thread::spawn(move || run(&library, device, &rx, &send));
        Self {
            tx: Some(tx),
            cancel_token: Arc::default(),
            thread: Some(thread),
        }
    }

    /// A worker with no thread, whose commands can be read back.
    #[cfg(test)]
    pub fn recording() -> (Worker, mpsc::Receiver<Sent>) {
        let (tx, rx) = mpsc::channel();
        let worker = Worker {
            tx: Some(tx),
            cancel_token: Arc::default(),
            thread: None,
        };
        (worker, rx)
    }

    pub fn send(&mut self, command: Command) {
        if let Some(tx) = &self.tx {
            let _ = tx.send((self.cancel_token.clone(), command));
        }
    }

    /// Stops the command under way after its current game, and drops those
    /// waiting.
    pub fn cancel(&mut self) {
        self.cancel_token.store(true, Ordering::Relaxed);
        self.cancel_token = Arc::default();
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel();
        // With nothing left to come, the thread returns.
        drop(self.tx.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(library: &Path, pinned: Option<PathBuf>, rx: &mpsc::Receiver<Sent>, send: &dyn Fn(Update)) {
    let mut p = match Pocketeer::open(library, FrontEnd::Tui) {
        Ok(p) => p,
        Err(e) => {
            send(Update::LibraryFailed(e.to_string()));
            return;
        }
    };
    let mut state = State {
        send,
        pinned,
        seen: Vec::new(),
        orphans: Orphans::default(),
        cores_wanted: false,
    };
    state.refresh_orphans(&p);
    state.send_snapshot(&p);
    state.sync_mounted_device(&mut p);
    loop {
        match rx.recv_timeout(WATCH) {
            Ok((cancel_token, command)) => {
                if !cancel_token.load(Ordering::Relaxed) {
                    state.run_command(&mut p, command, &cancel_token);
                }
            }
            Err(RecvTimeoutError::Timeout) => state.sync_mounted_device(&mut p),
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[derive(Default)]
struct Orphans {
    in_library: Vec<OrphanLocalSave>,
    on_device: Vec<OrphanDeviceSave>,
}

struct State<'a> {
    send: &'a dyn Fn(Update),
    /// The device asked for, if one was.
    pinned: Option<PathBuf>,
    /// The devices found last time.
    seen: Vec<PathBuf>,
    orphans: Orphans,
    /// Whether the Cores page has been opened: until then nothing goes online
    /// for it.
    cores_wanted: bool,
}

impl State<'_> {
    fn send_snapshot(&self, p: &Pocketeer) {
        (self.send)(Update::Snapshot(snapshot(p, &self.orphans)));
    }

    /// Orphans are looked for only when a command ends: that reads every
    /// save, and there may be a snapshot for each game.
    fn refresh_orphans(&mut self, p: &Pocketeer) {
        self.orphans.in_library = p.find_orphan_saves_in_library().unwrap_or_default();
        self.orphans.on_device = p.find_orphan_saves_on_device().unwrap_or_default();
    }

    fn send_cores(&self, p: &mut Pocketeer) {
        if !self.cores_wanted {
            return;
        }
        let (rows, failure) = match p.load_core_rows() {
            Ok((rows, failure)) => (rows, failure.map(|e| e.to_string())),
            Err(e) => (Vec::new(), Some(e.to_string())),
        };
        (self.send)(Update::Cores { rows, failure });
    }

    fn send_bios(&self, p: &Pocketeer) {
        (self.send)(Update::Bios(p.read_bios_rows().unwrap_or_default()));
    }

    /// Opens a device that came, closes one that went.
    fn sync_mounted_device(&mut self, p: &mut Pocketeer) {
        let found = match &self.pinned {
            Some(path) if Device::is_device(path) => vec![path.clone()],
            Some(_) => Vec::new(),
            None => Device::find_mounted(),
        };
        let open = p.device().map(|d| d.root().to_path_buf());
        if found == self.seen && open.as_ref().is_none_or(|o| found.contains(o)) {
            return;
        }
        self.seen = found.clone();
        if open.as_ref().is_some_and(|o| found.contains(o)) {
            return;
        }
        p.close_device();
        match &found[..] {
            [] => {
                self.finish_with_refresh(p, "device gone".to_string(), false);
            }
            [root] => self.open_device(p, root),
            many => {
                let list: Vec<_> = many.iter().map(|d| d.display().to_string()).collect();
                let summary = format!("more than one device mounted: {}", list.join(", "));
                self.finish_with_refresh(p, summary, true);
            }
        }
    }

    fn open_device(&mut self, p: &mut Pocketeer, root: &Path) {
        (self.send)(Update::ReadingDevice(root.to_path_buf()));
        let opened = p
            .open_device(root)
            .map_err(|e| e.to_string())
            .and_then(|()| p.scan_device().map_err(|e| e.to_string()));
        let failure = match opened {
            Ok(report) => {
                send_duplicates(self.send, p, &report.duplicates);
                None
            }
            Err(e) => {
                p.close_device();
                Some(format!("couldn't read the device: {e}"))
            }
        };
        self.refresh_orphans(p);
        self.send_snapshot(p);
        self.send_cores(p);
        self.send_bios(p);
        (self.send)(Update::DeviceRead);
        if let Some(summary) = failure {
            (self.send)(Update::Finished {
                summary,
                failed: true,
            });
        }
    }

    fn finish_with_refresh(&mut self, p: &mut Pocketeer, summary: String, failed: bool) {
        self.refresh_orphans(p);
        self.send_snapshot(p);
        self.send_cores(p);
        self.send_bios(p);
        (self.send)(Update::Finished { summary, failed });
    }

    fn run_command(&mut self, p: &mut Pocketeer, command: Command, cancel_token: &AtomicBool) {
        if command == Command::ShowCores {
            self.cores_wanted = true;
            self.send_cores(p);
            return;
        }
        let mut batch = Batch {
            send: self.send,
            orphans: &self.orphans,
            cancel_token,
            total: target_count(&command),
            reports: Vec::new(),
        };
        let label = job_label(&command);
        (self.send)(Update::Started {
            label: label.to_string(),
            total: batch.total,
        });
        // A command that isn't a batch reports through `batch` by hand: it has no
        // entry to go by, only what came of it.
        let batch_result = match command {
            Command::PullGames(games) => {
                p.pull_games(&games, |p, e| batch.report_step(p, e.report))
            }
            Command::PullSaves(games) => {
                p.pull_saves(&games, |p, e| batch.report_step(p, e.report))
            }
            Command::PushGames(games) => {
                p.push_games(&games, |p, e| batch.report_step(p, e.report))
            }
            Command::PushSaves(games) => {
                p.push_saves(&games, |p, e| batch.report_step(p, e.report))
            }
            Command::PushSaveVersion(game, version) => {
                p.push_save_versions(&[(game, version)], |p, e| batch.report_step(p, e.report))
            }
            Command::FetchCovers(games) => {
                p.fetch_covers(&games, |p, e| batch.report_step(p, e.report))
            }
            Command::ImportGame(path, platform) => {
                pocketeer_core::pocketeer::expand_import_paths(&[path]).and_then(|files| {
                    batch.total = files.len();
                    p.import_games(&files, platform.as_ref(), |p, e| {
                        batch.report_step(p, e.report)
                    })
                })
            }
            Command::ImportSave(game, from) => {
                p.import_saves(&[(game, from)], |p, e| batch.report_step(p, e.report))
            }
            Command::ImportCover(game, from) => {
                p.import_covers(&[(game, from)], |p, e| batch.report_step(p, e.report))
            }
            Command::TrashGames(games) => {
                p.trash_games_in_library(&games, |p, e| batch.report_step(p, e.report))
            }
            Command::TrashSaveVersion(game, version) => p
                .trash_save_versions_in_library(&[(game, version)], |p, e| {
                    batch.report_step(p, e.report)
                }),
            Command::TrashOrphanSaves(orphans) => {
                p.trash_orphan_saves_in_library(&orphans, |p, e| batch.report_step(p, e.report))
            }
            Command::DeleteGames(games) => {
                p.delete_games_on_device(&games, |p, e| batch.report_step(p, e.report))
            }
            Command::DeleteSaves(games) => {
                p.delete_saves_on_device(&games, |p, e| batch.report_step(p, e.report))
            }
            Command::DeleteOrphanSaves(orphans) => {
                p.delete_orphan_saves_on_device(&orphans, |p, e| batch.report_step(p, e.report))
            }
            // Taken care of above: it is no job to report on.
            Command::ShowCores => Ok(ControlFlow::Continue(())),
            Command::InstallCores(ids) => {
                p.install_cores(&ids, |p, e| batch.report_step(p, e.report))
            }
            Command::RemoveCores(ids) => {
                p.remove_cores(&ids, |p, e| batch.report_step(p, e.report))
            }
            Command::PutBios(row, from) => {
                p.put_bios_files(&[(row, from)], |p, e| batch.report_step(p, e.report))
            }
            Command::RenameGamesInLibrary(games) => {
                p.rename_games_in_library(&games, |p, e| batch.report_step(p, e.report))
            }
            Command::RenameOnDevice(game, new) => {
                p.rename_games_on_device(&[(game, new)], |p, e| batch.report_step(p, e.report))
            }
            Command::SyncDats => {
                let synced = p.sync_dats();
                Ok(batch.report_step(p, OperationReport::SyncDats(synced)))
            }
            Command::Reload => {
                let read = p.reload();
                let read = read.map(|report| send_duplicates(self.send, p, &report.duplicates));
                Ok(batch.report_step(p, OperationReport::Reload(read)))
            }
            Command::Eject => {
                let root = p.device().map(|d| d.root().to_path_buf());
                let ejected = p.eject_device();
                Ok(batch.report_step(p, OperationReport::Eject(root, ejected)))
            }
        };
        let (mut summary, mut failed) = summary(label, batch.total, &batch.reports);
        if let Err(e) = batch_result {
            summary.push_str(&format!("; {e}"));
            failed = true;
        }
        self.finish_with_refresh(p, summary, failed);
    }
}

/// A command under way.
struct Batch<'a> {
    send: &'a dyn Fn(Update),
    orphans: &'a Orphans,
    cancel_token: &'a AtomicBool,
    total: usize,
    /// What it has done so far.
    reports: Vec<OperationReport>,
}

impl Batch<'_> {
    /// Notes how one game went and sends how things stand; breaks if the
    /// command was canceled.
    fn report_step(&mut self, p: &Pocketeer, report: OperationReport) -> ControlFlow<()> {
        let game = report.subject();
        self.reports.push(report);
        (self.send)(Update::Snapshot(snapshot(p, self.orphans)));
        (self.send)(Update::Progress {
            done: self.reports.len(),
            total: self.total,
            game,
        });
        if self.cancel_token.load(Ordering::Relaxed) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    }
}

/// How a command labeled `label`, meant for `total` games, went; and whether
/// any of it failed.
fn summary(label: &str, total: usize, reports: &[OperationReport]) -> (String, bool) {
    let done = reports.len();
    let failed = reports.iter().filter(|r| !r.is_ok()).count();
    let first_failure = reports.iter().find(|r| !r.is_ok());
    let summary = match (total, first_failure, reports.last()) {
        (1, _, Some(last)) => last.to_line(),
        (_, Some(first), _) => format!(
            "{label}: {done} of {total} done, {failed} failed; first: {}",
            first.to_line()
        ),
        _ if done < total => format!("{label}: stopped after {done} of {total}"),
        _ => format!("{label}: {done} done"),
    };
    (summary, failed > 0)
}

fn snapshot(p: &Pocketeer, orphans: &Orphans) -> Snapshot {
    let mut rows: Vec<Row> = p
        .game_rows()
        .into_iter()
        .map(|row| Row::Game {
            standard_name: p.find_dat_entry(row.game()).map(|d| d.name.clone()),
            files: row
                .local_entry()
                .map_or(Vec::new(), |l| p.library().files_of(l)),
            row,
        })
        .collect();
    rows.extend(orphans.in_library.iter().cloned().map(Row::OrphanInLibrary));
    rows.extend(orphans.on_device.iter().cloned().map(Row::OrphanOnDevice));
    Snapshot {
        rows,
        device: p.device().map(|d| d.root().to_path_buf()),
    }
}

/// Tells the UI of `duplicates`, the copies a scan left out, if there are any.
fn send_duplicates(send: &dyn Fn(Update), p: &Pocketeer, duplicates: &[DeviceEntry]) {
    if duplicates.is_empty() {
        return;
    }
    let lines = duplicates.iter().map(|d| {
        let first = p.device().and_then(|device| device.entry_for(&d.game));
        let first = first.map_or(String::new(), device_ref);
        format!("{} is the same game as {first}", device_ref(d))
    });
    send(Update::Duplicates(lines.collect()));
}

fn job_label(command: &Command) -> &'static str {
    match command {
        Command::PullGames(_) => "pulling games",
        Command::PullSaves(_) => "backing up saves",
        Command::PushGames(_) => "pushing games",
        Command::PushSaves(_) => "pushing saves",
        Command::PushSaveVersion(..) => "pushing a save",
        Command::FetchCovers(_) => "downloading covers",
        Command::ImportGame(..) => "importing games",
        Command::ImportSave(..) => "importing a save",
        Command::ImportCover(..) => "importing a cover",
        Command::TrashGames(_) => "putting games in the trash",
        Command::TrashSaveVersion(..) => "putting a save in the trash",
        Command::TrashOrphanSaves(_) => "putting saves in the trash",
        Command::DeleteGames(_) => "deleting games",
        Command::DeleteSaves(_) | Command::DeleteOrphanSaves(_) => "deleting saves",
        Command::SyncDats => "updating the DATs",
        Command::Reload => "reading everything again",
        Command::Eject => "ejecting the device",
        Command::ShowCores => "reading the cores",
        Command::InstallCores(_) => "installing cores",
        Command::RemoveCores(_) => "removing cores",
        Command::PutBios(..) => "putting a file on the device",
        Command::RenameGamesInLibrary(_) => "renaming games",
        Command::RenameOnDevice(..) => "renaming a game",
    }
}

fn target_count(command: &Command) -> usize {
    match command {
        Command::PullGames(g)
        | Command::PullSaves(g)
        | Command::PushSaves(g)
        | Command::DeleteGames(g)
        | Command::DeleteSaves(g) => g.len(),
        Command::PushGames(g) | Command::FetchCovers(g) | Command::TrashGames(g) => g.len(),
        Command::TrashOrphanSaves(o) => o.len(),
        Command::InstallCores(ids) | Command::RemoveCores(ids) => ids.len(),
        Command::ShowCores => 0,
        Command::DeleteOrphanSaves(o) => o.len(),
        Command::RenameGamesInLibrary(g) => g.len(),
        Command::PushSaveVersion(..)
        | Command::PutBios(..)
        | Command::RenameOnDevice(..)
        | Command::ImportGame(..)
        | Command::ImportSave(..)
        | Command::ImportCover(..)
        | Command::TrashSaveVersion(..)
        | Command::SyncDats
        | Command::Reload
        | Command::Eject => 1,
    }
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

    /// The worker on `library` and `device`, and what it tells.
    fn worker_on(library: PathBuf, device: Option<PathBuf>) -> (Worker, Receiver<Update>) {
        let (tx, rx) = mpsc::channel();
        let worker = Worker::spawn(library, device, move |u| {
            let _ = tx.send(u);
        });
        (worker, rx)
    }

    /// The summary, and whether it failed, of the command `updates` end with.
    fn finished(updates: &[Update]) -> (&str, bool) {
        let Some(Update::Finished { summary, failed }) = updates.last() else {
            panic!("{updates:?}")
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
    fn started() -> (tempfile::TempDir, Worker, Receiver<Update>) {
        let (dir, library, device) = empty_library_and_device();
        put(&library, "roms/gba/A.gba", b"a");
        put(&library, "roms/gba/B.gba", b"b");
        put(&device, "Assets/gba/common/C.gba", b"c");
        put(&device, "Saves/gba/common/Gone.sav", b"orphan");
        let (worker, rx) = worker_on(library, Some(device));
        (dir, worker, rx)
    }

    /// Every update up to the first that `is_last`.
    fn updates_until(rx: &Receiver<Update>, is_last: impl Fn(&Update) -> bool) -> Vec<Update> {
        let mut updates = Vec::new();
        loop {
            let update = rx.recv_timeout(Duration::from_secs(10)).unwrap();
            let last = is_last(&update);
            updates.push(update);
            if last {
                return updates;
            }
        }
    }

    /// Every update up to the end of the next command.
    fn until_finished(rx: &Receiver<Update>) -> Vec<Update> {
        updates_until(rx, |u| matches!(u, Update::Finished { .. }))
    }

    /// Every update up to the device having been read.
    fn until_device_read(rx: &Receiver<Update>) -> Vec<Update> {
        updates_until(rx, |u| *u == Update::DeviceRead)
    }

    fn last_snapshot(updates: &[Update]) -> &Snapshot {
        updates
            .iter()
            .rev()
            .find_map(|u| match u {
                Update::Snapshot(s) => Some(s),
                _ => None,
            })
            .unwrap()
    }

    fn locals(snapshot: &Snapshot) -> Vec<LocalEntry> {
        snapshot
            .rows
            .iter()
            .filter_map(Row::local_entry)
            .cloned()
            .collect()
    }

    /// A device with two copies of a game, the worker on it.
    fn started_with_two_copies() -> (tempfile::TempDir, Worker, Receiver<Update>) {
        let (dir, library, device) = empty_library_and_device();
        put(&device, "Assets/gba/common/One.gba", b"c");
        put(&device, "Assets/gba/common/RPG/Two.gba", b"c");
        let (worker, rx) = worker_on(library, Some(device));
        (dir, worker, rx)
    }

    /// A device with the core `a.B`, and an inventory just written, so that it
    /// isn't asked for again, offering `Spiritualized.GB` from nowhere.
    fn started_with_cores() -> (tempfile::TempDir, Worker, Receiver<Update>) {
        let (dir, library, device) = empty_library_and_device();
        put(
            &library,
            "cores.json",
            br#"[{"id":"Spiritualized.GB","version":"2.0","platforms":["gb"],"download_url":"http://127.0.0.1:1/never"}]"#,
        );
        put(&device, "Cores/a.B/core.json", b"{}");
        let (worker, rx) = worker_on(library, Some(device));
        (dir, worker, rx)
    }

    /// The ids of the cores last told of in `updates`.
    fn cores_told(updates: &[Update]) -> Option<Vec<String>> {
        updates.iter().rev().find_map(|u| match u {
            Update::Cores { rows, .. } => Some(rows.iter().map(|r| r.id().to_string()).collect()),
            _ => None,
        })
    }

    /// Every update up to the cores being told of.
    fn until_cores(rx: &Receiver<Update>) -> Vec<Update> {
        updates_until(rx, |u| matches!(u, Update::Cores { .. }))
    }

    /// A device with a game that can't be read, the worker on it: a folder
    /// is where its save goes.
    fn started_with_unreadable_device() -> (tempfile::TempDir, Worker, Receiver<Update>) {
        let (dir, library, device) = empty_library_and_device();
        put(&device, "Assets/gba/common/One.gba", b"c");
        put(&device, "Saves/gba/common/One.sav/x", b"");
        let (worker, rx) = worker_on(library, Some(device));
        (dir, worker, rx)
    }

    fn duplicates(updates: &[Update]) -> Vec<&[String]> {
        updates
            .iter()
            .filter_map(|u| match u {
                Update::Duplicates(lines) => Some(&lines[..]),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn reports_duplicates_on_open_and_reload() {
        let (_dir, mut worker, rx) = started_with_two_copies();
        let lines = ["gba/RPG/Two.gba is the same game as gba/One.gba".to_string()];
        let opening = until_device_read(&rx);
        assert_eq!(duplicates(&opening), [&lines[..]]);
        assert_eq!(last_snapshot(&opening).rows.len(), 1);
        worker.send(Command::Reload);
        assert_eq!(duplicates(&until_finished(&rx)), [&lines[..]]);
    }

    #[test]
    fn shows_next_copy_without_warning_after_deleting_first() {
        let (_dir, mut worker, rx) = started_with_two_copies();
        let opening = until_device_read(&rx);
        let first = last_snapshot(&opening).rows[0]
            .device_entry()
            .unwrap()
            .clone();
        assert_eq!(first.path, Path::new("One.gba"));
        worker.send(Command::DeleteGames(vec![first]));
        let updates = until_finished(&rx);
        assert_eq!(duplicates(&updates), Vec::<&[String]>::new());
        let rows = &last_snapshot(&updates).rows;
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].device_entry().unwrap().path,
            Path::new("RPG/Two.gba")
        );
    }

    #[test]
    fn reports_only_failure_when_library_cannot_open() {
        let dir = tempfile::tempdir().unwrap();
        let (_worker, rx) = worker_on(dir.path().join("typo"), None);
        let told = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(matches!(told, Update::LibraryFailed(_)), "{told:?}");
        // The worker has gone, and its end of the channel with it.
        assert!(rx.recv_timeout(Duration::from_secs(10)).is_err());
    }

    #[test]
    fn opens_device_and_sends_every_row() {
        let (_dir, _worker, rx) = started();
        let opening = until_device_read(&rx);
        let snapshot = last_snapshot(&opening);
        assert!(snapshot.device.is_some());
        // A, B, C and the orphan save.
        assert_eq!(snapshot.rows.len(), 4);
        assert!(matches!(snapshot.rows[3], Row::OrphanOnDevice(_)));
        assert!(matches!(opening[1], Update::ReadingDevice(_)));
        // Reading it well is no job to report on.
        let is_job = |u: &Update| matches!(u, Update::Started { .. } | Update::Finished { .. });
        assert!(!opening.iter().any(is_job));
    }

    #[test]
    fn reports_why_device_cannot_be_read() {
        let (_dir, _worker, rx) = started_with_unreadable_device();
        let updates = until_finished(&rx);
        let (summary, failed) = finished(&updates);
        assert!(failed);
        assert!(
            summary.starts_with("couldn't read the device: "),
            "{summary}"
        );
        assert!(updates.contains(&Update::DeviceRead));
        assert_eq!(last_snapshot(&updates).device, None);
    }

    #[test]
    fn reports_progress_for_each_game() {
        let (_dir, mut worker, rx) = started();
        let opening = until_device_read(&rx);
        let games = locals(last_snapshot(&opening));
        worker.send(Command::PushGames(games));
        let updates = until_finished(&rx);
        let progress: Vec<_> = updates
            .iter()
            .filter_map(|u| match u {
                Update::Progress { done, total, game } => Some((*done, *total, game.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(progress, [(1, 2, "gba/A.gba"), (2, 2, "gba/B.gba")]);
        assert_eq!(finished(&updates), ("pushing games: 2 done", false));
        let snapshot = last_snapshot(&updates);
        let on_device = snapshot.rows.iter().filter(|r| r.device_entry().is_some());
        assert_eq!(on_device.count(), 3);
    }

    #[test]
    fn skips_canceled_commands() {
        let (dir, library, _) = empty_library_and_device();
        let (tx, commands) = mpsc::channel();
        let canceled = Arc::new(AtomicBool::new(true));
        tx.send((canceled, Command::PullSaves(Vec::new()))).unwrap();
        tx.send((Arc::default(), Command::Reload)).unwrap();
        // With nothing left to come, `run` returns.
        drop(tx);
        let labels = std::cell::RefCell::new(Vec::new());
        // A device that isn't there: this must not read one that is mounted.
        let no_device = Some(dir.path().join("no device"));
        run(&library, no_device, &commands, &|update| {
            if let Update::Started { label, .. } = update {
                labels.borrow_mut().push(label);
            }
        });
        assert_eq!(*labels.borrow(), ["reading everything again"]);
    }

    #[test]
    fn adds_row_for_imported_game() {
        let (dir, mut worker, rx) = started();
        let opening = until_device_read(&rx);
        let before = locals(last_snapshot(&opening)).len();
        let from = put(dir.path(), "elsewhere/New.bin", b"new");
        worker.send(Command::ImportGame(from, Some(Platform::Gb)));
        let updates = until_finished(&rx);
        assert!(!finished(&updates).1);
        assert_eq!(locals(last_snapshot(&updates)).len(), before + 1);
    }

    #[test]
    fn counts_games_in_imported_dir() {
        let (dir, mut worker, rx) = started();
        until_device_read(&rx);
        put(dir.path(), "elsewhere/New.gb", b"new");
        put(dir.path(), "elsewhere/deep/Other.gb", b"other");
        worker.send(Command::ImportGame(dir.path().join("elsewhere"), None));
        let updates = until_finished(&rx);
        assert_eq!(finished(&updates), ("importing games: 2 done", false));
    }

    #[test]
    fn deletes_orphan_save_from_device() {
        let (_dir, mut worker, rx) = started();
        let opening = until_device_read(&rx);
        let Row::OrphanOnDevice(orphan) = last_snapshot(&opening).rows[3].clone() else {
            panic!()
        };
        worker.send(Command::DeleteOrphanSaves(vec![orphan]));
        let updates = until_finished(&rx);
        assert_eq!(
            finished(&updates),
            ("gba/Gone.sav: save deleted from the device", false)
        );
        assert_eq!(last_snapshot(&updates).rows.len(), 3);
    }

    #[test]
    fn reports_renamed_game_under_new_name() {
        let (_dir, mut worker, rx) = started();
        let opening = until_device_read(&rx);
        let local = locals(last_snapshot(&opening)).remove(0);
        worker.send(Command::RenameGamesInLibrary(vec![(
            local,
            "Renamed".to_string(),
        )]));
        let updates = until_finished(&rx);
        assert_eq!(
            finished(&updates),
            ("gba/A.gba: renamed to gba/Renamed.gba", false)
        );
        let names: Vec<String> = locals(last_snapshot(&updates))
            .into_iter()
            .map(|l| l.name)
            .collect();
        assert_eq!(names, ["B.gba", "Renamed.gba"]);
    }

    #[test]
    fn counts_games_renamed_in_library() {
        let (_dir, mut worker, rx) = started();
        let opening = until_device_read(&rx);
        let renames = locals(last_snapshot(&opening))
            .into_iter()
            .map(|local| {
                let new = format!("New {}", local.stem());
                (local, new)
            })
            .collect();
        worker.send(Command::RenameGamesInLibrary(renames));
        let updates = until_finished(&rx);
        assert_eq!(finished(&updates), ("renaming games: 2 done", false));
        let names: Vec<String> = locals(last_snapshot(&updates))
            .into_iter()
            .map(|l| l.name)
            .collect();
        assert_eq!(names, ["New A.gba", "New B.gba"]);
    }

    #[test]
    fn cancels_only_commands_sent_before_cancel() {
        let (mut worker, rx) = Worker::recording();
        worker.send(Command::Reload);
        worker.send(Command::SyncDats);
        worker.cancel();
        worker.send(Command::Eject);
        let flags: Vec<bool> = rx
            .try_iter()
            .map(|(cancel_token, _)| cancel_token.load(Ordering::Relaxed))
            .collect();
        assert_eq!(flags, [true, true, false]);
    }

    #[test]
    fn ends_thread_when_dropped() {
        let (_dir, worker, rx) = started();
        until_device_read(&rx);
        drop(worker);
        // The thread held the other end.
        while rx.try_recv().is_ok() {}
        assert_eq!(rx.try_recv(), Err(mpsc::TryRecvError::Disconnected));
    }

    #[test]
    fn finishes_at_most_current_game_when_dropped() {
        let (dir, mut worker, rx) = started();
        let games = locals(last_snapshot(&until_device_read(&rx)));
        worker.send(Command::PushGames(games));
        drop(worker);
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
        let log = fs::read_to_string(dir.path().join("library/pocketeer.log")).unwrap_or_default();
        assert_eq!(log.lines().count(), on_device);
    }

    /// The files last told of in `updates`.
    fn bios_told(updates: &[Update]) -> &[BiosRow] {
        let told = updates.iter().rev().find_map(|u| match u {
            Update::Bios(rows) => Some(rows),
            _ => None,
        });
        told.unwrap()
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
        let (mut worker, rx) = worker_on(library, Some(device.clone()));
        let opening = until_device_read(&rx);
        let told = bios_told(&opening);
        assert_eq!(told[0].status, BiosStatus::Missing);
        worker.send(Command::PutBios(told[0].clone(), from));
        let updates = until_finished(&rx);
        assert_eq!(
            finished(&updates),
            ("gba/gba_bios.bin: put on the device", false)
        );
        assert_eq!(bios_told(&updates)[0].status, BiosStatus::Present);
        assert!(device.join("Assets/gba/common/gba_bios.bin").exists());
    }

    #[test]
    fn sends_cores_only_after_page_opens() {
        let (_dir, mut worker, rx) = started_with_cores();
        let opening = until_device_read(&rx);
        assert_eq!(cores_told(&opening), None);
        worker.send(Command::Reload);
        assert_eq!(cores_told(&until_finished(&rx)), None);
        worker.send(Command::ShowCores);
        let told = cores_told(&until_cores(&rx)).unwrap();
        assert_eq!(told, ["Spiritualized.GB", "a.B"]);
    }

    #[test]
    fn drops_removed_core_from_cores_update() {
        let (_dir, mut worker, rx) = started_with_cores();
        until_device_read(&rx);
        worker.send(Command::ShowCores);
        until_cores(&rx);
        worker.send(Command::RemoveCores(vec!["a.B".to_string()]));
        let updates = until_finished(&rx);
        assert_eq!(finished(&updates), ("a.B: removed from the device", false));
        assert_eq!(cores_told(&updates).unwrap(), ["Spiritualized.GB"]);
    }

    #[test]
    fn reports_failed_core_download() {
        let (_dir, mut worker, rx) = started_with_cores();
        until_device_read(&rx);
        worker.send(Command::InstallCores(vec!["Spiritualized.GB".to_string()]));
        let updates = until_finished(&rx);
        let (summary, failed) = finished(&updates);
        assert!(failed);
        assert!(
            summary.starts_with("Spiritualized.GB: error: "),
            "{summary}"
        );
    }

    #[test]
    fn summarizes_how_command_went() {
        let ok = || OperationReport::Reload(Ok(()));
        let bad = || OperationReport::Reload(Err(pocketeer_core::error::SharedError::NoDevice));
        assert_eq!(
            summary("reading", 1, &[ok()]),
            ("everything: read again".to_string(), false)
        );
        assert_eq!(
            summary("reading", 2, &[ok(), ok()]),
            ("reading: 2 done".to_string(), false)
        );
        assert_eq!(
            summary("reading", 3, &[ok()]),
            ("reading: stopped after 1 of 3".to_string(), false)
        );
        assert_eq!(
            summary("reading", 2, &[ok(), bad()]),
            (
                "reading: 2 of 2 done, 1 failed; first: everything: error: no device is open"
                    .to_string(),
                true
            )
        );
    }

    #[test]
    fn logs_commands_with_tui_front_end() {
        let (dir, mut worker, rx) = started();
        let opening = until_device_read(&rx);
        let games = locals(last_snapshot(&opening));
        worker.send(Command::PushGames(games));
        until_finished(&rx);
        let log = fs::read_to_string(dir.path().join("library/pocketeer.log")).unwrap();
        assert!(log.contains(r#""front_end":"tui""#), "{log}");
    }

    #[test]
    fn stops_command_and_reports_when_log_cannot_be_written() {
        let (dir, mut worker, rx) = started();
        let opening = until_device_read(&rx);
        let games = locals(last_snapshot(&opening));
        fs::create_dir(dir.path().join("library/pocketeer.log")).unwrap();
        worker.send(Command::PushGames(games));
        let updates = until_finished(&rx);
        let done = updates
            .iter()
            .filter(|u| matches!(u, Update::Progress { .. }))
            .count();
        assert_eq!(done, 1);
        let (summary, failed) = finished(&updates);
        assert!(failed);
        assert!(summary.contains("couldn't write the log"), "{summary}");
    }
}
