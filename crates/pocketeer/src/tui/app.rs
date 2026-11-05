//! The screen's state, and what each key does to it. Nothing here touches a
//! disk or the network: that is asked of the worker.

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
    path_input::{self, PathInput, PathReader},
    worker::{Command, Row, Snapshot, Update, Worker},
};

/// A command under way.
pub struct Job {
    pub label: String,
    pub done: usize,
    pub total: usize,
    pub game: Option<String>,
    pub cancelling: bool,
}

/// What a button in a dialog says and does.
pub struct Button {
    pub label: &'static str,
    pub command: Option<Command>,
}

impl Button {
    fn new(label: &'static str, command: Option<Command>) -> Self {
        Self { label, command }
    }
}

/// A question or a text over the screen.
pub struct Dialog {
    pub title: String,
    pub lines: Vec<String>,
    /// The last has no command and cancels.
    pub buttons: Vec<Button>,
    pub focus: usize,
    /// Whether what it asks can't be undone.
    pub danger: bool,
}

/// What last happened.
pub struct Message {
    pub text: String,
    /// Whether it went wrong.
    pub failed: bool,
}

/// What a path being typed is to be imported as.
pub enum Importing {
    Game,
    /// A version of this game's save.
    Save(LocalEntry),
    /// The file this row asks for.
    Bios(BiosRow),
}

/// A path being typed, to import what it leads to.
pub struct Import {
    pub importing: Importing,
    pub input: PathInput,
}

/// Which copy of a game is being renamed.
pub enum Renaming {
    InLibrary(LocalEntry),
    OnDevice(DeviceEntry),
}

/// A new name being typed for a game.
pub struct Rename {
    pub renaming: Renaming,
    input: Input,
}

impl Rename {
    /// What is typed before and after the cursor.
    pub fn text_around_cursor(&self) -> (String, String) {
        path_input::text_around_cursor(&self.input)
    }

    /// The name it has, without the extension.
    fn stem(&self) -> String {
        match &self.renaming {
            Renaming::InLibrary(local) => local.stem().to_string(),
            Renaming::OnDevice(on_device) => file_stem(&on_device.path),
        }
    }
}

/// The name of the file at `path`, without the extension.
fn file_stem(path: &Path) -> String {
    let stem = path.file_stem().unwrap_or_default();
    stem.to_string_lossy().into_owned()
}

/// A file to import as a game, while the user says which platform it is for.
pub struct PlatformChoice {
    pub file: PathBuf,
    /// In `Platform::iter()`.
    pub selected: usize,
}

/// Which page is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Games,
    Cores,
    Bios,
}

/// The cores as the worker last told of them.
pub struct CoreList {
    pub rows: Vec<CoreRow>,
    /// Why the inventory couldn't be had, if it couldn't.
    pub failure: Option<String>,
}

/// What the keys go to, when there is no dialog.
#[derive(Clone, Copy)]
enum Mode {
    List,
    /// Typing in the search box.
    Typing,
    /// The saves panel is open, with this version selected.
    Panel(usize),
}

const NOT_ON_BOTH: &str = "nothing here is both in the library and on the device";

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

pub struct App {
    worker: Worker,
    pub covers: Covers,
    paths: PathReader,
    snapshot: Option<Snapshot>,
    /// Indexes into the snapshot's rows of those the search leaves.
    visible: Vec<usize>,
    selected: usize,
    pub table: TableState,
    marks: HashSet<RowKey>,
    page: Page,
    /// None until the worker has told of them.
    cores: Option<CoreList>,
    selected_core: usize,
    pub cores_table: TableState,
    bios: Vec<BiosRow>,
    selected_bios: usize,
    pub bios_table: TableState,
    query: Input,
    mode: Mode,
    dialog: Option<Dialog>,
    /// Shown once `dialog` is closed, so as not to cover it.
    waiting_dialog: Option<Dialog>,
    import: Option<Import>,
    platform_choice: Option<PlatformChoice>,
    rename: Option<Rename>,
    job: Option<Job>,
    /// The device being opened and read.
    reading_device: Option<PathBuf>,
    message: Option<Message>,
    /// Said along with the next summary: what the last key had to leave out.
    skipped: usize,
    quit: bool,
    /// Why the library couldn't be opened, which ends the UI.
    library_failure: Option<String>,
}

impl App {
    pub fn new(worker: Worker, covers: Covers, paths: PathReader) -> Self {
        Self {
            worker,
            covers,
            paths,
            snapshot: None,
            visible: Vec::new(),
            selected: 0,
            table: TableState::default(),
            marks: HashSet::new(),
            page: Page::Games,
            cores: None,
            selected_core: 0,
            cores_table: TableState::default(),
            bios: Vec::new(),
            selected_bios: 0,
            bios_table: TableState::default(),
            query: Input::default(),
            mode: Mode::List,
            dialog: None,
            waiting_dialog: None,
            import: None,
            platform_choice: None,
            rename: None,
            job: None,
            reading_device: None,
            message: None,
            skipped: 0,
            quit: false,
            library_failure: None,
        }
    }

    pub fn should_quit(&self) -> bool {
        self.quit
    }

    /// Why the library couldn't be opened, if that is why it should quit.
    pub fn take_library_failure(&mut self) -> Option<String> {
        self.library_failure.take()
    }

    pub fn is_busy(&self) -> bool {
        self.job.is_some() || self.reading_device.is_some()
    }

    pub fn reading_device(&self) -> Option<&Path> {
        self.reading_device.as_deref()
    }

    pub fn is_loaded(&self) -> bool {
        self.snapshot.is_some()
    }

    pub fn device_root(&self) -> Option<&Path> {
        self.snapshot.as_ref()?.device.as_deref()
    }

    pub fn job(&self) -> Option<&Job> {
        self.job.as_ref()
    }

    pub fn message(&self) -> Option<&Message> {
        self.message.as_ref()
    }

    pub fn dialog(&self) -> Option<&Dialog> {
        self.dialog.as_ref()
    }

    pub fn import(&self) -> Option<&Import> {
        self.import.as_ref()
    }

    pub fn platform_choice(&self) -> Option<&PlatformChoice> {
        self.platform_choice.as_ref()
    }

    pub fn rename(&self) -> Option<&Rename> {
        self.rename.as_ref()
    }

    pub fn page(&self) -> Page {
        self.page
    }

    pub fn cores(&self) -> Option<&CoreList> {
        self.cores.as_ref()
    }

    pub fn selected_core(&self) -> Option<&CoreRow> {
        self.cores.as_ref()?.rows.get(self.selected_core)
    }

    pub fn bios(&self) -> &[BiosRow] {
        &self.bios
    }

    pub fn selected_bios(&self) -> Option<&BiosRow> {
        self.bios.get(self.selected_bios)
    }

    pub fn query(&self) -> &str {
        self.query.value()
    }

    /// What is typed in the search box before and after the cursor.
    pub fn query_around_cursor(&self) -> (String, String) {
        path_input::text_around_cursor(&self.query)
    }

    pub fn is_typing(&self) -> bool {
        matches!(self.mode, Mode::Typing)
    }

