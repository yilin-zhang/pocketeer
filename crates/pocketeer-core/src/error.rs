use std::path::PathBuf;

use serde::Serialize;
use serde_json::{Value, json};

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Every failure pocketeer can report.
///
/// Each variant maps to a stable, machine-readable [`Error::code`]. Agents
/// branch on the code, so codes must never be renamed once released.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("path does not exist: {}", path.display())]
    RootNotFound { path: PathBuf },

    #[error("{} does not look like a Pocket SD card (missing: {})", path.display(), missing.join(", "))]
    NotAPocket { path: PathBuf, missing: Vec<String> },

    #[error("no Analogue Pocket SD card found in {}", searched.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "))]
    SdNotFound { searched: Vec<PathBuf> },

    #[error("found {} Pocket SD cards; pick one", candidates.len())]
    MultipleSdCards { candidates: Vec<PathBuf> },

    #[error("core folder has no core.json: {}", path.display())]
    CoreJsonMissing { path: PathBuf },

    #[error("failed to read {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse {}: {source}", path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("failed to parse response from {url}: {source}")]
    ParseResponse {
        url: String,
        #[source]
        source: serde_json::Error,
    },

    #[error("request to {url} failed: {source}")]
    Network {
        url: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("{url} returned HTTP {status}")]
    Http { url: String, status: u16 },

    #[error("download from {url} was interrupted: {source}")]
    Download {
        url: String,
        #[source]
        source: std::io::Error,
    },

    #[error("{what} is not available offline and nothing is cached")]
    OfflineNoCache { what: String },

    #[error("no cache directory available; pass --cache-dir")]
    NoCacheDir,

    #[error("core {id} is not installed and not in the inventory")]
    CoreNotFound { id: String },

    #[error("core {id} is not installed")]
    CoreNotInstalled { id: String },

    #[error("{id} has no published release")]
    NoRelease { id: String },

    #[error("{id} has no release {version} (available: {})", available.join(", "))]
    VersionNotFound {
        id: String,
        version: String,
        available: Vec<String>,
    },

    #[error("zip error in {context}: {source}")]
    Zip {
        #[source]
        source: zip::result::ZipError,
        context: String,
    },

    #[error("zip contains an unsafe path: {name}")]
    UnsafeZipPath { name: String },

    #[error("{origin} does not contain a core (no Cores/<id>/core.json)")]
    NotACoreZip { origin: String },

    #[error("asked for {requested} but the zip contains {}", found.join(", "))]
    ZipCoreMismatch {
        requested: String,
        found: Vec<String>,
    },

    #[error("apply failed at step {step}{}: {source}", if *rolled_back { ", all changes rolled back" } else { ", ROLLBACK FAILED" })]
    ApplyFailed {
        step: usize,
        rolled_back: bool,
        #[source]
        source: Box<Error>,
    },

    #[error("{failed} of {total} operation(s) failed")]
    SomeFailed { failed: usize, total: usize },

    #[error("not a directory: {}", path.display())]
    NotADirectory { path: PathBuf },

    #[error("already exists: {}", path.display())]
    AlreadyExists { path: PathBuf },

    #[error("failed to parse DAT file {}: {message}", path.display())]
    DatParse { path: PathBuf, message: String },

    #[error("no library at {}", path.display())]
    LibraryNotFound { path: PathBuf },

    #[error("a library already exists at {}", path.display())]
    LibraryExists { path: PathBuf },

    #[error("library schema version {version} is newer than this pocketeer supports ({supported})")]
    LibraryTooNew { version: i64, supported: usize },

    #[error("library database error ({context}): {source}")]
    Db {
        #[source]
        source: rusqlite::Error,
        context: String,
    },

    #[error("failed to parse config {}: {message}", path.display())]
    ConfigParse { path: PathBuf, message: String },

    #[error("can't determine your home directory")]
    NoHomeDir,

    #[error("no save version with id {id}")]
    SaveVersionNotFound { id: i64 },

    #[error("no library ROM matches {spec}")]
    RomNotFound { spec: String },

    #[error("{spec} matches {} ROMs: {}", candidates.len(), candidates.join(", "))]
    RomAmbiguous {
        spec: String,
        candidates: Vec<String>,
    },

    #[error("{name} is already used on {platform}{}", if what.is_empty() { String::new() } else { format!(" ({what})") })]
    NameTaken {
        name: String,
        platform: String,
        what: String,
    },

    #[error("invalid file name {name:?}: {reason}")]
    InvalidName { name: String, reason: String },

    /// Something a plan was made from (the card, a file, a library row)
    /// changed between planning and applying.
    #[error("{what} changed since the plan was made")]
    Stale { what: String },

    /// A file changed while pocketeer was reading or copying it.
    #[error("{} changed while it was being read", path.display())]
    Changed { path: PathBuf },

    /// A file's contents are not what the records say they should be.
    #[error("{}: {what}", path.display())]
    Mismatch { path: PathBuf, what: String },

    /// A card-relative path that isn't where this kind of file lives.
    #[error("{}: {reason}", path.display())]
    InvalidPath { path: PathBuf, reason: String },

    /// Renaming this ROM would move saves that another ROM in the same
    /// folder may own (their names differ only by extension or case).
    #[error("another ROM next to {} shares its save file name; rename one of them by hand", path.display())]
    SaveOwnerAmbiguous { path: PathBuf },

    /// A cover image pocketeer won't import.
    #[error("{}: {reason}", path.display())]
    UnsupportedImage { path: PathBuf, reason: String },

    /// The command line could not be parsed (CLI-level; exit 2).
    #[error("{message}")]
    Usage {
        message: String,
        hint: Option<String>,
    },
}

