//! Table rows: which pane is current, how rows are computed from the
//! snapshot and the filters, and what is selected.

use super::*;

pub(super) enum GameSelection {
    Library(i64),
    Card(PathBuf),
}

impl App {
    // -----------------------------------------------------------------------
    // rows

    /// The table of the current tab (none on the Device tab).
    pub fn pane(&self) -> Option<&dyn PaneOps> {
        match self.tab {
            Tab::Cores => Some(&self.cores),
            Tab::Bios => Some(&self.bios),
            Tab::Games => Some(&self.games),
            Tab::Device => None,
        }
    }

    pub(super) fn pane_mut(&mut self) -> Option<&mut dyn PaneOps> {
        match self.tab {
            Tab::Cores => Some(&mut self.cores),
            Tab::Bios => Some(&mut self.bios),
            Tab::Games => Some(&mut self.games),
            Tab::Device => None,
        }
    }

    /// The card's part of the snapshot, if a card is mounted and loaded.
    pub fn card_view(&self) -> Option<&CardSnapshot> {
        self.snapshot.as_ref()?.card.as_ref()
    }

    pub(super) fn installed_list(&self) -> CoreList {
        CoreList {
            cores: self
                .card_view()
                .map(|s| s.cores.cores.iter().map(|c| c.core.clone()).collect())
                .unwrap_or_default(),
            broken: Vec::new(),
        }
    }

    pub(super) fn game_selection(&self) -> Option<GameSelection> {
        match *self.games.selected()? {
            GameRef::Library(i) => Some(GameSelection::Library(
                self.library_view()?.games.get(i)?.game.id,
            )),
            GameRef::CardOnly(i) => Some(GameSelection::Card(
                self.card_view()?.card_roms.get(i)?.rel_path.clone(),
            )),
        }
    }

    /// Recomputes every table from the snapshot and the current filters.
    pub fn rebuild_rows(&mut self) {
        self.rebuild_rows_preserving(self.game_selection());
    }

    pub(super) fn rebuild_rows_preserving(&mut self, selected: Option<GameSelection>) {
        let rows = self.compute_core_rows();
        self.cores.set_rows(rows);
        let rows = self.compute_game_rows();
        let selected_index = rows.iter().position(|row| match &selected {
            Some(GameSelection::Library(id)) => {
                self.game_row(*row).is_some_and(|g| g.game.id == *id)
            }
            Some(GameSelection::Card(path)) => {
                self.card_rom(*row).is_some_and(|r| r.rel_path == *path)
                    || self.game_row(*row).is_some_and(|g| {
                        g.matches
                            .iter()
                            .flat_map(|m| &m.copies)
                            .any(|c| c.path == *path)
                    })
            }
            None => false,
        });
        self.games.set_rows(rows);
        if let Some(i) = selected_index {
            self.games.sel = i;
        }
        let rows = self.compute_bios_rows();
        self.bios.set_rows(rows);
    }

    fn compute_bios_rows(&self) -> Vec<(usize, usize)> {
        let Some(snap) = self.card_view() else {
            return Vec::new();
        };
        let q = self.bios.needle();
        let mut rows = Vec::new();
        for (ci, core) in snap.bios.cores.iter().enumerate() {
            for (fi, f) in core.files.iter().enumerate() {
                let present = matches!(f.status, bios::FileStatus::Present);
                if self.bios.filter == BiosFilter::Problems && present {
                    continue;
                }
                if matches(q.as_deref(), &[&core.core_id, &f.path.to_string_lossy()]) {
                    rows.push((ci, fi));
                }
            }
        }
        rows
    }

    /// The selected required file and the core that wants it.
    pub fn selected_bios(&self) -> Option<(&bios::CoreRequirements, &bios::RequiredFile)> {
        let (ci, fi) = *self.bios.selected()?;
        let core = self.card_view()?.bios.cores.get(ci)?;
        Some((core, core.files.get(fi)?))
    }

