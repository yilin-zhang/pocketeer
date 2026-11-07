//! The core of Pocketeer, covering games, the library, the device, and moving
//! games and saves between them.

pub mod cores;
pub mod cover;
pub mod dat;
pub mod device;
pub mod error;
pub mod game;
pub mod library;
pub mod log;
mod os;
pub mod platform;
pub mod pocketeer;
pub mod sources;
pub mod transfer;

pub use pocketeer::Pocketeer;
