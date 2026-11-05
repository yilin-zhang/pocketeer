//! The errors any operation can run into; what only one operation can run
//! into is beside that operation.

use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use strum::IntoStaticStr;

/// Clones, as what an operation returns also goes in the log; the errors from
/// the system don't, so they are shared.
#[derive(Debug, Clone, thiserror::Error, IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum BaseError {
    #[error("{}: {source}", .path.display())]
    Io {
        path: PathBuf,
        source: Arc<io::Error>,
    },
    /// What was done went well, but can't be noted in the log.
    #[error("couldn't write the log: {source}")]
    LogFailed { source: Arc<io::Error> },
    /// There is no home folder to keep the default library in.
    #[error("no home folder; give a --library")]
    NoHome,
    /// `path` is not a folder, so not a library.
    #[error("no library at {}", .path.display())]
    NoLibrary { path: PathBuf },
    /// The library holds a game more than once, as files put there by hand:
    /// each of `copies` has the contents of the first file of its pair.
    #[error("the library has the same game more than once; keep one of each: {}", copies_text(.copies))]
    SameGameInLibrary { copies: Vec<(PathBuf, PathBuf)> },
    /// `path` lacks some of the files and folders every Pocket has.
    #[error("{} is not a Pocket: no {}", .path.display(), .missing.join(", "))]
    NotADevice {
        path: PathBuf,
        missing: Vec<&'static str>,
    },
    /// No device is open.
    #[error("no device is open")]
    NoDevice,
    /// More than one device is mounted, at `found`.
    #[error("more than one device is mounted: {}", joined(.found))]
    ManyDevices { found: Vec<PathBuf> },
    /// The device doesn't have the game given, not any more.
    #[error("the device doesn't have the game")]
    NotOnDevice,
    /// The library doesn't have the game given, not any more.
    #[error("the library doesn't have the game")]
    NotInLibrary,
    /// The system didn't eject the device; `message` is what it said.
    #[error("couldn't eject the device: {message}")]
    #[strum(serialize = "eject_failed")]
    Eject { message: String },
    #[error("{url}: {source}")]
    Http {
        url: String,
        source: Arc<ureq::Error>,
    },
    /// A source of DATs or covers answered with something that isn't what
    /// it should be.
    #[error("{from}: {message}")]
    Remote {
        /// The URL or DAT name that was asked for.
        from: String,
        message: String,
    },
    /// The system didn't put the files in its trash.
    #[error("couldn't put it in the trash: {source}")]
    #[strum(serialize = "trash_failed")]
    Trash { source: Arc<trash::Error> },
}

impl BaseError {
    /// What kind of error it is, for programs: a word that stays the same
    /// whatever the message says.
    pub fn code(&self) -> &'static str {
        self.into()
    }
}

fn joined(paths: &[PathBuf]) -> String {
    let paths: Vec<_> = paths.iter().map(|p| p.display().to_string()).collect();
    paths.join(", ")
}

fn copies_text(copies: &[(PathBuf, PathBuf)]) -> String {
    let copies: Vec<_> = copies
        .iter()
        .map(|(a, b)| format!("{} and {}", a.display(), b.display()))
        .collect();
    copies.join("; ")
}

/// `io::Error` → [`BaseError::Io`] at `path`.
pub(crate) fn io_at(path: &Path) -> impl FnOnce(io::Error) -> BaseError + '_ {
    move |source| BaseError::Io {
        path: path.to_path_buf(),
        source: Arc::new(source),
    }
}

/// `trash::Error` → [`BaseError::Trash`].
pub(crate) fn trash_failed(source: trash::Error) -> BaseError {
    BaseError::Trash {
        source: Arc::new(source),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Programs go by these, so changing a variant's name mustn't change them.
    #[test]
    fn a_code_is_the_variants_name_unless_it_says_otherwise() {
        assert_eq!(BaseError::NoHome.code(), "no_home");
        assert_eq!(
            BaseError::SameGameInLibrary { copies: Vec::new() }.code(),
            "same_game_in_library"
        );
        let eject = BaseError::Eject {
            message: String::new(),
        };
        assert_eq!(eject.code(), "eject_failed");
        let trash = trash_failed(trash::Error::Unknown {
            description: String::new(),
        });
        assert_eq!(trash.code(), "trash_failed");
    }
}
