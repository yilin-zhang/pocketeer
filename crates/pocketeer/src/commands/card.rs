//! The card itself and what it needs: status, the core inventory, BIOS and
//! data files, macOS junk. Mirrors `human/card.rs`.

use std::process::ExitCode;

use pocketeer_core::{
    Result,
    clean::{self, CleanPlan, CleanResult},
    install::{self, PlacePlan},
    inventory::{Inventory, Platforms},
    net::{Policy, Source},
    progress::Event,
    required::{self, BiosReport},
    status::Status,
};
use serde::Serialize;

use crate::commands::Mode;
use crate::human::{actions, bytes, source, staged_footer};
use crate::{
    cli::{ApplyArgs, BiosCommand},
    commands::{Plan, Staged, apply_actions, stage},
    context::Context,
    output::Output,
};
use pocketeer_core::{clean::JunkKind, required::FileStatus};
use std::fmt::Write;

// ---------------------------------------------------------------------------
// status

pub fn status(ctx: &Context) -> Result<Status> {
    pocketeer_core::status::status(&ctx.pocket()?)
}

// ---------------------------------------------------------------------------
// inventory refresh

#[derive(Debug, Serialize)]
pub struct RefreshReport {
    pub cores: usize,
    pub platforms: usize,
    pub cores_source: Source,
    pub platforms_source: Source,
    pub cache_dir: std::path::PathBuf,
}

pub fn inventory_refresh(ctx: &Context) -> Result<RefreshReport> {
    let net = ctx.net()?;
    let policy = if ctx.policy() == Policy::Offline {
        Policy::Offline
    } else {
        Policy::Refresh
    };
    let inventory = Inventory::load(&net.http, &net.cache, policy)?;
    let platforms = Platforms::load(&net.http, &net.cache, policy)?;
    Ok(RefreshReport {
        cores: inventory.cores.len(),
        platforms: platforms.platforms.len(),
        cores_source: inventory.source,
        platforms_source: platforms.source,
        cache_dir: net.cache.dir().to_path_buf(),
    })
}

// ---------------------------------------------------------------------------
// bios

pub fn bios(
    ctx: &Context,
    out: &Output,
    cmd: BiosCommand,
    progress: &mut dyn FnMut(Event),
) -> ExitCode {
    match cmd {
        BiosCommand::Check { ids, present } => {
            out.emit(bios_check(ctx, &ids, present), bios_report)
        }
        BiosCommand::Place { apply } => {
            out.emit_outcome(bios_place(ctx, apply, progress), place_report)
        }
    }
}

/// `present`: also list the files that are already in place.
pub fn bios_check(ctx: &Context, ids: &[String], present: bool) -> Result<BiosReport> {
    let pocket = ctx.pocket()?;
    let installed = pocketeer_core::cores::list_installed(&pocket)?;
    let only = (!ids.is_empty()).then_some(ids);
    let mut report = required::check(&pocket, &installed, only)?;
    if !present {
        for core in &mut report.cores {
            core.files
                .retain(|f| !matches!(f.status, required::FileStatus::Present));
        }
    }
    Ok(report)
}

impl Plan for PlacePlan {
    fn changes(&self) -> usize {
        self.actions.len()
    }
}

pub type PlaceReport = Staged<PlacePlan, install::Applied>;

pub fn bios_place(
    ctx: &Context,
    apply: ApplyArgs,
    progress: &mut dyn FnMut(Event),
) -> Result<PlaceReport> {
    let pocket = ctx.pocket()?;
    let installed = pocketeer_core::cores::list_installed(&pocket)?;
    let report = required::check(&pocket, &installed, None)?;
    stage(apply, install::plan_place(&report), |plan| {
        apply_actions(&pocket, "bios-place", &plan.actions, progress)
    })
}

// ---------------------------------------------------------------------------
// clean

pub type CleanReport = Staged<CleanPlan, CleanResult>;

impl Plan for CleanPlan {
    fn changes(&self) -> usize {
        self.entries.len()
    }
}

pub fn clean(ctx: &Context, system: bool, apply: ApplyArgs) -> Result<CleanReport> {
    let pocket = ctx.pocket()?;
    let plan = clean::plan(&pocket, system)?;
    stage(apply, plan, |plan| clean::apply(&pocket, plan))
}

// ---------------------------------------------------------------------------
// human-readable output

