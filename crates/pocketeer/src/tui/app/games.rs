//! The Games tab: the library and the card in one table, a game's saves
//! panel, its cover, and what the keys do to the selected game. Importing
//! and renaming are flows of their own (`import`, `rename`).

use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent};
use pocketeer_core::{
    card::roms::{self, CardRom},
    catalog,
    library::{covers::import as cover_import, remove},
    names,
    transfer::{Presence, to_card},
};
use ratatui::layout::Size;
use ratatui_image::picker::Picker;

use super::{
    Cx,
    covers::{Cover, Covers},
    dialog::{Answer, Step},
    import,
    pane::{Cycle, Pane, PaneOps, matches},
    path_input::{expand_home, start_path},
    rename,
    saves::{self, SaveIndex, SaveLine, SaveMark},
    session::Session,
};
use crate::{
    ops::{
        card::{self as card_cmd, ImportRequest},
        library as library_ops,
    },
    tui::{app::Import, pending, worker::GameRow},
};

/// Where a game has to be to show up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GameFilter {
    All,
    InLibrary,
    OnCard,
    CardOnly,
    NotOnCard,
    WithSaves,
}

impl Cycle for GameFilter {
    const ALL: &'static [Self] = &[
        GameFilter::All,
        GameFilter::InLibrary,
        GameFilter::OnCard,
        GameFilter::CardOnly,
        GameFilter::NotOnCard,
        GameFilter::WithSaves,
    ];

    fn label(self) -> &'static str {
        match self {
            GameFilter::All => "all",
            GameFilter::InLibrary => "in library",
            GameFilter::OnCard => "on card",
            GameFilter::CardOnly => "card only",
            GameFilter::NotOnCard => "not on card",
            GameFilter::WithSaves => "saves",
        }
    }
}

/// One line of the Games table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GameRef {
    /// Index into `LibraryView::games`.
    Library(usize),
    /// Index into `CardSnapshot::files`.
    CardOnly(usize),
}

impl GameRef {
    pub fn game(self, s: &Session) -> Option<&GameRow> {
        match self {
            GameRef::Library(i) => s.library()?.games.get(i),
            GameRef::CardOnly(_) => None,
        }
    }

    pub fn card_rom(self, s: &Session) -> Option<&CardRom> {
        match self {
            GameRef::CardOnly(i) => s.card()?.files.get(i).map(|f| &f.rom),
            GameRef::Library(_) => None,
        }
    }

    /// The name the row goes by.
    pub fn name(self, s: &Session) -> Option<String> {
        match self {
            GameRef::Library(_) => self.game(s).map(|g| g.game.name.clone()),
            GameRef::CardOnly(_) => self.card_rom(s).map(|r| names::stem(&r.name).to_string()),
        }
    }
}

/// Which game is selected, by identity rather than row index, so it can be
/// found again after the rows are recomputed from a new snapshot.
pub enum Keep {
    Library(i64),
    Card(PathBuf),
}

pub struct Games {
    pane: Pane<GameFilter, GameRef>,
    saves: SaveIndex,
    /// The selected line of the saves panel while it has focus.
    panel: Option<usize>,
    /// Covers live in the library, so a reload keeps them.
    covers: Covers,
}

impl Games {
    pub fn new(picker: Picker) -> Self {
        Games {
            pane: Pane::new(GameFilter::All),
            saves: SaveIndex::default(),
            panel: None,
            covers: Covers::spawn(picker),
        }
    }

    // -- what the screen shows ----------------------------------------------

    pub fn pane(&self) -> &Pane<GameFilter, GameRef> {
        &self.pane
    }

    pub fn pane_mut(&mut self) -> &mut dyn PaneOps {
        &mut self.pane
    }

    pub fn selected(&self) -> Option<GameRef> {
        self.pane.selected().copied()
    }