    fn compute_core_rows(&self) -> Vec<CoreRow> {
        let Some(snap) = self.card_view() else {
            return Vec::new();
        };
        let q = self.cores.needle();
        let hit = |id: &str, platforms: &[String]| {
            let mut fields: Vec<&str> = vec![id];
            fields.extend(platforms.iter().map(String::as_str));
            matches(q.as_deref(), &fields)
        };
        let filter = self.cores.filter;
        let mut rows: Vec<CoreRow> = Vec::new();
        if filter != CoreFilter::Available {
            for c in &snap.cores.cores {
                if !hit(&c.core.id, &c.core.platform_ids) {
                    continue;
                }
                if filter == CoreFilter::Updates && !c.update_available {
                    continue;
                }
                rows.push(CoreRow {
                    id: c.core.id.clone(),
                    platforms: c.core.platform_ids.clone(),
                    installed: Some(c.core.version.clone()),
                    latest: c.latest.as_ref().map(|l| l.version.clone()),
                    update: c.update_available,
                    in_inventory: c.in_inventory,
                    license: c.latest.as_ref().is_some_and(|l| l.requires_license),
                    missing_files: self.bios_for(&c.core.id).len(),
                    date: c.latest.as_ref().and_then(|l| l.date_release.clone()),
                    repo: snap
                        .inventory
                        .as_ref()
                        .and_then(|inv| inv.get(&c.core.id).map(|e| e.repo_url())),
                    description: c.core.description.clone(),
                });
            }
        }
        if matches!(filter, CoreFilter::All | CoreFilter::Available)
            && let Some(inv) = &snap.inventory
        {
            let installed = self.installed_list();
            for h in cores::search(
                inv,
                &installed,
                &SearchFilter {
                    query: None,
                    platform: None,
                    include_installed: false,
                    include_unreleased: false,
                },
            ) {
                if !hit(&h.id, &h.platform_ids) {
                    continue;
                }
                rows.push(CoreRow {
                    id: h.id.clone(),
                    platforms: h.platform_ids.clone(),
                    installed: None,
                    latest: h.version.clone(),
                    update: false,
                    in_inventory: true,
                    license: h.requires_license,
                    missing_files: 0,
                    date: h.date_release.clone(),
                    repo: Some(h.repo_url.clone()),
                    description: None,
                });
            }
        }
        rows.sort_by_key(|r| (r.installed.is_none(), r.id.to_lowercase()));
        rows
    }

    fn compute_game_rows(&self) -> Vec<GameRef> {
        let Some(snap) = &self.snapshot else {
            return Vec::new();
        };
        let q = self.games.needle();
        let f = self.games.filter;
        let mut rows: Vec<(String, String, GameRef)> = Vec::new();
        if let Some(l) = &snap.library {
            for (i, g) in l.games.iter().enumerate() {
                let keep = match f {
                    GameFilter::All | GameFilter::InLibrary => true,
                    GameFilter::OnCard => g.on_card(),
                    GameFilter::NotOnCard => g
                        .matches
                        .iter()
                        .all(|m| m.presence == pocketeer_core::transfer::Presence::Local),
                    GameFilter::CardOnly => false,
                };
                if keep && matches(q.as_deref(), &[&g.game.name, &g.game.platform]) {
                    rows.push((
                        g.game.platform.to_lowercase(),
                        g.game.name.to_lowercase(),
                        GameRef::Library(i),
                    ));
                }
            }
        }
        if !matches!(f, GameFilter::InLibrary | GameFilter::NotOnCard)
            && let Some(card) = &snap.card
        {
            for (i, o) in card.card_roms.iter().enumerate() {
                let presence = card.card_presence[i];
                if presence == pocketeer_core::transfer::Presence::Both {
                    continue;
                }
                if f == GameFilter::CardOnly && presence != pocketeer_core::transfer::Presence::Card
                {
                    continue;
                }
                if matches(q.as_deref(), &[&o.name, &o.platform]) {
                    rows.push((
                        o.platform.to_lowercase(),
                        o.name.to_lowercase(),
                        GameRef::CardOnly(i),
                    ));
                }
            }
        }
        rows.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
        rows.into_iter().map(|(_, _, r)| r).collect()
    }

    pub fn selected_core(&self) -> Option<&CoreRow> {
        self.cores.selected()
    }

    pub fn game_row(&self, r: GameRef) -> Option<&GameRow> {
        match r {
            GameRef::Library(i) => self.library_view()?.games.get(i),
            GameRef::CardOnly(_) => None,
        }
    }

