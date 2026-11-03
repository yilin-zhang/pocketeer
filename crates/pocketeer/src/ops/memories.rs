//! Memories (save states): list what the card and the library have, back
//! them up, put one back on the card, throw one away. A Memory is its
//! `.sta` file plus any file of the same name beside it; they always move
//! together. Backups use the same history as cartridge saves.

use std::path::Path;

use pocketeer_core::{
    Result,
    plan::Event,
    transfer::{
        memories::{self, MemoryEntry},
        saves::{ConflictChoice, SavesPlan, Scope, TrashPlan, TrashTarget},
    },
};
use serde::Serialize;

use super::{
    Mode,
    context::Context,
    saves::{SyncReport, TrashReport},
    stage,
};

#[derive(Debug, Serialize)]
pub struct MemoriesReport {
    /// Whether a card was there to look at.
    pub card: bool,
    pub memories: Vec<MemoryEntry>,
}

/// Every Memory on the card (when one is mounted) and in the library.
pub fn list(ctx: &Context) -> Result<MemoriesReport> {
    let pocket = ctx.pocket().ok();
    Ok(MemoriesReport {
        card: pocket.is_some(),
        memories: memories::overview(&ctx.library()?, pocket.as_ref())?,
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

/// `memories restore`: put the library's copy back on the card. The card's
/// copy, if different, is archived first.
pub fn restore(
    ctx: &Context,
    rel: &Path,
    mode: Mode,
    progress: &mut dyn FnMut(Event),
) -> Result<SyncReport> {
    stage(ctx, mode, plan_restore(ctx, rel)?, progress)
}

/// Plans putting the library's copy of the Memory at `rel` (and the files
/// beside it) back on the card.
pub fn plan_restore(ctx: &Context, rel: &Path) -> Result<SavesPlan> {
    memories::plan_restore(&ctx.library()?, &ctx.pocket()?, rel)
}

/// `memories trash`: from the card (archived first) or from the library.
pub fn trash(ctx: &Context, rel: &Path, target: TrashTarget, mode: Mode) -> Result<TrashReport> {
    stage(ctx, mode, plan_trash(ctx, rel, target)?, &mut |_| {})
}

/// Plans throwing away the Memory at `rel` with the files beside it.
pub fn plan_trash(ctx: &Context, rel: &Path, target: TrashTarget) -> Result<TrashPlan> {
    let pocket = match target {
        TrashTarget::Card => Some(ctx.pocket()?),
        _ => None,
    };
    memories::plan_trash(&ctx.library()?, pocket.as_ref(), rel, target)
}