    /// The version selected in the saves panel, if it is open.
    pub fn selected_version(&self) -> Option<usize> {
        match self.mode {
            Mode::Panel(selected) => Some(selected),
            Mode::List | Mode::Typing => None,
        }
    }

    /// How many rows are marked, seen or not.
    pub fn marked_count(&self) -> usize {
        self.rows().iter().filter(|r| self.is_marked(r)).count()
    }

    pub fn is_marked(&self, row: &Row) -> bool {
        self.marks.contains(&key_of(row))
    }

    /// How many games there are, and how many orphan saves.
    pub fn game_and_orphan_counts(&self) -> (usize, usize) {
        let is_game = |row: &&Row| matches!(row, Row::Game { .. });
        let games = self.rows().iter().filter(is_game).count();
        (games, self.rows().len() - games)
    }

    pub fn visible_rows(&self) -> impl Iterator<Item = &Row> {
        self.visible.iter().map(|&i| &self.rows()[i])
    }

    pub fn visible_len(&self) -> usize {
        self.visible.len()
    }

    pub fn selected_index(&self) -> Option<usize> {
        (!self.visible.is_empty()).then_some(self.selected)
    }

    pub fn selected_row(&self) -> Option<&Row> {
        self.visible.get(self.selected).map(|&i| &self.rows()[i])
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
        let at = |i: usize| {
            let &row = self.visible.get(i)?;
            self.rows()[row].local_entry()?.cover_path.clone()
        };
        let before = self.selected.checked_sub(1).and_then(at);
        [at(self.selected + 1), before]
            .into_iter()
            .flatten()
            .collect()
    }

    pub fn on_cover_ready(&mut self) {
        self.covers.collect_finished();
    }

    pub fn on_folder_read(&mut self) {
        for (folder, entries) in self.paths.folders_read() {
            if let Some(import) = &mut self.import {
                import.input.set_entries(folder, entries);
            }
        }
    }

    /// Every row, whatever the search leaves.
    pub fn rows(&self) -> &[Row] {
        self.snapshot.as_ref().map_or(&[], |s| &s.rows)
    }

    pub fn update(&mut self, update: Update) {
        match update {
            Update::Snapshot(snapshot) => {
                let keep = self.selected_row().map(key_of);
                self.snapshot = Some(snapshot);
                self.search_again(keep);
            }
            Update::Cores { rows, failure } => {
                let keep = self.selected_core().map(|r| r.id().to_string());
                let kept = keep.and_then(|id| rows.iter().position(|r| r.id() == id));
                self.selected_core = kept.unwrap_or(self.selected_core);
                self.cores = Some(CoreList { rows, failure });
                self.select_core(self.selected_core);
            }
            Update::Bios(rows) => {
                let same = |a: &BiosRow, b: &BiosRow| a.platform == b.platform && a.path == b.path;
                let keep = self.selected_bios().cloned();
                let kept = keep.and_then(|k| rows.iter().position(|r| same(r, &k)));
                self.selected_bios = kept.unwrap_or(self.selected_bios);
                self.bios = rows;
                self.select_bios(self.selected_bios);
            }
            Update::ReadingDevice(root) => self.reading_device = Some(root),
            Update::DeviceRead => self.reading_device = None,
            Update::Started { label, total } => {
                self.job = Some(Job {
                    label,
                    done: 0,
                    total,
                    game: None,
                    cancelling: false,
                });
            }
            Update::Progress { done, total, game } => {
                if let Some(job) = &mut self.job {
                    (job.done, job.total, job.game) = (done, total, Some(game));
                }
            }
            Update::Finished { summary, failed } => {
                self.job = None;
                let summary = match std::mem::take(&mut self.skipped) {
                    0 => summary,
                    n => format!("{summary} ({n} left out)"),
                };
                self.message = Some(Message {
                    text: summary,
                    failed,
                });
            }
            Update::LibraryFailed(reason) => {
                self.library_failure = Some(reason);
                self.quit = true;
            }
            Update::Duplicates(lines) => {
                let dialog = duplicates_dialog(lines);
                if self.dialog.is_some() {
                    self.waiting_dialog = Some(dialog);
                } else {
                    self.dialog = Some(dialog);
                }
            }
        }
    }

    /// Works out the visible rows, keeping the selection on the row `keep`.
    fn search_again(&mut self, keep: Option<RowKey>) {
        let found = self
            .rows()
            .iter()
            .enumerate()
            .filter(|(_, r)| matches_query(r, self.query.value()));
        let visible: Vec<usize> = found.map(|(i, _)| i).collect();
        let kept = keep.and_then(|k| visible.iter().position(|&i| key_of(&self.rows()[i]) == k));
        self.selected = kept.unwrap_or(self.selected.min(visible.len().saturating_sub(1)));
        self.visible = visible;
        self.table.select(self.selected_index());
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        // Before the keys below stand for others: here they edit the text.
        if self.dialog.is_none() && self.import.is_some() {
            self.handle_import_key(key);
            return;
        }
        if self.dialog.is_none() && self.rename.is_some() {
            self.handle_rename_key(key);
            return;
        }
        let key = translate_emacs_key(key);
        if self.dialog.is_none() && self.platform_choice.is_some() {
            self.handle_platform_choice_key(key);
            return;
        }
        if self.dialog.is_some() {
            self.handle_dialog_key(key);
            if self.dialog.is_none() {
                self.dialog = self.waiting_dialog.take();
            }
            return;
        }
        match self.page {
            Page::Games => {}
            Page::Cores => return self.handle_cores_key(key),
            Page::Bios => return self.handle_bios_key(key),
        }
        match self.mode {
            Mode::List => self.handle_list_key(key),
            Mode::Typing => self.handle_search_key(key),
            Mode::Panel(selected) => self.handle_panel_key(key, selected),
        }
    }

    fn handle_dialog_key(&mut self, key: KeyEvent) {
        let Some(dialog) = &mut self.dialog else {
            return;
        };
        let last = dialog.buttons.len().saturating_sub(1);
        match key.code {
            KeyCode::Left | KeyCode::Char('h') => dialog.focus = dialog.focus.saturating_sub(1),
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Tab => {
                dialog.focus = (dialog.focus + 1).min(last);
            }
            KeyCode::Enter | KeyCode::Char('y') => {
                let command = dialog
                    .buttons
                    .get_mut(dialog.focus)
                    .and_then(|b| b.command.take());
                self.dialog = None;
                match command {
                    Some(command) => {
                        self.marks.clear();
                        self.worker.send(command);
                    }
                    None => self.skipped = 0,
                }
            }
            KeyCode::Esc | KeyCode::Char('n' | 'q' | '?') => {
                self.dialog = None;
                self.skipped = 0;
            }
            _ => {}
        }
    }

