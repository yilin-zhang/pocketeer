//! The card itself and what is on it: status, the core inventory, BIOS and
//! data files, macOS junk, and ROM transfers in both directions.

use std::path::{Path, PathBuf};

use pocketeer_core::{
    Error, Result,
    card::{
        Status,
        bios::{self, BiosReport, PlacePlan},
        clean::{self, CleanPlan, CleanResult},
        install, rename as card_rename,
        roms::{CardRom, Index},
    },
    catalog::inventory::{Inventory, Platforms},
    library::import::{ImportPlan, ImportResult},
    names::RenameKind,
    net::{Policy, Source},
    plan::{Event, Progress},
    transfer::{
        self, Comparison,
        from_card::{self, CardImportPlan},
        to_card,
    },
};
use serde::Serialize;

use super::{
    Apply, Mode, Staged, apply_actions,
    context::Context,
    library::{ImportSettings, Imported, ImportsRoms, suggest_standard_names},
    load_dat, stage,
};
use crate::output::Outcome;

pub fn status(ctx: &Context) -> Result<Status> {
    pocketeer_core::card::status(&ctx.pocket()?)
}

#[derive(Debug, Serialize)]
pub struct RefreshReport {
    pub cores: usize,
    pub platforms: usize,
    pub cores_source: Source,
    pub platforms_source: Source,
    pub cache_dir: std::path::PathBuf,
}

pub fn inventory_refresh(ctx: &Context) -> Result<RefreshReport> {
    let net = ctx.net()?;
    let policy = if ctx.policy() == Policy::Offline {
        Policy::Offline
    } else {
        Policy::Refresh
    };
    let inventory = Inventory::load(&net.http, &net.cache, policy)?;
    let platforms = Platforms::load(&net.http, &net.cache, policy)?;
    Ok(RefreshReport {
        cores: inventory.cores.len(),
        platforms: platforms.platforms.len(),
        cores_source: inventory.source,
        platforms_source: platforms.source,
        cache_dir: net.cache.dir().to_path_buf(),
    })
}

/// `present`: also list the files that are already in place.
pub fn bios_check(ctx: &Context, ids: &[String], present: bool) -> Result<BiosReport> {
    let pocket = ctx.pocket()?;
    let installed = pocketeer_core::card::cores::list_installed(&pocket)?;
    let only = (!ids.is_empty()).then_some(ids);
    let mut report = bios::check(&pocket, &installed, only)?;
    if !present {
        for core in &mut report.cores {
            core.files
                .retain(|f| !matches!(f.status, bios::FileStatus::Present));
        }
    }
    Ok(report)
}

pub type PlaceReport = Staged<PlacePlan>;

impl Apply for PlacePlan {
    type Output = install::Changes;
    fn apply(&mut self, ctx: &Context, progress: Progress<'_>) -> Result<install::Changes> {
        apply_actions(&ctx.pocket()?, "bios-place", &self.actions, progress)
    }
}

pub fn bios_place(
    ctx: &Context,
    mode: Mode,
    progress: &mut dyn FnMut(Event),
) -> Result<PlaceReport> {
    let pocket = ctx.pocket()?;
    let installed = pocketeer_core::card::cores::list_installed(&pocket)?;
    let report = bios::check(&pocket, &installed, None)?;
    stage(ctx, mode, bios::plan_place(&report), progress)
}

pub type CleanReport = Staged<CleanPlan>;

impl Apply for CleanPlan {
    type Output = CleanResult;
    fn apply(&mut self, ctx: &Context, _: Progress<'_>) -> Result<CleanResult> {
        clean::apply(&ctx.pocket()?, self)
    }
}

pub fn clean(
    ctx: &Context,
    system: bool,
    mode: Mode,
    progress: Progress<'_>,
) -> Result<CleanReport> {
    let plan = clean::plan(&ctx.pocket()?, system)?;
    stage(ctx, mode, plan, progress)
}

#[derive(Debug, Serialize)]
pub struct RomsReport {
    pub roms: Vec<CardRom>,
    pub comparison: Comparison,
    pub warnings: Vec<String>,
}

impl Outcome for RomsReport {
    fn error(&self) -> Option<Error> {
        let failed = self.roms.iter().filter(|r| r.error.is_some()).count();
        (failed > 0).then_some(Error::SomeFailed {
            failed,
            total: self.roms.len(),
        })
    }
}

/// Card ROMs with their library presence. `check` verifies cached
/// identities first (`Some(true)` reads every byte); `None` lists only.
pub fn roms(
    ctx: &Context,
    check: Option<bool>,
    progress: &mut dyn FnMut(Event),
) -> Result<RomsReport> {
    let pocket = ctx.pocket()?;
    let games = match ctx.library() {
        Ok(lib) => lib.list_games(None, None)?,
        Err(Error::LibraryNotFound { .. }) => Vec::new(),
        Err(e) => return Err(e),
    };
    let mut index = Index::scan(&pocket)?;
    let mut warnings: Vec<String> = index.warning.take().into_iter().collect();
    if let Some(full) = check {
        for i in 0..index.roms.len() {
            progress(Event::Status(format!(
                "Checking {}",
                index.roms[i].rel_path.display()
            )));
            index.verify(&pocket, i, full, &|| false);
            progress(Event::Step {
                done: i + 1,
                total: index.roms.len(),
            });
        }
        if let Err(e) = index.save(&pocket) {
            warnings.push(format!(
                "ROM identities updated in memory; cache not saved: {e}"
            ));
        }
    }
    Ok(RomsReport {
        comparison: transfer::compare(&games, &index.roms),
        roms: index.roms,
        warnings,
    })
}

