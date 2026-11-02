//! The TUI's data types: tabs, filters, the filterable table every tab
//! shares (`Pane`), table rows, dialogs and what a pending dialog carries.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Cores,
    Bios,
    Games,
    Device,
}

impl Cycle for Tab {
    const ALL: &'static [Tab] = &[Tab::Cores, Tab::Bios, Tab::Games, Tab::Device];

    fn label(self) -> &'static str {
        match self {
            Tab::Cores => "1  CORES",
            Tab::Bios => "2  BIOS",
            Tab::Games => "3  GAMES",
            Tab::Device => "4  CARD",
        }
    }
}

/// Cores tab: which cores to list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreFilter {
    Installed,
    All,
    Updates,
    Available,
}

impl Cycle for CoreFilter {
    const ALL: &'static [Self] = &[
        CoreFilter::Installed,
        CoreFilter::All,
        CoreFilter::Updates,
        CoreFilter::Available,
    ];

    fn label(self) -> &'static str {
        match self {
            CoreFilter::Installed => "installed",
            CoreFilter::All => "all",
            CoreFilter::Updates => "updates",
            CoreFilter::Available => "not installed",
        }
    }
}

/// BIOS tab: which required files to list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BiosFilter {
    All,
    Problems,
}

impl Cycle for BiosFilter {
    const ALL: &'static [Self] = &[BiosFilter::All, BiosFilter::Problems];

    fn label(self) -> &'static str {
        match self {
            BiosFilter::All => "all",
            BiosFilter::Problems => "missing or wrong",
        }
    }
}

/// Games tab: where a game has to be to show up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GameFilter {
    All,
    InLibrary,
    OnCard,
    CardOnly,
    NotOnCard,
}

impl Cycle for GameFilter {
    const ALL: &'static [Self] = &[
        GameFilter::All,
        GameFilter::InLibrary,
        GameFilter::OnCard,
        GameFilter::CardOnly,
        GameFilter::NotOnCard,
    ];

    fn label(self) -> &'static str {
        match self {
            GameFilter::All => "all",
            GameFilter::InLibrary => "in library",
            GameFilter::OnCard => "on card",
            GameFilter::CardOnly => "card only",
            GameFilter::NotOnCard => "not on card",
        }
    }
}

/// One line of the Cores table: an installed core, an inventory core, or both.
#[derive(Debug, Clone)]
pub struct CoreRow {
    pub id: String,
    pub platforms: Vec<String>,
    pub installed: Option<String>,
    pub latest: Option<String>,
    pub update: bool,
    pub in_inventory: bool,
    pub license: bool,
    pub missing_files: usize,
    pub date: Option<String>,
    pub repo: Option<String>,
    pub description: Option<String>,
}

/// Where things were drawn last frame, so clicks can be mapped back.
#[derive(Debug, Clone, Default)]
pub struct HitAreas {
    /// Dialog buttons: rect and whether it is the primary (apply/OK) one.
    pub buttons: Vec<(Rect, bool)>,
    /// Draggable pane dividers: the grab area, the area the split divides,
    /// and which split it is.
    pub splitters: Vec<(Rect, Rect, Splitter)>,
    /// The tabs row; tab titles are laid out left to right.
    pub tabs: Rect,
    /// The `/ query` part of the filter bar.
    pub query: Rect,
    /// One rect per "what to show" segment, with the filter index it selects.
    pub segments: Vec<(Rect, usize)>,
    /// Rows of the current tab's table (header excluded); `offset` is the
    /// index of the first visible row.
    pub rows: Rect,
    pub offset: usize,
}

/// A pane divider the mouse can drag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Splitter {
    /// Table | details, on the current tab.
    Cols,
    /// Cover / details on the Games tab.
    GamesRows,
    /// Junk / log on the Device tab.
    DeviceRows,
}

/// Where the dividers sit, as a percentage of the area they divide.
#[derive(Debug, Clone, Copy)]
pub struct Splits {
    /// Left pane width per tab, indexed by `Tab::index`.
    pub cols: [u16; 4],
    pub games_rows: u16,
    pub device_rows: u16,
}

impl Default for Splits {
    fn default() -> Self {
        Splits {
            cols: [62, 62, 58, 50],
            games_rows: 60,
            device_rows: 40,
        }
    }
}

