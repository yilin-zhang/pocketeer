//! Cores: listing, searching, installing, updating and removing.

use std::path::PathBuf;

use pocketeer_core::{
    Error, Result,
    card::cores::{self, CoreInfo, CoreListReport, CoreView, SearchFilter},
    card::install::{self, Installer, PlanOptions, Prepared, UninstallPlan},
    catalog::inventory::{Inventory, InventoryStatus},
    plan::Event,
};
use serde::Serialize;

use super::{Batch, Mode, context::Context};

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

pub type InstallReport = Batch<Prepared>;

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
    let installer = Installer::new(&pocket, &net);
    let installed = cores::list_installed(&pocket)?;
    let mut out = Vec::new();
    for spec in specs {
        progress(Event::Status(format!("Resolving {spec}")));
        let result = install::resolve(inventory, spec).and_then(|(core, release)| {
            installer.prepare_release(core, release, &installed, opts.clone(), progress)
        });
        out.push((spec.clone(), result));
    }
    Ok(out)
}

pub fn install(
    ctx: &Context,
    req: &InstallRequest,
    mode: Mode,
    progress: &mut dyn FnMut(Event),
) -> Result<InstallReport> {
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
    Ok(Batch::stage(ctx, mode, prepared, progress))
}

pub fn update(
    ctx: &Context,
    ids: &[String],
    all: bool,
    keep_platforms: bool,
    mode: Mode,
    progress: &mut dyn FnMut(Event),
) -> Result<InstallReport> {
    let pocket = ctx.pocket()?;
    let inventory = ctx.inventory()?;
    let installed = cores::list_installed(&pocket)?;

    // Which cores: explicit ids (must be installed), or everything outdated.
    let mut prepared = Vec::new();
    let targets: Vec<String> = if all {
        installed
            .cores
            .iter()
            .filter(|c| {
                inventory
                    .get(&c.id)
                    .and_then(|e| e.latest())
                    .is_some_and(|l| {
                        pocketeer_core::catalog::inventory::update_available(
                            &c.version,
                            &l.core.metadata.version,
                        )
                    })
            })
            .map(|c| c.id.clone())
            .collect()
    } else {
        let mut targets = Vec::new();
        for id in ids {
            match installed.get_ci(id) {
                Some(c) => targets.push(c.id.clone()),
                None => {
                    prepared.push((id.clone(), Err(Error::CoreNotInstalled { id: id.clone() })))
                }
            }
        }
        targets
    };
    let opts = PlanOptions {
        keep_platforms,
        ..Default::default()
    };
    prepared.extend(prepare(ctx, &inventory, &targets, opts, progress)?);
    Ok(Batch::stage(ctx, mode, prepared, progress))
}

pub type RemoveReport = Batch<UninstallPlan>;

pub fn remove(
    ctx: &Context,
    ids: &[String],
    purge: bool,
    mode: Mode,
    progress: &mut dyn FnMut(Event),
) -> Result<RemoveReport> {
    let pocket = ctx.pocket()?;
    let installed = cores::list_installed(&pocket)?;
    let plans = ids.iter().map(|id| {
        let plan = install::plan_remove(&pocket, &installed, id, purge);
        // Named as installed, whatever case was typed.
        let target = plan.as_ref().map_or(id.clone(), |p| p.core_id.clone());
        (target, plan)
    });
    Ok(Batch::stage(ctx, mode, plans, progress))
}
