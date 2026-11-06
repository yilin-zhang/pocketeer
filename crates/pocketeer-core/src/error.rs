//! Errors that several operations can run into. An error only one operation can
//! hit lives beside that operation.

use std::{io, path::PathBuf, sync::Arc};

/// Derives `Clone` because operation results also go in the log. System errors
/// can't be cloned, so they are kept in an `Arc`.
#[derive(Debug, Clone, thiserror::Error)]
pub enum SharedError {
    /// What went wrong and at which path, as `fs_err` writes it.
    #[error(transparent)]
    Io(Arc<io::Error>),
    /// The operation succeeded, but it couldn't be noted in the log.
    #[error("couldn't write the log: {source}")]
    LogFailed { source: Arc<io::Error> },
    /// The library holds a game more than once, because of files put there by
    /// hand. Each pair in `copies` holds the same contents.
    #[error("the library has the same game more than once; keep one of each: {}", copies_text(.copies))]
    SameGameInLibrary { copies: Vec<(PathBuf, PathBuf)> },
    #[error("no device is open")]
    NoDevice,
    /// The device no longer has the given game.
    #[error("the device doesn't have the game")]
    NotOnDevice,
    /// The library no longer has the given game.
    #[error("the library doesn't have the game")]
    NotInLibrary,
    #[error("{url}: {source}")]
    Http {
        url: String,
        source: Arc<ureq::Error>,
    },
    /// A source of DATs, covers or cores sent an unexpected response.
    #[error("{from}: {message}")]
    BadResponse {
        /// The URL or DAT name that was requested.
        from: String,
        message: String,
    },
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
