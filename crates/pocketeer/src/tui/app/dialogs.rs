//! What happens when an input / pick / toggle dialog is submitted, and the
//! small text helpers those dialogs need.

use super::path_input::expand_home;
use pocketeer_core::library::import::apply_suggestions;

use super::*;
use crate::ops::library::ImportRequest;

/// The platform-id step of a folder import: known ids to pick from, plus
/// this entry for typing another.
const OTHER_PLATFORM: &str = "another platform id…";

impl App {
    // -----------------------------------------------------------------------
    // modal submissions

    pub(super) fn submit_input(&mut self, kind: InputKind, value: String) {
        match kind {
            InputKind::ImportCover { game_id } => {
                if value.trim().is_empty() {
                    return;
                }
                self.submit(
                    "Checking cover image",
                    Job::Plan(Request::ImportCover {
                        game_id,
                        source: PathBuf::from(expand_home(value.trim())),
                    }),
                );
            }
            InputKind::CardRename { path } => self.submit(
                "Planning AP rename",
                Job::Plan(Request::CardRename {
                    path,
                    new_name: value,
                }),
            ),
            InputKind::Rename { platform, spec } => {
                if value.trim().is_empty() {
                    return;
                }
                self.submit(
                    "Planning rename",
                    Job::Plan(Request::Rename {
                        platform,
                        spec,
                        new_name: value,
                    }),
                );
            }
            InputKind::ImportDir => {
                let dir = PathBuf::from(expand_home(value.trim()));
                if !dir.is_dir() && !dir.is_file() {
                    self.notice(format!("{} is not a folder or file.", dir.display()));
                    self.import_folder_with(value);
                    return;
                }
                let mut platforms = self.known_platforms();
                // A file's extension, or the folder name, is a strong hint
                // when it is a known platform id.
                let guess = [
                    dir.is_file().then(|| dir.extension()).flatten(),
                    if dir.is_file() {
                        dir.parent().and_then(|d| d.file_name())
                    } else {
                        dir.file_name()
                    },
                ]
                .into_iter()
                .flatten()
                .map(|n| n.to_string_lossy().to_lowercase())
                .find(|n| platforms.contains(n));
                if let Some(g) = &guess {
                    platforms.retain(|p| p != g);
                    platforms.insert(0, g.clone());
                }
                if platforms.is_empty() {
                    self.ask_platform(dir, String::new());
                    return;
                }
                let mut items = platforms.clone();
                items.push(OTHER_PLATFORM.into());
                self.modal = Some(Modal::Pick {
                    title: "Import ROMs  2/3: platform".into(),
                    items,
                    selected: 0,
                    kind: PickKind::ImportPlatform { dir, platforms },
                });
            }
            InputKind::ImportPlatform { dir } => {
                let platform = value.trim().to_lowercase();
                if platform.is_empty() || platform.contains(['/', ' ']) {
                    self.notice("A platform id looks like gba or snes: letters and digits only.");
                    self.ask_platform(dir, String::new());
                    return;
                }
                self.ask_dat(dir, platform);
            }
            InputKind::ImportDat { dir, platform } => {
                let dat = {
                    let v = value.trim();
                    (!v.is_empty()).then(|| PathBuf::from(expand_home(v)))
                };
                if let Some(d) = &dat
                    && !d.is_file()
                {
                    self.notice(format!("{} is not a file.", d.display()));
                    self.ask_dat(dir, platform);
                    return;
                }
                self.submit(
                    "Planning import",
                    Job::Plan(Request::Import(ImportRequest {
                        source: dir,
                        platform,
                        dat,
                        dat_only: false,
                        standard_names: false,
                        covers: true,
                    })),
                );
            }
            InputKind::CoverSystem { platform } => {
                let system = value.trim().to_string();
                if system.is_empty() || self.covers_busy() {
                    return;
                }
                self.start_cover_fetch(Some(platform), Some(system));
            }
        }
    }

    /// Step 1 again, keeping what was typed.
    fn import_folder_with(&mut self, value: String) {
        self.import_folder();
        if let Some(Modal::Input { value: v, .. }) = &mut self.modal {
            *v = value;
        }
        self.refresh_input();
    }

    /// Step 2 as a free-text prompt.
    fn ask_platform(&mut self, dir: PathBuf, value: String) {
        self.input(
            "Import ROMs  2/3: platform",
            vec![
                format!("Folder: {}", dir.display()),
                "Platform id these ROMs belong to, e.g. gba, gb, snes:".into(),
            ],
            value,
            InputKind::ImportPlatform { dir },
        );
    }

