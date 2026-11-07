//! The screen's state and what each key does to it. Anything that touches a
//! disk or the network is left to the worker.
//!
//! `confirm_*` functions open a yes-or-no window and `prompt_for_*` functions
//! open one to type in. The work happens once the user answers.

use std::{
    collections::HashSet,
    fmt,
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
use strum::{EnumIter, IntoEnumIterator};
use tui_input::Input;

use super::{
    covers::CoverLoader,
    path_input::{self, DirReader, PathInput},
    worker::{
        DevicePageData, GamesPageData, GamesTableRow, WorkerCommand, WorkerHandle, WorkerMessage,
    },
};

// ============================================================================
// Status
// ============================================================================

/// What the screen shows of a command under way.
pub struct CommandStatus {
    pub label: String,
    pub finished_count: usize,
    pub target_count: usize,
    /// The target last finished.
    pub last_target: Option<String>,
    pub is_canceling: bool,
}

pub struct StatusMessage {
    pub text: String,
    pub is_failure: bool,
}

pub enum DeviceStatus {
    NoDevice,
    /// Being opened and read.
    Reading(PathBuf),
    Open(PathBuf),
}

// ============================================================================
// Dialogs
// ============================================================================

pub struct DialogButton {
    pub label: &'static str,
    pub command: Option<WorkerCommand>,
}

impl DialogButton {
    fn new(label: &'static str, command: Option<WorkerCommand>) -> Self {
        Self { label, command }
    }
}

/// A question or message shown over the screen.
pub struct Dialog {
    pub title: String,
    pub lines: Vec<String>,
    /// The last button has no command and cancels.
    pub buttons: Vec<DialogButton>,
    pub focused_button: usize,
    /// Whether the action can't be undone.
    pub is_irreversible: bool,
}

// ============================================================================
// Prompts
// ============================================================================

/// What the path being typed will be imported as.
pub enum ImportTarget {
    Game,
    /// A version of this game's save.
    Save(LocalEntry),
    /// This game's cover.
    Cover(LocalEntry),
    /// The file this row needs.
    Bios(BiosRow),
}

/// A path being typed, whose target will be imported.
pub struct ImportPrompt {
    pub target: ImportTarget,
    pub input: PathInput,
}

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
    pub fn text_around_cursor(&self) -> (String, String) {
        path_input::text_around_cursor(&self.input)
    }

    fn stem(&self) -> String {
        match &self.target {
            RenameTarget::InLibrary(local) => local.stem().to_string(),
            RenameTarget::OnDevice(on_device) => file_stem(&on_device.path),
        }
    }
}

fn file_stem(path: &Path) -> String {
    let stem = path.file_stem().unwrap_or_default();
    stem.to_string_lossy().into_owned()
}

/// A file being imported as a game, while the user picks its platform.
pub struct PlatformPrompt {
    pub file_to_import: PathBuf,
    /// An index into `Platform::iter()`.
    pub selected_platform_index: usize,
}

/// A window the user types or chooses in. A dialog can still open over it.
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

/// Identifies a row across updates, so marks and the selection stay with it. A
/// game is identified by its contents, whatever it is called and wherever it
/// is.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum RowKey {
    Game(Game),
    OrphanSaveInLibrary { platform: Platform, stem: String },
    OrphanSaveOnDevice { platform: Platform, path: PathBuf },
}

fn key_of(row: &GamesTableRow) -> RowKey {
    match row {
        GamesTableRow::Game(game_row) => RowKey::Game(game_row.game().clone()),
        GamesTableRow::OrphanSaveInLibrary(orphan) => RowKey::OrphanSaveInLibrary {
            platform: orphan.platform.clone(),
            stem: orphan.stem.clone(),
        },
        GamesTableRow::OrphanSaveOnDevice(orphan) => RowKey::OrphanSaveOnDevice {
            platform: orphan.platform.clone(),
            path: orphan.path.clone(),
        },
    }
}

/// A row's name in the table, which is also what a search matches.
pub fn name_of(row: &GamesTableRow) -> String {
    match row {
        GamesTableRow::Game(game_row) => match game_row {
            GameRow::InLibrary(local) | GameRow::Both { local, .. } => local.name.clone(),
            GameRow::OnDevice(on_device) => on_device.path.display().to_string(),
        },
        GamesTableRow::OrphanSaveInLibrary(orphan) => orphan.stem.clone(),
        GamesTableRow::OrphanSaveOnDevice(orphan) => orphan.path.display().to_string(),
    }
}

fn matches_query(row: &GamesTableRow, query: &str) -> bool {
    let on_device = row.device_entry().map_or(String::new(), |on_device| {
        on_device.path.display().to_string()
    });
    path_input::fuzzy_match(query, &name_of(row)).is_some()
        || path_input::fuzzy_match(query, &on_device).is_some()
}

/// Which rows the Games page shows, by where they are. A game on both sides
/// counts for both.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, EnumIter)]
pub enum GamesFilter {
    #[default]
    All,
    InLibrary,
    OnDevice,
}

impl GamesFilter {
    fn next(self) -> Self {
        match self {
            GamesFilter::All => GamesFilter::InLibrary,
            GamesFilter::InLibrary => GamesFilter::OnDevice,
            GamesFilter::OnDevice => GamesFilter::All,
        }
    }

    fn shows(self, row: &GamesTableRow) -> bool {
        match self {
            GamesFilter::All => true,
            GamesFilter::InLibrary => {
                row.local_entry().is_some() || matches!(row, GamesTableRow::OrphanSaveInLibrary(_))
            }
            GamesFilter::OnDevice => {
                row.device_entry().is_some() || matches!(row, GamesTableRow::OrphanSaveOnDevice(_))
            }
        }
    }
}

impl fmt::Display for GamesFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            GamesFilter::All => "all",
            GamesFilter::InLibrary => "in library",
            GamesFilter::OnDevice => "on device",
        })
    }
}

/// The games and orphan saves, and what the user has done with them.
#[derive(Default)]
pub struct GamesPage {
    /// None until the worker has sent them.
    rows: Option<Vec<GamesTableRow>>,
    /// Indices into `rows` of the rows the filter and the search leave.
    visible_row_indices: Vec<usize>,
    filter: GamesFilter,
    pub table_state: TableState,
    marked_rows: HashSet<RowKey>,
    search_input: Input,
    focus: GamesPageFocus,
}

impl GamesPage {
    pub fn is_loaded(&self) -> bool {
        self.rows.is_some()
    }

    pub fn filter(&self) -> GamesFilter {
        self.filter
    }

    /// Every row, ignoring the filter and the search.
    pub fn rows(&self) -> &[GamesTableRow] {
        self.rows.as_deref().unwrap_or(&[])
    }

    pub fn visible_rows(&self) -> impl Iterator<Item = &GamesTableRow> {
        self.visible_row_indices.iter().map(|&i| &self.rows()[i])
    }

    pub fn visible_len(&self) -> usize {
        self.visible_row_indices.len()
    }

    pub fn selected_row(&self) -> Option<&GamesTableRow> {
        let &row = self.visible_row_indices.get(self.table_state.selected()?)?;
        Some(&self.rows()[row])
    }

    /// The version selected in the saves panel, if it is open.
    pub fn selected_version_index(&self) -> Option<usize> {
        match self.focus {
            GamesPageFocus::SavesPanel(selected) => Some(selected),
            GamesPageFocus::List | GamesPageFocus::Search => None,
        }
    }

    /// The versions of the selected game's save in the library, newest first.
    pub fn selected_save_versions(&self) -> Vec<Save> {
        let local = self.selected_row().and_then(GamesTableRow::local_entry);
        let history = local.and_then(|local| local.save.as_ref());
        history
            .iter()
            .flat_map(|history| history.iter().cloned())
            .collect()
    }

    /// Where the covers of the rows around the selected one are, so they can be
    /// prepared ahead.
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

    /// How many rows are marked, visible or not.
    pub fn marked_count(&self) -> usize {
        self.rows().iter().filter(|row| self.is_marked(row)).count()
    }

    pub fn is_marked(&self, row: &GamesTableRow) -> bool {
        self.marked_rows.contains(&key_of(row))
    }

    pub fn game_and_orphan_counts(&self) -> (usize, usize) {
        let is_game = |row: &&GamesTableRow| matches!(row, GamesTableRow::Game(_));
        let games = self.rows().iter().filter(is_game).count();
        (games, self.rows().len() - games)
    }

    pub fn search_text(&self) -> &str {
        self.search_input.value()
    }

    pub fn search_text_around_cursor(&self) -> (String, String) {
        path_input::text_around_cursor(&self.search_input)
    }

    pub fn is_searching(&self) -> bool {
        matches!(self.focus, GamesPageFocus::Search)
    }

    /// Stores the rows the worker sent, keeping the selection on its row.
    fn set_rows(&mut self, rows: Vec<GamesTableRow>) {
        let selected_key = self.selected_row().map(key_of);
        self.rows = Some(rows);
        self.refresh_visible_rows(selected_key);
    }

    /// Recomputes the visible rows, keeping the selection on the row `keep`.
    fn refresh_visible_rows(&mut self, selected_key: Option<RowKey>) {
        let found = self.rows().iter().enumerate().filter(|(_, row)| {
            self.filter.shows(row) && matches_query(row, self.search_input.value())
        });
        let visible: Vec<usize> = found.map(|(i, _)| i).collect();
        let new_selected_row = selected_key
            .and_then(|key| visible.iter().position(|&i| key_of(&self.rows()[i]) == key));
        let selected = new_selected_row
            .or(self.table_state.selected())
            .unwrap_or(0);
        self.visible_row_indices = visible;
        self.select_row(selected);
    }

