//! The errors several operations can run into; what only one operation can
//! run into is beside that operation.

use std::{io, path::PathBuf, sync::Arc};

/// Clones, as what an operation returns also goes in the log; the errors from
/// the system don't, so they are kept in an `Arc`.
#[derive(Debug, Clone, thiserror::Error)]
pub enum SharedError {
    /// What went wrong and at which path, as `fs_err` writes it.
    #[error(transparent)]
    Io(Arc<io::Error>),
    /// What was done went well, but can't be noted in the log.
    #[error("couldn't write the log: {source}")]
    LogFailed { source: Arc<io::Error> },
    /// The library holds a game more than once, as files put there by hand:
    /// each of `copies` has the contents of the first file of its pair.
    #[error("the library has the same game more than once; keep one of each: {}", copies_text(.copies))]
    SameGameInLibrary { copies: Vec<(PathBuf, PathBuf)> },
    /// No device is open.
    #[error("no device is open")]
    NoDevice,
    /// The device doesn't have the game given, not any more.
    #[error("the device doesn't have the game")]
    NotOnDevice,
    /// The library doesn't have the game given, not any more.
    #[error("the library doesn't have the game")]
    NotInLibrary,
    /// `url` couldn't be reached, or answered with an error.
    #[error("{url}: {source}")]
    Http {
        url: String,
        source: Arc<ureq::Error>,
    },
    /// A source of DATs, covers or cores answered with something that isn't
    /// what it should be.
    #[error("{from}: {message}")]
    BadResponse {
        /// The URL or DAT name that was asked for.
        from: String,
        message: String,
    },
    /// The system didn't put the files in its trash.
    #[error("couldn't put it in the trash: {source}")]
    TrashFailed { source: Arc<trash::Error> },
}

fn copies_text(copies: &[(PathBuf, PathBuf)]) -> String {
    let copies: Vec<_> = copies
        .iter()
        .map(|(a, b)| format!("{} and {}", a.display(), b.display()))
        .collect();
    copies.join("; ")
}

impl From<io::Error> for SharedError {
    fn from(source: io::Error) -> Self {
        SharedError::Io(Arc::new(source))
    }
}

impl From<trash::Error> for SharedError {
    fn from(source: trash::Error) -> Self {
        SharedError::TrashFailed {
            source: Arc::new(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_operation_and_path_in_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Gone.gba");
        let error = SharedError::from(fs_err::remove_file(&path).unwrap_err());
        let text = error.to_string();
        assert!(
            text.starts_with(&format!("failed to remove file `{}`: ", path.display())),
            "{text}"
        );
    }
}
