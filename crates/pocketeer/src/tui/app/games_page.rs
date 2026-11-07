//! The Games page: its rows, what the filter and the search leave of them,
//! the marks, and the saves panel.

use std::{collections::HashSet, fmt, path::PathBuf};

use crossterm::event::KeyEvent;
use pocketeer_core::{
    device::DeviceEntry,
    game::Game,
    library::{LibrarySave, LocalEntry},
    platform::Platform,
};
use ratatui::widgets::TableState;
use strum::EnumIter;
use tui_input::Input;

use super::{clamped_row, keymap::Keymap};
use crate::tui::{path_input, worker::GamesTableRow};

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

impl RowKey {
    fn of(row: &GamesTableRow) -> Self {
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

    /// The version selected in the saves panel, if it is open, kept within
    /// the versions there are now.
    pub fn selected_version_index(&self) -> Option<usize> {
        match self.focus {
            GamesPageFocus::SavesPanel(selected) => {
                let last = self.selected_save_versions().len().saturating_sub(1);
                Some(selected.min(last))
            }
            GamesPageFocus::List | GamesPageFocus::Search => None,
        }
    }

    /// The versions of the selected game's save in the library, newest first.
    pub fn selected_save_versions(&self) -> Vec<LibrarySave> {
        let local = self.selected_row().and_then(GamesTableRow::local_entry);
        let saves = local.and_then(|local| local.save.as_ref());
        saves
            .iter()
            .flat_map(|saves| saves.iter().cloned())
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
        self.marked_rows.contains(&RowKey::of(row))
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

    // ---- The search box -------------------------------------------------------

    pub(super) fn start_search(&mut self) {
        self.focus = GamesPageFocus::Search;
    }

    /// Keeps the search and puts the focus back on the list.
    pub(super) fn finish_search(&mut self) {
        self.focus = GamesPageFocus::List;
    }

    pub(super) fn clear_search(&mut self) {
        let selected_key = self.selected_row().map(RowKey::of);
        self.focus = GamesPageFocus::List;
        self.search_input.reset();
        self.refresh_visible_rows(selected_key);
    }

    pub(super) fn type_in_search(&mut self, key: KeyEvent) {
        let selected_key = self.selected_row().map(RowKey::of);
        path_input::edit(&mut self.search_input, key);
        self.refresh_visible_rows(selected_key);
    }

    // ---- The saves panel ------------------------------------------------------

    /// Opens the panel if the selected row is a game.
    pub(super) fn open_saves_panel(&mut self) {
        if let Some(GamesTableRow::Game(_)) = self.selected_row() {
            self.focus = GamesPageFocus::SavesPanel(0);
        }
    }

    pub(super) fn close_saves_panel(&mut self) {
        self.focus = GamesPageFocus::List;
    }

    pub(super) fn move_in_saves_panel(&mut self, by: isize) {
        if let Some(selected) = self.selected_version_index() {
            let last = self.selected_save_versions().len().saturating_sub(1);
            let to = selected.saturating_add_signed(by).min(last);
            self.focus = GamesPageFocus::SavesPanel(to);
        }
    }

    pub(super) fn clear_marks(&mut self) {
        self.marked_rows.clear();
    }

    /// The keys of what has the focus.
    pub(super) fn keymap(&self) -> &'static Keymap {
        match self.focus {
            GamesPageFocus::List => &Keymap::GAMES_PAGE,
            GamesPageFocus::Search => &Keymap::SEARCH,
            GamesPageFocus::SavesPanel(_) => &Keymap::SAVES_PANEL,
        }
    }

    /// Stores the rows the worker sent, keeping the selection on its row.
    pub(super) fn set_rows(&mut self, rows: Vec<GamesTableRow>) {
        let selected_key = self.selected_row().map(RowKey::of);
        self.rows = Some(rows);
        self.refresh_visible_rows(selected_key);
    }

    /// Recomputes the visible rows, keeping the selection on the row `keep`.
    fn refresh_visible_rows(&mut self, selected_key: Option<RowKey>) {
        let found =
            self.rows().iter().enumerate().filter(|(_, row)| {
                self.filter.shows(row) && row.matches(self.search_input.value())
            });
        let visible: Vec<usize> = found.map(|(i, _)| i).collect();
        let new_selected_row = selected_key.and_then(|key| {
            visible
                .iter()
                .position(|&i| RowKey::of(&self.rows()[i]) == key)
        });
        let selected = new_selected_row
            .or(self.table_state.selected())
            .unwrap_or(0);
        self.visible_row_indices = visible;
        self.select_row(selected);
    }

    pub(super) fn show_next_filter(&mut self) {
        let selected_key = self.selected_row().map(RowKey::of);
        self.filter = self.filter.next();
        self.refresh_visible_rows(selected_key);
    }

    pub(super) fn move_selection_by(&mut self, by: isize) {
        let selected = self.table_state.selected().unwrap_or(0);
        self.select_row(selected.saturating_add_signed(by));
    }

    /// Selects row `i`, or the last if there are fewer.
    pub(super) fn select_row(&mut self, i: usize) {
        let row = clamped_row(i, self.visible_row_indices.len());
        self.table_state.select(row);
    }

    pub(super) fn toggle_mark_and_move_on(&mut self) {
        let Some(key) = self.selected_row().map(RowKey::of) else {
            return;
        };
        if !self.marked_rows.remove(&key) {
            self.marked_rows.insert(key);
        }
        self.move_selection_by(1);
    }

    /// Marks the rows shown. If they are all marked already, clears every mark,
    /// including those on rows a search hides.
    pub(super) fn mark_all_shown_or_clear_marks(&mut self) {
        let shown: Vec<RowKey> = self.visible_rows().map(RowKey::of).collect();
        if shown.iter().all(|key| self.marked_rows.contains(key)) {
            self.marked_rows.clear();
        } else {
            self.marked_rows.extend(shown);
        }
    }

    /// The rows a key acts on, which are the marked rows, visible or not, or
    /// otherwise the selected row. In the saves panel it is always the selected
    /// row.
    pub(super) fn rows_to_act_on(&self) -> Vec<&GamesTableRow> {
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

    /// The copies on the device among the rows to act on.
    pub(super) fn device_entries_to_act_on(&self) -> Vec<DeviceEntry> {
        let rows = self.rows_to_act_on();
        rows.iter()
            .filter_map(|row| row.device_entry().cloned())
            .collect()
    }

    /// The games in the library among the rows to act on.
    pub(super) fn library_entries_to_act_on(&self) -> Vec<LocalEntry> {
        let rows = self.rows_to_act_on();
        rows.iter()
            .filter_map(|row| row.local_entry().cloned())
            .collect()
    }
}

#[cfg(test)]
pub(in crate::tui::app) mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};
    use pocketeer_core::{
        device::OrphanDeviceSave,
        game::{Files, Rom, Save},
        library::{LibrarySaves, OrphanLocalSave},
        pocketeer::GameRow,
    };
    use std::time::{Duration, UNIX_EPOCH};

