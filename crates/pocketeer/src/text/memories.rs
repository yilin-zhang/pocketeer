//! Text for `ops::memories`.

use std::fmt::Write;

use super::{Reader, Text, bytes, table, time_ago};
use crate::ops::memories::{MemoriesReport, MemoryEntry, Place};

impl MemoryEntry {
    /// What the Memory is of: the ROM (with its core) or the cartridge.
    pub fn game(&self) -> String {
        let h = &self.memory.header;
        match (h.game.is_empty(), h.cartridge) {
            // A core names the ROM file; the extension adds nothing here.
            (false, false) => pocketeer_core::names::stem(&h.game).to_string(),
            (false, true) => h.game.clone(),
            (true, true) => "cartridge".into(),
            (true, false) => self
                .memory
                .rel_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        }
    }
}

impl Place {
    /// The WHERE column.
    pub fn label(self) -> &'static str {
        match self {
            Place::Card => "card",
            Place::Library => "library",
            Place::BackedUp => "library + card",
            Place::Changed => "card (changed)",
        }
    }
}

impl Text for MemoriesReport {
    fn text(&self, _: Reader) -> String {
        let r = self;
        let mut out = String::new();
        if r.memories.is_empty() {
            let _ = writeln!(
                out,
                "{}",
                if r.card {
                    "No Memories on the card or in the library."
                } else {
                    "No Memories in the library, and no card mounted."
                }
            );
            return out;
        }
        let rows: Vec<[String; 6]> = r
            .memories
            .iter()
            .map(|e| {
                let h = &e.memory.header;
                [
                    e.game(),
                    h.core.clone().unwrap_or_else(|| "cartridge".into()),
                    e.place.label().to_string(),
                    time_ago(e.memory.mtime),
                    bytes(e.memory.size),
                    e.memory.rel_path.display().to_string(),
                ]
            })
            .collect();
        table(
            &mut out,
            ["GAME", "CORE", "WHERE", "SAVED", "SIZE", "PATH"],
            &rows,
        );
        let new = r.memories.iter().filter(|e| e.place.needs_backup()).count();
        let _ = writeln!(
            out,
            "\n{} Memories, {} not backed up",
            r.memories.len(),
            new
        );
        out
    }
}
