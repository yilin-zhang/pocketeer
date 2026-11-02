//! The local library: creating it, importing, naming, removing, identifying
//! ROMs against DATs, and box art.

use std::path::{Path, PathBuf};

use pocketeer_core::{
    Error, Result,
    library::{
        self, Game, Library,
        covers::{self, CoverStats, FetchOptions, FetchReport},
        import::{self, ImportAction, ImportOptions, ImportPlan, ImportResult},
        remove::{self, RemovePlan, RemoveResult},
        rename::{self, RenamePlan, RenameResult},
    },
    plan::{Event, Progress},
};
use serde::Serialize;

use super::{Apply, Batch, Mode, Staged, context::Context, load_dat, stage, stage_if};
use crate::output::Outcome;

#[derive(Debug, Serialize)]
pub struct InitReport {
    pub root: PathBuf,
    pub created: bool,
}

pub fn init(ctx: &Context) -> Result<InitReport> {
    let root = ctx.library_path()?;
    Library::init(&root)?;
    Ok(InitReport {
        root,
        created: true,
    })
}

pub fn status(ctx: &Context) -> Result<library::Status> {
    ctx.library()?.status()
}

/// What an import did: the plan (and its result), then the identification
/// and cover fetch that follow when games were added. The same for a folder
/// import and a card import.
/// A plan that copies ROMs into the library: from a folder on this
/// computer ([`ImportPlan`]) or from the card (`CardImportPlan`).
pub trait ImportsRoms: Apply<Output = ImportResult> {
    fn import(&self) -> &ImportPlan;
    fn import_mut(&mut self) -> &mut ImportPlan;
    /// Card paths asked for that aren't there; nothing runs while any are.
    fn not_found(&self) -> &[PathBuf] {
        &[]
    }
}

impl ImportsRoms for ImportPlan {
    fn import(&self) -> &ImportPlan {
        self
    }

    fn import_mut(&mut self) -> &mut ImportPlan {
        self
    }
}

#[derive(Serialize)]
pub struct Imported<P: Apply> {
    #[serde(flatten)]
    pub staged: Staged<P>,
    /// Covers fetched for the new games afterwards (online only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub covers: Option<FetchReport>,
    /// Problems identifying or fetching covers for the new games; the
    /// import itself succeeded.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl<P: ImportsRoms> Imported<P> {
    /// Stages `plan`, then identifies and fetches covers for what it added.
    pub fn stage(
        ctx: &Context,
        mode: Mode,
        plan: P,
        covers: bool,
        progress: Progress<'_>,
    ) -> Result<Self> {
        let allowed = plan.not_found().is_empty();
        // Only the platforms that get games need identifying afterwards.
        let platforms: Vec<String> = plan
            .import()
            .actions
            .iter()
            .filter(|a| matches!(a, ImportAction::Add { .. }))
            .map(|a| a.platform().to_owned())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let staged = stage_if(ctx, mode, allowed, plan, progress)?;
        let added = staged.result.as_ref().map_or(0, |r| r.added);
        let metadata =
            metadata_after_import(ctx, &platforms, (added, covers), progress).unwrap_or_default();
        Ok(Imported {
            staged,
            warnings: metadata.problems().map(str::to_owned).collect(),
            covers: metadata.covers,
        })
    }
}

impl<P: Apply> Outcome for Imported<P> {
    fn error(&self) -> Option<Error> {
        self.staged.error()
    }
}

pub type ImportReport = Imported<ImportPlan>;

/// How an import treats its files, wherever they come from (a folder or
/// the card).
#[derive(Debug, Clone)]
pub struct ImportSettings {
    /// A DAT to verify and name the files with.
    pub dat: Option<PathBuf>,
    /// With a DAT: skip files it doesn't know instead of importing them unverified.
    pub dat_only: bool,
    /// Import files under the standard name they match (needs the network).
    pub standard_names: bool,
    /// Fetch covers for the new games afterwards (when online).
    pub covers: bool,
}

