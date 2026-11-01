//! TUI state and key handling. Rendering is in `ui.rs`; the actions behind
//! the keys are in `actions.rs` / `library.rs` / `dialogs.rs`; the per-tab
//! table state is `pane.rs`.
//!
//! Four tabs, Calibre-style: one list per kind of thing (cores, required
//! files, games) with a text filter and a "what to show" filter, rather than
//! one tab per source. Row lists are computed once per change
//! (`rebuild_rows`), not per frame. Everything that touches the card or the
//! network runs on the worker threads in `jobs.rs`; the app only submits
//! jobs and reacts to their messages.

use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::atomic::Ordering,
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use pocketeer_core::{
    Error, ErrorInfo, Pocket,
    cores::{self, CoreList, SearchFilter},
    library::rename::RenamePlan,
    progress::Event,
    required,
    roms::index::CardRom,
};
use ratatui::layout::Rect;
use ratatui_image::{picker::Picker, protocol::StatefulProtocol};

mod actions;
mod dialogs;
mod keys;
mod library;
mod model;
mod pane;
mod path_worker;
mod rows;

pub use keys::SUGGESTIONS_SHOWN;
pub use model::*;
pub use pane::{Cycle, Pane, PaneOps};

use super::jobs::{
    CardSnapshot, GameRow, Job, LibraryView, Msg, Pending, Request, Snapshot, Worker,
};
use crate::{
    cli::Global,
    commands::{
        Staged, card as card_cmd, card_import as card_import_cmd,
        card_import::ImportReport as CardImportReport, library as library_cmd,
        library::ImportReport as LibraryImportReport, library::StandardizePlan, saves as saves_cmd,
    },
    context::Context,
    human,
};