impl Splits {
    pub fn get(&self, which: Splitter, tab: Tab) -> u16 {
        match which {
            Splitter::Cols => self.cols[tab.index()],
            Splitter::GamesRows => self.games_rows,
            Splitter::DeviceRows => self.device_rows,
        }
    }

    pub fn set(&mut self, which: Splitter, tab: Tab, pct: u16) {
        let pct = pct.clamp(15, 85);
        match which {
            Splitter::Cols => self.cols[tab.index()] = pct,
            Splitter::GamesRows => self.games_rows = pct,
            Splitter::DeviceRows => self.device_rows = pct,
        }
    }
}

/// One line of the Games table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GameRef {
    /// Index into `LibraryView::games`.
    Library(usize),
    /// Index into `Snapshot::card_roms`.
    CardOnly(usize),
}

pub enum Modal {
    Confirm {
        title: String,
        lines: Vec<String>,
        scroll: u16,
        /// Which button has keyboard focus: Apply (true) or Cancel.
        focus_apply: bool,
        pending: Pending,
    },
    Info {
        title: String,
        lines: Vec<String>,
        scroll: u16,
    },
    /// A one-line text prompt; Enter submits, Esc cancels.
    Input {
        title: String,
        lines: Vec<String>,
        value: String,
        kind: InputKind,
        /// Entries matching what is typed so far (path prompts only).
        suggestions: Vec<Suggestion>,
        completion: Option<String>,
        searching: bool,
        tab_pending: bool,
        /// Which suggestion ↑/↓ moved to, if any.
        highlight: Option<usize>,
    },
    /// Choose one item.
    Pick {
        title: String,
        items: Vec<String>,
        selected: usize,
        kind: PickKind,
    },
    /// Check any number of items; Space toggles, Enter applies.
    Toggle {
        title: String,
        items: Vec<(String, bool)>,
        cursor: usize,
        kind: ToggleKind,
    },
}

pub const HELP: &str = include_str!("../../../docs/tui-help.txt");

pub enum InputKind {
    ImportCover {
        game_id: i64,
    },
    CardRename {
        path: PathBuf,
    },
    Rename {
        platform: String,
        spec: String,
    },
    /// Folder import, step 1 of 3.
    ImportDir,
    ImportPlatform {
        dir: PathBuf,
    },
    ImportDat {
        dir: PathBuf,
        platform: String,
    },
    /// libretro system name for a platform without a built-in mapping.
    CoverSystem {
        platform: String,
    },
}

impl InputKind {
    /// This prompt takes a file-system path: it gets live suggestions and
    /// Tab completion.
    pub fn completes_paths(&self) -> bool {
        matches!(
            self,
            InputKind::ImportDir | InputKind::ImportDat { .. } | InputKind::ImportCover { .. }
        )
    }
}

pub enum PickKind {
    SaveConflict {
        plan: Box<pocketeer_core::transfer::saves::SavesPlan>,
        index: usize,
    },
    RenameActions(Vec<RenameChoice>),
    /// Which ROM of a multi-ROM game to rename.
    RenameRom {
        platform: String,
        roms: Vec<String>,
    },
    /// Folder import, step 2: a known platform, or the last item to type one.
    ImportPlatform {
        dir: PathBuf,
        platforms: Vec<String>,
    },
}

pub enum ToggleKind {
    /// Standard names suggested for an import; items are "old → new".
    ImportNames {
        import: Box<Pending>,
        /// (platform, current name) per item, in item order.
        keys: Vec<(String, String)>,
    },
    /// Renames of existing games to standard names; items align with `plans`.
    Standardize { plans: Vec<RenamePlan> },
}

pub enum RenameChoice {
    LocalCustom {
        platform: String,
        name: String,
    },
    CardCustom {
        path: PathBuf,
        name: String,
    },
    LocalAdopt {
        platform: String,
        spec: String,
        new_name: String,
    },
    CardAdopt {
        path: PathBuf,
        new_name: String,
    },
}

// ---------------------------------------------------------------------------
// One filterable table: the state the Cores, BIOS and Games tabs share.
// Each tab has a "what to show" filter (a small enum the user cycles with
// `l` or clicks), a free-text query typed after `/`, the rows that survive
// both, and a selection. Rows are recomputed by the tab (`App::rebuild_rows`)
// whenever the filter or query changes, never per frame.

