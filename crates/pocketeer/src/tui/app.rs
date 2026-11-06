//! The screen's state, and what each key does to it. Nothing here touches a
//! disk or the network: that is asked of the worker.
//!
//! `confirm_*` opens a window that asks yes or no; `prompt_for_*` opens one to
//! type in. Both only open it: what is asked is done once the user answers.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pocketeer_core::{
    cores::{BiosRow, BiosStatus, CoreRow},
    device::{DeviceEntry, OrphanDeviceSave},
    game::{Game, Save},
    library::{LocalEntry, OrphanLocalSave},
    log::{device_orphan_ref, library_orphan_ref},
    platform::Platform,
    pocketeer::GameRow,
};
use ratatui::widgets::TableState;
use strum::IntoEnumIterator;
use tui_input::Input;

use super::{
    covers::Covers,
    path_input::{self, DirReader, PathInput},
    worker::{Command, Row, Snapshot, Update, Worker},
};

// ============================================================================
// Status: what the screen says is going on
// ============================================================================

/// What the screen shows of a command under way.
pub struct JobStatus {
    pub description: String,
    pub done_count: usize,
    pub total_count: usize,
    /// The game last finished.
    pub last_game: Option<String>,
    pub is_cancelling: bool,
}

/// What last happened.
pub struct StatusMessage {
    pub text: String,
    /// Whether it went wrong.
    pub is_failure: bool,
}

/// Where the device is at.
pub enum DeviceStatus {
    NoDevice,
    /// Being opened and read.
    Reading(PathBuf),
    Open(PathBuf),
}

// ============================================================================
// Dialogs: the windows that ask yes or no, or only tell
// ============================================================================

/// What a button in a dialog says and does.
pub struct DialogButton {
    pub label: &'static str,
    pub command: Option<Command>,
}

impl DialogButton {
    fn new(label: &'static str, command: Option<Command>) -> Self {
        Self { label, command }
    }
}

/// A question or a text over the screen.
pub struct Dialog {
    pub title: String,
    pub lines: Vec<String>,
    /// The last has no command and cancels.
    pub buttons: Vec<DialogButton>,
    pub focused_button: usize,
    /// Whether what it asks can't be undone.
    pub is_irreversible: bool,
}

// ============================================================================
// Prompts: the windows that ask for a path, a platform or a name
// ============================================================================

/// What a path being typed is to be imported as.
pub enum ImportTarget {
    Game,
    /// A version of this game's save.
    Save(LocalEntry),
    /// The file this row asks for.
    Bios(BiosRow),
}

/// A path being typed, to import what it leads to.
pub struct ImportPrompt {
    pub target: ImportTarget,
    pub input: PathInput,
}

/// Which copy of a game is being renamed.
pub enum RenameTarget {
    InLibrary(LocalEntry),
    OnDevice(DeviceEntry),
}

/// A new name being typed for a game.
pub struct RenamePrompt {
    pub target: RenameTarget,
    input: Input,
}

impl RenamePrompt {
    /// What is typed before and after the cursor.
    pub fn text_around_cursor(&self) -> (String, String) {
        path_input::text_around_cursor(&self.input)
    }

    /// The name it has, without the extension.
    fn stem(&self) -> String {
        match &self.target {
            RenameTarget::InLibrary(local) => local.stem().to_string(),
            RenameTarget::OnDevice(on_device) => file_stem(&on_device.path),
        }
    }
}

/// The name of the file at `path`, without the extension.
fn file_stem(path: &Path) -> String {
    let stem = path.file_stem().unwrap_or_default();
    stem.to_string_lossy().into_owned()
}

/// A file to import as a game, while the user says which platform it is for.
pub struct PlatformPrompt {
    pub file_to_import: PathBuf,
    /// In `Platform::iter()`.
    pub selected_platform_index: usize,
}

/// What the user is typing or choosing in a window of its own. A dialog can
/// still open over it.
pub enum Prompt {
    Import(ImportPrompt),
    Platform(PlatformPrompt),
    Rename(RenamePrompt),
}

// ============================================================================
// Games page
// ============================================================================

/// Where the Games page's keys go.
#[derive(Clone, Copy, Default)]
enum GamesPageFocus {
    #[default]
    List,
    /// Typing in the search box.
    Search,
    /// The saves panel is open, with this version selected.
    SavesPanel(usize),
}

/// A row, whatever else changes: marks and the selection stay with it. A game
/// is its contents, whatever it is called and wherever it is.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum RowKey {
    Game(Game),
    OrphanInLibrary { platform: Platform, stem: String },
    OrphanOnDevice { platform: Platform, path: PathBuf },
}

fn key_of(row: &Row) -> RowKey {
    match row {
        Row::Game { row, .. } => RowKey::Game(row.game().clone()),
        Row::OrphanInLibrary(o) => RowKey::OrphanInLibrary {
            platform: o.platform.clone(),
            stem: o.stem.clone(),
        },
        Row::OrphanOnDevice(o) => RowKey::OrphanOnDevice {
            platform: o.platform.clone(),
            path: o.path.clone(),
        },
    }
}

/// What the table calls a row, which is also what a search looks in.
pub fn name_of(row: &Row) -> String {
    match row {
        Row::Game { row, .. } => match row {
            GameRow::InLibrary(local) | GameRow::Both { local, .. } => local.name.clone(),
            GameRow::OnDevice(on_device) => on_device.path.display().to_string(),
        },
        Row::OrphanInLibrary(o) => o.stem.clone(),
        Row::OrphanOnDevice(o) => o.path.display().to_string(),
    }
}

fn matches_query(row: &Row, query: &str) -> bool {
    let on_device = row
        .device_entry()
        .map_or(String::new(), |d| d.path.display().to_string());
    path_input::fuzzy_match(query, &name_of(row)).is_some()
        || path_input::fuzzy_match(query, &on_device).is_some()
}

/// The games and orphan saves, and what the user has done with them.
#[derive(Default)]
pub struct GamesPage {
    /// None until the worker has told of them.
    rows: Option<Vec<Row>>,
    /// Indexes into `rows` of those the search leaves.
    visible_row_indices: Vec<usize>,
    pub table_state: TableState,
    marked_rows: HashSet<RowKey>,
    search_input: Input,
    focus: GamesPageFocus,
}

impl GamesPage {
    pub fn is_loaded(&self) -> bool {
        self.rows.is_some()
    }

    /// Every row, whatever the search leaves.
    pub fn rows(&self) -> &[Row] {
        self.rows.as_deref().unwrap_or(&[])
    }

    pub fn visible_rows(&self) -> impl Iterator<Item = &Row> {
        self.visible_row_indices.iter().map(|&i| &self.rows()[i])
    }

    pub fn visible_len(&self) -> usize {
        self.visible_row_indices.len()
    }

    pub fn selected_row(&self) -> Option<&Row> {
        let &row = self.visible_row_indices.get(self.table_state.selected()?)?;
        Some(&self.rows()[row])
    }

    /// The version selected in the saves panel, if it is open.
    pub fn selected_version(&self) -> Option<usize> {
        match self.focus {
            GamesPageFocus::SavesPanel(selected) => Some(selected),
            GamesPageFocus::List | GamesPageFocus::Search => None,
        }
    }

    /// The versions of the selected game's save in the library, newest first.
    pub fn selected_save_versions(&self) -> Vec<Save> {
        let local = self.selected_row().and_then(Row::local_entry);
        let history = local.and_then(|l| l.save.as_ref());
        history.iter().flat_map(|h| h.iter().cloned()).collect()
    }

    /// Where the covers of the rows around the selected one are, to prepare
    /// them ahead.
    pub fn covers_around(&self) -> Vec<PathBuf> {
        let Some(selected) = self.table_state.selected() else {
            return Vec::new();
        };
        let at = |i: usize| {
            let &row = self.visible_row_indices.get(i)?;
            self.rows()[row].local_entry()?.cover_path.clone()
        };
        let before = selected.checked_sub(1).and_then(at);
        [at(selected + 1), before].into_iter().flatten().collect()
    }

    /// How many rows are marked, seen or not.
    pub fn marked_count(&self) -> usize {
        self.rows().iter().filter(|r| self.is_marked(r)).count()
    }

    pub fn is_marked(&self, row: &Row) -> bool {
        self.marked_rows.contains(&key_of(row))
    }

    /// How many games there are, and how many orphan saves.
    pub fn game_and_orphan_counts(&self) -> (usize, usize) {
        let is_game = |row: &&Row| matches!(row, Row::Game { .. });
        let games = self.rows().iter().filter(is_game).count();
        (games, self.rows().len() - games)
    }

    pub fn search_text(&self) -> &str {
        self.search_input.value()
    }

    /// What is typed in the search box before and after the cursor.
    pub fn search_text_around_cursor(&self) -> (String, String) {
        path_input::text_around_cursor(&self.search_input)
    }

    pub fn is_searching(&self) -> bool {
        matches!(self.focus, GamesPageFocus::Search)
    }

    /// Takes the rows the worker told of, keeping the selection on its row.
    fn set_rows(&mut self, rows: Vec<Row>) {
        let keep = self.selected_row().map(key_of);
        self.rows = Some(rows);
        self.refresh_visible_rows(keep);
    }

    /// Works out the visible rows, keeping the selection on the row `keep`.
    fn refresh_visible_rows(&mut self, keep: Option<RowKey>) {
        let found = self
            .rows()
            .iter()
            .enumerate()
            .filter(|(_, r)| matches_query(r, self.search_input.value()));
        let visible: Vec<usize> = found.map(|(i, _)| i).collect();
        let kept = keep.and_then(|k| visible.iter().position(|&i| key_of(&self.rows()[i]) == k));
        let selected = kept.or(self.table_state.selected()).unwrap_or(0);
        self.visible_row_indices = visible;
        self.select_row(selected);
    }

    fn move_selection_by(&mut self, by: isize) {
        let selected = self.table_state.selected().unwrap_or(0);
        self.select_row(selected.saturating_add_signed(by));
    }

    /// Selects row `i`, or the last if there are fewer.
    fn select_row(&mut self, i: usize) {
        let row = clamped_row(i, self.visible_row_indices.len());
        self.table_state.select(row);
    }

