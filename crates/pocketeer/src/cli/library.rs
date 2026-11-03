//! The library: creating it, importing into it (from a folder; the card's
//! import lives with the card but shares [`imported`]), renaming, removing,
//! DAT checks, and covers.

use std::path::PathBuf;

use clap::{Args, Subcommand};
use pocketeer_core::{
    Result,
    library::{
        import::{ImportPlan, ImportResult},
        remove,
        rename::{self, RenamePlan},
    },
    plan::Progress,
};

use super::{
    ApplyArgs,
    output::{Outcome, Staged, stage, stage_batch, verdict},
};
use crate::{
    ops::{
        Batch,
        context::Context,
        library::{self as ops, ImportRequest, ImportSettings, MetadataReport},
    },
    text::{Text, covers_summary, warning_lines},
};

#[derive(Debug, Subcommand)]
pub enum LibraryCommand {
    /// Identify ROMs by SHA-1 and size using automatically cached DATs.
    Identify {
        /// Only this platform.
        #[arg(long, value_name = "ID")]
        platform: Option<String>,
    },
    /// Create a new library (default: ~/Pocketeer Library, or --library).
    Init,
    /// Show where the library is and what's in it.
    Status,
    /// Import ROMs from a folder into the library (zips are extracted).
    Import(ImportArgs),
    /// List games in the library.
    List {
        /// Only this platform.
        #[arg(long, value_name = "ID")]
        platform: Option<String>,
        /// Case-insensitive substring of the game name.
        query: Option<String>,
    },
    /// Rename a local ROM file and its game title; card copies keep their names.
    Rename {
        /// ROM id, file name or game name (case-insensitive).
        #[arg(value_name = "ROM")]
        rom: String,
        /// New file name; without an extension the old one is kept.
        #[arg(value_name = "NEW_NAME")]
        new_name: String,
        /// Disambiguate when the same name exists on several platforms.
        #[arg(long, value_name = "ID")]
        platform: Option<String>,
        #[command(flatten)]
        apply: ApplyArgs,
    },
    /// Rename library games to the standard (No-Intro) names they match.
    Standardize {
        /// Only this platform.
        #[arg(long, value_name = "ID")]
        platform: Option<String>,
        #[command(flatten)]
        apply: ApplyArgs,
    },
    /// Remove ROMs from the library (files go to .trash/ inside it; cards are untouched).
    Remove {
        /// ROM ids, file names or game names (a game name takes all its ROMs).
        #[arg(value_name = "ROM", required = true)]
        roms: Vec<String>,
        /// Disambiguate when the same name exists on several platforms.
        #[arg(long, value_name = "ID")]
        platform: Option<String>,
        #[command(flatten)]
        apply: ApplyArgs,
    },
}

#[derive(Debug, Args, Clone)]
pub struct ImportArgs {
    /// Folder of ROMs on this computer (scanned recursively), or one ROM / zip.
    pub dir: PathBuf,
    /// Platform id for every file, e.g. "gba". Without it each file's
    /// platform comes from its extension, and files it can't tell are left out.
    #[arg(long, value_name = "ID")]
    pub platform: Option<String>,
    #[command(flatten)]
    pub flags: ImportFlags,
    #[command(flatten)]
    pub apply: ApplyArgs,
}

/// How `library import` and `card import` treat the files they import.
#[derive(Debug, Args, Clone, Default)]
pub struct ImportFlags {
    /// XML/clrmamepro DAT to verify and name the files with (one platform's,
    /// so it needs --platform).
    #[arg(long, value_name = "FILE", requires = "platform")]
    pub dat: Option<PathBuf>,
    /// Skip files the DAT doesn't know (default: import them as unverified).
    #[arg(long, requires = "dat")]
    pub dat_only: bool,
    /// Don't fetch covers for the imported games afterwards.
    #[arg(long)]
    pub no_covers: bool,
    /// Import files under the standard (No-Intro) name they match, when a DAT
    /// didn't name them. Needs the libretro name list (network, cached a week).
    #[arg(long)]
    pub standard_names: bool,
}

