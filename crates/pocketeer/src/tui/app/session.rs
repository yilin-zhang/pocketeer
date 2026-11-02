//! What the app knows and what it has running: the settings, the card, the
//! last snapshot of card and library, the saves the side worker read, the
//! two worker threads, the status line and the log. Everything that
//! touches the card, the library or the network runs on a worker; this
//! only submits jobs and folds their messages into the snapshot, then says
//! what the screen has to react to (`Update`).

use std::{collections::VecDeque, path::PathBuf, sync::atomic::Ordering};

use pocketeer_core::{Error, ErrorInfo, Pocket, card::install, library::identify, plan::Event};

use crate::{cli::Global, ops::context::Context, tui::pending::Pending};

use super::super::worker::{CardSnapshot, Job, LibraryView, Msg, SavesView, Snapshot, Worker};

/// What the status line shows while workers run.
#[derive(Default)]
pub struct Busy {
    /// The main worker's job, if one is running. Keys that change things
    /// are ignored while it is set.
    pub main: Option<String>,
    /// Progress of that job, when it reports steps.
    pub step: Option<(usize, usize)>,
    pub download_pct: Option<u64>,
    /// The side worker (covers, DAT checks); never blocks keys.
    pub side: Option<String>,
    /// Background ROM verification ("ROM check 3/12").
    pub roms: Option<String>,
}

/// What the screen has to do after the workers reported.
pub enum Update {
    /// The snapshot changed: recompute the tables, keeping the selection.
    Rows,
    /// A new snapshot: saves have to be matched to games again too.
    Loaded,
    /// The side worker read the saves and Memories.
    Saves,
    /// A plan came back and needs its dialog.
    Planned(Pending),
    /// Core installs were prepared.
    Prepared(Vec<(String, Result<install::Prepared, Error>)>),
    Info {
        title: String,
        lines: Vec<String>,
    },
    Error {
        title: &'static str,
        error: Error,
    },
    /// The card was pulled: any open dialog is about it, so close it.
    CardRemoved,
}

pub struct Session {
    global: Global,
    /// Settings as loaded at the last reload; every job gets a clone.
    base: Context,
    worker: Worker,
    pocket: Option<Pocket>,
    pocket_error: Option<ErrorInfo>,
    snapshot: Option<Snapshot>,
    /// Saves and Memories, from the side worker after each load.
    saves: SavesView,
    library_path: Option<PathBuf>,
    busy: Busy,
    log: VecDeque<String>,
    auto: AutoRefresh,
    verify: Verification,
}

/// What the app re-reads or re-checks without being asked.
#[derive(Default)]
struct AutoRefresh {
    /// The background DAT and cover check ran this session; cleared after
    /// an import so new games get one too.
    metadata_done: bool,
    /// A change check is in flight on the side worker.
    fingerprint_inflight: bool,
    /// The running main-worker job adds games one by one; refresh the Games
    /// tab after each step.
    importing: bool,
    /// One such library re-read is in flight (never more).
    library_inflight: bool,
}

/// Where the one-file-at-a-time ROM verification is.
#[derive(Default)]
struct Verification {
    next: usize,
    inflight: bool,
    /// Worker epoch of the snapshot being verified; results from an older
    /// one are dropped.
    epoch: u64,
}

impl Session {
    pub fn new(global: Global) -> Self {
        let base = Context::new(global.clone());
        let library_path = base.library_path().ok();
        let (pocket, pocket_error) = match base.pocket() {
            Ok(p) => (Some(p), None),
            Err(e) => (None, Some(e.to_info())),
        };
        Session {
            global,
            base,
            worker: Worker::spawn(),
            pocket,
            pocket_error,
            snapshot: None,
            saves: SavesView::default(),
            library_path,
            busy: Busy::default(),
            log: VecDeque::new(),
            auto: AutoRefresh::default(),
            verify: Verification::default(),
        }
    }

    // -- what is known ------------------------------------------------------

    pub fn pocket(&self) -> Option<&Pocket> {
        self.pocket.as_ref()
    }

    pub fn pocket_error(&self) -> Option<&ErrorInfo> {
        self.pocket_error.as_ref()
    }

    /// `None` until the first load lands.
    pub fn snapshot(&self) -> Option<&Snapshot> {
        self.snapshot.as_ref()
    }

