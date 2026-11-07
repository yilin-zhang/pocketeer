//! What an operation did and how it went, as the front ends show it and the log
//! file keeps it.

use std::{
    fmt,
    io::{self, Write},
    path::PathBuf,
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
pub enum OperationReport {
    PullGame(DeviceEntry, Result<PullGameOutcome, AddGameError>),
    PullSave(DeviceEntry, Result<PullSaveOutcome, BasicError>),
    PushGame(LocalEntry, Result<PushGameOutcome, PushGameError>),
    PushSave(DeviceEntry, Result<PushSaveOutcome, PushSaveError>),
    FetchCover(LocalEntry, Result<FetchCoverOutcome, BasicError>),
    /// The file it was imported from.
    ImportGame(PathBuf, Result<ImportGameOutcome, ImportGameError>),
    /// The file it was imported from.
    ImportSave(LocalEntry, PathBuf, Result<ImportSaveOutcome, BasicError>),
    /// The file it was imported from.
    ImportCover(LocalEntry, PathBuf, Result<(), ImportCoverError>),
    TrashGame(LocalEntry, Result<(), BasicError>),
    TrashSaveAll(LocalEntry, Result<TrashSaveAllOutcome, BasicError>),
    /// The version, as [`crate::game::Save::timestamp`] writes it.
    TrashSaveVersion(LocalEntry, String, Result<(), TrashSaveError>),
    TrashOrphanSave(OrphanLocalSave, Result<(), TrashOrphanSaveError>),
    DeleteGame(DeviceEntry, Result<(), BasicError>),
    DeleteSave(DeviceEntry, Result<DeleteSaveOutcome, BasicError>),
    DeleteOrphanSave(OrphanDeviceSave, Result<(), DeleteOrphanSaveError>),
    /// The core's id.
    InstallCore(String, Result<InstallCoreOutcome, InstallCoreError>),
    /// The core's id.
    RemoveCore(String, Result<(), RemoveCoreError>),
    RenameInLibrary(LocalEntry, Result<LocalEntry, RenameInLibraryError>),
    RenameOnDevice(DeviceEntry, Result<DeviceEntry, RenameOnDeviceError>),
    /// The file it was copied from.
    PutBios(BiosRow, PathBuf, Result<(), PutBiosError>),
    SyncDats(Result<SyncReport, BasicError>),
    Reload(Result<(), BasicError>),
    /// The device's root, if one was open.
    Eject(Option<PathBuf>, Result<(), EjectDeviceError>),
}

impl OperationReport {
    /// Whether it went well. Each operation decides that by what it returns,
    /// and finding nothing to do is an outcome, not an error.
    pub fn is_ok(&self) -> bool {
        match self {
            OperationReport::PullGame(_, done) => done.is_ok(),
            OperationReport::PullSave(_, done) => done.is_ok(),
            OperationReport::PushGame(_, done) => done.is_ok(),
            OperationReport::PushSave(_, done) => done.is_ok(),
            OperationReport::FetchCover(_, done) => done.is_ok(),
            OperationReport::ImportGame(_, done) => done.is_ok(),
            OperationReport::ImportSave(_, _, done) => done.is_ok(),
            OperationReport::ImportCover(_, _, done) => done.is_ok(),
            OperationReport::TrashGame(_, done)
            | OperationReport::DeleteGame(_, done)
            | OperationReport::Reload(done) => done.is_ok(),
            OperationReport::Eject(_, done) => done.is_ok(),
            OperationReport::TrashSaveAll(_, done) => done.is_ok(),
            OperationReport::TrashSaveVersion(_, _, done) => done.is_ok(),
            OperationReport::TrashOrphanSave(_, done) => done.is_ok(),
            OperationReport::DeleteSave(_, done) => done.is_ok(),
            OperationReport::DeleteOrphanSave(_, done) => done.is_ok(),
            OperationReport::InstallCore(_, done) => done.is_ok(),
            OperationReport::RemoveCore(_, done) => done.is_ok(),
            OperationReport::PutBios(_, _, done) => done.is_ok(),
            OperationReport::RenameInLibrary(_, done) => done.is_ok(),
            OperationReport::RenameOnDevice(_, done) => done.is_ok(),
            OperationReport::SyncDats(done) => done.is_ok(),
        }
    }

    /// What it was done to, in words for a person.
    pub fn subject(&self) -> String {
        match self {
            OperationReport::PullGame(on_device, _)
            | OperationReport::PullSave(on_device, _)
            | OperationReport::PushSave(on_device, _)
            | OperationReport::DeleteGame(on_device, _)
            | OperationReport::DeleteSave(on_device, _)
            | OperationReport::RenameOnDevice(on_device, _) => device_ref(on_device),
            OperationReport::PushGame(local, _)
            | OperationReport::FetchCover(local, _)
            | OperationReport::ImportSave(local, ..)
            | OperationReport::ImportCover(local, ..)
            | OperationReport::TrashGame(local, _)
            | OperationReport::TrashSaveAll(local, _)
            | OperationReport::TrashSaveVersion(local, ..)
            | OperationReport::RenameInLibrary(local, _) => library_ref(local),
            OperationReport::TrashOrphanSave(orphan, _) => library_orphan_ref(orphan),
            OperationReport::DeleteOrphanSave(orphan, _) => device_orphan_ref(orphan),
            // Its path is in `to_json`, since here it would crowd out what
            // comes after.
            OperationReport::ImportGame(from, _) => {
                let name = from.file_name().unwrap_or(from.as_os_str());
                name.to_string_lossy().into_owned()
            }
            OperationReport::InstallCore(id, _) | OperationReport::RemoveCore(id, _) => id.clone(),
            OperationReport::PutBios(row, ..) => bios_ref(row),
            OperationReport::SyncDats(_) => "the DATs".to_string(),
            OperationReport::Reload(_) => "everything".to_string(),
            OperationReport::Eject(..) => "the device".to_string(),
        }
    }

    /// `<subject>: <message>`.
    pub fn to_line(&self) -> String {
        format!("{}: {self}", self.subject())
    }
}

impl fmt::Display for OperationReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OperationReport::PullGame(_, Ok(PullGameOutcome::Pulled(entry))) => {
                write!(f, "pulled as {}", library_ref(entry))
            }
            OperationReport::PullGame(_, Ok(PullGameOutcome::AlreadyHad)) => {
                write!(f, "the library has it already")
            }
            OperationReport::PullGame(
                _,
                Err(error @ (AddGameError::NameTaken | AddGameError::BadName)),
            ) => {
                write!(f, "not pulled: {error}")
            }
            OperationReport::PullGame(_, Err(AddGameError::Other(error))) => {
                write!(f, "error: {error}")
            }

            OperationReport::PullSave(_, Ok(PullSaveOutcome::Pulled)) => write!(f, "save kept"),
            OperationReport::PullSave(_, Ok(PullSaveOutcome::AlreadyHad)) => {
                write!(f, "the library has this save already")
            }
            OperationReport::PullSave(_, Ok(PullSaveOutcome::NoSave)) => {
                write!(f, "no save on the device")
            }
            OperationReport::PullSave(_, Err(error @ BasicError::NotInLibrary)) => {
                write!(f, "{error}; pull the game first")
            }
            OperationReport::PullSave(_, Err(error)) => write!(f, "error: {error}"),

            OperationReport::PushGame(_, Ok(PushGameOutcome::Pushed(entry))) => {
                write!(f, "pushed to {}", device_ref(entry))
            }
            OperationReport::PushGame(_, Ok(PushGameOutcome::AlreadyThere(entry))) => {
                write!(f, "on the device already, at {}", device_ref(entry))
            }
            OperationReport::PushGame(_, Err(error @ PushGameError::NameTaken)) => {
                write!(f, "{error}")
            }
            OperationReport::PushGame(_, Err(PushGameError::Other(error))) => {
                write!(f, "error: {error}")
            }

            OperationReport::PushSave(_, Ok(PushSaveOutcome::Pushed(save))) => {
                write!(f, "save {} pushed", save.name)
            }
            OperationReport::PushSave(_, Ok(PushSaveOutcome::AlreadyThere)) => {
                write!(f, "the device has this save already")
            }
            OperationReport::PushSave(_, Err(error @ PushSaveError::NotBackedUp)) => {
                write!(f, "{error}; back it up first")
            }
            OperationReport::PushSave(_, Err(PushSaveError::Other(error))) => {
                write!(f, "error: {error}")
            }
            OperationReport::PushSave(_, Err(error)) => write!(f, "{error}"),

            OperationReport::FetchCover(_, Ok(FetchCoverOutcome::Fetched)) => {
                write!(f, "cover downloaded")
            }
            OperationReport::FetchCover(_, Ok(FetchCoverOutcome::AlreadyHad)) => {
                write!(f, "it has a cover already")
            }
            OperationReport::FetchCover(_, Ok(FetchCoverOutcome::NoCover)) => {
                write!(f, "no cover found for it")
            }

            OperationReport::ImportGame(_, Ok(ImportGameOutcome::Imported(entry))) => {
                write!(f, "imported as {}", library_ref(entry))
            }
            OperationReport::ImportGame(_, Ok(ImportGameOutcome::AlreadyHad(entry))) => {
                write!(f, "the library has it already, as {}", library_ref(entry))
            }
            OperationReport::ImportGame(_, Err(error @ ImportGameError::UnknownPlatform)) => {
                write!(f, "{error}")
            }
            OperationReport::ImportGame(
                _,
                Err(ImportGameError::Add(
                    error @ (AddGameError::NameTaken | AddGameError::BadName),
                )),
            ) => {
                write!(f, "not imported: {error}")
            }
            OperationReport::ImportGame(
                _,
                Err(ImportGameError::Add(AddGameError::Other(error))),
            )
            | OperationReport::ImportGame(_, Err(ImportGameError::Other(error))) => {
                write!(f, "error: {error}")
            }

            OperationReport::ImportSave(_, _, Ok(ImportSaveOutcome::Imported(save))) => {
                write!(f, "save {} imported", save.name)
            }
            OperationReport::ImportSave(_, _, Ok(ImportSaveOutcome::AlreadyHad)) => {
                write!(f, "the library has this save already")
            }
            OperationReport::ImportSave(_, _, Err(error)) => write!(f, "error: {error}"),

            OperationReport::ImportCover(_, _, Ok(())) => write!(f, "cover imported"),
            OperationReport::ImportCover(_, _, Err(ImportCoverError::Other(error))) => {
                write!(f, "error: {error}")
            }
            OperationReport::ImportCover(_, _, Err(error)) => write!(f, "{error}"),

            OperationReport::TrashGame(_, Ok(())) => write!(f, "put in the trash"),
            OperationReport::TrashSaveAll(_, Ok(TrashSaveAllOutcome::Trashed))
            | OperationReport::TrashSaveVersion(_, _, Ok(()))
            | OperationReport::TrashOrphanSave(_, Ok(())) => {
                write!(f, "save put in the trash")
            }
            OperationReport::TrashSaveAll(_, Ok(TrashSaveAllOutcome::NoSave)) => {
                write!(f, "no save in the library")
            }
            OperationReport::TrashSaveVersion(_, _, Err(TrashSaveError::Other(error))) => {
                write!(f, "error: {error}")
            }
            OperationReport::TrashSaveVersion(_, _, Err(error)) => write!(f, "{error}"),
            OperationReport::TrashOrphanSave(_, Err(TrashOrphanSaveError::Other(error))) => {
                write!(f, "error: {error}")
            }
            OperationReport::TrashOrphanSave(_, Err(error)) => write!(f, "{error}"),

            OperationReport::DeleteGame(_, Ok(())) => write!(f, "deleted from the device"),
            OperationReport::DeleteSave(_, Ok(DeleteSaveOutcome::Deleted))
            | OperationReport::DeleteOrphanSave(_, Ok(())) => {
                write!(f, "save deleted from the device")
            }
            OperationReport::DeleteSave(_, Ok(DeleteSaveOutcome::NoSave)) => {
                write!(f, "no save on the device")
            }
            OperationReport::DeleteOrphanSave(_, Err(DeleteOrphanSaveError::Other(error))) => {
                write!(f, "error: {error}")
            }
            OperationReport::DeleteOrphanSave(_, Err(error)) => write!(f, "{error}"),

            OperationReport::InstallCore(_, Ok(InstallCoreOutcome::Installed { version })) => {
                write!(f, "{version} installed")
            }
            OperationReport::InstallCore(_, Ok(InstallCoreOutcome::AlreadyCurrent)) => {
                write!(f, "the device has the newest version")
            }
            OperationReport::InstallCore(_, Err(InstallCoreError::Other(error))) => {
                write!(f, "error: {error}")
            }
            OperationReport::InstallCore(_, Err(error)) => write!(f, "{error}"),
            OperationReport::RemoveCore(_, Ok(())) => write!(f, "removed from the device"),
            OperationReport::RemoveCore(_, Err(RemoveCoreError::Other(error))) => {
                write!(f, "error: {error}")
            }
            OperationReport::RemoveCore(_, Err(error)) => write!(f, "{error}"),
            OperationReport::RenameInLibrary(_, Ok(entry)) => {
                write!(f, "renamed to {}", library_ref(entry))
            }
            OperationReport::RenameInLibrary(_, Err(RenameInLibraryError::Other(error))) => {
                write!(f, "error: {error}")
            }
            OperationReport::RenameInLibrary(_, Err(error)) => write!(f, "not renamed: {error}"),
            OperationReport::RenameOnDevice(_, Ok(entry)) => {
                write!(f, "renamed to {}", device_ref(entry))
            }
            OperationReport::RenameOnDevice(_, Err(RenameOnDeviceError::Other(error))) => {
                write!(f, "error: {error}")
            }
            OperationReport::RenameOnDevice(_, Err(error)) => write!(f, "not renamed: {error}"),
            OperationReport::PutBios(_, _, Ok(())) => write!(f, "put on the device"),
            OperationReport::PutBios(_, _, Err(PutBiosError::Other(error))) => {
                write!(f, "error: {error}")
            }
            OperationReport::PutBios(_, _, Err(error)) => write!(f, "{error}"),

            OperationReport::SyncDats(Ok(report)) => {
                write!(f, "{} platforms updated", report.updated.len())
            }
            OperationReport::Reload(Ok(())) => write!(f, "read again"),
            OperationReport::Eject(root, Ok(())) => {
                let root = root.as_ref().map(|root| root.display().to_string());
                write!(f, "ejected {}", root.unwrap_or_default())
            }

            OperationReport::FetchCover(_, Err(error))
            | OperationReport::TrashGame(_, Err(error))
            | OperationReport::TrashSaveAll(_, Err(error))
            | OperationReport::DeleteGame(_, Err(error))
            | OperationReport::DeleteSave(_, Err(error))
            | OperationReport::SyncDats(Err(error))
            | OperationReport::Reload(Err(error)) => write!(f, "error: {error}"),
            OperationReport::Eject(_, Err(error)) => write!(f, "error: {error}"),
        }
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
            "game": self.report.subject(),
            "ok": self.report.is_ok(),
            "message": self.report.to_string(),
        });
        match &self.report {
            OperationReport::PullGame(_, Ok(PullGameOutcome::Pulled(entry))) => {
                json["library"] = json!(library_ref(entry));
            }
            OperationReport::PushGame(
                _,
                Ok(PushGameOutcome::Pushed(entry) | PushGameOutcome::AlreadyThere(entry)),
            ) => {
                json["device"] = json!(device_ref(entry));
            }
            OperationReport::PushSave(_, Ok(PushSaveOutcome::Pushed(save))) => {
                json["version"] = json!(save.name);
            }
            OperationReport::TrashSaveVersion(_, version, _) => {
                json["version"] = json!(version);
            }
            OperationReport::InstallCore(_, Ok(InstallCoreOutcome::Installed { version })) => {
                json["version"] = json!(version);
            }
            OperationReport::RenameInLibrary(_, Ok(entry)) => {
                json["library"] = json!(library_ref(entry));
            }
            OperationReport::RenameOnDevice(_, Ok(entry)) => {
                json["device"] = json!(device_ref(entry));
            }
            OperationReport::PutBios(_, from, _) => {
                json["from"] = json!(from.display().to_string());
            }
            OperationReport::ImportGame(from, done) => {
                json["from"] = json!(from.display().to_string());
                if let Ok(
                    ImportGameOutcome::Imported(entry) | ImportGameOutcome::AlreadyHad(entry),
                ) = done
                {
                    json["library"] = json!(library_ref(entry));
                }
            }
            OperationReport::ImportCover(_, from, _) => {
                json["from"] = json!(from.display().to_string());
            }
            OperationReport::ImportSave(_, from, done) => {
                json["from"] = json!(from.display().to_string());
                if let Ok(ImportSaveOutcome::Imported(save)) = done {
                    json["version"] = json!(save.name);
                }
            }
            _ => {}
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
        let pulled = OperationReport::PullGame(on_device(), Ok(PullGameOutcome::Pulled(local())));
        assert_eq!(pulled.to_line(), "gba/RPG/Foo.gba: pulled as gba/Foo.gba");
        let synced = OperationReport::SyncDats(Ok(SyncReport::default()));
        assert_eq!(synced.to_line(), "the DATs: 0 platforms updated");
        let failed = OperationReport::DeleteGame(on_device(), Err(BasicError::NoDevice));
        assert_eq!(failed.to_string(), "error: no device is open");
        let refused =
            OperationReport::RenameInLibrary(local(), Err(RenameInLibraryError::NameTaken));
        assert_eq!(
            refused.to_string(),
            "not renamed: another game in the library has that name"
        );
    }

    #[test]
    fn formats_event_as_json() {
        let pushed = OperationReport::PushGame(local(), Ok(PushGameOutcome::Pushed(on_device())));
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
        logger.record(OperationReport::Reload(Ok(()))).1.unwrap();
        logger
            .record(OperationReport::DeleteGame(on_device(), Ok(())))
            .1
            .unwrap();
        // Another run appends to the file.
        let next = Logger::new(path.clone(), FrontEnd::Tui);
        next.record(OperationReport::Reload(Err(BasicError::NoDevice)))
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
}
