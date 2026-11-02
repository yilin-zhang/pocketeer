//! TUI state. `App` owns the parts and routes to them: keys and clicks to
//! the open dialog or the current tab, worker updates to the tables, and
//! dialog answers to the flow that asked. Nothing here waits on disk,
//! network or images: `Session` hands that to the worker threads, covers
//! decode on their own thread, and path suggestions are looked up on
//! another.
//!
//! Five tabs, Calibre-style: one list per kind of thing (games, Memories,
//! cores, required files) with a text filter and a "what to show" filter,
//! rather than one tab per source. Rows are recomputed when something
//! changes, never per frame.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Size;
use ratatui_image::picker::Picker;
use std::path::Path;

mod card;
pub mod covers;
mod dialog;
mod games;
mod import;
mod layout;
pub mod line_edit;
mod memories;
mod pane;
mod path_input;
mod rename;
mod saves;
mod session;

pub use card::BiosFilter;
pub use dialog::{Modal, SUGGESTIONS_SHOWN, Suggestion};
#[cfg(test)]
pub use games::CoverStep;
pub use games::GameRef;
pub use layout::{Layout, Splitter};
pub use pane::{Cycle, PaneOps};
pub use saves::{SaveLine, SaveMark, status as save_status};
pub use session::{JobProgress, Session};

use card::{Bios, Cores};
pub use dialog::Step;
use dialog::{Dialog, Submitted};
use games::Games;
use layout::Hit;
use memories::Memories;
use session::Update;

use super::{
    pending::{self, Pending, Preview},
    worker::Job,
};
use crate::cli::Global;

pub const HELP: &str = include_str!("../../../docs/tui-help.txt");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Games,
    Memories,
    Cores,
    Bios,
    Device,
}

impl Cycle for Tab {
    const ALL: &'static [Tab] = &[
        Tab::Games,
        Tab::Memories,
        Tab::Cores,
        Tab::Bios,
        Tab::Device,
    ];

    fn label(self) -> &'static str {
        match self {
            Tab::Games => "1  GAMES",
            Tab::Memories => "2  MEMORIES",
            Tab::Cores => "3  CORES",
            Tab::Bios => "4  BIOS",
            Tab::Device => "5  CARD",
        }
    }
}

pub struct App {
    session: Session,
    dialog: Dialog,
    layout: Layout,
    tab: Tab,
    games: Games,
    memories: Memories,
    cores: Cores,
    bios: Bios,
    should_quit: bool,
}

/// What a tab or a flow may touch while it acts: start jobs and write the
/// status line (`session`), and ask the user something (`dialog`).
struct Cx<'a> {
    session: &'a mut Session,
    dialog: &'a mut Dialog,
}

impl Cx<'_> {
    /// A plan is ready: open the dialog it asks for.
    fn planned(&mut self, pending: impl Pending + 'static) {
        self.show(pending::preview(pending));
    }

    fn show(&mut self, preview: Preview) {
        match preview {
            Preview::Notice(line) => self.session.notice(line),
            Preview::Info { title, lines } => self.dialog.info(title, lines),
            Preview::Confirm {
                title,
                lines,
                pending,
            } => self.dialog.confirm(title, lines, pending),
            Preview::SaveConflict { backup, index } => saves::ask_conflict(self, backup, index),
            Preview::PickRenames(plans) => rename::offer_standard_names(self, plans),
            Preview::PickImportNames(import) => import::offer_names(self, import),
            Preview::AskPlatform { source, files } => import::ask_platform(self, source, files),
        }
    }
}

impl App {
    pub fn new(global: Global, picker: Picker) -> Self {
        App {
            session: Session::new(global),
            dialog: Dialog::new(),
            layout: Layout::default(),
            tab: Tab::Games,
            games: Games::new(picker),
            memories: Memories::new(),
            cores: Cores::new(),
            bios: Bios::new(),
            should_quit: false,
        }
    }

    // -- read by `ui` -------------------------------------------------------