    fn handle_import_key(&mut self, key: KeyEvent) {
        let Some(import) = &mut self.import else {
            return;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let cancelling = key.code == KeyCode::Esc || (ctrl && key.code == KeyCode::Char('g'));
        if cancelling {
            if !import.input.unpick() {
                self.import = None;
            }
            return;
        }
        let takes_folder = matches!(import.importing, Importing::Game);
        if key.code != KeyCode::Enter {
            import.input.handle_key(key);
        } else if !import.input.fill_in_picked() && (takes_folder || !import.input.is_folder()) {
            self.import_typed_path();
            return;
        }
        self.read_typed_folder();
    }

    /// Imports what the path typed leads to, or else, for a game whose
    /// extension doesn't tell its platform, asks which it is for.
    fn import_typed_path(&mut self) {
        let Some(import) = self.import.take() else {
            return;
        };
        let path = import.input.path();
        match import.importing {
            Importing::Save(local) => self.worker.send(Command::ImportSave(local, path)),
            Importing::Bios(row) => self.worker.send(Command::PutBios(row, path)),
            Importing::Game => {
                let extension = path.extension().and_then(|e| e.to_str());
                let tells_platform = extension.and_then(Platform::from_extension).is_some();
                if tells_platform || import.input.names_folder() {
                    self.worker.send(Command::ImportGame(path, None));
                } else {
                    self.platform_choice = Some(PlatformChoice {
                        file: path,
                        selected: 0,
                    });
                }
            }
        }
    }

    fn handle_rename_key(&mut self, key: KeyEvent) {
        let Some(rename) = &mut self.rename else {
            return;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if key.code == KeyCode::Esc || (ctrl && key.code == KeyCode::Char('g')) {
            self.rename = None;
        } else if key.code != KeyCode::Enter {
            path_input::edit(&mut rename.input, key);
        } else if let Some(rename) = self.rename.take() {
            let new = rename.input.value().trim().to_string();
            if new != rename.stem() {
                self.worker.send(match rename.renaming {
                    Renaming::InLibrary(local) => Command::RenameInLibrary(local, new),
                    Renaming::OnDevice(on_device) => Command::RenameOnDevice(on_device, new),
                });
            }
        }
    }

    /// Asks what to call the selected row's copy in the library.
    fn ask_to_rename_in_library(&mut self) {
        let Some(local) = self.selected_row().and_then(Row::local_entry).cloned() else {
            self.say("nothing here is in the library");
            return;
        };
        self.start_rename(Renaming::InLibrary(local));
    }

    /// Asks what to call the selected row's copy on the device.
    fn ask_to_rename_on_device(&mut self) {
        let Some(on_device) = self.selected_row().and_then(Row::device_entry).cloned() else {
            self.say("this game isn't on the device");
            return;
        };
        self.start_rename(Renaming::OnDevice(on_device));
    }

    fn start_rename(&mut self, renaming: Renaming) {
        let mut rename = Rename {
            renaming,
            input: Input::default(),
        };
        rename.input = Input::new(rename.stem());
        self.rename = Some(rename);
    }

    fn handle_platform_choice_key(&mut self, key: KeyEvent) {
        let Some(choice) = &mut self.platform_choice else {
            return;
        };
        let last = Platform::iter().count().saturating_sub(1);
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => choice.selected = (choice.selected + 1).min(last),
            KeyCode::Up | KeyCode::Char('k') => choice.selected = choice.selected.saturating_sub(1),
            KeyCode::Enter => {
                let platform = Platform::iter().nth(choice.selected);
                let file = choice.file.clone();
                self.platform_choice = None;
                self.worker.send(Command::ImportGame(file, platform));
            }
            KeyCode::Esc => self.platform_choice = None,
            _ => {}
        }
    }

    /// Asks for the folder of the path being typed, if it has changed.
    fn read_typed_folder(&mut self) {
        let import = self.import.as_mut();
        if let Some(folder) = import.and_then(|i| i.input.folder_to_read()) {
            self.paths.request_folder(folder);
        }
    }

    fn ask_for_a_save_to_import(&mut self) {
        let Some(local) = self.selected_row().and_then(Row::local_entry).cloned() else {
            self.say("nothing here is in the library");
            return;
        };
        self.ask_for_a_path(Importing::Save(local));
    }

    fn ask_for_a_path(&mut self, importing: Importing) {
        self.import = Some(Import {
            importing,
            input: PathInput::new("~/"),
        });
        self.read_typed_folder();
    }

    fn handle_search_key(&mut self, key: KeyEvent) {
        let keep = self.selected_row().map(key_of);
        match key.code {
            KeyCode::Enter => self.mode = Mode::List,
            KeyCode::Esc => {
                self.mode = Mode::List;
                self.query.reset();
            }
            KeyCode::Down | KeyCode::Up | KeyCode::PageDown | KeyCode::PageUp => {
                self.mode = Mode::List;
                self.handle_list_key(key);
                return;
            }
            _ => {
                path_input::edit(&mut self.query, key);
            }
        }
        self.search_again(keep);
    }

    fn handle_list_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.move_selection_by(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection_by(-1),
            KeyCode::PageDown => self.move_selection_by(PAGE),
            KeyCode::PageUp => self.move_selection_by(-PAGE),
            KeyCode::Char('g') | KeyCode::Home => self.select_row(0),
            KeyCode::Char('G') | KeyCode::End => self.select_row(usize::MAX),
            KeyCode::Char('m') => self.set_mark_and_move_on(true),
            KeyCode::Char('u') => self.set_mark_and_move_on(false),
            KeyCode::Char('M') => {
                let keys: Vec<RowKey> = self.visible_rows().map(key_of).collect();
                self.marks.extend(keys);
            }
            KeyCode::Char('U') => self.marks.clear(),
            KeyCode::Char('i') => self.send_for_device_rows(Command::PullGames),
            KeyCode::Char('b') => self.send_for_device_rows(Command::PullSaves),
            KeyCode::Char('I') => self.ask_for_a_path(Importing::Game),
            KeyCode::Char('n') => self.ask_to_rename_in_library(),
            KeyCode::Char('N') => self.ask_to_rename_on_device(),
            KeyCode::Char('s') => self.send_for_library_rows(Command::PushGames),
            KeyCode::Char('f') => self.send_for_library_rows(Command::FetchCovers),
            KeyCode::Char('p') => self.push_saves(),
            KeyCode::Char('x') => self.ask_to_trash_games(),
            KeyCode::Char('X') => self.ask_to_delete(),
            KeyCode::Char('d') => self.worker.send(Command::SyncDats),
            KeyCode::Char('E') => self.eject(),
            KeyCode::Char('r') => self.worker.send(Command::Reload),
            KeyCode::Enter | KeyCode::Right => self.open_panel(),
            KeyCode::Tab | KeyCode::Char('2') => self.show_cores(),
            KeyCode::Char('3') => self.page = Page::Bios,
            KeyCode::Char('/') => self.mode = Mode::Typing,
            KeyCode::Char('?') => self.dialog = Some(help_dialog()),
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc => self.handle_escape(),
            _ => {}
        }
    }

