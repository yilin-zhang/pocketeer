//! Save files and save states: card ⇄ library, with history.

use std::path::{Path, PathBuf};

use pocketeer_core::{
    Result,
    plan::Event,
    transfer::saves::{
        self, ConflictChoice, SaveEntry, SaveVersion, SavesOptions, SavesPlan, Scope, TrashPlan,
        TrashTarget,
    },
};
use serde::Serialize;

use super::context::Context;

/// Plans backing up the card's saves in `scope`. `conflicts` settles every
/// conflict the same way; `None` leaves them for the user.
pub fn plan_backup(
    ctx: &Context,
    scope: Scope,
    conflicts: Option<ConflictChoice>,
    progress: &mut dyn FnMut(Event),
) -> Result<SavesPlan> {
    let mut plan = saves::plan(
        &ctx.library()?,
        &ctx.pocket()?,
        SavesOptions { scope, only: None },
        progress,
    )?;
    if let Some(choice) = conflicts {
        plan.resolve_all(choice);
    }
    Ok(plan)
}

#[derive(Debug, Serialize)]
pub struct ListReport {
    pub saves: Vec<SaveEntry>,
    pub bytes: u64,
}

pub fn list(ctx: &Context) -> Result<ListReport> {
    let lib = ctx.library()?;
    let saves = saves::list(&lib)?;
    Ok(ListReport {
        bytes: saves.iter().map(|s| s.size).sum(),
        saves,
    })
}

#[derive(Debug, Serialize)]
pub struct VersionsReport {
    pub rel_path: PathBuf,
    pub versions: Vec<SaveVersion>,
}

pub fn versions(ctx: &Context, rel: &Path) -> Result<VersionsReport> {
    let lib = ctx.library()?;
    Ok(VersionsReport {
        rel_path: rel.to_path_buf(),
        versions: saves::versions(&lib, rel)?,
    })
}

/// Plans throwing away copies of the saves at card paths `rels`. Only
/// deleting from the card needs the card.
pub fn plan_trash(ctx: &Context, rels: &[PathBuf], target: TrashTarget) -> Result<TrashPlan> {
    let pocket = match target {
        TrashTarget::Card => Some(ctx.pocket()?),
        _ => None,
    };
    saves::plan_trash(&ctx.library()?, pocket.as_ref(), rels, target)
}