impl Default for ImportSettings {
    fn default() -> Self {
        ImportSettings {
            dat: None,
            dat_only: false,
            standard_names: false,
            covers: true,
        }
    }
}

/// What `library import` was asked to do.
#[derive(Debug, Clone)]
pub struct ImportRequest {
    /// A folder of ROMs (scanned recursively) or one ROM / zip.
    pub source: PathBuf,
    /// Every file is this platform's; without it each file's extension says.
    pub platform: Option<String>,
    pub settings: ImportSettings,
}

impl ImportSettings {
    pub(super) fn options(&self, ctx: &Context) -> ImportOptions {
        ImportOptions {
            dat_only: self.dat_only,
            platform_names: ctx.platform_names(),
        }
    }
}

/// What re-checking the library's metadata did: DAT identification, then
/// (when asked and online) cover downloads. Neither step aborts the other.
#[derive(Debug, Default, Serialize)]
pub struct MetadataReport {
    /// `None` when identification could not run at all (see `errors`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identify: Option<library::identify::Report>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub covers: Option<FetchReport>,
    /// A step that failed as a whole (no library, no HTTP client, …).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

impl MetadataReport {
    /// Whether the library's rows or files changed (names learned, covers saved).
    pub fn changed(&self) -> bool {
        self.identify
            .as_ref()
            .is_some_and(|r| r.matched + r.no_match + r.ambiguous > 0)
            || self.covers.as_ref().is_some_and(|c| c.summary.fetched > 0)
    }

