use std::process::ExitCode;

use pocketeer_core::{
    Error, Pocket, Result,
    library::{
        Library,
        covers::FetchReport,
        device::{self, Device},
        device_sync::{
            self, CoreSources, DeviceSyncOptions, DeviceSyncPlan, DeviceSyncPrepared,
            DeviceSyncResult,
        },
        import::{ImportAction, ImportPlan, ImportSkip},
        orphans::{self, CardImportPlan, CardOnlyRom, PlatformImport},
    },
    progress::Event,
};
use serde::Serialize;

use crate::human::{
    bytes, covers_summary, import_actions, install_plan, plan_lines, skipped_summary,
    staged_footer, suggestion_footer, sync_action_line, table, time_ago,
};
use crate::{
    cli::{DeviceCommand, DeviceImportArgs, SyncArgs},
    commands::{
        Applied, Plan, Staged,
        library::{covers_after_import, suggest_standard_names},
        load_dat, stage, stage_if, verdict,
    },
    context::Context,
    output::{Outcome, Output},
};
use std::fmt::Write;

pub fn run(
    ctx: &Context,
    out: &Output,
    cmd: DeviceCommand,
    progress: &mut dyn FnMut(Event),
) -> ExitCode {
    match cmd {
        DeviceCommand::Add { name } => out.emit(add(ctx, &name), device_show),
        DeviceCommand::List => out.emit(list(ctx), device_list),
        DeviceCommand::Show { device } => out.emit(show(ctx, device.as_deref()), device_show),
        DeviceCommand::Subscribe {
            device,
            platforms,
            cores,
        } => out.emit(
            subscribe(ctx, device.as_deref(), &platforms, &cores, true),
            device_show,
        ),
        DeviceCommand::Unsubscribe {
            device,
            platforms,
            cores,
        } => out.emit(
            subscribe(ctx, device.as_deref(), &platforms, &cores, false),
            device_show,
        ),
        DeviceCommand::Sync(args) => out.emit_outcome(sync(ctx, &args, progress), device_sync),
        DeviceCommand::Forget { device } => out.emit(forget(ctx, &device), device_show),
        DeviceCommand::Orphans { platform, verify } => out.emit(
            orphans(ctx, platform.as_deref(), verify, progress),
            device_orphans,
        ),
        DeviceCommand::Import(args) => {
            out.emit_outcome(import(ctx, &args, progress), device_import)
        }
    }
}

/// `spec` given → by id or name; otherwise the device registered on the
/// mounted card. The card is detected once; callers that need it unwrap the
/// `Result`, the others ignore it.
fn resolve(ctx: &Context, lib: &Library, spec: Option<&str>) -> Result<(Device, Result<Pocket>)> {
    match (spec, ctx.pocket()) {
        (Some(s), pocket) => Ok((device::get(lib, s)?, pocket)),
        (None, Err(e)) => Err(e),
        (None, Ok(pocket)) => {
            let device = device::current(lib, &pocket)?.ok_or(Error::NoDevice)?;
            Ok((device, Ok(pocket)))
        }
    }
}

pub fn add(ctx: &Context, name: &str) -> Result<Device> {
    let lib = ctx.library()?;
    let pocket = ctx.pocket()?;
    device::add(&lib, &pocket, name)
}

#[derive(Debug, Serialize)]
pub struct ListReport {
    pub devices: Vec<Device>,
    /// Id of the device on the mounted card, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
}

pub fn list(ctx: &Context) -> Result<ListReport> {
    let lib = ctx.library()?;
    let current = ctx
        .pocket()
        .ok()
        .and_then(|p| device::current(&lib, &p).ok().flatten())
        .map(|d| d.id);
    Ok(ListReport {
        devices: device::list(&lib)?,
        current,
    })
}

pub fn show(ctx: &Context, spec: Option<&str>) -> Result<Device> {
    let lib = ctx.library()?;
    Ok(resolve(ctx, &lib, spec)?.0)
}

pub fn subscribe(
    ctx: &Context,
    spec: Option<&str>,
    platforms: &[String],
    cores: &[String],
    add: bool,
) -> Result<Device> {
    let lib = ctx.library()?;
    let (d, _) = resolve(ctx, &lib, spec)?;
    device::subscribe(&lib, &d.id, platforms, cores, add)
}

pub fn forget(ctx: &Context, spec: &str) -> Result<Device> {
    let lib = ctx.library()?;
    let d = device::get(&lib, spec)?;
    device::forget(&lib, &d.id)?;
    Ok(d)
}

pub type SyncReport = Staged<DeviceSyncPlan, DeviceSyncResult>;

impl Plan for DeviceSyncPlan {
    fn changes(&self) -> usize {
        DeviceSyncPlan::changes(self)
    }
    fn errors(&self) -> usize {
        self.summary.errors
    }
}

