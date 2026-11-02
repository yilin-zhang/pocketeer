//! The local library: creating it, importing, naming, removing, identifying
//! ROMs against DATs, and box art.

use std::path::{Path, PathBuf};

use pocketeer_core::{
    Error, ErrorInfo, Result,
    library::{
        self, Library,
        covers::{self, CoverStats, FetchOptions, FetchReport},
        db::Game,
        import::{self, ImportOptions, ImportPlan, ImportResult},
        remove::{self, RemovePlan, RemoveResult},
        rename::{self, RenamePlan, RenameResult},
    },
    plan::{Applied, Event, Plan},
};
use serde::Serialize;

use super::{Mode, Staged, context::Context, load_dat, stage};
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

#[derive(Debug, Serialize)]
pub struct ImportReport {
    #[serde(flatten)]
    pub staged: Staged<ImportPlan, ImportResult>,
    /// Covers fetched for the imported platform afterwards (online only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub covers: Option<FetchReport>,
}

impl ImportReport {
    /// A plan shown in a TUI dialog before it is applied.
    pub fn preview(plan: ImportPlan) -> Self {
        ImportReport {
            staged: Staged::preview(plan),
            covers: None,
        }
    }
}

impl Outcome for ImportReport {
    fn error(&self) -> Option<Error> {
        self.staged.error()
    }
}

/// What `library import` was asked to do.
#[derive(Debug, Clone)]
pub struct ImportRequest {
    /// A folder of ROMs (scanned recursively) or one ROM / zip.
    pub source: PathBuf,
    pub platform: String,
    pub dat: Option<PathBuf>,
    pub dat_only: bool,
    /// Import files under the standard name they match (needs the network).
    pub standard_names: bool,
    /// Fetch covers for the new games afterwards (when online).
    pub covers: bool,
}

/// Best-effort cover fetch once an import added `added` games: nothing
/// happens when it added none, when `--no-covers` was given or offline, and
/// errors are swallowed (the import already succeeded).
pub fn covers_after_import(
    ctx: &Context,
    lib: &Library,
    platform: Option<&str>,
    (added, enabled): (usize, bool),
    progress: &mut dyn FnMut(Event),
) -> Option<FetchReport> {
    if added > 0 {
        let _ = identify(ctx, platform, progress);
    }
    if added == 0 || !enabled || ctx.offline() {
        return None;
    }
    let http = ctx.http().ok()?;
    covers::fetch(
        lib,
        &http,
        &FetchOptions {
            platform: platform.map(str::to_owned),
            retry_missing: false,
            system: None,
        },
        progress,
    )
    .ok()
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
    platform: &str,
    plan: &mut ImportPlan,
    take: bool,
) -> Result<()> {
    if let Some(names) = names_for(ctx, platform)
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
) -> Result<(ImportOptions, ImportPlan)> {
    let lib = ctx.library()?;
    let dat = load_dat(req.dat.as_deref())?;
    let opts = ImportOptions {
        platform: req.platform.clone(),
        platform_name: ctx.platform_name(&req.platform),
        dat_only: req.dat_only,
    };
    let mut plan = import::plan_import(
        &lib,
        &req.source,
        &opts,
        dat.as_ref(),
        &mut ctx.hash_cache(),
        progress,
    )?;
    suggest_standard_names(ctx, &lib, &req.platform, &mut plan, req.standard_names)?;
    Ok((opts, plan))
}

pub fn apply_import(
    ctx: &Context,
    options: &ImportOptions,
    plan: &ImportPlan,
    progress: &mut dyn FnMut(Event),
) -> Result<ImportResult> {
    import::apply_import(&mut ctx.library()?, options, plan, progress)
}