/// Every error code with a one-line meaning, for `pocketeer docs`. Codes
/// are SCREAMING_SNAKE_CASE and never change once released.
pub const CODES: &[(&str, &str)] = &[
    (
        "USAGE",
        "command line could not be parsed, or no subcommand given (exit 2)",
    ),
    ("ROOT_NOT_FOUND", "the --root path does not exist"),
    (
        "NOT_A_POCKET",
        "folder lacks Analogue_Pocket.json, Assets/ or Cores/",
    ),
    (
        "SD_NOT_FOUND",
        "no Pocket SD card among the mounted volumes (details.searched)",
    ),
    ("MULTIPLE_SD_CARDS", "more than one card found; pass --root"),
    ("CORE_JSON_MISSING", "a Cores/<id> folder has no core.json"),
    ("IO_ERROR", "file system error (details.path)"),
    ("PARSE_ERROR", "a JSON file on the card is malformed"),
    ("PARSE_RESPONSE_ERROR", "a server response was malformed"),
    ("NETWORK_ERROR", "request failed (offline? DNS?)"),
    (
        "HTTP_ERROR",
        "server returned a non-success status (details.status)",
    ),
    (
        "DOWNLOAD_ERROR",
        "the connection dropped while a body was being read",
    ),
    (
        "OFFLINE_NO_CACHE",
        "--offline, and the resource was never cached",
    ),
    ("NO_CACHE_DIR", "no cache directory; pass --cache-dir"),
    (
        "CORE_NOT_FOUND",
        "id is neither installed nor in the inventory",
    ),
    ("CORE_NOT_INSTALLED", "id is not installed on the card"),
    ("NO_RELEASE", "inventory lists the core but no release"),
    (
        "VERSION_NOT_FOUND",
        "no such release (details.available lists them)",
    ),
    ("ZIP_ERROR", "a zip could not be read"),
    (
        "UNSAFE_ZIP_PATH",
        "zip entry path escapes its folder; refused",
    ),
    ("NOT_A_CORE_ZIP", "zip has no Cores/<id>/core.json"),
    (
        "ZIP_CORE_MISMATCH",
        "zip contains a different core than requested",
    ),
    (
        "APPLY_FAILED",
        "a write failed; details.rolled_back says if the card was restored",
    ),
    (
        "SOME_FAILED",
        "batch finished but some items failed (see data)",
    ),
    ("NOT_A_DIRECTORY", "a path that should be a folder isn't"),
    (
        "ALREADY_EXISTS",
        "refused to overwrite an existing file (details.path)",
    ),
    ("DAT_PARSE_ERROR", "the DAT file is not Logiqx XML"),
    (
        "LIBRARY_NOT_FOUND",
        "no library at the configured path; run library init",
    ),
    (
        "LIBRARY_EXISTS",
        "library init on a folder that already has one",
    ),
    (
        "LIBRARY_TOO_NEW",
        "the library was written by a newer pocketeer; upgrade",
    ),
    ("DB_ERROR", "library database error (details.context)"),
    ("CONFIG_PARSE_ERROR", "config.toml is malformed"),
    ("NO_HOME_DIR", "home directory unknown; pass explicit paths"),
    (
        "SAVE_VERSION_NOT_FOUND",
        "no save version with that id (see saves versions)",
    ),
    (
        "ROM_NOT_FOUND",
        "no library ROM with that id, file name or game name",
    ),
    (
        "ROM_AMBIGUOUS",
        "spec matches several ROMs (details.candidates); use the id",
    ),
    ("NAME_TAKEN", "another ROM or file already has the new name"),
    (
        "INVALID_NAME",
        "the new name can't be a file name on the card",
    ),
    (
        "STALE_PLAN",
        "the card, library or file changed between the plan and the apply; plan again",
    ),
    (
        "FILE_CHANGED",
        "a file changed while it was being read or copied; retry",
    ),
    (
        "INVALID_PATH",
        "a card-relative path is outside the folders that kind of file lives in (details.reason)",
    ),
    (
        "SAVE_OWNER_AMBIGUOUS",
        "another ROM in the folder shares the save file name; rename by hand",
    ),
    (
        "UNSUPPORTED_IMAGE",
        "a cover image isn't a PNG, JPEG or WebP within the size limits (details.reason)",
    ),
    (
        "CONTENT_MISMATCH",
        "a file's contents don't match what the library recorded (details.what)",
    ),
];

