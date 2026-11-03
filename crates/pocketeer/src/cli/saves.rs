//! Saves and Memories: backing them up from the card, putting them back,
//! their history, and throwing copies away.

use std::path::PathBuf;

use clap::Subcommand;
use pocketeer_core::{
    Result,
    plan::Progress,
    transfer::saves::{ConflictChoice, Scope, TrashTarget},
};

use super::{
    ApplyArgs,
    output::{Outcome, stage},
};
use crate::ops::{context::Context, memories, saves};

#[derive(Debug, Subcommand)]
pub enum SavesCommand {
    /// Back up AP saves. Newer computer copies and content conflicts need a choice.
    Backup {
        /// Leave Memories save states alone.
        #[arg(long)]
        no_states: bool,
        /// Explicitly resolve every conflict; omitted means leave conflicts unchanged.
        #[arg(long, value_enum)]
        conflicts: Option<SaveConflictArg>,
        #[command(flatten)]
        apply: ApplyArgs,
    },
    /// Save files the library holds.
    List,
    /// Older copies of one save file.
    Versions {
        /// Path as on the card, e.g. "Saves/gba/common/Game.sav".
        #[arg(value_name = "PATH")]
        path: PathBuf,
    },
    /// Restore a computer backup to AP, archiving AP's current bytes first.
    #[command(disable_version_flag = true)]
    Restore {
        /// Card-relative save path.
        path: PathBuf,
        /// Use this historical version instead of the current backup.
        #[arg(long)]
        version: Option<i64>,
        #[command(flatten)]
        apply: ApplyArgs,
    },
    /// Move the library's copies of a save to its .trash/ folder, or delete
    /// it from AP (archived into the library first).
    #[command(disable_version_flag = true)]
    Trash {
        /// Card-relative save path.
        path: PathBuf,
        /// Delete the save from AP instead.
        #[arg(long, conflicts_with_all = ["version", "backup"])]
        card: bool,
        /// Only this older version.
        #[arg(long, conflicts_with = "backup")]
        version: Option<i64>,
        /// Only the current backup; older versions stay.
        #[arg(long)]
        backup: bool,
        #[command(flatten)]
        apply: ApplyArgs,
    },
}

#[derive(Debug, Subcommand)]
pub enum MemoriesCommand {
    /// Memories on AP and in the library.
    List,
    /// Back up new and changed Memories from AP.
    Backup {
        /// Explicitly resolve every conflict; omitted means leave conflicts unchanged.
        #[arg(long, value_enum)]
        conflicts: Option<SaveConflictArg>,
        #[command(flatten)]
        apply: ApplyArgs,
    },
    /// Put the library's copy of a Memory back on AP.
    Restore {
        /// Card-relative path of the .sta file.
        path: PathBuf,
        #[command(flatten)]
        apply: ApplyArgs,
    },
    /// Move a Memory to the library's .trash/, or delete it from AP
    /// (archived into the library first).
    Trash {
        /// Card-relative path of the .sta file.
        path: PathBuf,
        /// Delete it from AP instead.
        #[arg(long)]
        card: bool,
        #[command(flatten)]
        apply: ApplyArgs,
    },
}

/// Which copy wins every conflict.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum SaveConflictArg {
    Ap,
    Computer,
    Skip,
}

impl SaveConflictArg {
    fn choice(self) -> ConflictChoice {
        match self {
            SaveConflictArg::Ap => ConflictChoice::Card,
            SaveConflictArg::Computer => ConflictChoice::Computer,
            SaveConflictArg::Skip => ConflictChoice::Skip,
        }
    }
}

pub fn run(ctx: &Context, cmd: SavesCommand, progress: Progress<'_>) -> Result<Outcome> {
    match cmd {
        SavesCommand::Backup {
            no_states,
            conflicts,
            apply,
        } => {
            let scope = if no_states { Scope::Saves } else { Scope::All };
            let plan =
                saves::plan_backup(ctx, scope, conflicts.map(SaveConflictArg::choice), progress)?;
            let errors = plan.errors();
            Ok(stage(apply.into(), plan.changes(), plan, |p| {
                p.apply(&mut ctx.library()?, &ctx.pocket()?, progress)
            })?
            .planning_errors(errors)
            .outcome())
        }
        SavesCommand::List => {
            let report = saves::list(ctx)?;
            Ok(Outcome::report(&report).hint(
                report.saves.is_empty(),
                "Run `pocketeer saves backup` with a card mounted.",
            ))
        }
        SavesCommand::Versions { path } => {
            let report = saves::versions(ctx, &path)?;
            Ok(Outcome::report(&report).hint(
                !report.versions.is_empty(),
                "Restore one with: pocketeer saves restore <PATH> --version <ID>",
            ))
        }
        SavesCommand::Restore {
            path,
            version,
            apply,
        } => {
            let plan = pocketeer_core::transfer::saves::plan_restore(
                &ctx.library()?,
                &ctx.pocket()?,
                &path,
                version,
            )?;
            let errors = plan.errors();
            Ok(stage(apply.into(), plan.changes(), plan, |p| {
                p.apply(&mut ctx.library()?, &ctx.pocket()?, progress)
            })?
            .planning_errors(errors)
            .outcome())
        }
        SavesCommand::Trash {
            path,
            card,
            version,
            backup,
            apply,
        } => {
            let target = match (card, version, backup) {
                (true, ..) => TrashTarget::Card,
                (_, Some(id), _) => TrashTarget::Version(id),
                (_, None, true) => TrashTarget::Backup,
                _ => TrashTarget::Library,
            };
            let plan = saves::plan_trash(ctx, &[path], target)?;
            Ok(stage(apply.into(), plan.changes(), plan, |p| {
                let card = if p.target == TrashTarget::Card {
                    Some(ctx.pocket()?)
                } else {
                    None
                };
                p.apply(&mut ctx.library()?, card.as_ref(), progress)
            })?
            .outcome())
        }
    }
}

pub fn memories(ctx: &Context, cmd: MemoriesCommand, progress: Progress<'_>) -> Result<Outcome> {
    match cmd {
        MemoriesCommand::List => Ok(Outcome::report(&memories::list(ctx)?)),
        MemoriesCommand::Backup { conflicts, apply } => {
            let choice = conflicts.map(SaveConflictArg::choice);
            let plan = saves::plan_backup(ctx, Scope::Memories, choice, progress)?;
            let errors = plan.errors();
            Ok(stage(apply.into(), plan.changes(), plan, |p| {
                p.apply(&mut ctx.library()?, &ctx.pocket()?, progress)
            })?
            .planning_errors(errors)
            .outcome())
        }
        MemoriesCommand::Restore { path, apply } => {
            let plan = pocketeer_core::transfer::memories::plan_restore(
                &ctx.library()?,
                &ctx.pocket()?,
                &path,
            )?;
            let errors = plan.errors();
            Ok(stage(apply.into(), plan.changes(), plan, |p| {
                p.apply(&mut ctx.library()?, &ctx.pocket()?, progress)
            })?
            .planning_errors(errors)
            .outcome())
        }
        MemoriesCommand::Trash { path, card, apply } => {
            let target = if card {
                TrashTarget::Card
            } else {
                TrashTarget::Library
            };
            let plan = memories::plan_trash(ctx, &path, target)?;
            Ok(stage(apply.into(), plan.changes(), plan, |p| {
                let card = if p.target == TrashTarget::Card {
                    Some(ctx.pocket()?)
                } else {
                    None
                };
                p.apply(&mut ctx.library()?, card.as_ref(), progress)
            })?
            .outcome())
        }
    }
}
