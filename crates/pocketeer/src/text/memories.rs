//! Text for `ops::memories`.

use std::fmt::Write;

use super::{Label, Reader, Text, bytes, table, time_ago};

use pocketeer_core::transfer::memories::Place;

use crate::ops::memories::MemoriesReport;

impl Label for Place {
    fn label(&self) -> &'static str {
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
                    h.core().unwrap_or("cartridge").to_string(),
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
