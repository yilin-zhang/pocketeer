//! What was done, through which front end: a record of it, and the file it is
//! kept in.

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
    device::{DeleteSaveError, DeviceEntry, OrphanDeviceSave, RenameOnDeviceError},
    error::BaseError,
    library::{AddGameError, LocalEntry, OrphanLocalSave, RenameInLibraryError},
    pocketeer::{
        FetchCoverError, FetchCoverOutcome, ImportGameError, ImportGameOutcome, ImportSaveOutcome,
        TrashSaveError,
    },
    transfer::{
        PullGameOutcome, PullSaveError, PullSaveOutcome, PushGameError, PushGameOutcome,
        PushSaveError, PushSaveOutcome,
    },
};

/// What was done, to what, and how it went.
pub enum LogEvent {
    PullGame(DeviceEntry, Result<PullGameOutcome, AddGameError>),
    PullSave(DeviceEntry, Result<PullSaveOutcome, PullSaveError>),
    PushGame(LocalEntry, Result<PushGameOutcome, PushGameError>),
    PushSave(DeviceEntry, Result<PushSaveOutcome, PushSaveError>),
    FetchCover(LocalEntry, Result<FetchCoverOutcome, FetchCoverError>),
    /// The file it was imported from.
    ImportGame(PathBuf, Result<ImportGameOutcome, ImportGameError>),
    /// The file it was imported from.
    ImportSave(LocalEntry, PathBuf, Result<ImportSaveOutcome, BaseError>),
    TrashGame(LocalEntry, Result<(), BaseError>),
    TrashSaveAll(LocalEntry, Result<(), TrashSaveError>),
    /// The version, as [`crate::game::Save::timestamp`] writes it.
    TrashSaveVersion(LocalEntry, String, Result<(), TrashSaveError>),
    TrashOrphanSave(OrphanLocalSave, Result<(), TrashSaveError>),
    DeleteGame(DeviceEntry, Result<(), BaseError>),
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
    SyncDats(Result<SyncReport, BaseError>),
    Reload(Result<(), BaseError>),
    /// The device's root, if one was open.
    Eject(Option<PathBuf>, Result<(), BaseError>),
}

impl LogEvent {
    /// Whether it went well. Finding nothing to do is not a failure, unless
    /// something in particular was asked for.
    pub fn is_ok(&self) -> bool {
        match self {
            LogEvent::PullSave(_, Err(PullSaveError::NoSave))
            | LogEvent::FetchCover(_, Err(FetchCoverError::NoCover))
            | LogEvent::TrashSaveAll(_, Err(TrashSaveError::NoSave))
            | LogEvent::DeleteSave(_, Err(DeleteSaveError::NoSave)) => true,
            LogEvent::PullGame(_, done) => done.is_ok(),
            LogEvent::PullSave(_, done) => done.is_ok(),
            LogEvent::PushGame(_, done) => done.is_ok(),
            LogEvent::PushSave(_, done) => done.is_ok(),
            LogEvent::FetchCover(_, done) => done.is_ok(),
            LogEvent::ImportGame(_, done) => done.is_ok(),
            LogEvent::ImportSave(_, _, done) => done.is_ok(),
            LogEvent::TrashGame(_, done)
            | LogEvent::DeleteGame(_, done)
            | LogEvent::Reload(done)
            | LogEvent::Eject(_, done) => done.is_ok(),
            LogEvent::TrashSaveAll(_, done)
            | LogEvent::TrashSaveVersion(_, _, done)
            | LogEvent::TrashOrphanSave(_, done) => done.is_ok(),
            LogEvent::DeleteSave(_, done) | LogEvent::DeleteOrphanSave(_, done) => done.is_ok(),
            LogEvent::InstallCore(_, done) => done.is_ok(),
            LogEvent::RemoveCore(_, done) => done.is_ok(),
            LogEvent::PutBios(_, _, done) => done.is_ok(),
            LogEvent::RenameInLibrary(_, done) => done.is_ok(),
            LogEvent::RenameOnDevice(_, done) => done.is_ok(),
            LogEvent::SyncDats(done) => done.is_ok(),
        }
    }

