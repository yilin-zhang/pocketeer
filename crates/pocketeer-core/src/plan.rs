//! Shared progress, target identity checks and failure reports.
//! Each business plan exposes its own apply method with explicit resources.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{
    Error, ErrorInfo, Pocket, Result,
    fsutil::FileId,
    library::{DB_FILE, Library},
};

/// Progress events for long operations.
///
/// Core functions take a `Progress` callback and never print themselves.
/// The CLI turns these into stderr lines; the TUI into gauges.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A download is in flight.
    Download {
        url: String,
        received: u64,
        total: Option<u64>,
    },
    /// A short status line ("Extracting", "Applying 3 changes").
    Status(String),
    /// Discrete progress within the current status.
    Step { done: usize, total: usize },
}

pub type Progress<'a> = &'a mut dyn FnMut(Event);

/// Which card and library a plan was made against: their folders and what
/// they were on disk then. A card swapped at the same mount point, or a
/// library replaced at the same path, reads differently.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Pinned {
    card: Option<Pin>,
    library: Option<Pin>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Pin {
    root: PathBuf,
    id: FileId,
}

impl Pin {
    /// The card's root folder, still a Pocket card.
    fn card(pocket: &Pocket) -> Result<Self> {
        let root = Pocket::open(pocket.root())?.root().to_path_buf();
        Ok(Pin {
            id: FileId::of(&root)?,
            root,
        })
    }

    /// The library's folder, identified by its database file.
    fn library(lib: &Library) -> Result<Self> {
        Ok(Pin {
            root: lib.root().to_path_buf(),
            id: FileId::of(&lib.root().join(DB_FILE))?,
        })
    }
}

impl Pinned {
    pub(crate) fn card(pocket: &Pocket) -> Result<Self> {
        Ok(Pinned {
            card: Some(Pin::card(pocket)?),
            library: None,
        })
    }

    pub(crate) fn library(lib: &Library) -> Result<Self> {
        Ok(Pinned {
            card: None,
            library: Some(Pin::library(lib)?),
        })
    }

    pub(crate) fn and_card(mut self, pocket: &Pocket) -> Result<Self> {
        self.card = Some(Pin::card(pocket)?);
        Ok(self)
    }

    /// The library's folder, when the plan was made against one.
    pub(crate) fn library_root(&self) -> Option<&Path> {
        self.library.as_ref().map(|p| p.root.as_path())
    }

    /// Fails with [`Error::Stale`] unless `pocket` is the pinned card. For
    /// work that keeps state about a card between steps (the ROM index).
    pub(crate) fn check_card(&self, pocket: &Pocket) -> Result<()> {
        match &self.card {
            Some(pin) if Pin::card(pocket)? != *pin => Err(Error::Stale {
                what: "the card".into(),
            }),
            _ => Ok(()),
        }
    }

    /// Fails with [`Error::Stale`] unless `lib` is the pinned library.
    pub(crate) fn check_library(&self, lib: &Library) -> Result<()> {
        match &self.library {
            Some(pin) if Pin::library(lib)? != *pin => Err(Error::Stale {
                what: "the library".into(),
            }),
            _ => Ok(()),
        }
    }
}

/// What applying a plan produced.
pub trait Applied: Serialize {
    /// Items that failed one by one while the rest went through.
    fn failures(&self) -> &[Failure] {
        &[]
    }

    fn failed(&self) -> usize {
        self.failures().len()
    }
}

/// One item that failed while the rest of a plan went through: which file,
/// and why.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Failure {
    pub path: PathBuf,
    pub error: ErrorInfo,
}

impl Failure {
    pub fn new(path: impl Into<PathBuf>, error: &crate::Error) -> Self {
        Failure {
            path: path.into(),
            error: error.to_info(),
        }
    }
}
