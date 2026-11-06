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
        DeleteSaveError, DeviceEntry, EjectDeviceError, OrphanDeviceSave, RenameOnDeviceError,
    },
    error::SharedError,
    library::{AddGameError, LocalEntry, OrphanLocalSave, RenameInLibraryError},
    pocketeer::{
        FetchCoverError, FetchCoverOutcome, ImportCoverError, ImportGameError, ImportGameOutcome,
        ImportSaveOutcome, TrashSaveError,
    },
    transfer::{
        PullGameOutcome, PullSaveError, PullSaveOutcome, PushGameError, PushGameOutcome,
        PushSaveError, PushSaveOutcome,
    },
};

/// What was done, to what, and how it went.
pub enum OperationReport {
    PullGame(DeviceEntry, Result<PullGameOutcome, AddGameError>),
    PullSave(DeviceEntry, Result<PullSaveOutcome, PullSaveError>),
    PushGame(LocalEntry, Result<PushGameOutcome, PushGameError>),
    PushSave(DeviceEntry, Result<PushSaveOutcome, PushSaveError>),
    FetchCover(LocalEntry, Result<FetchCoverOutcome, FetchCoverError>),
    /// The file it was imported from.
    ImportGame(PathBuf, Result<ImportGameOutcome, ImportGameError>),
    /// The file it was imported from.
    ImportSave(LocalEntry, PathBuf, Result<ImportSaveOutcome, SharedError>),
    /// The file it was imported from.
    ImportCover(LocalEntry, PathBuf, Result<(), ImportCoverError>),
    TrashGame(LocalEntry, Result<(), SharedError>),
    TrashSaveAll(LocalEntry, Result<(), TrashSaveError>),
    /// The version, as [`crate::game::Save::timestamp`] writes it.
    TrashSaveVersion(LocalEntry, String, Result<(), TrashSaveError>),
    TrashOrphanSave(OrphanLocalSave, Result<(), TrashSaveError>),
    DeleteGame(DeviceEntry, Result<(), SharedError>),
    DeleteSave(DeviceEntry, Result<(), DeleteSaveError>),
    DeleteOrphanSave(OrphanDeviceSave, Result<(), DeleteSaveError>),
    /// The core's id.
    InstallCore(String, Result<InstallCoreOutcome, InstallCoreError>),
    /// The core's id.
    RemoveCore(String, Result<(), RemoveCoreError>),
    RenameInLibrary(LocalEntry, Result<LocalEntry, RenameInLibraryError>),
    RenameOnDevice(DeviceEntry, Result<DeviceEntry, RenameOnDeviceError>),
    /// The file it was copied from.
    PutBios(BiosRow, PathBuf, Result<(), PutBiosError>),
    SyncDats(Result<SyncReport, SharedError>),
    Reload(Result<(), SharedError>),
    /// The device's root, if one was open.
    Eject(Option<PathBuf>, Result<(), EjectDeviceError>),
}

impl OperationReport {
    /// Whether it went well. Finding nothing to do is not a failure, unless
    /// something in particular was asked for.
    pub fn is_ok(&self) -> bool {
        match self {
            OperationReport::PullSave(_, Err(PullSaveError::NoSave))
            | OperationReport::FetchCover(_, Err(FetchCoverError::NoCover))
            | OperationReport::TrashSaveAll(_, Err(TrashSaveError::NoSave))
            | OperationReport::DeleteSave(_, Err(DeleteSaveError::NoSave)) => true,
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
            OperationReport::TrashSaveAll(_, done)
            | OperationReport::TrashSaveVersion(_, _, done)
            | OperationReport::TrashOrphanSave(_, done) => done.is_ok(),
            OperationReport::DeleteSave(_, done) | OperationReport::DeleteOrphanSave(_, done) => {
                done.is_ok()
            }
            OperationReport::InstallCore(_, done) => done.is_ok(),
            OperationReport::RemoveCore(_, done) => done.is_ok(),
            OperationReport::PutBios(_, _, done) => done.is_ok(),
            OperationReport::RenameInLibrary(_, done) => done.is_ok(),
            OperationReport::RenameOnDevice(_, done) => done.is_ok(),
            OperationReport::SyncDats(done) => done.is_ok(),
        }
    }