pub fn import(
    ctx: &Context,
    req: &ImportRequest,
    mode: Mode,
    progress: &mut dyn FnMut(Event),
) -> Result<ImportReport> {
    let (opts, plan) = plan_import(ctx, req, progress)?;
    let staged = stage(mode, plan, |plan| apply_import(ctx, &opts, plan, progress))?;
    let added = staged.result.as_ref().map_or(0, |r| r.added);
    let covers = covers_after_import(
        ctx,
        &ctx.library()?,
        Some(&req.platform),
        (added, req.covers),
        progress,
    );
    Ok(ImportReport { staged, covers })
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

pub type RenameReport = Staged<RenamePlan, RenameResult>;

/// Rename a managed ROM locally.
pub fn plan_rename(
    ctx: &Context,
    platform: Option<&str>,
    spec: &str,
    new_name: &str,
) -> Result<RenamePlan> {
    rename::plan(&ctx.library()?, platform, spec, new_name)
}

pub fn apply_rename(ctx: &Context, plan: &RenamePlan) -> Result<RenameResult> {
    rename::apply(&mut ctx.library()?, plan)
}

pub fn rename(
    ctx: &Context,
    platform: Option<&str>,
    spec: &str,
    new_name: &str,
    mode: Mode,
) -> Result<RenameReport> {
    let plan = plan_rename(ctx, platform, spec, new_name)?;
    stage(mode, plan, |plan| apply_rename(ctx, plan))
}

pub type RemoveReport = Staged<RemovePlan, RemoveResult>;

pub fn plan_remove(ctx: &Context, platform: Option<&str>, specs: &[String]) -> Result<RemovePlan> {
    remove::plan(&ctx.library()?, platform, specs)
}

pub fn apply_remove(ctx: &Context, plan: &RemovePlan) -> Result<RemoveResult> {
    remove::apply(&mut ctx.library()?, plan)
}

pub fn remove(
    ctx: &Context,
    platform: Option<&str>,
    specs: &[String],
    mode: Mode,
) -> Result<RemoveReport> {
    let plan = plan_remove(ctx, platform, specs)?;
    stage(mode, plan, |plan| apply_remove(ctx, plan))
}

/// One rename per game that matches a standard name.
#[derive(Debug, Serialize)]
pub struct StandardizePlan {
    pub renames: Vec<RenamePlan>,
}

impl Plan for StandardizePlan {
    fn changes(&self) -> usize {
        self.renames.len()
    }
}

/// `result` of a standardize: one entry per rename, in plan order.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct StandardizeResults(pub Vec<std::result::Result<RenameResult, ErrorInfo>>);

impl std::ops::Deref for StandardizeResults {
    type Target = [std::result::Result<RenameResult, ErrorInfo>];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Applied for StandardizeResults {
    fn failed(&self) -> usize {
        self.iter().filter(|r| r.is_err()).count()
    }
}

pub type StandardizeReport = Staged<StandardizePlan, StandardizeResults>;

pub fn apply_standardize(
    ctx: &Context,
    renames: &[RenamePlan],
    progress: &mut dyn FnMut(Event),
) -> Result<StandardizeResults> {
    let mut lib = ctx.library()?;
    Ok(StandardizeResults(
        renames
            .iter()
            .map(|plan| {
                progress(Event::Status(format!("Renaming {}", plan.old_name)));
                rename::apply(&mut lib, plan).map_err(|e| e.to_info())
            })
            .collect(),
    ))
}

/// Every unverified game that matches a standard name, as a rename plan
/// for the local library.
pub fn plan_standardize(ctx: &Context, platform: Option<&str>) -> Result<Vec<RenamePlan>> {
    library::rename::standardize_plan(&ctx.library()?, platform, |p| names_for(ctx, p))
}

pub fn standardize(ctx: &Context, platform: Option<&str>, mode: Mode) -> Result<StandardizeReport> {
    let renames = plan_standardize(ctx, platform)?;
    stage(mode, StandardizePlan { renames }, |plan| {
        apply_standardize(ctx, &plan.renames, &mut |_| {})
    })
}

pub type CoverImportReport = Staged<covers::import::ImportPlan, covers::import::ImportResult>;

/// Copies a local image in as the cover of the game `spec` names.
pub fn import_cover(
    ctx: &Context,
    platform: Option<&str>,
    spec: &str,
    image: &Path,
    mode: Mode,
) -> Result<CoverImportReport> {
    let mut lib = ctx.library()?;
    let hits = library::resolve_game_roms(&lib, platform, spec)?;
    let plan = covers::import::plan(&lib, hits[0].0.id, image)?;
    stage(mode, plan, |p| covers::import::apply(&mut lib, p))
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
    let base = std::env::var("POCKETEER_DAT_BASE")
        .unwrap_or_else(|_| pocketeer_core::catalog::libretro::DAT_BASE.into());
    library::identify::run(
        &mut ctx.library()?,
        &ctx.http()?,
        &ctx.cache()?,
        library::identify::Options {
            policy,
            base: &base,
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
    // Undocumented: lets tests (or a mirror) replace the libretro host.
    match std::env::var("POCKETEER_THUMBNAILS_BASE") {
        Ok(base) if !base.is_empty() => {
            covers::fetch_with_base(&lib, &http, &opts, &base, progress)
        }
        _ => covers::fetch(&lib, &http, &opts, progress),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use pocketeer_core::fsutil::hash::HashCache;

    #[test]
    fn standardize_reports_partial_success_and_continues_after_a_collision() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        std::fs::create_dir(&source).unwrap();
        for name in ["one", "two", "three"] {
            std::fs::write(source.join(format!("{name}.gba")), name).unwrap();
        }
        let mut lib = Library::init(&tmp.path().join("library")).unwrap();
        let options = ImportOptions {
            platform: "gba".into(),
            platform_name: None,
            dat_only: false,
        };
        let import = import::plan_import(
            &lib,
            &source,
            &options,
            None,
            &mut HashCache::in_memory(),
            &mut |_| {},
        )
        .unwrap();
        import::apply_import(&mut lib, &options, &import, &mut |_| {}).unwrap();
        let renames: Vec<_> = ["one", "two", "three"]
            .into_iter()
            .map(|name| rename::plan(&lib, Some("gba"), name, &format!("new {name}")).unwrap())
            .collect();
        std::fs::write(lib.root().join("roms/gba/new two.gba"), b"keep").unwrap();
        let cli = crate::cli::Cli::parse_from([
            "pocketeer",
            "--library",
            lib.root().to_str().unwrap(),
            "--config",
            tmp.path().join("config.toml").to_str().unwrap(),
        ]);
        let result = apply_standardize(&Context::new(cli.global), &renames, &mut |_| {}).unwrap();
        assert!(result[0].is_ok());
        assert!(result[1].is_err());
        assert!(result[2].is_ok());
        assert_eq!(result.failed(), 1);
        assert_eq!(
            std::fs::read(lib.root().join("roms/gba/new one.gba")).unwrap(),
            b"one"
        );
        assert_eq!(
            std::fs::read(lib.root().join("roms/gba/two.gba")).unwrap(),
            b"two"
        );
        assert_eq!(
            std::fs::read(lib.root().join("roms/gba/new two.gba")).unwrap(),
            b"keep"
        );
        assert_eq!(
            std::fs::read(lib.root().join("roms/gba/new three.gba")).unwrap(),
            b"three"
        );
    }
}
