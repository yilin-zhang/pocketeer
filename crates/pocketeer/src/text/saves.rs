//! Text for `ops::saves`.

use std::{collections::BTreeMap, fmt::Write, path::PathBuf};

use pocketeer_core::{
    plan::Plan,
    transfer::saves::{SaveAction, SaveReason, SaveSkip, TrashTarget},
};

use super::{Reader, Text, bytes, failure_lines, plan_lines, staged_footer, table, time_ago};
use crate::ops::saves::{ListReport, SyncReport, TrashReport, VersionsReport};

fn save_reason(reason: SaveReason) -> &'static str {
    match reason {
        SaveReason::OnlyOnCard => "new on AP",
        SaveReason::CardNewer => "AP is newer",
        SaveReason::ComputerNewer => "computer is newer",
        SaveReason::DifferentContent => "similar timestamps, different bytes",
        SaveReason::UserChoice => "explicit choice",
    }
}

/// One line of a backup plan; `shown` is how the save is named.
fn action_line(action: &SaveAction, shown: &str) -> Option<String> {
    Some(match action {
        SaveAction::Pull { reason, .. } => format!("← backup {shown} ({})", save_reason(*reason)),
        SaveAction::Push { source, .. } => {
            format!("→ restore AP {shown} from {}", source.display())
        }
        SaveAction::Conflict {
            card,
            computer,
            reason,
            ..
        } => format!(
            "? {shown}: {}; AP {}, computer {} — unchanged until chosen",
            save_reason(*reason),
            time_ago(card.mtime),
            time_ago(computer.mtime)
        ),
        SaveAction::Skip {
            reason: SaveSkip::OnlyInLibrary,
            ..
        } => format!("· keep computer backup {shown}"),
        SaveAction::Skip {
            reason: SaveSkip::LeftUnchanged,
            ..
        } => format!("· leave both unchanged {shown}"),
        SaveAction::Skip { .. } => return None,
    })
}

impl Text for SyncReport {
    fn text(&self, reader: Reader) -> String {
        let report = self;
        saves_sync_by_game(report, &BTreeMap::new(), reader)
    }
}

/// A backup plan with its saves listed under the game each belongs to
/// (`games`: card path → game name); the rest come last as "Other saves".
/// With no names it lists card paths, as the CLI prints.
pub fn saves_sync_by_game(
    report: &SyncReport,
    games: &BTreeMap<PathBuf, String>,
    reader: Reader,
) -> String {
    let mut text = String::new();
    if games.is_empty() {
        plan_lines(
            &mut text,
            report
                .plan
                .actions
                .iter()
                .filter_map(|a| action_line(a, &a.path().display().to_string())),
        );
    } else {
        let mut groups: BTreeMap<(bool, &str), Vec<String>> = BTreeMap::new();
        for a in &report.plan.actions {
            let rel = a.path();
            let name = rel.file_name().unwrap_or_default().to_string_lossy();
            let (key, shown) = match games.get(rel) {
                Some(game) => ((false, game.as_str()), name.into_owned()),
                None => ((true, "Other saves"), rel.display().to_string()),
            };
            if let Some(line) = action_line(a, &shown) {
                groups.entry(key).or_default().push(format!("  {line}"));
            }
        }
        plan_lines(
            &mut text,
            groups
                .into_iter()
                .flat_map(|((_, game), lines)| std::iter::once(game.to_string()).chain(lines)),
        );
    }
    let s = &report.plan.summary;
    let _ = writeln!(
        text,
        "{} backup(s), {} restore(s), {} conflict(s), {} identical, {} retained",
        s.pull, s.push, s.conflicts, s.up_to_date, s.retained
    );
    staged_footer(
        &mut text,
        report.mode,
        reader,
        report.result.as_ref(),
        report.plan.changes(),
        "No file changes requested.",
        |text, result| {
            let _ = writeln!(
                text,
                "Backed up {}, restored {}, archived {} version(s)",
                result.pulled, result.pushed, result.versions_created
            );
            failure_lines(text, &result.failed);
        },
    );
    text
}

