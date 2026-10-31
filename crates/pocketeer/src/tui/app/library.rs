//! Games tab actions: library, device, covers, rename, remove, import.

use pocketeer_core::{
    install,
    library::{Library, covers},
};

use super::*;
use crate::{cli::DeviceImportArgs, commands::library as library_cmd};

impl App {
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
        let home = dirs::home_dir()
            .map(|h| format!("{}/", h.display()))
            .unwrap_or_default();
        self.input(
            "Import ROMs  1/3: what",
            vec![
                "A folder of ROMs on this computer, or one ROM / zip file.".into(),
                "Matching entries are listed as you type; ↓ picks one, Tab completes.".into(),
            ],
            home,
            InputKind::ImportDir,
        );
    }

    /// `D`: register the card, or edit its subscriptions.
    pub(super) fn device_dialog(&mut self) {
        if self.card().is_none() {
            return;
        }
        let Some(view) = self.library_view() else {
            self.no_library();
            return;
        };
        match view.device.clone() {
            None => {
                self.input(
                    "Register this card",
                    vec![
                        "Name for this card, e.g. \"Pocket A\". A marker file on the card".into(),
                        "makes it recognisable wherever it's mounted.".into(),
                    ],
                    String::new(),
                    InputKind::DeviceName,
                );
            }
            Some(d) => {
                let platforms = view.platforms.clone();
                let cores: Vec<String> = self
                    .card_view()
                    .map(|s| s.cores.cores.iter().map(|c| c.core.id.clone()).collect())
                    .unwrap_or_default();
                let mut items: Vec<(String, bool)> = platforms
                    .iter()
                    .map(|p| (format!("platform  {p}"), d.platforms.contains(p)))
                    .collect();
                items.extend(
                    cores
                        .iter()
                        .map(|c| (format!("core      {c}"), d.cores.contains(c))),
                );
                if items.is_empty() {
                    self.notice(
                        "Nothing to subscribe to yet: import ROMs (a) or install cores first.",
                    );
                    return;
                }
                self.modal = Some(Modal::Toggle {
                    title: format!("{}: what this card gets on sync", d.name),
                    items,
                    cursor: 0,
                    kind: ToggleKind::Subscribe {
                        device_id: d.id,
                        platforms,
                        cores,
                    },
                });
            }
        }
    }

    pub(super) fn plan_device_sync(&mut self) {
        if self.card().is_none() {
            return;
        }
        let Some(device) = self.library_view().and_then(|l| l.device.clone()) else {
            self.device_dialog();
            return;
        };
        if device.platforms.is_empty() && device.cores.is_empty() {
            self.device_dialog();
            return;
        }
        self.submit("Planning device sync", Job::Plan(Request::DeviceSync));
    }

    pub(super) fn plan_saves(&mut self) {
        if self.card().is_some() && self.lib_ctx().is_some() {
            self.submit("Comparing saves", Job::Plan(Request::Saves));
        }
    }

    /// Once per session (and after imports): fetch covers nobody has tried yet.
    pub(super) fn auto_fetch_covers(&mut self) {
        if self.auto_covers_done || !self.online() || self.side_status.is_some() {
            return;
        }
        self.auto_covers_done = true;
        let wanted = self
            .library_view()
            .is_some_and(|l| l.games.iter().any(|g| g.cover.is_none()));
        if !wanted {
            return;
        }
        self.side_status = Some("fetching missing covers".into());
        self.submit_side(Job::FetchCovers {
            platform: None,
            system: None,
            quiet: true,
        });
    }

    /// `t`: offer standard (No-Intro) names for games whose file names aren't.
    pub(super) fn standardize(&mut self) {
        if self.lib_ctx().is_none() {
            return;
        }
        if !self.online() {
            self.notice("Standard names need the libretro name list: start without --offline.");
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
        if covers::system_for(&platform).is_none() {
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
        self.side_status = Some("fetching covers".into());
        self.submit_side(Job::FetchCovers {
            platform,
            system,
            quiet: false,
        });
    }

    /// One cover fetch at a time; says so instead of queueing another.
    pub(super) fn covers_busy(&mut self) -> bool {
        if self.side_status.is_some() {
            self.notice("Covers are being fetched right now; wait for the status line to clear.");
            return true;
        }
        false
    }

    pub(super) fn rename_selected(&mut self) {
        let Some(game) = self.selected_game() else {
            if self.selected_orphan().is_some() {
                self.notice("Not in the library yet: import it first (i), then rename it.");
            }
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
        self.rename_prompt(platform, rom.name.clone(), true);
    }

    pub(super) fn rename_prompt(&mut self, platform: String, name: String, single: bool) {
        self.input(format!("Rename {name}"), vec![
                "New file name; leave the extension out to keep it.".into(),
                if single {
                    "The game name, the library file, and the card's copy with its saves all follow."
                        .into()
                } else {
                    "The library file and the card's copy with its saves follow; the game keeps its name."
                        .into()
                },
            ], pocketeer_core::roms::rename::stem(&name).to_string(), InputKind::Rename {
                platform,
                spec: name,
            });
    }

    /// `x` on the Games tab: library game → .trash; card-only ROM → deleted from the card.
    pub(super) fn remove_selected_game(&mut self) {
        if let Some(o) = self.selected_orphan().cloned() {
            self.confirm(
                format!("Delete {} from the card?", o.name),
                vec![
                    format!("  - {}  ({})", o.rel_path.display(), human::bytes(o.size)),
                    String::new(),
                    "This ROM isn't in the library, so nothing keeps a copy: the file is".into(),
                    "deleted from the card. Import it first (i) if you want to keep it.".into(),
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
        let name = game.game.name.clone();
        match library_cmd::plan_remove(&self.ctx(), Some(&platform), &specs) {
            Err(e) => self.error("Can't remove", &e),
            Ok(plan) => {
                let lines =
                    text_lines(&library_cmd::library_remove(&Staged::preview(plan.clone())));
                self.confirm(
                    format!("Remove {name} from the library?"),
                    lines,
                    Pending::LibraryRemove(Box::new(plan)),
                );
            }
        }
    }

    pub(super) fn import_orphans(&mut self, all: bool) {
        if self.card().is_none() || self.lib_ctx().is_none() {
            return;
        }
        let card_roms: Vec<CardOnlyRom> = self
            .card_view()
            .map(|s| s.card_roms.clone())
            .unwrap_or_default();
        if card_roms.is_empty() {
            self.notice("Every ROM on the card is already in the library.");
            return;
        }
        let args = if all {
            DeviceImportArgs::default()
        } else {
            match self.selected_orphan() {
                Some(o) => DeviceImportArgs {
                    platform: Some(o.platform.clone()),
                    names: vec![o.name.clone()],
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
