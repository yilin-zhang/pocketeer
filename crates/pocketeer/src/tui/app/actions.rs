//! Cores, BIOS and Device tab actions: install, update, remove, place, clean.

use pocketeer_core::install;

use super::*;
use crate::commands::{card as card_cmd, cores as cores_cmd};

impl App {
    // -----------------------------------------------------------------------
    // cores

    pub(super) fn prepare(&mut self, specs: Vec<String>) {
        if self.card().is_none() {
            return;
        }
        let Some(inventory) = self.card_view().and_then(|s| s.inventory.clone()) else {
            self.info(
                "Inventory unavailable",
                vec![
                    "Can't resolve releases without the inventory.".into(),
                    "Offline and nothing cached? Press r once online.".into(),
                ],
            );
            return;
        };
        if specs.is_empty() {
            self.notice("No cores selected.");
            return;
        }
        self.submit("Preparing", Job::Prepare { inventory, specs });
    }

    pub(super) fn install_or_update_selected(&mut self) {
        let Some(row) = self.selected_core() else {
            return;
        };
        let id = row.id.clone();
        if row.installed.is_some() && !row.in_inventory {
            self.notice(format!("{id} isn't in the inventory; nothing to update."));
            return;
        }
        if row.installed.is_some() && !row.update {
            self.notice(format!("{id} is already at its latest release."));
            return;
        }
        self.prepare(vec![id]);
    }

    pub(super) fn update_all(&mut self) {
        let ids: Vec<String> = self
            .card_view()
            .map(|s| {
                s.cores
                    .cores
                    .iter()
                    .filter(|c| c.update_available)
                    .map(|c| c.core.id.clone())
                    .collect()
            })
            .unwrap_or_default();
        if ids.is_empty() {
            self.notice("Every installed core is at its latest version.");
            return;
        }
        self.prepare(ids);
    }

    pub(super) fn show_install_plans(
        &mut self,
        results: Vec<(String, Result<install::Prepared, Error>)>,
    ) {
        let mut text = Vec::new();
        let mut preps = Vec::new();
        let mut changes = 0;
        for (spec, result) in results {
            match result {
                Ok(prep) => {
                    text.push(cores_cmd::install_plan_text(&prep.plan));
                    if prep.plan.changed {
                        changes += 1;
                    }
                    preps.push(prep);
                }
                Err(e) => text.push(format!("{spec}: error: {e}\n")),
            }
        }
        let lines = text_lines(&text.join("\n"));
        if changes == 0 {
            self.info("Nothing to do", lines);
        } else {
            self.confirm(
                format!("Install {changes} core(s)?"),
                lines,
                Pending::Install(preps),
            );
        }
    }

    pub(super) fn remove_selected(&mut self) {
        let Some(pocket) = self.card() else {
            return;
        };
        let Some(row) = self.selected_core() else {
            return;
        };
        if row.installed.is_none() {
            self.notice(format!("{} isn't on this card.", row.id));
            return;
        }
        let id = row.id.clone();
        let installed = self.installed_list();
        match install::plan_remove(&pocket, &installed, &id, false) {
            Ok(plan) if plan.actions.is_empty() => {
                self.notice(format!("{id} has no folders on the card."))
            }
            Ok(plan) => {
                let lines = text_lines(&cores_cmd::remove_plan_text(&plan));
                self.confirm(format!("Remove {id}?"), lines, Pending::Remove(vec![plan]));
            }
            Err(e) => self.error("Can't remove", &e),
        }
    }

    // -----------------------------------------------------------------------
    // BIOS / device tab

    pub(super) fn place(&mut self) {
        if self.card().is_none() {
            return;
        }
        let Some(snap) = self.card_view() else {
            return;
        };
        let plan = install::plan_place(&snap.bios);
        if plan.actions.is_empty() {
            if plan.zipped.is_empty() {
                self.notice("No file at the SD card root matches a missing file.");
            } else {
                let lines = plan
                    .zipped
                    .iter()
                    .map(|z| {
                        format!(
                            "{} is inside a zip at the root; unzip it first.",
                            z.path.display()
                        )
                    })
                    .collect();
                self.info("Nothing to place yet", lines);
            }
            return;
        }
        let lines = text_lines(&card_cmd::place_report(&Staged::preview(plan.clone())));
        self.confirm(
            format!("Move {} file(s) into place?", plan.actions.len()),
            lines,
            Pending::Place(plan),
        );
    }

    pub(super) fn clean(&mut self) {
        if self.card().is_none() {
            return;
        }
        let Some(snap) = self.card_view() else {
            return;
        };
        if snap.clean.entries.is_empty() {
            self.notice("No macOS junk on the card.");
            return;
        }
        let title = format!(
            "Delete {} junk item(s), {}?",
            snap.clean.entries.len(),
            human::bytes(snap.clean.bytes)
        );
        let plan = snap.clean.clone();
        let lines = text_lines(&card_cmd::clean_report(&Staged::preview(plan.clone())));
        self.confirm(title, lines, Pending::Clean(plan));
    }
}
