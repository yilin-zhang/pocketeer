//! Cores: what is installed, what the inventory has, installing, updating
//! and removing them.

use std::path::PathBuf;

use clap::{Args, Subcommand};
use pocketeer_core::{
    Result,
    card::install::{Prepared, Targets},
    catalog::inventory::InventoryStatus,
    plan::{Plan, Progress},
};

use super::{
    ApplyArgs,
    output::{Outcome, stage_batch},
};
use crate::ops::{self, Batch, context::Context, cores::InstallRequest};

#[derive(Debug, Subcommand)]
pub enum CoresCommand {
    /// List installed cores, with update status when the inventory is available.
    List {
        /// Only cores with an update available.
        #[arg(long)]
        outdated: bool,
    },
    /// Search the inventory for cores to install.
    Search {
        /// Case-insensitive substring of the core id (e.g. "gba", "jotego").
        query: Option<String>,
        /// Only cores for this platform id (e.g. "gba", "snes").
        #[arg(long, value_name = "ID")]
        platform: Option<String>,
        /// Include cores that are already installed.
        #[arg(long)]
        installed: bool,
    },
    /// Show everything known about one core.
    Info {
        /// Core id, `<author>.<shortname>` (case-insensitive).
        id: String,
    },
    /// Install cores from the inventory (or a local zip).
    Install(InstallArgs),
    /// Update installed cores to the latest inventory release.
    Update {
        /// Core ids to update. Omit with --all to update everything outdated.
        #[arg(value_name = "ID", required_unless_present = "all")]
        ids: Vec<String>,
        /// Update every installed core that has an update.
        #[arg(long, conflicts_with = "ids")]
        all: bool,
        /// Don't overwrite existing Platforms/ files (keeps your customisations).
        #[arg(long)]
        keep_platforms: bool,
        #[command(flatten)]
        apply: ApplyArgs,
    },
    /// Remove installed cores (Cores/, Presets/, Settings/ are deleted).
    Remove {
        /// Core ids to remove.
        #[arg(value_name = "ID", required = true)]
        ids: Vec<String>,
        /// Also remove the core's Assets/ and Saves/ folders.
        #[arg(long)]
        purge: bool,
        #[command(flatten)]
        apply: ApplyArgs,
    },
}

#[derive(Debug, Args, Clone)]
pub struct InstallArgs {
    /// Core ids, optionally pinned: `jotego.jtcps1` or `jotego.jtcps1@1.2.0`.
    #[arg(value_name = "ID[@VERSION]", required_unless_present = "from")]
    pub specs: Vec<String>,
    /// Install from a release zip on disk instead of downloading.
    #[arg(long, value_name = "FILE", conflicts_with = "specs")]
    pub from: Option<PathBuf>,
    /// Reinstall even if this version is already installed.
    #[arg(long)]
    pub force: bool,
    /// Don't overwrite existing Platforms/ files (keeps your customisations).
    #[arg(long)]
    pub keep_platforms: bool,
    #[command(flatten)]
    pub apply: ApplyArgs,
}

#[derive(Debug, Subcommand)]
pub enum InventoryCommand {
    /// Fetch the latest inventory now and report its size.
    Refresh,
}

pub fn run(ctx: &Context, cmd: CoresCommand, progress: Progress<'_>) -> Result<Outcome> {
    match cmd {
        CoresCommand::List { outdated } => {
            let report = ops::cores::list(ctx, outdated)?;
            Ok(Outcome::report(&report).remedy(inventory_remedy(&report.inventory)))
        }
        CoresCommand::Search {
            query,
            platform,
            installed,
        } => Ok(Outcome::report(&ops::cores::search(
            ctx,
            query.as_deref(),
            platform.as_deref(),
            installed,
        )?)),
        CoresCommand::Info { id } => {
            let info = ops::cores::info(ctx, &id)?;
            Ok(Outcome::report(&info).remedy(inventory_remedy(&info.inventory_status)))
        }
        CoresCommand::Install(args) => {
            let req = InstallRequest {
                specs: args.specs,
                from: args.from,
                force: args.force,
                keep_platforms: args.keep_platforms,
            };
            let batch = ops::cores::plan_install(ctx, &req, progress)?;
            Ok(installs(ctx, args.apply.into(), batch, progress))
        }
        CoresCommand::Update {
            ids,
            all,
            keep_platforms,
            apply,
        } => {
            let targets = if all {
                Targets::Outdated
            } else {
                Targets::Ids(ids)
            };
            let batch = ops::cores::plan_update(ctx, &targets, keep_platforms, progress)?;
            Ok(installs(ctx, apply.into(), batch, progress))
        }
        CoresCommand::Remove { ids, purge, apply } => {
            let batch = ops::cores::plan_remove(ctx, &ids, purge)?;
            Ok(stage_batch(ctx, apply.into(), batch, progress))
        }
    }
}

pub fn inventory_refresh(ctx: &Context) -> Result<Outcome> {
    Ok(Outcome::report(&ops::card::inventory_refresh(ctx)?))
}

/// Installs or updates, with how to reinstall a core already at its version.
fn installs(
    ctx: &Context,
    mode: super::output::Mode,
    batch: Batch<Prepared>,
    progress: Progress<'_>,
) -> Outcome {
    let current = batch.plans().any(|p| p.changes() == 0);
    stage_batch(ctx, mode, batch, progress).hint(
        current,
        "Use --force to reinstall a core that is already at this version.",
    )
}

/// Without the inventory, the way to get it.
fn inventory_remedy(status: &InventoryStatus) -> Option<pocketeer_core::Remedy> {
    match status {
        InventoryStatus::Unavailable { error } => error.remedy,
        InventoryStatus::Loaded { .. } => None,
    }
}