impl ImportFlags {
    pub fn settings(&self) -> ImportSettings {
        ImportSettings {
            dat: self.dat.clone(),
            dat_only: self.dat_only,
            standard_names: self.standard_names,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum CoversCommand {
    /// Copy a local PNG/JPEG/WebP into the library as this game's cover.
    Import {
        /// ROM id, filename or game name identifying the game.
        #[arg(value_name = "ROM")]
        rom: String,
        /// Local PNG, JPEG or WebP image to copy; the source is kept.
        #[arg(value_name = "IMAGE")]
        image: PathBuf,
        /// Disambiguate names shared by several platforms.
        #[arg(long, value_name = "ID")]
        platform: Option<String>,
        #[command(flatten)]
        apply: ApplyArgs,
    },
    /// Download missing covers (matched by DAT game name).
    Fetch {
        /// Only this platform.
        #[arg(long, value_name = "ID")]
        platform: Option<String>,
        /// Retry games that had no cover last time.
        #[arg(long)]
        retry: bool,
        /// libretro system name to use, e.g. "Nintendo - Game Boy Advance" (for unmapped platforms).
        #[arg(long, value_name = "NAME", requires = "platform")]
        libretro_system: Option<String>,
    },
    /// How many games have covers, per platform.
    Status,
}

const LIBRETRO_SYSTEM: &str = "Give a platform's libretro system name with `pocketeer covers fetch --platform <id> --libretro-system <name>`.";

pub fn run(ctx: &Context, cmd: LibraryCommand, progress: Progress<'_>) -> Result<Outcome> {
    match cmd {
        LibraryCommand::Identify { platform } => {
            let report = ops::identify(ctx, platform.as_ref().map(std::slice::from_ref), progress)?;
            let checked = report.matched + report.no_match + report.ambiguous + report.unchanged;
            Ok(Outcome::report(&report).failing(verdict(checked, report.errors.len())))
        }
        LibraryCommand::Init => Ok(Outcome::report(&ops::init(ctx)?).hint(
            true,
            "Next: pocketeer library import <folder> --platform <id> [--dat <file>]",
        )),
        LibraryCommand::Status => Ok(Outcome::report(&ctx.library()?.status()?)),
        LibraryCommand::Import(args) => {
            let req = ImportRequest {
                source: args.dir,
                platform: args.platform,
                settings: args.flags.settings(),
            };
            let plan = ops::plan_import(ctx, &req, progress)?;
            let staged = stage(args.apply.into(), plan.changes(), plan, |p| {
                p.apply(&mut ctx.library()?, progress)
            })?;
            Ok(imported(ctx, &staged, &staged.plan, &args.flags, progress))
        }
        LibraryCommand::List { platform, query } => Ok(Outcome::report(&ops::list(
            ctx,
            platform.as_deref(),
            query.as_deref(),
        )?)),
        LibraryCommand::Rename {
            rom,
            new_name,
            platform,
            apply,
        } => {
            let plan = rename::plan(&ctx.library()?, platform.as_deref(), &rom, &new_name)?;
            Ok(stage(apply.into(), plan.changes(), plan, |p| {
                p.apply(&mut ctx.library()?, progress)
            })?
            .outcome())
        }
        LibraryCommand::Remove {
            roms,
            platform,
            apply,
        } => {
            let plan = remove::plan(&ctx.library()?, platform.as_deref(), &roms)?;
            Ok(stage(apply.into(), plan.changes(), plan, |p| {
                p.apply(&mut ctx.library()?, progress)
            })?
            .outcome())
        }
        LibraryCommand::Standardize { platform, apply } => {
            let renames = ops::plan_standardize(ctx, platform.as_deref())?;
            let none = renames.is_empty();
            // Each game is renamed on its own, so one failure stops no other.
            let batch = Batch::plan(renames.into_iter().map(|p| (p.rom.name.clone(), Ok(p))));
            Ok(stage_batch(apply.into(), batch, RenamePlan::changes, |p| {
                p.apply(&mut ctx.library()?, progress)
            })
            .then_text(if none {
                "Every unverified game already has a standard name, or no standard name matches.\n"
            } else {
                ""
            }))
        }
    }
}

pub fn covers(ctx: &Context, cmd: CoversCommand, progress: Progress<'_>) -> Result<Outcome> {
    match cmd {
        CoversCommand::Import {
            rom,
            image,
            platform,
            apply,
        } => {
            let plan = ops::plan_import_cover(ctx, platform.as_deref(), &rom, &image)?;
            Ok(stage(apply.into(), plan.changes(), plan, |p| {
                p.apply(&mut ctx.library()?, progress)
            })?
            .outcome())
        }
        CoversCommand::Fetch {
            platform,
            retry,
            libretro_system,
        } => {
            let report = ops::covers_fetch(
                ctx,
                platform.as_ref().map(std::slice::from_ref),
                retry,
                libretro_system.as_deref(),
                progress,
            )?;
            Ok(Outcome::report(&report)
                .hint(!report.unsupported_platforms.is_empty(), LIBRETRO_SYSTEM))
        }
        CoversCommand::Status => {
            let report = ops::covers_status(ctx)?;
            let unmapped = report.platforms.iter().any(|p| !p.covers_mapped);
            Ok(Outcome::report(&report).hint(unmapped, LIBRETRO_SYSTEM))
        }
    }
}

/// After an import, identify its games and fetch covers, then report both.
/// `plan` is the inner library import for either source; execution stays at the call site.
pub fn imported<P: serde::Serialize + Text>(
    ctx: &Context,
    staged: &Staged<P, ImportResult>,
    plan: &ImportPlan,
    flags: &ImportFlags,
    progress: Progress<'_>,
) -> Outcome {
    let platforms = added_platforms(plan);
    let added = staged.result.as_ref().map_or(0, |r| r.added);
    let metadata = if added > 0 {
        ops::refresh_metadata(ctx, Some(&platforms), !flags.no_covers, progress)
    } else {
        MetadataReport::default()
    };
    let problems: Vec<_> = metadata.problems().cloned().collect();
    let mut more = String::new();
    covers_summary(&mut more, metadata.covers.as_ref());
    warning_lines(&mut more, &problems);
    let suggested = plan.suggestions().count();
    let unmapped = metadata
        .covers
        .as_ref()
        .is_some_and(|c| !c.unsupported_platforms.is_empty());
    staged
        .outcome()
        .then_text(&more)
        .with("covers", &metadata.covers)
        .with("warnings", &problems)
        .hint(
            !plan.unknown.is_empty(),
            "Give the files of unknown type a platform with --platform <id>.",
        )
        .hint(
            staged.result.is_none() && plan.changes() > 0 && suggested > 0,
            format!(
                "{suggested} file(s) have a standard name (→ above); add --standard-names to import them under it."
            ),
        )
        .hint(unmapped, LIBRETRO_SYSTEM)
}

/// The platforms an import plan adds games to.
fn added_platforms(plan: &ImportPlan) -> Vec<String> {
    plan.actions()
        .iter()
        .filter(|a| matches!(a, pocketeer_core::library::import::ImportAction::Add { .. }))
        .map(|a| a.platform().to_owned())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}