    /// The card's part of the snapshot, if a card is mounted and loaded.
    pub fn card(&self) -> Option<&CardSnapshot> {
        self.snapshot.as_ref()?.card.as_ref()
    }

    pub fn library(&self) -> Option<&LibraryView> {
        self.snapshot.as_ref()?.library.as_ref()
    }

    pub fn saves(&self) -> &SavesView {
        &self.saves
    }

    pub fn busy(&self) -> &Busy {
        &self.busy
    }

    /// The status line's history, newest first.
    pub fn log(&self) -> impl Iterator<Item = &str> {
        self.log.iter().rev().map(String::as_str)
    }

    pub fn library_path_display(&self) -> String {
        self.library_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(no path configured)".into())
    }

    /// Platform ids the library holds or an installed core handles, sorted.
    pub fn known_platforms(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .library()
            .map(|l| l.platforms.clone())
            .unwrap_or_default();
        if let Some(s) = self.card() {
            for c in &s.cores.cores {
                ids.extend(c.core.platform_ids.iter().cloned());
            }
        }
        ids.sort();
        ids.dedup();
        ids
    }

    // -- the status line ----------------------------------------------------

    /// A one-liner for the status bar: nothing to confirm, nothing to read twice.
    pub fn notice(&mut self, line: impl Into<String>) {
        self.log.push_back(line.into());
        while self.log.len() > 50 {
            self.log.pop_front();
        }
    }

    // -- jobs ---------------------------------------------------------------

    /// What a job needs: the CLI's context, told which card we have (if any)
    /// instead of detecting one.
    pub fn ctx(&self) -> Context {
        self.base.clone().with_pocket(self.pocket.clone())
    }

    /// For actions that need the library to exist; `None` says so.
    pub fn require_library(&mut self) -> Option<Context> {
        if self.library().is_none() {
            let path = self.library_path_display();
            self.notice(format!(
                "No library yet: press n on the Games tab to create one at {path}."
            ));
            return None;
        }
        Some(self.ctx())
    }

    /// For actions that need a card; `None` says so.
    pub fn require_card(&mut self) -> Option<Pocket> {
        if self.pocket.is_none() {
            self.notice("No SD card mounted: plug one in, it is picked up by itself.");
        }
        self.pocket.clone()
    }

    /// A main-worker job is running: keys that change things wait.
    pub fn is_busy(&self) -> bool {
        self.busy.main.is_some()
    }

    /// Runs a job on the main worker; `status` is shown while it runs.
    pub fn submit(&mut self, status: &str, job: Job) {
        let epoch = self.worker.epoch.fetch_add(1, Ordering::Relaxed) + 1;
        if matches!(job, Job::Load | Job::Apply(_)) {
            self.verify.epoch = epoch;
        }
        if matches!(&job, Job::Apply(p) if p.is_import()) {
            // New games: show each as it lands, check them after the reload.
            self.auto.importing = true;
            self.auto.metadata_done = false;
        }
        self.busy.main = Some(status.to_string());
        self.worker.submit(self.ctx(), job);
    }

    /// Runs a job that doesn't block the UI.
    fn submit_side(&mut self, job: Job) {
        self.worker.submit_side(self.ctx(), job);
    }

    /// Re-reads everything: the card if one is mounted (looking for one
    /// first if none was), and the library.
    pub fn reload(&mut self) {
        // Pick up an edited config file.
        self.base = Context::new(self.global.clone());
        if self.pocket.is_none() {
            match self.base.pocket() {
                Ok(p) => {
                    self.pocket = Some(p);
                    self.pocket_error = None;
                }
                Err(e) => self.pocket_error = Some(e.to_info()),
            }
        }
        self.submit("Loading", Job::Load);
    }

    /// DAT checks for the library, on the side worker.
    pub fn identify(&mut self) {
        if self.library().is_none() || self.busy.side.is_some() {
            return;
        }
        self.busy.side = Some("checking DATs".into());
        self.submit_side(Job::Identify);
    }

    /// Missing covers of one platform (or all), on the side worker; one
    /// fetch at a time.
    pub fn fetch_covers(&mut self, platform: Option<String>, system: Option<String>) {
        if self.busy.side.is_some() {
            self.notice("Covers are being fetched right now; wait for the status line to clear.");
            return;
        }
        self.busy.side = Some("fetching covers".into());
        self.submit_side(Job::FetchCovers {
            platform,
            system,
            quiet: false,
        });
    }

