//! Text for `ops::memories`.

use std::fmt::Write;

use super::{bytes, table, time_ago};
use crate::ops::memories::{MemoriesReport, MemoryEntry};

/// What a Memory is of: the ROM (with its core) or the cartridge.
pub fn memory_game(e: &MemoryEntry) -> String {
    let h = &e.memory.header;
    match (h.game.is_empty(), h.cartridge) {
        // A core names the ROM file; the extension adds nothing here.
        (false, false) => pocketeer_core::fsutil::names::stem(&h.game).to_string(),
        (false, true) => h.game.clone(),
        (true, true) => "cartridge".into(),
        (true, false) => e
            .memory
            .rel_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    }
}

/// "card", "library" or "library + card".
pub fn memory_where(e: &MemoryEntry) -> &'static str {
    match (e.in_library, e.on_card) {
        (true, true) if e.backed_up => "library + card",
        (true, true) => "card (changed)",
        (true, false) => "library",
        _ => "card",
    }
}

pub fn memories_list(r: &MemoriesReport) -> String {
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
                memory_game(e),
                h.core.clone().unwrap_or_else(|| "cartridge".into()),
                memory_where(e).to_string(),
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
    let new = r
        .memories
        .iter()
        .filter(|e| e.on_card && !e.backed_up)
        .count();
    let _ = writeln!(
        out,
        "\n{} Memories, {} not backed up",
        r.memories.len(),
        new
    );
    out
}