    pub fn tab(&self) -> Tab {
        self.tab
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn modal(&self) -> Option<&Modal> {
        self.dialog.modal()
    }

    pub fn games(&self) -> &Games {
        &self.games
    }

    pub fn memories(&self) -> &Memories {
        &self.memories
    }

    pub fn cores(&self) -> &Cores {
        &self.cores
    }

    pub fn bios(&self) -> &Bios {
        &self.bios
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// Drawing records where things went, for the mouse.
    pub fn layout_mut(&mut self) -> &mut Layout {
        &mut self.layout
    }

    /// The table of the current tab (none on the Card tab).
    pub fn pane(&self) -> Option<&dyn PaneOps> {
        match self.tab {
            Tab::Games => Some(self.games.pane()),
            Tab::Memories => Some(self.memories.pane()),
            Tab::Cores => Some(self.cores.pane()),
            Tab::Bios => Some(self.bios.pane()),
            Tab::Device => None,
        }
    }

    /// The selected game's cover (decoded off-thread; `Loading` until then).
    pub fn cover(&mut self, path: &Path, size: Size) -> covers::Cover<'_> {
        self.games.cover(&self.session, path, size)
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    #[cfg(test)]
    pub fn dialog_mut(&mut self) -> &mut Dialog {
        &mut self.dialog
    }

    fn pane_mut(&mut self) -> Option<&mut dyn PaneOps> {
        match self.tab {
            Tab::Games => Some(self.games.pane_mut()),
            Tab::Memories => Some(self.memories.pane_mut()),
            Tab::Cores => Some(self.cores.pane_mut()),
            Tab::Bios => Some(self.bios.pane_mut()),
            Tab::Device => None,
        }
    }

    fn cx(&mut self) -> Cx<'_> {
        Cx {
            session: &mut self.session,
            dialog: &mut self.dialog,
        }
    }

    // -- the event loop -----------------------------------------------------

    pub fn reload(&mut self) {
        self.session.reload();
    }

    /// Handles everything the workers sent since last time.
    pub fn pump(&mut self) {
        self.games.receive_covers();
        self.dialog.pump();
        let keep = self.games.keep(&self.session);
        for update in self.session.pump(self.dialog.is_open()) {
            match update {
                Update::Rows => self.rebuild_keeping(keep.as_ref()),
                Update::Loaded | Update::Saves => {
                    self.games.index_saves(&self.session);
                    self.rebuild_keeping(keep.as_ref());
                }
                Update::Planned(preview) => self.cx().show(preview),
                Update::Info { title, lines } => self.dialog.info(title, lines),
                Update::Error { title, error } => self.dialog.error(title, &error),
                Update::CardRemoved => self.dialog.close(),
            }
        }
    }

    /// Called every couple of seconds by the event loop.
    pub fn tick(&mut self) {
        if let Some(Update::CardRemoved) = self.session.tick() {
            self.dialog.close();
        }
    }

    /// Recomputes every table, keeping the selected game.
    fn rebuild(&mut self) {
        let keep = self.games.keep(&self.session);
        self.rebuild_keeping(keep.as_ref());
    }

    fn rebuild_keeping(&mut self, keep: Option<&games::Keep>) {
        self.games.rebuild(&self.session, keep);
        self.memories.rebuild(&self.session);
        self.cores.rebuild(&self.session);
        self.bios.rebuild(&self.session);
    }

    /// A dialog was answered: apply the plan, or hand the answer to the
    /// flow that asked.
    fn submitted(&mut self, submitted: Submitted) {
        match submitted {
            Submitted::Apply(pending) => self.session.submit("Applying", Job::Apply(pending)),
            Submitted::Answer(step, answer) => {
                let cx = &mut self.cx();
                match step {
                    Step::Import(s) => import::answer(cx, s, answer),
                    Step::Rename(s) => rename::answer(cx, s, answer),
                    Step::Saves(s) => saves::answer(cx, s, answer),
                    Step::Memories(s) => memories::answer(cx, s, answer),
                    Step::Cover(s) => games::answer(cx, s, answer),
                }
            }
        }
    }

    // -- keys ---------------------------------------------------------------

    pub fn key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        let key = self.emacs_keys(key);
        if self.dialog.is_open() {
            if let Some(submitted) = self.dialog.key(key) {
                self.submitted(submitted);
            }
            return;
        }
        if self.tab == Tab::Games {
            let cx = &mut Cx {
                session: &mut self.session,
                dialog: &mut self.dialog,
            };
            if self.games.panel_key(key, cx) {
                return;
            }
        }
        if let Some(pane) = self.pane_mut()
            && pane.editing()
        {
            if pane.edit_key(key) {
                self.rebuild();
            }
            return;
        }
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('?') => self
                .dialog
                .info("Help", HELP.lines().map(String::from).collect()),
            KeyCode::Tab => self.tab = self.tab.next(),
            KeyCode::BackTab => self.tab = self.tab.prev(),
            KeyCode::Char(c @ '1'..='5') => self.tab = Tab::ALL[c as usize - '1' as usize],
            KeyCode::Char('r') if !self.session.is_busy() => self.reload(),
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            KeyCode::PageDown => self.move_sel(10),
            KeyCode::PageUp => self.move_sel(-10),
            KeyCode::Char('g') | KeyCode::Home => self.move_sel(i32::MIN / 2),
            KeyCode::Char('G') | KeyCode::End => self.move_sel(i32::MAX / 2),
            KeyCode::Char('/') => self.start_search(),
            KeyCode::Char('l') => {
                if let Some(pane) = self.pane_mut() {
                    pane.cycle_filter();
                    self.rebuild();
                }
            }
            KeyCode::Esc => {
                if let Some(pane) = self.pane_mut()
                    && pane.clear_query()
                {
                    self.rebuild();
                }
            }
            _ if self.session.is_busy() => {
                let busy = self.session.busy().main.as_ref().map(|j| j.label.clone());
                let busy = busy.unwrap_or_default();
                self.session
                    .notice(format!("⟳ {busy} — wait for it to finish."));
            }
            _ => {
                let cx = &mut Cx {
                    session: &mut self.session,
                    dialog: &mut self.dialog,
                };
                match self.tab {
                    Tab::Games => self.games.key(key, cx),
                    Tab::Memories => self.memories.key(key, cx),
                    Tab::Cores => self.cores.key(key, cx),
                    Tab::Bios => self.bios.key(key, cx),
                    Tab::Device => card::card_key(key, cx),
                }
            }
        }
    }

    /// Emacs motion as the arrow keys everywhere: C-n / C-p down and up,
    /// C-v / M-v a page, C-g cancels. C-f / C-b are right and left except
    /// in a text field, where they move the cursor (see `line_edit`).
    fn emacs_keys(&self, key: KeyEvent) -> KeyEvent {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::META);
        let typing = matches!(self.dialog.modal(), Some(Modal::Input { .. }))
            || (!self.dialog.is_open() && self.pane().is_some_and(|p| p.editing()));
        let code = match key.code {
            KeyCode::Char('n') if ctrl => KeyCode::Down,
            KeyCode::Char('p') if ctrl => KeyCode::Up,
            KeyCode::Char('v') if ctrl => KeyCode::PageDown,
            KeyCode::Char('v') if alt => KeyCode::PageUp,
            KeyCode::Char('g') if ctrl => KeyCode::Esc,
            KeyCode::Char('f') if ctrl && !typing => KeyCode::Right,
            KeyCode::Char('b') if ctrl && !typing => KeyCode::Left,
            _ => return key,
        };
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// `/`: type into the current tab's filter.
    fn start_search(&mut self) {
        if self.tab == Tab::Cores {
            self.cores.start_search();
            self.rebuild();
        } else if let Some(pane) = self.pane_mut() {
            pane.start_editing();
        }
    }

    fn move_sel(&mut self, delta: i32) {
        if let Some(pane) = self.pane_mut() {
            pane.move_sel(delta);
        }
    }

    // -- mouse --------------------------------------------------------------

    /// Click a tab, a filter segment, the query, a row or a dialog button;
    /// drag a divider; the wheel moves.
    pub fn mouse(&mut self, m: MouseEvent) {
        let (x, y) = (m.column, m.row);
        match m.kind {
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let down = m.kind == MouseEventKind::ScrollDown;
                if self.dialog.is_open() {
                    let code = if down { KeyCode::Down } else { KeyCode::Up };
                    self.dialog.key(KeyEvent::from(code));
                } else {
                    self.move_sel(if down { 1 } else { -1 });
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => self.layout.drag(x, y, self.tab),
            MouseEventKind::Up(_) => self.layout.end_drag(),
            MouseEventKind::Down(MouseButton::Left) if self.dialog.is_open() => {
                if let Some(primary) = self.layout.button_at(x, y)
                    && let Some(submitted) = self.dialog.press(primary)
                {
                    self.submitted(submitted);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => match self.layout.hit(x, y) {
                Some(Hit::Splitter(which, area)) => self.layout.start_drag(which, area),
                Some(Hit::Tab(tab)) => self.tab = tab,
                Some(Hit::Query) => self.start_search(),
                Some(Hit::Segment(i)) => {
                    if let Some(pane) = self.pane_mut() {
                        pane.set_filter_index(i);
                        self.rebuild();
                    }
                }
                Some(Hit::Row(row)) => {
                    self.games.leave_panel();
                    if let Some(pane) = self.pane_mut() {
                        pane.select(row);
                    }
                }
                None => {}
            },
            _ => {}
        }
    }
}

/// An app for tests: no card, offline, everything under `tmp`.
#[cfg(test)]
pub(crate) fn test_app(tmp: &Path) -> App {
    let global = Global {
        root: Some(tmp.join("no-card")),
        offline: true,
        cache_dir: Some(tmp.join("cache")),
        library: Some(tmp.join("library")),
        config: Some(tmp.join("config")),
        ..Default::default()
    };
    App::new(global, Picker::halfblocks())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::worker::{GameRow, LibraryView, Snapshot};
    use pocketeer_core::{
        card::roms::Confidence,
        library::Game,
        transfer::{CardMatch, LocalMatch, Presence},
    };
    use std::path::PathBuf;

    fn game(id: i64, name: &str) -> GameRow {
        GameRow {
            game: Game {
                id,
                platform: "gba".into(),
                name: name.into(),
                roms: Vec::new(),
            },
            cover: None,
            matches: Vec::new(),
        }
    }

    fn library(app: &mut App) -> &mut LibraryView {
        app.session
            .snapshot_mut()
            .and_then(|s| s.library.as_mut())
            .unwrap()
    }

    fn selected_id(app: &App) -> i64 {
        app.games.selected_game(&app.session).unwrap().game.id
    }

    #[test]
    fn a_reload_keeps_the_selected_game_across_insertion_and_card_matching() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = test_app(tmp.path());
        app.session.set_snapshot(Snapshot {
            fingerprint: 0,
            card: None,
            library: Some(LibraryView {
                root: tmp.path().join("library"),
                games: vec![game(7, "Middle"), game(9, "Zulu")],
                platforms: vec!["gba".into()],
            }),
        });
        app.rebuild();
        app.key(KeyEvent::from(KeyCode::Down));
        assert_eq!(selected_id(&app), 9);

        // A game sorted in ahead of it doesn't move the selection.
        let keep = app.games.keep(&app.session);
        library(&mut app).games.insert(0, game(10, "Alpha"));
        app.rebuild_keeping(keep.as_ref());
        assert_eq!(selected_id(&app), 9);

        // A card path selected before the card copy was matched to a game
        // finds that game afterwards.
        let path = PathBuf::from("Assets/gba/common/nested/card-name.gba");
        library(&mut app).games[1].matches.push(LocalMatch {
            rom_id: 11,
            platform: "gba".into(),
            name: "local-name.gba".into(),
            presence: Presence::Both,
            copies: vec![CardMatch {
                path: path.clone(),
                name: "card-name.gba".into(),
                confidence: Confidence::Full,
                name_mismatch: true,
            }],
        });
        app.rebuild_keeping(Some(&games::Keep::Card(path)));
        assert_eq!(selected_id(&app), 7);
    }

    #[test]
    fn prompts_take_emacs_keys_and_c_n_picks_a_suggestion() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = test_app(tmp.path());
        app.dialog.input(
            "Import",
            vec![],
            "abc".into(),
            Step::Import(import::Step::Source),
        );
        let key = |app: &mut App, code, m| app.key(KeyEvent::new(code, m));
        key(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        key(&mut app, KeyCode::Char('X'), KeyModifiers::SHIFT);
        key(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
        key(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert_eq!(app.dialog.text(), Some("bXc"));
        app.dialog.set_suggestions(
            (0..3)
                .map(|i| Suggestion {
                    path: format!("/s{i}"),
                    name: format!("s{i}"),
                    is_dir: false,
                    matched: None,
                })
                .collect(),
        );
        key(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
        key(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
        key(&mut app, KeyCode::Char('p'), KeyModifiers::CONTROL);
        assert_eq!(app.dialog.highlight(), Some(0));
        key(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
        key(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
        assert!(!app.dialog.is_open());
    }
}
