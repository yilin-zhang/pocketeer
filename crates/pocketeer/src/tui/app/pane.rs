//! One filterable table: the state the Cores, BIOS and Games tabs share.
//!
//! Each tab has a "what to show" filter (a small enum the user cycles with
//! `l` or clicks), a free-text query typed after `/`, the rows that survive
//! both, and a selection. Rows are recomputed by the tab (`App::rebuild_rows`)
//! whenever the filter or query changes, never per frame.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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
