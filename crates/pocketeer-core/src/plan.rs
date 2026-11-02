//! The shape every write operation in this crate shares.
//!
//! An operation is a `plan_*` function that returns a plan (plain data,
//! serialisable, nothing done yet) and an `apply_*` function that takes the
//! plan and returns what happened. The CLI prints the plan, applies it unless
//! `--dry-run`, then prints the result; the TUI shows the plan in a dialog and
//! applies it on confirmation. These two traits are what those layers need
//! from a plan and a result without knowing which operation they belong to.

use std::path::PathBuf;

use serde::Serialize;

use crate::ErrorInfo;

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

/// Something that can be previewed and then applied.
pub trait Plan: Serialize {
    /// How many things applying this plan would change. Zero means there is
    /// nothing to do and the plan is not applied.
    fn changes(&self) -> usize;

    /// Problems found while planning that count as failures even though
    /// nothing ran (an unresolved save conflict, say). Default none.
    fn errors(&self) -> usize {
        0
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