    fn handle_panel_key(&mut self, key: KeyEvent, selected: usize) {
        let len = self.selected_save_versions().len();
        let selected = selected.min(len.saturating_sub(1));
        match key.code {
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') => self.mode = Mode::List,
            KeyCode::Down | KeyCode::Char('j') => {
                self.mode = Mode::Panel((selected + 1).min(len.saturating_sub(1)));
            }
            KeyCode::Up | KeyCode::Char('k') => self.mode = Mode::Panel(selected.saturating_sub(1)),
            KeyCode::Char('b') => self.send_for_device_rows(Command::PullSaves),
            KeyCode::Char('p') => self.push_version(selected),
            KeyCode::Char('I') => self.ask_for_a_save_to_import(),
            KeyCode::Char('x') => self.ask_to_trash_version(selected),
            KeyCode::Char('X') => self.ask_to_delete_save(),
            KeyCode::Char('?') => self.dialog = Some(help_dialog()),
            KeyCode::Char('q') => self.quit = true,
            _ => {}
        }
    }

    /// Opens the Cores page, asking for them unless they have been told of.
    fn show_cores(&mut self) {
        self.page = Page::Cores;
        if self.cores.is_none() {
            self.worker.send(Command::ShowCores);
        }
    }

    fn handle_cores_key(&mut self, key: KeyEvent) {
        if let Some(i) = row_after_key(key, self.selected_core) {
            return self.select_core(i);
        }
        match key.code {
            KeyCode::Char('1') => self.page = Page::Games,
            KeyCode::Tab | KeyCode::Char('3') => self.page = Page::Bios,
            KeyCode::Char('u') => self.install_selected_core(),
            KeyCode::Char('U') => self.update_every_core(),
            KeyCode::Char('x') => self.ask_to_remove_core(),
            KeyCode::Char('r') => self.worker.send(Command::Reload),
            KeyCode::Char('?') => self.dialog = Some(help_dialog()),
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc => self.handle_escape(),
            _ => {}
        }
    }

    /// Selects core `i`, or the last if there are fewer.
    fn select_core(&mut self, i: usize) {
        let len = self.cores.as_ref().map_or(0, |c| c.rows.len());
        self.selected_core = i.min(len.saturating_sub(1));
        self.cores_table
            .select((len > 0).then_some(self.selected_core));
    }

    fn install_selected_core(&mut self) {
        let Some(row) = self.selected_core() else {
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
            Some(why) => self.say(why),
            None => self.worker.send(Command::InstallCores(vec![id])),
        }
    }

    fn update_every_core(&mut self) {
        let rows = self.cores.as_ref().map_or(&[][..], |c| &c.rows);
        let with_updates = rows.iter().filter(|r| r.has_update());
        let ids: Vec<String> = with_updates.map(|r| r.id().to_string()).collect();
        if ids.is_empty() {
            self.say("every core is up to date");
        } else {
            self.worker.send(Command::InstallCores(ids));
        }
    }

