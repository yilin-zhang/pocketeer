//! TUI state and key handling. Rendering is in `ui.rs`; the actions behind
//! the keys are in `actions.rs` / `dialogs.rs`; the data types (tabs,
//! filters, the per-tab table, dialogs) are `model.rs`.
//!
//! Four tabs, Calibre-style: one list per kind of thing (cores, required
//! files, games) with a text filter and a "what to show" filter, rather than
//! one tab per source. Row lists are computed once per change
//! (`rebuild_rows`), not per frame. Everything that touches the card or the
//! network runs on the worker threads in `worker.rs`, and covers are decoded
//! on their own thread in `covers.rs`; the app only submits jobs and reacts
//! to their messages, so a frame never waits on disk, network or images.

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::atomic::Ordering,
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use pocketeer_core::{
    Error, ErrorInfo, Pocket,
    card::bios,
    card::cores::{self, CoreList, SearchFilter},
    card::roms::CardRom,
    library::rename::RenamePlan,
    plan::Event,
};
use ratatui::layout::Rect;
use ratatui_image::picker::Picker;

mod actions;
mod dialogs;
mod keys;
mod model;
mod path_input;
pub use path_input::Suggestion;
pub mod covers;
mod rows;

pub use keys::SUGGESTIONS_SHOWN;
pub use model::*;

use super::{
    pending::{Pending, Preview, Request, text_lines},
    worker::{CardSnapshot, GameRow, Job, LibraryView, Msg, Snapshot, Worker},
};
use crate::{
    cli::Global,
    ops::context::Context,
    ops::{Staged, library::StandardizePlan},
    text,
};

pub struct App {
    global: Global,
    /// Settings as loaded at the last reload; every job gets a clone.
    base: Context,
    worker: Worker,
    path_input: path_input::PathLookup,
    pub pocket: Option<Pocket>,
    pub pocket_error: Option<ErrorInfo>,
    pub snapshot: Option<Snapshot>,
    pub tab: Tab,
    pub cores: Pane<CoreFilter, CoreRow>,
    /// Rows are (core index, file index) into `Snapshot::bios`.
    pub bios: Pane<BiosFilter, (usize, usize)>,
    pub games: Pane<GameFilter, GameRef>,
    /// Covers live in the library, so a reload keeps them.
    pub covers: covers::Covers,
    library_path: Option<PathBuf>,
    pub modal: Option<Modal>,
    pub busy: Busy,
    pub hit: HitAreas,
    /// Pane divider positions; dragged with the mouse.
    pub splits: Splits,
    /// A divider being dragged: which, and the area it divides.
    dragging: Option<(Splitter, Rect)>,
    pub log: VecDeque<String>,
    pub should_quit: bool,
    auto: AutoRefresh,
    verify: Verification,
}

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

impl App {
    pub fn new(global: Global, picker: Picker) -> Self {
        let ctx = Context::new(global.clone());
        let library_path = ctx.library_path().ok();
        let (pocket, pocket_error) = match ctx.pocket() {
            Ok(p) => (Some(p), None),
            Err(e) => (None, Some(e.to_info())),
        };
        // Without a card the library is all there is to look at.
        let tab = if pocket.is_some() {
            Tab::Cores
        } else {
            Tab::Games
        };
        App {
            base: ctx,
            worker: Worker::spawn(),
            path_input: path_input::PathLookup::spawn(),
            pocket,
            pocket_error,
            snapshot: None,
            tab,
            cores: Pane::new(CoreFilter::Installed),
            bios: Pane::new(BiosFilter::All),
            games: Pane::new(GameFilter::All),
            covers: covers::Covers::spawn(picker),
            library_path,
            modal: None,
            busy: Busy::default(),
            hit: HitAreas::default(),
            splits: Splits::default(),
            dragging: None,
            log: VecDeque::new(),
            should_quit: false,
            auto: AutoRefresh::default(),
            verify: Verification::default(),
            global,
        }
    }

    // -----------------------------------------------------------------------
    // jobs

    /// What a job needs: the CLI's context, told which card we have (if any)
    /// instead of detecting one.
    fn ctx(&self) -> Context {
        self.base.clone().with_pocket(self.pocket.clone())
    }

    /// For actions that need the library to exist; `None` says so.
    pub(super) fn lib_ctx(&mut self) -> Option<Context> {
        if self.library_view().is_none() {
            self.no_library();
            return None;
        }
        Some(self.ctx())
    }

    /// For actions that need a card; `None` says so.
    pub(super) fn card(&mut self) -> Option<Pocket> {
        if self.pocket.is_none() {
            self.notice("No SD card mounted: plug one in, it is picked up by itself.");
        }
        self.pocket.clone()
    }

