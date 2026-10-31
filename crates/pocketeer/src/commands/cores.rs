use std::process::ExitCode;

use pocketeer_core::{
    Error, ErrorInfo, Pocket, Result,
    cores::{self, CoreInfo, CoreListReport, SearchFilter, SearchHit},
    install::{self, Applied, InstallPlan, PlanOptions, Prepared, UninstallPlan},
    inventory::{Inventory, InventoryStatus},
    progress::Event,
};
use serde::Serialize;

use crate::human::{install_plan, inventory_footer, mode_footer, remove_plan, table};
use crate::{
    cli::{ApplyArgs, CoresCommand, InstallArgs},
    commands::{Mode, Plan, batch_verdict, stage_item},
    context::Context,
    output::{Outcome, Output},
};
use std::fmt::Write;

pub fn run(
    ctx: &Context,
    out: &Output,
    cmd: CoresCommand,
    progress: &mut dyn FnMut(Event),
) -> ExitCode {
    match cmd {
        CoresCommand::List { outdated } => out.emit(list(ctx, outdated), core_list),
        CoresCommand::Search {
            query,
            platform,
            installed,
        } => out.emit(
            search(ctx, query.as_deref(), platform.as_deref(), installed),
            search_text,
        ),
        CoresCommand::Info { id } => out.emit(info(ctx, &id), core_info),
        CoresCommand::Install(args) => {
            out.emit_outcome(install(ctx, &args, progress), install_report)
        }
        CoresCommand::Update {
            ids,
            all,
            keep_platforms,
            apply,
        } => out.emit_outcome(
            update(ctx, &ids, all, keep_platforms, apply, progress),
            install_report,
        ),
        CoresCommand::Remove { ids, purge, apply } => {
            out.emit_outcome(remove(ctx, &ids, purge, apply, progress), remove_report)
        }
    }
}

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

// ---------------------------------------------------------------------------
// install / update