impl Applied for DeviceSyncResult {
    fn failed(&self) -> usize {
        self.failed
    }
}

/// Plans a sync: library ROMs and subscribed cores against the card.
pub fn plan_sync(
    ctx: &Context,
    args: &SyncArgs,
    progress: &mut dyn FnMut(Event),
) -> Result<DeviceSyncPrepared> {
    let lib = ctx.library()?;
    let (device, pocket) = resolve(ctx, &lib, args.device.as_deref())?;
    let pocket = pocket?;
    // Guard against syncing device A's subscriptions onto card B.
    if let Some(marker) = device::read_marker(&pocket)?
        && marker.id != device.id
    {
        return Err(Error::DeviceMismatch {
            card: marker.name,
            requested: device.name,
        });
    }
    let opts = DeviceSyncOptions {
        roms: !args.cores_only,
        cores: !args.roms_only,
        delete: args.delete,
        verify: args.verify,
    };
    // Cores need the inventory and an installer; without them they're reported, not fatal.
    let net = ctx.net()?;
    let installer = net.installer(&pocket);
    let inventory = if opts.cores && !device.cores.is_empty() {
        ctx.inventory().ok()
    } else {
        None
    };
    let sources = inventory.as_ref().map(|inv| CoreSources {
        inventory: inv,
        installer: &installer,
    });
    device_sync::plan(
        &lib,
        &pocket,
        &device,
        opts,
        sources.as_ref(),
        &mut ctx.hash_cache(),
        progress,
    )
}

pub fn apply_sync(
    ctx: &Context,
    prepared: DeviceSyncPrepared,
    progress: &mut dyn FnMut(Event),
) -> Result<DeviceSyncResult> {
    prepared.apply(&ctx.library()?, &ctx.pocket()?, progress)
}

pub fn sync(ctx: &Context, args: &SyncArgs, progress: &mut dyn FnMut(Event)) -> Result<SyncReport> {
    let prepared = plan_sync(ctx, args, progress)?;
    stage(args.apply, prepared.plan.clone(), |_| {
        apply_sync(ctx, prepared, progress)
    })
}

#[derive(Debug, Serialize)]
pub struct OrphansReport {
    pub roms: Vec<CardOnlyRom>,
    pub bytes: u64,
}

/// ROMs on the mounted card the library doesn't hold.
pub fn orphans(
    ctx: &Context,
    platform: Option<&str>,
    verify: bool,
    progress: &mut dyn FnMut(Event),
) -> Result<OrphansReport> {
    let lib = ctx.library()?;
    let pocket = ctx.pocket()?;
    let roms = if verify {
        orphans::list_verified(&lib, &pocket, platform, &mut ctx.hash_cache(), progress)?
    } else {
        orphans::list(&lib, &pocket, platform)?
    };
    Ok(OrphansReport {
        bytes: roms.iter().map(|r| r.size).sum(),
        roms,
    })
}

/// `result` of a card import: one entry per platform that had something to import.
impl Applied for Vec<PlatformImport> {
    fn failed(&self) -> usize {
        self.iter().map(|p| p.result.failed.len()).sum()
    }
}

#[derive(Debug, Serialize)]
pub struct ImportReport {
    #[serde(flatten)]
    pub staged: Staged<CardImportPlan, Vec<PlatformImport>>,
    /// Names asked for that aren't orphans on the card.
    pub not_found: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub covers: Option<FetchReport>,
}

impl ImportReport {
    /// A plan shown in a TUI dialog before it is applied.
    pub fn preview(plan: CardImportPlan) -> Self {
        ImportReport {
            staged: Staged::preview(plan),
            not_found: Vec::new(),
            covers: None,
        }
    }
}

impl Outcome for ImportReport {
    fn error(&self) -> Option<Error> {
        verdict(
            self.staged.changes(),
            self.staged.failed() + self.not_found.len(),
        )
    }
}

/// `--dat-only`: files the DAT doesn't know are skipped instead of imported
/// unverified. The card planner always plans them in; this takes them out.
fn drop_unverified(plan: &mut ImportPlan) {
    for a in &mut plan.actions {
        let ImportAction::Add {
            source,
            inner,
            size,
            verified: false,
            ..
        } = a
        else {
            continue;
        };
        plan.summary.add -= 1;
        plan.summary.unverified -= 1;
        plan.summary.bytes -= *size;
        plan.summary.skip += 1;
        let location = match inner {
            Some(inner) => format!("{}::{inner}", source.display()),
            None => source.display().to_string(),
        };
        *a = ImportAction::Skip {
            source: location,
            reason: ImportSkip::NotInDat,
        };
    }
}

