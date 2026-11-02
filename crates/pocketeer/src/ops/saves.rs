//! Save files and save states: card ⇄ library, with history.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use pocketeer_core::{
    Result,
    plan::{Event, Progress},
    transfer::saves::{
        self, ConflictChoice, SaveEntry, SaveVersion, SavesOptions, SavesPlan, SavesResult, Scope,
        TrashPlan, TrashResult, TrashTarget,
    },
};
use serde::Serialize;

use super::{Apply, Mode, Staged, context::Context, stage};

pub type SyncReport = Staged<SavesPlan>;

pub fn plan_sync(
    ctx: &Context,
    opts: SavesOptions,
    progress: &mut dyn FnMut(Event),
) -> Result<SavesPlan> {
    saves::plan(&ctx.library()?, &ctx.pocket()?, opts, progress)
}

impl Apply for SavesPlan {
    type Output = SavesResult;
    fn apply(&mut self, ctx: &Context, progress: Progress<'_>) -> Result<SavesResult> {
        let mut lib = ctx.library()?;
        let result = saves::apply(&mut lib, &ctx.pocket()?, self, progress)?;
        // New backups are linked to their games by name.
        let games = lib.list_games(None, None)?;
        saves::link_owners(&lib, &games, &BTreeMap::new())?;
        Ok(result)
    }
}

/// `saves backup`: pull newer card saves into the library. `conflicts`
/// resolves every conflict the same way; `None` leaves them unchanged (and
/// the command exits 1 while any remain).
pub fn backup(
    ctx: &Context,
    scope: Scope,
    conflicts: Option<ConflictChoice>,
    mode: Mode,
    progress: &mut dyn FnMut(Event),
) -> Result<SyncReport> {
    let opts = SavesOptions { scope, only: None };
    let mut plan = plan_sync(ctx, opts, progress)?;
    if let Some(choice) = conflicts {
        plan.resolve_all(choice);
    }
    stage(ctx, mode, plan, progress)
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
    stage(ctx, mode, plan, progress)
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

pub type TrashReport = Staged<TrashPlan>;

/// Plans throwing away copies of the saves at card paths `rels`. Only
/// deleting from the card needs the card.
pub fn plan_trash(ctx: &Context, rels: &[PathBuf], target: TrashTarget) -> Result<TrashPlan> {
    let pocket = match target {
        TrashTarget::Card => Some(ctx.pocket()?),
        _ => None,
    };
    saves::plan_trash(&ctx.library()?, pocket.as_ref(), rels, target)
}

impl Apply for TrashPlan {
    type Output = TrashResult;
    fn apply(&mut self, ctx: &Context, _: Progress<'_>) -> Result<TrashResult> {
        let pocket = match self.target {
            TrashTarget::Card => Some(ctx.pocket()?),
            _ => None,
        };
        saves::apply_trash(&mut ctx.library()?, pocket.as_ref(), self)
    }
}

/// `saves trash`.
pub fn trash(ctx: &Context, rel: &Path, target: TrashTarget, mode: Mode) -> Result<TrashReport> {
    let plan = plan_trash(ctx, &[rel.to_path_buf()], target)?;
    stage(ctx, mode, plan, &mut |_| {})
}