    /// Every problem worth telling the user: whole-step failures, then the
    /// per-platform ones each step reported.
    pub fn problems(&self) -> impl Iterator<Item = &str> {
        self.errors
            .iter()
            .chain(self.identify.iter().flat_map(|r| r.errors.iter()))
            .map(String::as_str)
    }
}

/// Identifies the ROMs of `platforms` (all when `None`) against DATs, then
/// fetches missing covers if `covers` is set and we're online.
pub fn refresh_metadata(
    ctx: &Context,
    platforms: Option<&[String]>,
    covers: bool,
    progress: &mut dyn FnMut(Event),
) -> MetadataReport {
    let mut report = MetadataReport::default();
    match identify(ctx, platforms, progress) {
        Ok(r) => report.identify = Some(r),
        Err(e) => report.errors.push(format!("DAT check: {e}")),
    }
    if covers && !ctx.offline() {
        match covers_fetch(ctx, platforms, false, None, progress) {
            Ok(c) => report.covers = Some(c),
            Err(e) => report.errors.push(format!("Covers: {e}")),
        }
    }
    report
}

/// After an import that added games: identify them and fetch their covers.
/// Nothing happens when the import added none.
pub fn metadata_after_import(
    ctx: &Context,
    platforms: &[String],
    (added, covers): (usize, bool),
    progress: &mut dyn FnMut(Event),
) -> Option<MetadataReport> {
    (added > 0).then(|| refresh_metadata(ctx, Some(platforms), covers, progress))
}

/// Standard-name list for a platform (online only; `None` means no suggestions).
pub fn names_for(ctx: &Context, platform: &str) -> Option<Vec<String>> {
    if ctx.offline() {
        return None;
    }
    let net = ctx.net().ok()?;
    pocketeer_core::catalog::libretro::standard_names_for_platform(
        &net.http,
        &net.cache,
        platform,
        ctx.policy(),
    )
}

/// Marks each unverified file in `plan` with the standard name it matches;
/// with `take` (`--standard-names`) the plan imports under those names.
pub fn suggest_standard_names(
    ctx: &Context,
    lib: &Library,
    plan: &mut ImportPlan,
    take: bool,
) -> Result<()> {
    let platforms: Vec<String> = plan.platforms().into_iter().map(str::to_owned).collect();
    let mut suggested = 0;
    for platform in platforms {
        if let Some(names) = names_for(ctx, &platform) {
            suggested += import::suggest_names(plan, &platform, &names);
        }
    }
    if suggested > 0 && take {
        import::apply_suggestions(lib, plan, |_, _| true)?;
    }
    Ok(())
}

/// Platform ids the library holds, plus those the card's cores run: what a
/// file extension may name.
pub fn known_platforms(ctx: &Context) -> Result<Vec<String>> {
    let mut ids: Vec<String> = ctx
        .library()?
        .platform_stats()?
        .into_iter()
        .map(|p| p.platform)
        .collect();
    if let Ok(pocket) = ctx.pocket() {
        for core in pocketeer_core::card::cores::list_installed(&pocket)?.cores {
            ids.extend(core.platform_ids);
        }
    }
    ids.sort();
    ids.dedup();
    Ok(ids)
}

/// Plans an import: what would be added, skipped, and (when online) which
/// files have a standard name to suggest.
pub fn plan_import(
    ctx: &Context,
    req: &ImportRequest,
    progress: Progress<'_>,
) -> Result<ImportPlan> {
    let lib = ctx.library()?;
    let dat = load_dat(req.settings.dat.as_deref())?;
    let mut plan = import::plan_folder(
        &lib,
        &req.source,
        req.platform.as_deref(),
        &known_platforms(ctx)?,
        &req.settings.options(ctx),
        dat.as_ref(),
        &mut ctx.hash_cache(),
        progress,
    )?;
    suggest_standard_names(ctx, &lib, &mut plan, req.settings.standard_names)?;
    Ok(plan)
}

impl Apply for ImportPlan {
    type Output = ImportResult;
    fn apply(&mut self, ctx: &Context, progress: Progress<'_>) -> Result<ImportResult> {
        import::apply_import(&mut ctx.library()?, self, progress)
    }
}

pub fn import(
    ctx: &Context,
    req: &ImportRequest,
    mode: Mode,
    progress: Progress<'_>,
) -> Result<ImportReport> {
    let plan = plan_import(ctx, req, progress)?;
    Imported::stage(ctx, mode, plan, req.settings.covers, progress)
}

#[derive(Debug, Serialize)]
pub struct ListReport {
    pub games: Vec<Game>,
    pub roms: usize,
    pub bytes: u64,
}

pub fn list(ctx: &Context, platform: Option<&str>, query: Option<&str>) -> Result<ListReport> {
    let lib = ctx.library()?;
    let games = lib.list_games(platform, query)?;
    Ok(ListReport {
        roms: games.iter().map(|g| g.roms.len()).sum(),
        bytes: games
            .iter()
            .flat_map(|g| g.roms.iter())
            .map(|r| r.size)
            .sum(),
        games,
    })
}

pub type RenameReport = Staged<RenamePlan>;

impl Apply for RenamePlan {
    type Output = RenameResult;
    fn apply(&mut self, ctx: &Context, progress: Progress<'_>) -> Result<RenameResult> {
        progress(Event::Status(format!("Renaming {}", self.old_name)));
        rename::apply(&mut ctx.library()?, self)
    }
}

/// Rename a managed ROM locally.
pub fn plan_rename(
    ctx: &Context,
    platform: Option<&str>,
    spec: &str,
    new_name: &str,
) -> Result<RenamePlan> {
    rename::plan(&ctx.library()?, platform, spec, new_name)
}

pub fn rename(
    ctx: &Context,
    platform: Option<&str>,
    spec: &str,
    new_name: &str,
    mode: Mode,
) -> Result<RenameReport> {
    let plan = plan_rename(ctx, platform, spec, new_name)?;
    stage(ctx, mode, plan, &mut |_| {})
}

pub type RemoveReport = Staged<RemovePlan>;

impl Apply for RemovePlan {
    type Output = RemoveResult;
    fn apply(&mut self, ctx: &Context, progress: Progress<'_>) -> Result<RemoveResult> {
        progress(Event::Status("Removing from library".into()));
        remove::apply(&mut ctx.library()?, self)
    }
}

pub fn plan_remove(ctx: &Context, platform: Option<&str>, specs: &[String]) -> Result<RemovePlan> {
    remove::plan(&ctx.library()?, platform, specs)
}

pub fn remove(
    ctx: &Context,
    platform: Option<&str>,
    specs: &[String],
    mode: Mode,
) -> Result<RemoveReport> {
    let plan = plan_remove(ctx, platform, specs)?;
    stage(ctx, mode, plan, &mut |_| {})
}

/// Every unverified game that matches a standard name, as a rename plan
/// for the local library.
pub fn plan_standardize(ctx: &Context, platform: Option<&str>) -> Result<Vec<RenamePlan>> {
    library::rename::standardize_plan(&ctx.library()?, platform, |p| names_for(ctx, p))
}

/// One rename per game that matches a standard name, each on its own.
pub type StandardizeReport = Batch<RenamePlan>;

pub fn standardize(ctx: &Context, platform: Option<&str>, mode: Mode) -> Result<StandardizeReport> {
    let renames = plan_standardize(ctx, platform)?
        .into_iter()
        .map(|plan| (plan.old_name.clone(), Ok(plan)));
    Ok(Batch::stage(ctx, mode, renames, &mut |_| {}))
}

pub type CoverImportReport = Staged<covers::import::ImportPlan>;

impl Apply for covers::import::ImportPlan {
    type Output = covers::import::ImportResult;
    fn apply(&mut self, ctx: &Context, _: Progress<'_>) -> Result<Self::Output> {
        covers::import::apply(&mut ctx.library()?, self)
    }
}

/// Copies a local image in as the cover of the game `spec` names.
pub fn import_cover(
    ctx: &Context,
    platform: Option<&str>,
    spec: &str,
    image: &Path,
    mode: Mode,
) -> Result<CoverImportReport> {
    let lib = ctx.library()?;
    let hits = library::resolve_game_roms(&lib, platform, spec)?;
    let plan = covers::import::plan(&lib, hits[0].0.id, image)?;
    stage(ctx, mode, plan, &mut |_| {})
}

impl Outcome for library::identify::Report {
    fn error(&self) -> Option<Error> {
        super::verdict(
            self.matched + self.no_match + self.ambiguous + self.unchanged,
            self.errors.len(),
        )
    }
}

pub fn identify(
    ctx: &Context,
    platforms: Option<&[String]>,
    progress: &mut dyn FnMut(Event),
) -> Result<library::identify::Report> {
    let policy = match ctx.policy() {
        pocketeer_core::net::Policy::MaxAge(_) => {
            pocketeer_core::net::Policy::MaxAge(pocketeer_core::catalog::libretro::MAX_AGE)
        }
        p => p,
    };
    library::identify::run(
        &mut ctx.library()?,
        &ctx.http()?,
        &ctx.cache()?,
        library::identify::Options {
            policy,
            base: ctx.dat_base(),
            platforms,
        },
        progress,
    )
}

pub fn covers_fetch(
    ctx: &Context,
    platforms: Option<&[String]>,
    retry: bool,
    system: Option<&str>,
    progress: &mut dyn FnMut(Event),
) -> Result<FetchReport> {
    let lib = ctx.library()?;
    let http = ctx.http()?;
    let opts = FetchOptions {
        platforms: platforms.map(<[String]>::to_vec),
        retry_missing: retry,
        system: system.map(str::to_owned),
    };
    covers::fetch(&lib, &http, &opts, ctx.thumbnails_base(), progress)
}

#[derive(Debug, Serialize)]
pub struct CoversStatusReport {
    pub platforms: Vec<CoverStats>,
}

pub fn covers_status(ctx: &Context) -> Result<CoversStatusReport> {
    Ok(CoversStatusReport {
        platforms: covers::stats(&ctx.library()?)?,
    })
}
