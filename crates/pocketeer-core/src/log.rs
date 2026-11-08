//! What an operation did and how it went, as the front ends show it and the log
//! file keeps it.

use std::{
    io::{self, Write},
    path::{Path, PathBuf},
};

use fs_err::OpenOptions;
use jiff::Timestamp;
use serde_json::{Value, json};

use crate::{
    cores::{BiosRow, InstallCoreError, InstallCoreOutcome, PutBiosError, RemoveCoreError},
    dat::SyncReport,
    device::{
        DeleteOrphanSaveError, DeleteSaveOutcome, DeviceEntry, EjectDeviceError, OrphanDeviceSave,
        RenameOnDeviceError,
    },
    error::BasicError,
    library::{
        AddGameError, AddGameOutcome, LocalEntry, OrphanLocalSave, RenameInLibraryError,
        TrashOrphanSaveError, TrashSaveAllOutcome, TrashSaveError,
    },
    pocketeer::{FetchCoverOutcome, ImportCoverError, ImportSaveOutcome},
    transfer::{PullSaveOutcome, PushGameError, PushGameOutcome, PushSaveError, PushSaveOutcome},
};

/// What was done, to what, and how it went.
pub struct OperationReport {
    /// What it was done to, in words for a person.
    pub subject: String,
    /// Whether it went well. Finding nothing to do is an outcome, not an error.
    pub is_ok: bool,
    pub message: String,
    /// What the JSON says besides these, such as where a game went.
    fields: Vec<(&'static str, String)>,
}

impl OperationReport {
    fn new<T, E>(
        subject: String,
        result: &Result<T, E>,
        message: String,
        fields: Vec<(&'static str, String)>,
    ) -> Self {
        Self {
            subject,
            is_ok: result.is_ok(),
            message,
            fields,
        }
    }

    /// `<subject>: <message>`.
    pub fn to_line(&self) -> String {
        format!("{}: {}", self.subject, self.message)
    }

    // ---- Games ------------------------------------------------------------------

    pub(crate) fn pull_game(
        game: &DeviceEntry,
        result: &Result<AddGameOutcome, AddGameError>,
    ) -> Self {
        let mut fields = Vec::new();
        let message = match result {
            Ok(AddGameOutcome::Added(entry)) => {
                fields.push(("library", entry.to_string()));
                format!("pulled as {entry}")
            }
            Ok(AddGameOutcome::AlreadyHad(entry)) => {
                fields.push(("library", entry.to_string()));
                format!("the library has it already, as {entry}")
            }
            Err(AddGameError::Other(error)) => format!("error: {error}"),
            Err(error) => format!("not pulled: {error}"),
        };
        Self::new(game.to_string(), result, message, fields)
    }

    pub(crate) fn push_game(
        game: &LocalEntry,
        result: &Result<PushGameOutcome, PushGameError>,
    ) -> Self {
        let mut fields = Vec::new();
        let message = match result {
            Ok(PushGameOutcome::Pushed(entry)) => {
                fields.push(("device", entry.to_string()));
                format!("pushed to {entry}")
            }
            Ok(PushGameOutcome::AlreadyThere(entry)) => {
                fields.push(("device", entry.to_string()));
                format!("on the device already, at {entry}")
            }
            Err(PushGameError::Other(error)) => format!("error: {error}"),
            Err(error) => error.to_string(),
        };
        Self::new(game.to_string(), result, message, fields)
    }

    /// `from` is the file it was imported from.
    pub(crate) fn import_game(from: &Path, result: &Result<AddGameOutcome, AddGameError>) -> Self {
        let mut fields = vec![("from", from.display().to_string())];
        let message = match result {
            Ok(AddGameOutcome::Added(entry)) => {
                fields.push(("library", entry.to_string()));
                format!("imported as {entry}")
            }
            Ok(AddGameOutcome::AlreadyHad(entry)) => {
                fields.push(("library", entry.to_string()));
                format!("the library has it already, as {entry}")
            }
            Err(AddGameError::Other(error)) => format!("error: {error}"),
            Err(error) => format!("not imported: {error}"),
        };
        // Its path is in the JSON, since here it would crowd out what comes
        // after.
        let name = from.file_name().unwrap_or(from.as_os_str());
        let subject = name.to_string_lossy().into_owned();
        Self::new(subject, result, message, fields)
    }

    pub(crate) fn rename_in_library(
        game: &LocalEntry,
        result: &Result<LocalEntry, RenameInLibraryError>,
    ) -> Self {
        let mut fields = Vec::new();
        let message = match result {
            Ok(entry) => {
                fields.push(("library", entry.to_string()));
                format!("renamed to {entry}")
            }
            Err(RenameInLibraryError::Other(error)) => format!("error: {error}"),
            Err(error) => format!("not renamed: {error}"),
        };
        Self::new(game.to_string(), result, message, fields)
    }

    pub(crate) fn rename_on_device(
        game: &DeviceEntry,
        result: &Result<DeviceEntry, RenameOnDeviceError>,
    ) -> Self {
        let mut fields = Vec::new();
        let message = match result {
            Ok(entry) => {
                fields.push(("device", entry.to_string()));
                format!("renamed to {entry}")
            }
            Err(RenameOnDeviceError::Other(error)) => format!("error: {error}"),
            Err(error) => format!("not renamed: {error}"),
        };
        Self::new(game.to_string(), result, message, fields)
    }

    pub(crate) fn trash_game(game: &LocalEntry, result: &Result<(), BasicError>) -> Self {
        let message = match result {
            Ok(()) => "put in the trash".to_string(),
            Err(error) => format!("error: {error}"),
        };
        Self::new(game.to_string(), result, message, Vec::new())
    }

    pub(crate) fn delete_game(game: &DeviceEntry, result: &Result<(), BasicError>) -> Self {
        let message = match result {
            Ok(()) => "deleted from the device".to_string(),
            Err(error) => format!("error: {error}"),
        };
        Self::new(game.to_string(), result, message, Vec::new())
    }

    // ---- Saves ------------------------------------------------------------------

    pub(crate) fn pull_save(
        game: &DeviceEntry,
        result: &Result<PullSaveOutcome, BasicError>,
    ) -> Self {
        let message = match result {
            Ok(PullSaveOutcome::Pulled) => "save kept".to_string(),
            Ok(PullSaveOutcome::AlreadyHad) => "the library has this save already".to_string(),
            Ok(PullSaveOutcome::NoSave) => "no save on the device".to_string(),
            Err(error @ BasicError::NotInLibrary) => format!("{error}; pull the game first"),
            Err(error) => format!("error: {error}"),
        };
        Self::new(game.to_string(), result, message, Vec::new())
    }

    pub(crate) fn push_save(
        game: &DeviceEntry,
        result: &Result<PushSaveOutcome, PushSaveError>,
    ) -> Self {
        let mut fields = Vec::new();
        let message = match result {
            Ok(PushSaveOutcome::Pushed(save)) => {
                fields.push(("version", save.name.clone()));
                format!("save {} pushed", save.name)
            }
            Ok(PushSaveOutcome::AlreadyThere) => "the device has this save already".to_string(),
            Err(error @ PushSaveError::NotBackedUp) => format!("{error}; back it up first"),
            Err(PushSaveError::Other(error)) => format!("error: {error}"),
            Err(error) => error.to_string(),
        };
        Self::new(game.to_string(), result, message, fields)
    }

    /// `from` is the file it was imported from.
    pub(crate) fn import_save(
        game: &LocalEntry,
        from: &Path,
        result: &Result<ImportSaveOutcome, BasicError>,
    ) -> Self {
        let mut fields = vec![("from", from.display().to_string())];
        let message = match result {
            Ok(ImportSaveOutcome::Imported(save)) => {
                fields.push(("version", save.name.clone()));
                format!("save {} imported", save.name)
            }
            Ok(ImportSaveOutcome::AlreadyHad) => "the library has this save already".to_string(),
            Err(error) => format!("error: {error}"),
        };
        Self::new(game.to_string(), result, message, fields)
    }

    pub(crate) fn trash_every_version(
        game: &LocalEntry,
        result: &Result<TrashSaveAllOutcome, BasicError>,
    ) -> Self {
        let message = match result {
            Ok(TrashSaveAllOutcome::Trashed) => "save put in the trash".to_string(),
            Ok(TrashSaveAllOutcome::NoSave) => "no save in the library".to_string(),
            Err(error) => format!("error: {error}"),
        };
        Self::new(game.to_string(), result, message, Vec::new())
    }

    pub(crate) fn trash_save_version(
        game: &LocalEntry,
        version_name: &str,
        result: &Result<(), TrashSaveError>,
    ) -> Self {
        let message = match result {
            Ok(()) => "save put in the trash".to_string(),
            Err(TrashSaveError::Other(error)) => format!("error: {error}"),
            Err(error) => error.to_string(),
        };
        let fields = vec![("version", version_name.to_string())];
        Self::new(game.to_string(), result, message, fields)
    }

    pub(crate) fn trash_orphan_save(
        orphan: &OrphanLocalSave,
        result: &Result<(), TrashOrphanSaveError>,
    ) -> Self {
        let message = match result {
            Ok(()) => "save put in the trash".to_string(),
            Err(TrashOrphanSaveError::Other(error)) => format!("error: {error}"),
            Err(error) => error.to_string(),
        };
        Self::new(orphan.to_string(), result, message, Vec::new())
    }

    pub(crate) fn delete_save(
        game: &DeviceEntry,
        result: &Result<DeleteSaveOutcome, BasicError>,
    ) -> Self {
        let message = match result {
            Ok(DeleteSaveOutcome::Deleted) => "save deleted from the device".to_string(),
            Ok(DeleteSaveOutcome::NoSave) => "no save on the device".to_string(),
            Err(error) => format!("error: {error}"),
        };
        Self::new(game.to_string(), result, message, Vec::new())
    }

    pub(crate) fn delete_orphan_save(
        orphan: &OrphanDeviceSave,
        result: &Result<(), DeleteOrphanSaveError>,
    ) -> Self {
        let message = match result {
            Ok(()) => "save deleted from the device".to_string(),
            Err(DeleteOrphanSaveError::Other(error)) => format!("error: {error}"),
            Err(error) => error.to_string(),
        };
        Self::new(orphan.to_string(), result, message, Vec::new())
    }

    // ---- Covers -----------------------------------------------------------------

    pub(crate) fn fetch_cover(
        game: &LocalEntry,
        result: &Result<FetchCoverOutcome, BasicError>,
    ) -> Self {
        let message = match result {
            Ok(FetchCoverOutcome::Fetched) => "cover downloaded".to_string(),
            Ok(FetchCoverOutcome::AlreadyHad) => "it has a cover already".to_string(),
            Ok(FetchCoverOutcome::NoCover) => "no cover found for it".to_string(),
            Err(error) => format!("error: {error}"),
        };
        Self::new(game.to_string(), result, message, Vec::new())
    }

    /// `from` is the file it was imported from.
    pub(crate) fn import_cover(
        game: &LocalEntry,
        from: &Path,
        result: &Result<(), ImportCoverError>,
    ) -> Self {
        let message = match result {
            Ok(()) => "cover imported".to_string(),
            Err(ImportCoverError::Other(error)) => format!("error: {error}"),
            Err(error) => error.to_string(),
        };
        let fields = vec![("from", from.display().to_string())];
        Self::new(game.to_string(), result, message, fields)
    }

    // ---- Cores and BIOS files ---------------------------------------------------

    pub(crate) fn install_core(
        id: &str,
        result: &Result<InstallCoreOutcome, InstallCoreError>,
    ) -> Self {
        let mut fields = Vec::new();
        let message = match result {
            Ok(InstallCoreOutcome::Installed(version)) => {
                fields.push(("version", version.clone()));
                format!("{version} installed")
            }
            Ok(InstallCoreOutcome::AlreadyCurrent) => {
                "the device has the newest version".to_string()
            }
            Err(InstallCoreError::Other(error)) => format!("error: {error}"),
            Err(error) => error.to_string(),
        };
        Self::new(id.to_string(), result, message, fields)
    }

    pub(crate) fn remove_core(id: &str, result: &Result<(), RemoveCoreError>) -> Self {
        let message = match result {
            Ok(()) => "removed from the device".to_string(),
            Err(RemoveCoreError::Other(error)) => format!("error: {error}"),
            Err(error) => error.to_string(),
        };
        Self::new(id.to_string(), result, message, Vec::new())
    }

    /// `from` is the file it was copied from.
    pub(crate) fn put_bios(row: &BiosRow, from: &Path, result: &Result<(), PutBiosError>) -> Self {
        let message = match result {
            Ok(()) => "put on the device".to_string(),
            Err(PutBiosError::Other(error)) => format!("error: {error}"),
            Err(error) => error.to_string(),
        };
        let fields = vec![("from", from.display().to_string())];
        Self::new(row.to_string(), result, message, fields)
    }

    // ---- Operations on everything -----------------------------------------------

    pub fn sync_dats(result: &Result<SyncReport, BasicError>) -> Self {
        let message = match result {
            Ok(report) => format!("{} platforms updated", report.updated.len()),
            Err(error) => format!("error: {error}"),
        };
        Self::new("the DATs".to_string(), result, message, Vec::new())
    }

    pub fn reload<T>(result: &Result<T, BasicError>) -> Self {
        let message = match result {
            Ok(_) => "read again".to_string(),
            Err(error) => format!("error: {error}"),
        };
        Self::new("everything".to_string(), result, message, Vec::new())
    }

    /// When it succeeds, `result` holds the root of the device that was
    /// ejected.
    pub fn eject(result: &Result<PathBuf, EjectDeviceError>) -> Self {
        let message = match result {
            Ok(root) => format!("ejected {}", root.display()),
            Err(error) => format!("error: {error}"),
        };
        Self::new("the device".to_string(), result, message, Vec::new())
    }
}

#[derive(Debug, Clone, Copy)]
pub enum FrontEnd {
    Cli,
    Tui,
}

/// A whole record of what happened, when, and through which front end.
pub struct LogEntry {
    pub time: Timestamp,
    pub front_end: FrontEnd,
    pub report: OperationReport,
}

impl LogEntry {
    pub fn to_json(&self) -> Value {
        let front_end = match self.front_end {
            FrontEnd::Cli => "cli",
            FrontEnd::Tui => "tui",
        };
        let mut json = json!({
            "time": self.time.to_string(),
            "front_end": front_end,
            "game": self.report.subject,
            "ok": self.report.is_ok,
            "message": self.report.message,
        });
        for (name, value) in &self.report.fields {
            json[*name] = json!(value);
        }
        json
    }
}

/// What a logged operation returns when it fails: its own failure, or the
/// log's.
///
/// Neither variant converts by itself: `E` could be an `io::Error`, and then
/// both would.
#[derive(Debug, thiserror::Error)]
pub enum LoggedError<E> {
    #[error(transparent)]
    OperationFailed(E),
    /// The operation is done, but it couldn't be noted in the log.
    #[error("couldn't write the log: {0}")]
    LogFailed(#[source] io::Error),
}

/// Adds each event to the log file as it comes.
pub(crate) struct Logger {
    path: PathBuf,
    front_end: FrontEnd,
}

impl Logger {
    pub(crate) fn new(path: PathBuf, front_end: FrontEnd) -> Self {
        Self { path, front_end }
    }

