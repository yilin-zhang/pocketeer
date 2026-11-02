//! What the keys do: cores, BIOS and card actions first, then the Games
//! tab (library, transfers, covers, rename, removal).

use pocketeer_core::{card::install, library::Library};

use super::*;
use crate::ops::{card::ImportRequest as CardImportRequest, library as library_cmd};

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
        let mut preps = Vec::new();
        let mut errors = Vec::new();
        for (spec, result) in results {
            match result {
                Ok(prep) => preps.push(prep),
                Err(e) => errors.push(format!("{spec}: error: {e}\n")),
            }
        }
        self.planned(Pending::Install { preps, errors });
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
            Ok(plan) => self.planned(Pending::Remove(vec![plan])),
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
        let plan = bios::plan_place(&snap.bios);
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
        self.planned(Pending::Place(plan));
    }

    pub(super) fn clean(&mut self) {
        if self.card().is_none() {
            return;
        }
        self.submit("Scanning for junk", Job::Plan(Request::Clean));
    }
}

// ---------------------------------------------------------------------------
// Games tab

impl App {
    pub(super) fn import_cover(&mut self) {
        let Some(game) = self.selected_game() else {
            self.notice("Select a library game first; import a card-only ROM with i before adding its cover.");
            return;
        };
        let game_id = game.game.id;
        let title = format!("Import cover: {}", game.game.name);
        self.input(
            title,
            vec![
                "Choose a PNG, JPEG or WebP image to copy into the library.".into(),
                "Tab completes paths. Replacement is confirmed before saving.".into(),
            ],
            super::path_input::start_path(),
            InputKind::ImportCover { game_id },
        );
    }

    /// `n`: create the library at the configured path, then reload.
    pub(super) fn init_library(&mut self) {
        if self.library_view().is_some() {
            self.notice("There already is a library here.");
            return;
        }
        let Ok(path) = self.ctx().library_path() else {
            self.notice("Can't determine a home directory for the library.");
            return;
        };
        match Library::init(&path) {
            Ok(_) => {
                self.notice(format!("✓ created library at {}", path.display()));
                self.reload();
            }
            Err(e) => self.error("Can't create the library", &e),
        }
    }

    /// `a`: import ROMs from a folder, asked for in three steps.
    pub(super) fn import_folder(&mut self) {
        if self.library_view().is_none() {
            self.no_library();
            return;
        }
        self.input(
            "Import ROMs  1/3: what",
            vec![
                "A folder of ROMs on this computer, or one ROM / zip file.".into(),
                "Matching entries are listed as you type; ↓ picks one, Tab completes.".into(),
            ],
            super::path_input::start_path(),
            InputKind::ImportDir,
        );
    }

    pub(super) fn copy_selected(&mut self) {
        if self.card().is_none() {
            return;
        }
        let Some(game) = self.selected_game() else {
            self.notice("Select a local game to copy.");
            return;
        };
        let platform = Some(game.game.platform.clone());
        let roms = game.game.roms.iter().map(|r| r.id.to_string()).collect();
        self.submit("Planning copy", Job::Plan(Request::Copy { platform, roms }));
    }

    pub(super) fn plan_saves(&mut self) {
        if self.card().is_some() && self.lib_ctx().is_some() {
            self.submit("Comparing saves", Job::Plan(Request::Saves));
        }
    }

    /// Identify content before fetching covers, once per session and after imports.
    pub(super) fn auto_identify(&mut self) {
        let unchecked = self.library_view().is_some_and(|l| {
            l.games.iter().any(|g| {
                g.game
                    .roms
                    .iter()
                    .any(|r| r.dat.status == pocketeer_core::library::identify::Status::Unchecked)
            })
        });
        if (self.auto.metadata_done && !unchecked)
            || self.busy.side.is_some()
            || self.library_view().is_none()
        {
            return;
        }
        self.auto.metadata_done = true;
        self.identify_library();
    }

    pub(super) fn identify_library(&mut self) {
        if self.library_view().is_none() || self.busy.side.is_some() {
            return;
        }
        self.busy.side = Some("checking DATs".into());
        self.submit_side(Job::Identify);
    }

    /// `t`: offer standard (No-Intro) names for games whose file names aren't.
    pub(super) fn standardize(&mut self) {
        if self.lib_ctx().is_none() {
            return;
        }
        self.submit("Matching standard names", Job::Plan(Request::Standardize));
    }

