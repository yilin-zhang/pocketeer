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
    library::{covers::FetchReport, import::ImportOptions},
    net::{Policy, Source},
    plan::Event,
    transfer::{
        self, Comparison,
        from_card::{self, CardImportPlan, PlatformImport},
        to_card,
    },
};
use serde::Serialize;

use super::{
    Mode, Staged, apply_actions,
    context::Context,
    library::{metadata_after_import, suggest_standard_names},
    load_dat, stage, stage_if, verdict,
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

pub type PlaceReport = Staged<PlacePlan, install::Changes>;

pub fn bios_place(
    ctx: &Context,
    mode: Mode,
    progress: &mut dyn FnMut(Event),
) -> Result<PlaceReport> {
    let pocket = ctx.pocket()?;
    let installed = pocketeer_core::card::cores::list_installed(&pocket)?;
    let report = bios::check(&pocket, &installed, None)?;
    stage(mode, bios::plan_place(&report), |plan| {
        apply_actions(&pocket, "bios-place", &plan.actions, progress)
    })
}

pub type CleanReport = Staged<CleanPlan, CleanResult>;

pub fn clean(ctx: &Context, system: bool, mode: Mode) -> Result<CleanReport> {
    let pocket = ctx.pocket()?;
    let plan = clean::plan(&pocket, system)?;
    stage(mode, plan, |plan| clean::apply(&pocket, plan))
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

pub type CopyReport = Staged<to_card::CopyPlan, to_card::CopyResult>;

pub fn plan_copy(
    ctx: &Context,
    platform: Option<&str>,
    roms: &[String],
    progress: &mut dyn FnMut(Event),
) -> Result<to_card::CopyPlan> {
    to_card::plan(&ctx.library()?, &ctx.pocket()?, platform, roms, progress)
}

pub fn apply_copy(
    ctx: &Context,
    plan: &to_card::CopyPlan,
    progress: &mut dyn FnMut(Event),
) -> Result<to_card::CopyResult> {
    to_card::apply(&ctx.pocket()?, plan, progress)
}

pub type RenameReport = Staged<card_rename::CardRenamePlan, card_rename::CardRenameResult>;

pub fn plan_rename(ctx: &Context, path: &Path, name: &str) -> Result<card_rename::CardRenamePlan> {
    card_rename::plan_card(&ctx.pocket()?, path, name)
}

pub fn apply_rename(
    ctx: &Context,
    plan: &card_rename::CardRenamePlan,
) -> Result<card_rename::CardRenameResult> {
    card_rename::apply_card(&ctx.pocket()?, plan)
}

// ---------------------------------------------------------------------------
// Importing card ROMs into the library.

/// What `card import` was asked to do.
#[derive(Debug, Clone)]
pub struct ImportRequest {
    /// Only this platform.
    pub platform: Option<String>,
    /// Card-relative ROM paths; empty means every unmatched file.
    pub paths: Vec<PathBuf>,
    pub dat: Option<PathBuf>,
    pub dat_only: bool,
    pub standard_names: bool,
    /// Fetch covers for the new games afterwards (when online).
    pub covers: bool,
}

impl Default for ImportRequest {
    fn default() -> Self {
        ImportRequest {
            platform: None,
            paths: Vec::new(),
            dat: None,
            dat_only: false,
            standard_names: false,
            covers: true,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ImportReport {
    #[serde(flatten)]
    pub staged: Staged<CardImportPlan, Vec<PlatformImport>>,
    /// Names asked for that aren't orphans on the card.
    pub not_found: Vec<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub covers: Option<FetchReport>,
    /// Problems identifying or fetching covers for the new games.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl ImportReport {
    /// A plan shown in a TUI dialog before it is applied.
    pub fn preview(plan: CardImportPlan) -> Self {
        ImportReport {
            staged: Staged::preview(plan),
            not_found: Vec::new(),
            covers: None,
            warnings: Vec::new(),
        }
    }
}

impl Outcome for ImportReport {
    fn error(&self) -> Option<Error> {
        verdict(
            self.staged.changes(),
            self.staged.failed() + self.not_found.len(),
        )
    }
}

/// Plans copying card-only ROMs into the library, plus the names asked for
/// that aren't orphans on the card.
pub fn plan_import(
    ctx: &Context,
    req: &ImportRequest,
    progress: &mut dyn FnMut(Event),
) -> Result<(CardImportPlan, Vec<PathBuf>)> {
    let lib = ctx.library()?;
    let pocket = ctx.pocket()?;
    let dat = load_dat(req.dat.as_deref())?;
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
        dat.as_ref(),
        &|id| ImportOptions {
            platform: id.into(),
            platform_name: ctx.platform_name(id),
            dat_only: req.dat_only,
        },
        &mut ctx.hash_cache(),
        progress,
    )?;
    for part in &mut plan.parts {
        suggest_standard_names(
            ctx,
            &lib,
            &part.platform,
            &mut part.plan,
            req.standard_names,
        )?;
    }
    Ok((plan, not_found))
}

/// Copies the planned ROMs into the library. The card itself is not changed.
pub fn apply_import(
    ctx: &Context,
    plan: &CardImportPlan,
    progress: &mut dyn FnMut(Event),
) -> Result<Vec<PlatformImport>> {
    from_card::apply_import(&mut ctx.library()?, &ctx.pocket()?, plan, progress)
}

pub fn import(
    ctx: &Context,
    req: &ImportRequest,
    mode: Mode,
    progress: &mut dyn FnMut(Event),
) -> Result<ImportReport> {
    let (plan, not_found) = plan_import(ctx, req, progress)?;
    let staged = stage_if(mode, not_found.is_empty(), plan, |plan| {
        apply_import(ctx, plan, progress)
    })?;
    let added = staged
        .result
        .as_ref()
        .map_or(0, |r| r.iter().map(|p| p.result.added).sum());
    let metadata =
        metadata_after_import(ctx, None, (added, req.covers), progress).unwrap_or_default();
    Ok(ImportReport {
        staged,
        not_found,
        warnings: metadata.problems().map(str::to_owned).collect(),
        covers: metadata.covers,
    })
}
