use crate::human::{bytes, plan_lines, staged_footer, table, time_ago};
use crate::{
    cli::{SaveConflictArg, SavesCommand},
    commands::{Staged, stage},
    context::Context,
    output::Output,
};
use pocketeer_core::{
    Result,
    library::saves::{
        self, ConflictChoice, SaveAction, SaveEntry, SaveReason, SaveSkip, SaveVersion,
        SavesOptions, SavesPlan, SavesResult,
    },
    progress::Event,
};
use serde::Serialize;
use std::{fmt::Write, path::Path, process::ExitCode};

pub fn run(
    ctx: &Context,
    out: &Output,
    cmd: SavesCommand,
    progress: &mut dyn FnMut(Event),
) -> ExitCode {
    match cmd {
        SavesCommand::Backup {
            no_states,
            conflicts,
            apply,
        } => {
            let result = plan_sync(ctx, !no_states, progress).and_then(|mut plan| {
                if let Some(choice) = conflicts {
                    let choice = match choice {
                        SaveConflictArg::Ap => ConflictChoice::Card,
                        SaveConflictArg::Computer => ConflictChoice::Computer,
                        SaveConflictArg::Skip => ConflictChoice::Skip,
                    };
                    for i in 0..plan.actions.len() {
                        plan.resolve(i, choice);
                    }
                }
                stage(apply, plan, |p| apply_sync(ctx, p, progress))
            });
            out.emit_outcome(result, saves_sync)
        }
        SavesCommand::Restore {
            path,
            version,
            apply,
        } => {
            let result =
                (|| saves::plan_restore(&ctx.library()?, &ctx.pocket()?, &path, version))();
            let result =
                result.and_then(|plan| stage(apply, plan, |p| apply_sync(ctx, p, progress)));
            out.emit_outcome(result, saves_sync)
        }
        SavesCommand::List => out.emit(list(ctx), saves_list),
        SavesCommand::Versions { path } => out.emit(versions(ctx, &path), saves_versions),
    }
}

pub type SyncReport = Staged<SavesPlan, SavesResult>;

pub fn plan_sync(
    ctx: &Context,
    include_states: bool,
    progress: &mut dyn FnMut(Event),
) -> Result<SavesPlan> {
    saves::plan(
        &ctx.library()?,
        &ctx.pocket()?,
        SavesOptions { include_states },
        progress,
    )
}

pub fn apply_sync(
    ctx: &Context,
    plan: &SavesPlan,
    progress: &mut dyn FnMut(Event),
) -> Result<SavesResult> {
    saves::apply(&mut ctx.library()?, &ctx.pocket()?, plan, progress)
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

fn save_reason(reason: SaveReason) -> &'static str {
    match reason {
        SaveReason::OnlyOnCard => "new on AP",
        SaveReason::CardNewer => "AP is newer",
        SaveReason::ComputerNewer => "computer is newer",
        SaveReason::DifferentContent => "similar timestamps, different bytes",
        SaveReason::UserChoice => "explicit choice",
    }
}

pub fn saves_sync(report: &SyncReport) -> String {
    let mut text = String::new();
    plan_lines(
        &mut text,
        report.plan.actions.iter().filter_map(|a| {
            Some(match a {
                SaveAction::Pull { rel, reason, .. } => {
                    format!("← backup {} ({})", rel.display(), save_reason(*reason))
                }
                SaveAction::Push { rel, source, .. } => {
                    format!("→ restore AP {} from {}", rel.display(), source.display())
                }
                SaveAction::Conflict {
                    rel,
                    card,
                    computer,
                    reason,
                } => format!(
                    "? {}: {}; AP {}, computer {} — unchanged until chosen",
                    rel.display(),
                    save_reason(*reason),
                    time_ago(card.mtime),
                    time_ago(computer.mtime)
                ),
                SaveAction::Skip {
                    rel,
                    reason: SaveSkip::OnlyInLibrary,
                } => format!("· keep computer backup {}", rel.display()),
                SaveAction::Skip {
                    rel,
                    reason: SaveSkip::LeftUnchanged,
                } => format!("· leave both unchanged {}", rel.display()),
                SaveAction::Skip { .. } => return None,
            })
        }),
    );
    let s = &report.plan.summary;
    let _ = writeln!(
        text,
        "{} backup(s), {} restore(s), {} conflict(s), {} identical, {} retained",
        s.pull, s.push, s.conflicts, s.up_to_date, s.retained
    );
    staged_footer(
        &mut text,
        report.mode,
        report.result.as_ref(),
        report.plan.changes(),
        "No file changes requested.",
        |text, result| {
            let _ = writeln!(
                text,
                "Backed up {}, restored {}, archived {} version(s)",
                result.pulled, result.pushed, result.versions_created
            );
            for failure in &result.failed {
                let _ = writeln!(
                    text,
                    "✗ {}: {}",
                    failure.rel.display(),
                    failure.error.message
                );
            }
        },
    );
    text
}

pub fn saves_list(r: &ListReport) -> String {
    let mut out = String::new();
    if r.saves.is_empty() {
        let _ = writeln!(
            out,
            "No saves in the library yet. Run `pocketeer saves backup` with a card mounted."
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
    let _ = writeln!(
        out,
        "\nRestore one with: pocketeer saves restore <PATH> --version <ID>"
    );
    out
}
