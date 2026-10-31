//! ROM files: finding them, hashing them against DATs, renaming, syncing to the card.
//!
//! Nothing here downloads anything. It only looks at files you already have.

pub mod card;
pub mod dat;
pub mod hash;
pub mod rename;
pub mod sync;
pub mod verify;

use std::path::{Path, PathBuf};

pub use dat::Dat;
pub use hash::{HashCache, Hashes};

/// Where a ROM's bytes are: a plain file, or one entry inside a zip. Paths
/// are relative or absolute depending on who built the reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRef {
    /// The file, or the zip holding it.
    pub path: PathBuf,
    /// Entry name inside the zip, if any.
    pub inner: Option<String>,
}

impl FileRef {
    pub fn plain(path: impl Into<PathBuf>) -> Self {
        FileRef {
            path: path.into(),
            inner: None,
        }
    }

    /// The name a DAT would know this file by.
    pub fn display_name(&self) -> String {
        match &self.inner {
            Some(inner) => inner.rsplit('/').next().unwrap_or(inner).to_string(),
            None => self
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        }
    }

    /// `path` or `path::inner`, for messages and skip reasons.
    pub fn location(&self) -> String {
        match &self.inner {
            Some(inner) => format!("{}::{inner}", self.path.display()),
            None => self.path.display().to_string(),
        }
    }

    /// The same reference with `path` resolved under `dir`.
    pub fn under(&self, dir: &Path) -> FileRef {
        FileRef {
            path: dir.join(&self.path),
            inner: self.inner.clone(),
        }
    }
}
