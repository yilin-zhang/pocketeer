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
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use pocketeer_core::{
    Error, ErrorInfo, Pocket,
    cores::{self, CoreList, SearchFilter},
    library::{orphans::CardOnlyRom, rename::RenamePlan},
    progress::Event,
    required,
};
use ratatui::layout::Rect;
use ratatui_image::{picker::Picker, protocol::StatefulProtocol};

mod actions;
mod dialogs;
mod keys;
mod library;
mod model;
mod pane;
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
        Staged, device as device_cmd, device::ImportReport as DeviceImportReport,
        library as library_cmd, library::ImportReport as LibraryImportReport,
        library::StandardizePlan, saves as saves_cmd,
    },
    context::Context,
    human,
};

pub struct App {
    global: Global,
    worker: Worker,
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
    /// After registering a card, open its subscription dialog once the reload lands.
    open_subscriptions_after_reload: bool,
    /// Missing covers are fetched once per session (and again after imports).
    auto_covers_done: bool,
    /// A change check is in flight on the side worker.
    fingerprint_pending: bool,
    /// The running main-worker job adds games one by one; refresh the Games
    /// tab after each step (at most one refresh in flight).
    importing: bool,
    refresh_pending: bool,
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
            open_subscriptions_after_reload: false,
            auto_covers_done: false,
            fingerprint_pending: false,
            importing: false,
            refresh_pending: false,
            global,
        }
    }

    // -----------------------------------------------------------------------
    // jobs

    pub(super) fn online(&self) -> bool {
        !self.global.offline
    }

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
        while let Some(msg) = self.worker.try_recv() {
            match msg {
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
                    if let (Ok(view), Some(snap)) = (result, &mut self.snapshot) {
                        snap.library = Some(*view);
                        self.rebuild_rows();
                    }
                }
                Msg::Loaded(result) => {
                    self.clear_busy();
                    match result {
                        Ok(snap) => {
                            self.snapshot = Some(*snap);
                            self.covers.clear();
                            self.rebuild_rows();
                            if std::mem::take(&mut self.open_subscriptions_after_reload) {
                                self.device_dialog();
                            }
                            self.auto_fetch_covers();
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
                        self.info("Cover fetch failed", lines);
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
    }

    /// A plan came back: show it, or say there is nothing to do.
    fn planned(&mut self, pending: Pending) {
        match pending {
            Pending::DeviceSync(prep) => {
                let changes = prep.plan.changes();
                let lines = text_lines(&device_cmd::device_sync(&Staged::preview(
                    prep.plan.clone(),
                )));
                if changes == 0 {
                    self.info("Device in sync", lines);
                } else {
                    let title = format!("Sync {changes} change(s) to {}?", prep.plan.device.name);
                    self.confirm(title, lines, Pending::DeviceSync(prep));
                }
            }
            Pending::Saves(plan) => {
                let changes = plan.changes();
                let lines = text_lines(&saves_cmd::saves_sync(&Staged::preview((*plan).clone())));
                if changes == 0 {
                    self.info("Saves in sync", lines);
                } else {
                    self.confirm(
                        format!("Sync {changes} save file(s)?"),
                        lines,
                        Pending::Saves(plan),
                    );
                }
            }
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
                text_lines(&device_cmd::device_import(&DeviceImportReport::preview(
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
