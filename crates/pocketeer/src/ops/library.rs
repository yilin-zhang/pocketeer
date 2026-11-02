//! The local library: creating it, importing, naming, removing, identifying
//! ROMs against DATs, and box art.

use std::path::{Path, PathBuf};

use pocketeer_core::{
    Error, Result,
    library::{
        self, Game, Library,
        covers::{self, CoverStats, FetchOptions, FetchReport},
        import::{self, ImportOptions, ImportPlan, ImportResult},
        remove::{self, RemovePlan, RemoveResult},
        rename::{self, RenamePlan, RenameResult},
    },
    plan::{Event, Progress},
};
use serde::Serialize;

use super::{Apply, Batch, Mode, Staged, context::Context, load_dat, stage};
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

#[derive(Serialize)]
pub struct ImportReport {
    #[serde(flatten)]
    pub staged: Staged<ImportPlan>,
    /// Covers fetched for the imported platform afterwards (online only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub covers: Option<FetchReport>,
    /// Problems identifying or fetching covers for the new games; the
    /// import itself succeeded.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl Outcome for ImportReport {
    fn error(&self) -> Option<Error> {
        self.staged.error()
    }
}

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

/// What `library import --platform` was asked to do.
#[derive(Debug, Clone)]
pub struct ImportRequest {
    /// A folder of ROMs (scanned recursively) or one ROM / zip.
    pub source: PathBuf,
    pub platform: String,
    pub settings: ImportSettings,
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

/// Identifies `platform`'s ROMs (all platforms when `None`) against DATs,
/// then fetches missing covers if `covers` is set and we're online.
pub fn refresh_metadata(
    ctx: &Context,
    platform: Option<&str>,
    covers: bool,
    progress: &mut dyn FnMut(Event),
) -> MetadataReport {
    let mut report = MetadataReport::default();
    match identify(ctx, platform, progress) {
        Ok(r) => report.identify = Some(r),
        Err(e) => report.errors.push(format!("DAT check: {e}")),
    }
    if covers && !ctx.offline() {
        match covers_fetch(ctx, platform, false, None, progress) {
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
    platform: Option<&str>,
    (added, covers): (usize, bool),
    progress: &mut dyn FnMut(Event),
) -> Option<MetadataReport> {
    (added > 0).then(|| refresh_metadata(ctx, platform, covers, progress))
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
    if let Some(names) = names_for(ctx, &plan.platform)
        && import::suggest_names(plan, &names) > 0
        && take
    {
        import::apply_suggestions(lib, plan, |_| true)?;
    }
    Ok(())
}

/// Plans an import: what would be added, skipped, and (when online) which
/// files have a standard name to suggest.
pub fn plan_import(
    ctx: &Context,
    req: &ImportRequest,
    progress: &mut dyn FnMut(Event),
) -> Result<ImportPlan> {
    let lib = ctx.library()?;
    let dat = load_dat(req.settings.dat.as_deref())?;
    let opts = ImportOptions {
        platform: req.platform.clone(),
        platform_name: ctx.platform_name(&req.platform),
        dat_only: req.settings.dat_only,
    };
    let mut plan = import::plan_import(
        &lib,
        &req.source,
        &opts,
        dat.as_ref(),
        &mut ctx.hash_cache(),
        progress,
    )?;
    suggest_standard_names(ctx, &lib, &mut plan, req.settings.standard_names)?;
    Ok(plan)
}

/// What a folder holds, sorted by platform: one import plan per platform
/// the file extensions name, and the files whose platform they don't.
pub struct SortedImport {
    pub parts: Vec<ImportPlan>,
    /// Files no extension rule recognised, relative to the folder.
    pub unknown: Vec<String>,
}

/// Plans importing `source` with each file's platform taken from its
/// extension (`known`: platform ids the library or card already use).
/// Files it can't place go to `fallback` when given, else into `unknown`.
pub fn plan_import_sorted(
    ctx: &Context,
    source: &Path,
    known: &[String],
    fallback: Option<&str>,
    standard_names: bool,
    progress: &mut dyn FnMut(Event),
) -> Result<SortedImport> {
    let lib = ctx.library()?;
    let (dir, candidates) = pocketeer_core::library::import::scan_source(source)?;
    let mut groups: std::collections::BTreeMap<String, Vec<_>> = Default::default();
    let mut unknown = Vec::new();
    for c in candidates {
        let platform = pocketeer_core::catalog::platform_of(&c.file.display_name(), known)
            .or_else(|| fallback.map(str::to_owned));
        match platform {
            Some(p) => groups.entry(p).or_default().push(c),
            None => unknown.push(c.file.location()),
        }
    }
    let mut parts = Vec::new();
    for (platform, files) in groups {
        let opts = ImportOptions {
            platform_name: ctx.platform_name(&platform),
            platform: platform.clone(),
            dat_only: false,
        };
        let mut plan = import::plan_import_candidates(
            &lib,
            &dir,
            &files,
            &opts,
            None,
            &mut ctx.hash_cache(),
            progress,
        )?;
        suggest_standard_names(ctx, &lib, &mut plan, standard_names)?;
        parts.push(plan);
    }
    Ok(SortedImport { parts, unknown })
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
    progress: &mut dyn FnMut(Event),
) -> Result<ImportReport> {
    let plan = plan_import(ctx, req, progress)?;
    let staged = stage(ctx, mode, plan, progress)?;
    let added = staged.result.as_ref().map_or(0, |r| r.added);
    let metadata = metadata_after_import(
        ctx,
        Some(&req.platform),
        (added, req.settings.covers),
        progress,
    )
    .unwrap_or_default();
    Ok(ImportReport {
        staged,
        warnings: metadata.problems().map(str::to_owned).collect(),
        covers: metadata.covers,
    })
}

/// `library import` without `--platform`: one report per platform the
/// file extensions name, and the files they don't.
#[derive(Serialize)]
pub struct SortedImportReport {
    pub parts: Vec<ImportReport>,
    /// Files whose platform couldn't be told; give `--platform` for them.
    pub unknown: Vec<String>,
}

impl Outcome for SortedImportReport {
    fn error(&self) -> Option<Error> {
        self.parts.iter().find_map(|p| p.error())
    }
}

/// `library import` without `--platform`: each file's platform comes from
/// its extension.
pub fn import_sorted(
    ctx: &Context,
    source: &Path,
    standard_names: bool,
    covers: bool,
    mode: Mode,
    progress: &mut dyn FnMut(Event),
) -> Result<SortedImportReport> {
    let known: Vec<String> = ctx
        .library()?
        .platform_stats()?
        .into_iter()
        .map(|p| p.platform)
        .collect();
    let sorted = plan_import_sorted(ctx, source, &known, None, standard_names, progress)?;
    let mut parts = Vec::new();
    for plan in sorted.parts {
        let platform = plan.platform.clone();
        let staged = stage(ctx, mode, plan, progress)?;
        let added = staged.result.as_ref().map_or(0, |r| r.added);
        let metadata = metadata_after_import(ctx, Some(&platform), (added, covers), progress)
            .unwrap_or_default();
        parts.push(ImportReport {
            staged,
            warnings: metadata.problems().map(str::to_owned).collect(),
            covers: metadata.covers,
        });
    }
    Ok(SortedImportReport {
        parts,
        unknown: sorted.unknown,
    })
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
    platform: Option<&str>,
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
            platform,
        },
        progress,
    )
}

pub fn covers_fetch(
    ctx: &Context,
    platform: Option<&str>,
    retry: bool,
    system: Option<&str>,
    progress: &mut dyn FnMut(Event),
) -> Result<FetchReport> {
    let lib = ctx.library()?;
    let http = ctx.http()?;
    let opts = FetchOptions {
        platform: platform.map(str::to_owned),
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