    /// Marks the row, or not, and moves on to the next.
    fn set_mark_and_move_on(&mut self, mark: bool) {
        let Some(key) = self.selected_row().map(key_of) else {
            return;
        };
        if mark {
            self.marked_rows.insert(key);
        } else {
            self.marked_rows.remove(&key);
        }
        self.move_selection_by(1);
    }

    /// What a key acts on: the rows marked, seen or not, or else the one
    /// selected. In the saves panel, always the one selected.
    fn rows_to_act_on(&self) -> Vec<&Row> {
        let marked: Vec<&Row> = self.rows().iter().filter(|r| self.is_marked(r)).collect();
        if marked.is_empty() || self.selected_version().is_some() {
            self.selected_row().into_iter().collect()
        } else {
            marked
        }
    }

    /// The copies on the device among the rows to act on, and how many rows
    /// had none.
    fn device_entries_to_act_on(&self) -> (Vec<DeviceEntry>, usize) {
        let rows = self.rows_to_act_on();
        let entries: Vec<DeviceEntry> = rows
            .iter()
            .filter_map(|r| r.device_entry().cloned())
            .collect();
        let inapplicable = rows.len() - entries.len();
        (entries, inapplicable)
    }

    /// The games in the library among the rows to act on, and how many rows
    /// had none.
    fn library_entries_to_act_on(&self) -> (Vec<LocalEntry>, usize) {
        let rows = self.rows_to_act_on();
        let entries: Vec<LocalEntry> = rows
            .iter()
            .filter_map(|r| r.local_entry().cloned())
            .collect();
        let inapplicable = rows.len() - entries.len();
        (entries, inapplicable)
    }
}

// ============================================================================
// Cores page
// ============================================================================

/// The cores as the worker last told of them.
pub struct CoreList {
    pub rows: Vec<CoreRow>,
    /// Why the inventory couldn't be had, if it couldn't.
    pub inventory_failure: Option<String>,
}

#[derive(Default)]
pub struct CoresPage {
    /// None until the worker has told of them.
    core_list: Option<CoreList>,
    pub table_state: TableState,
}

impl CoresPage {
    pub fn core_list(&self) -> Option<&CoreList> {
        self.core_list.as_ref()
    }

    pub fn selected_row(&self) -> Option<&CoreRow> {
        let selected = self.table_state.selected()?;
        self.core_list.as_ref()?.rows.get(selected)
    }

    /// Takes the cores the worker told of, keeping the selection on its core.
    fn set_rows(&mut self, rows: Vec<CoreRow>, inventory_failure: Option<String>) {
        let keep = self.selected_row().map(|r| r.id().to_string());
        let kept = keep.and_then(|id| rows.iter().position(|r| r.id() == id));
        let selected = kept.or(self.table_state.selected()).unwrap_or(0);
        self.core_list = Some(CoreList {
            rows,
            inventory_failure,
        });
        self.select_row(selected);
    }

    /// Selects core `i`, or the last if there are fewer.
    fn select_row(&mut self, i: usize) {
        let len = self.core_list.as_ref().map_or(0, |c| c.rows.len());
        self.table_state.select(clamped_row(i, len));
    }
}

// ============================================================================
// BIOS page
// ============================================================================

#[derive(Default)]
pub struct BiosPage {
    rows: Vec<BiosRow>,
    pub table_state: TableState,
}

impl BiosPage {
    pub fn rows(&self) -> &[BiosRow] {
        &self.rows
    }

    pub fn selected_row(&self) -> Option<&BiosRow> {
        self.rows.get(self.table_state.selected()?)
    }

    /// Takes the files the worker told of, keeping the selection on its file.
    fn set_rows(&mut self, rows: Vec<BiosRow>) {
        let same = |a: &BiosRow, b: &BiosRow| a.platform == b.platform && a.path == b.path;
        let keep = self.selected_row().cloned();
        let kept = keep.and_then(|k| rows.iter().position(|r| same(r, &k)));
        let selected = kept.or(self.table_state.selected()).unwrap_or(0);
        self.rows = rows;
        self.select_row(selected);
    }

    /// Selects file `i`, or the last if there are fewer.
    fn select_row(&mut self, i: usize) {
        self.table_state.select(clamped_row(i, self.rows.len()));
    }
}

// ============================================================================
// The app
// ============================================================================

/// Which page is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Games,
    Cores,
    Bios,
}

const NOT_ON_BOTH: &str = "nothing here is both in the library and on the device";

pub struct App {
    // Threads.
    worker: Worker,
    pub covers: Covers,
    dir_reader: DirReader,
    // Pages.
    current_page: Page,
    pub games: GamesPage,
    pub cores: CoresPage,
    pub bios: BiosPage,
    // Windows over them.
    dialog: Option<Dialog>,
    /// Shown once `dialog` is closed, so as not to cover it.
    next_dialog: Option<Dialog>,
    prompt: Option<Prompt>,
    // What is going on.
    device_status: DeviceStatus,
    job_status: Option<JobStatus>,
    status_message: Option<StatusMessage>,
    /// How many rows the command just sent doesn't apply to, said with its
    /// summary.
    inapplicable_row_count: usize,
    should_quit: bool,
}

impl App {
    pub fn new(worker: Worker, covers: Covers, dir_reader: DirReader) -> Self {
        Self {
            worker,
            covers,
            dir_reader,
            current_page: Page::Games,
            games: GamesPage::default(),
            cores: CoresPage::default(),
            bios: BiosPage::default(),
            dialog: None,
            next_dialog: None,
            prompt: None,
            device_status: DeviceStatus::NoDevice,
            job_status: None,
            status_message: None,
            inapplicable_row_count: 0,
            should_quit: false,
        }
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub fn is_busy(&self) -> bool {
        self.job_status.is_some() || matches!(self.device_status, DeviceStatus::Reading(_))
    }

    pub fn device_status(&self) -> &DeviceStatus {
        &self.device_status
    }

    /// Where the open device is. None while one is still being read.
    pub fn device_root(&self) -> Option<&Path> {
        match &self.device_status {
            DeviceStatus::Open(root) => Some(root),
            DeviceStatus::NoDevice | DeviceStatus::Reading(_) => None,
        }
    }

    pub fn job_status(&self) -> Option<&JobStatus> {
        self.job_status.as_ref()
    }

    pub fn status_message(&self) -> Option<&StatusMessage> {
        self.status_message.as_ref()
    }

    pub fn dialog(&self) -> Option<&Dialog> {
        self.dialog.as_ref()
    }

    pub fn prompt(&self) -> Option<&Prompt> {
        self.prompt.as_ref()
    }

    pub fn current_page(&self) -> Page {
        self.current_page
    }

    // ---- Events from the threads -----------------------------------------------

    pub fn on_cover_ready(&mut self) {
        self.covers.collect_finished();
    }

    pub fn on_dir_read(&mut self) {
        for (dir, entries) in self.dir_reader.dirs_read() {
            if let Some(Prompt::Import(import)) = &mut self.prompt {
                import.input.set_entries(dir, entries);
            }
        }
    }

    pub fn update(&mut self, update: Update) {
        match update {
            Update::Snapshot(Snapshot { rows, device }) => {
                self.games.set_rows(rows);
                self.device_status = match device {
                    Some(root) => DeviceStatus::Open(root),
                    None => DeviceStatus::NoDevice,
                };
            }
            Update::Cores { rows, failure } => self.cores.set_rows(rows, failure),
            Update::Bios(rows) => self.bios.set_rows(rows),
            Update::ReadingDevice(root) => self.device_status = DeviceStatus::Reading(root),
            Update::DeviceRead => {
                if matches!(self.device_status, DeviceStatus::Reading(_)) {
                    self.device_status = DeviceStatus::NoDevice;
                }
            }
            Update::Started { label, total } => {
                self.job_status = Some(JobStatus {
                    description: label,
                    done_count: 0,
                    total_count: total,
                    last_game: None,
                    is_cancelling: false,
                });
            }
            Update::Progress { done, total, game } => {
                if let Some(job) = &mut self.job_status {
                    (job.done_count, job.total_count, job.last_game) = (done, total, Some(game));
                }
            }
            Update::Finished { summary, failed } => {
                self.job_status = None;
                let summary = match std::mem::take(&mut self.inapplicable_row_count) {
                    0 => summary,
                    n => format!("{summary} ({n} left out)"),
                };
                self.status_message = Some(StatusMessage {
                    text: summary,
                    is_failure: failed,
                });
            }
            // The event loop ends on this, before it gets here.
            Update::LibraryFailed(_) => {}
            Update::Duplicates(lines) => {
                let dialog = duplicates_dialog(lines);
                if self.dialog.is_some() {
                    self.next_dialog = Some(dialog);
                } else {
                    self.dialog = Some(dialog);
                }
            }
        }
    }

    // ---- Keys: where each goes -------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        // The keys that edit text go on as they are; for the others, Emacs keys
        // stand for the keys they mean.
        let raw_key = key;
        let key = translate_emacs_key(key);
        if self.dialog.is_some() {
            self.handle_dialog_key(key);
            if self.dialog.is_none() {
                self.dialog = self.next_dialog.take();
            }
            return;
        }
        match self.prompt {
            Some(Prompt::Import(_)) => return self.handle_import_key(raw_key),
            Some(Prompt::Rename(_)) => return self.handle_rename_key(raw_key),
            Some(Prompt::Platform(_)) => return self.handle_platform_prompt_key(key),
            None => {}
        }
        match self.current_page {
            Page::Games => {}
            Page::Cores => return self.handle_cores_key(key),
            Page::Bios => return self.handle_bios_key(key),
        }
        match self.games.focus {
            GamesPageFocus::List => self.handle_list_key(key),
            GamesPageFocus::Search => self.handle_search_key(key),
            GamesPageFocus::SavesPanel(selected) => self.handle_saves_panel_key(key, selected),
        }
    }

    /// Closes what is open, or else stops what is running.
    fn handle_escape(&mut self) {
        if !self.games.search_input.value().is_empty() {
            let keep = self.games.selected_row().map(key_of);
            self.games.search_input.reset();
            self.games.refresh_visible_rows(keep);
        } else if let Some(job) = &mut self.job_status {
            job.is_cancelling = true;
            self.worker.cancel();
        }
    }

    // ---- Keys on the Games page ------------------------------------------------