    /// Adds `report` to the file with the time and this front end.
    ///
    /// Returns the entry together with whether it was written, rather than one
    /// or the other, because the operation is done either way and the front end
    /// still has to show it.
    pub(crate) fn record(&self, report: OperationReport) -> (LogEntry, io::Result<()>) {
        let entry = LogEntry {
            time: Timestamp::now(),
            front_end: self.front_end,
            report,
        };
        let log_write_result = self.write(&entry);
        (entry, log_write_result)
    }

    fn write(&self, entry: &LogEntry) -> io::Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(file, "{}", entry.to_json())
    }
}

/// Helpers shared by the tests of modules that write the log.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use std::path::Path;

    use serde_json::Value;

    /// The log at `path`, one value for each line.
    pub fn logged_lines(path: &Path) -> Vec<Value> {
        fs_err::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{testing::logged_lines, *};
    use crate::{
        cores::BiosStatus,
        game::{Files, Game, Rom, Save},
        library::LibrarySave,
        platform::Platform,
    };

    fn game() -> Game {
        Game {
            platform: Platform::Gba,
            files: Files::Single(Rom { size: 1, crc32: 1 }),
        }
    }

    fn local_entry() -> LocalEntry {
        LocalEntry {
            name: "Foo.gba".into(),
            game: game(),
            cover_path: None,
            save: None,
        }
    }

    fn device_entry() -> DeviceEntry {
        DeviceEntry {
            path: PathBuf::from("RPG/Foo.gba"),
            game: game(),
            save: None,
        }
    }

    fn entry(report: OperationReport) -> LogEntry {
        LogEntry {
            time: Timestamp::UNIX_EPOCH,
            front_end: FrontEnd::Cli,
            report,
        }
    }

    #[test]
    fn appends_each_event_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pocketeer.log");
        let logger = Logger::new(path.clone(), FrontEnd::Cli);
        logger.record(OperationReport::reload(&Ok(()))).1.unwrap();
        logger
            .record(OperationReport::delete_game(&device_entry(), &Ok(())))
            .1
            .unwrap();
        // Another run appends to the file.
        let next = Logger::new(path.clone(), FrontEnd::Tui);
        next.record(OperationReport::reload::<()>(&Err(BasicError::NoDevice)))
            .1
            .unwrap();
        let written = logged_lines(&path);
        assert_eq!(written.len(), 3);
        assert_eq!(written[0]["front_end"], "cli");
        assert_eq!(written[1]["game"], "gba/RPG/Foo.gba");
        assert!(written[1]["time"].is_string());
        assert_eq!(written[2]["front_end"], "tui");
        assert_eq!(written[2]["ok"], false);
    }

