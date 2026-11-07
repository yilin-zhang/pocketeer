//! The basic failures any operation can run into. An operation's own reasons
//! to fail live beside it, in an error type that wraps these as `Other`.

use std::{io, path::PathBuf, sync::Arc};

/// Derives `Clone` because operation results also go in the log. System errors
/// can't be cloned, so they are kept in an `Arc`.
#[derive(Debug, Clone, thiserror::Error)]
pub enum BasicError {
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
    /// The library doesn't have the given game.
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

impl From<io::Error> for BasicError {
    fn from(source: io::Error) -> Self {
        BasicError::Io(Arc::new(source))
    }
}

impl From<trash::Error> for BasicError {
    fn from(source: trash::Error) -> Self {
        BasicError::TrashFailed {
            source: Arc::new(source),
        }
    }
}

fn copies_text(copies: &[(PathBuf, PathBuf)]) -> String {
    let copies: Vec<_> = copies
        .iter()
        .map(|(a, b)| format!("{} and {}", a.display(), b.display()))
        .collect();
    copies.join("; ")
}