pub fn status_text(s: &Status) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "SD card    {}", s.root.display());
    if let Some(product) = &s.product {
        let _ = writeln!(out, "Product    {product}");
    }
    match &s.firmware {
        Some(fw) => {
            let version = fw.version.as_deref().unwrap_or("unknown");
            match &fw.build_date {
                Some(date) => {
                    let _ = writeln!(out, "Firmware   {version} (built {date})");
                }
                None => {
                    let _ = writeln!(out, "Firmware   {version}");
                }
            }
        }
        None => {
            let _ = writeln!(out, "Firmware   unknown");
        }
    }
    if let Some(disk) = s.disk {
        let _ = writeln!(
            out,
            "Disk       {} free of {}",
            bytes(disk.available_bytes),
            bytes(disk.total_bytes)
        );
    }
    let broken = if s.broken_cores > 0 {
        format!(" ({} broken, see `pocketeer cores list`)", s.broken_cores)
    } else {
        String::new()
    };
    let _ = writeln!(out, "Cores      {} installed{broken}", s.installed_cores);
    for w in &s.warnings {
        let _ = writeln!(out, "warning: {}", w.message);
    }
    out
}

pub fn bios_report(r: &BiosReport) -> String {
    let mut out = String::new();
    for core in &r.cores {
        let _ = writeln!(out, "{}", core.core_id);
        if core.files.is_empty() && core.notes.is_empty() {
            let _ = writeln!(out, "  ✓ nothing missing");
        }
        for f in &core.files {
            let req = if f.required { "required" } else { "optional" };
            let line = match &f.status {
                FileStatus::Present => format!("  ✓ {}", f.path.display()),
                FileStatus::Missing => format!("  ✗ {}  ({req}, missing)", f.path.display()),
                FileStatus::WrongHash { actual_md5 } => format!(
                    "  ! {}  (wrong md5 {}, expected {})",
                    f.path.display(),
                    &actual_md5[..8.min(actual_md5.len())],
                    f.expected_md5
                        .as_deref()
                        .map(|m| &m[..8.min(m.len())])
                        .unwrap_or("?")
                ),
                FileStatus::Unreadable { error } => {
                    format!("  ! {}  (unreadable: {})", f.path.display(), error.message)
                }
                FileStatus::FoundAtRoot { root_file } => format!(
                    "  ↑ {}  ({req}, found at root: {} → `pocketeer bios place`)",
                    f.path.display(),
                    root_file.display()
                ),
                FileStatus::FoundAtRootZipped { zip, inner } => format!(
                    "  ↑ {}  ({req}, inside {} as {inner}; unzip it at the root)",
                    f.path.display(),
                    zip.display()
                ),
            };
            let _ = writeln!(out, "{line}");
        }
        for note in &core.notes {
            let _ = writeln!(out, "  note: {note}");
        }
    }
    let s = &r.summary;
    let _ = writeln!(
        out,
        "\n{} missing ({} placeable from root, {} zipped at root), {} wrong hash, {} present",
        s.missing, s.placeable, s.zipped_at_root, s.wrong_hash, s.present
    );
    out
}

pub fn place_report(r: &PlaceReport) -> String {
    let mut out = String::new();
    let list = &r.plan.actions;
    if list.is_empty() {
        let _ = writeln!(out, "No root files to place.");
    } else {
        let _ = writeln!(out, "{} file(s) to move into place:", list.len());
        actions(&mut out, list);
    }
    for z in &r.plan.zipped {
        if let FileStatus::FoundAtRootZipped { zip, inner } = &z.status {
            let _ = writeln!(
                out,
                "  (skipped) {} is inside {}; unzip it at the root first",
                inner,
                zip.display()
            );
        }
    }
    staged_footer(
        &mut out,
        r.mode,
        r.result.as_ref(),
        list.len(),
        "",
        |out, a| {
            let _ = writeln!(out, "  ✓ done: {} file(s) moved", a.moved_files);
        },
    );
    out
}

