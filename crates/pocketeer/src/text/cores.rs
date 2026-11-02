//! Text for `ops::cores`.

use std::fmt::Write;

use pocketeer_core::card::{
    cores::{CoreInfo, CoreListReport},
    install::{InstallPlan, UninstallPlan},
};

use super::{install_plan, inventory_footer, mode_footer, remove_plan, table};
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