    /// What it was done to, as a person would say it.
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
            // Its path is in `to_json`: it would crowd out what comes after.
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

/// The message.
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
                Err(e @ (AddGameError::NameTaken | AddGameError::BadName)),
            ) => {
                write!(f, "not pulled: {e}")
            }
            OperationReport::PullGame(_, Err(AddGameError::Shared(e))) => write!(f, "error: {e}"),

            OperationReport::PullSave(_, Ok(PullSaveOutcome::Pulled)) => write!(f, "save kept"),
            OperationReport::PullSave(_, Ok(PullSaveOutcome::AlreadyHad)) => {
                write!(f, "the library has this save already")
            }
            OperationReport::PullSave(_, Err(PullSaveError::Shared(e))) => write!(f, "error: {e}"),
            OperationReport::PullSave(_, Err(e)) => write!(f, "{e}"),

            OperationReport::PushGame(_, Ok(PushGameOutcome::Pushed(entry))) => {
                write!(f, "pushed to {}", device_ref(entry))
            }
            OperationReport::PushGame(_, Ok(PushGameOutcome::AlreadyThere(entry))) => {
                write!(f, "on the device already, at {}", device_ref(entry))
            }
            OperationReport::PushGame(_, Err(e @ PushGameError::NameTaken)) => write!(f, "{e}"),
            OperationReport::PushGame(_, Err(PushGameError::Shared(e))) => write!(f, "error: {e}"),

            OperationReport::PushSave(_, Ok(PushSaveOutcome::Pushed(save))) => {
                write!(f, "save {} pushed", save.timestamp())
            }
            OperationReport::PushSave(_, Ok(PushSaveOutcome::AlreadyThere)) => {
                write!(f, "the device has this save already")
            }
            OperationReport::PushSave(_, Err(e @ PushSaveError::NotBackedUp)) => {
                write!(f, "{e}; back it up first")
            }
            OperationReport::PushSave(
                _,
                Err(e @ (PushSaveError::Shared(_) | PushSaveError::AmbiguousVersion { .. })),
            ) => write!(f, "error: {e}"),
            OperationReport::PushSave(_, Err(e)) => write!(f, "{e}"),

            OperationReport::FetchCover(_, Ok(FetchCoverOutcome::Fetched)) => {
                write!(f, "cover downloaded")
            }
            OperationReport::FetchCover(_, Ok(FetchCoverOutcome::AlreadyHad)) => {
                write!(f, "it has a cover already")
            }
            OperationReport::FetchCover(_, Err(FetchCoverError::Shared(e))) => {
                write!(f, "error: {e}")
            }
            OperationReport::FetchCover(_, Err(e)) => write!(f, "{e}"),

            OperationReport::ImportGame(_, Ok(ImportGameOutcome::Imported(entry))) => {
                write!(f, "imported as {}", library_ref(entry))
            }
            OperationReport::ImportGame(_, Ok(ImportGameOutcome::AlreadyHad(entry))) => {
                write!(f, "the library has it already, as {}", library_ref(entry))
            }
            OperationReport::ImportGame(_, Err(e @ ImportGameError::UnknownPlatform)) => {
                write!(f, "{e}")
            }
            OperationReport::ImportGame(
                _,
                Err(ImportGameError::Add(e @ (AddGameError::NameTaken | AddGameError::BadName))),
            ) => {
                write!(f, "not imported: {e}")
            }
            OperationReport::ImportGame(_, Err(ImportGameError::Add(AddGameError::Shared(e))))
            | OperationReport::ImportGame(_, Err(ImportGameError::Shared(e))) => {
                write!(f, "error: {e}")
            }

            OperationReport::ImportSave(_, _, Ok(ImportSaveOutcome::Imported(save))) => {
                write!(f, "save {} imported", save.timestamp())
            }
            OperationReport::ImportSave(_, _, Ok(ImportSaveOutcome::AlreadyHad)) => {
                write!(f, "the library has this save already")
            }
            OperationReport::ImportSave(_, _, Err(e)) => write!(f, "error: {e}"),

            OperationReport::ImportCover(_, _, Ok(())) => write!(f, "cover imported"),
            OperationReport::ImportCover(_, _, Err(ImportCoverError::Shared(e))) => {
                write!(f, "error: {e}")
            }
            OperationReport::ImportCover(_, _, Err(e)) => write!(f, "{e}"),

            OperationReport::TrashGame(_, Ok(())) => write!(f, "put in the trash"),
            OperationReport::TrashSaveAll(_, Ok(()))
            | OperationReport::TrashSaveVersion(_, _, Ok(()))
            | OperationReport::TrashOrphanSave(_, Ok(())) => {
                write!(f, "save put in the trash")
            }
            OperationReport::TrashSaveAll(_, Err(e))
            | OperationReport::TrashSaveVersion(_, _, Err(e))
            | OperationReport::TrashOrphanSave(_, Err(e))
                if matches!(
                    e,
                    TrashSaveError::Shared(_) | TrashSaveError::AmbiguousVersion { .. }
                ) =>
            {
                write!(f, "error: {e}")
            }
            OperationReport::TrashSaveAll(_, Err(e))
            | OperationReport::TrashSaveVersion(_, _, Err(e))
            | OperationReport::TrashOrphanSave(_, Err(e)) => write!(f, "{e}"),

            OperationReport::DeleteGame(_, Ok(())) => write!(f, "deleted from the device"),
            OperationReport::DeleteSave(_, Ok(()))
            | OperationReport::DeleteOrphanSave(_, Ok(())) => {
                write!(f, "save deleted from the device")
            }
            OperationReport::DeleteSave(_, Err(DeleteSaveError::Shared(e)))
            | OperationReport::DeleteOrphanSave(_, Err(DeleteSaveError::Shared(e))) => {
                write!(f, "error: {e}")
            }
            OperationReport::DeleteSave(_, Err(e))
            | OperationReport::DeleteOrphanSave(_, Err(e)) => {
                write!(f, "{e}")
            }

            OperationReport::InstallCore(_, Ok(InstallCoreOutcome::Installed { version })) => {
                write!(f, "{version} installed")
            }
            OperationReport::InstallCore(_, Ok(InstallCoreOutcome::AlreadyCurrent)) => {
                write!(f, "the device has the newest version")
            }
            OperationReport::InstallCore(_, Err(InstallCoreError::Shared(e))) => {
                write!(f, "error: {e}")
            }
            OperationReport::InstallCore(_, Err(e)) => write!(f, "{e}"),
            OperationReport::RemoveCore(_, Ok(())) => write!(f, "removed from the device"),
            OperationReport::RemoveCore(_, Err(RemoveCoreError::Shared(e))) => {
                write!(f, "error: {e}")
            }
            OperationReport::RemoveCore(_, Err(e)) => write!(f, "{e}"),
            OperationReport::RenameInLibrary(_, Ok(entry)) => {
                write!(f, "renamed to {}", library_ref(entry))
            }
            OperationReport::RenameInLibrary(_, Err(RenameInLibraryError::Shared(e))) => {
                write!(f, "error: {e}")
            }
            OperationReport::RenameInLibrary(_, Err(e)) => write!(f, "not renamed: {e}"),
            OperationReport::RenameOnDevice(_, Ok(entry)) => {
                write!(f, "renamed to {}", device_ref(entry))
            }
            OperationReport::RenameOnDevice(_, Err(RenameOnDeviceError::Shared(e))) => {
                write!(f, "error: {e}")
            }
            OperationReport::RenameOnDevice(_, Err(e)) => write!(f, "not renamed: {e}"),
            OperationReport::PutBios(_, _, Ok(())) => write!(f, "put on the device"),
            OperationReport::PutBios(_, _, Err(PutBiosError::Shared(e))) => write!(f, "error: {e}"),
            OperationReport::PutBios(_, _, Err(e)) => write!(f, "{e}"),

            OperationReport::SyncDats(Ok(report)) => {
                write!(f, "{} platforms updated", report.updated.len())
            }
            OperationReport::Reload(Ok(())) => write!(f, "read again"),
            OperationReport::Eject(root, Ok(())) => {
                let root = root.as_ref().map(|r| r.display().to_string());
                write!(f, "ejected {}", root.unwrap_or_default())
            }

            OperationReport::TrashGame(_, Err(e))
            | OperationReport::DeleteGame(_, Err(e))
            | OperationReport::SyncDats(Err(e))
            | OperationReport::Reload(Err(e)) => write!(f, "error: {e}"),
            OperationReport::Eject(_, Err(e)) => write!(f, "error: {e}"),
        }
    }
}