pub fn clean_report(r: &CleanReport) -> String {
    let mut out = String::new();
    if r.plan.entries.is_empty() {
        let _ = writeln!(out, "No macOS junk found.");
    } else {
        for e in &r.plan.entries {
            let kind = match e.kind {
                JunkKind::AppleDouble => "AppleDouble",
                JunkKind::DsStore => ".DS_Store",
                JunkKind::MacosxDir => "__MACOSX",
                JunkKind::SystemDir => "system dir",
                JunkKind::OldBackups => "old pocketeer backups",
            };
            let _ = writeln!(
                out,
                "  - {}  ({kind}, {})",
                e.path.display(),
                bytes(e.bytes)
            );
        }
        let _ = writeln!(
            out,
            "{} item(s), {}",
            r.plan.entries.len(),
            bytes(r.plan.bytes)
        );
    }
    if !r.plan.system_dirs_present.is_empty() {
        let names: Vec<String> = r
            .plan
            .system_dirs_present
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        let _ = writeln!(
            out,
            "macOS system folders left alone: {}{}",
            names.join(", "),
            if r.mode == Mode::Interactive {
                ""
            } else {
                " (use --system to remove)"
            }
        );
    }
    staged_footer(
        &mut out,
        r.mode,
        r.result.as_ref(),
        r.plan.entries.len(),
        "",
        |out, res| {
            let _ = writeln!(
                out,
                "✓ removed {} item(s), {}",
                res.removed,
                bytes(res.bytes)
            );
            for f in &res.failed {
                let _ = writeln!(out, "✗ {}: {}", f.path.display(), f.error.message);
            }
        },
    );
    out
}

pub fn refresh_text(r: &RefreshReport) -> String {
    format!(
        "Inventory: {} cores ({}), {} platforms ({})\nCache: {}\n",
        r.cores,
        source(&r.cores_source),
        r.platforms,
        source(&r.platforms_source),
        r.cache_dir.display()
    )
}

#[derive(Debug, Serialize)]
pub struct RomsReport {
    pub roms: Vec<pocketeer_core::roms::index::CardRom>,
    pub comparison: pocketeer_core::library::matching::Comparison,
    pub warnings: Vec<String>,
}

impl crate::output::Outcome for RomsReport {
    fn error(&self) -> Option<pocketeer_core::Error> {
        let failed = self.roms.iter().filter(|r| r.error.is_some()).count();
        (failed > 0).then_some(pocketeer_core::Error::SomeFailed {
            failed,
            total: self.roms.len(),
        })
    }
}

pub fn roms(
    ctx: &Context,
    out: &Output,
    cmd: crate::cli::CardCommand,
    progress: &mut dyn FnMut(Event),
) -> ExitCode {
    match cmd {
        crate::cli::CardCommand::Rename {
            path,
            new_name,
            apply,
        } => {
            let result = plan_rename(ctx, &path, &new_name)
                .and_then(|plan| stage(apply, plan, |p| apply_rename(ctx, p)));
            out.emit_outcome(result, rename_report)
        }
        crate::cli::CardCommand::Copy {
            roms,
            platform,
            apply,
        } => {
            let result = plan_copy(ctx, platform.as_deref(), &roms, progress)
                .and_then(|plan| stage(apply, plan, |p| apply_copy(ctx, p, progress)));
            out.emit_outcome(result, copy_report)
        }
        crate::cli::CardCommand::Import(args) => out.emit_outcome(
            super::card_import::import(ctx, &args, progress),
            super::card_import::report,
        ),
        _ => out.emit_outcome(roms_report(ctx, cmd, progress), roms_text),
    }
}

fn roms_report(
    ctx: &Context,
    cmd: crate::cli::CardCommand,
    progress: &mut dyn FnMut(Event),
) -> Result<RomsReport> {
    use pocketeer_core::{Error, library::matching, roms::index::Index};
    let pocket = ctx.pocket()?;
    let games = match ctx.library() {
        Ok(lib) => lib.list_games(None, None)?,
        Err(Error::LibraryNotFound { .. }) => Vec::new(),
        Err(e) => return Err(e),
    };
    let mut index = Index::scan(&pocket)?;
    let mut warnings: Vec<String> = index.warning.take().into_iter().collect();
    if let crate::cli::CardCommand::Check { full } = cmd {
        for i in 0..index.roms.len() {
            progress(Event::Status(format!(
                "Checking {}",
                index.roms[i].rel_path.display()
            )));
            index.verify(&pocket, i, full, &|| false);
            progress(Event::Step {
                done: i + 1,
                total: index.roms.len(),
            });
        }
        if let Err(e) = index.save(&pocket) {
            warnings.push(format!(
                "ROM identities updated in memory; cache not saved: {e}"
            ));
        }
    }
    Ok(RomsReport {
        comparison: matching::compare(&games, &index.roms),
        roms: index.roms,
        warnings,
    })
}