    fn handle_search_key(&mut self, key: KeyEvent) {
        let keep = self.games.selected_row().map(key_of);
        match key.code {
            KeyCode::Enter => self.games.focus = GamesPageFocus::List,
            KeyCode::Esc => {
                self.games.focus = GamesPageFocus::List;
                self.games.search_input.reset();
            }
            KeyCode::Down | KeyCode::Up | KeyCode::PageDown | KeyCode::PageUp => {
                self.games.focus = GamesPageFocus::List;
                self.handle_list_key(key);
                return;
            }
            _ => {
                path_input::edit(&mut self.games.search_input, key);
            }
        }
        self.games.refresh_visible_rows(keep);
    }

    fn handle_list_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.games.move_selection_by(1),
            KeyCode::Up | KeyCode::Char('k') => self.games.move_selection_by(-1),
            KeyCode::PageDown => self.games.move_selection_by(PAGE),
            KeyCode::PageUp => self.games.move_selection_by(-PAGE),
            KeyCode::Char('g') | KeyCode::Home => self.games.select_row(0),
            KeyCode::Char('G') | KeyCode::End => self.games.select_row(usize::MAX),
            KeyCode::Char('m') => self.games.set_mark_and_move_on(true),
            KeyCode::Char('u') => self.games.set_mark_and_move_on(false),
            KeyCode::Char('M') => {
                let keys: Vec<RowKey> = self.games.visible_rows().map(key_of).collect();
                self.games.marked_rows.extend(keys);
            }
            KeyCode::Char('U') => self.games.marked_rows.clear(),
            KeyCode::Char('i') => self.send_for_device_rows(Command::PullGames),
            KeyCode::Char('b') => self.send_for_device_rows(Command::PullSaves),
            KeyCode::Char('I') => self.prompt_for_import_path(ImportTarget::Game),
            KeyCode::Char('n') => self.prompt_for_name_in_library(),
            KeyCode::Char('N') => self.prompt_for_name_on_device(),
            KeyCode::Char('s') => self.send_for_library_rows(Command::PushGames),
            KeyCode::Char('f') => self.send_for_library_rows(Command::FetchCovers),
            KeyCode::Char('p') => self.push_saves(),
            KeyCode::Char('x') => self.confirm_trash_games(),
            KeyCode::Char('X') => self.confirm_delete_games(),
            KeyCode::Char('d') => self.worker.send(Command::SyncDats),
            KeyCode::Char('E') => self.eject(),
            KeyCode::Char('r') => self.worker.send(Command::Reload),
            KeyCode::Enter | KeyCode::Right => self.open_panel(),
            KeyCode::Tab | KeyCode::Char('2') => self.show_cores(),
            KeyCode::Char('3') => self.current_page = Page::Bios,
            KeyCode::Char('/') => self.games.focus = GamesPageFocus::Search,
            KeyCode::Char('?') => self.dialog = Some(help_dialog()),
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Esc => self.handle_escape(),
            _ => {}
        }
    }

    fn handle_saves_panel_key(&mut self, key: KeyEvent, selected: usize) {
        let len = self.games.selected_save_versions().len();
        let selected = selected.min(len.saturating_sub(1));
        match key.code {
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') => {
                self.games.focus = GamesPageFocus::List
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.games.focus =
                    GamesPageFocus::SavesPanel((selected + 1).min(len.saturating_sub(1)));
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.games.focus = GamesPageFocus::SavesPanel(selected.saturating_sub(1))
            }
            KeyCode::Char('b') => self.send_for_device_rows(Command::PullSaves),
            KeyCode::Char('p') => self.push_save_version(selected),
            KeyCode::Char('I') => self.prompt_for_save_to_import(),
            KeyCode::Char('x') => self.confirm_trash_version(selected),
            KeyCode::Char('X') => self.confirm_delete_save(),
            KeyCode::Char('?') => self.dialog = Some(help_dialog()),
            KeyCode::Char('q') => self.should_quit = true,
            _ => {}
        }
    }

    fn open_panel(&mut self) {
        if let Some(Row::Game { .. }) = self.games.selected_row() {
            self.games.focus = GamesPageFocus::SavesPanel(0);
        }
    }

    fn eject(&mut self) {
        match self.device_root() {
            Some(_) => self.worker.send(Command::Eject),
            None => self.show_status_message("no device is open"),
        }
    }

    // ---- Keys on the Cores page ------------------------------------------------

    /// Opens the Cores page, asking for them unless they have been told of.
    fn show_cores(&mut self) {
        self.current_page = Page::Cores;
        if self.cores.core_list.is_none() {
            self.worker.send(Command::ShowCores);
        }
    }

    fn handle_cores_key(&mut self, key: KeyEvent) {
        if let Some(i) = row_after_key(key, self.cores.table_state.selected().unwrap_or(0)) {
            return self.cores.select_row(i);
        }
        match key.code {
            KeyCode::Char('1') => self.current_page = Page::Games,
            KeyCode::Tab | KeyCode::Char('3') => self.current_page = Page::Bios,
            KeyCode::Char('u') => self.install_selected_core(),
            KeyCode::Char('U') => self.update_every_core(),
            KeyCode::Char('x') => self.confirm_remove_core(),
            KeyCode::Char('r') => self.worker.send(Command::Reload),
            KeyCode::Char('?') => self.dialog = Some(help_dialog()),
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Esc => self.handle_escape(),
            _ => {}
        }
    }

    fn install_selected_core(&mut self) {
        let Some(row) = self.cores.selected_row() else {
            return;
        };
        let id = row.id().to_string();
        let why_not = match row {
            CoreRow::Installed(_) => Some("only the cores with a latest version can be installed"),
            CoreRow::Both { .. } if !row.has_update() => Some("the device has the newest version"),
            _ if self.device_root().is_none() => Some("no device is open"),
            _ => None,
        };
        match why_not {
            Some(why) => self.show_status_message(why),
            None => self.worker.send(Command::InstallCores(vec![id])),
        }
    }

    fn update_every_core(&mut self) {
        let rows = self.cores.core_list.as_ref().map_or(&[][..], |c| &c.rows);
        let with_updates = rows.iter().filter(|r| r.has_update());
        let ids: Vec<String> = with_updates.map(|r| r.id().to_string()).collect();
        if ids.is_empty() {
            self.show_status_message("every core is up to date");
        } else {
            self.worker.send(Command::InstallCores(ids));
        }
    }

    fn confirm_remove_core(&mut self) {
        let Some(row) = self
            .cores
            .selected_row()
            .filter(|r| r.installed().is_some())
        else {
            self.show_status_message("this core isn't on the device");
            return;
        };
        let id = row.id().to_string();
        self.dialog = Some(Dialog {
            title: format!("Delete {id} from the device"),
            lines: vec![
                "This deletes the core for good: there is no trash there.".to_string(),
                "Its settings and saves stay on the device.".to_string(),
            ],
            buttons: vec![
                DialogButton::new("Delete", Some(Command::RemoveCores(vec![id]))),
                DialogButton::new("Cancel", None),
            ],
            focused_button: 1,
            is_irreversible: true,
        });
    }

    // ---- Keys on the BIOS page -------------------------------------------------

    fn handle_bios_key(&mut self, key: KeyEvent) {
        if let Some(i) = row_after_key(key, self.bios.table_state.selected().unwrap_or(0)) {
            return self.bios.select_row(i);
        }
        match key.code {
            KeyCode::Tab | KeyCode::Char('1') => self.current_page = Page::Games,
            KeyCode::Char('2') => self.show_cores(),
            KeyCode::Char('I') => self.prompt_for_bios_file(),
            KeyCode::Char('r') => self.worker.send(Command::Reload),
            KeyCode::Char('?') => self.dialog = Some(help_dialog()),
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Esc => self.handle_escape(),
            _ => {}
        }
    }

    // ---- Prompts ---------------------------------------------------------------

    fn handle_import_key(&mut self, key: KeyEvent) {
        let Some(Prompt::Import(import)) = &mut self.prompt else {
            return;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let cancelling = key.code == KeyCode::Esc || (ctrl && key.code == KeyCode::Char('g'));
        if cancelling {
            if !import.input.unpick() {
                self.prompt = None;
            }
            return;
        }
        let takes_dir = matches!(import.target, ImportTarget::Game);
        if key.code != KeyCode::Enter {
            import.input.handle_key(key);
        } else if !import.input.fill_in_picked() && (takes_dir || !import.input.is_dir()) {
            self.import_input_path();
            return;
        }
        self.request_input_dir();
    }

    /// Imports what the path typed leads to, or else, for a game whose
    /// extension doesn't tell its platform, asks which it is for.
    fn import_input_path(&mut self) {
        let Some(Prompt::Import(import)) = self.prompt.take() else {
            return;
        };
        let path = import.input.path();
        match import.target {
            ImportTarget::Save(local) => self.worker.send(Command::ImportSave(local, path)),
            ImportTarget::Bios(row) => self.worker.send(Command::PutBios(row, path)),
            ImportTarget::Game => {
                let extension = path.extension().and_then(|e| e.to_str());
                let tells_platform = extension.and_then(Platform::from_extension).is_some();
                if tells_platform || import.input.names_dir() {
                    self.worker.send(Command::ImportGame(path, None));
                } else {
                    self.prompt = Some(Prompt::Platform(PlatformPrompt {
                        file_to_import: path,
                        selected_platform_index: 0,
                    }));
                }
            }
        }
    }

    fn handle_rename_key(&mut self, key: KeyEvent) {
        let Some(Prompt::Rename(rename)) = &mut self.prompt else {
            return;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if key.code == KeyCode::Esc || (ctrl && key.code == KeyCode::Char('g')) {
            self.prompt = None;
        } else if key.code != KeyCode::Enter {
            path_input::edit(&mut rename.input, key);
        } else if let Some(Prompt::Rename(rename)) = self.prompt.take() {
            let new = rename.input.value().trim().to_string();
            if new != rename.stem() {
                self.worker.send(match rename.target {
                    RenameTarget::InLibrary(local) => Command::RenameInLibrary(local, new),
                    RenameTarget::OnDevice(on_device) => Command::RenameOnDevice(on_device, new),
                });
            }
        }
    }

    /// Asks what to call the selected row's copy in the library.
    fn prompt_for_name_in_library(&mut self) {
        let Some(local) = self
            .games
            .selected_row()
            .and_then(Row::local_entry)
            .cloned()
        else {
            self.show_status_message("nothing here is in the library");
            return;
        };
        self.open_rename_prompt(RenameTarget::InLibrary(local));
    }

    /// Asks what to call the selected row's copy on the device.
    fn prompt_for_name_on_device(&mut self) {
        let Some(on_device) = self
            .games
            .selected_row()
            .and_then(Row::device_entry)
            .cloned()
        else {
            self.show_status_message("this game isn't on the device");
            return;
        };
        self.open_rename_prompt(RenameTarget::OnDevice(on_device));
    }

    fn open_rename_prompt(&mut self, target: RenameTarget) {
        let mut rename = RenamePrompt {
            target,
            input: Input::default(),
        };
        rename.input = Input::new(rename.stem());
        self.prompt = Some(Prompt::Rename(rename));
    }

    fn handle_platform_prompt_key(&mut self, key: KeyEvent) {
        let Some(Prompt::Platform(choice)) = &mut self.prompt else {
            return;
        };
        let last = Platform::iter().count().saturating_sub(1);
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                choice.selected_platform_index = (choice.selected_platform_index + 1).min(last)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                choice.selected_platform_index = choice.selected_platform_index.saturating_sub(1)
            }
            KeyCode::Enter => {
                let platform = Platform::iter().nth(choice.selected_platform_index);
                let file = choice.file_to_import.clone();
                self.prompt = None;
                self.worker.send(Command::ImportGame(file, platform));
            }
            KeyCode::Esc => self.prompt = None,
            _ => {}
        }
    }

    /// Asks for the folder of the path being typed, if it has changed.
    fn request_input_dir(&mut self) {
        if let Some(Prompt::Import(import)) = &mut self.prompt
            && let Some(dir) = import.input.dir_to_read()
        {
            self.dir_reader.request_dir(dir);
        }
    }

    fn prompt_for_save_to_import(&mut self) {
        let Some(local) = self
            .games
            .selected_row()
            .and_then(Row::local_entry)
            .cloned()
        else {
            self.show_status_message("nothing here is in the library");
            return;
        };
        self.prompt_for_import_path(ImportTarget::Save(local));
    }

    fn prompt_for_import_path(&mut self, target: ImportTarget) {
        self.prompt = Some(Prompt::Import(ImportPrompt {
            target,
            input: PathInput::new("~/"),
        }));
        self.request_input_dir();
    }

    fn prompt_for_bios_file(&mut self) {
        match self.bios.selected_row().cloned() {
            None if self.device_root().is_none() => self.show_status_message("no device is open"),
            None => self.show_status_message("no core on the device needs a file"),
            Some(row) if row.status != BiosStatus::Missing => {
                self.show_status_message("the device has this file already");
            }
            Some(row) => self.prompt_for_import_path(ImportTarget::Bios(row)),
        }
    }

    // ---- Dialogs ---------------------------------------------------------------

    fn handle_dialog_key(&mut self, key: KeyEvent) {
        let Some(dialog) = &mut self.dialog else {
            return;
        };
        let last = dialog.buttons.len().saturating_sub(1);
        match key.code {
            KeyCode::Left | KeyCode::Char('h') => {
                dialog.focused_button = dialog.focused_button.saturating_sub(1)
            }
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Tab => {
                dialog.focused_button = (dialog.focused_button + 1).min(last);
            }
            KeyCode::Enter | KeyCode::Char('y') => {
                let command = dialog
                    .buttons
                    .get_mut(dialog.focused_button)
                    .and_then(|b| b.command.take());
                self.dialog = None;
                match command {
                    Some(command) => {
                        self.games.marked_rows.clear();
                        self.worker.send(command);
                    }
                    None => self.inapplicable_row_count = 0,
                }
            }
            KeyCode::Esc | KeyCode::Char('n' | 'q' | '?') => {
                self.dialog = None;
                self.inapplicable_row_count = 0;
            }
            _ => {}
        }
    }

    fn confirm_trash_games(&mut self) {
        let (entries, inapplicable) = self.games.library_entries_to_act_on();
        if entries.is_empty() {
            self.confirm_trash_orphans();
            return;
        }
        let rows = self.games.rows_to_act_on();
        let mut files = Vec::new();
        for row in rows {
            if let Row::Game { files: f, .. } = row {
                for file in f {
                    if !files.contains(file) {
                        files.push(file.clone());
                    }
                }
            }
        }
        let mut lines = vec![
            "These go to the system's trash. Their saves stay.".to_string(),
            String::new(),
        ];
        lines.extend(cut_short_for_dialog(
            files.iter().map(|f| f.display().to_string()).collect(),
        ));
        self.inapplicable_row_count += inapplicable;
        self.dialog = Some(Dialog {
            title: format!("Trash {}", pluralized(entries.len(), "game")),
            lines,
            buttons: vec![
                DialogButton::new("Trash", Some(Command::TrashGames(entries))),
                DialogButton::new("Cancel", None),
            ],
            focused_button: 0,
            is_irreversible: false,
        });
    }

    fn confirm_trash_orphans(&mut self) {
        let targets = self.games.rows_to_act_on();
        let orphans = targets.iter().filter_map(|r| r.orphan_in_library());
        let orphans: Vec<OrphanLocalSave> = orphans.cloned().collect();
        if orphans.is_empty() {
            self.show_status_message("nothing here is in the library");
            return;
        }
        let mut lines = vec![
            "Every version goes to the system's trash.".to_string(),
            String::new(),
        ];
        lines.extend(cut_short_for_dialog(
            orphans.iter().map(library_orphan_ref).collect(),
        ));
        self.dialog = Some(Dialog {
            title: format!("Trash {}", pluralized(orphans.len(), "orphan save")),
            lines,
            buttons: vec![
                DialogButton::new("Trash", Some(Command::TrashOrphanSaves(orphans))),
                DialogButton::new("Cancel", None),
            ],
            focused_button: 1,
            is_irreversible: false,
        });
    }

    fn confirm_trash_version(&mut self, selected: usize) {
        let Some(local) = self
            .games
            .selected_row()
            .and_then(Row::local_entry)
            .cloned()
        else {
            return;
        };
        let Some(version) = self.games.selected_save_versions().get(selected).cloned() else {
            return;
        };
        self.dialog = Some(Dialog {
            title: "Trash a save".to_string(),
            lines: vec![format!(
                "Version {} goes to the system's trash.",
                version.timestamp()
            )],
            buttons: vec![
                DialogButton::new("Trash", Some(Command::TrashSaveVersion(local, version))),
                DialogButton::new("Cancel", None),
            ],
            focused_button: 0,
            is_irreversible: false,
        });
    }

    fn confirm_delete_games(&mut self) {
        let (entries, inapplicable) = self.games.device_entries_to_act_on();
        if entries.is_empty() {
            self.confirm_delete_orphans();
            return;
        }
        let names: Vec<String> = entries
            .iter()
            .map(|e| e.path.display().to_string())
            .collect();
        let one = entries.len() == 1;
        let (it, them) = if one {
            ("it", "It isn't")
        } else {
            ("them", "Some aren't")
        };
        let mut lines = vec![format!(
            "This deletes {it} from the device for good: there is no trash there."
        )];
        let only_here = self.games.rows_to_act_on().iter().any(|r| {
            matches!(
                r,
                Row::Game {
                    row: GameRow::OnDevice(_),
                    ..
                }
            )
        });
        if only_here {
            lines.push(format!("{them} in the library."));
        }
        lines.push(if one {
            "Its save stays on the device.".to_string()
        } else {
            "Their saves stay on the device.".to_string()
        });
        lines.push(String::new());
        lines.extend(cut_short_for_dialog(names));
        self.inapplicable_row_count += inapplicable;
        self.dialog = Some(Dialog {
            title: format!(
                "Delete {} from the device",
                pluralized(entries.len(), "game")
            ),
            lines,
            buttons: vec![
                DialogButton::new("Delete", Some(Command::DeleteGames(entries))),
                DialogButton::new("Cancel", None),
            ],
            focused_button: 0,
            is_irreversible: true,
        });
    }

    fn confirm_delete_orphans(&mut self) {
        let targets = self.games.rows_to_act_on();
        let orphans = targets.iter().filter_map(|r| r.orphan_on_device());
        let orphans: Vec<OrphanDeviceSave> = orphans.cloned().collect();
        if orphans.is_empty() {
            self.show_status_message("nothing here is on the device");
            return;
        }
        let mut lines = vec![
            "This deletes them from the device for good: there is no trash there.".to_string(),
            String::new(),
        ];
        lines.extend(cut_short_for_dialog(
            orphans.iter().map(device_orphan_ref).collect(),
        ));
        self.dialog = Some(Dialog {
            title: format!(
                "Delete {} from the device",
                pluralized(orphans.len(), "orphan save")
            ),
            lines,
            buttons: vec![
                DialogButton::new("Delete", Some(Command::DeleteOrphanSaves(orphans))),
                DialogButton::new("Cancel", None),
            ],
            focused_button: 1,
            is_irreversible: true,
        });
    }

    fn confirm_delete_save(&mut self) {
        let (entries, _) = self.games.device_entries_to_act_on();
        if entries.is_empty() {
            self.show_status_message("nothing here is on the device");
            return;
        }
        let mut lines = vec![
            "This deletes the save from the device for good: there is no trash there.".to_string(),
        ];
        let unsaved = self
            .games
            .rows_to_act_on()
            .iter()
            .any(|r| matches!(r, Row::Game { row, .. } if row.has_new_save_on_device()));
        if unsaved {
            lines.push("The library doesn't have this save.".to_string());
        }
        self.dialog = Some(Dialog {
            title: "Delete a save from the device".to_string(),
            lines,
            buttons: vec![
                DialogButton::new("Delete", Some(Command::DeleteSaves(entries))),
                DialogButton::new("Cancel", None),
            ],
            focused_button: 1,
            is_irreversible: true,
        });
    }

    // ---- Commands for the worker -----------------------------------------------

    fn send_for_device_rows(&mut self, command: fn(Vec<DeviceEntry>) -> Command) {
        let (entries, inapplicable) = self.games.device_entries_to_act_on();
        self.send_for_entries(
            entries,
            inapplicable,
            command,
            "nothing here is on the device",
        );
    }

    fn send_for_library_rows(&mut self, command: fn(Vec<LocalEntry>) -> Command) {
        let (entries, inapplicable) = self.games.library_entries_to_act_on();
        self.send_for_entries(
            entries,
            inapplicable,
            command,
            "nothing here is in the library",
        );
    }

    fn send_for_entries<T>(
        &mut self,
        entries: Vec<T>,
        inapplicable: usize,
        command: impl FnOnce(Vec<T>) -> Command,
        message_if_none: &str,
    ) {
        if entries.is_empty() {
            self.show_status_message(message_if_none);
            return;
        }
        self.inapplicable_row_count += inapplicable;
        self.games.marked_rows.clear();
        self.worker.send(command(entries));
    }

    /// The newest versions, onto the copies among the targets with their
    /// game in the library.
    fn push_saves(&mut self) {
        let targets = self.games.rows_to_act_on();
        let entries: Vec<DeviceEntry> = targets
            .iter()
            .filter_map(|r| match r {
                Row::Game {
                    row: GameRow::Both { on_device, .. },
                    ..
                } => Some(on_device.clone()),
                _ => None,
            })
            .collect();
        let inapplicable = targets.len() - entries.len();
        self.send_for_entries(entries, inapplicable, Command::PushSaves, NOT_ON_BOTH);
    }

    /// The version selected in the saves panel, onto the selected row's copy.
    fn push_save_version(&mut self, selected: usize) {
        let version = self.games.selected_save_versions().get(selected).cloned();
        let on_device = self
            .games
            .selected_row()
            .and_then(Row::device_entry)
            .cloned();
        match (on_device, version) {
            (Some(on_device), Some(version)) => {
                self.worker
                    .send(Command::PushSaveVersion(on_device, version));
            }
            _ => self.show_status_message(NOT_ON_BOTH),
        }
    }

    /// Tells why a key did nothing.
    fn show_status_message(&mut self, text: &str) {
        self.status_message = Some(StatusMessage {
            text: text.to_string(),
            is_failure: false,
        });
    }
}