/// A fixed set of filter values shown as clickable segments.
pub trait Cycle: Copy + Eq + 'static {
    const ALL: &'static [Self];
    fn label(self) -> &'static str;

    fn index(self) -> usize {
        Self::ALL.iter().position(|f| *f == self).unwrap_or(0)
    }

    fn next(self) -> Self {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    fn prev(self) -> Self {
        Self::ALL[(self.index() + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

pub struct Pane<F: Cycle, R> {
    pub filter: F,
    pub query: String,
    /// Keys go to the query instead of the table.
    pub editing: bool,
    pub rows: Vec<R>,
    pub sel: usize,
}

impl<F: Cycle, R> Pane<F, R> {
    pub fn new(filter: F) -> Self {
        Pane {
            filter,
            query: String::new(),
            editing: false,
            rows: Vec::new(),
            sel: 0,
        }
    }

    pub fn selected(&self) -> Option<&R> {
        self.rows.get(self.sel)
    }

    pub fn set_rows(&mut self, rows: Vec<R>) {
        self.rows = rows;
        self.sel = self.sel.min(self.rows.len().saturating_sub(1));
    }

    /// The query, ready for case-insensitive matching (`None` when empty).
    pub fn needle(&self) -> Option<String> {
        let q = self.query.trim().to_lowercase();
        (!q.is_empty()).then_some(q)
    }
}

/// `true` when any field contains the needle (or there is no needle).
pub fn matches(needle: Option<&str>, fields: &[&str]) -> bool {
    match needle {
        None => true,
        Some(q) => fields.iter().any(|f| f.to_lowercase().contains(q)),
    }
}

/// The parts of a pane the key and mouse handlers need without knowing the
/// row type.
pub trait PaneOps {
    fn len(&self) -> usize;
    fn set_sel(&mut self, i: usize);
    fn move_sel(&mut self, delta: i32);
    fn editing(&self) -> bool;
    fn set_editing(&mut self, on: bool);
    fn query(&self) -> &str;
    /// Drops the query; `true` when there was one.
    fn clear_query(&mut self) -> bool;
    /// A key while the query has focus; `true` when the rows must be recomputed.
    fn edit_key(&mut self, key: KeyEvent) -> bool;
    fn cycle_filter(&mut self);
    fn set_filter_index(&mut self, i: usize);
    fn filter_index(&self) -> usize;
    fn filter_labels(&self) -> Vec<&'static str>;
}

impl<F: Cycle, R> PaneOps for Pane<F, R> {
    fn len(&self) -> usize {
        self.rows.len()
    }

    fn set_sel(&mut self, i: usize) {
        if i < self.rows.len() {
            self.sel = i;
        }
    }

    fn move_sel(&mut self, delta: i32) {
        if self.rows.is_empty() {
            self.sel = 0;
            return;
        }
        let max = self.rows.len() as i64 - 1;
        self.sel = (self.sel as i64 + delta as i64).clamp(0, max) as usize;
    }

    fn editing(&self) -> bool {
        self.editing
    }

    fn set_editing(&mut self, on: bool) {
        self.editing = on;
    }

    fn query(&self) -> &str {
        &self.query
    }

    fn clear_query(&mut self) -> bool {
        let had = !self.query.is_empty();
        self.query.clear();
        self.editing = false;
        had
    }

    fn edit_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => {
                self.editing = false;
                false
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.sel = 0;
                true
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.query.clear();
                self.sel = 0;
                true
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.query.push(c);
                self.sel = 0;
                true
            }
            KeyCode::Down => {
                self.move_sel(1);
                false
            }
            KeyCode::Up => {
                self.move_sel(-1);
                false
            }
            _ => false,
        }
    }

    fn cycle_filter(&mut self) {
        self.filter = self.filter.next();
        self.sel = 0;
    }

    fn set_filter_index(&mut self, i: usize) {
        if let Some(f) = F::ALL.get(i) {
            self.filter = *f;
            self.sel = 0;
        }
    }

    fn filter_index(&self) -> usize {
        self.filter.index()
    }

    fn filter_labels(&self) -> Vec<&'static str> {
        F::ALL.iter().map(|f| f.label()).collect()
    }
}