    /// `C`: every missing cover in the library, including ones that failed before.
    pub(super) fn fetch_all_covers(&mut self) {
        if self.lib_ctx().is_none() || self.covers_busy() {
            return;
        }
        self.start_cover_fetch(None, None);
    }

    pub(super) fn fetch_covers(&mut self, whole_platform: bool) {
        let Some(game) = self.selected_game() else {
            if self.selected_orphan().is_some() {
                self.notice(
                    "Not in the library yet: import it first (i); covers live in the library.",
                );
            } else {
                self.notice("No game selected.");
            }
            return;
        };
        if !whole_platform && game.cover.is_some() {
            self.notice("This game already has a cover.");
            return;
        }
        let platform = game.game.platform.clone();
        if pocketeer_core::catalog::thumbnail_system(&platform).is_none() {
            self.input(
                format!("libretro system for {platform}"),
                vec![
                    "No built-in mapping for this platform. Give the libretro-thumbnails".into(),
                    "repository name, e.g. \"Nintendo - Game Boy Advance\".".into(),
                ],
                String::new(),
                InputKind::CoverSystem { platform },
            );
            return;
        }
        if self.covers_busy() {
            return;
        }
        self.start_cover_fetch(Some(platform), None);
    }

    pub(super) fn start_cover_fetch(&mut self, platform: Option<String>, system: Option<String>) {
        self.busy.side = Some("fetching covers".into());
        self.submit_side(Job::FetchCovers {
            platform,
            system,
            quiet: false,
        });
    }

    /// One cover fetch at a time; says so instead of queueing another.
    pub(super) fn covers_busy(&mut self) -> bool {
        if self.busy.side.is_some() {
            self.notice("Covers are being fetched right now; wait for the status line to clear.");
            return true;
        }
        false
    }

    pub(super) fn rename_selected(&mut self) {
        if let Some(rom) = self.selected_orphan().cloned() {
            self.choose_rename(RenameChoice::CardCustom {
                path: rom.rel_path,
                name: rom.name,
            });
            return;
        }
        let Some(game) = self.selected_game() else {
            return;
        };
        let Some(rom) = game.game.roms.first() else {
            return;
        };
        let platform = game.game.platform.clone();
        if game.game.roms.len() > 1 {
            let roms: Vec<String> = game.game.roms.iter().map(|r| r.name.clone()).collect();
            self.modal = Some(Modal::Pick {
                title: format!("{} has {} ROMs; which one?", game.game.name, roms.len()),
                items: roms.clone(),
                selected: 0,
                kind: PickKind::RenameRom { platform, roms },
            });
            return;
        }
        self.rename_options(platform, rom.name.clone());
    }

    pub(super) fn rename_prompt(&mut self, platform: String, name: String, single: bool) {
        self.input(
            format!("Rename {name}"),
            vec![
                "New file name; leave the extension out to keep it.".into(),
                if single {
                    "The local game title and library file follow.".into()
                } else {
                    "The local library file changes; the multi-ROM game keeps its title.".into()
                },
            ],
            pocketeer_core::fsutil::names::stem(&name).to_string(),
            InputKind::Rename {
                platform,
                spec: name,
            },
        );
    }

    /// `x` on the Games tab: library game → .trash; card-only ROM → deleted from the card.
    pub(super) fn remove_selected_game(&mut self) {
        if let Some(o) = self.selected_orphan().cloned() {
            self.confirm(
                format!("Delete {} from the card?", o.name),
                vec![
                    format!("  - {}  ({})", o.rel_path.display(), text::bytes(o.size)),
                    String::new(),
                    "This deletes the card file without creating a backup.".into(),
                    "Import it first (i) if you want to keep a local copy.".into(),
                ],
                Pending::Files {
                    label: format!("delete {}", o.name),
                    actions: vec![install::Action::Remove {
                        path: o.rel_path.clone(),
                        reason: install::plan::RemoveReason::Uninstall,
                    }],
                },
            );
            return;
        }
        let Some(game) = self.selected_game() else {
            return;
        };
        let platform = game.game.platform.clone();
        let specs: Vec<String> = game.game.roms.iter().map(|r| r.id.to_string()).collect();
        match library_cmd::plan_remove(&self.ctx(), Some(&platform), &specs) {
            Err(e) => self.error("Can't remove", &e),
            Ok(plan) => self.planned(Pending::LibraryRemove(Box::new(plan))),
        }
    }

