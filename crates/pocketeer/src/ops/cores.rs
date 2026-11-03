//! Cores: listing, searching, installing, updating and removing.

use std::path::PathBuf;

use pocketeer_core::{
    Result,
    card::cores::{self, CoreInfo, CoreListReport, CoreView, SearchFilter},
    card::install::{self, Installer, PlanOptions, Prepared, Targets, UninstallPlan},
    catalog::inventory::{Inventory, InventoryStatus},
    plan::Event,
};
use serde::Serialize;

use super::{Batch, context::Context};

pub fn list(ctx: &Context, outdated: bool) -> Result<CoreListReport> {
    let pocket = ctx.pocket()?;
    let inventory = ctx.inventory_lenient();
    let mut report = cores::report(&pocket, &inventory)?;
    if outdated {
        report.cores.retain(|c| c.update_available);
    }
    Ok(report)
}

#[derive(Debug, Serialize)]
pub struct SearchReport {
    pub hits: Vec<CoreView>,
    pub inventory: InventoryStatus,
}

pub fn search(
    ctx: &Context,
    query: Option<&str>,
    platform: Option<&str>,
    include_installed: bool,
) -> Result<SearchReport> {
    // The SD card is optional here: without one, nothing is "installed".
    let installed = match ctx.pocket() {
        Ok(pocket) => cores::list_installed(&pocket)?,
        Err(_) => Default::default(),
    };
    let inventory = ctx.inventory()?;
    let filter = SearchFilter {
        query,
        platform,
        include_installed,
        include_unreleased: false,
    };
    let hits = cores::search(&inventory, &installed, &filter);
    Ok(SearchReport {
        hits,
        inventory: inventory.status(),
    })
}

pub fn info(ctx: &Context, id: &str) -> Result<CoreInfo> {
    let pocket = ctx.pocket()?;
    let inventory = ctx.inventory_lenient();
    cores::info(&pocket, &inventory, id)
}

/// What `cores install` was asked to do.
#[derive(Debug, Clone, Default)]
pub struct InstallRequest {
    /// `id` or `id@version`.
    pub specs: Vec<String>,
    /// A release zip on disk instead of a download.
    pub from: Option<PathBuf>,
    pub force: bool,
    pub keep_platforms: bool,
}

/// Resolves `specs` (`id` or `id@version`) against the inventory and
/// downloads what each install needs. One entry per spec, failures included.
pub fn prepare(
    ctx: &Context,
    inventory: &Inventory,
    specs: &[String],
    opts: PlanOptions,
    progress: &mut dyn FnMut(Event),
) -> Result<Vec<(String, Result<Prepared>)>> {
    let pocket = ctx.pocket()?;
    let net = ctx.net()?;
    let installed = cores::list_installed(&pocket)?;
    Ok(Installer::new(&pocket, &net).prepare_specs(inventory, &installed, specs, &opts, progress))
}

/// Plans `cores install`: each spec resolved and downloaded, or the zip
/// given with `--from`.
pub fn plan_install(
    ctx: &Context,
    req: &InstallRequest,
    progress: &mut dyn FnMut(Event),
) -> Result<Batch<Prepared, install::Changes>> {
    let pocket = ctx.pocket()?;
    let opts = PlanOptions {
        force: req.force,
        keep_platforms: req.keep_platforms,
        ..Default::default()
    };
    let prepared = match &req.from {
        Some(file) => {
            let net = ctx.net()?;
            let installed = cores::list_installed(&pocket)?;
            progress(Event::Status(format!("Planning {}", file.display())));
            let result =
                Installer::new(&pocket, &net).prepare_file(file, &installed, opts, progress);
            vec![(file.display().to_string(), result)]
        }
        None => prepare(ctx, &ctx.inventory()?, &req.specs, opts, progress)?,
    };
    Ok(Batch::plan(prepared))
}

/// Plans `cores update` for `targets`.
pub fn plan_update(
    ctx: &Context,
    targets: &Targets,
    keep_platforms: bool,
    progress: &mut dyn FnMut(Event),
) -> Result<Batch<Prepared, install::Changes>> {
    let pocket = ctx.pocket()?;
    let net = ctx.net()?;
    let installed = cores::list_installed(&pocket)?;
    let opts = PlanOptions {
        keep_platforms,
        ..Default::default()
    };
    Ok(Batch::plan(Installer::new(&pocket, &net).prepare_updates(
        &ctx.inventory()?,
        &installed,
        targets,
        &opts,
        progress,
    )))
}

/// Plans removing each of `ids`; `purge` takes its assets and saves too.
pub fn plan_remove(
    ctx: &Context,
    ids: &[String],
    purge: bool,
) -> Result<Batch<UninstallPlan, install::Changes>> {
    let pocket = ctx.pocket()?;
    let installed = cores::list_installed(&pocket)?;
    Ok(Batch::plan(ids.iter().map(|id| {
        let plan = install::plan_remove(&pocket, &installed, id, purge);
        // Named as installed, whatever case was typed.
        let target = plan.as_ref().map_or(id.clone(), |p| p.core_id.clone());
        (target, plan)
    })))
}