impl Text for ListReport {
    fn text(&self, _: Reader) -> String {
        let r = self;
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
}

impl Text for VersionsReport {
    fn text(&self, _: Reader) -> String {
        let r = self;
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
}

impl Text for TrashReport {
    fn text(&self, reader: Reader) -> String {
        let report = self;
        let mut text = String::new();
        let plan = &report.plan;
        plan_lines(
            &mut text,
            plan.files.iter().map(|f| match plan.target {
                TrashTarget::Card if f.archive_first => format!(
                    "- delete from AP {} (archived into the library first)",
                    f.path.display()
                ),
                TrashTarget::Card => format!(
                    "- delete from AP {} (the library has these bytes)",
                    f.path.display()
                ),
                TrashTarget::Version(_) => {
                    format!(
                        "→ trash older version of {} ({})",
                        f.save.display(),
                        bytes(f.size)
                    )
                }
                _ if f.path.starts_with("saves/.history") => {
                    format!(
                        "→ trash older version of {} ({})",
                        f.save.display(),
                        bytes(f.size)
                    )
                }
                _ => format!("→ trash backup {} ({})", f.save.display(), bytes(f.size)),
            }),
        );
        if plan.target != TrashTarget::Card && !plan.files.is_empty() {
            let _ = writeln!(
                text,
                "Files go to {} inside the library.",
                plan.trash.display()
            );
        }
        staged_footer(
            &mut text,
            report.mode,
            reader,
            report.result.as_ref(),
            plan.changes(),
            "Nothing to throw away.",
            |text, result| {
                let _ = writeln!(
                    text,
                    "Trashed {}, archived {}",
                    result.trashed, result.archived
                );
                failure_lines(text, &result.failed);
            },
        );
        text
    }
}

/// A backup or restore plan as the TUI's dialog shows it: saves under the
/// game they belong to, each with what happens in a few words. Saves that
/// stay as they are aren't listed.
pub fn saves_dialog(
    plan: &pocketeer_core::transfer::saves::SavesPlan,
    games: &BTreeMap<PathBuf, String>,
) -> Vec<String> {
    let mut groups: BTreeMap<(bool, String), Vec<String>> = BTreeMap::new();
    for a in &plan.actions {
        let what = match a {
            SaveAction::Pull {
                reason: SaveReason::OnlyOnCard,
                ..
            } => "← back up, new on AP".to_string(),
            SaveAction::Pull { .. } => "← back up, AP is newer".to_string(),
            SaveAction::Push { .. } => "→ put back on AP".to_string(),
            SaveAction::Conflict { .. } => "? changed on both sides, left as is".to_string(),
            SaveAction::Skip { .. } => continue,
        };
        let rel = a.path();
        let file = rel.file_name().unwrap_or_default().to_string_lossy();
        let (key, line) = match games.get(rel) {
            Some(game) if pocketeer_core::names::stem(&file) == game => {
                ((false, game.clone()), format!("  {what}"))
            }
            Some(game) => ((false, game.clone()), format!("  {what}: {file}")),
            None => ((true, String::new()), format!("{what}: {file}")),
        };
        groups.entry(key).or_default().push(line);
    }
    let mut lines = Vec::new();
    for ((other, game), items) in groups {
        if !other {
            lines.push(game);
        } else if !lines.is_empty() {
            lines.push("Other saves".into());
        }
        lines.extend(items);
    }
    lines
}

/// A trash plan as the TUI's dialog shows it.
pub fn trash_dialog(plan: &pocketeer_core::transfer::saves::TrashPlan) -> Vec<String> {
    let name = |p: &std::path::Path| {
        p.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    };
    let mut lines: Vec<String> = plan
        .files
        .iter()
        .map(|f| match plan.target {
            TrashTarget::Card => name(&f.path),
            _ if f.path.starts_with("saves/.history") => {
                format!("{}  older version, {}", name(&f.save), bytes(f.size))
            }
            _ => format!("{}  backup, {}", name(&f.save), bytes(f.size)),
        })
        .collect();
    lines.push(String::new());
    lines.push(match plan.target {
        TrashTarget::Card if plan.files.iter().any(|f| f.archive_first) => {
            "The library keeps a copy first.".into()
        }
        TrashTarget::Card => "The library already has a copy.".into(),
        _ => "They go to .trash/ in the library.".into(),
    });
    lines
}