pub type CopyReport = Staged<to_card::CopyPlan>;

pub fn plan_copy(
    ctx: &Context,
    platform: Option<&str>,
    roms: &[String],
    progress: &mut dyn FnMut(Event),
) -> Result<to_card::CopyPlan> {
    to_card::plan(&ctx.library()?, &ctx.pocket()?, platform, roms, progress)
}

impl Apply for to_card::CopyPlan {
    type Output = to_card::CopyResult;
    fn apply(&mut self, ctx: &Context, progress: Progress<'_>) -> Result<Self::Output> {
        to_card::apply(&ctx.pocket()?, self, progress)
    }
}

pub type RenameReport = Staged<card_rename::CardRenamePlan>;

pub fn plan_rename(ctx: &Context, path: &Path, name: &str) -> Result<card_rename::CardRenamePlan> {
    card_rename::plan_card(&ctx.pocket()?, path, name)
}

impl Apply for card_rename::CardRenamePlan {
    type Output = card_rename::CardRenameResult;
    fn apply(&mut self, ctx: &Context, _: Progress<'_>) -> Result<Self::Output> {
        let result = card_rename::apply_card(&ctx.pocket()?, self)?;
        // Backed-up saves follow, or the next backup would see them as new.
        let saves: Vec<_> = self
            .moves
            .iter()
            .filter(|m| m.kind == RenameKind::Save)
            .map(|m| (m.from.clone(), m.to.clone()))
            .collect();
        if !saves.is_empty()
            && let Ok(lib) = ctx.library()
        {
            pocketeer_core::transfer::saves::follow_renames(&lib, &saves)?;
        }
        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// Importing card ROMs into the library.

/// What `card import` was asked to do.
#[derive(Debug, Clone, Default)]
pub struct ImportRequest {
    /// Only this platform.
    pub platform: Option<String>,
    /// Card-relative ROM paths; empty means every unmatched file.
    pub paths: Vec<PathBuf>,
    pub settings: ImportSettings,
}

/// `not_found` is in the plan: names asked for that aren't card-only ROMs.
pub type ImportReport = Imported<CardImportPlan>;

/// Plans copying card-only ROMs into the library, plus the names asked for
/// that aren't orphans on the card.
pub fn plan_import(
    ctx: &Context,
    req: &ImportRequest,
    progress: &mut dyn FnMut(Event),
) -> Result<CardImportPlan> {
    let lib = ctx.library()?;
    let pocket = ctx.pocket()?;
    let dat = load_dat(req.settings.dat.as_deref())?;
    let all = from_card::list(&lib, &pocket, req.platform.as_deref())?;
    let (roms, not_found) = if req.paths.is_empty() {
        (all, Vec::new())
    } else {
        let picked: Vec<CardRom> = all
            .iter()
            .filter(|r| req.paths.contains(&r.rel_path))
            .cloned()
            .collect();
        let missing = req
            .paths
            .iter()
            .filter(|n| !picked.iter().any(|r| r.rel_path == **n))
            .cloned()
            .collect();
        (picked, missing)
    };
    let mut plan = from_card::plan_import(
        &lib,
        &pocket,
        &roms,
        not_found,
        dat.as_ref(),
        &req.settings.options(ctx),
        &mut ctx.hash_cache(),
        progress,
    )?;
    suggest_standard_names(ctx, &lib, &mut plan.plan, req.settings.standard_names)?;
    Ok(plan)
}

/// Copies the planned ROMs into the library. The card itself is not changed.
impl Apply for CardImportPlan {
    type Output = ImportResult;
    fn apply(&mut self, ctx: &Context, progress: Progress<'_>) -> Result<Self::Output> {
        from_card::apply_import(&mut ctx.library()?, &ctx.pocket()?, self, progress)
    }
}

impl ImportsRoms for CardImportPlan {
    fn import(&self) -> &ImportPlan {
        &self.plan
    }

    fn import_mut(&mut self) -> &mut ImportPlan {
        &mut self.plan
    }

    fn not_found(&self) -> &[PathBuf] {
        &self.not_found
    }
}

pub fn import(
    ctx: &Context,
    req: &ImportRequest,
    mode: Mode,
    progress: &mut dyn FnMut(Event),
) -> Result<ImportReport> {
    let plan = plan_import(ctx, req, progress)?;
    Imported::stage(ctx, mode, plan, req.settings.covers, progress)
}