    pub fn selected_game<'s>(&self, s: &'s Session) -> Option<&'s GameRow> {
        self.selected()?.game(s)
    }

    pub fn selected_card_rom<'s>(&self, s: &'s Session) -> Option<&'s CardRom> {
        self.selected()?.card_rom(s)
    }

    pub fn saves_of(&self, row: GameRef) -> &[PathBuf] {
        self.saves.of(row)
    }

    pub fn save_mark(&self, row: GameRef, s: &Session) -> Option<SaveMark> {
        self.saves.mark(row, s.saves())
    }

    /// The selected line of the saves panel, while it has focus.
    pub fn panel(&self) -> Option<usize> {
        self.panel
    }

    /// The lines of the selected game's saves panel.
    pub fn panel_lines(&self, s: &Session) -> Vec<SaveLine> {
        self.selected()
            .map(|row| self.saves.lines(row, s.saves()))
            .unwrap_or_default()
    }

    pub fn panel_line(&self, s: &Session) -> Option<SaveLine> {
        self.panel_lines(s).get(self.panel?).cloned()
    }

    /// The selected game's cover, and the ones a step or two away started
    /// ahead so moving onto them finds them ready.
    pub fn cover(&mut self, s: &Session, path: &Path, size: Size) -> Cover<'_> {
        let sel = self.pane.sel();
        let near: Vec<PathBuf> = [sel + 1, sel.wrapping_sub(1), sel + 2]
            .into_iter()
            .filter_map(|i| self.pane.rows().get(i))
            .filter_map(|r| r.game(s)?.cover.clone())
            .collect();
        self.covers.show(path, &near, size)
    }

    /// Decoded covers that finished since the last frame.
    pub fn receive_covers(&mut self) {
        self.covers.receive();
    }

    // -- rows ---------------------------------------------------------------

    /// The selected game, to find again after a reload.
    pub fn keep(&self, s: &Session) -> Option<Keep> {
        match self.selected()? {
            GameRef::Library(i) => Some(Keep::Library(s.library()?.games.get(i)?.game.id)),
            GameRef::CardOnly(i) => Some(Keep::Card(s.card()?.files.get(i)?.rom.rel_path.clone())),
        }
    }

    /// Saves were read again (or games changed): match them to the rows.
    pub fn index_saves(&mut self, s: &Session) {
        self.saves = SaveIndex::build(s);
    }

    /// Recomputes the rows, staying on `keep`.
    pub fn rebuild(&mut self, s: &Session, keep: Option<&Keep>) {
        let rows = self.compute_rows(s);
        self.pane.set_rows(rows, |row| match keep {
            Some(Keep::Library(id)) => row.game(s).is_some_and(|g| g.game.id == *id),
            Some(Keep::Card(path)) => {
                row.card_rom(s).is_some_and(|r| r.rel_path == *path)
                    || row.game(s).is_some_and(|g| {
                        g.matches
                            .iter()
                            .flat_map(|m| &m.copies)
                            .any(|c| c.path == *path)
                    })
            }
            None => false,
        });
        if let Some(sel) = self.panel {
            let lines = self.panel_lines(s).len();
            self.panel = (lines > 0).then(|| sel.min(lines - 1));
        }
    }

    fn compute_rows(&self, s: &Session) -> Vec<GameRef> {
        let Some(snap) = s.snapshot() else {
            return Vec::new();
        };
        let q = self.pane.needle();
        let f = self.pane.filter();
        let mut rows: Vec<(String, String, GameRef)> = Vec::new();
        if let Some(l) = &snap.library {
            for (i, g) in l.games.iter().enumerate() {
                let keep = match f {
                    GameFilter::All | GameFilter::InLibrary => true,
                    GameFilter::WithSaves => !self.saves.of(GameRef::Library(i)).is_empty(),
                    GameFilter::OnCard => g.on_card(),
                    GameFilter::NotOnCard => {
                        g.matches.iter().all(|m| m.presence == Presence::Local)
                    }
                    GameFilter::CardOnly => false,
                };
                if keep && matches(q.as_deref(), &[&g.game.name, &g.game.platform]) {
                    rows.push((
                        g.game.platform.to_lowercase(),
                        g.game.name.to_lowercase(),
                        GameRef::Library(i),
                    ));
                }
            }
        }
        if !matches!(f, GameFilter::InLibrary | GameFilter::NotOnCard)
            && let Some(card) = &snap.card
        {
            for (i, file) in card.files.iter().enumerate() {
                let (o, presence) = (&file.rom, file.presence);
                if presence == Presence::Both
                    || (f == GameFilter::CardOnly && presence != Presence::Card)
                    || (f == GameFilter::WithSaves
                        && self.saves.of(GameRef::CardOnly(i)).is_empty())
                {
                    continue;
                }
                if matches(q.as_deref(), &[&o.name, &o.platform]) {
                    rows.push((
                        o.platform.to_lowercase(),
                        o.name.to_lowercase(),
                        GameRef::CardOnly(i),
                    ));
                }
            }
        }
        rows.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
        rows.into_iter().map(|(_, _, r)| r).collect()
    }

    /// A click on a row leaves the saves panel.
    pub fn leave_panel(&mut self) {
        self.panel = None;
    }

    // -- keys ---------------------------------------------------------------

    /// Keys while the saves panel has focus; `false` lets the key through.
    pub(super) fn panel_key(&mut self, key: KeyEvent, cx: &mut Cx) -> bool {
        let Some(sel) = self.panel else {
            return false;
        };
        let count = self.panel_lines(cx.session).len();
        match key.code {
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') => self.panel = None,
            KeyCode::Down | KeyCode::Char('j') => {
                self.panel = Some((sel + 1).min(count.saturating_sub(1)))
            }
            KeyCode::Up | KeyCode::Char('k') => self.panel = Some(sel.saturating_sub(1)),
            KeyCode::Char('b') => self.back_up_selected(cx),
            KeyCode::Char('p') => {
                if let Some(line) = self.panel_line(cx.session) {
                    let games = self.saves.names(cx.session);
                    saves::restore(cx, &line, games);
                }
            }
            KeyCode::Char('x') => {
                if let Some(line) = self.panel_line(cx.session) {
                    saves::trash(cx, &line);
                }
            }
            KeyCode::Char('q' | '?' | 'r' | 'B') | KeyCode::Tab | KeyCode::BackTab => return false,
            KeyCode::Char(c) if c.is_ascii_digit() => return false,
            _ => {}
        }
        true
    }

    /// The tab's own keys (the table keys are handled by the app).
    pub(super) fn key(&mut self, key: KeyEvent, cx: &mut Cx) {
        match key.code {
            KeyCode::Enter | KeyCode::Right => self.open_panel(cx),
            KeyCode::Char('b') => self.back_up_selected(cx),
            KeyCode::Char('B') => {
                let games = self.saves.names(cx.session);
                saves::back_up(cx, None, games);
            }
            KeyCode::Char('a') => import::start(cx),
            KeyCode::Char('i') => self.import_from_card(cx, false),
            KeyCode::Char('I') => self.import_from_card(cx, true),
            KeyCode::Char('n') => init_library(cx),
            KeyCode::Char('s') => self.copy_to_card(cx),
            KeyCode::Char('e') => rename::start(cx, self.selected()),
            KeyCode::Char('t') => rename::standardize(cx),
            KeyCode::Char('d') => cx.session.identify(),
            KeyCode::Char('f') => self.fetch_cover(cx),
            KeyCode::Char('c') => self.choose_cover(cx),
            KeyCode::Char('x') => self.remove(cx),
            _ => {}
        }
    }

    /// Enter / → on a game: focus its saves panel.
    fn open_panel(&mut self, cx: &mut Cx) {
        let lines = self.panel_lines(cx.session);
        if lines.is_empty() {
            cx.session.notice("No saves for this game yet.");
            return;
        }
        // Start on the first copy rather than the file heading.
        self.panel = Some(usize::from(lines.len() > 1));
    }

    /// `b`: back up the selected game's saves only.
    fn back_up_selected(&mut self, cx: &mut Cx) {
        let Some(row) = self.selected() else {
            return;
        };
        let only = self.saves.of(row).to_vec();
        if only.is_empty() {
            cx.session.notice("This game has no saves.");
            return;
        }
        let games = self.saves.names(cx.session);
        saves::back_up(cx, Some(only), games);
    }

    /// `s`: copy the selected library game to the card.
    fn copy_to_card(&self, cx: &mut Cx) {
        if cx.session.require_card().is_none() {
            return;
        }
        let Some(game) = self.selected_game(cx.session) else {
            cx.session.notice("Select a local game to copy.");
            return;
        };
        let platform = Some(game.game.platform.clone());
        let roms: Vec<String> = game.game.roms.iter().map(|r| r.id.to_string()).collect();
        cx.session.plan("Planning copy", move |ctx, progress| {
            let plan = to_card::plan(
                &ctx.library()?,
                &ctx.pocket()?,
                platform.as_deref(),
                &roms,
                progress,
            )?;
            Ok(pending::copy(plan))
        });
    }

    /// `i` / `I`: copy the selected card-only ROM (or all of them) into the library.
    fn import_from_card(&self, cx: &mut Cx, all: bool) {
        if cx.session.require_card().is_none() || cx.session.require_library().is_none() {
            return;
        }
        if cx.session.card().is_none_or(|c| c.files.is_empty()) {
            cx.session
                .notice("Every ROM on the card is already in the library.");
            return;
        }
        let request = if all {
            ImportRequest::default()
        } else {
            match self.selected_card_rom(cx.session) {
                Some(o) => ImportRequest {
                    platform: Some(o.platform.clone()),
                    paths: vec![o.rel_path.clone()],
                    ..Default::default()
                },
                None => {
                    cx.session.notice(
                        "Select a row whose WHERE says \"card\" and press i, or I imports all of them.",
                    );
                    return;
                }
            }
        };
        cx.session.plan("Planning import", move |ctx, progress| {
            let plan = card_cmd::plan_import(ctx, &request, progress)?;
            Ok((Import::Card(plan)).preview())
        });
    }

    /// `x`: a library game goes to the library's trash; a card-only ROM is
    /// deleted from the card.
    fn remove(&self, cx: &mut Cx) {
        if let Some(o) = self.selected_card_rom(cx.session).cloned() {
            cx.session.plan("Planning", move |ctx, _| {
                Ok(pending::delete(roms::plan_delete(
                    &ctx.pocket()?,
                    &o.rel_path,
                )?))
            });
            return;
        }
        let Some(game) = self.selected_game(cx.session) else {
            return;
        };
        let platform = game.game.platform.clone();
        let specs: Vec<String> = game.game.roms.iter().map(|r| r.id.to_string()).collect();
        cx.session.plan("Planning", move |ctx, _| {
            let plan = remove::plan(&ctx.library()?, Some(&platform), &specs)?;
            Ok(pending::remove(plan))
        });
    }

    // -- covers -------------------------------------------------------------

    /// `c`: use an image file as the selected game's cover.
    fn choose_cover(&self, cx: &mut Cx) {
        let Some(game) = self.selected_game(cx.session) else {
            cx.session.notice(
                "Select a library game first; import a card-only ROM with i before adding its cover.",
            );
            return;
        };
        cx.dialog.input(
            format!("Import cover: {}", game.game.name),
            vec![
                "Choose a PNG, JPEG or WebP image to copy into the library.".into(),
                "Tab completes paths. Replacement is confirmed before saving.".into(),
            ],
            start_path(),
            Step::Cover(CoverStep::Image {
                game_id: game.game.id,
            }),
        );
    }

    /// `f`: download the selected game's cover. The fetch fills every
    /// missing cover of its platform; missing covers are also fetched on
    /// start and after imports, so this is mostly a retry.
    fn fetch_cover(&self, cx: &mut Cx) {
        let Some(game) = self.selected_game(cx.session) else {
            let line = if self.selected_card_rom(cx.session).is_some() {
                "Not in the library yet: import it first (i); covers live in the library."
            } else {
                "No game selected."
            };
            cx.session.notice(line);
            return;
        };
        if game.cover.is_some() {
            cx.session.notice("This game already has a cover.");
            return;
        }
        let platform = game.game.platform.clone();
        if catalog::thumbnail_system(&platform).is_none() {
            cx.dialog.input(
                format!("libretro system for {platform}"),
                vec![
                    "No built-in mapping for this platform. Give the libretro-thumbnails".into(),
                    "repository name, e.g. \"Nintendo - Game Boy Advance\".".into(),
                ],
                String::new(),
                Step::Cover(CoverStep::System { platform }),
            );
            return;
        }
        cx.session.fetch_covers(Some(platform), None);
    }
}

/// `n`: create the library at the configured path, then reload.
fn init_library(cx: &mut Cx) {
    if cx.session.library().is_some() {
        cx.session.notice("There already is a library here.");
        return;
    }
    match library_ops::init(&cx.session.ctx()) {
        Ok(created) => {
            cx.session
                .notice(format!("✓ created library at {}", created.root.display()));
            cx.session.reload();
        }
        Err(e) => cx.dialog.error("Can't create the library", &e),
    }
}

/// The prompts a cover asks.
pub enum CoverStep {
    /// The image file for a game.
    Image { game_id: i64 },
    /// The libretro system name of a platform without a built-in mapping.
    System { platform: String },
}

pub fn answer(cx: &mut Cx, step: CoverStep, answer: Answer) {
    let Answer::Text(value) = answer else {
        return;
    };
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    match step {
        CoverStep::Image { game_id } => {
            let source = PathBuf::from(expand_home(value));
            cx.session.plan("Checking cover image", move |ctx, _| {
                Ok(pending::cover(cover_import::plan(
                    &ctx.library()?,
                    game_id,
                    &source,
                )?))
            })
        }
        CoverStep::System { platform } => cx
            .session
            .fetch_covers(Some(platform), Some(value.to_string())),
    }
}