/// Plans copying card-only ROMs into the library, plus the names asked for
/// that aren't orphans on the card.
pub fn plan_import(
    ctx: &Context,
    args: &DeviceImportArgs,
    progress: &mut dyn FnMut(Event),
) -> Result<(CardImportPlan, Vec<String>)> {
    let lib = ctx.library()?;
    let pocket = ctx.pocket()?;
    let dat = load_dat(args.dat.as_deref())?;
    let all = orphans::list(&lib, &pocket, args.platform.as_deref())?;
    let (roms, not_found) = if args.names.is_empty() {
        (all, Vec::new())
    } else {
        let picked: Vec<CardOnlyRom> = all
            .iter()
            .filter(|r| args.names.iter().any(|n| n.eq_ignore_ascii_case(&r.name)))
            .cloned()
            .collect();
        let missing = args
            .names
            .iter()
            .filter(|n| !picked.iter().any(|r| r.name.eq_ignore_ascii_case(n)))
            .cloned()
            .collect();
        (picked, missing)
    };
    let mut plan = orphans::plan_import(
        &lib,
        &pocket,
        &roms,
        dat.as_ref(),
        &|id| ctx.platform_name(id),
        &mut ctx.hash_cache(),
        progress,
    )?;
    for part in &mut plan.parts {
        if args.dat_only {
            drop_unverified(&mut part.plan);
        }
        suggest_standard_names(
            ctx,
            &lib,
            &part.platform,
            &mut part.plan,
            args.standard_names,
        )?;
    }
    Ok((plan, not_found))
}

/// Copies the planned ROMs into the library. The card itself is not changed.
pub fn apply_import(
    ctx: &Context,
    plan: &CardImportPlan,
    progress: &mut dyn FnMut(Event),
) -> Result<Vec<PlatformImport>> {
    orphans::apply_import(&mut ctx.library()?, plan, progress)
}

pub fn import(
    ctx: &Context,
    args: &DeviceImportArgs,
    progress: &mut dyn FnMut(Event),
) -> Result<ImportReport> {
    let (plan, not_found) = plan_import(ctx, args, progress)?;
    let staged = stage_if(args.apply, not_found.is_empty(), plan, |plan| {
        apply_import(ctx, plan, progress)
    })?;
    let added = staged
        .result
        .as_ref()
        .map_or(0, |r| r.iter().map(|p| p.result.added).sum());
    let covers = covers_after_import(
        ctx,
        &ctx.library()?,
        None,
        (added, !args.no_covers),
        progress,
    );
    Ok(ImportReport {
        staged,
        not_found,
        covers,
    })
}

// ---------------------------------------------------------------------------
// human-readable output

pub fn device_show(d: &Device) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{}  ({})", d.name, d.id);
    let _ = writeln!(
        out,
        "  Platforms  {}",
        if d.platforms.is_empty() {
            "(none)".to_string()
        } else {
            d.platforms.join(", ")
        }
    );
    let _ = writeln!(
        out,
        "  Cores      {}",
        if d.cores.is_empty() {
            "(none)".to_string()
        } else {
            d.cores.join(", ")
        }
    );
    let _ = writeln!(
        out,
        "  Last sync  {}",
        match d.last_sync {
            Some(t) => time_ago(t),
            None => "never".into(),
        }
    );
    out
}

pub fn device_list(r: &ListReport) -> String {
    let mut out = String::new();
    if r.devices.is_empty() {
        let _ = writeln!(
            out,
            "No devices. Register the mounted card: pocketeer device add --name <NAME>"
        );
        return out;
    }
    let rows: Vec<[String; 5]> = r
        .devices
        .iter()
        .map(|d| {
            [
                format!(
                    "{}{}",
                    d.name,
                    if r.current.as_deref() == Some(&d.id) {
                        " *"
                    } else {
                        ""
                    }
                ),
                d.id.clone(),
                d.platforms.join(", "),
                d.cores.len().to_string(),
                d.last_sync.map(time_ago).unwrap_or_else(|| "never".into()),
            ]
        })
        .collect();
    table(
        &mut out,
        ["DEVICE", "ID", "PLATFORMS", "CORES", "LAST SYNC"],
        &rows,
    );
    if r.current.is_some() {
        let _ = writeln!(out, "\n* = the card mounted right now");
    }
    out
}