/// Result for one requested core.
#[derive(Debug, Serialize)]
pub struct InstallOutcome {
    /// What the user asked for (`id` or `id@version`, or the zip path).
    pub spec: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<InstallPlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Applied>,
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

impl Plan for InstallPlan {
    fn changes(&self) -> usize {
        self.changed as usize
    }
}

impl Plan for UninstallPlan {
    fn changes(&self) -> usize {
        self.actions.len()
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
    args: &InstallArgs,
    progress: &mut dyn FnMut(Event),
) -> Result<InstallReport> {
    let pocket = ctx.pocket()?;
    let opts = PlanOptions {
        force: args.force,
        keep_platforms: args.keep_platforms,
        ..Default::default()
    };
    let prepared = match &args.from {
        Some(file) => {
            let net = ctx.net()?;
            let installed = cores::list_installed(&pocket)?;
            progress(Event::Status(format!("Planning {}", file.display())));
            let result = net
                .installer(&pocket)
                .prepare_file(file, &installed, opts, progress);
            vec![(file.display().to_string(), result)]
        }
        None => prepare(ctx, &ctx.inventory()?, &args.specs, opts, progress)?,
    };
    Ok(apply_prepared(
        &pocket,
        Mode::from_args(args.apply),
        prepared,
        progress,
    ))
}

pub fn update(
    ctx: &Context,
    ids: &[String],
    all: bool,
    keep_platforms: bool,
    apply: ApplyArgs,
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
                        pocketeer_core::inventory::update_available(
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
    Ok(apply_prepared(
        &pocket,
        Mode::from_args(apply),
        prepared,
        progress,
    ))
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

// ---------------------------------------------------------------------------
// remove

#[derive(Debug, Serialize)]
pub struct RemoveOutcome {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<UninstallPlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Applied>,
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
    apply: ApplyArgs,
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
    Ok(apply_removes(
        &pocket,
        Mode::from_args(apply),
        plans,
        progress,
    ))
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

// ---------------------------------------------------------------------------
// human-readable output

pub fn core_list(report: &CoreListReport) -> String {
    let mut out = String::new();
    if report.cores.is_empty() && report.broken.is_empty() {
        out.push_str("No cores installed.\n");
    }

    if !report.cores.is_empty() {
        let rows: Vec<[String; 4]> = report
            .cores
            .iter()
            .map(|c| {
                let (latest, state) = match (&c.latest, c.in_inventory) {
                    (Some(l), _) if c.update_available => (l.version.clone(), "update available"),
                    (Some(l), _) => (l.version.clone(), "up to date"),
                    (None, true) => ("-".into(), "no release in inventory"),
                    (None, false) if report.inventory.available => ("-".into(), "not in inventory"),
                    (None, false) => ("?".into(), ""),
                };
                [
                    c.core.id.clone(),
                    c.core.version.clone(),
                    latest,
                    state.into(),
                ]
            })
            .collect();
        table(&mut out, ["ID", "INSTALLED", "LATEST", "STATUS"], &rows);
    }

    if !report.broken.is_empty() {
        let _ = writeln!(out, "\nBroken cores:");
        for b in &report.broken {
            let _ = writeln!(out, "  {}  [{}] {}", b.id, b.error.code, b.error.message);
        }
    }

    inventory_footer(&mut out, &report.inventory);
    out
}

pub fn search_text(report: &SearchReport) -> String {
    let mut out = String::new();
    if report.hits.is_empty() {
        out.push_str("No matching cores.\n");
    } else {
        let rows: Vec<[String; 4]> = report
            .hits
            .iter()
            .map(|h| {
                let mut notes = Vec::new();
                if h.installed {
                    notes.push(format!(
                        "installed {}",
                        h.installed_version.as_deref().unwrap_or("?")
                    ));
                }
                if h.requires_license {
                    notes.push("needs license".to_string());
                }
                [
                    h.id.clone(),
                    h.version.clone().unwrap_or_else(|| "-".into()),
                    h.platform_ids.join(", "),
                    notes.join(", "),
                ]
            })
            .collect();
        table(&mut out, ["ID", "VERSION", "PLATFORMS", "NOTES"], &rows);
    }
    inventory_footer(&mut out, &report.inventory);
    out
}

pub fn core_info(info: &CoreInfo) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{}", info.id);
    match &info.installed {
        Some(i) => {
            let _ = writeln!(out, "  Installed   {}  ({})", i.version, i.path.display());
            let _ = writeln!(out, "  Platforms   {}", i.platform_ids.join(", "));
            if let Some(d) = &i.description {
                let _ = writeln!(out, "  About       {d}");
            }
        }
        None => {
            let _ = writeln!(out, "  Installed   no");
        }
    }
    match &info.inventory {
        Some(inv) => {
            if let Some(url) = &info.repo_url {
                let _ = writeln!(out, "  Repository  {url}");
            }
            match inv.latest() {
                Some(latest) => {
                    let flag = if info.update_available {
                        "  ← update available"
                    } else {
                        ""
                    };
                    let _ = writeln!(
                        out,
                        "  Latest      {}{}{flag}",
                        latest.core.metadata.version,
                        latest
                            .core
                            .metadata
                            .date_release
                            .as_deref()
                            .map(|d| format!("  ({d})"))
                            .unwrap_or_default()
                    );
                    if latest.requires_license {
                        let _ = writeln!(out, "  License     required (beta/licensed core)");
                    }
                    if let Some(data) = &latest.data {
                        let files: Vec<&str> = data
                            .data_slots
                            .iter()
                            .filter_map(|s| s.filename.as_deref())
                            .collect();
                        if !files.is_empty() {
                            let _ = writeln!(out, "  Needs files {}", files.join(", "));
                        }
                    }
                }
                None => {
                    let _ = writeln!(out, "  Latest      no release published");
                }
            }
            if inv.releases.len() > 1 {
                let versions: Vec<&str> = inv
                    .releases
                    .iter()
                    .map(|r| r.core.metadata.version.as_str())
                    .collect();
                let _ = writeln!(out, "  Releases    {}", versions.join(", "));
            }
        }
        None if info.inventory_status.available => {
            let _ = writeln!(out, "  Inventory   not listed");
        }
        None => {}
    }
    inventory_footer(&mut out, &info.inventory_status);
    out
}

pub fn install_report(r: &InstallReport) -> String {
    let mut out = String::new();
    for (i, o) in r.outcomes.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        match (&o.plan, &o.error) {
            (None, Some(err)) => {
                let _ = writeln!(out, "{}: error [{}] {}", o.spec, err.code, err.message);
                if let Some(hint) = &err.hint {
                    let _ = writeln!(out, "  hint: {hint}");
                }
            }
            (Some(plan), err) => {
                install_plan(&mut out, plan, r.mode);
                if let Some(a) = &o.result {
                    let mut parts = vec![format!("{} file(s) written", a.installed)];
                    if a.removed > 0 {
                        parts.push(format!("{} removed", a.removed));
                    }
                    if a.moved_files > 0 {
                        parts.push(format!("{} moved", a.moved_files));
                    }
                    let _ = writeln!(out, "  ✓ done: {}", parts.join(", "));
                }
                if let Some(err) = err {
                    let _ = writeln!(out, "  ✗ failed [{}] {}", err.code, err.message);
                }
            }
            (None, None) => {}
        }
    }
    mode_footer(&mut out, r.mode);
    out
}

pub fn remove_report(r: &RemoveReport) -> String {
    let mut out = String::new();
    for (i, o) in r.outcomes.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        match (&o.plan, &o.error) {
            (None, Some(err)) => {
                let _ = writeln!(out, "{}: error [{}] {}", o.id, err.code, err.message);
            }
            (Some(plan), err) => {
                remove_plan(&mut out, plan);
                if let Some(a) = &o.result {
                    let _ = writeln!(out, "  ✓ done: {} item(s) removed", a.removed);
                }
                if let Some(err) = err {
                    let _ = writeln!(out, "  ✗ failed [{}] {}", err.code, err.message);
                }
            }
            (None, None) => {}
        }
    }
    mode_footer(&mut out, r.mode);
    out
}

/// Plan renderings as standalone strings for the TUI's confirm dialogs
/// (so `Mode::Interactive`: no CLI flags in the text).
pub fn install_plan_text(plan: &InstallPlan) -> String {
    let mut s = String::new();
    install_plan(&mut s, plan, Mode::Interactive);
    s
}

pub fn remove_plan_text(plan: &UninstallPlan) -> String {
    let mut s = String::new();
    remove_plan(&mut s, plan);
    s
}
