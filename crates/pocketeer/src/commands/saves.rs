use std::{path::Path, process::ExitCode};

pub use pocketeer_core::library::saves::Direction;
use pocketeer_core::{
    Result,
    library::saves::{self, SaveEntry, SaveVersion, SavesOptions, SavesPlan, SavesResult},
    progress::Event,
};
use serde::Serialize;

use crate::human::{bytes, plan_lines, staged_footer, table, time_ago};
use crate::{
    cli::{ApplyArgs, SavesCommand},
    commands::{Applied, Plan, Staged, stage},
    context::Context,
    output::Output,
};
use pocketeer_core::library::saves::{SaveAction, SaveReason, SaveSkip};
use std::fmt::Write;

pub fn run(
    ctx: &Context,
    out: &Output,
    cmd: SavesCommand,
    progress: &mut dyn FnMut(Event),
) -> ExitCode {
    match cmd {
        SavesCommand::Sync {
            pull,
            push,
            no_states,
            apply,
        } => {
            let direction = match (pull, push) {
                (true, _) => Direction::Pull,
                (_, true) => Direction::Push,
                _ => Direction::Both,
            };
            out.emit_outcome(
                sync(ctx, direction, !no_states, apply, progress),
                saves_sync,
            )
        }
        SavesCommand::List => out.emit(list(ctx), saves_list),
        SavesCommand::Versions { path } => out.emit(versions(ctx, &path), saves_versions),
        SavesCommand::Restore { version_id, apply } => {
            out.emit_outcome(restore(ctx, version_id, apply), saves_restore)
        }
    }
}

pub type SyncReport = Staged<SavesPlan, SavesResult>;

pub fn plan_sync(
    ctx: &Context,
    direction: Direction,
    include_states: bool,
    progress: &mut dyn FnMut(Event),
) -> Result<SavesPlan> {
    let opts = SavesOptions {
        direction,
        include_states,
    };
    saves::plan(
        &ctx.library()?,
        &ctx.pocket()?,
        opts,
        &mut ctx.hash_cache(),
        progress,
    )
}

pub fn apply_sync(
    ctx: &Context,
    plan: &SavesPlan,
    progress: &mut dyn FnMut(Event),
) -> Result<SavesResult> {
    saves::apply(
        &mut ctx.library()?,
        &ctx.pocket()?,
        plan,
        &mut ctx.hash_cache(),
        progress,
    )
}

pub fn sync(
    ctx: &Context,
    direction: Direction,
    include_states: bool,
    apply: ApplyArgs,
    progress: &mut dyn FnMut(Event),
) -> Result<SyncReport> {
    let plan = plan_sync(ctx, direction, include_states, progress)?;
    stage(apply, plan, |plan| apply_sync(ctx, plan, progress))
}

#[derive(Debug, Serialize)]
pub struct ListReport {
    pub saves: Vec<SaveEntry>,
    pub bytes: u64,
}

pub fn list(ctx: &Context) -> Result<ListReport> {
    let lib = ctx.library()?;
    let saves = saves::list(&lib)?;
    Ok(ListReport {
        bytes: saves.iter().map(|s| s.size).sum(),
        saves,
    })
}

#[derive(Debug, Serialize)]
pub struct VersionsReport {
    pub rel_path: std::path::PathBuf,
    pub versions: Vec<SaveVersion>,
}

pub fn versions(ctx: &Context, rel: &Path) -> Result<VersionsReport> {
    let lib = ctx.library()?;
    Ok(VersionsReport {
        rel_path: rel.to_path_buf(),
        versions: saves::versions(&lib, rel)?,
    })
}

/// The version that would become the library's current copy.
#[derive(Debug, Serialize)]
pub struct RestorePlan {
    pub version: SaveVersion,
}

impl Plan for RestorePlan {
    fn changes(&self) -> usize {
        1
    }
}

#[derive(Debug, Serialize)]
pub struct Restored {
    pub restored: bool,
}

impl Applied for Restored {
    fn failed(&self) -> usize {
        0
    }
}

pub type RestoreReport = Staged<RestorePlan, Restored>;

pub fn restore(ctx: &Context, version_id: i64, apply: ApplyArgs) -> Result<RestoreReport> {
    let mut lib = ctx.library()?;
    let mut cache = ctx.hash_cache();
    let version = saves::versions_by_id(&lib, version_id)?;
    stage(apply, RestorePlan { version }, |plan| {
        saves::restore(&mut lib, plan.version.id, &mut cache)?;
        Ok(Restored { restored: true })
    })
}

