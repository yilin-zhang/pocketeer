//! Where things are on screen: the draggable pane dividers, and the areas
//! the last frame drew, so a click can be mapped back to what is under it.
//! `ui` records the areas while drawing; the mouse handler asks `hit`.

use ratatui::layout::Rect;

use super::{Tab, pane::Cycle};

/// A pane divider the mouse can drag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Splitter {
    /// Table | details, on the current tab.
    Cols,
    /// Cover / details on the Games tab.
    GamesRows,
}

/// What a click landed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    Tab(Tab),
    /// The `/ query` part of the filter bar.
    Query,
    /// A "what to show" segment, by filter index.
    Segment(usize),
    /// A table row, by row index.
    Row(usize),
    /// A divider: which one and the area it divides.
    Splitter(Splitter, Rect),
}

pub struct Layout {
    /// Left pane width per tab, as a percentage, indexed by `Tab::index`.
    cols: [u16; 5],
    /// Cover height on the Games tab, as a percentage.
    games_rows: u16,
    dragging: Option<(Splitter, Rect)>,
    drawn: Drawn,
}

/// The areas of the last frame.
#[derive(Default)]
struct Drawn {
    /// Dialog buttons, and whether each is the primary (apply / OK) one.
    buttons: Vec<(Rect, bool)>,
    splitters: Vec<(Rect, Rect, Splitter)>,
    tabs: Rect,
    query: Rect,
    segments: Vec<(Rect, usize)>,
    /// Rows of the current table (header excluded), and the index of the
    /// first visible row.
    rows: Rect,
    offset: usize,
}

impl Default for Layout {
    fn default() -> Self {
        Layout {
            cols: [62, 62, 58, 62, 50],
            games_rows: 60,
            dragging: None,
            drawn: Drawn::default(),
        }
    }
}

fn inside(r: Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
}

impl Layout {
    /// Where a divider sits, as a percentage of the area it divides.
    pub fn split(&self, which: Splitter, tab: Tab) -> u16 {
        match which {
            Splitter::Cols => self.cols[tab.index()],
            Splitter::GamesRows => self.games_rows,
        }
    }

    // -- recorded while drawing ---------------------------------------------

    /// Forgets the last frame's areas; called before drawing a new one.
    pub fn begin_frame(&mut self) {
        self.drawn = Drawn::default();
    }

    pub fn record_tabs(&mut self, area: Rect) {
        self.drawn.tabs = area;
    }

    pub fn record_filter_bar(&mut self, query: Rect, segments: Vec<(Rect, usize)>) {
        self.drawn.query = query;
        self.drawn.segments = segments;
    }

    pub fn record_table(&mut self, rows: Rect, offset: usize) {
        self.drawn.rows = rows;
        self.drawn.offset = offset;
    }

    pub fn record_splitter(&mut self, grab: Rect, area: Rect, which: Splitter) {
        self.drawn.splitters.push((grab, area, which));
    }

    pub fn record_buttons(&mut self, buttons: Vec<(Rect, bool)>) {
        self.drawn.buttons = buttons;
    }

    #[cfg(test)]
    pub fn button(&self, i: usize) -> Option<Rect> {
        self.drawn.buttons.get(i).map(|(r, _)| *r)
    }

    // -- asked by the mouse handler -----------------------------------------

    /// The dialog button under the pointer: whether it is the primary one.
    pub fn button_at(&self, x: u16, y: u16) -> Option<bool> {
        self.drawn
            .buttons
            .iter()
            .find(|(r, _)| inside(*r, x, y))
            .map(|(_, primary)| *primary)
    }

    /// What is under the pointer outside a dialog.
    pub fn hit(&self, x: u16, y: u16) -> Option<Hit> {
        let d = &self.drawn;
        if let Some((_, area, which)) = d.splitters.iter().find(|(r, _, _)| inside(*r, x, y)) {
            return Some(Hit::Splitter(*which, *area));
        }
        if inside(d.tabs, x, y) {
            // Tabs render as " title " cells with a two-cell gap.
            let mut cursor = d.tabs.x;
            for tab in Tab::ALL.iter().copied() {
                let width = tab.label().chars().count() as u16 + 2;
                if x >= cursor && x < cursor + width {
                    return Some(Hit::Tab(tab));
                }
                cursor += width + 2;
            }
            return None;
        }
        if inside(d.query, x, y) {
            return Some(Hit::Query);
        }
        if let Some((_, i)) = d.segments.iter().find(|(r, _)| inside(*r, x, y)) {
            return Some(Hit::Segment(*i));
        }
        if inside(d.rows, x, y) {
            return Some(Hit::Row((y - d.rows.y) as usize + d.offset));
        }
        None
    }

    pub fn start_drag(&mut self, which: Splitter, area: Rect) {
        self.dragging = Some((which, area));
    }

    /// Moves the divider being dragged to the pointer.
    pub fn drag(&mut self, x: u16, y: u16, tab: Tab) {
        let Some((which, area)) = self.dragging else {
            return;
        };
        let (pos, start, len) = match which {
            Splitter::Cols => (x, area.x, area.width),
            Splitter::GamesRows => (y, area.y, area.height),
        };
        if len == 0 {
            return;
        }
        let pct = ((pos.saturating_sub(start) as u32 * 100 / len as u32) as u16).clamp(15, 85);
        match which {
            Splitter::Cols => self.cols[tab.index()] = pct,
            Splitter::GamesRows => self.games_rows = pct,
        }
    }

    pub fn end_drag(&mut self) {
        self.dragging = None;
    }
}
