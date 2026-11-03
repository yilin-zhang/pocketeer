//! Memories (save states): what the card and the library have, and plans to
//! put one back on the card or throw one away. Backing them up is a saves
//! backup with `Scope::Memories`.

use std::path::Path;

use pocketeer_core::{
    Result,
    transfer::{
        memories::{self, MemoryEntry},
        saves::{SavesPlan, TrashPlan, TrashTarget},
    },
};
use serde::Serialize;

use super::context::Context;

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

/// Plans putting the library's copy of the Memory at `rel` (and the files
/// beside it) back on the card.
pub fn plan_restore(ctx: &Context, rel: &Path) -> Result<SavesPlan> {
    memories::plan_restore(&ctx.library()?, &ctx.pocket()?, rel)
}

/// Plans throwing away the Memory at `rel` with the files beside it.
pub fn plan_trash(ctx: &Context, rel: &Path, target: TrashTarget) -> Result<TrashPlan> {
    let pocket = match target {
        TrashTarget::Card => Some(ctx.pocket()?),
        _ => None,
    };
    memories::plan_trash(&ctx.library()?, pocket.as_ref(), rel, target)
}