    /// The log names what each operation touched besides its subject, and
    /// some messages tell the user what to do.
    #[test]
    fn log_names_what_each_operation_touched() {
        let version = LibrarySave {
            name: "v1".into(),
            save: Save::from_bytes(b"", std::time::SystemTime::UNIX_EPOCH),
        };
        let row = BiosRow {
            platform: Platform::Gba,
            path: PathBuf::from("bios.bin"),
            needed_by: Vec::new(),
            size_exact: None,
            status: BiosStatus::Missing,
        };
        let (game, local) = (device_entry(), local_entry());
        let (dl_game, dl_save) = (Path::new("/dl/Foo.gba"), Path::new("/dl/Foo.sav"));
        let library = ("library", "gba/Foo.gba");
        let device = ("device", "gba/RPG/Foo.gba");
        let v1 = ("version", "v1");
        type Row<'a> = (
            OperationReport,
            &'a str,
            bool,
            Vec<(&'a str, &'a str)>,
            Option<&'a str>,
        );
        let rows: Vec<Row> = vec![
            (
                OperationReport::pull_game(&game, &Ok(AddGameOutcome::Added(local.clone()))),
                "gba/RPG/Foo.gba",
                true,
                vec![library],
                None,
            ),
            (
                OperationReport::push_game(&local, &Ok(PushGameOutcome::Pushed(game.clone()))),
                "gba/Foo.gba",
                true,
                vec![device],
                None,
            ),
            // An imported file is named by its file name, and its path is in
            // the JSON.
            (
                OperationReport::import_game(
                    dl_game,
                    &Ok(AddGameOutcome::AlreadyHad(local.clone())),
                ),
                "Foo.gba",
                true,
                vec![("from", "/dl/Foo.gba"), library],
                None,
            ),
            (
                OperationReport::import_game(dl_game, &Err(AddGameError::NameTaken)),
                "Foo.gba",
                false,
                vec![("from", "/dl/Foo.gba")],
                None,
            ),
            (
                OperationReport::import_save(
                    &local,
                    dl_save,
                    &Ok(ImportSaveOutcome::Imported(version.clone())),
                ),
                "gba/Foo.gba",
                true,
                vec![("from", "/dl/Foo.sav"), v1],
                None,
            ),
            (
                OperationReport::import_cover(&local, Path::new("/dl/Foo.png"), &Ok(())),
                "gba/Foo.gba",
                true,
                vec![("from", "/dl/Foo.png")],
                None,
            ),
            (
                OperationReport::push_save(&game, &Ok(PushSaveOutcome::Pushed(version.clone()))),
                "gba/RPG/Foo.gba",
                true,
                vec![v1],
                None,
            ),
            (
                OperationReport::trash_save_version(
                    &local,
                    "v1",
                    &Err(TrashSaveError::NoSuchVersion),
                ),
                "gba/Foo.gba",
                false,
                vec![v1],
                None,
            ),
            (
                OperationReport::install_core(
                    "a.B",
                    &Ok(InstallCoreOutcome::Installed("1.2".into())),
                ),
                "a.B",
                true,
                vec![("version", "1.2")],
                None,
            ),
            (
                OperationReport::rename_in_library(&local, &Ok(local.clone())),
                "gba/Foo.gba",
                true,
                vec![library],
                None,
            ),
            (
                OperationReport::rename_on_device(&game, &Ok(game.clone())),
                "gba/RPG/Foo.gba",
                true,
                vec![device],
                None,
            ),
            (
                OperationReport::put_bios(&row, Path::new("/dl/b.bin"), &Ok(())),
                "gba/bios.bin",
                true,
                vec![("from", "/dl/b.bin")],
                None,
            ),
            // Finding nothing to do isn't a failure.
            (
                OperationReport::pull_save(&game, &Ok(PullSaveOutcome::NoSave)),
                "gba/RPG/Foo.gba",
                true,
                vec![],
                None,
            ),
            // Refusals say what to do instead.
            (
                OperationReport::pull_save(&game, &Err(BasicError::NotInLibrary)),
                "gba/RPG/Foo.gba",
                false,
                vec![],
                Some("the library doesn't have the game; pull the game first"),
            ),
            (
                OperationReport::push_save(&game, &Err(PushSaveError::NotBackedUp)),
                "gba/RPG/Foo.gba",
                false,
                vec![],
                Some("not pushed: the device's save isn't in the library; back it up first"),
            ),
            (
                OperationReport::pull_game(&game, &Err(AddGameError::BadName)),
                "gba/RPG/Foo.gba",
                false,
                vec![],
                Some("not pulled: the library can't hold a game of that name"),
            ),
            // Errors, unlike refusals, say so.
            (
                OperationReport::delete_save(&game, &Err(BasicError::NoDevice)),
                "gba/RPG/Foo.gba",
                false,
                vec![],
                Some("error: no device is open"),
            ),
        ];
        for (report, subject, is_ok, fields, message) in rows {
            let line = report.to_line();
            let mut json = entry(report).to_json();
            let logged = json.as_object_mut().unwrap().remove("message").unwrap();
            let mut expected = json!({
                "time": "1970-01-01T00:00:00Z",
                "front_end": "cli",
                "game": subject,
                "ok": is_ok,
            });
            for (name, value) in &fields {
                expected[*name] = json!(value);
            }
            assert_eq!(json, expected, "{line}");
            assert_eq!(line, format!("{subject}: {}", logged.as_str().unwrap()));
            if let Some(message) = message {
                assert_eq!(logged, message);
            }
        }
    }
}
