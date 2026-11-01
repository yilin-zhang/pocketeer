//! The TUI's data types: tabs, filters, table rows, dialogs and what a
//! pending dialog carries.

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
        matches!(self, InputKind::ImportDir | InputKind::ImportDat { .. })
    }
}

/// One entry a path prompt offers.
#[derive(Debug, Clone)]
pub struct Suggestion {
    /// Full path, as the prompt's value becomes when accepted.
    pub path: String,
    /// Shown in the list.
    pub name: String,
    pub is_dir: bool,
}

pub enum PickKind {
    SaveConflict {
        plan: Box<pocketeer_core::library::saves::SavesPlan>,
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