    fn show_next_filter(&mut self) {
        let selected_key = self.selected_row().map(key_of);
        self.filter = self.filter.next();
        self.refresh_visible_rows(selected_key);
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

    fn toggle_mark_and_move_on(&mut self) {
        let Some(key) = self.selected_row().map(key_of) else {
            return;
        };
        if !self.marked_rows.remove(&key) {
            self.marked_rows.insert(key);
        }
        self.move_selection_by(1);
    }

    /// Marks the rows shown. If they are all marked already, clears every mark,
    /// including those on rows a search hides.
    fn mark_all_shown_or_clear_marks(&mut self) {
        let shown: Vec<RowKey> = self.visible_rows().map(key_of).collect();
        if shown.iter().all(|key| self.marked_rows.contains(key)) {
            self.marked_rows.clear();
        } else {
            self.marked_rows.extend(shown);
        }
    }

    /// The rows a key acts on, which are the marked rows, visible or not, or
    /// otherwise the selected row. In the saves panel it is always the selected
    /// row.
    fn rows_to_act_on(&self) -> Vec<&GamesTableRow> {
        let marked: Vec<&GamesTableRow> = self
            .rows()
            .iter()
            .filter(|row| self.is_marked(row))
            .collect();
        if marked.is_empty() || self.selected_version_index().is_some() {
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
            .filter_map(|row| row.device_entry().cloned())
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
            .filter_map(|row| row.local_entry().cloned())
            .collect();
        let inapplicable = rows.len() - entries.len();
        (entries, inapplicable)
    }
}

// ============================================================================
// Cores page
// ============================================================================

/// The cores as the worker last sent them.
pub struct CoreList {
    pub rows: Vec<CoreRow>,
    /// Why the inventory couldn't be fetched, if it couldn't.
    pub inventory_failure: Option<String>,
}

#[derive(Default)]
pub struct CoresPage {
    /// None until the worker has sent them.
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

    /// Stores the cores the worker sent, keeping the selection on its core.
    fn set_rows(&mut self, rows: Vec<CoreRow>, inventory_failure: Option<String>) {
        let selected_id = self.selected_row().map(|row| row.id().to_string());
        let new_selected_row =
            selected_id.and_then(|id| rows.iter().position(|row| row.id() == id));
        let selected = new_selected_row
            .or(self.table_state.selected())
            .unwrap_or(0);
        self.core_list = Some(CoreList {
            rows,
            inventory_failure,
        });
        self.select_row(selected);
    }

    /// Selects core `i`, or the last if there are fewer.
    fn select_row(&mut self, i: usize) {
        let len = self
            .core_list
            .as_ref()
            .map_or(0, |core_list| core_list.rows.len());
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

    /// Stores the files the worker sent, keeping the selection on its file.
    fn set_rows(&mut self, rows: Vec<BiosRow>) {
        let same = |a: &BiosRow, b: &BiosRow| a.platform == b.platform && a.path == b.path;
        let selected_row = self.selected_row().cloned();
        let new_selected_row =
            selected_row.and_then(|key| rows.iter().position(|row| same(row, &key)));
        let selected = new_selected_row
            .or(self.table_state.selected())
            .unwrap_or(0);
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Games,
    Cores,
    Bios,
    Device,
}

const NOT_ON_BOTH: &str = "nothing here is both in the library and on the device";

pub struct App {
    // Threads.
    worker_handle: WorkerHandle,
    pub cover_loader: CoverLoader,
    dir_reader: DirReader,
    // Pages.
    current_page: Page,
    pub games: GamesPage,
    pub cores: CoresPage,
    pub bios: BiosPage,
    /// None without a device.
    pub device_page: Option<DevicePageData>,
    // Windows over them.
    dialog: Option<Dialog>,
    /// Shown once `dialog` closes, so that it doesn't cover it.
    next_dialog: Option<Dialog>,
    prompt: Option<Prompt>,
    // Status.
    device_status: DeviceStatus,
    command_status: Option<CommandStatus>,
    status_message: Option<StatusMessage>,
    /// How many rows the command just sent doesn't apply to, reported with its
    /// summary.
    inapplicable_row_count: usize,
    should_quit: bool,
}

impl App {
    pub fn new(
        worker_handle: WorkerHandle,
        cover_loader: CoverLoader,
        dir_reader: DirReader,
    ) -> Self {
        Self {
            worker_handle,
            cover_loader,
            dir_reader,
            current_page: Page::Games,
            games: GamesPage::default(),
            cores: CoresPage::default(),
            bios: BiosPage::default(),
            device_page: None,
            dialog: None,
            next_dialog: None,
            prompt: None,
            device_status: DeviceStatus::NoDevice,
            command_status: None,
            status_message: None,
            inapplicable_row_count: 0,
            should_quit: false,
        }
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub fn is_busy(&self) -> bool {
        self.command_status.is_some() || matches!(self.device_status, DeviceStatus::Reading(_))
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

    pub fn command_status(&self) -> Option<&CommandStatus> {
        self.command_status.as_ref()
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
        self.cover_loader.collect_finished();
    }

    pub fn on_dir_read(&mut self) {
        for (dir, entries) in self.dir_reader.dirs_read() {
            if let Some(Prompt::Import(import)) = &mut self.prompt {
                import.input.set_entries(dir, entries);
            }
        }
    }

    pub fn handle_worker_message(&mut self, message: WorkerMessage) {
        match message {
            WorkerMessage::GamesPageChanged(GamesPageData { rows, device_root }) => {
                self.games.set_rows(rows);
                self.device_status = match device_root {
                    Some(root) => DeviceStatus::Open(root),
                    None => DeviceStatus::NoDevice,
                };
            }
            WorkerMessage::CoreListChanged { rows, failure } => self.cores.set_rows(rows, failure),
            WorkerMessage::BiosFilesChanged(rows) => self.bios.set_rows(rows),
            WorkerMessage::DevicePageChanged(data) => self.device_page = data,
            WorkerMessage::DeviceReadStarted(root) => {
                self.device_status = DeviceStatus::Reading(root)
            }
            WorkerMessage::DeviceReadFinished => {
                if matches!(self.device_status, DeviceStatus::Reading(_)) {
                    self.device_status = DeviceStatus::NoDevice;
                }
            }
            WorkerMessage::CommandStarted {
                label,
                target_count,
            } => {
                self.command_status = Some(CommandStatus {
                    label,
                    finished_count: 0,
                    target_count,
                    last_target: None,
                    is_canceling: false,
                });
            }
            WorkerMessage::CommandProgressed {
                finished_count,
                target_count,
                target,
            } => {
                if let Some(command_status) = &mut self.command_status {
                    (
                        command_status.finished_count,
                        command_status.target_count,
                        command_status.last_target,
                    ) = (finished_count, target_count, Some(target));
                }
            }
            WorkerMessage::CommandFinished { summary, failed } => {
                self.command_status = None;
                let summary = match std::mem::take(&mut self.inapplicable_row_count) {
                    0 => summary,
                    left_out_count => format!("{summary} ({left_out_count} left out)"),
                };
                self.status_message = Some(StatusMessage {
                    text: summary,
                    is_failure: failed,
                });
            }
            // The event loop ends before this message gets here.
            WorkerMessage::LibraryOpenFailed(_) => {}
            WorkerMessage::RenamesProposed(renames) if renames.is_empty() => {
                self.inapplicable_row_count = 0;
                self.show_status_message("no game to rename");
            }
            WorkerMessage::RenamesProposed(renames) => {
                self.open_dialog_when_free(renames_dialog(renames));
            }
            WorkerMessage::DuplicateGamesSkipped(lines) => {
                self.open_dialog_when_free(duplicates_dialog(lines));
            }
        }
    }

    /// Opens `dialog` now, or once the open dialog closes if there is one.
    fn open_dialog_when_free(&mut self, dialog: Dialog) {
        if self.dialog.is_some() {
            self.next_dialog = Some(dialog);
        } else {
            self.dialog = Some(dialog);
        }
    }

    // ---- Keys: where each goes -------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        // Keys that edit text stay as they are. For the others, Emacs keys
        // become the keys they stand for.
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
            Page::Device => return self.handle_device_page_key(key),
        }
        match self.games.focus {
            GamesPageFocus::List => self.handle_games_list_key(key),
            GamesPageFocus::Search => self.handle_search_key(key),
            GamesPageFocus::SavesPanel(selected) => self.handle_saves_panel_key(key, selected),
        }
    }

    /// Closes what is open, or else stops what is running.
    fn handle_escape(&mut self) {
        if !self.games.search_input.value().is_empty() {
            let selected_key = self.games.selected_row().map(key_of);
            self.games.search_input.reset();
            self.games.refresh_visible_rows(selected_key);
        } else if let Some(command_status) = &mut self.command_status {
            command_status.is_canceling = true;
            self.worker_handle.cancel();
        }
    }

    // ---- Keys on the Games page ------------------------------------------------

    fn handle_search_key(&mut self, key: KeyEvent) {
        let selected_key = self.games.selected_row().map(key_of);
        match key.code {
            KeyCode::Enter => self.games.focus = GamesPageFocus::List,
            KeyCode::Esc => {
                self.games.focus = GamesPageFocus::List;
                self.games.search_input.reset();
            }
            KeyCode::Down | KeyCode::Up | KeyCode::PageDown | KeyCode::PageUp => {
                self.games.focus = GamesPageFocus::List;
                self.handle_games_list_key(key);
                return;
            }
            _ => {
                path_input::edit(&mut self.games.search_input, key);
            }
        }
        self.games.refresh_visible_rows(selected_key);
    }

    fn handle_games_list_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.games.move_selection_by(1),
            KeyCode::Up | KeyCode::Char('k') => self.games.move_selection_by(-1),
            KeyCode::PageDown => self.games.move_selection_by(PAGE_KEY_ROW_COUNT),
            KeyCode::PageUp => self.games.move_selection_by(-PAGE_KEY_ROW_COUNT),
            KeyCode::Char('g') | KeyCode::Home => self.games.select_row(0),
            KeyCode::Char('G') | KeyCode::End => self.games.select_row(usize::MAX),
            KeyCode::Char('m') => self.games.toggle_mark_and_move_on(),
            KeyCode::Char('M') => self.games.mark_all_shown_or_clear_marks(),
            // Lowercase changes the library, Shift changes the device.
            KeyCode::Char('i') => self.prompt_for_import_path(ImportTarget::Game),
            KeyCode::Char('p') => self.send_for_device_rows(WorkerCommand::PullGames),
            KeyCode::Char('P') => self.send_for_library_rows(WorkerCommand::PushGames),
            KeyCode::Char('s') => self.send_for_device_rows(WorkerCommand::PullSaves),
            KeyCode::Char('S') => self.push_saves(),
            KeyCode::Char('n') => self.prompt_for_name_in_library(),
            KeyCode::Char('N') => self.prompt_for_name_on_device(),
            KeyCode::Char('t') => self.propose_renames_to_standard_names(),
            KeyCode::Char('a') => self.send_for_library_rows(WorkerCommand::FetchCovers),
            KeyCode::Char('c') => self.prompt_for_cover_to_import(),
            KeyCode::Char('x') => self.confirm_trash_games(),
            KeyCode::Char('X') => self.confirm_delete_games(),
            KeyCode::Char('d') => self.worker_handle.send(WorkerCommand::SyncDats),
            KeyCode::Char('E') => self.eject(),
            KeyCode::Char('r') => self.worker_handle.send(WorkerCommand::Reload),
            KeyCode::Enter | KeyCode::Right => self.open_saves_panel(),
            KeyCode::Tab | KeyCode::Char('2') => self.show_cores(),
            KeyCode::Char('3') => self.current_page = Page::Bios,
            KeyCode::Char('4') => self.current_page = Page::Device,
            KeyCode::Char('/') => self.games.focus = GamesPageFocus::Search,
            KeyCode::Char('f') => self.games.show_next_filter(),
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
            KeyCode::Char('i') => self.prompt_for_save_to_import(),
            KeyCode::Char('s') => self.send_for_device_rows(WorkerCommand::PullSaves),
            KeyCode::Char('S') => self.push_save_version(selected),
            KeyCode::Char('x') => self.confirm_trash_version(selected),
            KeyCode::Char('X') => self.confirm_delete_save(),
            KeyCode::Char('?') => self.dialog = Some(help_dialog()),
            KeyCode::Char('q') => self.should_quit = true,
            _ => {}
        }
    }

    fn open_saves_panel(&mut self) {
        if let Some(GamesTableRow::Game(_)) = self.games.selected_row() {
            self.games.focus = GamesPageFocus::SavesPanel(0);
        }
    }

    fn eject(&mut self) {
        match self.device_root() {
            Some(_) => self.worker_handle.send(WorkerCommand::Eject),
            None => self.show_status_message("no device is open"),
        }
    }

    // ---- Keys on the Cores page ------------------------------------------------

    /// Opens the Cores page, requesting the cores unless the worker has already
    /// sent them.
    fn show_cores(&mut self) {
        self.current_page = Page::Cores;
        if self.cores.core_list.is_none() {
            self.worker_handle.send(WorkerCommand::ShowCores);
        }
    }

    fn handle_cores_key(&mut self, key: KeyEvent) {
        if let Some(i) = row_after_key(key, self.cores.table_state.selected().unwrap_or(0)) {
            return self.cores.select_row(i);
        }
        match key.code {
            KeyCode::Char('1') => self.current_page = Page::Games,
            KeyCode::Tab | KeyCode::Char('3') => self.current_page = Page::Bios,
            KeyCode::Char('4') => self.current_page = Page::Device,
            KeyCode::Char('U') => self.install_selected_core(),
            KeyCode::Char('A') => self.update_every_core(),
            KeyCode::Char('X') => self.confirm_remove_core(),
            KeyCode::Char('r') => self.worker_handle.send(WorkerCommand::Reload),
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
            None => self
                .worker_handle
                .send(WorkerCommand::InstallCores(vec![id])),
        }
    }

    fn update_every_core(&mut self) {
        let rows = self
            .cores
            .core_list
            .as_ref()
            .map_or(&[][..], |core_list| &core_list.rows);
        let with_updates = rows.iter().filter(|row| row.has_update());
        let ids: Vec<String> = with_updates.map(|row| row.id().to_string()).collect();
        if ids.is_empty() {
            self.show_status_message("every core is up to date");
        } else {
            self.worker_handle.send(WorkerCommand::InstallCores(ids));
        }
    }

    fn confirm_remove_core(&mut self) {
        let Some(row) = self
            .cores
            .selected_row()
            .filter(|row| row.installed().is_some())
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
                DialogButton::new("Delete", Some(WorkerCommand::RemoveCores(vec![id]))),
                DialogButton::new("Cancel", None),
            ],
            focused_button: 1,
            is_irreversible: true,
        });
    }

    // ---- Keys on the BIOS page -------------------------------------------------

    fn handle_device_page_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Tab | KeyCode::Char('1') => self.current_page = Page::Games,
            KeyCode::Char('2') => self.show_cores(),
            KeyCode::Char('3') => self.current_page = Page::Bios,
            KeyCode::Char('r') => self.worker_handle.send(WorkerCommand::Reload),
            KeyCode::Char('?') => self.dialog = Some(help_dialog()),
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Esc => self.handle_escape(),
            _ => {}
        }
    }

    fn handle_bios_key(&mut self, key: KeyEvent) {
        if let Some(i) = row_after_key(key, self.bios.table_state.selected().unwrap_or(0)) {
            return self.bios.select_row(i);
        }
        match key.code {
            KeyCode::Tab | KeyCode::Char('4') => self.current_page = Page::Device,
            KeyCode::Char('1') => self.current_page = Page::Games,
            KeyCode::Char('2') => self.show_cores(),
            KeyCode::Char('I') => self.prompt_for_bios_file(),
            KeyCode::Char('r') => self.worker_handle.send(WorkerCommand::Reload),
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
        let canceling = key.code == KeyCode::Esc || (ctrl && key.code == KeyCode::Char('g'));
        if canceling {
            if !import.input.unpick() {
                self.prompt = None;
            }
            return;
        }
        let takes_dir = matches!(import.target, ImportTarget::Game);
        if key.code != KeyCode::Enter {
            import.input.handle_key(key);
        } else if !import.input.fill_in_picked() && (takes_dir || !import.input.ends_with_slash()) {
            self.import_input_path();
            return;
        }
        self.request_input_dir();
    }

    /// Imports what the typed path points to. If a game's extension doesn't
    /// indicate its platform, the user picks one first.
    fn import_input_path(&mut self) {
        let Some(Prompt::Import(import)) = self.prompt.take() else {
            return;
        };
        let path = import.input.path();
        match import.target {
            ImportTarget::Save(local) => self
                .worker_handle
                .send(WorkerCommand::ImportSave(local, path)),
            ImportTarget::Cover(local) => self
                .worker_handle
                .send(WorkerCommand::ImportCover(local, path)),
            ImportTarget::Bios(row) => self.worker_handle.send(WorkerCommand::PutBios(row, path)),
            ImportTarget::Game => {
                let extension = path.extension().and_then(|extension| extension.to_str());
                let tells_platform = extension.and_then(Platform::from_extension).is_some();
                if tells_platform || import.input.names_dir() {
                    self.worker_handle
                        .send(WorkerCommand::ImportGame(path, None));
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
        match key.code {
            KeyCode::Esc => self.prompt = None,
            KeyCode::Char('g') if ctrl => self.prompt = None,
            KeyCode::Enter => {
                let Some(Prompt::Rename(rename)) = self.prompt.take() else {
                    return;
                };
                let new = rename.input.value().trim().to_string();
                if new != rename.stem() {
                    self.worker_handle.send(match rename.target {
                        RenameTarget::InLibrary(local) => {
                            WorkerCommand::RenameGamesInLibrary(vec![(local, new)])
                        }
                        RenameTarget::OnDevice(on_device) => {
                            WorkerCommand::RenameOnDevice(on_device, new)
                        }
                    });
                }
            }
            _ => {
                path_input::edit(&mut rename.input, key);
            }
        }
    }

    fn prompt_for_name_in_library(&mut self) {
        let Some(local) = self
            .games
            .selected_row()
            .and_then(GamesTableRow::local_entry)
            .cloned()
        else {
            self.show_status_message("nothing here is in the library");
            return;
        };
        self.open_rename_prompt(RenameTarget::InLibrary(local));
    }

    fn prompt_for_name_on_device(&mut self) {
        let Some(on_device) = self
            .games
            .selected_row()
            .and_then(GamesTableRow::device_entry)
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
                self.worker_handle
                    .send(WorkerCommand::ImportGame(file, platform));
            }
            KeyCode::Esc => self.prompt = None,
            _ => {}
        }
    }

    /// Requests the folder of the path being typed, if it has changed.
    fn request_input_dir(&mut self) {
        if let Some(Prompt::Import(import)) = &mut self.prompt
            && let Some(dir) = import.input.dir_to_read()
        {
            self.dir_reader.request_dir(dir);
        }
    }

    fn prompt_for_save_to_import(&mut self) {
        if let Some(local) = self.selected_local_entry_or_say_none() {
            self.prompt_for_import_path(ImportTarget::Save(local));
        }
    }

    fn prompt_for_cover_to_import(&mut self) {
        if let Some(local) = self.selected_local_entry_or_say_none() {
            self.prompt_for_import_path(ImportTarget::Cover(local));
        }
    }

    /// The selected game in the library. If there is none, the status line says
    /// so.
    fn selected_local_entry_or_say_none(&mut self) -> Option<LocalEntry> {
        let selected = self
            .games
            .selected_row()
            .and_then(GamesTableRow::local_entry);
        let local = selected.cloned();
        if local.is_none() {
            self.show_status_message("nothing here is in the library");
        }
        local
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
                    .and_then(|button| button.command.take());
                self.dialog = None;
                match command {
                    Some(command) => {
                        self.games.marked_rows.clear();
                        self.worker_handle.send(command);
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
        let mut lines = vec![
            "These go to the system's trash, with their covers. Their saves stay.".to_string(),
            String::new(),
        ];
        lines.extend(cut_short_for_dialog(
            entries.iter().map(|local| local.name.clone()).collect(),
        ));
        self.inapplicable_row_count += inapplicable;
        self.dialog = Some(Dialog {
            title: format!("Trash {}", pluralized(entries.len(), "game")),
            lines,
            buttons: vec![
                DialogButton::new("Trash", Some(WorkerCommand::TrashGames(entries))),
                DialogButton::new("Cancel", None),
            ],
            focused_button: 0,
            is_irreversible: false,
        });
    }

    /// Asks the worker what the target games would be renamed to. Its answer
    /// opens the dialog. While it is busy that answer could take long to come,
    /// so nothing is asked then.
    fn propose_renames_to_standard_names(&mut self) {
        let (entries, inapplicable) = self.games.library_entries_to_act_on();
        if entries.is_empty() {
            self.show_status_message("nothing here is in the library");
            return;
        }
        if self.is_busy() {
            self.show_status_message("busy; try again when this is done");
            return;
        }
        self.inapplicable_row_count += inapplicable;
        self.worker_handle
            .send(WorkerCommand::ProposeRenames(entries));
    }

    fn confirm_trash_orphans(&mut self) {
        let targets = self.games.rows_to_act_on();
        let orphans = targets.iter().filter_map(|row| row.orphan_in_library());
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
                DialogButton::new("Trash", Some(WorkerCommand::TrashOrphanSaves(orphans))),
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
            .and_then(GamesTableRow::local_entry)
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
                DialogButton::new(
                    "Trash",
                    Some(WorkerCommand::TrashSaveVersion(local, version)),
                ),
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
            .map(|entry| entry.path.display().to_string())
            .collect();
        let is_single = entries.len() == 1;
        let (it, them) = if is_single {
            ("it", "It isn't")
        } else {
            ("them", "Some aren't")
        };
        let mut lines = vec![format!(
            "This deletes {it} from the device for good: there is no trash there."
        )];
        let only_here = self
            .games
            .rows_to_act_on()
            .iter()
            .any(|row| matches!(row, GamesTableRow::Game(GameRow::OnDevice(_))));
        if only_here {
            lines.push(format!("{them} in the library."));
        }
        lines.push(if is_single {
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
                DialogButton::new("Delete", Some(WorkerCommand::DeleteGames(entries))),
                DialogButton::new("Cancel", None),
            ],
            focused_button: 0,
            is_irreversible: true,
        });
    }

    fn confirm_delete_orphans(&mut self) {
        let targets = self.games.rows_to_act_on();
        let orphans = targets.iter().filter_map(|row| row.orphan_on_device());
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
                DialogButton::new("Delete", Some(WorkerCommand::DeleteOrphanSaves(orphans))),
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
        let unsaved =
            self.games.rows_to_act_on().iter().any(
                |table_row| matches!(table_row, GamesTableRow::Game(row) if row.has_new_save_on_device()),
            );
        if unsaved {
            lines.push("The library doesn't have this save.".to_string());
        }
        self.dialog = Some(Dialog {
            title: "Delete a save from the device".to_string(),
            lines,
            buttons: vec![
                DialogButton::new("Delete", Some(WorkerCommand::DeleteSaves(entries))),
                DialogButton::new("Cancel", None),
            ],
            focused_button: 1,
            is_irreversible: true,
        });
    }

    // ---- Commands for the worker -----------------------------------------------

    fn send_for_device_rows(&mut self, command: fn(Vec<DeviceEntry>) -> WorkerCommand) {
        let (entries, inapplicable) = self.games.device_entries_to_act_on();
        self.send_for_entries(
            entries,
            inapplicable,
            command,
            "nothing here is on the device",
        );
    }

    fn send_for_library_rows(&mut self, command: fn(Vec<LocalEntry>) -> WorkerCommand) {
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
        command: impl FnOnce(Vec<T>) -> WorkerCommand,
        message_if_none: &str,
    ) {
        if entries.is_empty() {
            self.show_status_message(message_if_none);
            return;
        }
        self.inapplicable_row_count += inapplicable;
        self.games.marked_rows.clear();
        self.worker_handle.send(command(entries));
    }

    /// Pushes the newest versions onto the target copies whose game is in the
    /// library.
    fn push_saves(&mut self) {
        let targets = self.games.rows_to_act_on();
        let entries: Vec<DeviceEntry> = targets
            .iter()
            .filter_map(|row| match row {
                GamesTableRow::Game(GameRow::Both { on_device, .. }) => Some(on_device.clone()),
                _ => None,
            })
            .collect();
        let inapplicable = targets.len() - entries.len();
        self.send_for_entries(entries, inapplicable, WorkerCommand::PushSaves, NOT_ON_BOTH);
    }

    /// Pushes the version selected in the saves panel onto the selected row's
    /// copy.
    fn push_save_version(&mut self, selected: usize) {
        let version = self.games.selected_save_versions().get(selected).cloned();
        let on_device = self
            .games
            .selected_row()
            .and_then(GamesTableRow::device_entry)
            .cloned();
        match (on_device, version) {
            (Some(on_device), Some(version)) => {
                self.worker_handle
                    .send(WorkerCommand::PushSaveVersion(on_device, version));
            }
            _ => self.show_status_message(NOT_ON_BOTH),
        }
    }

    /// Explains why a key did nothing.
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
const PAGE_KEY_ROW_COUNT: isize = 20;

fn pluralized(count: usize, what: &str) -> String {
    match count {
        1 => format!("1 {what}"),
        count => format!("{count} {what}s"),
    }
}

/// Keeps the first 10 lines, and says how many more there are.
fn cut_short_for_dialog(mut lines: Vec<String>) -> Vec<String> {
    const SHOWN: usize = 10;
    if lines.len() > SHOWN {
        let more = lines.len() - SHOWN;
        lines.truncate(SHOWN);
        lines.push(format!("… and {more} more"));
    }
    lines
}

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

/// Asks whether to rename games to their DAT names, showing the new names first.
fn renames_dialog(renames: Vec<(LocalEntry, String)>) -> Dialog {
    let mut lines = vec![
        "These get the names the DATs have; saves and covers go with them.".to_string(),
        "The device isn't touched.".to_string(),
        String::new(),
    ];
    // Each game takes two lines because names are long, so this shows half as
    // many as other dialogs.
    const SHOWN: usize = 5;
    for (local, new) in renames.iter().take(SHOWN) {
        lines.push(local.stem().to_string());
        lines.push(format!("  → {new}"));
    }
    if renames.len() > SHOWN {
        lines.push(format!("… and {} more", renames.len() - SHOWN));
    }
    Dialog {
        title: format!("Rename {}", pluralized(renames.len(), "game")),
        lines,
        buttons: vec![
            DialogButton::new("Rename", Some(WorkerCommand::RenameGamesInLibrary(renames))),
            DialogButton::new("Cancel", None),
        ],
        focused_button: 0,
        is_irreversible: false,
    }
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

/// Row `i` of `len`, the last row if there are fewer, or none if there are
/// none.
fn clamped_row(i: usize, len: usize) -> Option<usize> {
    (len > 0).then(|| i.min(len - 1))
}

/// The row a key moves the selection to from row `at`, if it is a movement key.
/// It may be past the last row.
fn row_after_key(key: KeyEvent, at: usize) -> Option<usize> {
    Some(match key.code {
        KeyCode::Down | KeyCode::Char('j') => at + 1,
        KeyCode::Up | KeyCode::Char('k') => at.saturating_sub(1),
        KeyCode::PageDown => at + PAGE_KEY_ROW_COUNT as usize,
        KeyCode::PageUp => at.saturating_sub(PAGE_KEY_ROW_COUNT as usize),
        KeyCode::Char('g') | KeyCode::Home => 0,
        KeyCode::Char('G') | KeyCode::End => usize::MAX,
        _ => return None,
    })
}

fn help_dialog() -> Dialog {
    let lines = [
        "Move      j k, arrows, C-n C-p; C-v M-v page; g G top, bottom",
        "Mark      m mark or unmark; M mark all shown, again to clear every mark",
        "          Keys act on what is marked, or else on the selected row.",
        "",
        "          Lowercase changes the library, Shift changes the device.",
        "Library   i import a game from a file; p pull the game; s back up the save",
        "          n rename; t rename to what the DATs call it; it asks",
        "          x game and cover to the system's trash",
        "Device    P push the game; S push the newest save; N rename; E eject",
        "          X delete from the device: for good, it asks",
        "          On an orphan save, x and X remove the save.",
        "Covers    a download; c choose a file; d update the DATs, which name games",
        "",
        "Saves     Enter opens them; there S pushes the version selected,",
        "          x trashes it, X deletes the device's save,",
        "          i imports one from a file, Esc goes back",
        "",
        "Pages     Tab the next one; 1 games, 2 cores, 3 BIOS, 4 device",
        "Cores     U installs or updates, A updates all, X deletes from the device",
        "BIOS      I puts a file of yours where the cores look for it",
        "",
        "Others    / search; f filter; r read everything again; ? this; q quit",
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
    use crate::tui::{ui, worker::QueuedCommand};
    use pocketeer_core::{
        cores::{BiosRow, BiosStatus, InstalledCore, InventoryCore},
        device::OrphanDeviceSave,
        device::{DiskSpace, Firmware},
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
            .map(|(i, save_bytes)| Save::from_bytes(save_bytes, at(i)));
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
            save: save.map(|save_bytes| Save::from_bytes(save_bytes, UNIX_EPOCH)),
        }
    }

    /// One row only in the library, one on both sides, one only on the device,
    /// and an orphan save.
    fn rows() -> Vec<GamesTableRow> {
        vec![
            GamesTableRow::Game(GameRow::InLibrary(local("Alpha.gba", 1, &[]))),
            GamesTableRow::Game(GameRow::Both {
                local: local("Both.gba", 2, &[b"old", b"new"]),
                on_device: on_device("Both.gba", 2, Some(b"new")),
            }),
            GamesTableRow::Game(GameRow::OnDevice(on_device("Hack.gba", 3, None))),
            GamesTableRow::OrphanSaveOnDevice(OrphanDeviceSave {
                platform: Platform::Gba,
                path: PathBuf::from("Gone.sav"),
                save: Save::from_bytes(b"x", UNIX_EPOCH),
            }),
        ]
    }

    fn app() -> (App, Receiver<QueuedCommand>) {
        let (worker_handle, rx) = WorkerHandle::recording();
        let cover_loader = CoverLoader::spawn(Picker::halfblocks(), || {});
        let paths = DirReader::spawn(|| {});
        let mut app = App::new(worker_handle, cover_loader, paths);
        app.handle_worker_message(WorkerMessage::GamesPageChanged(GamesPageData {
            rows: rows(),
            device_root: Some(PathBuf::from("/Volumes/POCKET")),
        }));
        (app, rx)
    }

    fn press(app: &mut App, keys: &str) {
        for character in keys.chars() {
            app.handle_key(KeyEvent::from(match character {
                '\n' => KeyCode::Enter,
                '\x1b' => KeyCode::Esc,
                character => KeyCode::Char(character),
            }));
        }
    }

    fn sent(rx: &Receiver<QueuedCommand>) -> Vec<WorkerCommand> {
        rx.try_iter().map(|queued| queued.command).collect()
    }

    fn paths(entries: &[DeviceEntry]) -> Vec<&Path> {
        entries.iter().map(|entry| entry.path.as_path()).collect()
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

    /// On the Cores page, one core each that isn't on the device, has an
    /// update, is up to date, and can't be installed.
    fn on_cores_page() -> (App, Receiver<QueuedCommand>) {
        let (mut app, rx) = app();
        app.handle_key(KeyEvent::from(KeyCode::Tab));
        app.handle_worker_message(WorkerMessage::CoreListChanged {
            rows: vec![
                core_row("new.Core", None, Some("1")),
                core_row("old.Core", Some("1"), Some("2")),
                core_row("same.Core", Some("2"), Some("2")),
                core_row("mine.Core", Some("1"), None),
            ],
            failure: None,
        });
        assert_eq!(sent(&rx), [WorkerCommand::ShowCores]);
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

    /// On the BIOS page with a device open, a file that is there and then one
    /// that is missing.
    fn on_bios_page() -> (App, Receiver<QueuedCommand>) {
        let (mut app, rx) = app();
        press(&mut app, "3");
        app.handle_worker_message(WorkerMessage::BiosFilesChanged(vec![
            bios_row("there.bin", BiosStatus::Present),
            bios_row("gone.bin", BiosStatus::Missing),
        ]));
        (app, rx)
    }

    mod actions {
        use super::*;

        #[test]
        fn reports_why_key_does_not_apply_without_sending() {
            type Setup = fn() -> (App, Receiver<QueuedCommand>);
            let in_library = "nothing here is in the library";
            let cases: [(Setup, &str, &str); 12] = [
                (app, "p", "nothing here is on the device"),
                (app, "jjjx", in_library),
                (app, "jjc", in_library),
                (app, "jj\ni", in_library),
                (app, "jjt", in_library),
                (app, "jjn", in_library),
                (app, "N", "this game isn't on the device"),
                (on_cores_page, "jjU", "the device has the newest version"),
                (
                    on_cores_page,
                    "jjjU",
                    "only the cores with a latest version can be installed",
                ),
                (on_cores_page, "X", "this core isn't on the device"),
                (on_bios_page, "I", "the device has this file already"),
                (app, "3I", "no core on the device needs a file"),
            ];
            for (setup, keys, message) in cases {
                let (mut app, rx) = setup();
                press(&mut app, keys);
                assert_eq!(app.status_message().unwrap().text, message, "{keys}");
                assert!(app.dialog().is_none() && app.prompt().is_none(), "{keys}");
                assert_eq!(sent(&rx), [], "{keys}");
            }
        }

        #[test]
        fn acts_only_on_selected_version_in_saves_panel() {
            let (mut app, rx) = app();
            // A mark elsewhere doesn't count in the panel.
            press(&mut app, "mk");
            press(&mut app, "j\n");
            assert_eq!(app.games.selected_save_versions().len(), 2);
            press(&mut app, "jS");
            let [WorkerCommand::PushSaveVersion(game, version)] = &sent(&rx)[..] else {
                panic!()
            };
            assert_eq!(game.path, Path::new("Both.gba"));
            assert_eq!(version, &app.games.selected_save_versions()[1]);
            press(&mut app, "x");
            press(&mut app, "y");
            let [WorkerCommand::TrashSaveVersion(game, version)] = &sent(&rx)[..] else {
                panic!()
            };
            assert_eq!(game.name, "Both.gba");
            assert_eq!(version, &app.games.selected_save_versions()[1]);
            press(&mut app, "\x1b");
            assert_eq!(app.games.selected_version_index(), None);
        }

        #[test]
        fn escape_cancels_running_command() {
            let (mut app, rx) = app();
            press(&mut app, "jp");
            let is_canceled = rx.try_recv().unwrap().is_canceled;
            assert!(!is_canceled.load(Ordering::Relaxed));
            app.handle_worker_message(WorkerMessage::CommandStarted {
                label: "pulling games".into(),
                target_count: 2,
            });
            press(&mut app, "\x1b");
            assert!(is_canceled.load(Ordering::Relaxed));
            assert!(app.command_status().unwrap().is_canceling);
            app.handle_worker_message(WorkerMessage::CommandFinished {
                summary: "stopped".into(),
                failed: false,
            });
            assert!(!app.is_busy());
        }
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
            assert_eq!(app.current_page(), Page::Device);
            app.handle_key(KeyEvent::from(KeyCode::Tab));
            assert_eq!(app.current_page(), Page::Games);
        }

        #[test]
        fn number_keys_jump_to_pages() {
            let (mut app, _rx) = app();
            press(&mut app, "4");
            assert_eq!(app.current_page(), Page::Device);
            press(&mut app, "3");
            assert_eq!(app.current_page(), Page::Bios);
            press(&mut app, "4");
            assert_eq!(app.current_page(), Page::Device);
            press(&mut app, "2");
            assert_eq!(app.current_page(), Page::Cores);
            press(&mut app, "4");
            assert_eq!(app.current_page(), Page::Device);
            press(&mut app, "3");
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

    mod filter {
        use super::*;

        fn visible_names(app: &App) -> Vec<String> {
            app.games.visible_rows().map(name_of).collect()
        }

        fn with_orphan_in_library() -> (App, Receiver<QueuedCommand>) {
            let (mut app, rx) = app();
            let mut rows = rows();
            rows.push(GamesTableRow::OrphanSaveInLibrary(OrphanLocalSave {
                platform: Platform::Gba,
                stem: "Lost".to_string(),
                save: SaveHistory::from_saves(vec![Save::from_bytes(b"x", UNIX_EPOCH)]).unwrap(),
            }));
            app.handle_worker_message(WorkerMessage::GamesPageChanged(GamesPageData {
                rows,
                device_root: Some(PathBuf::from("/Volumes/POCKET")),
            }));
            (app, rx)
        }

        #[test]
        fn f_cycles_through_filters_with_their_orphans() {
            let (mut app, _rx) = with_orphan_in_library();
            let every_row = ["Alpha.gba", "Both.gba", "Hack.gba", "Gone.sav", "Lost"].as_slice();
            assert_eq!(app.games.filter(), GamesFilter::All);
            assert_eq!(visible_names(&app), every_row);
            press(&mut app, "f");
            assert_eq!(app.games.filter(), GamesFilter::InLibrary);
            assert_eq!(visible_names(&app), ["Alpha.gba", "Both.gba", "Lost"]);
            press(&mut app, "f");
            assert_eq!(app.games.filter(), GamesFilter::OnDevice);
            assert_eq!(visible_names(&app), ["Both.gba", "Hack.gba", "Gone.sav"]);
            press(&mut app, "f");
            assert_eq!(app.games.filter(), GamesFilter::All);
            assert_eq!(visible_names(&app), every_row);
        }

        #[test]
        fn applies_filter_and_search_together() {
            let (mut app, _rx) = app();
            press(&mut app, "/h\nf");
            assert_eq!(visible_names(&app), ["Alpha.gba", "Both.gba"]);
        }

        #[test]
        fn keeps_selection_on_row_still_shown() {
            let (mut app, _rx) = app();
            // Both moves to the top, and the row now second is Hack.
            press(&mut app, "jff");
            assert_eq!(name_of(app.games.selected_row().unwrap()), "Both.gba");
        }

        #[test]
        fn keeps_filter_when_rows_change() {
            let (mut app, _rx) = app();
            press(&mut app, "f");
            app.handle_worker_message(WorkerMessage::GamesPageChanged(GamesPageData {
                rows: rows(),
                device_root: None,
            }));
            assert_eq!(visible_names(&app), ["Alpha.gba", "Both.gba"]);
        }

        #[test]
        fn acts_on_marked_rows_that_filter_hides() {
            let (mut app, rx) = app();
            press(&mut app, "mff");
            assert_eq!(visible_names(&app), ["Both.gba", "Hack.gba", "Gone.sav"]);
            assert_eq!(app.games.marked_count(), 1);
            press(&mut app, "x\n");
            let [WorkerCommand::TrashGames(games)] = &sent(&rx)[..] else {
                panic!("expected the marked game to be trashed");
            };
            assert_eq!(games[0].name, "Alpha.gba");
        }

        #[test]
        fn esc_clears_search_and_keeps_filter() {
            let (mut app, _rx) = app();
            press(&mut app, "f/al\n\x1b");
            assert_eq!(app.games.filter(), GamesFilter::InLibrary);
            assert_eq!(app.games.search_text(), "");
        }
    }

    mod marks {
        use super::*;

        #[test]
        fn acts_on_selected_row_without_marks() {
            let (mut app, rx) = app();
            press(&mut app, "jp");
            let [WorkerCommand::PullGames(games)] = &sent(&rx)[..] else {
                panic!()
            };
            assert_eq!(paths(games), [Path::new("Both.gba")]);
            press(&mut app, "S");
            let [WorkerCommand::PushSaves(games)] = &sent(&rx)[..] else {
                panic!()
            };
            assert_eq!(paths(games), [Path::new("Both.gba")]);
            press(&mut app, "ka");
            let [WorkerCommand::FetchCovers(games)] = &sent(&rx)[..] else {
                panic!()
            };
            assert_eq!(games[0].name, "Alpha.gba");
        }

        #[test]
        fn acts_on_all_marked_rows_and_clears_marks() {
            let (mut app, rx) = app();
            // Each mark moves on.
            press(&mut app, "jmms");
            let [WorkerCommand::PullSaves(games)] = &sent(&rx)[..] else {
                panic!()
            };
            assert_eq!(paths(games), [Path::new("Both.gba"), Path::new("Hack.gba")]);
            assert_eq!(app.games.marked_count(), 0);
        }

        #[test]
        fn m_unmarks_row_already_marked() {
            let (mut app, _rx) = app();
            press(&mut app, "mm");
            assert_eq!(app.games.marked_count(), 2);
            press(&mut app, "km");
            assert_eq!(app.games.marked_count(), 1);
        }

        #[test]
        fn shift_m_clears_marks_of_rows_hidden_by_search() {
            let (mut app, _rx) = app();
            press(&mut app, "jjm/alpha\nM");
            assert_eq!(app.games.marked_count(), 2);
            press(&mut app, "M");
            assert_eq!(app.games.marked_count(), 0);
        }

        #[test]
        fn acts_on_marked_rows_hidden_by_search() {
            let (mut app, rx) = app();
            press(&mut app, "jjm/alpha\nmP");
            let [WorkerCommand::PushGames(games)] = &sent(&rx)[..] else {
                panic!()
            };
            // The game only on the device, hidden, has nothing to push.
            assert_eq!(games.len(), 1);
            assert_eq!(games[0].name, "Alpha.gba");
        }

        #[test]
        fn keeps_selection_and_marks_on_same_rows_after_rows_change() {
            let (mut app, _rx) = app();
            press(&mut app, "jjkm");
            let mut fewer = rows();
            fewer.remove(0);
            app.handle_worker_message(WorkerMessage::GamesPageChanged(GamesPageData {
                rows: fewer,
                device_root: None,
            }));
            assert_eq!(name_of(app.games.selected_row().unwrap()), "Hack.gba");
            assert_eq!(app.games.marked_count(), 1);
        }
    }

    mod dialogs {
        use super::*;

        #[test]
        fn holds_dialog_from_worker_until_open_dialog_closes() {
            for (message, title, line) in [
                (
                    WorkerMessage::DuplicateGamesSkipped(vec!["a line".to_string()]),
                    "Duplicate games on the device",
                    "a line",
                ),
                (
                    WorkerMessage::RenamesProposed(vec![(
                        local("Alpha.gba", 1, &[]),
                        "Alpha (USA)".to_string(),
                    )]),
                    "Rename 1 game",
                    "  → Alpha (USA)",
                ),
            ] {
                let (mut app, rx) = app();
                press(&mut app, "jjX");
                let asking = app.dialog().unwrap().title.clone();
                app.handle_worker_message(message);
                assert_eq!(app.dialog().unwrap().title, asking);
                // Closing the open dialog shows the next, and closing that
                // shows nothing.
                press(&mut app, "\x1b");
                assert!(sent(&rx).is_empty());
                let dialog = app.dialog().unwrap();
                assert_eq!(dialog.title, title);
                assert!(dialog.lines.iter().any(|text| text == line), "{title}");
                press(&mut app, "\x1b");
                assert!(app.dialog().is_none());
            }
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
            assert!(matches!(sent(&rx)[..], [WorkerCommand::DeleteGames(_)]));
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
            assert!(matches!(
                sent(&rx)[..],
                [WorkerCommand::DeleteOrphanSaves(_)]
            ));
        }

        #[test]
        fn shift_x_in_saves_panel_confirms_deleting_save_and_defaults_to_cancel() {
            let (mut app, rx) = app();
            press(&mut app, "j\nX");
            let dialog = app.dialog().unwrap();
            assert!(dialog.is_irreversible);
            assert_eq!(dialog.focused_button, 1);
            press(&mut app, "\n");
            assert!(sent(&rx).is_empty());
            press(&mut app, "Xh\n");
            assert!(matches!(sent(&rx)[..], [WorkerCommand::DeleteSaves(_)]));
        }

        #[test]
        fn x_lists_games_before_trashing() {
            let (mut app, rx) = app();
            press(&mut app, "x");
            let dialog = app.dialog().unwrap();
            assert!(dialog.lines.iter().any(|line| line == "Alpha.gba"));
            press(&mut app, "y");
            assert!(matches!(sent(&rx)[..], [WorkerCommand::TrashGames(_)]));
        }
    }

    mod import {
        use super::*;

        #[test]
        fn imports_without_asking_for_known_extension_or_dir() {
            for (keys, expected) in [
                ("/some/Mine.GBA\n", "/some/Mine.GBA"),
                ("/some/roms/\n", "/some/roms"),
            ] {
                let (mut app, rx) = app();
                press(&mut app, "i");
                retype(&mut app, keys);
                let [WorkerCommand::ImportGame(path, None)] = &sent(&rx)[..] else {
                    panic!("{keys}")
                };
                assert_eq!(path, Path::new(expected));
                assert!(app.prompt().is_none());
            }
        }

        #[test]
        fn asks_for_platform_when_extension_gives_none() {
            let (mut app, rx) = app();
            press(&mut app, "i");
            retype(&mut app, "/some/x.bin\n");
            assert!(sent(&rx).is_empty());
            assert!(matches!(app.prompt(), Some(Prompt::Platform(_))));
            press(&mut app, "j\n");
            let [WorkerCommand::ImportGame(path, Some(Platform::Gbc))] = &sent(&rx)[..] else {
                panic!()
            };
            assert_eq!(path, Path::new("/some/x.bin"));
            assert!(app.prompt().is_none());
        }

        #[test]
        fn ctrl_n_moves_down_platform_list() {
            let (mut app, rx) = app();
            press(&mut app, "i");
            retype(&mut app, "/some/x.bin\n");
            app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
            press(&mut app, "\n");
            let [WorkerCommand::ImportGame(_, Some(Platform::Gbc))] = &sent(&rx)[..] else {
                panic!()
            };
        }

        #[test]
        fn sends_keys_to_dialog_before_import_window() {
            let (mut app, rx) = app();
            press(&mut app, "i");
            retype(&mut app, "/some/Mine.gba");
            app.handle_worker_message(WorkerMessage::DuplicateGamesSkipped(vec![
                "a line".to_string(),
            ]));
            press(&mut app, "\n");
            assert!(app.dialog().is_none());
            assert!(sent(&rx).is_empty());
            assert!(app.prompt().is_some());
            press(&mut app, "\n");
            let [WorkerCommand::ImportGame(path, None)] = &sent(&rx)[..] else {
                panic!()
            };
            assert_eq!(path, Path::new("/some/Mine.gba"));
        }

        #[test]
        fn escape_in_platform_list_imports_nothing() {
            let (mut app, rx) = app();
            press(&mut app, "i");
            retype(&mut app, "/some/x.bin\n\x1b");
            assert!(app.prompt().is_none());
            assert!(sent(&rx).is_empty());
        }

        #[test]
        fn c_imports_cover_for_selected_game() {
            let (mut app, rx) = app();
            press(&mut app, "c");
            assert!(app.prompt().is_some());
            retype(&mut app, "/some/mine.png\n");
            let [WorkerCommand::ImportCover(game, path)] = &sent(&rx)[..] else {
                panic!()
            };
            assert_eq!(game.name, "Alpha.gba");
            assert_eq!(path, Path::new("/some/mine.png"));
            assert!(app.prompt().is_none());
        }

        #[test]
        fn enter_on_dir_keeps_save_window_open() {
            let (mut app, rx) = app();
            press(&mut app, "j\ni\n");
            assert!(app.prompt().is_some());
            assert!(sent(&rx).is_empty());
        }

        #[test]
        fn imports_save_for_game_of_selected_row() {
            let (mut app, rx) = app();
            press(&mut app, "j\ni");
            assert!(app.prompt().is_some());
            // Keys are text here, whatever they do elsewhere.
            press(&mut app, "x");
            assert!(sent(&rx).is_empty());
            retype(&mut app, "/some/Mine.sav\n");
            let [WorkerCommand::ImportSave(game, path)] = &sent(&rx)[..] else {
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
            press(&mut app, "j\ni");
            type_path(&mut app, &typed);
            app.handle_key(KeyEvent::from(KeyCode::Down));
            press(&mut app, "\n");
            assert!(sent(&rx).is_empty());
            press(&mut app, "\n");
            let [WorkerCommand::ImportSave(_, path)] = &sent(&rx)[..] else {
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
            press(&mut app, "j\ni");
            type_path(&mut app, &typed);
            app.handle_key(KeyEvent::from(KeyCode::Down));
            press(&mut app, "\x1b");
            let Some(Prompt::Import(import)) = app.prompt() else {
                panic!()
            };
            assert_eq!(import.input.picked_match_index(), None);
            press(&mut app, "\x1b");
            assert!(app.prompt().is_none());
            assert_eq!(app.games.selected_version_index(), Some(0));
            assert!(sent(&rx).is_empty());
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
            let [WorkerCommand::RenameGamesInLibrary(games)] = &sent(&rx)[..] else {
                panic!()
            };
            let [(game, new)] = &games[..] else { panic!() };
            assert_eq!((game.name.as_str(), new.as_str()), ("Alpha.gba", "Alphaqx"));
            assert!(app.prompt().is_none());
        }

        /// The worker's answer that Alpha has another name in the DATs.
        fn answer_renaming_alpha(app: &mut App) {
            app.handle_worker_message(WorkerMessage::RenamesProposed(vec![(
                local("Alpha.gba", 1, &[]),
                "Alpha (USA)".to_string(),
            )]));
        }

        fn names_in(renames: &[(LocalEntry, String)]) -> Vec<(&str, &str)> {
            renames
                .iter()
                .map(|(local, new)| (local.name.as_str(), new.as_str()))
                .collect()
        }

        #[test]
        fn t_asks_worker_about_games_in_library() {
            let (mut app, rx) = app();
            press(&mut app, "Mt");
            let [WorkerCommand::ProposeRenames(games)] = &sent(&rx)[..] else {
                panic!()
            };
            let names: Vec<&str> = games.iter().map(|game| game.name.as_str()).collect();
            assert_eq!(names, ["Alpha.gba", "Both.gba"]);
            assert!(app.dialog().is_none());
        }

        #[test]
        fn t_asks_nothing_while_worker_is_busy() {
            let (mut app, rx) = app();
            app.handle_worker_message(WorkerMessage::CommandStarted {
                label: "pulling games".to_string(),
                target_count: 1,
            });
            press(&mut app, "t");
            assert!(sent(&rx).is_empty());
            assert_eq!(
                app.status_message().unwrap().text,
                "busy; try again when this is done"
            );
        }

        #[test]
        fn answer_shows_new_name_and_sends_rename_when_confirmed() {
            let (mut app, rx) = app();
            answer_renaming_alpha(&mut app);
            let dialog = app.dialog().unwrap();
            assert_eq!(dialog.title, "Rename 1 game");
            assert_eq!(dialog.lines[3..], ["Alpha", "  → Alpha (USA)"]);
            assert!(sent(&rx).is_empty());
            press(&mut app, "\n");
            let [WorkerCommand::RenameGamesInLibrary(games)] = &sent(&rx)[..] else {
                panic!()
            };
            assert_eq!(names_in(games), [("Alpha.gba", "Alpha (USA)")]);
        }

        #[test]
        fn answer_shows_five_games_and_counts_rest() {
            let (mut app, rx) = app();
            let renames = (0..7)
                .map(|i| (local(&format!("{i}.gba"), i, &[]), format!("Game {i}")))
                .collect();
            app.handle_worker_message(WorkerMessage::RenamesProposed(renames));
            let dialog = app.dialog().unwrap();
            assert_eq!(dialog.title, "Rename 7 games");
            assert_eq!(dialog.lines.last().unwrap(), "… and 2 more");
            press(&mut app, "\n");
            let [WorkerCommand::RenameGamesInLibrary(games)] = &sent(&rx)[..] else {
                panic!()
            };
            assert_eq!(games.len(), 7);
        }

        #[test]
        fn counts_rows_left_out_of_renames() {
            let (mut app, _rx) = app();
            press(&mut app, "Mt");
            answer_renaming_alpha(&mut app);
            press(&mut app, "\n");
            app.handle_worker_message(WorkerMessage::CommandFinished {
                summary: "renamed".to_string(),
                failed: false,
            });
            assert_eq!(app.status_message().unwrap().text, "renamed (2 left out)");
        }

        #[test]
        fn empty_answer_reports_no_game_and_forgets_rows_left_out() {
            let (mut app, _rx) = app();
            press(&mut app, "Mt");
            app.handle_worker_message(WorkerMessage::RenamesProposed(Vec::new()));
            assert!(app.dialog().is_none());
            assert_eq!(app.status_message().unwrap().text, "no game to rename");
            app.handle_worker_message(WorkerMessage::CommandFinished {
                summary: "next".to_string(),
                failed: false,
            });
            assert_eq!(app.status_message().unwrap().text, "next");
        }

        #[test]
        fn shift_n_renames_copy_on_device() {
            let (mut app, rx) = app();
            press(&mut app, "jN!\n");
            let [WorkerCommand::RenameOnDevice(game, new)] = &sent(&rx)[..] else {
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
        fn keeps_selection_and_mark_on_renamed_game() {
            let (mut app, _rx) = app();
            // Marks the first and moves on to the second, which goes first.
            press(&mut app, "m");
            let mut rows = rows();
            let GamesTableRow::Game(GameRow::Both { local, .. }) = &mut rows[1] else {
                panic!()
            };
            local.name = "Aaa.gba".to_string();
            let renamed = rows.remove(1);
            rows.insert(0, renamed);
            app.handle_worker_message(WorkerMessage::GamesPageChanged(GamesPageData {
                rows,
                device_root: Some(PathBuf::from("/Volumes/POCKET")),
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
        fn shift_u_installs_new_core_or_updates_installed_one() {
            let (mut app, rx) = on_cores_page();
            press(&mut app, "U");
            assert_eq!(sent(&rx), [WorkerCommand::InstallCores(ids(&["new.Core"]))]);
            press(&mut app, "jU");
            assert_eq!(sent(&rx), [WorkerCommand::InstallCores(ids(&["old.Core"]))]);
        }

        #[test]
        fn shift_u_reports_no_device() {
            let (mut app, rx) = on_cores_page();
            app.handle_worker_message(WorkerMessage::GamesPageChanged(GamesPageData {
                rows: Vec::new(),
                device_root: None,
            }));
            press(&mut app, "U");
            assert_eq!(app.status_message().unwrap().text, "no device is open");
            assert_eq!(sent(&rx), []);
        }

        #[test]
        fn shift_a_updates_cores_with_newer_release() {
            let (mut app, rx) = on_cores_page();
            press(&mut app, "A");
            assert_eq!(sent(&rx), [WorkerCommand::InstallCores(ids(&["old.Core"]))]);
        }

        #[test]
        fn shift_a_reports_every_core_up_to_date() {
            let (mut app, rx) = on_cores_page();
            app.handle_worker_message(WorkerMessage::CoreListChanged {
                rows: vec![core_row("same.Core", Some("2"), Some("2"))],
                failure: None,
            });
            press(&mut app, "A");
            assert_eq!(
                app.status_message().unwrap().text,
                "every core is up to date"
            );
            assert_eq!(sent(&rx), []);
        }

        #[test]
        fn shift_x_confirms_before_removing_and_defaults_to_cancel() {
            let (mut app, rx) = on_cores_page();
            press(&mut app, "jX");
            let dialog = app.dialog().unwrap();
            assert!(dialog.is_irreversible);
            assert_eq!(dialog.focused_button, 1);
            press(&mut app, "\n");
            assert_eq!(sent(&rx), []);
            press(&mut app, "Xh\n");
            assert_eq!(sent(&rx), [WorkerCommand::RemoveCores(ids(&["old.Core"]))]);
        }

        #[test]
        fn keeps_selection_on_same_core_when_rows_change() {
            let (mut app, _rx) = on_cores_page();
            press(&mut app, "jj");
            app.handle_worker_message(WorkerMessage::CoreListChanged {
                rows: vec![
                    core_row("old.Core", Some("2"), Some("2")),
                    core_row("same.Core", Some("2"), Some("2")),
                ],
                failure: None,
            });
            assert_eq!(app.cores.selected_row().unwrap().id(), "same.Core");
            app.handle_worker_message(WorkerMessage::CoreListChanged {
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
                [WorkerCommand::PutBios(
                    bios_row("gone.bin", BiosStatus::Missing),
                    PathBuf::from("/some/mine.bin")
                )]
            );
            assert!(app.prompt().is_none());
        }

        #[test]
        fn shift_i_reports_no_device() {
            let (mut app, rx) = app();
            app.handle_worker_message(WorkerMessage::GamesPageChanged(GamesPageData::default()));
            press(&mut app, "3I");
            assert!(app.prompt().is_none());
            assert_eq!(app.status_message().unwrap().text, "no device is open");
            assert_eq!(sent(&rx), []);
        }

        #[test]
        fn keeps_selection_on_same_file_when_rows_change() {
            let (mut app, _rx) = on_bios_page();
            press(&mut app, "j");
            app.handle_worker_message(WorkerMessage::BiosFilesChanged(vec![
                bios_row("first.bin", BiosStatus::Missing),
                bios_row("there.bin", BiosStatus::Present),
                bios_row("gone.bin", BiosStatus::Missing),
            ]));
            assert_eq!(app.bios.selected_row().unwrap().path, Path::new("gone.bin"));
            app.handle_worker_message(WorkerMessage::BiosFilesChanged(Vec::new()));
            assert_eq!(app.bios.selected_row(), None);
        }
    }

    mod drawing {
        use super::*;

        /// A terminal of this size with one frame of `app` on it.
        fn drawn(app: &mut App, width: u16, height: u16) -> Terminal<TestBackend> {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| ui::draw(frame, app)).unwrap();
            terminal
        }

        /// The screen's top line.
        fn header(app: &mut App) -> String {
            let terminal = drawn(app, 80, 24);
            let buffer = terminal.backend().buffer();
            (0..80).map(|x| buffer[(x, 0)].symbol()).collect()
        }

        #[test]
        fn shows_device_being_read_in_header_not_footer() {
            let (mut app, _rx) = app();
            assert_eq!(app.device_root(), Some(Path::new("/Volumes/POCKET")));
            let root = PathBuf::from("/Volumes/OTHER");
            app.handle_worker_message(WorkerMessage::DeviceReadStarted(root.clone()));
            assert_eq!(app.device_root(), None);
            assert!(app.is_busy());
            assert!(app.command_status().is_none());
            let top = header(&mut app);
            assert!(top.contains("reading OTHER…"), "{top}");
            // In the order the worker sends them.
            app.handle_worker_message(WorkerMessage::GamesPageChanged(GamesPageData {
                rows: rows(),
                device_root: Some(root),
            }));
            app.handle_worker_message(WorkerMessage::DeviceReadFinished);
            assert!(!app.is_busy());
            let top = header(&mut app);
            assert!(top.contains("● OTHER"), "{top}");
        }

        /// Where the header of the table says `PLATFORM`.
        fn platform_column(app: &mut App) -> usize {
            let terminal = drawn(app, 120, 24);
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
            app.handle_worker_message(WorkerMessage::BiosFilesChanged(vec![bios_row(
                "x.bin",
                BiosStatus::Missing,
            )]));
            assert_eq!(platform_column(&mut app), 3);
        }

        #[test]
        fn gives_marked_and_selected_rows_distinct_backgrounds() {
            let (mut app, _rx) = app();
            let background_of_row = |app: &mut App, row: u16| {
                let terminal = drawn(app, 120, 24);
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

        /// The text on the screen's last `count` lines.
        fn last_lines(terminal: &Terminal<TestBackend>, count: u16) -> Vec<String> {
            let buffer = terminal.backend().buffer();
            let height = buffer.area.height;
            ((height - count)..height)
                .map(|y| {
                    (0..buffer.area.width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                        .trim_end()
                        .to_string()
                })
                .collect()
        }

        #[test]
        fn shows_every_key_of_games_page_at_80_columns() {
            let (mut app, _rx) = app();
            let terminal = drawn(&mut app, 80, 24);
            assert_eq!(
                last_lines(&terminal, 3),
                [
                    " LIBRARY  i  import   p  pull   s  save   n  rename   t  standard   x  trash",
                    " DEVICE               P  push   S  save   N  rename   E  eject      X  delete",
                    " GENERAL  ↵  saves    m  mark   d  DATs   r  reload   ?  help       q  quit",
                ]
            );
        }

        #[test]
        fn shows_cover_keys_in_middle_of_panel_without_cover() {
            let (mut app, _rx) = app();
            let terminal = drawn(&mut app, 80, 24);
            let screen = last_lines(&terminal, 24).join("\n");
            assert!(screen.contains("no cover yet"), "{screen}");
            assert!(screen.contains(" a  download it"), "{screen}");
            assert!(screen.contains(" c  use an image file"), "{screen}");
        }

        fn screen_text(app: &mut App) -> String {
            let terminal = drawn(app, 80, 24);
            last_lines(&terminal, 24).join("\n")
        }

        #[test]
        fn shows_device_page_data() {
            let (mut app, _rx) = app();
            press(&mut app, "4");
            app.handle_worker_message(WorkerMessage::DevicePageChanged(Some(DevicePageData {
                root: PathBuf::from("/Volumes/POCKET"),
                firmware: Some(Firmware {
                    version: Some("2.6".to_string()),
                    build_date: Some("2026-06-16".to_string()),
                }),
                disk_space: Some(DiskSpace {
                    total_bytes: 128_000_000_000,
                    available_bytes: 12_300_000_000,
                }),
                core_count: Some(16),
                game_count: 342,
            })));
            let screen = screen_text(&mut app);
            assert!(screen.contains("Root      /Volumes/POCKET"), "{screen}");
            assert!(
                screen.contains("Firmware  2.6 (built 2026-06-16)"),
                "{screen}"
            );
            assert!(
                screen.contains("Space     12.3 GB free of 128.0 GB"),
                "{screen}"
            );
            assert!(screen.contains("Cores     16"), "{screen}");
            assert!(screen.contains("Games     342"), "{screen}");
        }

        #[test]
        fn shows_unknown_for_values_that_cant_be_read() {
            let (mut app, _rx) = app();
            press(&mut app, "4");
            app.handle_worker_message(WorkerMessage::DevicePageChanged(Some(DevicePageData {
                root: PathBuf::from("/Volumes/POCKET"),
                firmware: None,
                disk_space: None,
                core_count: None,
                game_count: 0,
            })));
            let screen = screen_text(&mut app);
            assert!(screen.contains("Firmware  unknown"), "{screen}");
            assert!(screen.contains("Space     unknown"), "{screen}");
            assert!(screen.contains("Cores     unknown"), "{screen}");
        }

        #[test]
        fn shows_no_device_on_device_page() {
            let (mut app, _rx) = app();
            press(&mut app, "4");
            app.handle_worker_message(WorkerMessage::DevicePageChanged(None));
            assert!(screen_text(&mut app).contains("no device"));
        }

        #[test]
        fn shows_search_at_left_and_filters_at_right() {
            let (mut app, _rx) = app();
            let terminal = drawn(&mut app, 80, 24);
            assert_eq!(
                last_lines(&terminal, 23)[0],
                " / search   4                                  f   all   in library   on device"
            );
        }

        /// Each state starts from a new `App`, so none is lost to an earlier
        /// state's keys.
        #[test]
        fn draws_every_state_at_any_size() {
            fn open_import_with_matches(app: &mut App) {
                press(app, "i");
                retype(app, "/");
                let Some(Prompt::Import(import)) = &mut app.prompt else {
                    panic!("no import window")
                };
                let entry = |name: &str, is_dir| path_input::PathEntry {
                    name: name.to_string(),
                    is_dir,
                };
                let entries = vec![entry("roms", true), entry("x.gba", false)];
                import.input.set_entries(PathBuf::from("/"), entries);
            }
            fn open_platform_prompt(app: &mut App) {
                press(app, "i");
                retype(app, "/x.bin\n");
            }
            fn open_cores_page(app: &mut App) {
                app.handle_key(KeyEvent::from(KeyCode::Tab));
                app.handle_worker_message(WorkerMessage::CoreListChanged {
                    rows: vec![
                        core_row("new.Core", None, Some("1")),
                        core_row("old.Core", Some("1"), Some("2")),
                        core_row("mine.Core", Some("1"), None),
                    ],
                    failure: Some("no network".into()),
                });
            }
            fn open_bios_page(app: &mut App) {
                press(app, "3");
                app.handle_worker_message(WorkerMessage::BiosFilesChanged(vec![
                    bios_row("there.bin", BiosStatus::Present),
                    bios_row("short.bin", BiosStatus::WrongSize { size: 1 }),
                    bios_row("gone.bin", BiosStatus::Missing),
                ]));
            }
            let states: [fn(&mut App); 21] = [
                |_| {},
                |app| {
                    let root = PathBuf::from("/Volumes/POCKET");
                    app.handle_worker_message(WorkerMessage::DeviceReadStarted(root));
                },
                |app| press(app, "jm"),
                |app| press(app, "j\n"),
                |app| press(app, "4"),
                |app| {
                    press(app, "4");
                    let data = DevicePageData {
                        root: PathBuf::from("/Volumes/POCKET"),
                        firmware: None,
                        disk_space: None,
                        core_count: None,
                        game_count: 0,
                    };
                    app.handle_worker_message(WorkerMessage::DevicePageChanged(Some(data)));
                },
                |app| press(app, "?"),
                |app| press(app, "jjX"),
                |app| press(app, "/x"),
                |app| press(app, "j\ni"),
                open_import_with_matches,
                |app| {
                    open_import_with_matches(app);
                    app.handle_key(KeyEvent::from(KeyCode::Down));
                },
                open_platform_prompt,
                |app| {
                    open_platform_prompt(app);
                    press(app, "jjjjjjjjj");
                },
                |app| {
                    app.handle_worker_message(WorkerMessage::CommandStarted {
                        label: "pushing".into(),
                        target_count: 3,
                    });
                },
                open_cores_page,
                |app| {
                    open_cores_page(app);
                    press(app, "jX");
                },
                open_bios_page,
                |app| {
                    open_bios_page(app);
                    press(app, "GI");
                },
                |app| press(app, "n"),
                |app| press(app, "jN"),
            ];
            for (width, height) in [(120, 40), (80, 24), (40, 10), (10, 3)] {
                for state in states {
                    let (mut app, _rx) = app();
                    state(&mut app);
                    drawn(&mut app, width, height);
                }
            }
        }
    }
}