    pub(super) fn import_orphans(&mut self, all: bool) {
        if self.card().is_none() || self.lib_ctx().is_none() {
            return;
        }
        let card_roms: Vec<CardRom> = self
            .card_view()
            .map(|s| s.card_roms.clone())
            .unwrap_or_default();
        if card_roms.is_empty() {
            self.notice("Every ROM on the card is already in the library.");
            return;
        }
        let args = if all {
            CardImportRequest::default()
        } else {
            match self.selected_orphan() {
                Some(o) => CardImportRequest {
                    platform: Some(o.platform.clone()),
                    paths: vec![o.rel_path.clone()],
                    ..Default::default()
                },
                None => {
                    self.notice(
                        "Select a row whose WHERE says \"card\" and press i, or I imports all of them.",
                    );
                    return;
                }
            }
        };
        self.submit("Planning import", Job::Plan(Request::CardImport(args)));
    }
}

impl App {
    pub(super) fn rename_options(&mut self, platform: String, name: String) {
        let copies = self
            .library_view()
            .into_iter()
            .flat_map(|l| &l.games)
            .filter(|g| g.game.platform == platform)
            .flat_map(|g| &g.matches)
            .find(|m| m.name == name)
            .map(|m| m.copies.clone())
            .unwrap_or_default();
        let mut items = vec![format!("Rename local ROM: {name}")];
        let mut actions = vec![RenameChoice::LocalCustom {
            platform: platform.clone(),
            name: name.clone(),
        }];
        for copy in copies {
            if copy.name_mismatch {
                items.push(format!(
                    "AP ← local name: {} → {}",
                    copy.path.display(),
                    name
                ));
                actions.push(RenameChoice::CardAdopt {
                    path: copy.path.clone(),
                    new_name: name.clone(),
                });
                items.push(format!("Local ← AP name: {}", copy.path.display()));
                actions.push(RenameChoice::LocalAdopt {
                    platform: platform.clone(),
                    spec: name.clone(),
                    new_name: copy.name.clone(),
                });
            }
            items.push(format!("Rename AP ROM: {}", copy.path.display()));
            actions.push(RenameChoice::CardCustom {
                path: copy.path,
                name: copy.name,
            });
        }
        self.modal = Some(Modal::Pick {
            title: "Choose which name to change".into(),
            items,
            selected: 0,
            kind: PickKind::RenameActions(actions),
        });
    }

    pub(super) fn choose_rename(&mut self, choice: RenameChoice) {
        match choice {
            RenameChoice::LocalCustom { platform, name } => {
                let single = self.library_view().into_iter().flat_map(|l| &l.games)
                    .find(|g| g.game.platform == platform && g.game.roms.iter().any(|r| r.name == name))
                    .is_some_and(|g| g.game.roms.len() == 1);
                self.rename_prompt(platform, name, single);
            },
            RenameChoice::CardCustom { path, name } => self.input("Rename AP ROM", vec![format!("AP path: {}", path.display()), "New filename; omit the extension to keep it. Changes are previewed before applying.".into()], pocketeer_core::fsutil::names::stem(&name).to_string(), InputKind::CardRename { path }),
            RenameChoice::LocalAdopt { platform, spec, new_name } => self.submit("Planning local rename", Job::Plan(Request::Rename { platform, spec, new_name })),
            RenameChoice::CardAdopt { path, new_name } => self.submit("Planning AP rename", Job::Plan(Request::CardRename { path, new_name })),
        }
    }
}

impl App {
    /// The first save conflict in `plan`: which copy wins. The answer comes
    /// back through `submit_pick`, which re-plans the rest.
    pub(super) fn ask_save_conflict(
        &mut self,
        plan: Box<pocketeer_core::transfer::saves::SavesPlan>,
        index: usize,
    ) {
        use pocketeer_core::transfer::saves::SaveAction;
        let SaveAction::Conflict {
            rel,
            card,
            computer,
            ..
        } = &plan.actions[index]
        else {
            return;
        };
        self.modal = Some(Modal::Pick {
            title: format!("Save conflict: {}", rel.display()),
            items: vec![
                format!(
                    "Use AP → backup ({}; {})",
                    text::bytes(card.size),
                    text::time_ago(card.mtime)
                ),
                format!(
                    "Use computer → restore AP ({}; {})",
                    text::bytes(computer.size),
                    text::time_ago(computer.mtime)
                ),
                "Leave both unchanged".into(),
            ],
            selected: 2,
            kind: PickKind::SaveConflict { plan, index },
        });
    }
}