fn roms_text(report: &RomsReport) -> String {
    let mut text = String::new();
    for (rom, presence) in report.roms.iter().zip(&report.comparison.card) {
        let _ = writeln!(
            text,
            "{}  {:?}  {:?}",
            rom.rel_path.display(),
            presence,
            rom.confidence
        );
        if let Some(error) = &rom.error {
            let _ = writeln!(text, "  {error}");
        }
    }
    for local in report.comparison.local.values() {
        if local.presence != pocketeer_core::library::matching::Presence::Both {
            let _ = writeln!(
                text,
                "[{}] {}  {:?}",
                local.platform, local.name, local.presence
            );
        } else {
            for copy in local.copies.iter().filter(|c| c.name_mismatch) {
                let _ = writeln!(
                    text,
                    "[{}] {} ≠ {}",
                    local.platform,
                    local.name,
                    copy.path.display()
                );
            }
        }
    }
    for warning in &report.warnings {
        let _ = writeln!(text, "Warning: {warning}");
    }
    text
}

pub type CopyReport = Staged<
    pocketeer_core::library::transfer::CopyPlan,
    pocketeer_core::library::transfer::CopyResult,
>;

pub fn plan_copy(
    ctx: &Context,
    platform: Option<&str>,
    roms: &[String],
    progress: &mut dyn FnMut(Event),
) -> Result<pocketeer_core::library::transfer::CopyPlan> {
    pocketeer_core::library::transfer::plan(
        &ctx.library()?,
        &ctx.pocket()?,
        platform,
        roms,
        progress,
    )
}

pub fn apply_copy(
    ctx: &Context,
    plan: &pocketeer_core::library::transfer::CopyPlan,
    progress: &mut dyn FnMut(Event),
) -> Result<pocketeer_core::library::transfer::CopyResult> {
    pocketeer_core::library::transfer::apply(&ctx.pocket()?, plan, progress)
}

pub fn copy_report(report: &CopyReport) -> String {
    let mut text = String::new();
    for file in &report.plan.files {
        let _ = writeln!(
            text,
            "+ {} ({})",
            file.destination.display(),
            bytes(file.size)
        );
    }
    for path in &report.plan.already_present {
        let _ = writeln!(text, "= already present: {}", path.display());
    }
    for warning in &report.plan.warnings {
        let _ = writeln!(text, "Warning: {warning}");
    }
    staged_footer(
        &mut text,
        report.mode,
        report.result.as_ref(),
        report.plan.changes(),
        "Nothing to copy.",
        |text, result| {
            let _ = writeln!(text, "Copied {} ROM(s)", result.copied);
            for error in &result.failed {
                let _ = writeln!(text, "✗ {}", error.message);
            }
            for warning in &result.warnings {
                let _ = writeln!(text, "Warning: {warning}");
            }
        },
    );
    text
}

pub type RenameReport = Staged<
    pocketeer_core::roms::rename::CardRenamePlan,
    pocketeer_core::roms::rename::CardRenameResult,
>;

pub fn plan_rename(
    ctx: &Context,
    path: &std::path::Path,
    name: &str,
) -> Result<pocketeer_core::roms::rename::CardRenamePlan> {
    pocketeer_core::roms::rename::plan_card(&ctx.pocket()?, path, name)
}

pub fn apply_rename(
    ctx: &Context,
    plan: &pocketeer_core::roms::rename::CardRenamePlan,
) -> Result<pocketeer_core::roms::rename::CardRenameResult> {
    pocketeer_core::roms::rename::apply_card(&ctx.pocket()?, plan)
}

pub fn rename_report(report: &RenameReport) -> String {
    let mut text = String::new();
    for file in &report.plan.moves {
        let _ = writeln!(
            text,
            "{:?}: {} → {}",
            file.kind,
            file.from.display(),
            file.to.display()
        );
    }
    for warning in &report.plan.warnings {
        let _ = writeln!(text, "Warning: {warning}");
    }
    staged_footer(
        &mut text,
        report.mode,
        report.result.as_ref(),
        report.plan.changes(),
        "Nothing to rename.",
        |text, result| {
            let _ = writeln!(text, "Renamed {} file(s)", result.renamed);
            for warning in &result.warnings {
                let _ = writeln!(text, "Warning: {warning}");
            }
        },
    );
    text
}