    pub fn card_rom(&self, r: GameRef) -> Option<&CardRom> {
        match r {
            GameRef::CardOnly(i) => self.card_view()?.card_roms.get(i),
            GameRef::Library(_) => None,
        }
    }

    pub fn selected_game(&self) -> Option<&GameRow> {
        self.games.selected().and_then(|r| self.game_row(*r))
    }

    pub fn selected_orphan(&self) -> Option<&CardRom> {
        self.games.selected().and_then(|r| self.card_rom(*r))
    }

    /// Required-file problems for one core, from the last snapshot.
    pub fn bios_for(&self, core_id: &str) -> Vec<&bios::RequiredFile> {
        self.card_view()
            .map(|s| {
                s.bios
                    .cores
                    .iter()
                    .filter(|c| c.core_id == core_id)
                    .flat_map(|c| c.files.iter())
                    .filter(|f| !matches!(f.status, bios::FileStatus::Present))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Decoded cover for a game, cached per path.
    pub fn cover_for(&mut self, path: &Path) -> Option<&mut StatefulProtocol> {
        if !self.covers.contains_key(path) {
            if self.covers.len() > 24 {
                self.covers.clear();
            }
            let img = image::ImageReader::open(path)
                .ok()?
                .with_guessed_format()
                .ok()?
                .decode()
                .ok()?;
            let proto = self.picker.new_resize_protocol(img);
            self.covers.insert(path.to_path_buf(), proto);
        }
        self.covers.get_mut(path)
    }

    pub fn library_view(&self) -> Option<&LibraryView> {
        self.snapshot.as_ref()?.library.as_ref()
    }

    pub fn library_path_display(&self) -> String {
        self.library_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(no path configured)".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pocketeer_core::{
        card::roms::Confidence,
        library::db::Game,
        transfer::{CardMatch, LocalMatch, Presence},
    };

    fn game(id: i64, name: &str) -> GameRow {
        GameRow {
            game: Game {
                id,
                platform: "gba".into(),
                name: name.into(),
                roms: Vec::new(),
            },
            cover: None,
            matches: Vec::new(),
        }
    }

    #[test]
    fn refresh_keeps_the_selected_game_across_insertion_and_card_matching() {
        let tmp = tempfile::tempdir().unwrap();
        let global = Global {
            json: false,
            root: Some(tmp.path().join("missing-card")),
            offline: true,
            refresh: false,
            cache_dir: Some(tmp.path().join("cache")),
            library: Some(tmp.path().join("library")),
            config: Some(tmp.path().join("config")),
            github_token: None,
            ..Default::default()
        };
        let mut app = App::new(global, Picker::halfblocks());
        app.snapshot = Some(Snapshot {
            fingerprint: 0,
            card: None,
            verification: None,
            library: Some(LibraryView {
                root: tmp.path().join("library"),
                games: vec![game(7, "Middle"), game(9, "Zulu")],
                saves: 0,
                platforms: vec!["gba".into()],
            }),
        });
        app.rebuild_rows();
        app.games.sel = 1;
        let selected = app.game_selection();
        app.snapshot
            .as_mut()
            .unwrap()
            .library
            .as_mut()
            .unwrap()
            .games
            .insert(0, game(10, "Alpha"));
        app.rebuild_rows_preserving(selected);
        assert_eq!(app.selected_game().unwrap().game.id, 9);

        let path = PathBuf::from("Assets/gba/common/nested/card-name.gba");
        let selected = Some(GameSelection::Card(path.clone()));
        app.snapshot
            .as_mut()
            .unwrap()
            .library
            .as_mut()
            .unwrap()
            .games[1]
            .matches
            .push(LocalMatch {
                rom_id: 11,
                platform: "gba".into(),
                name: "local-name.gba".into(),
                presence: Presence::Both,
                copies: vec![CardMatch {
                    path,
                    name: "card-name.gba".into(),
                    confidence: Confidence::Full,
                    name_mismatch: true,
                }],
            });
        app.rebuild_rows_preserving(selected);
        assert_eq!(app.selected_game().unwrap().game.id, 7);
    }
}
