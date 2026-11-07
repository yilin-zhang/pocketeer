//! The screen's state and what each key does to it. Anything that touches a
//! disk or the network is left to the worker.
//!
//! `confirm_*` functions open a yes-or-no window and `prompt_for_*` functions
//! open one to type in. The work happens once the user answers.

use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pocketeer_core::{
    cores::{BiosRow, BiosStatus, CoreList, CoreRow},
    device::{DeviceEntry, OrphanDeviceSave},
    library::{LocalEntry, OrphanLocalSave},
    platform::Platform,
    pocketeer::GameRow,
};
use ratatui::widgets::TableState;
use strum::IntoEnumIterator;
use tui_input::Input;

mod dialog;
mod games_page;
mod keymap;

pub use dialog::Dialog;
pub use games_page::{GamesFilter, GamesPage};
pub use keymap::{Group, Keymap};

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
            RenameTarget::OnDevice(on_device) => {
                let stem = on_device.path.file_stem().unwrap_or_default();
                stem.to_string_lossy().into_owned()
            }
        }
    }
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

impl Prompt {
    fn keymap(&self) -> &'static Keymap {
        match self {
            Prompt::Import(import) => match import.target {
                ImportTarget::Bios(_) => &Keymap::BIOS_PROMPT,
                _ => &Keymap::IMPORT_PROMPT,
            },
            Prompt::Platform(_) => &Keymap::PLATFORM_PROMPT,
            Prompt::Rename(_) => &Keymap::RENAME_PROMPT,
        }
    }
}

// ============================================================================
// Cores page
// ============================================================================

#[derive(Default)]
pub struct CoresPage {
    /// The cores as the worker last sent them, or none until it has.
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
    fn set_core_list(&mut self, core_list: CoreList) {
        let selected_id = self.selected_row().map(|row| row.id().to_string());
        let new_selected_row =
            selected_id.and_then(|id| core_list.rows.iter().position(|row| row.id() == id));
        let selected = new_selected_row
            .or(self.table_state.selected())
            .unwrap_or(0);
        self.core_list = Some(core_list);
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
            WorkerMessage::CoreListChanged(core_list) => self.cores.set_core_list(core_list),
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
                self.status_message = Some(StatusMessage {
                    text: summary,
                    is_failure: failed,
                });
            }
            // The event loop ends before this message gets here.
            WorkerMessage::LibraryOpenFailed(_) => {}
            WorkerMessage::RenamesProposed(renames) if renames.is_empty() => {
                self.show_status_message("no game to rename");
            }
            WorkerMessage::RenamesProposed(renames) => {
                self.open_dialog_when_free(Dialog::renames(renames));
            }
            WorkerMessage::DuplicateGamesSkipped(lines) => {
                self.open_dialog_when_free(Dialog::duplicates(lines));
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
        // A message stays until the next key.
        self.status_message = None;
        // The terminal is in raw mode, so Ctrl-C arrives as a key rather than
        // stopping the program. It quits whatever has the focus.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        // A key bound in what has the focus does its action. Any other key is
        // typed into the input or search, if one is open.
        if !self.keymap().press(key, self) {
            self.type_key(key);
        }
        // A dialog that came while another was open shows once that one has
        // closed.
        if self.dialog.is_none() {
            self.dialog = self.next_dialog.take();
        }
    }