    /// Runs a job on the main worker; `status` is shown while it runs.
    pub(super) fn submit(&mut self, status: &str, job: Job) {
        let epoch = self.worker.epoch.fetch_add(1, Ordering::Relaxed) + 1;
        if matches!(job, Job::Load | Job::Apply(_)) {
            self.verify.epoch = epoch;
        }
        self.busy.main = Some(status.to_string());
        self.worker.submit(self.ctx(), job);
    }

    /// Runs a job that doesn't block the UI.
    pub(super) fn submit_side(&mut self, job: Job) {
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

    /// Handles everything the workers sent since last time.
    pub fn pump(&mut self) {
        self.covers.receive();
        let mut complete_input = false;
        if let Some(result) = self.path_input.latest()
            && let Some(Modal::Input {
                value,
                kind,
                suggestions,
                completion,
                searching,
                tab_pending,
                ..
            }) = &mut self.modal
            && kind.completes_paths()
            && *value == result.value
        {
            *suggestions = result.suggestions;
            *completion = Some(result.completion);
            *searching = !result.finished;
            complete_input = std::mem::take(tab_pending);
        }
        if complete_input {
            self.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        }

        while let Some(msg) = self.worker.try_recv() {
            match msg {
                Msg::Verified {
                    result,
                    next,
                    epoch,
                } => {
                    self.verify.inflight = false;
                    if epoch >= self.verify.epoch {
                        match result {
                            Ok(mut index) => {
                                if let Some(warning) = index.warning.take() {
                                    self.log(warning);
                                }
                                let selected = self.game_selection();
                                if let Some(snap) = &mut self.snapshot {
                                    if let Some(card) = &mut snap.card {
                                        card.card_roms = index.roms.clone();
                                    }
                                    snap.verification = Some(index);
                                    snap.rematch();
                                }
                                self.verify.next = next;
                                self.rebuild_rows_preserving(selected);
                            }
                            Err(e) => self.log(format!("ROM verification stopped: {e}")),
                        }
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
                    let selected = self.game_selection();
                    if let (Ok(view), Some(snap)) = (result, &mut self.snapshot) {
                        snap.library = Some(*view);
                        snap.rematch();
                        self.rebuild_rows_preserving(selected);
                    }
                }
                Msg::Loaded(result) => {
                    self.clear_busy();
                    match result {
                        Ok(mut snap) => {
                            if let Some(warning) =
                                snap.verification.as_mut().and_then(|i| i.warning.take())
                            {
                                self.log(warning);
                            }
                            let selected = self.game_selection();
                            self.snapshot = Some(*snap);
                            self.verify.next = 0;
                            self.rebuild_rows_preserving(selected);
                            self.auto_identify();
                        }
                        Err(e) => self.error("Couldn't load", &e),
                    }
                }
                Msg::Prepared(results) => {
                    self.clear_busy();
                    self.show_install_plans(results);
                }
                Msg::Planned(result) => {
                    self.clear_busy();
                    match result {
                        Ok(planned) => self.planned(planned),
                        Err(e) => self.error("Couldn't plan that", &e),
                    }
                }
                Msg::Fingerprint(fp) => {
                    self.auto.fingerprint_inflight = false;
                    let changed = self.snapshot.as_ref().is_some_and(|s| s.fingerprint != fp);
                    // Don't pull the rug from under a dialog or a running job.
                    if changed && self.busy.main.is_none() && self.modal.is_none() {
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
                        self.log(line.clone());
                    }
                    if done.failed > 0 {
                        self.info("Background library task failed", done.lines);
                    }
                    // New names or files: refresh the Games tab when nothing else is running.
                    if done.changed && self.busy.main.is_none() && self.modal.is_none() {
                        self.reload();
                    }
                }
                Msg::Applied(done) => {
                    self.auto.importing = false;
                    self.clear_busy();
                    for line in &done.lines {
                        self.log(line.clone());
                    }
                    if done.failed > 0 {
                        self.info(&format!("{} operation(s) failed", done.failed), done.lines);
                    }
                    self.reload();
                }
            }
        }
        self.schedule_verification();
    }

    fn schedule_verification(&mut self) {
        if self.busy.main.is_some() || self.modal.is_some() || self.verify.inflight {
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

    /// A plan came back: open the dialog it asks for.
    pub(super) fn planned(&mut self, pending: Pending) {
        match pending.preview() {
            Preview::Notice(line) => self.notice(line),
            Preview::Info { title, lines } => self.info(&title, lines),
            Preview::Confirm {
                title,
                lines,
                pending,
            } => self.confirm(title, lines, pending),
            Preview::SaveConflict { plan, index } => self.ask_save_conflict(plan, index),
            Preview::PickRenames(plans) => self.offer_standardize(plans),
            Preview::PickImportNames(import) => self.offer_import(import),
        }
    }

    /// Called every couple of seconds by the event loop: notice a card being
    /// plugged in or pulled, and ask the side worker whether anything on the
    /// card or in the library changed.
    pub fn tick(&mut self) {
        if self.busy.main.is_some() {
            return;
        }
        match &self.pocket {
            // No card: look for one, so plugging it in is enough.
            None => {
                if let Ok(p) = self.base.pocket() {
                    self.pocket = Some(p);
                    self.pocket_error = None;
                    self.notice("SD card detected");
                    self.reload();
                    return;
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
                self.modal = None;
                self.notice("SD card removed; the Games tab still works from the library");
                self.reload();
                return;
            }
            Some(_) => {}
        }
        if self.auto.fingerprint_inflight || self.snapshot.is_none() {
            return;
        }
        self.auto.fingerprint_inflight = true;
        self.submit_side(Job::Fingerprint);
    }

    fn clear_busy(&mut self) {
        self.busy.main = None;
        self.busy.step = None;
        self.busy.download_pct = None;
    }

    // -----------------------------------------------------------------------
    // dialogs and messages

    /// A plan with Apply / Cancel buttons.
    pub(super) fn confirm(
        &mut self,
        title: impl Into<String>,
        lines: Vec<String>,
        pending: Pending,
    ) {
        self.modal = Some(Modal::Confirm {
            title: title.into(),
            lines,
            scroll: 0,
            focus_apply: true,
            pending,
        });
    }

    /// A one-line text prompt.
    pub(super) fn input(
        &mut self,
        title: impl Into<String>,
        lines: Vec<String>,
        value: String,
        kind: InputKind,
    ) {
        self.modal = Some(Modal::Input {
            title: title.into(),
            lines,
            value,
            kind,
            suggestions: Vec::new(),
            completion: None,
            searching: false,
            tab_pending: false,
            highlight: None,
        });
        self.refresh_input();
    }

    /// Something the user has to read (errors, reports, explanations).
    pub(super) fn info(&mut self, title: &str, lines: Vec<String>) {
        self.modal = Some(Modal::Info {
            title: title.to_string(),
            lines,
            scroll: 0,
        });
    }

    pub(super) fn error(&mut self, title: &str, e: &Error) {
        let info = e.to_info();
        let mut lines = vec![info.message];
        if let Some(hint) = info.hint {
            lines.push(String::new());
            lines.push(hint);
        }
        self.info(title, lines);
    }

    /// A one-liner for the status bar: nothing to confirm, nothing to read twice.
    pub(super) fn notice(&mut self, line: impl Into<String>) {
        self.log(line.into());
    }

    pub(super) fn no_library(&mut self) {
        let path = self.library_path_display();
        self.notice(format!(
            "No library yet: press n on the Games tab to create one at {path}."
        ));
    }

    /// A planned import whose files have standard-name suggestions: ask
    /// which to take, then come back to [`App::planned`].
    pub(super) fn offer_import(&mut self, import: Pending) {
        let suggestions = import.import_suggestions();
        let items = suggestions
            .iter()
            .map(|(_, old, new)| (format!("{old}  →  {new}"), true))
            .collect();
        let keys = suggestions
            .into_iter()
            .map(|(platform, old, _)| (platform, old))
            .collect();
        self.modal = Some(Modal::Toggle {
            title: "These look like known games. Import under the standard name?".into(),
            items,
            cursor: 0,
            kind: ToggleKind::ImportNames {
                import: Box::new(import),
                keys,
            },
        });
    }

    /// Standard-name renames found for existing games: a checklist.
    pub(super) fn offer_standardize(&mut self, plans: Vec<RenamePlan>) {
        let items = plans
            .iter()
            .map(|p| {
                (
                    format!("[{}] {}  →  {}", p.platform, p.old_name, p.new_name),
                    true,
                )
            })
            .collect();
        self.modal = Some(Modal::Toggle {
            title: "Rename to standard names?".into(),
            items,
            cursor: 0,
            kind: ToggleKind::Standardize { plans },
        });
    }

    pub(super) fn log(&mut self, line: String) {
        self.log.push_back(line);
        while self.log.len() > 50 {
            self.log.pop_front();
        }
    }

    pub fn last_log(&self) -> Option<&str> {
        self.log.back().map(String::as_str)
    }
}