    /// What it was done to, as a person would say it.
    pub fn subject(&self) -> String {
        match self {
            LogEvent::PullGame(on_device, _)
            | LogEvent::PullSave(on_device, _)
            | LogEvent::PushSave(on_device, _)
            | LogEvent::DeleteGame(on_device, _)
            | LogEvent::DeleteSave(on_device, _)
            | LogEvent::RenameOnDevice(on_device, _) => device_ref(on_device),
            LogEvent::PushGame(local, _)
            | LogEvent::FetchCover(local, _)
            | LogEvent::ImportSave(local, ..)
            | LogEvent::TrashGame(local, _)
            | LogEvent::TrashSaveAll(local, _)
            | LogEvent::TrashSaveVersion(local, ..)
            | LogEvent::RenameInLibrary(local, _) => library_ref(local),
            LogEvent::TrashOrphanSave(orphan, _) => library_orphan_ref(orphan),
            LogEvent::DeleteOrphanSave(orphan, _) => device_orphan_ref(orphan),
            // Its path is in `to_json`: it would crowd out what comes after.
            LogEvent::ImportGame(from, _) => {
                let name = from.file_name().unwrap_or(from.as_os_str());
                name.to_string_lossy().into_owned()
            }
            LogEvent::InstallCore(id, _) | LogEvent::RemoveCore(id, _) => id.clone(),
            LogEvent::PutBios(row, ..) => bios_ref(row),
            LogEvent::SyncDats(_) => "the DATs".to_string(),
            LogEvent::Reload(_) => "everything".to_string(),
            LogEvent::Eject(..) => "the device".to_string(),
        }
    }

    /// `<subject>: <message>`.
    pub fn to_line(&self) -> String {
        format!("{}: {self}", self.subject())
    }
}