    /// The keys of what has the focus.
    pub fn keymap(&self) -> &'static Keymap {
        if self.dialog.is_some() {
            return &Keymap::DIALOG;
        }
        if let Some(prompt) = &self.prompt {
            return prompt.keymap();
        }
        match self.current_page {
            Page::Games => self.games.keymap(),
            Page::Cores => &Keymap::CORES_PAGE,
            Page::Bios => &Keymap::BIOS_PAGE,
            Page::Device => &Keymap::DEVICE_PAGE,
        }
    }

    /// Gives a key that isn't bound to whatever is being typed in, if
    /// anything is.
    fn type_key(&mut self, key: KeyEvent) {
        if self.dialog.is_some() {
            return;
        }
        match &mut self.prompt {
            Some(Prompt::Import(import)) => {
                import.input.handle_key(key);
                self.request_input_dir();
            }
            Some(Prompt::Rename(rename)) => {
                path_input::edit(&mut rename.input, key);
            }
            Some(Prompt::Platform(_)) => {}
            None if self.games.is_searching() => self.games.type_in_search(key),
            None => {}
        }
    }

    /// Clears the search if there is one, or else stops what is running.
    fn escape(&mut self) {
        if !self.games.search_text().is_empty() {
            self.games.clear_search();
        } else if let Some(command_status) = &mut self.command_status {
            command_status.is_canceling = true;
            self.worker_handle.cancel();
        }
    }

    // ---- Pages and moving in their tables --------------------------------------

    /// How far a page key moves.
    const PAGE_KEY_ROW_COUNT: isize = 20;

    fn next_page(&mut self) {
        match self.current_page {
            Page::Games => self.show_cores(),
            Page::Cores => self.current_page = Page::Bios,
            Page::Bios => self.current_page = Page::Device,
            Page::Device => self.current_page = Page::Games,
        }
    }

    fn move_rows(&mut self, by: isize) {
        let from = |table_state: &TableState| table_state.selected().unwrap_or(0);
        match self.current_page {
            Page::Games => self.games.move_selection_by(by),
            Page::Cores => {
                let to = from(&self.cores.table_state).saturating_add_signed(by);
                self.cores.select_row(to);
            }
            Page::Bios => {
                let to = from(&self.bios.table_state).saturating_add_signed(by);
                self.bios.select_row(to);
            }
            Page::Device => {}
        }
    }

    /// Selects row `i`, or the last if there are fewer.
    fn select_row(&mut self, i: usize) {
        match self.current_page {
            Page::Games => self.games.select_row(i),
            Page::Cores => self.cores.select_row(i),
            Page::Bios => self.bios.select_row(i),
            Page::Device => {}
        }
    }

    fn push_selected_version(&mut self) {
        if let Some(selected) = self.games.selected_version_index() {
            self.push_save_version(selected);
        }
    }

    fn confirm_trash_selected_version(&mut self) {
        if let Some(selected) = self.games.selected_version_index() {
            self.confirm_trash_version(selected);
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
        self.dialog = Some(Dialog::remove_core(row.id().to_string()));
    }

    // ---- Prompts ---------------------------------------------------------------

    fn confirm_import(&mut self) {
        let Some(Prompt::Import(import)) = &mut self.prompt else {
            return;
        };
        let takes_dir = matches!(import.target, ImportTarget::Game);
        if !import.input.fill_in_picked() && (takes_dir || !import.input.ends_with_slash()) {
            self.import_input_path();
        } else {
            self.request_input_dir();
        }
    }

    /// Unpicks the match picked, or else closes the prompt.
    fn cancel_import(&mut self) {
        if let Some(Prompt::Import(import)) = &mut self.prompt
            && !import.input.unpick()
        {
            self.prompt = None;
        }
    }

    fn complete_path(&mut self) {
        if let Some(Prompt::Import(import)) = &mut self.prompt {
            import.input.complete();
            self.request_input_dir();
        }
    }

    fn pick_next_path(&mut self) {
        if let Some(Prompt::Import(import)) = &mut self.prompt {
            import.input.pick_next();
        }
    }

    fn pick_previous_path(&mut self) {
        if let Some(Prompt::Import(import)) = &mut self.prompt {
            import.input.pick_previous();
        }
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

    fn confirm_rename(&mut self) {
        let Some(Prompt::Rename(rename)) = self.prompt.take() else {
            return;
        };
        let new = rename.input.value().trim().to_string();
        if new != rename.stem() {
            self.worker_handle.send(match rename.target {
                RenameTarget::InLibrary(local) => {
                    WorkerCommand::RenameGamesInLibrary(vec![(local, new)])
                }
                RenameTarget::OnDevice(on_device) => WorkerCommand::RenameOnDevice(on_device, new),
            });
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

    fn confirm_platform(&mut self) {
        let Some(Prompt::Platform(choice)) = self.prompt.take() else {
            return;
        };
        let platform = Platform::iter().nth(choice.selected_platform_index);
        self.worker_handle
            .send(WorkerCommand::ImportGame(choice.file_to_import, platform));
    }

    fn pick_platform(&mut self, by: isize) {
        if let Some(Prompt::Platform(choice)) = &mut self.prompt {
            let last = Platform::iter().count().saturating_sub(1);
            choice.selected_platform_index = choice
                .selected_platform_index
                .saturating_add_signed(by)
                .min(last);
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

    fn dialog_focus_left(&mut self) {
        if let Some(dialog) = &mut self.dialog {
            dialog.focused_button = dialog.focused_button.saturating_sub(1);
        }
    }

    fn dialog_focus_right(&mut self) {
        if let Some(dialog) = &mut self.dialog {
            let last = dialog.buttons.len().saturating_sub(1);
            dialog.focused_button = (dialog.focused_button + 1).min(last);
        }
    }

    /// Presses the focused button.
    fn dialog_confirm(&mut self) {
        let Some(dialog) = &mut self.dialog else {
            return;
        };
        let command = dialog
            .buttons
            .get_mut(dialog.focused_button)
            .and_then(|button| button.command.take());
        self.dialog = None;
        if let Some(command) = command {
            self.games.clear_marks();
            self.worker_handle.send(command);
        }
    }

    fn confirm_trash_games(&mut self) {
        let entries = self.games.library_entries_to_act_on();
        if entries.is_empty() {
            self.confirm_trash_orphans();
            return;
        }
        self.dialog = Some(Dialog::trash_games(entries));
    }

    /// Asks the worker what the target games would be renamed to. Its answer
    /// opens the dialog. While it is busy that answer could take long to come,
    /// so nothing is asked then.
    fn propose_renames_to_standard_names(&mut self) {
        let entries = self.games.library_entries_to_act_on();
        if entries.is_empty() {
            self.show_status_message("nothing here is in the library");
            return;
        }
        if self.is_busy() {
            self.show_status_message("busy; try again when this is done");
            return;
        }
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
        self.dialog = Some(Dialog::trash_orphans(orphans));
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
        self.dialog = Some(Dialog::trash_version(local, version));
    }

    fn confirm_delete_games(&mut self) {
        let entries = self.games.device_entries_to_act_on();
        if entries.is_empty() {
            self.confirm_delete_orphans();
            return;
        }
        let is_any_only_here = self
            .games
            .rows_to_act_on()
            .iter()
            .any(|row| matches!(row, GamesTableRow::Game(GameRow::OnDevice(_))));
        self.dialog = Some(Dialog::delete_games(entries, is_any_only_here));
    }

    fn confirm_delete_orphans(&mut self) {
        let targets = self.games.rows_to_act_on();
        let orphans = targets.iter().filter_map(|row| row.orphan_on_device());
        let orphans: Vec<OrphanDeviceSave> = orphans.cloned().collect();
        if orphans.is_empty() {
            self.show_status_message("nothing here is on the device");
            return;
        }
        self.dialog = Some(Dialog::delete_orphans(orphans));
    }

    fn confirm_delete_save(&mut self) {
        let entries = self.games.device_entries_to_act_on();
        if entries.is_empty() {
            self.show_status_message("nothing here is on the device");
            return;
        }
        let is_any_unsaved =
            self.games.rows_to_act_on().iter().any(
                |table_row| matches!(table_row, GamesTableRow::Game(row) if row.has_new_save_on_device()),
            );
        self.dialog = Some(Dialog::delete_save(entries, is_any_unsaved));
    }

    // ---- Commands for the worker -----------------------------------------------

    fn send_for_device_rows(&mut self, command: fn(Vec<DeviceEntry>) -> WorkerCommand) {
        let entries = self.games.device_entries_to_act_on();
        self.send_for_entries(entries, command, "nothing here is on the device");
    }

    fn send_for_library_rows(&mut self, command: fn(Vec<LocalEntry>) -> WorkerCommand) {
        let entries = self.games.library_entries_to_act_on();
        self.send_for_entries(entries, command, "nothing here is in the library");
    }

    fn send_for_entries<T>(
        &mut self,
        entries: Vec<T>,
        command: impl FnOnce(Vec<T>) -> WorkerCommand,
        message_if_none: &str,
    ) {
        if entries.is_empty() {
            self.show_status_message(message_if_none);
            return;
        }
        self.games.clear_marks();
        self.worker_handle.send(command(entries));
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
            _ => self.show_status_message("nothing here is both in the library and on the device"),
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

/// Row `i` of `len`, the last row if there are fewer, or none if there are
/// none.
fn clamped_row(i: usize, len: usize) -> Option<usize> {
    (len > 0).then(|| i.min(len - 1))
}

#[cfg(test)]
mod tests {
    use super::games_page::tests::{local, rows};
    use super::*;
    use crate::tui::{ui, worker::QueuedCommand};
    use pocketeer_core::{
        cores::{BiosRow, BiosStatus, InstalledCore, InventoryCore},
        device::{DiskSpace, Firmware},
        error::BasicError,
        platform::Platform,
    };
    use ratatui::{Terminal, backend::TestBackend};
    use ratatui_image::picker::Picker;
    use std::{
        path::{Path, PathBuf},
        sync::{atomic::Ordering, mpsc::Receiver},
        time::Duration,
    };

    fn visible_names(app: &App) -> Vec<String> {
        app.games.visible_rows().map(GamesTableRow::name).collect()
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
        app.handle_worker_message(WorkerMessage::CoreListChanged(CoreList {
            rows: vec![
                core_row("new.Core", None, Some("1")),
                core_row("old.Core", Some("1"), Some("2")),
                core_row("same.Core", Some("2"), Some("2")),
                core_row("mine.Core", Some("1"), None),
            ],
            inventory_failure: None,
        }));
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
            press(&mut app, "xh\n");
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

    mod marks {
        use super::*;

        #[test]
        fn acts_on_marked_rows_that_filter_hides() {
            let (mut app, rx) = app();
            press(&mut app, "mff");
            assert_eq!(visible_names(&app), ["Both.gba", "Hack.gba", "Gone.sav"]);
            assert_eq!(app.games.marked_count(), 1);
            press(&mut app, "xh\n");
            let [WorkerCommand::TrashGames(games)] = &sent(&rx)[..] else {
                panic!("expected the marked game to be trashed");
            };
            assert_eq!(games[0].name, "Alpha.gba");
        }

        #[test]
        fn acts_on_selected_row_without_marks() {
            let (mut app, rx) = app();
            press(&mut app, "jp");
            let [WorkerCommand::PullGames(games)] = &sent(&rx)[..] else {
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
        fn shift_x_confirms_before_deleting_game_and_defaults_to_cancel() {
            let (mut app, rx) = app();
            press(&mut app, "jjX");
            let dialog = app.dialog().unwrap();
            assert!(dialog.buttons[0].is_removal);
            assert_eq!(dialog.focused_button, 1);
            press(&mut app, "\n");
            assert!(app.dialog().is_none());
            assert!(sent(&rx).is_empty());
            press(&mut app, "X\x1b");
            assert!(app.dialog().is_none());
            assert!(sent(&rx).is_empty());
            press(&mut app, "Xh\n");
            assert!(matches!(sent(&rx)[..], [WorkerCommand::DeleteGames(_)]));
        }

        #[test]
        fn shift_x_confirms_deleting_orphan_save_and_defaults_to_cancel() {
            let (mut app, rx) = app();
            press(&mut app, "jjjX");
            let dialog = app.dialog().unwrap();
            assert!(dialog.buttons[0].is_removal);
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
            assert!(dialog.buttons[0].is_removal);
            assert_eq!(dialog.focused_button, 1);
            press(&mut app, "\n");
            assert!(sent(&rx).is_empty());
            press(&mut app, "Xh\n");
            assert!(matches!(sent(&rx)[..], [WorkerCommand::DeleteSaves(_)]));
        }

        #[test]
        fn x_lists_games_before_trashing_and_defaults_to_cancel() {
            let (mut app, rx) = app();
            press(&mut app, "x");
            let dialog = app.dialog().unwrap();
            assert!(dialog.lines.iter().any(|line| line == "Alpha.gba"));
            assert!(dialog.buttons[0].is_removal);
            assert_eq!(dialog.focused_button, 1);
            press(&mut app, "y");
            assert!(sent(&rx).is_empty());
            press(&mut app, "xhy");
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
        fn empty_answer_reports_no_game() {
            let (mut app, _rx) = app();
            press(&mut app, "Mt");
            app.handle_worker_message(WorkerMessage::RenamesProposed(Vec::new()));
            assert!(app.dialog().is_none());
            assert_eq!(app.status_message().unwrap().text, "no game to rename");
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
            app.handle_worker_message(WorkerMessage::CoreListChanged(CoreList {
                rows: vec![core_row("same.Core", Some("2"), Some("2"))],
                inventory_failure: None,
            }));
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
            assert!(dialog.buttons[0].is_removal);
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
            app.handle_worker_message(WorkerMessage::CoreListChanged(CoreList {
                rows: vec![
                    core_row("old.Core", Some("2"), Some("2")),
                    core_row("same.Core", Some("2"), Some("2")),
                ],
                inventory_failure: None,
            }));
            assert_eq!(app.cores.selected_row().unwrap().id(), "same.Core");
            app.handle_worker_message(WorkerMessage::CoreListChanged(CoreList {
                rows: vec![core_row("old.Core", Some("2"), Some("2"))],
                inventory_failure: None,
            }));
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
                    " DEVICE               P  push             N  rename   E  eject      X  delete",
                    " GENERAL  ↵  saves    m  mark   d  DATs   r  reload   ?  help       q  quit",
                ]
            );
        }

        #[test]
        fn help_lists_keys_of_what_is_open() {
            let (mut app, _rx) = app();
            press(&mut app, "?");
            let lines = app.dialog().unwrap().lines.clone();
            assert!(
                lines
                    .iter()
                    .any(|line| line.starts_with("  p    Copies the game"))
            );
            assert!(
                lines
                    .iter()
                    .any(|line| line.starts_with("  a    Downloads a cover"))
            );
            press(&mut app, "\x1b\n?");
            let lines = app.dialog().unwrap().lines.clone();
            assert!(
                lines
                    .iter()
                    .any(|line| line.starts_with("  S    Pushes the version"))
            );
            assert!(!lines.iter().any(|line| line.starts_with("  p ")));
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
                app.handle_worker_message(WorkerMessage::CoreListChanged(CoreList {
                    rows: vec![
                        core_row("new.Core", None, Some("1")),
                        core_row("old.Core", Some("1"), Some("2")),
                        core_row("mine.Core", Some("1"), None),
                    ],
                    inventory_failure: Some(BasicError::BadResponse {
                        from: "the inventory".into(),
                        message: "no network".into(),
                    }),
                }));
            }
            fn open_bios_page(app: &mut App) {
                press(app, "3");
                app.handle_worker_message(WorkerMessage::BiosFilesChanged(vec![
                    bios_row("there.bin", BiosStatus::Present),
                    bios_row("short.bin", BiosStatus::WrongSize(1)),
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
