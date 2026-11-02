//! The three tabs that are about the card alone: Cores (installed cores and
//! the inventory in one list), BIOS (the files installed cores need) and
//! Card (junk cleanup).

use crossterm::event::{KeyCode, KeyEvent};
use pocketeer_core::card::{
    bios, clean,
    cores::{self, CoreView, SearchFilter},
    install::{self, PlanOptions},
};

use super::{
    Cx,
    pane::{Cycle, Pane, PaneOps, matches},
    session::Session,
};
use crate::{
    ops::{Batch, cores as cores_cmd},
    tui::{
        pending::{self, Preview},
        worker::CardSnapshot,
    },
};

// ---------------------------------------------------------------------------
// Cores

/// Which cores to list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreFilter {
    Installed,
    All,
    Updates,
    Available,
}

impl Cycle for CoreFilter {
    const ALL: &'static [Self] = &[
        CoreFilter::Installed,
        CoreFilter::All,
        CoreFilter::Updates,
        CoreFilter::Available,
    ];

    fn label(self) -> &'static str {
        match self {
            CoreFilter::Installed => "installed",
            CoreFilter::All => "all",
            CoreFilter::Updates => "updates",
            CoreFilter::Available => "not installed",
        }
    }
}

pub struct Cores {
    pane: Pane<CoreFilter, CoreView>,
}

impl Cores {
    pub fn new() -> Self {
        Cores {
            pane: Pane::new(CoreFilter::Installed),
        }
    }

    pub fn pane(&self) -> &Pane<CoreFilter, CoreView> {
        &self.pane
    }

    pub fn pane_mut(&mut self) -> &mut dyn PaneOps {
        &mut self.pane
    }

    /// `/` on the Cores tab searches for cores you don't have yet, so it
    /// widens "installed" to all.
    pub fn start_search(&mut self) {
        if self.pane.filter() == CoreFilter::Installed {
            self.pane.set_filter(CoreFilter::All);
        }
        self.pane.start_editing();
    }

    pub fn rebuild(&mut self, s: &Session) {
        let rows = s.card().map(|c| self.compute_rows(c)).unwrap_or_default();
        self.pane.set_rows(rows, |_| false);
    }

    fn compute_rows(&self, snap: &CardSnapshot) -> Vec<CoreView> {
        let q = self.pane.needle();
        let hit = |c: &CoreView| {
            let mut fields: Vec<&str> = vec![&c.id];
            fields.extend(c.platform_ids.iter().map(String::as_str));
            matches(q.as_deref(), &fields)
        };
        let filter = self.pane.filter();
        let mut rows: Vec<CoreView> = Vec::new();
        if filter != CoreFilter::Available {
            rows.extend(
                snap.cores
                    .cores
                    .iter()
                    .filter(|c| filter != CoreFilter::Updates || c.update_available)
                    .filter(|c| hit(c))
                    .cloned(),
            );
        }
        if matches!(filter, CoreFilter::All | CoreFilter::Available)
            && let Some(inv) = &snap.inventory
        {
            let available = cores::search(
                inv,
                &snap.installed(),
                &SearchFilter {
                    query: None,
                    platform: None,
                    include_installed: false,
                    include_unreleased: false,
                },
            );
            rows.extend(available.into_iter().filter(|c| hit(c)));
        }
        rows.sort_by_key(|r| (r.installed.is_none(), r.id.to_lowercase()));
        rows
    }

    pub(super) fn key(&mut self, key: KeyEvent, cx: &mut Cx) {
        match key.code {
            KeyCode::Enter | KeyCode::Char('i') | KeyCode::Char('u') => self.install_or_update(cx),
            KeyCode::Char('U') => update_all(cx),
            KeyCode::Char('x') => self.remove(cx),
            _ => {}
        }
    }

    fn install_or_update(&self, cx: &mut Cx) {
        let Some(row) = self.pane.selected() else {
            return;
        };
        let id = row.id.clone();
        if row.installed.is_some() && !row.in_inventory {
            cx.session
                .notice(format!("{id} isn't in the inventory; nothing to update."));
            return;
        }
        if row.installed.is_some() && !row.update_available {
            cx.session
                .notice(format!("{id} is already at its latest release."));
            return;
        }
        prepare(cx, vec![id]);
    }

    fn remove(&self, cx: &mut Cx) {
        if cx.session.require_card().is_none() {
            return;
        }
        let Some(row) = self.pane.selected() else {
            return;
        };
        if row.installed.is_none() {
            cx.session.notice(format!("{} isn't on this card.", row.id));
            return;
        }
        let id = row.id.clone();
        let installed = cx
            .session
            .card()
            .map(CardSnapshot::installed)
            .unwrap_or_default();
        cx.session.plan("Planning removal", move |ctx, _| {
            let plan = install::plan_remove(&ctx.pocket()?, &installed, &id, false)?;
            if plan.actions.is_empty() {
                return Ok(Preview::Notice(format!("{id} has no folders on the card.")));
            }
            Ok(pending::preview(Batch::planned([(id, Ok(plan))])))
        });
    }
}

