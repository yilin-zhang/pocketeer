//! Text for `ops::cores`.

use std::fmt::Write;

use pocketeer_core::card::{
    cores::{CoreInfo, CoreListReport},
    install::{InstallPlan, UninstallPlan},
};
use pocketeer_core::catalog::inventory::InventoryStatus;

use super::{actions, batch_report, bytes, source, table};
use crate::ops::{
    Mode,
    cores::{InstallReport, RemoveReport, SearchReport},
};

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
                    (None, false) if report.inventory.is_loaded() => {
                        ("-".into(), "not in inventory")
                    }
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
        None if info.inventory_status.is_loaded() => {
            let _ = writeln!(out, "  Inventory   not listed");
        }
        None => {}
    }
    inventory_footer(&mut out, &info.inventory_status);
    out
}

pub fn install_report(r: &InstallReport) -> String {
    batch_report(
        r,
        |out, p, mode| install_plan(out, &p.plan, mode),
        |a| {
            let mut parts = vec![format!("{} file(s) written", a.installed)];
            if a.removed > 0 {
                parts.push(format!("{} removed", a.removed));
            }
            if a.moved_files > 0 {
                parts.push(format!("{} moved", a.moved_files));
            }
            parts.join(", ")
        },
    )
}

pub fn remove_report(r: &RemoveReport) -> String {
    batch_report(
        r,
        |out, plan, _| remove_plan(out, plan),
        |a| format!("{} item(s) removed", a.removed),
    )
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

fn inventory_footer(out: &mut String, status: &InventoryStatus) {
    match status {
        InventoryStatus::Loaded { source: src } => {
            let _ = writeln!(out, "\nInventory: {}", source(src));
        }
        InventoryStatus::Unavailable { error } => {
            let _ = writeln!(out, "\nwarning: inventory unavailable: {}", error.message);
            if let Some(hint) = &error.hint {
                let _ = writeln!(out, "hint: {hint}");
            }
        }
    }
}

fn install_plan(out: &mut String, plan: &InstallPlan, mode: Mode) {
    let transition = match &plan.installed_version {
        Some(cur) if !plan.changed => format!("{cur} (already installed)"),
        Some(cur) => format!("{cur} → {}", plan.version),
        None => format!("install {}", plan.version),
    };
    let _ = writeln!(out, "{}: {transition}", plan.core_id);
    if !plan.changed {
        let _ = writeln!(
            out,
            "  nothing to do{}",
            if mode == Mode::Interactive {
                ""
            } else {
                " (use --force to reinstall)"
            }
        );
        return;
    }
    let s = &plan.summary;
    let _ = writeln!(
        out,
        "  {} file(s), {} ({} new, {} replaced){}",
        s.install,
        bytes(s.bytes),
        s.install - s.overwrite,
        s.overwrite,
        if s.skip > 0 {
            format!(", {} skipped", s.skip)
        } else {
            String::new()
        }
    );
    if !plan.also_installs.is_empty() {
        let _ = writeln!(out, "  also installs: {}", plan.also_installs.join(", "));
    }
    if !plan.replaces.is_empty() {
        let _ = writeln!(out, "  replaces: {}", plan.replaces.join(", "));
    }
    if plan.requires_license {
        let _ = writeln!(
            out,
            "  note: this core needs a license file; see the author's instructions"
        );
    }
    actions(out, &plan.actions);
}

fn remove_plan(out: &mut String, plan: &UninstallPlan) {
    let _ = writeln!(
        out,
        "{}: remove {}{}",
        plan.core_id,
        plan.installed_version,
        if plan.purge {
            " (with assets and saves)"
        } else {
            ""
        }
    );
    if plan.actions.is_empty() {
        let _ = writeln!(out, "  nothing to remove");
    }
    actions(out, &plan.actions);
}

#[cfg(test)]
mod tests {
    use pocketeer_core::card::install::{InstallPlan, plan::Summary, zipfile::ZipSource};

    use super::*;

    fn unchanged_plan() -> InstallPlan {
        InstallPlan {
            core_id: "a.B".into(),
            version: "1.0".into(),
            installed_version: Some("1.0".into()),
            source: ZipSource::Url { url: String::new() },
            requires_license: false,
            also_installs: vec![],
            replaces: vec![],
            changed: false,
            summary: Summary::default(),
            actions: vec![],
        }
    }

    #[test]
    fn interactive_text_names_no_cli_flags() {
        let plan = unchanged_plan();
        let mut cli = String::new();
        install_plan(&mut cli, &plan, Mode::DryRun);
        assert!(cli.contains("--force"), "{cli}");
        let tui = install_plan_text(&plan);
        assert!(!tui.contains("--"), "{tui}");
        assert!(tui.contains("nothing to do"), "{tui}");
    }
}
