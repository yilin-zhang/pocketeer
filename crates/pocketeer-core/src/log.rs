//! What an operation did and how it went, as the front ends show it and the log
//! file keeps it.

use std::{
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
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
        AddGameError, LocalEntry, OrphanLocalSave, RenameInLibraryError, TrashOrphanSaveError,
        TrashSaveAllOutcome, TrashSaveError,
    },
    pocketeer::{
        FetchCoverOutcome, ImportCoverError, ImportGameError, ImportGameOutcome, ImportSaveOutcome,
    },
    transfer::{
        PullGameOutcome, PullSaveOutcome, PushGameError, PushGameOutcome, PushSaveError,
        PushSaveOutcome,
    },
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
        result: &Result<PullGameOutcome, AddGameError>,
    ) -> Self {
        let mut fields = Vec::new();
        let message = match result {
            Ok(PullGameOutcome::Pulled(entry)) => {
                fields.push(("library", library_ref(entry)));
                format!("pulled as {}", library_ref(entry))
            }
            Ok(PullGameOutcome::AlreadyHad) => "the library has it already".to_string(),
            Err(AddGameError::Other(error)) => format!("error: {error}"),
            Err(error) => format!("not pulled: {error}"),
        };
        Self::new(device_ref(game), result, message, fields)
    }

    pub(crate) fn push_game(
        game: &LocalEntry,
        result: &Result<PushGameOutcome, PushGameError>,
    ) -> Self {
        let mut fields = Vec::new();
        let message = match result {
            Ok(PushGameOutcome::Pushed(entry)) => {
                fields.push(("device", device_ref(entry)));
                format!("pushed to {}", device_ref(entry))
            }
            Ok(PushGameOutcome::AlreadyThere(entry)) => {
                fields.push(("device", device_ref(entry)));
                format!("on the device already, at {}", device_ref(entry))
            }
            Err(PushGameError::Other(error)) => format!("error: {error}"),
            Err(error) => error.to_string(),
        };
        Self::new(library_ref(game), result, message, fields)
    }

    /// `from` is the file it was imported from.
    pub(crate) fn import_game(
        from: &Path,
        result: &Result<ImportGameOutcome, ImportGameError>,
    ) -> Self {
        let mut fields = vec![("from", from.display().to_string())];
        let message = match result {
            Ok(ImportGameOutcome::Imported(entry)) => {
                fields.push(("library", library_ref(entry)));
                format!("imported as {}", library_ref(entry))
            }
            Ok(ImportGameOutcome::AlreadyHad(entry)) => {
                fields.push(("library", library_ref(entry)));
                format!("the library has it already, as {}", library_ref(entry))
            }
            Err(ImportGameError::Add(AddGameError::Other(error)))
            | Err(ImportGameError::Other(error)) => format!("error: {error}"),
            Err(ImportGameError::Add(error)) => format!("not imported: {error}"),
            Err(error) => error.to_string(),
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
                fields.push(("library", library_ref(entry)));
                format!("renamed to {}", library_ref(entry))
            }
            Err(RenameInLibraryError::Other(error)) => format!("error: {error}"),
            Err(error) => format!("not renamed: {error}"),
        };
        Self::new(library_ref(game), result, message, fields)
    }

    pub(crate) fn rename_on_device(
        game: &DeviceEntry,
        result: &Result<DeviceEntry, RenameOnDeviceError>,
    ) -> Self {
        let mut fields = Vec::new();
        let message = match result {
            Ok(entry) => {
                fields.push(("device", device_ref(entry)));
                format!("renamed to {}", device_ref(entry))
            }
            Err(RenameOnDeviceError::Other(error)) => format!("error: {error}"),
            Err(error) => format!("not renamed: {error}"),
        };
        Self::new(device_ref(game), result, message, fields)
    }

    pub(crate) fn trash_game(game: &LocalEntry, result: &Result<(), BasicError>) -> Self {
        let message = match result {
            Ok(()) => "put in the trash".to_string(),
            Err(error) => format!("error: {error}"),
        };
        Self::new(library_ref(game), result, message, Vec::new())
    }

    pub(crate) fn delete_game(game: &DeviceEntry, result: &Result<(), BasicError>) -> Self {
        let message = match result {
            Ok(()) => "deleted from the device".to_string(),
            Err(error) => format!("error: {error}"),
        };
        Self::new(device_ref(game), result, message, Vec::new())
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
        Self::new(device_ref(game), result, message, Vec::new())
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
        Self::new(device_ref(game), result, message, fields)
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
        Self::new(library_ref(game), result, message, fields)
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
        Self::new(library_ref(game), result, message, Vec::new())
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
        Self::new(library_ref(game), result, message, fields)
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
        Self::new(library_orphan_ref(orphan), result, message, Vec::new())
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
        Self::new(device_ref(game), result, message, Vec::new())
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
        Self::new(device_orphan_ref(orphan), result, message, Vec::new())
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
        Self::new(library_ref(game), result, message, Vec::new())
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
        Self::new(library_ref(game), result, message, fields)
    }

    // ---- Cores and BIOS files ---------------------------------------------------

    pub(crate) fn install_core(
        id: &str,
        result: &Result<InstallCoreOutcome, InstallCoreError>,
    ) -> Self {
        let mut fields = Vec::new();
        let message = match result {
            Ok(InstallCoreOutcome::Installed { version }) => {
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
        Self::new(bios_ref(row), result, message, fields)
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

    /// `root` is the device's root, if one was open.
    pub fn eject(root: Option<&Path>, result: &Result<(), EjectDeviceError>) -> Self {
        let message = match result {
            Ok(()) => {
                let root = root.map(|root| root.display().to_string());
                format!("ejected {}", root.unwrap_or_default())
            }
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
    pub(crate) fn record(&self, report: OperationReport) -> (LogEntry, Result<(), BasicError>) {
        let entry = LogEntry {
            time: Timestamp::now(),
            front_end: self.front_end,
            report,
        };
        let log_write_result = self.write(&entry).map_err(|source| BasicError::LogFailed {
            source: Arc::new(source),
        });
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

pub fn library_ref(entry: &LocalEntry) -> String {
    format!("{}/{}", entry.game.platform.id(), entry.name)
}

pub fn device_ref(entry: &DeviceEntry) -> String {
    format!("{}/{}", entry.game.platform.id(), entry.path.display())
}

pub fn library_orphan_ref(orphan: &OrphanLocalSave) -> String {
    format!("{}/{}", orphan.platform.id(), orphan.stem)
}

pub fn device_orphan_ref(orphan: &OrphanDeviceSave) -> String {
    format!("{}/{}", orphan.platform.id(), orphan.path.display())
}

pub fn bios_ref(row: &BiosRow) -> String {
    format!("{}/{}", row.platform.id(), row.path.display())
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::*;
    use crate::{
        game::{Files, Game, Rom},
        platform::Platform,
    };

    fn game() -> Game {
        Game {
            platform: Platform::Gba,
            files: Files::Single(Rom { size: 1, crc32: 1 }),
        }
    }

    fn local() -> LocalEntry {
        LocalEntry {
            name: "Foo.gba".into(),
            game: game(),
            cover_path: None,
            save: None,
        }
    }

    fn on_device() -> DeviceEntry {
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
    fn formats_event_as_one_line() {
        let pulled =
            OperationReport::pull_game(&on_device(), &Ok(PullGameOutcome::Pulled(local())));
        assert_eq!(pulled.to_line(), "gba/RPG/Foo.gba: pulled as gba/Foo.gba");
        let synced = OperationReport::sync_dats(&Ok(SyncReport::default()));
        assert_eq!(synced.to_line(), "the DATs: 0 platforms updated");
        let failed = OperationReport::delete_game(&on_device(), &Err(BasicError::NoDevice));
        assert_eq!(failed.message, "error: no device is open");
        let refused =
            OperationReport::rename_in_library(&local(), &Err(RenameInLibraryError::NameTaken));
        assert_eq!(
            refused.message,
            "not renamed: another game in the library has that name"
        );
    }

    #[test]
    fn formats_event_as_json() {
        let pushed =
            OperationReport::push_game(&local(), &Ok(PushGameOutcome::Pushed(on_device())));
        assert_eq!(
            entry(pushed).to_json(),
            json!({
                "time": "1970-01-01T00:00:00Z",
                "front_end": "cli",
                "game": "gba/Foo.gba",
                "ok": true,
                "message": "pushed to gba/RPG/Foo.gba",
                "device": "gba/RPG/Foo.gba",
            })
        );
    }

    fn lines(path: &Path) -> Vec<Value> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn appends_each_event_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pocketeer.log");
        let logger = Logger::new(path.clone(), FrontEnd::Cli);
        assert!(!path.exists());
        logger.record(OperationReport::reload(&Ok(()))).1.unwrap();
        logger
            .record(OperationReport::delete_game(&on_device(), &Ok(())))
            .1
            .unwrap();
        // Another run appends to the file.
        let next = Logger::new(path.clone(), FrontEnd::Tui);
        next.record(OperationReport::reload::<()>(&Err(BasicError::NoDevice)))
            .1
            .unwrap();
        let written = lines(&path);
        assert_eq!(written.len(), 3);
        assert_eq!(written[0]["front_end"], "cli");
        assert_eq!(written[1]["game"], "gba/RPG/Foo.gba");
        assert!(written[1]["time"].is_string());
        assert_eq!(written[2]["front_end"], "tui");
        assert_eq!(written[2]["ok"], false);
    }

    /// Each operation with each way it can end, as the front ends and the log
    /// file see it.
    mod every_outcome {
        use std::time::SystemTime;

        use super::*;
        use crate::{cores::BiosStatus, game::Save, library::LibrarySave};

        fn no_device<T, E: From<BasicError>>() -> Result<T, E> {
            Err(BasicError::NoDevice.into())
        }

        const ERROR: &str = "error: no device is open";

        fn version() -> LibrarySave {
            LibrarySave {
                name: "v1".into(),
                save: Save::from_bytes(b"", SystemTime::UNIX_EPOCH),
            }
        }

        fn bios_row() -> BiosRow {
            BiosRow {
                platform: Platform::Gba,
                path: PathBuf::from("bios.bin"),
                needed_by: Vec::new(),
                size_exact: None,
                status: BiosStatus::Missing,
            }
        }

        fn library_orphan() -> OrphanLocalSave {
            OrphanLocalSave {
                platform: Platform::Gba,
                stem: "Gone".into(),
                save: crate::library::LibrarySaves::from_saves(vec![version()]).unwrap(),
            }
        }

        fn device_orphan() -> OrphanDeviceSave {
            OrphanDeviceSave {
                platform: Platform::Gba,
                path: PathBuf::from("RPG/Gone.sav"),
                save: version().save,
            }
        }

        /// That `report` is about `subject`, went as `is_ok` says, reads as
        /// `message`, and has `fields` besides in its JSON.
        fn check(
            subject: &str,
            report: OperationReport,
            is_ok: bool,
            message: &str,
            fields: &[(&str, &str)],
        ) {
            assert_eq!(report.to_line(), format!("{subject}: {message}"));
            let mut expected = json!({
                "time": "1970-01-01T00:00:00Z",
                "front_end": "cli",
                "game": subject,
                "ok": is_ok,
                "message": message,
            });
            for (name, value) in fields {
                expected[*name] = json!(value);
            }
            assert_eq!(entry(report).to_json(), expected);
        }

        #[test]
        fn pull_game() {
            let report = |result| OperationReport::pull_game(&on_device(), &result);
            let check = |result, is_ok, message, fields| {
                check("gba/RPG/Foo.gba", report(result), is_ok, message, fields);
            };
            check(
                Ok(PullGameOutcome::Pulled(local())),
                true,
                "pulled as gba/Foo.gba",
                &[("library", "gba/Foo.gba")],
            );
            check(
                Ok(PullGameOutcome::AlreadyHad),
                true,
                "the library has it already",
                &[],
            );
            check(
                Err(AddGameError::NameTaken),
                false,
                "not pulled: another game in the library has its name",
                &[],
            );
            check(
                Err(AddGameError::BadName),
                false,
                "not pulled: the library can't hold a game of that name",
                &[],
            );
            check(no_device(), false, ERROR, &[]);
        }

        #[test]
        fn pull_save() {
            let report = |result| OperationReport::pull_save(&on_device(), &result);
            let check = |result, is_ok, message| {
                check("gba/RPG/Foo.gba", report(result), is_ok, message, &[]);
            };
            check(Ok(PullSaveOutcome::Pulled), true, "save kept");
            check(
                Ok(PullSaveOutcome::AlreadyHad),
                true,
                "the library has this save already",
            );
            check(Ok(PullSaveOutcome::NoSave), true, "no save on the device");
            check(
                Err(BasicError::NotInLibrary),
                false,
                "the library doesn't have the game; pull the game first",
            );
            check(no_device(), false, ERROR);
        }

        #[test]
        fn push_game() {
            let report = |result| OperationReport::push_game(&local(), &result);
            let check = |result, is_ok, message, fields| {
                check("gba/Foo.gba", report(result), is_ok, message, fields);
            };
            check(
                Ok(PushGameOutcome::Pushed(on_device())),
                true,
                "pushed to gba/RPG/Foo.gba",
                &[("device", "gba/RPG/Foo.gba")],
            );
            check(
                Ok(PushGameOutcome::AlreadyThere(on_device())),
                true,
                "on the device already, at gba/RPG/Foo.gba",
                &[("device", "gba/RPG/Foo.gba")],
            );
            check(
                Err(PushGameError::NameTaken),
                false,
                "not pushed: another file is where it would go",
                &[],
            );
            check(no_device(), false, ERROR, &[]);
        }

        #[test]
        fn push_save() {
            let report = |result| OperationReport::push_save(&on_device(), &result);
            let check = |result, is_ok, message, fields| {
                check("gba/RPG/Foo.gba", report(result), is_ok, message, fields);
            };
            check(
                Ok(PushSaveOutcome::Pushed(version())),
                true,
                "save v1 pushed",
                &[("version", "v1")],
            );
            check(
                Ok(PushSaveOutcome::AlreadyThere),
                true,
                "the device has this save already",
                &[],
            );
            check(
                Err(PushSaveError::NotBackedUp),
                false,
                "not pushed: the device's save isn't in the library; back it up first",
                &[],
            );
            check(
                Err(PushSaveError::NoSuchVersion),
                false,
                "the library has no such save",
                &[],
            );
            check(no_device(), false, ERROR, &[]);
        }

        #[test]
        fn fetch_cover() {
            let report = |result| OperationReport::fetch_cover(&local(), &result);
            let check = |result, is_ok, message| {
                check("gba/Foo.gba", report(result), is_ok, message, &[]);
            };
            check(Ok(FetchCoverOutcome::Fetched), true, "cover downloaded");
            check(
                Ok(FetchCoverOutcome::AlreadyHad),
                true,
                "it has a cover already",
            );
            check(
                Ok(FetchCoverOutcome::NoCover),
                true,
                "no cover found for it",
            );
            check(no_device(), false, ERROR);
        }

        #[test]
        fn import_game() {
            let report = |result| OperationReport::import_game(Path::new("/dl/Foo.gba"), &result);
            let from = ("from", "/dl/Foo.gba");
            let library = ("library", "gba/Foo.gba");
            let check = |result, is_ok, message, fields: &[(&str, &str)]| {
                check("Foo.gba", report(result), is_ok, message, fields);
            };
            check(
                Ok(ImportGameOutcome::Imported(local())),
                true,
                "imported as gba/Foo.gba",
                &[from, library],
            );
            check(
                Ok(ImportGameOutcome::AlreadyHad(local())),
                true,
                "the library has it already, as gba/Foo.gba",
                &[from, library],
            );
            check(
                Err(ImportGameError::UnknownPlatform),
                false,
                "can't tell which platform it is for",
                &[from],
            );
            check(
                Err(ImportGameError::Add(AddGameError::NameTaken)),
                false,
                "not imported: another game in the library has its name",
                &[from],
            );
            check(
                Err(ImportGameError::Add(AddGameError::BadName)),
                false,
                "not imported: the library can't hold a game of that name",
                &[from],
            );
            check(
                Err(ImportGameError::Add(AddGameError::Other(
                    BasicError::NoDevice,
                ))),
                false,
                ERROR,
                &[from],
            );
            check(no_device(), false, ERROR, &[from]);
        }

        #[test]
        fn import_save() {
            let report =
                |result| OperationReport::import_save(&local(), Path::new("/dl/Foo.sav"), &result);
            let from = ("from", "/dl/Foo.sav");
            let check = |result, is_ok, message, fields: &[(&str, &str)]| {
                check("gba/Foo.gba", report(result), is_ok, message, fields);
            };
            check(
                Ok(ImportSaveOutcome::Imported(version())),
                true,
                "save v1 imported",
                &[from, ("version", "v1")],
            );
            check(
                Ok(ImportSaveOutcome::AlreadyHad),
                true,
                "the library has this save already",
                &[from],
            );
            check(no_device(), false, ERROR, &[from]);
        }

        #[test]
        fn import_cover() {
            let report =
                |result| OperationReport::import_cover(&local(), Path::new("/dl/Foo.png"), &result);
            let from = ("from", "/dl/Foo.png");
            let check = |result, is_ok, message| {
                check("gba/Foo.gba", report(result), is_ok, message, &[from]);
            };
            check(Ok(()), true, "cover imported");
            check(
                Err(ImportCoverError::NotAnImage),
                false,
                "not a PNG, JPEG or WebP file",
            );
            check(no_device(), false, ERROR);
        }

        #[test]
        fn trash_game() {
            let report = |result| OperationReport::trash_game(&local(), &result);
            check("gba/Foo.gba", report(Ok(())), true, "put in the trash", &[]);
            check("gba/Foo.gba", report(no_device()), false, ERROR, &[]);
        }

        #[test]
        fn trash_every_version_of_save() {
            let report = |result| OperationReport::trash_every_version(&local(), &result);
            let check = |result, is_ok, message| {
                check("gba/Foo.gba", report(result), is_ok, message, &[]);
            };
            check(
                Ok(TrashSaveAllOutcome::Trashed),
                true,
                "save put in the trash",
            );
            check(
                Ok(TrashSaveAllOutcome::NoSave),
                true,
                "no save in the library",
            );
            check(no_device(), false, ERROR);
        }

        #[test]
        fn trash_save_version() {
            let report = |result| OperationReport::trash_save_version(&local(), "v1", &result);
            let version = ("version", "v1");
            let check = |result, is_ok, message| {
                check("gba/Foo.gba", report(result), is_ok, message, &[version]);
            };
            check(Ok(()), true, "save put in the trash");
            check(
                Err(TrashSaveError::NoSuchVersion),
                false,
                "the library has no such save",
            );
            check(no_device(), false, ERROR);
        }

        #[test]
        fn trash_orphan_save() {
            let report = |result| OperationReport::trash_orphan_save(&library_orphan(), &result);
            let check = |result, is_ok, message| {
                check("gba/Gone", report(result), is_ok, message, &[]);
            };
            check(Ok(()), true, "save put in the trash");
            check(
                Err(TrashOrphanSaveError::NoOrphanSave),
                false,
                "the library has no such orphan save",
            );
            check(no_device(), false, ERROR);
        }

        #[test]
        fn delete_game() {
            let report = |result| OperationReport::delete_game(&on_device(), &result);
            let subject = "gba/RPG/Foo.gba";
            check(
                subject,
                report(Ok(())),
                true,
                "deleted from the device",
                &[],
            );
            check(subject, report(no_device()), false, ERROR, &[]);
        }

        #[test]
        fn delete_save() {
            let report = |result| OperationReport::delete_save(&on_device(), &result);
            let check = |result, is_ok, message| {
                check("gba/RPG/Foo.gba", report(result), is_ok, message, &[]);
            };
            check(
                Ok(DeleteSaveOutcome::Deleted),
                true,
                "save deleted from the device",
            );
            check(Ok(DeleteSaveOutcome::NoSave), true, "no save on the device");
            check(no_device(), false, ERROR);
        }

        #[test]
        fn delete_orphan_save() {
            let report = |result| OperationReport::delete_orphan_save(&device_orphan(), &result);
            let check = |result, is_ok, message| {
                check("gba/RPG/Gone.sav", report(result), is_ok, message, &[]);
            };
            check(Ok(()), true, "save deleted from the device");
            check(
                Err(DeleteOrphanSaveError::NoOrphanSave),
                false,
                "the device has no such orphan save",
            );
            check(no_device(), false, ERROR);
        }

        #[test]
        fn install_core() {
            let report = |result| OperationReport::install_core("a.B", &result);
            let check = |result, is_ok, message, fields| {
                check("a.B", report(result), is_ok, message, fields);
            };
            check(
                Ok(InstallCoreOutcome::Installed {
                    version: "1.2".into(),
                }),
                true,
                "1.2 installed",
                &[("version", "1.2")],
            );
            check(
                Ok(InstallCoreOutcome::AlreadyCurrent),
                true,
                "the device has the newest version",
                &[],
            );
            check(
                Err(InstallCoreError::NotSupported),
                false,
                "not a core that can be installed",
                &[],
            );
            check(
                Err(InstallCoreError::NotACoreZip("bad".into())),
                false,
                "not a zip of this core: bad",
                &[],
            );
            check(no_device(), false, ERROR, &[]);
        }

        #[test]
        fn remove_core() {
            let report = |result| OperationReport::remove_core("a.B", &result);
            check("a.B", report(Ok(())), true, "removed from the device", &[]);
            check(
                "a.B",
                report(Err(RemoveCoreError::NotInstalled)),
                false,
                "not on the device",
                &[],
            );
            check("a.B", report(no_device()), false, ERROR, &[]);
        }

        #[test]
        fn rename_in_library() {
            let report = |result| OperationReport::rename_in_library(&local(), &result);
            let check = |result, is_ok, message, fields| {
                check("gba/Foo.gba", report(result), is_ok, message, fields);
            };
            check(
                Ok(local()),
                true,
                "renamed to gba/Foo.gba",
                &[("library", "gba/Foo.gba")],
            );
            check(
                Err(RenameInLibraryError::NameTaken),
                false,
                "not renamed: another game in the library has that name",
                &[],
            );
            check(no_device(), false, ERROR, &[]);
        }

        #[test]
        fn rename_on_device() {
            let report = |result| OperationReport::rename_on_device(&on_device(), &result);
            let check = |result, is_ok, message, fields| {
                check("gba/RPG/Foo.gba", report(result), is_ok, message, fields);
            };
            check(
                Ok(on_device()),
                true,
                "renamed to gba/RPG/Foo.gba",
                &[("device", "gba/RPG/Foo.gba")],
            );
            check(
                Err(RenameOnDeviceError::BiosName),
                false,
                "not renamed: a core on the device loads a file of that name",
                &[],
            );
            check(no_device(), false, ERROR, &[]);
        }

        #[test]
        fn put_bios() {
            let report =
                |result| OperationReport::put_bios(&bios_row(), Path::new("/dl/b.bin"), &result);
            let from = ("from", "/dl/b.bin");
            let check = |result, is_ok, message| {
                check("gba/bios.bin", report(result), is_ok, message, &[from]);
            };
            check(Ok(()), true, "put on the device");
            check(
                Err(PutBiosError::AlreadyThere),
                false,
                "the device has this file already",
            );
            check(
                Err(PutBiosError::WrongSize {
                    expected: 2,
                    actual: 1,
                }),
                false,
                "the file has 1 bytes, and the core takes 2",
            );
            check(no_device(), false, ERROR);
        }

        #[test]
        fn sync_dats() {
            let report = |result| OperationReport::sync_dats(&result);
            check(
                "the DATs",
                report(Ok(SyncReport {
                    updated: vec![Platform::Gba],
                    missing: Vec::new(),
                })),
                true,
                "1 platforms updated",
                &[],
            );
            check("the DATs", report(no_device()), false, ERROR, &[]);
        }

        #[test]
        fn reload() {
            let report = |result: Result<(), BasicError>| OperationReport::reload(&result);
            check("everything", report(Ok(())), true, "read again", &[]);
            check("everything", report(no_device()), false, ERROR, &[]);
        }

        #[test]
        fn eject() {
            let report =
                |root: Option<&str>, result| OperationReport::eject(root.map(Path::new), &result);
            let subject = "the device";
            check(
                subject,
                report(Some("/Volumes/P"), Ok(())),
                true,
                "ejected /Volumes/P",
                &[],
            );
            check(subject, report(None, Ok(())), true, "ejected ", &[]);
            check(
                subject,
                report(
                    Some("/Volumes/P"),
                    Err(EjectDeviceError::EjectFailed {
                        message: "busy".into(),
                    }),
                ),
                false,
                "error: couldn't eject the device: busy",
                &[],
            );
            check(subject, report(None, no_device()), false, ERROR, &[]);
        }
    }
}