pub fn device_sync(r: &SyncReport) -> String {
    let mut out = String::new();
    let p = &r.plan;
    let _ = writeln!(out, "Sync {} ({})", p.device.name, p.device.id);
    for part in &p.roms {
        let s = &part.plan.summary;
        let _ = writeln!(
            out,
            "\n[{}] {} to copy, {} to rename, {} to delete, {} up to date ({})",
            part.platform,
            s.copy,
            s.rename,
            s.delete,
            s.up_to_date,
            bytes(s.bytes)
        );
        plan_lines(
            &mut out,
            part.plan.actions.iter().filter_map(sync_action_line),
        );
        for m in &part.missing_sources {
            let _ = writeln!(out, "  ✗ library file missing: {m}");
        }
    }
    if !p.cores.is_empty() {
        let _ = writeln!(out);
    }
    for c in &p.cores {
        match (&c.plan, &c.error) {
            (Some(plan), _) => install_plan(&mut out, plan, r.mode),
            (None, Some(e)) => {
                let _ = writeln!(out, "{}: error [{}] {}", c.id, e.code, e.message);
            }
            (None, None) => {}
        }
    }
    if r.result.is_none() {
        let _ = writeln!(
            out,
            "\n{} ROM change(s), {} core change(s){}",
            p.summary.rom_changes,
            p.summary.core_changes,
            if p.summary.errors > 0 {
                format!(", {} problem(s)", p.summary.errors)
            } else {
                String::new()
            }
        );
    }
    staged_footer(
        &mut out,
        r.mode,
        r.result.as_ref(),
        p.changes(),
        "Card is up to date.",
        |out, res| {
            let _ = writeln!(out);
            for rr in &res.roms {
                let _ = writeln!(
                    out,
                    "✓ [{}] copied {}, renamed {}, deleted {}",
                    rr.platform, rr.result.copied, rr.result.renamed, rr.result.deleted
                );
                for f in &rr.result.failed {
                    let _ = writeln!(out, "✗ {}: {}", f.path.display(), f.error.message);
                }
            }
            for cr in &res.cores {
                match (&cr.applied, &cr.error) {
                    (Some(a), _) => {
                        let _ = writeln!(out, "✓ {}: {} file(s) written", cr.id, a.installed);
                    }
                    (None, Some(e)) => {
                        let _ = writeln!(out, "✗ {}: {}", cr.id, e.message);
                    }
                    _ => {}
                }
            }
        },
    );
    out
}

pub fn device_orphans(r: &OrphansReport) -> String {
    let mut out = String::new();
    if r.roms.is_empty() {
        let _ = writeln!(out, "Every ROM on the card is in the library.");
        return out;
    }
    let rows: Vec<[String; 4]> = r
        .roms
        .iter()
        .map(|o| {
            [
                o.platform.clone(),
                o.owner.clone(),
                o.name.clone(),
                if o.name_in_library {
                    format!("{}  (name in library, different bytes)", bytes(o.size))
                } else {
                    bytes(o.size)
                },
            ]
        })
        .collect();
    table(&mut out, ["PLATFORM", "FOLDER", "NAME", "SIZE"], &rows);
    let _ = writeln!(
        out,
        "\n{} ROM(s) only on the card, {}. Import them: pocketeer device import [--platform ID] [NAME…]",
        r.roms.len(),
        bytes(r.bytes)
    );
    out
}

pub fn device_import(r: &ImportReport) -> String {
    let mut out = String::new();
    let p = &r.staged.plan;
    for part in &p.parts {
        let _ = writeln!(out, "[{}]", part.platform);
        let skipped = import_actions(&mut out, part.plan.actions.iter());
        if !skipped.is_empty() {
            let _ = writeln!(
                out,
                "  {}",
                skipped_summary(&skipped).trim_start_matches("; ")
            );
        }
    }
    for n in &r.not_found {
        let _ = writeln!(out, "✗ {n}: not an orphan on this card");
    }
    let _ = writeln!(
        out,
        "{} ROM(s) to copy into the library (the card is not changed)",
        p.changes()
    );
    if r.staged.result.is_none() && p.changes() > 0 {
        suggestion_footer(
            &mut out,
            r.staged.mode,
            p.parts.iter().flat_map(|p| p.plan.actions.iter()),
        );
    }
    // Nothing is applied while a name is missing, so no footer then either.
    let changes = if r.not_found.is_empty() {
        p.changes()
    } else {
        0
    };
    staged_footer(
        &mut out,
        r.staged.mode,
        r.staged.result.as_ref(),
        changes,
        "Nothing to import.",
        |out, res| {
            for pi in res {
                let ir = &pi.result;
                let _ = writeln!(
                    out,
                    "✓ [{}] added {} ROM(s), {}{}",
                    pi.platform,
                    ir.added,
                    bytes(ir.bytes),
                    if ir.duplicates.is_empty() {
                        String::new()
                    } else {
                        format!("; {} already in the library, dropped", ir.duplicates.len())
                    }
                );
                for f in &ir.failed {
                    let _ = writeln!(out, "✗ {}: {}", f.source.display(), f.error.message);
                }
            }
            covers_summary(out, r.covers.as_ref(), r.staged.mode);
        },
    );
    out
}