    fn game(crc32: u32) -> Game {
        Game {
            platform: Platform::Gba,
            files: Files::Single(Rom { size: 1, crc32 }),
        }
    }

    pub(in crate::tui::app) fn local(name: &str, crc32: u32, saves: &[&[u8]]) -> LocalEntry {
        let at = |i: usize| UNIX_EPOCH + Duration::from_secs(1_000 * (i as u64 + 1));
        let saves = saves.iter().enumerate().map(|(i, save_bytes)| LibrarySave {
            name: format!("v{i}"),
            save: Save::from_bytes(save_bytes, at(i)),
        });
        LocalEntry {
            name: name.into(),
            game: game(crc32),
            cover_path: None,
            save: LibrarySaves::from_saves(saves.collect()),
        }
    }

    pub(in crate::tui::app) fn on_device(
        path: &str,
        crc32: u32,
        save: Option<&[u8]>,
    ) -> DeviceEntry {
        DeviceEntry {
            path: PathBuf::from(path),
            game: game(crc32),
            save: save.map(|save_bytes| Save::from_bytes(save_bytes, UNIX_EPOCH)),
        }
    }

    /// One row only in the library, one on both sides, one only on the device,
    /// and an orphan save.
    pub(in crate::tui::app) fn rows() -> Vec<GamesTableRow> {
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

    /// `rows()` with an orphan save in the library as well.
    fn rows_with_orphan_in_library() -> Vec<GamesTableRow> {
        let mut rows = rows();
        rows.push(GamesTableRow::OrphanSaveInLibrary(OrphanLocalSave {
            platform: Platform::Gba,
            stem: "Lost".to_string(),
            save: LibrarySaves::from_saves(vec![LibrarySave {
                name: "x".to_string(),
                save: Save::from_bytes(b"x", UNIX_EPOCH),
            }])
            .unwrap(),
        }));
        rows
    }

    fn page(rows: Vec<GamesTableRow>) -> GamesPage {
        let mut page = GamesPage::default();
        page.set_rows(rows);
        page
    }

    fn visible_names(page: &GamesPage) -> Vec<String> {
        page.visible_rows().map(GamesTableRow::name).collect()
    }

    fn selected_name(page: &GamesPage) -> String {
        page.selected_row().unwrap().name()
    }

    /// Opens the search and types `text`.
    fn search(page: &mut GamesPage, text: &str) {
        page.start_search();
        for character in text.chars() {
            page.type_in_search(KeyEvent::from(KeyCode::Char(character)));
        }
    }

    #[test]
    fn edits_search_text_at_cursor() {
        let mut page = page(rows());
        search(&mut page, "hck");
        page.type_in_search(KeyEvent::from(KeyCode::Left));
        page.type_in_search(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        page.type_in_search(KeyEvent::from(KeyCode::Char('a')));
        assert_eq!(page.search_text(), "hack");
        assert_eq!(
            page.search_text_around_cursor(),
            ("ha".to_string(), "ck".to_string())
        );
        assert_eq!(selected_name(&page), "Hack.gba");
    }

    #[test]
    fn keeps_selection_on_row_while_searching() {
        let mut page = page(rows());
        page.move_selection_by(2);
        search(&mut page, "hac");
        assert_eq!(page.visible_len(), 1);
        assert_eq!(selected_name(&page), "Hack.gba");
        page.clear_search();
        assert_eq!(page.visible_len(), 4);
        assert_eq!(selected_name(&page), "Hack.gba");
    }

    #[test]
    fn selects_nothing_until_empty_search_is_cleared() {
        let mut page = page(rows());
        search(&mut page, "zzzzzz");
        page.finish_search();
        assert!(page.selected_row().is_none());
        assert_eq!(page.table_state.selected(), None);
        page.clear_search();
        assert!(page.selected_row().is_some());
        assert_eq!(page.table_state.selected(), Some(0));
    }

    #[test]
    fn cycles_through_filters_with_their_orphans() {
        let mut page = page(rows_with_orphan_in_library());
        let every_row = ["Alpha.gba", "Both.gba", "Hack.gba", "Gone.sav", "Lost"].as_slice();
        assert_eq!(page.filter(), GamesFilter::All);
        assert_eq!(visible_names(&page), every_row);
        page.show_next_filter();
        assert_eq!(page.filter(), GamesFilter::InLibrary);
        assert_eq!(visible_names(&page), ["Alpha.gba", "Both.gba", "Lost"]);
        page.show_next_filter();
        assert_eq!(page.filter(), GamesFilter::OnDevice);
        assert_eq!(visible_names(&page), ["Both.gba", "Hack.gba", "Gone.sav"]);
        page.show_next_filter();
        assert_eq!(page.filter(), GamesFilter::All);
        assert_eq!(visible_names(&page), every_row);
    }

    #[test]
    fn applies_filter_and_search_together() {
        let mut page = page(rows());
        search(&mut page, "h");
        page.finish_search();
        page.show_next_filter();
        assert_eq!(visible_names(&page), ["Alpha.gba", "Both.gba"]);
    }

    #[test]
    fn keeps_selection_on_row_still_shown() {
        let mut page = page(rows());
        // Both moves to the top, and the row now second is Hack.
        page.move_selection_by(1);
        page.show_next_filter();
        page.show_next_filter();
        assert_eq!(selected_name(&page), "Both.gba");
    }

    #[test]
    fn keeps_filter_when_rows_change() {
        let mut page = page(rows());
        page.show_next_filter();
        page.set_rows(rows());
        assert_eq!(visible_names(&page), ["Alpha.gba", "Both.gba"]);
    }

    #[test]
    fn clearing_search_keeps_filter() {
        let mut page = page(rows());
        page.show_next_filter();
        search(&mut page, "al");
        page.finish_search();
        page.clear_search();
        assert_eq!(page.filter(), GamesFilter::InLibrary);
        assert_eq!(page.search_text(), "");
    }

    #[test]
    fn unmarks_row_already_marked() {
        let mut page = page(rows());
        page.toggle_mark_and_move_on();
        page.toggle_mark_and_move_on();
        assert_eq!(page.marked_count(), 2);
        page.move_selection_by(-1);
        page.toggle_mark_and_move_on();
        assert_eq!(page.marked_count(), 1);
    }

    #[test]
    fn marking_all_shown_then_again_clears_marks_hidden_by_search() {
        let mut page = page(rows());
        page.move_selection_by(2);
        page.toggle_mark_and_move_on();
        search(&mut page, "alpha");
        page.finish_search();
        page.mark_all_shown_or_clear_marks();
        assert_eq!(page.marked_count(), 2);
        page.mark_all_shown_or_clear_marks();
        assert_eq!(page.marked_count(), 0);
    }

    #[test]
    fn keeps_selection_and_marks_on_same_rows_after_rows_change() {
        let mut page = page(rows());
        page.move_selection_by(1);
        page.toggle_mark_and_move_on();
        let mut fewer = rows();
        fewer.remove(0);
        page.set_rows(fewer);
        assert_eq!(selected_name(&page), "Hack.gba");
        assert_eq!(page.marked_count(), 1);
    }
}
