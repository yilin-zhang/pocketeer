//! The card: its status, macOS junk, BIOS files, and ROMs copied to it,
//! renamed on it or imported from it.

use std::path::PathBuf;

use clap::{Args, Subcommand};
use pocketeer_core::{
    Error, Result,
    card::{
        bios::{FileStatus, RootFile},
        clean,
        roms::{Depth, Identity},
    },
    plan::Progress,
    transfer::{rename, to_card},
};

use super::{
    ApplyArgs,
    library::{ImportFlags, imported},
    output::{Mode, Outcome, stage},
};
use crate::ops::{
    card::{self as ops, ImportRequest, RomsReport},
    context::Context,
};

#[derive(Debug, Subcommand)]
pub enum CardCommand {
    /// Rename one exact card ROM path and its mirrored save files.
    Rename {
        /// Exact ROM path relative to the card root, including nested folders.
        path: PathBuf,
        /// New filename; omit the extension to keep it.
        new_name: String,
        #[command(flatten)]
        apply: ApplyArgs,
    },
    /// Copy selected local ROMs to this card; show the plan first with --dry-run.
    Copy {
        /// Library ROM IDs, filenames or game names.
        #[arg(required = true, value_name = "ROM")]
        roms: Vec<String>,
        /// Disambiguate names shared by several platforms.
        #[arg(long)]
        platform: Option<String>,
        #[command(flatten)]
        apply: ApplyArgs,
    },
    /// Copy selected card ROM paths into the library (default: all unmatched files).
    Import(CardImportArgs),
    /// List ROMs using cached hashes, without reading ROM contents.
    List,
    /// Update unknown/changed ROM hashes and sample-check cached files; waits until finished.
    Check {
        /// Read every byte, even when metadata and samples match.
        #[arg(long)]
        full: bool,
    },
}

#[derive(Debug, Args, Clone, Default)]
pub struct CardImportArgs {
    /// Only this platform.
    #[arg(long, value_name = "ID")]
    pub platform: Option<String>,
    /// Card-relative ROM paths (default: every unmatched file).
    #[arg(value_name = "PATH")]
    pub paths: Vec<PathBuf>,
    #[command(flatten)]
    pub flags: ImportFlags,
    #[command(flatten)]
    pub apply: ApplyArgs,
}

#[derive(Debug, Subcommand)]
pub enum BiosCommand {
    /// Show BIOS and data files cores need, and which are missing.
    Check {
        /// Only these cores (default: all installed).
        #[arg(value_name = "ID")]
        ids: Vec<String>,
        /// Also list files that are present.
        #[arg(long)]
        present: bool,
    },
    /// Move files dropped at the SD card root into the folders cores expect.
    Place {
        #[command(flatten)]
        apply: ApplyArgs,
    },
}

pub fn status(ctx: &Context) -> Result<Outcome> {
    let status = pocketeer_core::card::status(&ctx.pocket()?)?;
    Ok(Outcome::report(&status).hint(
        status.broken_cores > 0,
        "See `pocketeer cores list` for the broken cores.",
    ))
}

pub fn clean(ctx: &Context, system: bool, mode: Mode, progress: Progress<'_>) -> Result<Outcome> {
    let plan = clean::plan(&ctx.pocket()?, system)?;
    let staged = stage(mode, plan.changes(), plan, |p| {
        p.apply(&ctx.pocket()?, progress)
    })?;
    Ok(staged.outcome().hint(
        !staged.plan.system_dirs_present.is_empty(),
        "Use --system to remove the macOS system folders too.",
    ))
}

pub fn bios(ctx: &Context, cmd: BiosCommand, progress: Progress<'_>) -> Result<Outcome> {
    match cmd {
        BiosCommand::Check { ids, present } => {
            let report = ops::bios_check(ctx, &ids, present)?;
            let at_root = report.all_files().any(|f| {
                matches!(
                    f.status,
                    FileStatus::AtRoot {
                        found: RootFile::File { .. }
                    }
                )
            });
            Ok(Outcome::report(&report).hint(
                at_root,
                "Move the files found at the card root into place with `pocketeer bios place`.",
            ))
        }
        BiosCommand::Place { apply } => {
            let plan = ops::plan_place(ctx)?;
            Ok(stage(apply.into(), plan.changes(), plan, |p| {
                p.apply(&ctx.pocket()?, progress)
            })?
            .outcome())
        }
    }
}

pub fn run(ctx: &Context, cmd: CardCommand, progress: Progress<'_>) -> Result<Outcome> {
    match cmd {
        CardCommand::Rename {
            path,
            new_name,
            apply,
        } => {
            let plan = rename::plan(&ctx.pocket()?, &path, &new_name)?;
            Ok(stage(apply.into(), plan.changes(), plan, |p| {
                p.apply(&ctx.pocket()?, ctx.library().ok().as_ref(), progress)
            })?
            .outcome())
        }
        CardCommand::Copy {
            roms,
            platform,
            apply,
        } => {
            let plan = to_card::plan(
                &ctx.library()?,
                &ctx.pocket()?,
                platform.as_deref(),
                &roms,
                progress,
            )?;
            Ok(stage(apply.into(), plan.changes(), plan, |p| {
                p.apply(&ctx.pocket()?, progress)
            })?
            .outcome())
        }
        CardCommand::Import(args) => {
            let req = ImportRequest {
                platform: args.platform,
                paths: args.paths,
                settings: args.flags.settings(),
            };
            let plan = ops::plan_import(ctx, &req, progress)?;
            let errors = plan.errors();
            let staged = stage(args.apply.into(), plan.changes(), plan, |p| {
                p.apply(&mut ctx.library()?, &ctx.pocket()?, progress)
            })?
            .planning_errors(errors);
            Ok(imported(
                ctx,
                &staged,
                &staged.plan.plan,
                &args.flags,
                progress,
            ))
        }
        CardCommand::List => roms(ctx, Depth::Cached, progress),
        CardCommand::Check { full } => {
            let depth = if full { Depth::Full } else { Depth::Sample };
            roms(ctx, depth, progress)
        }
    }
}

/// The card's ROMs; a file that couldn't be read fails the command.
fn roms(ctx: &Context, depth: Depth, progress: Progress<'_>) -> Result<Outcome> {
    let report: RomsReport = ops::roms(ctx, depth, progress)?;
    let failed = report
        .roms
        .iter()
        .filter(|r| matches!(r.identity, Identity::Error { .. }))
        .count();
    Ok(
        Outcome::report(&report).failing((failed > 0).then_some(Error::SomeFailed {
            failed,
            total: report.roms.len(),
        })),
    )
}
