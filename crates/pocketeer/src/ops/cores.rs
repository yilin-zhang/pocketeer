//! Cores: listing, searching, installing, updating and removing.

use std::path::PathBuf;

use pocketeer_core::{
    Error, ErrorInfo, Pocket, Result,
    card::cores::{self, CoreInfo, CoreListReport, SearchFilter, SearchHit},
    card::install::{self, Changes, InstallPlan, PlanOptions, Prepared, UninstallPlan},
    catalog::inventory::{Inventory, InventoryStatus},
    plan::{Event, Plan},
};
use serde::Serialize;

use super::{Mode, batch_verdict, context::Context, stage_item};
use crate::output::Outcome;

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
    pub hits: Vec<SearchHit>,
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
        inventory: InventoryStatus {
            available: true,
            source: Some(inventory.source.clone()),
            error: None,
        },
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

/// Result for one requested core.
#[derive(Debug, Serialize)]
pub struct InstallOutcome {
    /// What the user asked for (`id` or `id@version`, or the zip path).
    pub spec: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<InstallPlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Changes>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorInfo>,
}

#[derive(Debug, Serialize)]
pub struct InstallReport {
    pub mode: Mode,
    pub outcomes: Vec<InstallOutcome>,
}

impl InstallReport {
    pub fn failed(&self) -> usize {
        self.outcomes.iter().filter(|o| o.error.is_some()).count()
    }
}

impl Outcome for InstallReport {
    fn error(&self) -> Option<Error> {
        batch_verdict(
            self.outcomes
                .iter()
                .map(|o| (o.plan.as_ref().map_or(0, Plan::changes), o.error.is_some())),
        )
    }
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
    let installer = net.installer(&pocket);
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
            let result = net
                .installer(&pocket)
                .prepare_file(file, &installed, opts, progress);
            vec![(file.display().to_string(), result)]
        }
        None => prepare(ctx, &ctx.inventory()?, &req.specs, opts, progress)?,
    };
    Ok(apply_prepared(&pocket, mode, prepared, progress))
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
    Ok(apply_prepared(&pocket, mode, prepared, progress))
}

/// Applies (or, in a dry run, just reports) prepared installs one by one;
/// a failure stays with its core instead of aborting the batch.
pub fn apply_prepared(
    pocket: &Pocket,
    mode: Mode,
    prepared: Vec<(String, Result<Prepared>)>,
    progress: &mut dyn FnMut(Event),
) -> InstallReport {
    let mut outcomes = Vec::new();
    for (spec, result) in prepared {
        match result {
            Err(err) => outcomes.push(InstallOutcome {
                spec,
                plan: None,
                result: None,
                error: Some(err.to_info()),
            }),
            Ok(prep) => {
                let plan = prep.plan.clone();
                let (result, error) = stage_item(mode, &plan, |plan| {
                    progress(Event::Status(format!(
                        "Installing {} {}",
                        plan.core_id, plan.version
                    )));
                    prep.apply(pocket, progress)
                });
                outcomes.push(InstallOutcome {
                    spec,
                    plan: Some(plan),
                    result,
                    error,
                });
            }
        }
    }
    InstallReport { mode, outcomes }
}

#[derive(Debug, Serialize)]
pub struct RemoveOutcome {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<UninstallPlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Changes>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorInfo>,
}

#[derive(Debug, Serialize)]
pub struct RemoveReport {
    pub mode: Mode,
    pub outcomes: Vec<RemoveOutcome>,
}

impl RemoveReport {
    pub fn failed(&self) -> usize {
        self.outcomes.iter().filter(|o| o.error.is_some()).count()
    }
}

impl Outcome for RemoveReport {
    fn error(&self) -> Option<Error> {
        batch_verdict(
            self.outcomes
                .iter()
                .map(|o| (o.plan.as_ref().map_or(0, Plan::changes), o.error.is_some())),
        )
    }
}

pub fn remove(
    ctx: &Context,
    ids: &[String],
    purge: bool,
    mode: Mode,
    progress: &mut dyn FnMut(Event),
) -> Result<RemoveReport> {
    let pocket = ctx.pocket()?;
    let installed = cores::list_installed(&pocket)?;
    let plans = ids
        .iter()
        .map(|id| {
            (
                id.clone(),
                install::plan_remove(&pocket, &installed, id, purge),
            )
        })
        .collect();
    Ok(apply_removes(&pocket, mode, plans, progress))
}

/// Applies (or, in a dry run, just reports) remove plans one by one.
pub fn apply_removes(
    pocket: &Pocket,
    mode: Mode,
    plans: Vec<(String, Result<UninstallPlan>)>,
    progress: &mut dyn FnMut(Event),
) -> RemoveReport {
    let mut outcomes = Vec::new();
    for (id, plan) in plans {
        match plan {
            Err(err) => outcomes.push(RemoveOutcome {
                id,
                plan: None,
                result: None,
                error: Some(err.to_info()),
            }),
            Ok(plan) => {
                let (result, error) = stage_item(mode, &plan, |plan| {
                    progress(Event::Status(format!("Removing {}", plan.core_id)));
                    install::apply_remove(pocket, plan, progress)
                });
                outcomes.push(RemoveOutcome {
                    id: plan.core_id.clone(),
                    plan: Some(plan),
                    result,
                    error,
                });
            }
        }
    }
    RemoveReport { mode, outcomes }
}
