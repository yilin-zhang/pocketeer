//! Memories (save states): list what the card and the library have, back
//! them up, put one back on the card, throw one away. A Memory is its
//! `.sta` file plus any file of the same name beside it; they always move
//! together. Backups use the same history as cartridge saves.

use std::path::{Path, PathBuf};

use pocketeer_core::{
    Error, Result,
    card::memories::{self, Memory},
    plan::Event,
    transfer::saves::{self, ConflictChoice, Scope, TrashTarget},
};
use serde::Serialize;

use super::{
    Mode,
    context::Context,
    saves::{SyncReport, TrashReport, plan_trash},
    stage,
};

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

#[derive(Debug, Serialize)]
pub struct MemoriesReport {
    /// Whether a card was there to look at.
    pub card: bool,
    pub memories: Vec<MemoryEntry>,
}

/// Every Memory on the card (when one is mounted) and in the library.
pub fn list(ctx: &Context) -> Result<MemoriesReport> {
    let lib = ctx.library()?;
    let backed: Vec<Memory> = memories::scan(&lib.root().join("saves"))?;
    let pocket = ctx.pocket().ok();
    let on_card = match &pocket {
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
    Ok(MemoriesReport {
        card: pocket.is_some(),
        memories: entries,
    })
}

/// `memories backup`: every new or changed Memory on the card.
pub fn backup(
    ctx: &Context,
    conflicts: Option<ConflictChoice>,
    mode: Mode,
    progress: &mut dyn FnMut(Event),
) -> Result<SyncReport> {
    super::saves::backup(ctx, Scope::Memories, conflicts, mode, progress)
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

/// The files of a Memory in the library, for restoring it.
pub fn library_files(ctx: &Context, rel: &Path) -> Result<Vec<PathBuf>> {
    files_of(&ctx.library()?.root().join("saves"), rel)
}

/// The files of a Memory on the card (`card`) or in the library.
pub fn files(ctx: &Context, rel: &Path, card: bool) -> Result<Vec<PathBuf>> {
    if card {
        files_of(ctx.pocket()?.root(), rel)
    } else {
        library_files(ctx, rel)
    }
}

/// `memories restore`: put the library's copy back on the card. The card's
/// copy, if different, is archived first.
pub fn restore(
    ctx: &Context,
    rel: &Path,
    mode: Mode,
    progress: &mut dyn FnMut(Event),
) -> Result<SyncReport> {
    let rels = library_files(ctx, rel)?;
    let plan = saves::plan_restore_paths(&ctx.library()?, &ctx.pocket()?, &rels)?;
    stage(ctx, mode, plan, progress)
}

/// `memories trash`: from the card (archived first) or from the library.
pub fn trash(ctx: &Context, rel: &Path, card: bool, mode: Mode) -> Result<TrashReport> {
    let rels = files(ctx, rel, card)?;
    let target = if card {
        TrashTarget::Card
    } else {
        TrashTarget::Library
    };
    let plan = plan_trash(ctx, &rels, target)?;
    stage(ctx, mode, plan, &mut |_| {})
}