/// Which front end did it.
#[derive(Debug, Clone, Copy)]
pub enum FrontEnd {
    Cli,
    Tui,
}

/// A whole record: what happened, when, and through what.
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
                json["version"] = json!(save.timestamp());
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
                    json["version"] = json!(save.timestamp());
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

    /// Adds `report` to the file, with the time and this front end. Returns the
    /// entry that makes along with whether it was written, not one or the other:
    /// the operation is done either way, so the front end still has to be told.
    pub(crate) fn record(&self, report: OperationReport) -> (LogEntry, Result<(), SharedError>) {
        let entry = LogEntry {
            time: Timestamp::now(),
            front_end: self.front_end,
            report,
        };
        let log_write_result = self.write(&entry).map_err(|source| SharedError::LogFailed {
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
        let failed = OperationReport::DeleteGame(on_device(), Err(SharedError::NoDevice));
        assert_eq!(failed.to_string(), "error: no device is open");
    }

    #[test]
    fn treats_nothing_found_as_failure_only_when_asked_for() {
        for (report, ok) in [
            (
                OperationReport::PullSave(on_device(), Err(PullSaveError::NoSave)),
                true,
            ),
            (
                OperationReport::FetchCover(local(), Err(FetchCoverError::NoCover)),
                true,
            ),
            (
                OperationReport::TrashSaveAll(local(), Err(TrashSaveError::NoSave)),
                true,
            ),
            (
                OperationReport::DeleteSave(on_device(), Err(DeleteSaveError::NoSave)),
                true,
            ),
            (
                OperationReport::TrashSaveVersion(
                    local(),
                    "x".into(),
                    Err(TrashSaveError::NoSuchVersion),
                ),
                false,
            ),
            (
                OperationReport::PullSave(on_device(), Err(PullSaveError::NotInLibrary)),
                false,
            ),
            (
                OperationReport::PushSave(on_device(), Err(PushSaveError::NoSuchVersion)),
                false,
            ),
        ] {
            assert_eq!(report.is_ok(), ok, "{}", report.to_line());
        }
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
            .map(|l| serde_json::from_str(l).unwrap())
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
        // Another run adds to what is there.
        let next = Logger::new(path.clone(), FrontEnd::Tui);
        next.record(OperationReport::Reload(Err(SharedError::NoDevice)))
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