pub struct App {
    global: Global,
    worker: Worker,
    path_worker: path_worker::PathWorker,
    pub pocket: Option<Pocket>,
    pub pocket_error: Option<ErrorInfo>,
    pub snapshot: Option<Snapshot>,
    pub tab: Tab,
    pub cores: Pane<CoreFilter, CoreRow>,
    /// Rows are (core index, file index) into `Snapshot::bios`.
    pub bios: Pane<BiosFilter, (usize, usize)>,
    pub games: Pane<GameFilter, GameRef>,
    pub picker: Picker,
    covers: HashMap<PathBuf, StatefulProtocol>,
    library_path: Option<PathBuf>,
    pub modal: Option<Modal>,
    /// Current background status line, if a card-touching job is running.
    /// Keys that change things are ignored while it is set.
    pub busy: Option<String>,
    pub download_pct: Option<u64>,
    /// Status of the side worker (cover fetching); never blocks keys.
    pub side_status: Option<String>,
    /// Progress of the current main-worker job, when it reports steps.
    pub busy_step: Option<(usize, usize)>,
    pub hit: HitAreas,
    /// Pane divider positions; dragged with the mouse.
    pub splits: Splits,
    /// A divider being dragged: which, and the area it divides.
    dragging: Option<(Splitter, Rect)>,
    pub log: VecDeque<String>,
    pub should_quit: bool,
    /// DAT identification and covers run once per session (and after imports).
    auto_metadata_done: bool,
    /// A change check is in flight on the side worker.
    fingerprint_pending: bool,
    /// The running main-worker job adds games one by one; refresh the Games
    /// tab after each step (at most one refresh in flight).
    importing: bool,
    refresh_pending: bool,
    verification_next: usize,
    verification_inflight: bool,
    snapshot_epoch: u64,
    pub rom_status: Option<String>,
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
            worker: Worker::spawn(),
            path_worker: path_worker::PathWorker::spawn(),
            pocket,
            pocket_error,
            snapshot: None,
            tab,
            cores: Pane::new(CoreFilter::Installed),
            bios: Pane::new(BiosFilter::All),
            games: Pane::new(GameFilter::All),
            picker,
            covers: HashMap::new(),
            library_path,
            modal: None,
            busy: None,
            download_pct: None,
            side_status: None,
            busy_step: None,
            hit: HitAreas::default(),
            splits: Splits::default(),
            dragging: None,
            log: VecDeque::new(),
            should_quit: false,
            auto_metadata_done: false,
            fingerprint_pending: false,
            importing: false,
            refresh_pending: false,
            verification_next: 0,
            verification_inflight: false,
            snapshot_epoch: 0,
            rom_status: None,
            global,
        }
    }

    // -----------------------------------------------------------------------
    // jobs

    /// What a job needs: the CLI's context, told which card we have (if any)
    /// instead of detecting one.
    fn ctx(&self) -> Context {
        Context::new(self.global.clone()).with_pocket(self.pocket.clone())
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
            self.snapshot_epoch = epoch;
        }
        self.busy = Some(status.to_string());
        self.worker.submit(self.ctx(), job);
    }

    /// Runs a job that doesn't block the UI.
    pub(super) fn submit_side(&mut self, job: Job) {
        self.worker.submit_side(self.ctx(), job);
    }

    /// Re-reads everything: the card if one is mounted (looking for one
    /// first if none was), and the library.
    pub fn reload(&mut self) {
        if self.pocket.is_none() {
            match Context::new(self.global.clone()).pocket() {
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
        let mut complete_input = false;
        if let Some(result) = self.path_worker.latest()
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
                    self.verification_inflight = false;
                    if epoch >= self.snapshot_epoch {
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
                                self.verification_next = next;
                                self.rebuild_rows_preserving(selected);
                            }
                            Err(e) => self.log(format!("ROM verification stopped: {e}")),
                        }
                    }
                }
                Msg::Progress(Event::Status(s)) => {
                    self.busy = Some(s);
                    self.download_pct = None;
                    self.busy_step = None;
                }
                Msg::Progress(Event::Download {
                    received, total, ..
                }) => {
                    self.download_pct = total.filter(|t| *t > 0).map(|t| received * 100 / t);
                    self.busy = Some(match self.download_pct {
                        Some(p) => format!("Downloading {p}%"),
                        None => format!("Downloading {} KiB", received / 1024),
                    });
                }
                Msg::Progress(Event::Step { done, total }) => {
                    self.busy_step = Some((done, total));
                    if self.importing && !self.refresh_pending {
                        self.refresh_pending = true;
                        self.submit_side(Job::RefreshLibrary);
                    }
                }
                Msg::LibraryRefreshed(result) => {
                    self.refresh_pending = false;
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
                            self.verification_next = 0;
                            self.covers.clear();
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
                    self.fingerprint_pending = false;
                    let changed = self.snapshot.as_ref().is_some_and(|s| s.fingerprint != fp);
                    // Don't pull the rug from under a dialog or a running job.
                    if changed && self.busy.is_none() && self.modal.is_none() {
                        self.notice("↻ card or library changed outside pocketeer; reloaded");
                        self.reload();
                    }
                }
                Msg::SideProgress(Event::Status(s)) => self.side_status = Some(s),
                Msg::SideProgress(Event::Step { done, total }) => {
                    self.side_status = Some(format!("covers {done}/{total}"))
                }
                Msg::SideProgress(_) => {}
                Msg::SideDone { lines, failed } => {
                    self.side_status = None;
                    let fetched_any = !lines.is_empty();
                    for line in &lines {
                        self.log(line.clone());
                    }
                    if failed > 0 {
                        self.info("Background library task failed", lines);
                    }
                    // New files on disk: refresh the Games tab when nothing else is running.
                    if fetched_any && self.busy.is_none() && self.modal.is_none() {
                        self.reload();
                    }
                }
                Msg::Applied { lines, failed } => {
                    self.importing = false;
                    self.clear_busy();
                    for line in &lines {
                        self.log(line.clone());
                    }
                    if failed > 0 {
                        self.info(&format!("{failed} operation(s) failed"), lines);
                    }
                    self.reload();
                }
            }
        }
        self.schedule_verification();
    }

    fn schedule_verification(&mut self) {
        if self.busy.is_some() || self.modal.is_some() || self.verification_inflight {
            return;
        }
        let Some(index) = self.snapshot.as_mut().and_then(|s| s.verification.take()) else {
            return;
        };
        if self.verification_next >= index.roms.len() {
            self.rom_status = None;
            return;
        }
        self.rom_status = Some(format!(
            "ROM check {}/{}",
            self.verification_next + 1,
            index.roms.len()
        ));
        self.verification_inflight = true;
        self.worker.submit(
            self.ctx(),
            Job::Verify {
                index,
                cursor: self.verification_next,
                epoch: self.worker.epoch.load(Ordering::Relaxed),
                token: self.worker.epoch.clone(),
            },
        );
    }

    /// A plan came back: show it, or say there is nothing to do.
    fn planned(&mut self, pending: Pending) {
        match pending {
            Pending::ImportCover(plan) => {
                let replacing = plan.previous.as_ref().is_some_and(|p| p.rel_path.is_some());
                let title = if replacing {
                    "Replace this game's cover?"
                } else {
                    "Import this game's cover?"
                };
                let lines = text_lines(&library_cmd::cover_import(&Staged::preview(plan.clone())));
                self.confirm(title, lines, Pending::ImportCover(plan));
            }
            Pending::CardRename(plan) => {
                let lines = text_lines(&card_cmd::rename_report(&Staged::preview(plan.clone())));
                self.confirm(
                    "Rename this AP ROM and its saves?",
                    lines,
                    Pending::CardRename(plan),
                );
            }
            Pending::Clean(plan) => {
                if plan.entries.is_empty() {
                    self.notice("No macOS junk on the card.");
                } else {
                    let title = format!(
                        "Delete {} junk item(s), {}?",
                        plan.entries.len(),
                        human::bytes(plan.bytes)
                    );
                    let lines = text_lines(&card_cmd::clean_report(&Staged::preview(plan.clone())));
                    self.confirm(title, lines, Pending::Clean(plan));
                }
            }
            Pending::Copy(plan) => {
                let lines = text_lines(&card_cmd::copy_report(&Staged::preview(plan.clone())));
                if plan.changes() == 0 {
                    self.info("Already on this card", lines);
                } else {
                    self.confirm(
                        format!("Copy {} ROM(s) to this card?", plan.changes()),
                        lines,
                        Pending::Copy(plan),
                    );
                }
            }
            Pending::Saves(plan) => self.review_saves(plan),
            Pending::Rename(plan) => {
                let lines = text_lines(&library_cmd::library_rename(&Staged::preview(
                    (*plan).clone(),
                )));
                self.confirm(
                    format!("Rename to {}?", plan.new_name),
                    lines,
                    Pending::Rename(plan),
                );
            }
            Pending::Standardize(plans) if plans.is_empty() => self.info(
                "Standard names",
                vec![
                    "Every game either has a standard name already or matches none.".into(),
                    "(Games verified against a DAT are never renamed.)".into(),
                ],
            ),
            Pending::Standardize(plans) => {
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
            import @ (Pending::Import(_) | Pending::CardImport(_)) => self.offer_import(import),
            _ => unreachable!("not a planned operation"),
        }
    }

    /// Called every couple of seconds by the event loop: notice a card being
    /// plugged in or pulled, and ask the side worker whether anything on the
    /// card or in the library changed.
    pub fn tick(&mut self) {
        if self.busy.is_some() {
            return;
        }
        match &self.pocket {
            // No card: look for one, so plugging it in is enough.
            None => {
                if let Ok(p) = Context::new(self.global.clone()).pocket() {
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
        if self.fingerprint_pending || self.snapshot.is_none() {
            return;
        }
        self.fingerprint_pending = true;
        self.submit_side(Job::Fingerprint);
    }

    fn clear_busy(&mut self) {
        self.busy = None;
        self.busy_step = None;
        self.download_pct = None;
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

    /// A planned import: first the standard-name question (if any file has a
    /// suggestion), then the confirm dialog.
    pub(super) fn offer_import(&mut self, import: Pending) {
        let suggestions = import.import_suggestions();
        if suggestions.is_empty() {
            self.confirm_import(import);
            return;
        }
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

    pub(super) fn confirm_import(&mut self, import: Pending) {
        let (changes, title, lines) = match &import {
            Pending::Import(b) => (
                b.1.changes(),
                format!("Import {} ROM(s) into the library?", b.1.changes()),
                text_lines(&library_cmd::library_import(&LibraryImportReport::preview(
                    b.1.clone(),
                ))),
            ),
            Pending::CardImport(plan) => (
                plan.changes(),
                format!("Copy {} ROM(s) into the library?", plan.changes()),
                text_lines(&card_import_cmd::report(&CardImportReport::preview(
                    (**plan).clone(),
                ))),
            ),
            _ => unreachable!("not an import"),
        };
        let pending = import;
        if changes == 0 {
            self.info("Nothing to import", lines);
        } else {
            self.confirm(title, lines, pending);
        }
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

pub(super) fn text_lines(text: &str) -> Vec<String> {
    text.lines().map(str::to_owned).collect()
}
