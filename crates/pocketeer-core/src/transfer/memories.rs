//! Memories (save states) on the card and in the library: which side has
//! each one, and plans to put one back on the card or throw one away. A
//! Memory is its `.sta` file plus any file of the same name beside it; they
//! always move together. Backups use the same history as cartridge saves
//! (see [`saves`]).

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{
    Error, Pocket, Result,
    card::memories::{self, Header, Memory},
    fsutil::names,
    library::Library,
    transfer::saves::{self, SavesPlan, TrashPlan, TrashTarget},
};

/// A Memory and where it is.
#[derive(Debug, Clone, Serialize)]
pub struct MemoryEntry {
    #[serde(flatten)]
    pub memory: Memory,
    pub place: Place,
}

/// Where a Memory is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Place {
    /// Only on the card: never backed up.
    Card,
    /// Only in the library.
    Library,
    /// On both, the library's copy with the card's size and time.
    BackedUp,
    /// On both, but the card's copy changed since the backup.
    Changed,
}

impl Place {
    pub fn on_card(self) -> bool {
        self != Place::Library
    }

    pub fn in_library(self) -> bool {
        self != Place::Card
    }

    /// The card has bytes the library doesn't.
    pub fn needs_backup(self) -> bool {
        matches!(self, Place::Card | Place::Changed)
    }
}

impl MemoryEntry {
    /// What the Memory is of: the ROM (named without its extension) or the
    /// cartridge's title; its file name when the header names neither.
    pub fn game(&self) -> String {
        match &self.memory.header {
            Header::Cartridge { title } if title.is_empty() => "cartridge".into(),
            Header::Cartridge { title } => title.clone(),
            Header::Core { game, .. } if !game.is_empty() => names::stem(game).to_string(),
            Header::Core { .. } => self
                .memory
                .rel_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        }
    }
}

/// Every Memory on the card (when there is one) and in the library, by path.
pub fn overview(lib: &Library, pocket: Option<&Pocket>) -> Result<Vec<MemoryEntry>> {
    let backed: Vec<Memory> = memories::scan(&lib.root().join("saves"))?;
    let on_card = match pocket {
        Some(p) => memories::scan(p.root())?,
        None => Vec::new(),
    };
    let mut entries: Vec<MemoryEntry> = on_card
        .into_iter()
        .map(|m| {
            let place = match backed.iter().find(|b| b.rel_path == m.rel_path) {
                None => Place::Card,
                Some(b) if b.size == m.size && b.mtime == m.mtime => Place::BackedUp,
                Some(_) => Place::Changed,
            };
            MemoryEntry { memory: m, place }
        })
        .collect();
    for b in backed {
        if !entries.iter().any(|e| e.memory.rel_path == b.rel_path) {
            entries.push(MemoryEntry {
                memory: b,
                place: Place::Library,
            });
        }
    }
    entries.sort_by(|a, b| a.memory.rel_path.cmp(&b.memory.rel_path));
    Ok(entries)
}

/// Plans putting the library's copy of the Memory at `rel` (and the files
/// beside it) back on the card. The card's copy, if different, is archived
/// first.
pub fn plan_restore(lib: &Library, pocket: &Pocket, rel: &Path) -> Result<SavesPlan> {
    let rels = files_of(&lib.root().join("saves"), rel)?;
    saves::plan_restore_all(lib, pocket, &rels)
}

/// Plans throwing away the Memory at `rel` with the files beside it: the
/// card's copy (archived into the library first) or the library's.
pub fn plan_trash(
    lib: &Library,
    pocket: Option<&Pocket>,
    rel: &Path,
    target: TrashTarget,
) -> Result<TrashPlan> {
    let rels = match (target, pocket) {
        (TrashTarget::Card, Some(p)) => files_of(p.root(), rel)?,
        (TrashTarget::Card, None) => {
            return Err(Error::SdNotFound {
                searched: Vec::new(),
            });
        }
        _ => files_of(&lib.root().join("saves"), rel)?,
    };
    saves::plan_trash(lib, pocket, &rels, target)
}

/// The `.sta` at `rel` and the files beside it, as found under `root`.
fn files_of(root: &Path, rel: &Path) -> Result<Vec<PathBuf>> {
    let found = memories::scan(root)?
        .into_iter()
        .find(|m| m.rel_path == rel)
        .ok_or_else(|| Error::RootNotFound {
            path: root.join(rel),
        })?;
    Ok(std::iter::once(found.rel_path)
        .chain(found.companions)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{library_with, sd, write_with_mtime as write};

    const STATES: &str = "Memories/Save States";

    #[test]
    fn each_memory_is_placed_by_what_each_side_holds() {
        let (_card, pocket) = sd();
        let (_lib_dir, lib) = library_with(&[]);
        let state = |name: &str| format!("{STATES}/{name}.sta");
        let saves = lib.root().join("saves");
        // Same bytes, same time on both sides.
        write(pocket.root(), &state("same"), "state", 100);
        write(&saves, &state("same"), "state", 100);
        // The card's copy was written again after the backup.
        write(pocket.root(), &state("newer"), "state two", 200);
        write(&saves, &state("newer"), "state", 100);
        write(pocket.root(), &state("card only"), "state", 100);
        write(&saves, &state("library only"), "state", 100);

        let placed: Vec<(String, Place)> = overview(&lib, Some(&pocket))
            .unwrap()
            .into_iter()
            .map(|e| (e.memory.rel_path.display().to_string(), e.place))
            .collect();
        assert_eq!(
            placed,
            [
                (state("card only"), Place::Card),
                (state("library only"), Place::Library),
                (state("newer"), Place::Changed),
                (state("same"), Place::BackedUp),
            ]
        );
        // Without a card, everything is the library's.
        assert!(
            overview(&lib, None)
                .unwrap()
                .iter()
                .all(|e| e.place == Place::Library)
        );
    }
}