// ============================================================================
// Helpers
// ============================================================================

/// How far a page key moves.
const PAGE: isize = 20;

fn pluralized(n: usize, what: &str) -> String {
    match n {
        1 => format!("1 {what}"),
        n => format!("{n} {what}s"),
    }
}

/// `lines`, cut short for a dialog.
fn cut_short_for_dialog(mut lines: Vec<String>) -> Vec<String> {
    const SHOWN: usize = 10;
    if lines.len() > SHOWN {
        let more = lines.len() - SHOWN;
        lines.truncate(SHOWN);
        lines.push(format!("… and {more} more"));
    }
    lines
}

/// Emacs keys, as the keys they stand for.
fn translate_emacs_key(key: KeyEvent) -> KeyEvent {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let code = match key.code {
        KeyCode::Char('n') if ctrl => KeyCode::Down,
        KeyCode::Char('p') if ctrl => KeyCode::Up,
        KeyCode::Char('v') if ctrl => KeyCode::PageDown,
        KeyCode::Char('v') if alt => KeyCode::PageUp,
        KeyCode::Char('g') if ctrl => KeyCode::Esc,
        KeyCode::Char('f') if ctrl => KeyCode::Right,
        KeyCode::Char('b') if ctrl => KeyCode::Left,
        _ => return key,
    };
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn duplicates_dialog(duplicates: Vec<String>) -> Dialog {
    let mut lines = vec![
        "These copies are left out, and their saves aren't synced.".to_string(),
        "To clean them up, use a file manager.".to_string(),
        String::new(),
    ];
    lines.extend(cut_short_for_dialog(duplicates));
    Dialog {
        title: "Duplicate games on the device".to_string(),
        lines,
        buttons: vec![DialogButton::new("Close", None)],
        focused_button: 0,
        is_irreversible: false,
    }
}

/// Row `i` of `len`, or the last if there are fewer; none if there are none.
fn clamped_row(i: usize, len: usize) -> Option<usize> {
    (len > 0).then(|| i.min(len - 1))
}

/// The row a key moves the selection to, from row `at`, if it is such a key.
/// It may be past the last.
fn row_after_key(key: KeyEvent, at: usize) -> Option<usize> {
    Some(match key.code {
        KeyCode::Down | KeyCode::Char('j') => at + 1,
        KeyCode::Up | KeyCode::Char('k') => at.saturating_sub(1),
        KeyCode::PageDown => at + PAGE as usize,
        KeyCode::PageUp => at.saturating_sub(PAGE as usize),
        KeyCode::Char('g') | KeyCode::Home => 0,
        KeyCode::Char('G') | KeyCode::End => usize::MAX,
        _ => return None,
    })
}

fn help_dialog() -> Dialog {
    let lines = [
        "Move      j k, arrows, C-n C-p; C-v M-v page; g G top, bottom",
        "Mark      m mark, u unmark; M mark all shown, U unmark all",
        "          Keys act on what is marked, or else on the selected row.",
        "",
        "Library   i pull the game; b back up the save; I import a game from a file",
        "Device    s push the game; p push the newest save; E eject",
        "Covers    f download; d update the DATs, which name games",
        "Rename    n in the library; N on the device; saves and cover go with it",
        "Remove    x game and cover to the system's trash",
        "          X delete from the device: for good, it asks",
        "          On an orphan save, they remove the save.",
        "",
        "Saves     Enter opens them; there p pushes the version selected,",
        "          x trashes it, X deletes the device's save,",
        "          I imports one from a file, Esc goes back",
        "",
        "Pages     Tab the next one; 1 games, 2 cores, 3 BIOS",
        "Cores     u installs or updates, U updates all, x deletes from the device",
        "BIOS      I puts a file of yours where the cores look for it",
        "",
        "Others    / search; r read everything again; ? this; q quit",
        "          Esc closes the search, then stops what is running,",
        "          after the game under way.",
    ];
    Dialog {
        title: "Help".to_string(),
        lines: lines.iter().map(ToString::to_string).collect(),
        buttons: vec![DialogButton::new("Close", None)],
        focused_button: 0,
        is_irreversible: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::{ui, worker::Sent};
    use pocketeer_core::{
        cores::{BiosRow, BiosStatus, InstalledCore, InventoryCore},
        device::OrphanDeviceSave,
        game::{Files, Game, Rom, SaveHistory},
        platform::Platform,
    };
    use ratatui::{Terminal, backend::TestBackend};
    use ratatui_image::picker::Picker;
    use std::{
        path::{Path, PathBuf},
        sync::{atomic::Ordering, mpsc::Receiver},
        time::{Duration, UNIX_EPOCH},
    };

    fn game(crc32: u32) -> Game {
        Game {
            platform: Platform::Gba,
            files: Files::Single(Rom { size: 1, crc32 }),
        }
    }

    fn local(name: &str, crc32: u32, saves: &[&[u8]]) -> LocalEntry {
        let at = |i: usize| UNIX_EPOCH + Duration::from_secs(1_000 * (i as u64 + 1));
        let saves = saves
            .iter()
            .enumerate()
            .map(|(i, b)| Save::from_bytes(b, at(i)));
        LocalEntry {
            name: name.into(),
            game: game(crc32),
            cover_path: None,
            save: SaveHistory::from_saves(saves.collect()),
        }
    }

    fn on_device(path: &str, crc32: u32, save: Option<&[u8]>) -> DeviceEntry {
        DeviceEntry {
            path: PathBuf::from(path),
            game: game(crc32),
            save: save.map(|b| Save::from_bytes(b, UNIX_EPOCH)),
        }
    }

    fn row(row: GameRow) -> Row {
        Row::Game {
            files: row
                .local_entry()
                .iter()
                .map(|l| PathBuf::from(format!("roms/gba/{}", l.name)))
                .collect(),
            row,
            standard_name: None,
        }
    }

    /// In order: only in the library, on both sides, only on the device, and
    /// an orphan save.
    fn rows() -> Vec<Row> {
        vec![
            row(GameRow::InLibrary(local("Alpha.gba", 1, &[]))),
            row(GameRow::Both {
                local: local("Both.gba", 2, &[b"old", b"new"]),
                on_device: on_device("Both.gba", 2, Some(b"new")),
            }),
            row(GameRow::OnDevice(on_device("Hack.gba", 3, None))),
            Row::OrphanOnDevice(OrphanDeviceSave {
                platform: Platform::Gba,
                path: PathBuf::from("Gone.sav"),
                save: Save::from_bytes(b"x", UNIX_EPOCH),
            }),
        ]
    }

    fn app() -> (App, Receiver<Sent>) {
        let (worker, rx) = Worker::recording();
        let covers = Covers::spawn(Picker::halfblocks(), || {});
        let paths = DirReader::spawn(|| {});
        let mut app = App::new(worker, covers, paths);
        app.update(Update::Snapshot(Snapshot {
            rows: rows(),
            device: Some(PathBuf::from("/Volumes/POCKET")),
        }));
        (app, rx)
    }

    fn press(app: &mut App, keys: &str) {
        for c in keys.chars() {
            app.handle_key(KeyEvent::from(match c {
                '\n' => KeyCode::Enter,
                '\x1b' => KeyCode::Esc,
                c => KeyCode::Char(c),
            }));
        }
    }

    fn sent(rx: &Receiver<Sent>) -> Vec<Command> {
        rx.try_iter().map(|(_, c)| c).collect()
    }

    fn paths(entries: &[DeviceEntry]) -> Vec<&Path> {
        entries.iter().map(|e| e.path.as_path()).collect()
    }

    /// Clears the path being typed and presses `keys`.
    fn retype(app: &mut App, keys: &str) {
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        press(app, keys);
    }

    /// Clears the path being typed and types `path`, waiting for its folder.
    fn type_path(app: &mut App, path: &str) {
        retype(app, path);
        for _ in 0..1000 {
            app.on_dir_read();
            if let Some(Prompt::Import(import)) = app.prompt()
                && !import.input.matches().is_empty()
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("the folder wasn't read");
    }

    fn core_row(id: &str, on_device: Option<&str>, latest: Option<&str>) -> CoreRow {
        let installed = on_device.map(|version| InstalledCore {
            id: id.into(),
            version: Some(version.into()),
            platforms: vec![Platform::Gb],
        });
        let latest = latest.map(|version| InventoryCore {
            id: id.into(),
            version: version.into(),
            platforms: vec![Platform::Gb],
            download_url: "x".into(),
        });
        match (installed, latest) {
            (Some(installed), Some(latest)) => CoreRow::Both { installed, latest },
            (Some(installed), None) => CoreRow::Installed(installed),
            (None, Some(latest)) => CoreRow::Available(latest),
            (None, None) => unreachable!(),
        }
    }

    /// In order: not on the device, with an update, up to date, and one that
    /// can't be installed; on the Cores page.
    fn on_cores_page() -> (App, Receiver<Sent>) {
        let (mut app, rx) = app();
        app.handle_key(KeyEvent::from(KeyCode::Tab));
        app.update(Update::Cores {
            rows: vec![
                core_row("new.Core", None, Some("1")),
                core_row("old.Core", Some("1"), Some("2")),
                core_row("same.Core", Some("2"), Some("2")),
                core_row("mine.Core", Some("1"), None),
            ],
            failure: None,
        });
        assert_eq!(sent(&rx), [Command::ShowCores]);
        (app, rx)
    }

    fn bios_row(name: &str, status: BiosStatus) -> BiosRow {
        BiosRow {
            platform: Platform::Gba,
            path: PathBuf::from(name),
            needed_by: vec![("a.GBA".to_string(), true)],
            size_exact: Some(4),
            status,
        }
    }

    /// A file that is there, then one that is missing; on the BIOS page, with
    /// a device open.
    fn on_bios_page() -> (App, Receiver<Sent>) {
        let (mut app, rx) = app();
        press(&mut app, "3");
        app.update(Update::Bios(vec![
            bios_row("there.bin", BiosStatus::Present),
            bios_row("gone.bin", BiosStatus::Missing),
        ]));
        (app, rx)
    }

    #[test]
    fn i_reports_row_not_on_device_without_sending() {
        let (mut app, rx) = app();
        press(&mut app, "i");
        assert!(sent(&rx).is_empty());
        assert_eq!(
            app.status_message().unwrap().text,
            "nothing here is on the device"
        );
    }

    #[test]
    fn acts_only_on_selected_version_in_saves_panel() {
        let (mut app, rx) = app();
        // A mark elsewhere doesn't count in the panel.
        press(&mut app, "mk");
        press(&mut app, "j\n");
        assert_eq!(app.games.selected_save_versions().len(), 2);
        press(&mut app, "jp");
        let [Command::PushSaveVersion(game, version)] = &sent(&rx)[..] else {
            panic!()
        };
        assert_eq!(game.path, Path::new("Both.gba"));
        assert_eq!(version, &app.games.selected_save_versions()[1]);
        press(&mut app, "x");
        press(&mut app, "y");
        let [Command::TrashSaveVersion(game, version)] = &sent(&rx)[..] else {
            panic!()
        };
        assert_eq!(game.name, "Both.gba");
        assert_eq!(version, &app.games.selected_save_versions()[1]);
        press(&mut app, "\x1b");
        assert_eq!(app.games.selected_version(), None);
    }

    #[test]
    fn escape_cancels_running_job() {
        let (mut app, rx) = app();
        press(&mut app, "ji");
        let (cancel_token, _) = rx.try_recv().unwrap();
        assert!(!cancel_token.load(Ordering::Relaxed));
        app.update(Update::Started {
            label: "pulling games".into(),
            total: 2,
        });
        press(&mut app, "\x1b");
        assert!(cancel_token.load(Ordering::Relaxed));
        assert!(app.job_status().unwrap().is_cancelling);
        app.update(Update::Finished {
            summary: "stopped".into(),
            failed: false,
        });
        assert!(!app.is_busy());
    }

    mod pages {
        use super::*;

        #[test]
        fn tab_cycles_through_pages() {
            let (mut app, _rx) = app();
            app.handle_key(KeyEvent::from(KeyCode::Tab));
            assert_eq!(app.current_page(), Page::Cores);
            app.handle_key(KeyEvent::from(KeyCode::Tab));
            assert_eq!(app.current_page(), Page::Bios);
            app.handle_key(KeyEvent::from(KeyCode::Tab));
            assert_eq!(app.current_page(), Page::Games);
        }

        #[test]
        fn number_keys_jump_to_pages() {
            let (mut app, _rx) = app();
            press(&mut app, "3");
            assert_eq!(app.current_page(), Page::Bios);
            press(&mut app, "2");
            assert_eq!(app.current_page(), Page::Cores);
            press(&mut app, "1");
            assert_eq!(app.current_page(), Page::Games);
        }

        #[test]
        fn keeps_page_while_typing_search() {
            let (mut app, rx) = app();
            press(&mut app, "/2");
            app.handle_key(KeyEvent::from(KeyCode::Tab));
            assert_eq!(app.current_page(), Page::Games);
            assert_eq!(app.games.search_text(), "2");
            assert_eq!(sent(&rx), []);
        }
    }

    mod search {
        use super::*;

        #[test]
        fn edits_search_text_at_cursor() {
            let (mut app, _rx) = app();
            press(&mut app, "/hck");
            app.handle_key(KeyEvent::from(KeyCode::Left));
            app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
            press(&mut app, "a");
            assert_eq!(app.games.search_text(), "hack");
            assert_eq!(
                app.games.search_text_around_cursor(),
                ("ha".to_string(), "ck".to_string())
            );
            assert_eq!(name_of(app.games.selected_row().unwrap()), "Hack.gba");
        }

        #[test]
        fn matches_letters_in_order() {
            let (mut app, _rx) = app();
            press(&mut app, "/hck");
            assert_eq!(app.games.visible_len(), 1);
            assert_eq!(name_of(app.games.selected_row().unwrap()), "Hack.gba");
        }

        #[test]
        fn keeps_selection_on_row_while_searching() {
            let (mut app, _rx) = app();
            press(&mut app, "jj/hac");
            assert_eq!(app.games.visible_len(), 1);
            assert_eq!(name_of(app.games.selected_row().unwrap()), "Hack.gba");
            press(&mut app, "\x1b");
            assert_eq!(app.games.visible_len(), 4);
            assert_eq!(name_of(app.games.selected_row().unwrap()), "Hack.gba");
        }

        #[test]
        fn selects_nothing_until_empty_search_is_cleared() {
            let (mut app, _rx) = app();
            press(&mut app, "/zzzzzz\n");
            assert!(app.games.selected_row().is_none());
            assert_eq!(app.games.table_state.selected(), None);
            press(&mut app, "\x1b");
            assert!(app.games.selected_row().is_some());
            assert_eq!(app.games.table_state.selected(), Some(0));
        }
    }

    mod marks {
        use super::*;

        #[test]
        fn acts_on_selected_row_without_marks() {
            let (mut app, rx) = app();
            press(&mut app, "ji");
            let [Command::PullGames(games)] = &sent(&rx)[..] else {
                panic!()
            };
            assert_eq!(paths(games), [Path::new("Both.gba")]);
        }

        #[test]
        fn acts_on_all_marked_rows_and_clears_marks() {
            let (mut app, rx) = app();
            // Each mark moves on.
            press(&mut app, "jmmb");
            let [Command::PullSaves(games)] = &sent(&rx)[..] else {
                panic!()
            };
            assert_eq!(paths(games), [Path::new("Both.gba"), Path::new("Hack.gba")]);
            assert_eq!(app.games.marked_count(), 0);
        }

        #[test]
        fn u_unmarks_selected_row() {
            let (mut app, _rx) = app();
            press(&mut app, "mm");
            assert_eq!(app.games.marked_count(), 2);
            press(&mut app, "ku");
            assert_eq!(app.games.marked_count(), 1);
        }

        #[test]
        fn shift_u_unmarks_all_rows() {
            let (mut app, _rx) = app();
            press(&mut app, "mmU");
            assert_eq!(app.games.marked_count(), 0);
        }

        #[test]
        fn shift_m_marks_rows_shown_by_search() {
            let (mut app, _rx) = app();
            press(&mut app, "/hack\nM");
            assert_eq!(app.games.marked_count(), 1);
        }

        #[test]
        fn acts_on_marked_rows_hidden_by_search() {
            let (mut app, rx) = app();
            press(&mut app, "jjm/alpha\nms");
            let [Command::PushGames(games)] = &sent(&rx)[..] else {
                panic!()
            };
            // The game only on the device, hidden, has nothing to push.
            assert_eq!(games.len(), 1);
            assert_eq!(games[0].name, "Alpha.gba");
        }

        #[test]
        fn keeps_selection_and_marks_on_same_rows_after_snapshot() {
            let (mut app, _rx) = app();
            press(&mut app, "jjkm");
            let mut fewer = rows();
            fewer.remove(0);
            app.update(Update::Snapshot(Snapshot {
                rows: fewer,
                device: None,
            }));
            assert_eq!(name_of(app.games.selected_row().unwrap()), "Hack.gba");
            assert_eq!(app.games.marked_count(), 1);
        }
    }

    mod dialogs {
        use super::*;

        #[test]
        fn queues_duplicates_dialog_until_open_dialog_closes() {
            let (mut app, rx) = app();
            press(&mut app, "jjX");
            let asking = app.dialog().unwrap().title.clone();
            app.update(Update::Duplicates(vec!["a line".to_string()]));
            assert_eq!(app.dialog().unwrap().title, asking);
            // Closing the dialog open shows them; closing them shows nothing.
            press(&mut app, "\x1b");
            assert!(sent(&rx).is_empty());
            let dialog = app.dialog().unwrap();
            assert_eq!(dialog.title, "Duplicate games on the device");
            assert!(dialog.lines.iter().any(|l| l == "a line"));
            press(&mut app, "\n");
            assert!(app.dialog().is_none());
        }

        #[test]
        fn shift_x_confirms_before_deleting_game() {
            let (mut app, rx) = app();
            press(&mut app, "jjX");
            assert!(app.dialog().unwrap().is_irreversible);
            assert!(sent(&rx).is_empty());
            press(&mut app, "\x1b");
            assert!(app.dialog().is_none());
            assert!(sent(&rx).is_empty());
            press(&mut app, "X\n");
            assert!(matches!(sent(&rx)[..], [Command::DeleteGames(_)]));
        }

        #[test]
        fn shift_x_confirms_deleting_orphan_save_and_defaults_to_cancel() {
            let (mut app, rx) = app();
            press(&mut app, "jjjX");
            let dialog = app.dialog().unwrap();
            assert!(dialog.is_irreversible);
            assert_eq!(dialog.focused_button, 1);
            press(&mut app, "\n");
            assert!(sent(&rx).is_empty());
            press(&mut app, "Xh\n");
            assert!(matches!(sent(&rx)[..], [Command::DeleteOrphanSaves(_)]));
        }

        #[test]
        fn x_does_nothing_on_orphan_save_on_device() {
            let (mut app, rx) = app();
            press(&mut app, "jjjx");
            assert!(app.dialog().is_none());
            assert!(sent(&rx).is_empty());
        }

        #[test]
        fn x_lists_files_before_trashing() {
            let (mut app, rx) = app();
            press(&mut app, "x");
            let dialog = app.dialog().unwrap();
            assert!(
                dialog
                    .lines
                    .iter()
                    .any(|l| l.contains("roms/gba/Alpha.gba"))
            );
            press(&mut app, "y");
            assert!(matches!(sent(&rx)[..], [Command::TrashGames(_)]));
        }
    }

    mod import {
        use super::*;

        #[test]
        fn imports_game_without_asking_when_extension_gives_platform() {
            let (mut app, rx) = app();
            press(&mut app, "I");
            retype(&mut app, "/some/Mine.GBA\n");
            let [Command::ImportGame(path, None)] = &sent(&rx)[..] else {
                panic!()
            };
            assert_eq!(path, Path::new("/some/Mine.GBA"));
            assert!(app.prompt().is_none());
        }

        #[test]
        fn imports_dir_without_asking_for_platform() {
            let (mut app, rx) = app();
            press(&mut app, "I");
            retype(&mut app, "/some/roms/\n");
            let [Command::ImportGame(path, None)] = &sent(&rx)[..] else {
                panic!()
            };
            assert_eq!(path, Path::new("/some/roms"));
            assert!(app.prompt().is_none());
        }

        #[test]
        fn asks_for_platform_when_extension_gives_none() {
            let (mut app, rx) = app();
            press(&mut app, "I");
            retype(&mut app, "/some/x.bin\n");
            assert!(sent(&rx).is_empty());
            assert!(matches!(app.prompt(), Some(Prompt::Platform(_))));
            press(&mut app, "j\n");
            let [Command::ImportGame(path, Some(Platform::Gbc))] = &sent(&rx)[..] else {
                panic!()
            };
            assert_eq!(path, Path::new("/some/x.bin"));
            assert!(app.prompt().is_none());
        }

        #[test]
        fn ctrl_n_moves_down_platform_list() {
            let (mut app, rx) = app();
            press(&mut app, "I");
            retype(&mut app, "/some/x.bin\n");
            app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
            press(&mut app, "\n");
            let [Command::ImportGame(_, Some(Platform::Gbc))] = &sent(&rx)[..] else {
                panic!()
            };
        }

        #[test]
        fn sends_keys_to_dialog_before_import_window() {
            let (mut app, rx) = app();
            press(&mut app, "I");
            retype(&mut app, "/some/Mine.gba");
            app.update(Update::Duplicates(vec!["a line".to_string()]));
            press(&mut app, "\n");
            assert!(app.dialog().is_none());
            assert!(sent(&rx).is_empty());
            assert!(app.prompt().is_some());
            press(&mut app, "\n");
            let [Command::ImportGame(path, None)] = &sent(&rx)[..] else {
                panic!()
            };
            assert_eq!(path, Path::new("/some/Mine.gba"));
        }

        #[test]
        fn escape_in_platform_list_imports_nothing() {
            let (mut app, rx) = app();
            press(&mut app, "I");
            retype(&mut app, "/some/x.bin\n\x1b");
            assert!(app.prompt().is_none());
            assert!(sent(&rx).is_empty());
        }

        #[test]
        fn enter_on_dir_keeps_save_window_open() {
            let (mut app, rx) = app();
            press(&mut app, "j\nI\n");
            assert!(app.prompt().is_some());
            assert!(sent(&rx).is_empty());
        }

        #[test]
        fn imports_save_for_game_of_selected_row() {
            let (mut app, rx) = app();
            press(&mut app, "j\nI");
            assert!(app.prompt().is_some());
            // Keys are text here, whatever they do elsewhere.
            press(&mut app, "x");
            assert!(sent(&rx).is_empty());
            retype(&mut app, "/some/Mine.sav\n");
            let [Command::ImportSave(game, path)] = &sent(&rx)[..] else {
                panic!()
            };
            assert_eq!(game.name, "Both.gba");
            assert_eq!(path, Path::new("/some/Mine.sav"));
            assert!(app.prompt().is_none());
        }

        #[test]
        fn enter_fills_in_picked_match_before_importing() {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("Mine.sav"), b"").unwrap();
            let typed = format!("{}/", dir.path().display());
            let (mut app, rx) = app();
            press(&mut app, "j\nI");
            type_path(&mut app, &typed);
            app.handle_key(KeyEvent::from(KeyCode::Down));
            press(&mut app, "\n");
            assert!(sent(&rx).is_empty());
            press(&mut app, "\n");
            let [Command::ImportSave(_, path)] = &sent(&rx)[..] else {
                panic!()
            };
            assert_eq!(path, &dir.path().join("Mine.sav"));
        }

        #[test]
        fn escape_clears_picked_match_then_closes_window() {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("Mine.sav"), b"").unwrap();
            let typed = format!("{}/", dir.path().display());
            let (mut app, rx) = app();
            press(&mut app, "j\nI");
            type_path(&mut app, &typed);
            app.handle_key(KeyEvent::from(KeyCode::Down));
            press(&mut app, "\x1b");
            let Some(Prompt::Import(import)) = app.prompt() else {
                panic!()
            };
            assert_eq!(import.input.picked(), None);
            press(&mut app, "\x1b");
            assert!(app.prompt().is_none());
            assert_eq!(app.games.selected_version(), Some(0));
            assert!(sent(&rx).is_empty());
        }

        #[test]
        fn shift_i_in_saves_panel_reports_game_not_in_library() {
            let (mut app, rx) = app();
            press(&mut app, "jj\nI");
            assert!(app.prompt().is_none());
            assert!(sent(&rx).is_empty());
            assert_eq!(
                app.status_message().unwrap().text,
                "nothing here is in the library"
            );
        }
    }

    mod renaming {
        use super::*;

        #[test]
        fn n_prefills_name_without_extension_and_sends_new_name() {
            let (mut app, rx) = app();
            press(&mut app, "n");
            let Some(Prompt::Rename(rename)) = app.prompt() else {
                panic!()
            };
            let (before, after) = rename.text_around_cursor();
            assert_eq!((before.as_str(), after.as_str()), ("Alpha", ""));
            // Keys are text here, whatever they do elsewhere.
            press(&mut app, "qx\n");
            let [Command::RenameInLibrary(game, new)] = &sent(&rx)[..] else {
                panic!()
            };
            assert_eq!((game.name.as_str(), new.as_str()), ("Alpha.gba", "Alphaqx"));
            assert!(app.prompt().is_none());
        }

        #[test]
        fn shift_n_renames_copy_on_device() {
            let (mut app, rx) = app();
            press(&mut app, "jN!\n");
            let [Command::RenameOnDevice(game, new)] = &sent(&rx)[..] else {
                panic!()
            };
            assert_eq!(game.path, Path::new("Both.gba"));
            assert_eq!(new, "Both!");
        }

        #[test]
        fn sends_nothing_for_same_name_or_cancel() {
            let (mut app, rx) = app();
            press(&mut app, "n\n");
            assert!(app.prompt().is_none());
            press(&mut app, "nabc\x1b");
            assert!(app.prompt().is_none());
            assert_eq!(sent(&rx), []);
        }

        #[test]
        fn reports_game_not_on_side_being_renamed() {
            let (mut app, rx) = app();
            press(&mut app, "N");
            assert_eq!(
                app.status_message().unwrap().text,
                "this game isn't on the device"
            );
            press(&mut app, "jjn");
            assert_eq!(
                app.status_message().unwrap().text,
                "nothing here is in the library"
            );
            assert!(app.prompt().is_none());
            assert_eq!(sent(&rx), []);
        }

        #[test]
        fn keeps_selection_and_mark_on_renamed_game() {
            let (mut app, _rx) = app();
            // Marks the first and moves on to the second, which goes first.
            press(&mut app, "m");
            let mut rows = rows();
            let Row::Game { row, .. } = &mut rows[1] else {
                panic!()
            };
            let GameRow::Both { local, .. } = row else {
                panic!()
            };
            local.name = "Aaa.gba".to_string();
            let renamed = rows.remove(1);
            rows.insert(0, renamed);
            app.update(Update::Snapshot(Snapshot {
                rows,
                device: Some(PathBuf::from("/Volumes/POCKET")),
            }));
            let selected = app.games.selected_row().unwrap().local_entry().unwrap();
            assert_eq!(selected.name, "Aaa.gba");
            assert_eq!(app.games.marked_count(), 1);
        }
    }

    mod cores {
        use super::*;

        fn ids(ids: &[&str]) -> Vec<String> {
            ids.iter().map(ToString::to_string).collect()
        }

        #[test]
        fn requests_cores_only_on_first_visit() {
            let (mut app, rx) = on_cores_page();
            press(&mut app, "1");
            assert_eq!(app.current_page(), Page::Games);
            press(&mut app, "2");
            assert_eq!(app.current_page(), Page::Cores);
            assert_eq!(sent(&rx), []);
        }

        #[test]
        fn u_installs_new_core_or_updates_installed_one() {
            let (mut app, rx) = on_cores_page();
            press(&mut app, "u");
            assert_eq!(sent(&rx), [Command::InstallCores(ids(&["new.Core"]))]);
            press(&mut app, "ju");
            assert_eq!(sent(&rx), [Command::InstallCores(ids(&["old.Core"]))]);
        }

        #[test]
        fn u_reports_why_when_core_cannot_be_installed() {
            let (mut app, rx) = on_cores_page();
            press(&mut app, "jju");
            assert_eq!(
                app.status_message().unwrap().text,
                "the device has the newest version"
            );
            press(&mut app, "ju");
            assert_eq!(
                app.status_message().unwrap().text,
                "only the cores with a latest version can be installed"
            );
            assert_eq!(sent(&rx), []);
        }

        #[test]
        fn u_reports_no_device() {
            let (mut app, rx) = on_cores_page();
            app.update(Update::Snapshot(Snapshot {
                rows: Vec::new(),
                device: None,
            }));
            press(&mut app, "u");
            assert_eq!(app.status_message().unwrap().text, "no device is open");
            assert_eq!(sent(&rx), []);
        }

        #[test]
        fn shift_u_updates_cores_with_newer_release() {
            let (mut app, rx) = on_cores_page();
            press(&mut app, "U");
            assert_eq!(sent(&rx), [Command::InstallCores(ids(&["old.Core"]))]);
        }

        #[test]
        fn shift_u_reports_every_core_up_to_date() {
            let (mut app, rx) = on_cores_page();
            app.update(Update::Cores {
                rows: vec![core_row("same.Core", Some("2"), Some("2"))],
                failure: None,
            });
            press(&mut app, "U");
            assert_eq!(
                app.status_message().unwrap().text,
                "every core is up to date"
            );
            assert_eq!(sent(&rx), []);
        }

        #[test]
        fn x_confirms_before_removing_and_defaults_to_cancel() {
            let (mut app, rx) = on_cores_page();
            press(&mut app, "jx");
            let dialog = app.dialog().unwrap();
            assert!(dialog.is_irreversible);
            assert_eq!(dialog.focused_button, 1);
            press(&mut app, "\n");
            assert_eq!(sent(&rx), []);
            press(&mut app, "xh\n");
            assert_eq!(sent(&rx), [Command::RemoveCores(ids(&["old.Core"]))]);
        }

        #[test]
        fn x_reports_core_not_on_device() {
            let (mut app, rx) = on_cores_page();
            press(&mut app, "x");
            assert!(app.dialog().is_none());
            assert_eq!(
                app.status_message().unwrap().text,
                "this core isn't on the device"
            );
            assert_eq!(sent(&rx), []);
        }

        #[test]
        fn keeps_selection_on_same_core_when_rows_change() {
            let (mut app, _rx) = on_cores_page();
            press(&mut app, "jj");
            app.update(Update::Cores {
                rows: vec![
                    core_row("old.Core", Some("2"), Some("2")),
                    core_row("same.Core", Some("2"), Some("2")),
                ],
                failure: None,
            });
            assert_eq!(app.cores.selected_row().unwrap().id(), "same.Core");
            app.update(Update::Cores {
                rows: vec![core_row("old.Core", Some("2"), Some("2"))],
                failure: None,
            });
            assert_eq!(app.cores.selected_row().unwrap().id(), "old.Core");
        }
    }

    mod bios {
        use super::*;

        #[test]
        fn shift_i_prompts_for_file_and_sends_it_with_row() {
            let (mut app, rx) = on_bios_page();
            press(&mut app, "jI");
            assert!(app.prompt().is_some());
            retype(&mut app, "/some/mine.bin\n");
            assert_eq!(
                sent(&rx),
                [Command::PutBios(
                    bios_row("gone.bin", BiosStatus::Missing),
                    PathBuf::from("/some/mine.bin")
                )]
            );
            assert!(app.prompt().is_none());
        }

        #[test]
        fn shift_i_reports_file_already_on_device() {
            let (mut app, rx) = on_bios_page();
            press(&mut app, "I");
            assert!(app.prompt().is_none());
            assert_eq!(
                app.status_message().unwrap().text,
                "the device has this file already"
            );
            assert_eq!(sent(&rx), []);
        }

        #[test]
        fn shift_i_reports_no_core_needs_file() {
            let (mut app, rx) = app();
            press(&mut app, "3I");
            assert!(app.prompt().is_none());
            assert_eq!(
                app.status_message().unwrap().text,
                "no core on the device needs a file"
            );
            assert_eq!(sent(&rx), []);
        }

        #[test]
        fn shift_i_reports_no_device() {
            let (mut app, rx) = app();
            app.update(Update::Snapshot(Snapshot::default()));
            press(&mut app, "3I");
            assert!(app.prompt().is_none());
            assert_eq!(app.status_message().unwrap().text, "no device is open");
            assert_eq!(sent(&rx), []);
        }

        #[test]
        fn keeps_selection_on_same_file_when_rows_change() {
            let (mut app, _rx) = on_bios_page();
            press(&mut app, "j");
            app.update(Update::Bios(vec![
                bios_row("first.bin", BiosStatus::Missing),
                bios_row("there.bin", BiosStatus::Present),
                bios_row("gone.bin", BiosStatus::Missing),
            ]));
            assert_eq!(app.bios.selected_row().unwrap().path, Path::new("gone.bin"));
            app.update(Update::Bios(Vec::new()));
            assert_eq!(app.bios.selected_row(), None);
        }
    }

    mod drawing {
        use super::*;

        /// The screen's top line.
        fn header(app: &mut App) -> String {
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            terminal.draw(|frame| ui::draw(frame, app)).unwrap();
            let buffer = terminal.backend().buffer();
            (0..80).map(|x| buffer[(x, 0)].symbol()).collect()
        }

        #[test]
        fn shows_device_being_read_in_header_not_footer() {
            let (mut app, _rx) = app();
            assert_eq!(app.device_root(), Some(Path::new("/Volumes/POCKET")));
            let root = PathBuf::from("/Volumes/OTHER");
            app.update(Update::ReadingDevice(root.clone()));
            assert_eq!(app.device_root(), None);
            assert!(app.is_busy());
            assert!(app.job_status().is_none());
            let top = header(&mut app);
            assert!(top.contains("reading OTHER…"), "{top}");
            // In the order the worker tells of them.
            app.update(Update::Snapshot(Snapshot {
                rows: rows(),
                device: Some(root),
            }));
            app.update(Update::DeviceRead);
            assert!(!app.is_busy());
            let top = header(&mut app);
            assert!(top.contains("● OTHER"), "{top}");
        }

        /// Where the header of the table says `PLATFORM`.
        fn platform_column(app: &mut App) -> usize {
            let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
            terminal.draw(|frame| ui::draw(frame, app)).unwrap();
            let buffer = terminal.backend().buffer();
            let line: Vec<&str> = (0..120).map(|x| buffer[(x, 3)].symbol()).collect();
            line.iter().position(|cell| *cell == "P").unwrap()
        }

        #[test]
        fn starts_columns_at_same_place_on_every_page() {
            let (mut app, _rx) = on_cores_page();
            assert_eq!(platform_column(&mut app), 3);
            press(&mut app, "1");
            assert_eq!(platform_column(&mut app), 3);
            // Nothing to select.
            press(&mut app, "3");
            assert_eq!(platform_column(&mut app), 3);
            app.update(Update::Bios(vec![bios_row("x.bin", BiosStatus::Missing)]));
            assert_eq!(platform_column(&mut app), 3);
        }

        #[test]
        fn gives_marked_and_selected_rows_distinct_backgrounds() {
            let (mut app, _rx) = app();
            let background_of_row = |app: &mut App, row: u16| {
                let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
                terminal.draw(|frame| ui::draw(frame, app)).unwrap();
                terminal.backend().buffer()[(20, 4 + row)].bg
            };
            // Marks the first row and moves on to the second.
            press(&mut app, "m");
            let marked = background_of_row(&mut app, 0);
            let selected = background_of_row(&mut app, 1);
            let plain = background_of_row(&mut app, 2);
            press(&mut app, "k");
            let selected_and_marked = background_of_row(&mut app, 0);
            let backgrounds = [marked, selected, plain, selected_and_marked];
            for (i, background) in backgrounds.iter().enumerate() {
                assert!(
                    !backgrounds[i + 1..].contains(background),
                    "{backgrounds:?}"
                );
            }
        }

        /// Every screen draws, at sizes down to a tiny terminal.
        #[test]
        fn draws_every_state_at_any_size() {
            for (width, height) in [(120, 40), (80, 24), (40, 10), (10, 3)] {
                let (mut app, _rx) = app();
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                let mut draw = |app: &mut App| {
                    terminal.draw(|frame| ui::draw(frame, app)).unwrap();
                };
                draw(&mut app);
                app.update(Update::ReadingDevice(PathBuf::from("/Volumes/POCKET")));
                draw(&mut app);
                app.update(Update::DeviceRead);
                press(&mut app, "jm");
                draw(&mut app);
                press(&mut app, "\n");
                draw(&mut app);
                press(&mut app, "\x1b?");
                draw(&mut app);
                press(&mut app, "\x1bX");
                draw(&mut app);
                press(&mut app, "\x1b/x");
                draw(&mut app);
                press(&mut app, "\x1b\x1bj\nI");
                draw(&mut app);
                type_path(&mut app, "/");
                draw(&mut app);
                app.handle_key(KeyEvent::from(KeyCode::Down));
                draw(&mut app);
                press(&mut app, "\x1b\x1b\x1b");
                press(&mut app, "\x1bI");
                retype(&mut app, "/x.bin\n");
                draw(&mut app);
                press(&mut app, "jjjjjjjjj");
                draw(&mut app);
                press(&mut app, "\x1b");
                press(&mut app, "\x1b");
                app.update(Update::Started {
                    label: "pushing".into(),
                    total: 3,
                });
                draw(&mut app);
                app.update(Update::Finished {
                    summary: "pushed".into(),
                    failed: false,
                });
                app.handle_key(KeyEvent::from(KeyCode::Tab));
                draw(&mut app);
                app.update(Update::Cores {
                    rows: vec![
                        core_row("new.Core", None, Some("1")),
                        core_row("old.Core", Some("1"), Some("2")),
                        core_row("mine.Core", Some("1"), None),
                    ],
                    failure: Some("no network".into()),
                });
                draw(&mut app);
                press(&mut app, "jx");
                draw(&mut app);
                press(&mut app, "n3");
                draw(&mut app);
                app.update(Update::Bios(vec![
                    bios_row("there.bin", BiosStatus::Present),
                    bios_row("short.bin", BiosStatus::WrongSize { size: 1 }),
                    bios_row("gone.bin", BiosStatus::Missing),
                ]));
                draw(&mut app);
                press(&mut app, "GI");
                draw(&mut app);
                press(&mut app, "\x1b1n");
                draw(&mut app);
                press(&mut app, "\x1bjN");
                draw(&mut app);
            }
        }
    }
}
