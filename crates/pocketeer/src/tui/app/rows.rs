//! Table rows: which pane is current, how rows are computed from the
//! snapshot and the filters, and what is selected.

use super::*;

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

    /// Recomputes every table from the snapshot and the current filters.
    pub fn rebuild_rows(&mut self) {
        let rows = self.compute_core_rows();
        self.cores.set_rows(rows);
        let rows = self.compute_game_rows();
        self.games.set_rows(rows);
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
                let present = matches!(f.status, required::FileStatus::Present);
                if self.bios.filter == BiosFilter::Problems && present {
                    continue;
                }
                if pane::matches(q.as_deref(), &[&core.core_id, &f.path.to_string_lossy()]) {
                    rows.push((ci, fi));
                }
            }
        }
        rows
    }

    /// The selected required file and the core that wants it.
    pub fn selected_bios(&self) -> Option<(&required::CoreRequirements, &required::RequiredFile)> {
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
            pane::matches(q.as_deref(), &fields)
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
                    GameFilter::OnCard => g.on_card,
                    GameFilter::NotOnCard => !g.on_card,
                    GameFilter::CardOnly => false,
                };
                if keep && pane::matches(q.as_deref(), &[&g.game.name, &g.game.platform]) {
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
                if pane::matches(q.as_deref(), &[&o.name, &o.platform]) {
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

    pub fn card_rom(&self, r: GameRef) -> Option<&CardOnlyRom> {
        match r {
            GameRef::CardOnly(i) => self.card_view()?.card_roms.get(i),
            GameRef::Library(_) => None,
        }
    }

    pub fn selected_game(&self) -> Option<&GameRow> {
        self.games.selected().and_then(|r| self.game_row(*r))
    }

    pub fn selected_orphan(&self) -> Option<&CardOnlyRom> {
        self.games.selected().and_then(|r| self.card_rom(*r))
    }

    /// Required-file problems for one core, from the last snapshot.
    pub fn bios_for(&self, core_id: &str) -> Vec<&required::RequiredFile> {
        self.card_view()
            .map(|s| {
                s.bios
                    .cores
                    .iter()
                    .filter(|c| c.core_id == core_id)
                    .flat_map(|c| c.files.iter())
                    .filter(|f| !matches!(f.status, required::FileStatus::Present))
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