/// The message.
impl fmt::Display for LogEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LogEvent::PullGame(_, Ok(PullGameOutcome::Pulled(entry))) => {
                write!(f, "pulled as {}", library_ref(entry))
            }
            LogEvent::PullGame(_, Ok(PullGameOutcome::AlreadyHad)) => {
                write!(f, "the library has it already")
            }
            LogEvent::PullGame(_, Err(e @ (AddGameError::NameTaken | AddGameError::BadName))) => {
                write!(f, "not pulled: {e}")
            }
            LogEvent::PullGame(_, Err(AddGameError::Other(e))) => write!(f, "error: {e}"),

            LogEvent::PullSave(_, Ok(PullSaveOutcome::Pulled)) => write!(f, "save kept"),
            LogEvent::PullSave(_, Ok(PullSaveOutcome::AlreadyHad)) => {
                write!(f, "the library has this save already")
            }
            LogEvent::PullSave(_, Err(PullSaveError::Other(e))) => write!(f, "error: {e}"),
            LogEvent::PullSave(_, Err(e)) => write!(f, "{e}"),

            LogEvent::PushGame(_, Ok(PushGameOutcome::Pushed(entry))) => {
                write!(f, "pushed to {}", device_ref(entry))
            }
            LogEvent::PushGame(_, Ok(PushGameOutcome::AlreadyThere(entry))) => {
                write!(f, "on the device already, at {}", device_ref(entry))
            }
            LogEvent::PushGame(_, Err(e @ PushGameError::NameTaken)) => write!(f, "{e}"),
            LogEvent::PushGame(_, Err(PushGameError::Other(e))) => write!(f, "error: {e}"),

            LogEvent::PushSave(_, Ok(PushSaveOutcome::Pushed(save))) => {
                write!(f, "save {} pushed", save.timestamp())
            }
            LogEvent::PushSave(_, Ok(PushSaveOutcome::AlreadyThere)) => {
                write!(f, "the device has this save already")
            }
            LogEvent::PushSave(_, Err(e @ PushSaveError::NotBackedUp)) => {
                write!(f, "{e}; back it up first")
            }
            LogEvent::PushSave(
                _,
                Err(e @ (PushSaveError::Other(_) | PushSaveError::AmbiguousVersion { .. })),
            ) => write!(f, "error: {e}"),
            LogEvent::PushSave(_, Err(e)) => write!(f, "{e}"),

            LogEvent::FetchCover(_, Ok(FetchCoverOutcome::Fetched)) => {
                write!(f, "cover downloaded")
            }
            LogEvent::FetchCover(_, Ok(FetchCoverOutcome::AlreadyHad)) => {
                write!(f, "it has a cover already")
            }
            LogEvent::FetchCover(_, Err(FetchCoverError::Other(e))) => write!(f, "error: {e}"),
            LogEvent::FetchCover(_, Err(e)) => write!(f, "{e}"),

            LogEvent::ImportGame(_, Ok(ImportGameOutcome::Imported(entry))) => {
                write!(f, "imported as {}", library_ref(entry))
            }
            LogEvent::ImportGame(_, Ok(ImportGameOutcome::AlreadyHad(entry))) => {
                write!(f, "the library has it already, as {}", library_ref(entry))
            }
            LogEvent::ImportGame(_, Err(e @ ImportGameError::UnknownPlatform)) => {
                write!(f, "{e}")
            }
            LogEvent::ImportGame(
                _,
                Err(ImportGameError::Add(e @ (AddGameError::NameTaken | AddGameError::BadName))),
            ) => {
                write!(f, "not imported: {e}")
            }
            LogEvent::ImportGame(_, Err(ImportGameError::Add(AddGameError::Other(e)))) => {
                write!(f, "error: {e}")
            }

            LogEvent::ImportSave(_, _, Ok(ImportSaveOutcome::Imported(save))) => {
                write!(f, "save {} imported", save.timestamp())
            }
            LogEvent::ImportSave(_, _, Ok(ImportSaveOutcome::AlreadyHad)) => {
                write!(f, "the library has this save already")
            }
            LogEvent::ImportSave(_, _, Err(e)) => write!(f, "error: {e}"),

            LogEvent::TrashGame(_, Ok(())) => write!(f, "put in the trash"),
            LogEvent::TrashSaveAll(_, Ok(()))
            | LogEvent::TrashSaveVersion(_, _, Ok(()))
            | LogEvent::TrashOrphanSave(_, Ok(())) => {
                write!(f, "save put in the trash")
            }
            LogEvent::TrashSaveAll(_, Err(e))
            | LogEvent::TrashSaveVersion(_, _, Err(e))
            | LogEvent::TrashOrphanSave(_, Err(e))
                if matches!(
                    e,
                    TrashSaveError::Other(_) | TrashSaveError::AmbiguousVersion { .. }
                ) =>
            {
                write!(f, "error: {e}")
            }
            LogEvent::TrashSaveAll(_, Err(e))
            | LogEvent::TrashSaveVersion(_, _, Err(e))
            | LogEvent::TrashOrphanSave(_, Err(e)) => write!(f, "{e}"),

            LogEvent::DeleteGame(_, Ok(())) => write!(f, "deleted from the device"),
            LogEvent::DeleteSave(_, Ok(())) | LogEvent::DeleteOrphanSave(_, Ok(())) => {
                write!(f, "save deleted from the device")
            }
            LogEvent::DeleteSave(_, Err(DeleteSaveError::Other(e)))
            | LogEvent::DeleteOrphanSave(_, Err(DeleteSaveError::Other(e))) => {
                write!(f, "error: {e}")
            }
            LogEvent::DeleteSave(_, Err(e)) | LogEvent::DeleteOrphanSave(_, Err(e)) => {
                write!(f, "{e}")
            }

            LogEvent::InstallCore(_, Ok(InstallCoreOutcome::Installed { version })) => {
                write!(f, "{version} installed")
            }
            LogEvent::InstallCore(_, Ok(InstallCoreOutcome::AlreadyCurrent)) => {
                write!(f, "the device has the newest version")
            }
            LogEvent::InstallCore(_, Err(InstallCoreError::Other(e))) => write!(f, "error: {e}"),
            LogEvent::InstallCore(_, Err(e)) => write!(f, "{e}"),
            LogEvent::RemoveCore(_, Ok(())) => write!(f, "removed from the device"),
            LogEvent::RemoveCore(_, Err(RemoveCoreError::Other(e))) => write!(f, "error: {e}"),
            LogEvent::RemoveCore(_, Err(e)) => write!(f, "{e}"),
            LogEvent::RenameInLibrary(_, Ok(entry)) => {
                write!(f, "renamed to {}", library_ref(entry))
            }
            LogEvent::RenameInLibrary(_, Err(RenameInLibraryError::Other(e))) => {
                write!(f, "error: {e}")
            }
            LogEvent::RenameInLibrary(_, Err(e)) => write!(f, "not renamed: {e}"),
            LogEvent::RenameOnDevice(_, Ok(entry)) => {
                write!(f, "renamed to {}", device_ref(entry))
            }
            LogEvent::RenameOnDevice(_, Err(RenameOnDeviceError::Other(e))) => {
                write!(f, "error: {e}")
            }
            LogEvent::RenameOnDevice(_, Err(e)) => write!(f, "not renamed: {e}"),
            LogEvent::PutBios(_, _, Ok(())) => write!(f, "put on the device"),
            LogEvent::PutBios(_, _, Err(PutBiosError::Other(e))) => write!(f, "error: {e}"),
            LogEvent::PutBios(_, _, Err(e)) => write!(f, "{e}"),

            LogEvent::SyncDats(Ok(report)) => {
                write!(f, "{} platforms updated", report.updated.len())
            }
            LogEvent::Reload(Ok(())) => write!(f, "read again"),
            LogEvent::Eject(root, Ok(())) => {
                let root = root.as_ref().map(|r| r.display().to_string());
                write!(f, "ejected {}", root.unwrap_or_default())
            }

            LogEvent::TrashGame(_, Err(e))
            | LogEvent::DeleteGame(_, Err(e))
            | LogEvent::SyncDats(Err(e))
            | LogEvent::Reload(Err(e))
            | LogEvent::Eject(_, Err(e)) => write!(f, "error: {e}"),
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
    pub event: LogEvent,
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
            "game": self.event.subject(),
            "ok": self.event.is_ok(),
            "message": self.event.to_string(),
        });
        match &self.event {
            LogEvent::PullGame(_, Ok(PullGameOutcome::Pulled(entry))) => {
                json["library"] = json!(library_ref(entry));
            }
            LogEvent::PushGame(
                _,
                Ok(PushGameOutcome::Pushed(entry) | PushGameOutcome::AlreadyThere(entry)),
            ) => {
                json["device"] = json!(device_ref(entry));
            }
            LogEvent::PushSave(_, Ok(PushSaveOutcome::Pushed(save))) => {
                json["version"] = json!(save.timestamp());
            }
            LogEvent::TrashSaveVersion(_, version, _) => {
                json["version"] = json!(version);
            }
            LogEvent::InstallCore(_, Ok(InstallCoreOutcome::Installed { version })) => {
                json["version"] = json!(version);
            }
            LogEvent::RenameInLibrary(_, Ok(entry)) => {
                json["library"] = json!(library_ref(entry));
            }
            LogEvent::RenameOnDevice(_, Ok(entry)) => {
                json["device"] = json!(device_ref(entry));
            }
            LogEvent::PutBios(_, from, _) => {
                json["from"] = json!(from.display().to_string());
            }
            LogEvent::ImportGame(from, done) => {
                json["from"] = json!(from.display().to_string());
                if let Ok(
                    ImportGameOutcome::Imported(entry) | ImportGameOutcome::AlreadyHad(entry),
                ) = done
                {
                    json["library"] = json!(library_ref(entry));
                }
            }
            LogEvent::ImportSave(_, from, done) => {
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

    /// Adds `event` to the file, with the time and this front end; the entry
    /// that makes, even if it can't be written, and whether it was.
    pub(crate) fn record(&self, event: LogEvent) -> (LogEntry, Result<(), BaseError>) {
        let entry = LogEntry {
            time: Timestamp::now(),
            front_end: self.front_end,
            event,
        };
        let written = self.write(&entry).map_err(|source| BaseError::LogFailed {
            source: Arc::new(source),
        });
        (entry, written)
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

    fn entry(event: LogEvent) -> LogEntry {
        LogEntry {
            time: Timestamp::UNIX_EPOCH,
            front_end: FrontEnd::Cli,
            event,
        }
    }

    #[test]
    fn formats_event_as_one_line() {
        let pulled = LogEvent::PullGame(on_device(), Ok(PullGameOutcome::Pulled(local())));
        assert_eq!(pulled.to_line(), "gba/RPG/Foo.gba: pulled as gba/Foo.gba");
        let synced = LogEvent::SyncDats(Ok(SyncReport::default()));
        assert_eq!(synced.to_line(), "the DATs: 0 platforms updated");
        let failed = LogEvent::DeleteGame(on_device(), Err(BaseError::NoDevice));
        assert_eq!(failed.to_string(), "error: no device is open");
    }

    #[test]
    fn treats_nothing_found_as_failure_only_when_asked_for() {
        for (event, ok) in [
            (
                LogEvent::PullSave(on_device(), Err(PullSaveError::NoSave)),
                true,
            ),
            (
                LogEvent::FetchCover(local(), Err(FetchCoverError::NoCover)),
                true,
            ),
            (
                LogEvent::TrashSaveAll(local(), Err(TrashSaveError::NoSave)),
                true,
            ),
            (
                LogEvent::DeleteSave(on_device(), Err(DeleteSaveError::NoSave)),
                true,
            ),
            (
                LogEvent::TrashSaveVersion(local(), "x".into(), Err(TrashSaveError::NoSuchVersion)),
                false,
            ),
            (
                LogEvent::PullSave(on_device(), Err(PullSaveError::NotInLibrary)),
                false,
            ),
            (
                LogEvent::PushSave(on_device(), Err(PushSaveError::NoSuchVersion)),
                false,
            ),
        ] {
            assert_eq!(event.is_ok(), ok, "{}", event.to_line());
        }
    }

    #[test]
    fn formats_event_as_json() {
        let pushed = LogEvent::PushGame(local(), Ok(PushGameOutcome::Pushed(on_device())));
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
        logger.record(LogEvent::Reload(Ok(()))).1.unwrap();
        logger
            .record(LogEvent::DeleteGame(on_device(), Ok(())))
            .1
            .unwrap();
        // Another run adds to what is there.
        let next = Logger::new(path.clone(), FrontEnd::Tui);
        next.record(LogEvent::Reload(Err(BaseError::NoDevice)))
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