    fn ask_to_remove_core(&mut self) {
        let Some(row) = self.selected_core().filter(|r| r.installed().is_some()) else {
            self.say("this core isn't on the device");
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
                Button::new("Delete", Some(Command::RemoveCores(vec![id]))),
                Button::new("Cancel", None),
            ],
            focus: 1,
            danger: true,
        });
    }

    fn handle_bios_key(&mut self, key: KeyEvent) {
        if let Some(i) = row_after_key(key, self.selected_bios) {
            return self.select_bios(i);
        }
        match key.code {
            KeyCode::Tab | KeyCode::Char('1') => self.page = Page::Games,
            KeyCode::Char('2') => self.show_cores(),
            KeyCode::Char('I') => self.ask_for_a_bios_file(),
            KeyCode::Char('r') => self.worker.send(Command::Reload),
            KeyCode::Char('?') => self.dialog = Some(help_dialog()),
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc => self.handle_escape(),
            _ => {}
        }
    }

    /// Selects file `i`, or the last if there are fewer.
    fn select_bios(&mut self, i: usize) {
        self.selected_bios = i.min(self.bios.len().saturating_sub(1));
        self.bios_table
            .select((!self.bios.is_empty()).then_some(self.selected_bios));
    }

    fn ask_for_a_bios_file(&mut self) {
        match self.selected_bios().cloned() {
            None if self.device_root().is_none() => self.say("no device is open"),
            None => self.say("no core on the device needs a file"),
            Some(row) if row.status != BiosStatus::Missing => {
                self.say("the device has this file already");
            }
            Some(row) => self.ask_for_a_path(Importing::Bios(row)),
        }
    }

    /// Closes what is open, or else stops what is running.
    fn handle_escape(&mut self) {
        if !self.query.value().is_empty() {
            let keep = self.selected_row().map(key_of);
            self.query.reset();
            self.search_again(keep);
        } else if let Some(job) = &mut self.job {
            job.cancelling = true;
            self.worker.cancel();
        }
    }

    fn move_selection_by(&mut self, by: isize) {
        self.select_row(self.selected.saturating_add_signed(by));
    }

    /// Selects row `i`, or the last if there are fewer.
    fn select_row(&mut self, i: usize) {
        self.selected = i.min(self.visible.len().saturating_sub(1));
        self.table.select(self.selected_index());
    }

    /// Marks the row, or not, and moves on to the next.
    fn set_mark_and_move_on(&mut self, mark: bool) {
        let Some(key) = self.selected_row().map(key_of) else {
            return;
        };
        if mark {
            self.marks.insert(key);
        } else {
            self.marks.remove(&key);
        }
        self.move_selection_by(1);
    }

    /// What a key acts on: the rows marked, seen or not, or else the one
    /// selected. In the saves panel, always the one selected.
    fn target_rows(&self) -> Vec<&Row> {
        let marked: Vec<&Row> = self.rows().iter().filter(|r| self.is_marked(r)).collect();
        if marked.is_empty() || self.selected_version().is_some() {
            self.selected_row().into_iter().collect()
        } else {
            marked
        }
    }

    /// The copies on the device among the targets, and how many had none.
    fn device_targets(&self) -> (Vec<DeviceEntry>, usize) {
        let targets = self.target_rows();
        let entries: Vec<DeviceEntry> = targets
            .iter()
            .filter_map(|r| r.device_entry().cloned())
            .collect();
        let left = targets.len() - entries.len();
        (entries, left)
    }

    /// The games in the library among the targets, and how many had none.
    fn library_targets(&self) -> (Vec<LocalEntry>, usize) {
        let targets = self.target_rows();
        let entries: Vec<LocalEntry> = targets
            .iter()
            .filter_map(|r| r.local_entry().cloned())
            .collect();
        let left = targets.len() - entries.len();
        (entries, left)
    }

    fn send_for_device_rows(&mut self, command: fn(Vec<DeviceEntry>) -> Command) {
        let (entries, left) = self.device_targets();
        self.send_for_entries(entries, left, command, "nothing here is on the device");
    }

    fn send_for_library_rows(&mut self, command: fn(Vec<LocalEntry>) -> Command) {
        let (entries, left) = self.library_targets();
        self.send_for_entries(entries, left, command, "nothing here is in the library");
    }

    fn send_for_entries<T>(
        &mut self,
        entries: Vec<T>,
        left: usize,
        command: impl FnOnce(Vec<T>) -> Command,
        none: &str,
    ) {
        if entries.is_empty() {
            self.say(none);
            return;
        }
        self.skipped += left;
        self.marks.clear();
        self.worker.send(command(entries));
    }

    /// The newest versions, onto the copies among the targets with their
    /// game in the library.
    fn push_saves(&mut self) {
        let targets = self.target_rows();
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
        let left = targets.len() - entries.len();
        self.send_for_entries(entries, left, Command::PushSaves, NOT_ON_BOTH);
    }

    /// The version selected in the saves panel, onto the selected row's copy.
    fn push_version(&mut self, selected: usize) {
        let version = self.selected_save_versions().get(selected).cloned();
        let on_device = self.selected_row().and_then(Row::device_entry).cloned();
        match (on_device, version) {
            (Some(on_device), Some(version)) => {
                self.worker
                    .send(Command::PushSaveVersion(on_device, version));
            }
            _ => self.say(NOT_ON_BOTH),
        }
    }

    /// Tells why a key did nothing.
    fn say(&mut self, text: &str) {
        self.message = Some(Message {
            text: text.to_string(),
            failed: false,
        });
    }

    fn eject(&mut self) {
        match self.device_root() {
            Some(_) => self.worker.send(Command::Eject),
            None => self.say("no device is open"),
        }
    }

    fn open_panel(&mut self) {
        if let Some(Row::Game { .. }) = self.selected_row() {
            self.mode = Mode::Panel(0);
        }
    }

    fn ask_to_trash_games(&mut self) {
        let (entries, left) = self.library_targets();
        if entries.is_empty() {
            self.ask_to_trash_orphans();
            return;
        }
        let rows = self.target_rows();
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
        self.skipped += left;
        self.dialog = Some(Dialog {
            title: format!("Trash {}", pluralized(entries.len(), "game")),
            lines,
            buttons: vec![
                Button::new("Trash", Some(Command::TrashGames(entries))),
                Button::new("Cancel", None),
            ],
            focus: 0,
            danger: false,
        });
    }

    fn ask_to_trash_orphans(&mut self) {
        let targets = self.target_rows();
        let orphans = targets.iter().filter_map(|r| r.orphan_in_library());
        let orphans: Vec<OrphanLocalSave> = orphans.cloned().collect();
        if orphans.is_empty() {
            self.say("nothing here is in the library");
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
                Button::new("Trash", Some(Command::TrashOrphanSaves(orphans))),
                Button::new("Cancel", None),
            ],
            focus: 1,
            danger: false,
        });
    }

    fn ask_to_trash_version(&mut self, selected: usize) {
        let Some(local) = self.selected_row().and_then(Row::local_entry).cloned() else {
            return;
        };
        let Some(version) = self.selected_save_versions().get(selected).cloned() else {
            return;
        };
        self.dialog = Some(Dialog {
            title: "Trash a save".to_string(),
            lines: vec![format!(
                "Version {} goes to the system's trash.",
                version.timestamp()
            )],
            buttons: vec![
                Button::new("Trash", Some(Command::TrashSaveVersion(local, version))),
                Button::new("Cancel", None),
            ],
            focus: 0,
            danger: false,
        });
    }

    fn ask_to_delete(&mut self) {
        let (entries, left) = self.device_targets();
        if entries.is_empty() {
            self.ask_to_delete_orphans();
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
        let only_here = self.target_rows().iter().any(|r| {
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
        self.skipped += left;
        self.dialog = Some(Dialog {
            title: format!(
                "Delete {} from the device",
                pluralized(entries.len(), "game")
            ),
            lines,
            buttons: vec![
                Button::new("Delete", Some(Command::DeleteGames(entries))),
                Button::new("Cancel", None),
            ],
            focus: 0,
            danger: true,
        });
    }

    fn ask_to_delete_orphans(&mut self) {
        let targets = self.target_rows();
        let orphans = targets.iter().filter_map(|r| r.orphan_on_device());
        let orphans: Vec<OrphanDeviceSave> = orphans.cloned().collect();
        if orphans.is_empty() {
            self.say("nothing here is on the device");
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
                Button::new("Delete", Some(Command::DeleteOrphanSaves(orphans))),
                Button::new("Cancel", None),
            ],
            focus: 1,
            danger: true,
        });
    }

    fn ask_to_delete_save(&mut self) {
        let (entries, _) = self.device_targets();
        if entries.is_empty() {
            self.say("nothing here is on the device");
            return;
        }
        let mut lines = vec![
            "This deletes the save from the device for good: there is no trash there.".to_string(),
        ];
        let unsaved = self
            .target_rows()
            .iter()
            .any(|r| matches!(r, Row::Game { row, .. } if row.has_new_save_on_device()));
        if unsaved {
            lines.push("The library doesn't have this save.".to_string());
        }
        self.dialog = Some(Dialog {
            title: "Delete a save from the device".to_string(),
            lines,
            buttons: vec![
                Button::new("Delete", Some(Command::DeleteSaves(entries))),
                Button::new("Cancel", None),
            ],
            focus: 1,
            danger: true,
        });
    }
}

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
        buttons: vec![Button::new("Close", None)],
        focus: 0,
        danger: false,
    }
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
        buttons: vec![Button::new("Close", None)],
        focus: 0,
        danger: false,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::{Path, PathBuf},
        sync::mpsc::Receiver,
        time::{Duration, UNIX_EPOCH},
    };

    use pocketeer_core::{
        cores::{BiosRow, BiosStatus, InstalledCore, InventoryCore},
        device::OrphanDeviceSave,
        game::{Files, Game, Rom, SaveHistory},
        platform::Platform,
    };
    use ratatui::{Terminal, backend::TestBackend, style::Color};
    use ratatui_image::picker::Picker;

    use super::*;
    use crate::tui::{ui, worker::Sent};

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
        let paths = PathReader::spawn(|| {});
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

    #[test]
    fn a_library_that_cannot_be_opened_ends_the_ui_with_the_reason() {
        let (mut app, _rx) = app();
        app.update(Update::LibraryFailed("no library at x".to_string()));
        assert!(app.should_quit());
        assert_eq!(app.take_library_failure().unwrap(), "no library at x");
    }

    #[test]
    fn a_key_for_nothing_says_so_and_sends_nothing() {
        let (mut app, rx) = app();
        press(&mut app, "i");
        assert!(sent(&rx).is_empty());
        assert_eq!(app.message().unwrap().text, "nothing here is on the device");
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
            app.on_folder_read();
            if !app.import().unwrap().input.matches().is_empty() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("the folder wasn't read");
    }

    #[test]
    fn the_saves_panel_acts_on_its_version_of_its_row_only() {
        let (mut app, rx) = app();
        // A mark elsewhere doesn't count in the panel.
        press(&mut app, "mk");
        press(&mut app, "j\n");
        assert_eq!(app.selected_save_versions().len(), 2);
        press(&mut app, "jp");
        let [Command::PushSaveVersion(game, version)] = &sent(&rx)[..] else {
            panic!()
        };
        assert_eq!(game.path, Path::new("Both.gba"));
        assert_eq!(version, &app.selected_save_versions()[1]);
        press(&mut app, "x");
        press(&mut app, "y");
        let [Command::TrashSaveVersion(game, version)] = &sent(&rx)[..] else {
            panic!()
        };
        assert_eq!(game.name, "Both.gba");
        assert_eq!(version, &app.selected_save_versions()[1]);
        press(&mut app, "\x1b");
        assert_eq!(app.selected_version(), None);
    }

    #[test]
    fn escape_stops_what_is_running_and_says_so() {
        let (mut app, _rx) = app();
        app.update(Update::Started {
            label: "pushing games".into(),
            total: 2,
        });
        press(&mut app, "\x1b");
        assert!(app.job().unwrap().cancelling);
        app.update(Update::Finished {
            summary: "stopped".into(),
            failed: false,
        });
        assert!(!app.is_busy());
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

    mod renaming {
        use super::*;

        #[test]
        fn n_starts_from_its_name_without_the_extension_and_sends_the_new_one() {
            let (mut app, rx) = app();
            press(&mut app, "n");
            let (before, after) = app.rename().unwrap().text_around_cursor();
            assert_eq!((before.as_str(), after.as_str()), ("Alpha", ""));
            // Keys are text here, whatever they do elsewhere.
            press(&mut app, "qx\n");
            let [Command::RenameInLibrary(game, new)] = &sent(&rx)[..] else {
                panic!()
            };
            assert_eq!((game.name.as_str(), new.as_str()), ("Alpha.gba", "Alphaqx"));
            assert!(app.rename().is_none());
        }

        #[test]
        fn big_n_renames_the_copy_on_the_device() {
            let (mut app, rx) = app();
            press(&mut app, "jN!\n");
            let [Command::RenameOnDevice(game, new)] = &sent(&rx)[..] else {
                panic!()
            };
            assert_eq!(game.path, Path::new("Both.gba"));
            assert_eq!(new, "Both!");
        }

        #[test]
        fn nothing_is_sent_for_the_same_name_or_when_cancelled() {
            let (mut app, rx) = app();
            press(&mut app, "n\n");
            assert!(app.rename().is_none());
            press(&mut app, "nabc\x1b");
            assert!(app.rename().is_none());
            assert_eq!(sent(&rx), []);
        }

        #[test]
        fn a_row_with_no_such_copy_says_so() {
            let (mut app, rx) = app();
            press(&mut app, "N");
            assert_eq!(app.message().unwrap().text, "this game isn't on the device");
            press(&mut app, "jjn");
            assert_eq!(
                app.message().unwrap().text,
                "nothing here is in the library"
            );
            assert!(app.rename().is_none());
            assert_eq!(sent(&rx), []);
        }

        #[test]
        fn the_selection_and_the_mark_stay_with_the_game_renamed() {
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
            let selected = app.selected_row().unwrap().local_entry().unwrap();
            assert_eq!(selected.name, "Aaa.gba");
            assert_eq!(app.marked_count(), 1);
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

    fn bios_row(name: &str, status: BiosStatus) -> BiosRow {
        BiosRow {
            platform: Platform::Gba,
            path: PathBuf::from(name),
            needed_by: vec![("a.GBA".to_string(), true)],
            size_exact: Some(4),
            status,
        }
    }

    mod bios {
        use super::*;

        #[test]
        fn tab_goes_through_the_pages_and_numbers_go_straight_there() {
            let (mut app, _rx) = app();
            app.handle_key(KeyEvent::from(KeyCode::Tab));
            assert_eq!(app.page(), Page::Cores);
            app.handle_key(KeyEvent::from(KeyCode::Tab));
            assert_eq!(app.page(), Page::Bios);
            app.handle_key(KeyEvent::from(KeyCode::Tab));
            assert_eq!(app.page(), Page::Games);
            press(&mut app, "3");
            assert_eq!(app.page(), Page::Bios);
            press(&mut app, "2");
            assert_eq!(app.page(), Page::Cores);
            press(&mut app, "3");
            press(&mut app, "1");
            assert_eq!(app.page(), Page::Games);
        }

        #[test]
        fn big_i_asks_for_a_file_and_sends_it_with_the_row() {
            let (mut app, rx) = on_bios_page();
            press(&mut app, "jI");
            assert!(app.import().is_some());
            retype(&mut app, "/some/mine.bin\n");
            assert_eq!(
                sent(&rx),
                [Command::PutBios(
                    bios_row("gone.bin", BiosStatus::Missing),
                    PathBuf::from("/some/mine.bin")
                )]
            );
            assert!(app.import().is_none());
        }

        #[test]
        fn a_file_the_device_has_is_not_asked_for() {
            let (mut app, rx) = on_bios_page();
            press(&mut app, "I");
            assert!(app.import().is_none());
            assert_eq!(
                app.message().unwrap().text,
                "the device has this file already"
            );
            assert_eq!(sent(&rx), []);
        }

        #[test]
        fn with_nothing_needed_nothing_is_asked_for() {
            let (mut app, rx) = app();
            press(&mut app, "3I");
            assert!(app.import().is_none());
            assert_eq!(
                app.message().unwrap().text,
                "no core on the device needs a file"
            );
            assert_eq!(sent(&rx), []);
        }

        #[test]
        fn with_no_device_nothing_is_asked_for() {
            let (mut app, rx) = app();
            app.update(Update::Snapshot(Snapshot::default()));
            press(&mut app, "3I");
            assert!(app.import().is_none());
            assert_eq!(app.message().unwrap().text, "no device is open");
            assert_eq!(sent(&rx), []);
        }

        #[test]
        fn the_selection_stays_with_its_file() {
            let (mut app, _rx) = on_bios_page();
            press(&mut app, "j");
            app.update(Update::Bios(vec![
                bios_row("first.bin", BiosStatus::Missing),
                bios_row("there.bin", BiosStatus::Present),
                bios_row("gone.bin", BiosStatus::Missing),
            ]));
            assert_eq!(app.selected_bios().unwrap().path, Path::new("gone.bin"));
            app.update(Update::Bios(Vec::new()));
            assert_eq!(app.selected_bios(), None);
        }
    }

    mod cores {
        use super::*;

        fn ids(ids: &[&str]) -> Vec<String> {
            ids.iter().map(ToString::to_string).collect()
        }

        #[test]
        fn the_cores_are_asked_for_once() {
            let (mut app, rx) = on_cores_page();
            press(&mut app, "1");
            assert_eq!(app.page(), Page::Games);
            press(&mut app, "2");
            assert_eq!(app.page(), Page::Cores);
            assert_eq!(sent(&rx), []);
        }

        #[test]
        fn pages_do_not_change_while_typing_a_search() {
            let (mut app, rx) = app();
            press(&mut app, "/2");
            app.handle_key(KeyEvent::from(KeyCode::Tab));
            assert_eq!(app.page(), Page::Games);
            assert_eq!(app.query(), "2");
            assert_eq!(sent(&rx), []);
        }

        #[test]
        fn u_installs_a_core_not_on_the_device_or_updates_one() {
            let (mut app, rx) = on_cores_page();
            press(&mut app, "u");
            assert_eq!(sent(&rx), [Command::InstallCores(ids(&["new.Core"]))]);
            press(&mut app, "ju");
            assert_eq!(sent(&rx), [Command::InstallCores(ids(&["old.Core"]))]);
        }

        #[test]
        fn u_says_why_when_there_is_nothing_to_install() {
            let (mut app, rx) = on_cores_page();
            press(&mut app, "jju");
            assert_eq!(
                app.message().unwrap().text,
                "the device has the newest version"
            );
            press(&mut app, "ju");
            assert_eq!(
                app.message().unwrap().text,
                "only the cores with a latest version can be installed"
            );
            assert_eq!(sent(&rx), []);
        }

        #[test]
        fn without_a_device_nothing_is_installed() {
            let (mut app, rx) = on_cores_page();
            app.update(Update::Snapshot(Snapshot {
                rows: Vec::new(),
                device: None,
            }));
            press(&mut app, "u");
            assert_eq!(app.message().unwrap().text, "no device is open");
            assert_eq!(sent(&rx), []);
        }

        #[test]
        fn big_u_updates_those_with_a_newer_release_or_says_there_are_none() {
            let (mut app, rx) = on_cores_page();
            press(&mut app, "U");
            assert_eq!(sent(&rx), [Command::InstallCores(ids(&["old.Core"]))]);
            app.update(Update::Cores {
                rows: vec![core_row("same.Core", Some("2"), Some("2"))],
                failure: None,
            });
            press(&mut app, "U");
            assert_eq!(app.message().unwrap().text, "every core is up to date");
            assert_eq!(sent(&rx), []);
        }

        #[test]
        fn removing_asks_first_and_cancels_by_default() {
            let (mut app, rx) = on_cores_page();
            press(&mut app, "jx");
            let dialog = app.dialog().unwrap();
            assert!(dialog.danger);
            assert_eq!(dialog.focus, 1);
            press(&mut app, "\n");
            assert_eq!(sent(&rx), []);
            press(&mut app, "xh\n");
            assert_eq!(sent(&rx), [Command::RemoveCores(ids(&["old.Core"]))]);
        }

        #[test]
        fn a_core_not_on_the_device_is_not_removed() {
            let (mut app, rx) = on_cores_page();
            press(&mut app, "x");
            assert!(app.dialog().is_none());
            assert_eq!(app.message().unwrap().text, "this core isn't on the device");
            assert_eq!(sent(&rx), []);
        }

        #[test]
        fn the_selection_stays_on_its_core_as_the_list_changes() {
            let (mut app, _rx) = on_cores_page();
            press(&mut app, "jj");
            app.update(Update::Cores {
                rows: vec![
                    core_row("old.Core", Some("2"), Some("2")),
                    core_row("same.Core", Some("2"), Some("2")),
                ],
                failure: None,
            });
            assert_eq!(app.selected_core().unwrap().id(), "same.Core");
            app.update(Update::Cores {
                rows: vec![core_row("old.Core", Some("2"), Some("2"))],
                failure: None,
            });
            assert_eq!(app.selected_core().unwrap().id(), "old.Core");
        }
    }

    /// The screen's top line.
    fn header(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| ui::draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..80).map(|x| buffer[(x, 0)].symbol()).collect()
    }

    #[test]
    fn the_header_says_the_device_is_being_read_and_the_footer_does_not() {
        let (mut app, _rx) = app();
        let root = PathBuf::from("/Volumes/OTHER");
        app.update(Update::ReadingDevice(root));
        assert!(app.is_busy());
        assert!(app.job().is_none());
        let top = header(&mut app);
        assert!(top.contains("reading OTHER…"), "{top}");
        app.update(Update::DeviceRead);
        assert!(!app.is_busy());
        let top = header(&mut app);
        assert!(top.contains("● POCKET"), "{top}");
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
    fn every_page_s_columns_start_at_the_same_place() {
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
    fn a_marked_row_has_a_grey_background() {
        let (mut app, _rx) = app();
        // Marks the first row and moves on to the second.
        press(&mut app, "m");
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let background = |y: u16| buffer[(20, y)].bg;
        assert_eq!(background(4), Color::Indexed(237));
        assert_eq!(background(5), Color::Indexed(255));
        assert_eq!(background(6), Color::Reset);
    }

    #[test]
    fn the_selected_row_looks_different_once_marked() {
        let (mut app, _rx) = app();
        // Marks the first row, and goes back to it.
        press(&mut app, "mk");
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(20, 4)].bg, Color::Indexed(246));
    }

    #[test]
    fn a_symbol_is_in_the_middle_of_its_column() {
        let (mut app, _rx) = app();
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let line = |y: u16| -> Vec<&str> { (0..72).map(|x| buffer[(x, y)].symbol()).collect() };
        // The column of `ART` is 3 wide.
        let art = line(3)
            .windows(3)
            .position(|w| w == ["A", "R", "T"])
            .unwrap();
        let has_it = (4..10).any(|y| ["–", "✓"].contains(&line(y)[art + 1]));
        assert!(has_it);
    }

    /// Every screen draws, at sizes down to a tiny terminal.
    #[test]
    fn every_state_draws_at_any_size() {
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

    mod marks {
        use super::*;

        #[test]
        fn without_any_a_key_acts_on_the_selected_row() {
            let (mut app, rx) = app();
            press(&mut app, "ji");
            let [Command::PullGames(games)] = &sent(&rx)[..] else {
                panic!()
            };
            assert_eq!(paths(games), [Path::new("Both.gba")]);
        }

        #[test]
        fn a_key_acts_on_every_one_and_clears_them() {
            let (mut app, rx) = app();
            // Each mark moves on.
            press(&mut app, "jmmb");
            let [Command::PullSaves(games)] = &sent(&rx)[..] else {
                panic!()
            };
            assert_eq!(paths(games), [Path::new("Both.gba"), Path::new("Hack.gba")]);
            assert_eq!(app.marked_count(), 0);
        }

        #[test]
        fn u_unmarks_one_big_u_all_and_big_m_marks_what_is_shown() {
            let (mut app, _rx) = app();
            press(&mut app, "mm");
            assert_eq!(app.marked_count(), 2);
            press(&mut app, "ku");
            assert_eq!(app.marked_count(), 1);
            press(&mut app, "U");
            assert_eq!(app.marked_count(), 0);
            press(&mut app, "/hack\nM");
            assert_eq!(app.marked_count(), 1);
        }

        #[test]
        fn those_hidden_by_a_search_still_count() {
            let (mut app, rx) = app();
            press(&mut app, "jjm/alpha\nms");
            let [Command::PushGames(games)] = &sent(&rx)[..] else {
                panic!()
            };
            // The game only on the device, hidden, has nothing to push.
            assert_eq!(games.len(), 1);
            assert_eq!(games[0].name, "Alpha.gba");
        }
    }

    mod selection {
        use super::*;

        #[test]
        fn the_search_box_edits_at_the_cursor() {
            let (mut app, _rx) = app();
            press(&mut app, "/hck");
            app.handle_key(KeyEvent::from(KeyCode::Left));
            app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
            press(&mut app, "a");
            assert_eq!(app.query(), "hack");
            assert_eq!(
                app.query_around_cursor(),
                ("ha".to_string(), "ck".to_string())
            );
            assert_eq!(name_of(app.selected_row().unwrap()), "Hack.gba");
        }

        #[test]
        fn a_search_finds_a_game_by_letters_in_order() {
            let (mut app, _rx) = app();
            press(&mut app, "/hck");
            assert_eq!(app.visible_len(), 1);
            assert_eq!(name_of(app.selected_row().unwrap()), "Hack.gba");
        }

        #[test]
        fn a_search_keeps_the_selection_on_its_row() {
            let (mut app, _rx) = app();
            press(&mut app, "jj/hac");
            assert_eq!(app.visible_len(), 1);
            assert_eq!(name_of(app.selected_row().unwrap()), "Hack.gba");
            press(&mut app, "\x1b");
            assert_eq!(app.visible_len(), 4);
            assert_eq!(name_of(app.selected_row().unwrap()), "Hack.gba");
        }

        #[test]
        fn a_snapshot_keeps_the_selection_and_the_marks_on_their_rows() {
            let (mut app, _rx) = app();
            press(&mut app, "jjkm");
            let mut fewer = rows();
            fewer.remove(0);
            app.update(Update::Snapshot(Snapshot {
                rows: fewer,
                device: None,
            }));
            assert_eq!(name_of(app.selected_row().unwrap()), "Hack.gba");
            assert_eq!(app.marked_count(), 1);
        }
    }

    mod dialogs {
        use super::*;

        #[test]
        fn duplicates_wait_for_the_dialog_open_to_close() {
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
        fn duplicates_show_at_once_when_no_dialog_is_open() {
            let (mut app, _rx) = app();
            app.update(Update::Duplicates(vec!["a line".to_string()]));
            assert_eq!(app.dialog().unwrap().title, "Duplicate games on the device");
        }

        #[test]
        fn deleting_on_the_device_asks_first_and_the_answer_decides() {
            let (mut app, rx) = app();
            press(&mut app, "jjX");
            assert!(app.dialog().unwrap().danger);
            assert!(sent(&rx).is_empty());
            press(&mut app, "\x1b");
            assert!(app.dialog().is_none());
            assert!(sent(&rx).is_empty());
            press(&mut app, "X\n");
            assert!(matches!(sent(&rx)[..], [Command::DeleteGames(_)]));
        }

        #[test]
        fn deleting_an_orphan_save_on_the_device_asks_and_cancels_by_default() {
            let (mut app, rx) = app();
            press(&mut app, "jjjX");
            let dialog = app.dialog().unwrap();
            assert!(dialog.danger);
            assert_eq!(dialog.focus, 1);
            press(&mut app, "\n");
            assert!(sent(&rx).is_empty());
            press(&mut app, "Xh\n");
            assert!(matches!(sent(&rx)[..], [Command::DeleteOrphanSaves(_)]));
        }

        #[test]
        fn an_orphan_save_on_the_device_is_not_for_the_trash() {
            let (mut app, rx) = app();
            press(&mut app, "jjjx");
            assert!(app.dialog().is_none());
            assert!(sent(&rx).is_empty());
        }

        #[test]
        fn trashing_lists_the_files_first() {
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
        fn a_game_sends_the_path_when_its_extension_tells_the_platform() {
            let (mut app, rx) = app();
            press(&mut app, "I");
            retype(&mut app, "/some/Mine.GBA\n");
            let [Command::ImportGame(path, None)] = &sent(&rx)[..] else {
                panic!()
            };
            assert_eq!(path, Path::new("/some/Mine.GBA"));
            assert!(app.import().is_none());
            assert!(app.platform_choice().is_none());
        }

        #[test]
        fn a_folder_is_imported_with_no_platform_asked_for() {
            let (mut app, rx) = app();
            press(&mut app, "I");
            retype(&mut app, "/some/roms/\n");
            let [Command::ImportGame(path, None)] = &sent(&rx)[..] else {
                panic!()
            };
            assert_eq!(path, Path::new("/some/roms"));
            assert!(app.import().is_none());
            assert!(app.platform_choice().is_none());
        }

        #[test]
        fn a_game_whose_extension_does_not_tell_waits_for_the_platform_chosen() {
            let (mut app, rx) = app();
            press(&mut app, "I");
            retype(&mut app, "/some/x.bin\n");
            assert!(sent(&rx).is_empty());
            assert!(app.platform_choice().is_some());
            press(&mut app, "j\n");
            let [Command::ImportGame(path, Some(Platform::Gbc))] = &sent(&rx)[..] else {
                panic!()
            };
            assert_eq!(path, Path::new("/some/x.bin"));
            assert!(app.platform_choice().is_none());
        }

        #[test]
        fn escape_at_the_platforms_imports_nothing() {
            let (mut app, rx) = app();
            press(&mut app, "I");
            retype(&mut app, "/some/x.bin\n\x1b");
            assert!(app.platform_choice().is_none());
            assert!(sent(&rx).is_empty());
        }

        #[test]
        fn enter_on_a_folder_keeps_the_window_for_a_save_open() {
            let (mut app, rx) = app();
            press(&mut app, "j\nI\n");
            assert!(app.import().is_some());
            assert!(sent(&rx).is_empty());
        }

        #[test]
        fn a_save_sends_the_path_for_the_game_of_the_row() {
            let (mut app, rx) = app();
            press(&mut app, "j\nI");
            assert!(app.import().is_some());
            // Keys are text here, whatever they do elsewhere.
            press(&mut app, "x");
            assert!(sent(&rx).is_empty());
            retype(&mut app, "/some/Mine.sav\n");
            let [Command::ImportSave(game, path)] = &sent(&rx)[..] else {
                panic!()
            };
            assert_eq!(game.name, "Both.gba");
            assert_eq!(path, Path::new("/some/Mine.sav"));
            assert!(app.import().is_none());
        }

        #[test]
        fn enter_takes_the_match_picked_before_it_imports() {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("Mine.sav"), b"").unwrap();
            let folder = format!("{}/", dir.path().display());
            let (mut app, rx) = app();
            press(&mut app, "j\nI");
            type_path(&mut app, &folder);
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
        fn escape_lets_go_of_the_match_picked_and_then_closes() {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("Mine.sav"), b"").unwrap();
            let folder = format!("{}/", dir.path().display());
            let (mut app, rx) = app();
            press(&mut app, "j\nI");
            type_path(&mut app, &folder);
            app.handle_key(KeyEvent::from(KeyCode::Down));
            press(&mut app, "\x1b");
            let import = app.import().unwrap();
            assert_eq!(import.input.picked(), None);
            press(&mut app, "\x1b");
            assert!(app.import().is_none());
            assert_eq!(app.selected_version(), Some(0));
            assert!(sent(&rx).is_empty());
        }

        #[test]
        fn a_row_not_in_the_library_has_nothing_to_import_for() {
            let (mut app, rx) = app();
            press(&mut app, "jj\nI");
            assert!(app.import().is_none());
            assert!(sent(&rx).is_empty());
            assert_eq!(
                app.message().unwrap().text,
                "nothing here is in the library"
            );
        }
    }
}