// ---------------------------------------------------------------------------
// human-readable output

fn save_reason(r: SaveReason) -> &'static str {
    match r {
        SaveReason::OnlyOnCard => "new on card",
        SaveReason::OnlyInLibrary => "not on card",
        SaveReason::CardNewer => "card is newer",
        SaveReason::LibraryNewer => "library is newer",
        SaveReason::Tie => "same time, different content; card wins",
    }
}

pub fn saves_sync(r: &SyncReport) -> String {
    let mut out = String::new();
    let p = &r.plan;
    plan_lines(
        &mut out,
        p.actions.iter().filter_map(|a| {
            Some(match a {
                SaveAction::Pull { rel, reason, .. } => {
                    format!("  ← {}  ({})", rel.display(), save_reason(*reason))
                }
                SaveAction::Push { rel, reason, .. } => {
                    format!("  → {}  ({})", rel.display(), save_reason(*reason))
                }
                SaveAction::Skip {
                    rel,
                    reason: SaveSkip::DirectionExcluded,
                } => format!("  · {}  (would go the other way; excluded)", rel.display()),
                SaveAction::Skip { .. } => return None,
            })
        }),
    );
    let s = &p.summary;
    let _ = writeln!(
        out,
        "{} to pull (card → library), {} to push (library → card), {} up to date{} ({})",
        s.pull,
        s.push,
        s.up_to_date,
        if s.excluded > 0 {
            format!(", {} excluded", s.excluded)
        } else {
            String::new()
        },
        bytes(s.bytes)
    );
    staged_footer(
        &mut out,
        r.mode,
        r.result.as_ref(),
        p.changes(),
        "Saves are in sync.",
        |out, res| {
            let _ = writeln!(
                out,
                "✓ pulled {}, pushed {}, {} older cop{} kept as versions",
                res.pulled,
                res.pushed,
                res.versions_created,
                if res.versions_created == 1 {
                    "y"
                } else {
                    "ies"
                }
            );
            for f in &res.failed {
                let _ = writeln!(out, "✗ {}: {}", f.rel.display(), f.error.message);
            }
        },
    );
    out
}

pub fn saves_list(r: &ListReport) -> String {
    let mut out = String::new();
    if r.saves.is_empty() {
        let _ = writeln!(
            out,
            "No saves in the library yet. Run `pocketeer saves sync` with a card mounted."
        );
        return out;
    }
    let rows: Vec<[String; 4]> = r
        .saves
        .iter()
        .map(|s| {
            [
                s.rel_path.display().to_string(),
                bytes(s.size),
                time_ago(s.mtime),
                s.versions.to_string(),
            ]
        })
        .collect();
    table(&mut out, ["SAVE", "SIZE", "MODIFIED", "VERSIONS"], &rows);
    let _ = writeln!(out, "\n{} save file(s), {}", r.saves.len(), bytes(r.bytes));
    out
}

pub fn saves_versions(r: &VersionsReport) -> String {
    let mut out = String::new();
    if r.versions.is_empty() {
        let _ = writeln!(out, "No older versions of {}.", r.rel_path.display());
        return out;
    }
    let rows: Vec<[String; 4]> = r
        .versions
        .iter()
        .map(|v| {
            [
                v.id.to_string(),
                time_ago(v.mtime),
                bytes(v.size),
                v.sha1[..8.min(v.sha1.len())].to_string(),
            ]
        })
        .collect();
    table(&mut out, ["ID", "MODIFIED", "SIZE", "SHA1"], &rows);
    let _ = writeln!(out, "\nRestore one with: pocketeer saves restore <ID>");
    out
}

pub fn saves_restore(r: &RestoreReport) -> String {
    let v = &r.plan.version;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{} version {} ({}, modified {})",
        v.rel_path.display(),
        v.id,
        bytes(v.size),
        time_ago(v.mtime)
    );
    staged_footer(&mut out, r.mode, r.result.as_ref(), 1, "", |out, _| {
        let _ = writeln!(
            out,
            "✓ now the current copy in the library; push it with `pocketeer saves sync --push`"
        );
    });
    out
}
