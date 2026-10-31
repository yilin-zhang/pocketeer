//! pocketeer-core: everything pocketeer knows about an Analogue Pocket SD card.
//!
//! The CLI and the TUI are thin layers over this crate. Nothing in here prints,
//! prompts, or exits; every operation returns data or an [`Error`]. Long
//! operations take a progress callback.

pub mod clean;
pub mod config;
pub mod cores;
pub mod error;
pub mod formats;
pub mod fsutil;
pub mod install;
pub mod inventory;
pub mod library;
pub mod net;
pub mod required;
pub mod roms;
pub mod sd;
pub mod status;
pub mod watch;

#[cfg(test)]
pub(crate) mod testutil;

pub use error::{Error, ErrorInfo, Result};
pub use sd::Pocket;

/// Progress events for long operations.
///
/// Core functions take a `Progress` callback and never print themselves.
/// The CLI turns these into stderr lines; the TUI into gauges.
pub mod progress {
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
}
