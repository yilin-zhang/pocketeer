//! Save files and save states: card ⇄ library, with history.

use std::path::{Path, PathBuf};

use pocketeer_core::{
    Result,
    plan::Event,
    transfer::saves::{
        self, ConflictChoice, SaveEntry, SaveVersion, SavesOptions, SavesPlan, SavesResult,
    },
};
use serde::Serialize;

use super::{Mode, Staged, context::Context, stage};

pub type SyncReport = Staged<SavesPlan, SavesResult>;

pub fn plan_sync(
    ctx: &Context,
    include_states: bool,
    progress: &mut dyn FnMut(Event),
) -> Result<SavesPlan> {
    saves::plan(
        &ctx.library()?,
        &ctx.pocket()?,
        SavesOptions { include_states },
        progress,
    )
}

pub fn apply_sync(
    ctx: &Context,
    plan: &SavesPlan,
    progress: &mut dyn FnMut(Event),
) -> Result<SavesResult> {
    saves::apply(&mut ctx.library()?, &ctx.pocket()?, plan, progress)
}

/// `saves backup`: pull newer card saves into the library. `conflicts`
/// resolves every conflict the same way; `None` leaves them unchanged (and
/// the command exits 1 while any remain).
pub fn backup(
    ctx: &Context,
    include_states: bool,
    conflicts: Option<ConflictChoice>,
    mode: Mode,
    progress: &mut dyn FnMut(Event),
) -> Result<SyncReport> {
    let mut plan = plan_sync(ctx, include_states, progress)?;
    if let Some(choice) = conflicts {
        for i in 0..plan.actions.len() {
            plan.resolve(i, choice);
        }
    }
    stage(mode, plan, |p| apply_sync(ctx, p, progress))
}

/// `saves restore`: push the library's copy (or an older version) to the card.
pub fn restore(
    ctx: &Context,
    rel: &Path,
    version: Option<i64>,
    mode: Mode,
    progress: &mut dyn FnMut(Event),
) -> Result<SyncReport> {
    let plan = saves::plan_restore(&ctx.library()?, &ctx.pocket()?, rel, version)?;
    stage(mode, plan, |p| apply_sync(ctx, p, progress))
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