    /// Step 3: optional DAT.
    fn ask_dat(&mut self, dir: PathBuf, platform: String) {
        self.input(
            "Import ROMs  3/3: DAT",
            vec![
                format!("Folder: {}   Platform: {platform}", dir.display()),
                "Optional custom DAT; leave empty for automatic identification after import."
                    .into(),
                "Matching entries are listed as you type; ↓ picks one, Tab completes.".into(),
            ],
            String::new(),
            InputKind::ImportDat { dir, platform },
        );
    }

    pub(super) fn submit_pick(&mut self, kind: PickKind, selected: usize) {
        match kind {
            PickKind::SaveConflict { mut plan, index } => {
                use pocketeer_core::transfer::saves::ConflictChoice;
                let choice = match selected {
                    0 => ConflictChoice::Card,
                    1 => ConflictChoice::Computer,
                    _ => ConflictChoice::Skip,
                };
                plan.resolve(index, choice);
                self.review_saves(plan);
            }
            PickKind::RenameActions(mut actions) => {
                if selected < actions.len() {
                    self.choose_rename(actions.remove(selected));
                }
            }
            PickKind::RenameRom { platform, roms } => {
                if let Some(name) = roms.get(selected) {
                    self.rename_options(platform, name.clone());
                }
            }
            PickKind::ImportPlatform { dir, platforms } => match platforms.get(selected) {
                Some(p) => self.ask_dat(dir, p.clone()),
                None => self.ask_platform(dir, String::new()),
            },
        }
    }

    pub(super) fn submit_toggle(&mut self, kind: ToggleKind, items: Vec<(String, bool)>) {
        match kind {
            ToggleKind::ImportNames { import, keys } => {
                let accepted: std::collections::HashSet<(String, String)> = keys
                    .into_iter()
                    .zip(items.iter())
                    .filter(|(_, (_, on))| *on)
                    .map(|(k, _)| k)
                    .collect();
                let mut import = *import;
                let applied = self.ctx().library().and_then(|lib| match &mut import {
                    Pending::Import(b) => {
                        let platform = b.0.platform.clone();
                        apply_suggestions(&lib, &mut b.1, |name| {
                            accepted.contains(&(platform.clone(), name.to_string()))
                        })
                    }
                    Pending::CardImport(c) => {
                        let mut n = 0;
                        for part in &mut c.parts {
                            let platform = part.platform.clone();
                            n += apply_suggestions(&lib, &mut part.plan, |name| {
                                accepted.contains(&(platform.clone(), name.to_string()))
                            })?;
                        }
                        Ok(n)
                    }
                    _ => Ok(0),
                });
                if let Err(e) = applied {
                    self.error("Standard names", &e);
                    return;
                }
                self.planned(import);
            }
            ToggleKind::Standardize { plans } => {
                let chosen: Vec<_> = plans
                    .into_iter()
                    .zip(items.iter())
                    .filter(|(_, (_, on))| *on)
                    .map(|(p, _)| p)
                    .collect();
                if chosen.is_empty() {
                    return;
                }
                let lines = text_lines(&text::library::library_standardize(&Staged::preview(
                    StandardizePlan {
                        renames: chosen.clone(),
                    },
                )));
                self.confirm(
                    format!("Rename {} game(s) to standard names?", chosen.len()),
                    lines,
                    Pending::Standardize(chosen),
                );
            }
        }
    }

    /// Recomputes a path prompt's suggestion list from its current value.
    pub(super) fn refresh_input(&mut self) {
        if let Some(Modal::Input {
            value,
            kind,
            suggestions,
            highlight,
            completion,
            searching,
            tab_pending,
            ..
        }) = &mut self.modal
            && kind.completes_paths()
        {
            suggestions.clear();
            *highlight = None;
            *completion = None;
            *tab_pending = false;
            *searching = true;
            self.path_input.request(value.clone());
        }
    }

    /// Platform ids the library holds or an installed core handles, sorted.
    fn known_platforms(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .library_view()
            .map(|l| l.platforms.clone())
            .unwrap_or_default();
        if let Some(s) = self.card_view() {
            for c in &s.cores.cores {
                ids.extend(c.core.platform_ids.iter().cloned());
            }
        }
        ids.sort();
        ids.dedup();
        ids
    }
}
