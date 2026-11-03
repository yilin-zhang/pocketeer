//! The card: its status, macOS junk, BIOS files, and ROMs copied to it,
//! renamed on it or imported from it.

use std::path::PathBuf;

use clap::{Args, Subcommand};
use pocketeer_core::{
    Error, Result,
    card::roms::{Depth, Identity},
    plan::Progress,
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
    let status = ops::status(ctx)?;
    Ok(Outcome::report(&status).hint(
        status.broken_cores > 0,
        "See `pocketeer cores list` for the broken cores.",
    ))
}

pub fn clean(ctx: &Context, system: bool, mode: Mode, progress: Progress<'_>) -> Result<Outcome> {
    let staged = stage(ctx, mode, ops::plan_clean(ctx, system)?, progress)?;
    Ok(staged.outcome().hint(
        !staged.plan.system_dirs_present.is_empty(),
        "Use --system to remove the macOS system folders too.",
    ))
}

pub fn bios(ctx: &Context, cmd: BiosCommand, progress: Progress<'_>) -> Result<Outcome> {
    match cmd {
        BiosCommand::Check { ids, present } => {
            Ok(Outcome::report(&ops::bios_check(ctx, &ids, present)?))
        }
        BiosCommand::Place { apply } => {
            Ok(stage(ctx, apply.into(), ops::plan_place(ctx)?, progress)?.outcome())
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
            let plan = ops::plan_rename(ctx, &path, &new_name)?;
            Ok(stage(ctx, apply.into(), plan, progress)?.outcome())
        }
        CardCommand::Copy {
            roms,
            platform,
            apply,
        } => {
            let plan = ops::plan_copy(ctx, platform.as_deref(), &roms, progress)?;
            Ok(stage(ctx, apply.into(), plan, progress)?.outcome())
        }
        CardCommand::Import(args) => {
            let req = ImportRequest {
                platform: args.platform,
                paths: args.paths,
                settings: args.flags.settings(),
            };
            let plan = ops::plan_import(ctx, &req, progress)?;
            imported(
                ctx,
                args.apply.into(),
                plan,
                |p| &p.plan,
                &args.flags,
                progress,
            )
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