impl Error {
    pub(crate) fn db(source: rusqlite::Error, context: impl Into<String>) -> Self {
        Error::Db {
            source,
            context: context.into(),
        }
    }

    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }

    /// `context` names the archive (and entry) the failure concerns.
    pub(crate) fn zip(source: zip::result::ZipError, context: impl Into<String>) -> Self {
        Error::Zip {
            source,
            context: context.into(),
        }
    }

    /// Stable, machine-readable error code.
    pub fn code(&self) -> &'static str {
        self.meta().0
    }

    /// Structured extra data for agents (paths, candidates, ...).
    pub fn details(&self) -> Option<Value> {
        self.meta().1
    }

    /// Code and details together, so each variant is spelled out once: the
    /// code an agent branches on sits next to the fields it gets to see.
    fn meta(&self) -> (&'static str, Option<Value>) {
        match self {
            Error::RootNotFound { path } => ("ROOT_NOT_FOUND", Some(json!({ "path": path }))),
            Error::NotAPocket { path, missing } => (
                "NOT_A_POCKET",
                Some(json!({ "path": path, "missing": missing })),
            ),
            Error::SdNotFound { searched } => {
                ("SD_NOT_FOUND", Some(json!({ "searched": searched })))
            }
            Error::MultipleSdCards { candidates } => (
                "MULTIPLE_SD_CARDS",
                Some(json!({ "candidates": candidates })),
            ),
            Error::CoreJsonMissing { path } => ("CORE_JSON_MISSING", Some(json!({ "path": path }))),
            Error::Io { path, .. } => ("IO_ERROR", Some(json!({ "path": path }))),
            Error::Parse { path, .. } => ("PARSE_ERROR", Some(json!({ "path": path }))),
            Error::ParseResponse { url, .. } => {
                ("PARSE_RESPONSE_ERROR", Some(json!({ "url": url })))
            }
            Error::Network { url, .. } => ("NETWORK_ERROR", Some(json!({ "url": url }))),
            Error::Http { url, status } => {
                ("HTTP_ERROR", Some(json!({ "url": url, "status": status })))
            }
            Error::Download { url, .. } => ("DOWNLOAD_ERROR", Some(json!({ "url": url }))),
            Error::OfflineNoCache { what } => ("OFFLINE_NO_CACHE", Some(json!({ "what": what }))),
            Error::NoCacheDir => ("NO_CACHE_DIR", None),
            Error::CoreNotFound { id } => ("CORE_NOT_FOUND", Some(json!({ "id": id }))),
            Error::CoreNotInstalled { id } => ("CORE_NOT_INSTALLED", Some(json!({ "id": id }))),
            Error::NoRelease { id } => ("NO_RELEASE", Some(json!({ "id": id }))),
            Error::VersionNotFound {
                id,
                version,
                available,
            } => (
                "VERSION_NOT_FOUND",
                Some(json!({ "id": id, "version": version, "available": available })),
            ),
            Error::Zip { context, .. } => ("ZIP_ERROR", Some(json!({ "context": context }))),
            Error::UnsafeZipPath { name } => ("UNSAFE_ZIP_PATH", Some(json!({ "name": name }))),
            Error::NotACoreZip { origin } => ("NOT_A_CORE_ZIP", Some(json!({ "source": origin }))),
            Error::ZipCoreMismatch { requested, found } => (
                "ZIP_CORE_MISMATCH",
                Some(json!({ "requested": requested, "found": found })),
            ),
            Error::ApplyFailed {
                step,
                rolled_back,
                source,
            } => (
                "APPLY_FAILED",
                Some(json!({
                    "step": step,
                    "rolled_back": rolled_back,
                    "cause": source.to_info(),
                })),
            ),
            Error::SomeFailed { failed, total } => (
                "SOME_FAILED",
                Some(json!({ "failed": failed, "total": total })),
            ),
            Error::NotADirectory { path } => ("NOT_A_DIRECTORY", Some(json!({ "path": path }))),
            Error::AlreadyExists { path } => ("ALREADY_EXISTS", Some(json!({ "path": path }))),
            Error::DatParse { path, message } => (
                "DAT_PARSE_ERROR",
                Some(json!({ "path": path, "message": message })),
            ),
            Error::LibraryNotFound { path } => ("LIBRARY_NOT_FOUND", Some(json!({ "path": path }))),
            Error::LibraryExists { path } => ("LIBRARY_EXISTS", Some(json!({ "path": path }))),
            Error::LibraryTooNew { version, supported } => (
                "LIBRARY_TOO_NEW",
                Some(json!({ "version": version, "supported": supported })),
            ),
            Error::Db { context, .. } => ("DB_ERROR", Some(json!({ "context": context }))),
            Error::ConfigParse { path, message } => (
                "CONFIG_PARSE_ERROR",
                Some(json!({ "path": path, "message": message })),
            ),
            Error::NoHomeDir => ("NO_HOME_DIR", None),
            Error::SaveVersionNotFound { id } => {
                ("SAVE_VERSION_NOT_FOUND", Some(json!({ "id": id })))
            }
            Error::RomNotFound { spec } => ("ROM_NOT_FOUND", Some(json!({ "spec": spec }))),
            Error::RomAmbiguous { spec, candidates } => (
                "ROM_AMBIGUOUS",
                Some(json!({ "spec": spec, "candidates": candidates })),
            ),
            Error::NameTaken {
                name,
                platform,
                what,
            } => (
                "NAME_TAKEN",
                Some(json!({ "name": name, "platform": platform, "what": what })),
            ),
            Error::Usage { .. } => ("USAGE", None),
            Error::InvalidName { name, reason } => (
                "INVALID_NAME",
                Some(json!({ "name": name, "reason": reason })),
            ),
            Error::InvalidPath { path, reason } => (
                "INVALID_PATH",
                Some(json!({ "path": path, "reason": reason })),
            ),
            Error::SaveOwnerAmbiguous { path } => {
                ("SAVE_OWNER_AMBIGUOUS", Some(json!({ "path": path })))
            }
            Error::UnsupportedImage { path, reason } => (
                "UNSUPPORTED_IMAGE",
                Some(json!({ "path": path, "reason": reason })),
            ),
            Error::Stale { what } => ("STALE_PLAN", Some(json!({ "what": what }))),
            Error::Changed { path } => ("FILE_CHANGED", Some(json!({ "path": path }))),
            Error::Mismatch { path, what } => (
                "CONTENT_MISMATCH",
                Some(json!({ "path": path, "what": what })),
            ),
        }
    }

    /// A suggestion for what to do next, if there is an obvious one.
    pub fn hint(&self) -> Option<String> {
        match self {
            Error::Usage { hint, .. } => hint.clone(),
            Error::SdNotFound { .. } => {
                Some("Insert the SD card (or mount the Pocket), or pass --root <PATH>".into())
            }
            Error::MultipleSdCards { .. } => Some("Pass --root <PATH> to choose one".into()),
            Error::NotAPocket { .. } => Some(
                "A Pocket SD card has Analogue_Pocket.json, Assets/ and Cores/ at its root".into(),
            ),
            Error::OfflineNoCache { .. } => {
                Some("Run once without --offline to populate the cache".into())
            }
            Error::Http {
                status: 403 | 429,
                url,
            } if url.contains("github.com") => {
                Some("GitHub rate limit? Set POCKETEER_GITHUB_TOKEN or pass --github-token".into())
            }
            Error::CoreNotFound { .. } => {
                Some("Use `pocketeer cores search <query>` to find the exact id".into())
            }
            Error::CoreNotInstalled { .. } => {
                Some("Use `pocketeer cores list` to see installed cores".into())
            }
            Error::VersionNotFound { .. } => {
                Some("Pick one of the listed versions, or omit @version for the latest".into())
            }
            Error::ApplyFailed {
                rolled_back: false, ..
            } => Some("Some changes could not be rolled back; inspect the reported paths. Core install recovery files, if any, are under .pocketeer/staging".into()),
            Error::LibraryNotFound { .. } => Some(
                "Run `pocketeer library init`, or point at one with --library / `pocketeer config set library <DIR>`"
                    .into(),
            ),
            Error::LibraryTooNew { .. } => {
                Some("Upgrade pocketeer; downgrading the library is not supported".into())
            }
            Error::RomNotFound { .. } => {
                Some("Use `pocketeer library list [query]` to find the file name or id".into())
            }
            Error::RomAmbiguous { .. } => {
                Some("Pass the ROM id (from `pocketeer library list --json`) or --platform".into())
            }
            Error::Stale { .. } => Some("Run the command again to plan from the current state".into()),
            Error::Changed { .. } => Some("Retry once nothing else is writing to it".into()),
            Error::UnsupportedImage { .. } => Some(
                "Choose a PNG, JPEG or WebP image (up to 32 MiB and 8192 × 8192 pixels)".into(),
            ),
            _ => None,
        }
    }

    pub fn to_info(&self) -> ErrorInfo {
        ErrorInfo::from(self)
    }
}

/// Serializable form of an [`Error`], used in JSON output and for per-item
/// failures (e.g. one broken core in a list) that should not abort the whole
/// operation.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ErrorInfo {
    pub code: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl From<&Error> for ErrorInfo {
    fn from(err: &Error) -> Self {
        ErrorInfo {
            code: err.code(),
            message: err.to_string(),
            hint: err.hint(),
            details: err.details(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::CODES;

    #[test]
    fn codes_are_unique_and_described() {
        let mut seen = std::collections::HashSet::new();
        for (code, desc) in CODES {
            assert!(seen.insert(*code), "duplicate {code}");
            assert!(!desc.is_empty());
        }
    }
}
