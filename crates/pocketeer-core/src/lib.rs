//! pocketeer-core: everything pocketeer knows about an Analogue Pocket SD card.
//!
//! The CLI and the TUI are thin layers over this crate. Nothing in here prints,
//! prompts, or exits; every operation returns data or an [`Error`]. Long
//! operations take a progress callback.

pub mod card;
pub mod catalog;
pub mod config;
pub mod error;
pub mod fsutil;
pub mod library;
pub mod net;
pub mod plan;
pub mod transfer;

#[cfg(test)]
pub(crate) mod testutil;

pub use card::Pocket;
pub use error::{Error, ErrorInfo, Result};
