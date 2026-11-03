//! Text for saves: backup and restore plans, trash plans, the library's
//! saves and their versions.

use std::fmt::Write;

use pocketeer_core::transfer::saves::{
    ConflictReason, PullReason, SaveAction, SaveSkip, SavesPlan, SavesResult, TrashPlan,
    TrashResult, TrashTarget,
};

use super::{Label, Text, bytes, failure_lines, plan_lines, table, time_ago};
use crate::ops::saves::{ListReport, VersionsReport};

impl Label for PullReason {
    fn label(&self) -> &'static str {
        match self {
            PullReason::OnlyOnCard => "new on AP",
            PullReason::CardNewer => "AP is newer",
            PullReason::UserChoice => "explicit choice",
        }
    }
}

impl Label for ConflictReason {
    fn label(&self) -> &'static str {
        match self {
            ConflictReason::ComputerNewer => "computer is newer",
            ConflictReason::DifferentContent => "similar timestamps, different bytes",
        }
    }
}

/// One line of a backup plan; `shown` is how the save is named.
fn action_line(action: &SaveAction, shown: &str) -> Option<String> {
    Some(match action {
        SaveAction::Pull { reason, .. } => format!("← backup {shown} ({})", reason.label()),
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
            reason.label(),
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

/// Each save and what happens to it, by card path, then the counts.
impl Text for SavesPlan {
    fn text(&self) -> String {
        let mut text = String::new();
        plan_lines(
            &mut text,
            self.actions()
                .filter_map(|a| action_line(a, &a.path().display().to_string())),
        );
        let s = self.summary();
        let _ = writeln!(
            text,
            "{} backup(s), {} restore(s), {} conflict(s), {} identical, {} retained",
            s.pull, s.push, s.conflicts, s.up_to_date, s.retained
        );
        if self.changes() == 0 {
            let _ = writeln!(text, "No file changes requested.");
        }
        text
    }
}

impl Text for SavesResult {
    fn text(&self) -> String {
        let mut text = format!(
            "Backed up {}, restored {}, archived {} version(s)\n",
            self.pulled, self.pushed, self.versions_created
        );
        failure_lines(&mut text, &self.failed);
        text
    }
}

impl Text for ListReport {
    fn text(&self) -> String {
        let r = self;
        let mut out = String::new();
        if r.saves.is_empty() {
            let _ = writeln!(out, "No saves in the library yet.");
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
    fn text(&self) -> String {
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
        out
    }
}

impl Text for TrashPlan {
    fn text(&self) -> String {
        let mut text = String::new();
        let plan = self;
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
        if plan.files.is_empty() {
            let _ = writeln!(text, "Nothing to throw away.");
        }
        text
    }
}

impl Text for TrashResult {
    fn text(&self) -> String {
        let mut text = format!("Trashed {}, archived {}\n", self.trashed, self.archived);
        failure_lines(&mut text, &self.failed);
        text
    }
}