fn update_all(cx: &mut Cx) {
    let ids: Vec<String> = cx
        .session
        .card()
        .map(|s| {
            s.cores
                .cores
                .iter()
                .filter(|c| c.update_available)
                .map(|c| c.id.clone())
                .collect()
        })
        .unwrap_or_default();
    if ids.is_empty() {
        cx.session
            .notice("Every installed core is at its latest version.");
        return;
    }
    prepare(cx, ids);
}

/// Resolves releases for `specs` and downloads them on the worker; the
/// installs come back as one batch to confirm, failures listed.
fn prepare(cx: &mut Cx, specs: Vec<String>) {
    if cx.session.require_card().is_none() {
        return;
    }
    let Some(inventory) = cx.session.card().and_then(|s| s.inventory.clone()) else {
        cx.dialog.info(
            "Inventory unavailable",
            vec![
                "Can't resolve releases without the inventory.".into(),
                "Offline and nothing cached? Press r once online.".into(),
            ],
        );
        return;
    };
    cx.session.plan("Preparing", move |ctx, progress| {
        let plans = cores_cmd::prepare(ctx, &inventory, &specs, PlanOptions::default(), progress)?;
        Ok(pending::preview(Batch::planned(plans)))
    });
}

// ---------------------------------------------------------------------------
// BIOS

/// Which required files to list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BiosFilter {
    All,
    Problems,
}

impl Cycle for BiosFilter {
    const ALL: &'static [Self] = &[BiosFilter::All, BiosFilter::Problems];

    fn label(self) -> &'static str {
        match self {
            BiosFilter::All => "all",
            BiosFilter::Problems => "missing or wrong",
        }
    }
}

pub struct Bios {
    /// Rows are (core index, file index) into the card's BIOS report.
    pane: Pane<BiosFilter, (usize, usize)>,
}

impl Bios {
    pub fn new() -> Self {
        Bios {
            pane: Pane::new(BiosFilter::All),
        }
    }

    pub fn pane(&self) -> &Pane<BiosFilter, (usize, usize)> {
        &self.pane
    }

    pub fn pane_mut(&mut self) -> &mut dyn PaneOps {
        &mut self.pane
    }

    /// The selected required file and the core that wants it.
    pub fn selected<'s>(
        &self,
        s: &'s Session,
    ) -> Option<(&'s bios::CoreRequirements, &'s bios::RequiredFile)> {
        let (ci, fi) = *self.pane.selected()?;
        let core = s.card()?.bios.cores.get(ci)?;
        Some((core, core.files.get(fi)?))
    }

    pub fn rebuild(&mut self, s: &Session) {
        let mut rows = Vec::new();
        if let Some(snap) = s.card() {
            let q = self.pane.needle();
            for (ci, core) in snap.bios.cores.iter().enumerate() {
                for (fi, f) in core.files.iter().enumerate() {
                    let present = matches!(f.status, bios::FileStatus::Present);
                    if self.pane.filter() == BiosFilter::Problems && present {
                        continue;
                    }
                    if matches(q.as_deref(), &[&core.core_id, &f.path.to_string_lossy()]) {
                        rows.push((ci, fi));
                    }
                }
            }
        }
        self.pane.set_rows(rows, |_| false);
    }

    pub(super) fn key(&mut self, key: KeyEvent, cx: &mut Cx) {
        if key.code == KeyCode::Char('p') {
            place(cx);
        }
    }
}

/// `p`: move files dropped at the card root to where cores expect them.
fn place(cx: &mut Cx) {
    if cx.session.require_card().is_none() {
        return;
    }
    let Some(report) = cx.session.card().map(|s| s.bios.clone()) else {
        return;
    };
    cx.session.plan("Planning", move |ctx, _| {
        Ok(pending::preview(bios::plan_place(&ctx.pocket()?, &report)?))
    });
}

// ---------------------------------------------------------------------------
// Card

/// The Card tab's keys: `x` scans for macOS junk, then previews removal.
pub fn card_key(key: KeyEvent, cx: &mut Cx) {
    if key.code == KeyCode::Char('x') && cx.session.require_card().is_some() {
        cx.session.plan("Scanning for junk", |ctx, _| {
            Ok(pending::preview(clean::plan(&ctx.pocket()?, false)?))
        });
    }
}