    /// Identify content before fetching covers, once per session and after imports.
    fn auto_identify(&mut self) {
        let unchecked = self.library().is_some_and(|l| {
            l.games.iter().any(|g| {
                g.game
                    .roms
                    .iter()
                    .any(|r| r.dat.status == identify::Status::Unchecked)
            })
        });
        if (self.auto.metadata_done && !unchecked) || self.busy.side.is_some() {
            return;
        }
        self.auto.metadata_done = true;
        self.identify();
    }

    /// Asks the side worker for saves and Memories (after every load).
    fn request_saves(&mut self) {
        let copies = self
            .library()
            .map(|l| {
                l.games
                    .iter()
                    .map(|g| {
                        let paths = g
                            .matches
                            .iter()
                            .flat_map(|m| m.copies.iter().map(|c| c.path.clone()))
                            .collect();
                        (g.game.id, paths)
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.submit_side(Job::Saves { copies });
    }

    fn clear_busy(&mut self) {
        self.busy.main = None;
        self.busy.step = None;
        self.busy.download_pct = None;
    }

    // -- what the workers said ----------------------------------------------

    /// Folds everything the workers sent since last time into the snapshot.
    /// `dialog_open` holds back reloads that would pull the rug from under
    /// a dialog.
    pub fn pump(&mut self, dialog_open: bool) -> Vec<Update> {
        let mut updates = Vec::new();
        while let Some(msg) = self.worker.try_recv() {
            match msg {
                Msg::Verified {
                    result,
                    next,
                    epoch,
                } => {
                    self.verify.inflight = false;
                    if epoch < self.verify.epoch {
                        continue;
                    }
                    match result {
                        Ok(mut index) => {
                            if let Some(warning) = index.warning.take() {
                                self.notice(warning);
                            }
                            if let Some(snap) = &mut self.snapshot {
                                if let Some(card) = &mut snap.card {
                                    card.card_roms = index.roms.clone();
                                }
                                snap.verification = Some(index);
                                snap.rematch();
                            }
                            self.verify.next = next;
                            updates.push(Update::Rows);
                        }
                        Err(e) => self.notice(format!("ROM verification stopped: {e}")),
                    }
                }
                Msg::Progress(Event::Status(s)) => {
                    self.busy.main = Some(s);
                    self.busy.download_pct = None;
                    self.busy.step = None;
                }
                Msg::Progress(Event::Download {
                    received, total, ..
                }) => {
                    self.busy.download_pct = total.filter(|t| *t > 0).map(|t| received * 100 / t);
                    self.busy.main = Some(match self.busy.download_pct {
                        Some(p) => format!("Downloading {p}%"),
                        None => format!("Downloading {} KiB", received / 1024),
                    });
                }
                Msg::Progress(Event::Step { done, total }) => {
                    self.busy.step = Some((done, total));
                    if self.auto.importing && !self.auto.library_inflight {
                        self.auto.library_inflight = true;
                        self.submit_side(Job::RefreshLibrary);
                    }
                }
                Msg::LibraryRefreshed(result) => {
                    self.auto.library_inflight = false;
                    if let (Ok(view), Some(snap)) = (result, &mut self.snapshot) {
                        snap.library = Some(*view);
                        snap.rematch();
                        updates.push(Update::Rows);
                    }
                }
                Msg::Loaded(result) => {
                    self.clear_busy();
                    match result {
                        Ok(mut snap) => {
                            if let Some(warning) =
                                snap.verification.as_mut().and_then(|i| i.warning.take())
                            {
                                self.notice(warning);
                            }
                            self.snapshot = Some(*snap);
                            self.verify.next = 0;
                            updates.push(Update::Loaded);
                            self.request_saves();
                            self.auto_identify();
                        }
                        Err(error) => updates.push(Update::Error {
                            title: "Couldn't load",
                            error,
                        }),
                    }
                }
                Msg::Saves(result) => match result {
                    Ok(view) => {
                        self.saves = *view;
                        updates.push(Update::Saves);
                    }
                    Err(e) => self.notice(format!("Couldn't read saves: {e}")),
                },
                Msg::Prepared(results) => {
                    self.clear_busy();
                    updates.push(Update::Prepared(results));
                }
                Msg::Planned(result) => {
                    self.clear_busy();
                    updates.push(match result {
                        Ok(pending) => Update::Planned(pending),
                        Err(error) => Update::Error {
                            title: "Couldn't plan that",
                            error,
                        },
                    });
                }
                Msg::Fingerprint(fp) => {
                    self.auto.fingerprint_inflight = false;
                    let changed = self.snapshot.as_ref().is_some_and(|s| s.fingerprint != fp);
                    if changed && !self.is_busy() && !dialog_open {
                        self.notice("↻ card or library changed outside pocketeer; reloaded");
                        self.reload();
                    }
                }
                Msg::SideProgress(Event::Status(s)) => self.busy.side = Some(s),
                Msg::SideProgress(Event::Step { done, total }) => {
                    self.busy.side = Some(format!("covers {done}/{total}"))
                }
                Msg::SideProgress(_) => {}
                Msg::SideDone(done) => {
                    self.busy.side = None;
                    for line in &done.lines {
                        self.notice(line.clone());
                    }
                    if done.failed > 0 {
                        updates.push(Update::Info {
                            title: "Background library task failed".into(),
                            lines: done.lines,
                        });
                    }
                    // New names or files: refresh when nothing else is running.
                    if done.changed && !self.is_busy() && !dialog_open {
                        self.reload();
                    }
                }
                Msg::Applied(done) => {
                    self.auto.importing = false;
                    self.clear_busy();
                    for line in &done.lines {
                        self.notice(line.clone());
                    }
                    if done.failed > 0 {
                        updates.push(Update::Info {
                            title: format!("{} operation(s) failed", done.failed),
                            lines: done.lines,
                        });
                    }
                    self.reload();
                }
            }
        }
        self.schedule_verification(dialog_open);
        updates
    }

    fn schedule_verification(&mut self, dialog_open: bool) {
        if self.is_busy() || dialog_open || self.verify.inflight {
            return;
        }
        let Some(index) = self.snapshot.as_mut().and_then(|s| s.verification.take()) else {
            return;
        };
        if self.verify.next >= index.roms.len() {
            self.busy.roms = None;
            return;
        }
        self.busy.roms = Some(format!(
            "ROM check {}/{}",
            self.verify.next + 1,
            index.roms.len()
        ));
        self.verify.inflight = true;
        self.worker.submit(
            self.ctx(),
            Job::Verify {
                index,
                cursor: self.verify.next,
                epoch: self.worker.epoch.load(Ordering::Relaxed),
                token: self.worker.epoch.clone(),
            },
        );
    }

    /// Called every couple of seconds: notice a card being plugged in or
    /// pulled, and ask the side worker whether anything on the card or in
    /// the library changed.
    pub fn tick(&mut self) -> Option<Update> {
        if self.is_busy() {
            return None;
        }
        match &self.pocket {
            // No card: look for one, so plugging it in is enough.
            None => {
                if let Ok(p) = self.base.pocket() {
                    self.pocket = Some(p);
                    self.pocket_error = None;
                    self.notice("SD card detected");
                    self.reload();
                    return None;
                }
            }
            // The card went away: drop its tabs, keep the library, keep looking.
            Some(p) if !p.root().is_dir() => {
                self.pocket = None;
                self.pocket_error = Some(ErrorInfo {
                    code: "SD_NOT_FOUND",
                    message: "SD card removed".into(),
                    hint: Some("Plug it back in; pocketeer picks it up by itself.".into()),
                    details: None,
                });
                self.notice("SD card removed; the Games tab still works from the library");
                self.reload();
                return Some(Update::CardRemoved);
            }
            Some(_) => {}
        }
        if !self.auto.fingerprint_inflight && self.snapshot.is_some() {
            self.auto.fingerprint_inflight = true;
            self.submit_side(Job::Fingerprint);
        }
        None
    }

    #[cfg(test)]
    pub fn set_snapshot(&mut self, snapshot: Snapshot) {
        self.snapshot = Some(snapshot);
    }

    #[cfg(test)]
    pub fn snapshot_mut(&mut self) -> Option<&mut Snapshot> {
        self.snapshot.as_mut()
    }
}
