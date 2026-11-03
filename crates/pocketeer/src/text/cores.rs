//! Text for cores: the installed list, the inventory, install and remove plans.

use std::fmt::Write;

use pocketeer_core::card::{
    cores::{CoreInfo, CoreListReport},
    install::{Changes, InstallPlan, Prepared, UninstallPlan},
};
use pocketeer_core::catalog::inventory::InventoryStatus;

use super::{Text, actions, bytes, source, table};
use crate::ops::cores::SearchReport;

impl Text for CoreListReport {
    fn text(&self) -> String {
        let report = self;
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
                        (Some(l), _) if c.update_available => {
                            (l.version.clone(), "update available")
                        }
                        (Some(l), _) => (l.version.clone(), "up to date"),
                        (None, true) => ("-".into(), "no release in inventory"),
                        (None, false) if report.inventory.is_loaded() => {
                            ("-".into(), "not in inventory")
                        }
                        (None, false) => ("?".into(), ""),
                    };
                    let installed = c.installed.as_ref().map(|i| i.version.clone());
                    [
                        c.id.clone(),
                        installed.unwrap_or_default(),
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
}

impl Text for SearchReport {
    fn text(&self) -> String {
        let report = self;
        let mut out = String::new();
        if report.hits.is_empty() {
            out.push_str("No matching cores.\n");
        } else {
            let rows: Vec<[String; 4]> = report
                .hits
                .iter()
                .map(|h| {
                    let mut notes = Vec::new();
                    if let Some(i) = &h.installed {
                        notes.push(format!("installed {}", i.version));
                    }
                    if h.latest.as_ref().is_some_and(|l| l.requires_license) {
                        notes.push("needs license".to_string());
                    }
                    let version = h.latest.as_ref().map(|l| l.version.clone());
                    [
                        h.id.clone(),
                        version.unwrap_or_else(|| "-".into()),
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
}

impl Text for CoreInfo {
    fn text(&self) -> String {
        let info = self;
        let mut out = String::new();
        let _ = writeln!(out, "{}", info.core.id);
        match &info.core.installed {
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
                if let Some(url) = &info.core.repo_url {
                    let _ = writeln!(out, "  Repository  {url}");
                }
                match inv.latest() {
                    Some(latest) => {
                        let flag = if info.core.update_available {
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
}

impl Text for Prepared {
    fn text(&self) -> String {
        let mut out = String::new();
        install_plan(&mut out, &self.plan);
        out
    }
}

impl Text for UninstallPlan {
    fn text(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "{}: remove {}{}",
            self.core_id,
            self.installed_version,
            if self.purge {
                " (with assets and saves)"
            } else {
                ""
            }
        );
        if self.actions.is_empty() {
            let _ = writeln!(out, "  nothing to remove");
        }
        actions(&mut out, &self.actions);
        out
    }
}

/// What applying moved, wrote or removed on the card.
impl Text for Changes {
    fn text(&self) -> String {
        let parts: Vec<String> = [
            (self.installed, "file(s) written"),
            (self.removed, "removed"),
            (self.moved_files, "moved"),
        ]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, what)| format!("{n} {what}"))
        .collect();
        if parts.is_empty() {
            "✓ done: nothing changed\n".into()
        } else {
            format!("✓ done: {}\n", parts.join(", "))
        }
    }
}

fn inventory_footer(out: &mut String, status: &InventoryStatus) {
    match status {
        InventoryStatus::Loaded { source: src } => {
            let _ = writeln!(out, "\nInventory: {}", source(src));
        }
        InventoryStatus::Unavailable { error } => {
            let _ = writeln!(out, "\nwarning: inventory unavailable: {}", error.message);
        }
    }
}

fn install_plan(out: &mut String, plan: &InstallPlan) {
    let transition = match &plan.installed_version {
        Some(cur) if plan.changes() == 0 => format!("{cur} (already installed)"),
        Some(cur) => format!("{cur} → {}", plan.version),
        None => format!("install {}", plan.version),
    };
    let _ = writeln!(out, "{}: {transition}", plan.core_id);
    if plan.changes() == 0 {
        let _ = writeln!(out, "  nothing to do");
        return;
    }
    let s = plan.actions.summary();
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
