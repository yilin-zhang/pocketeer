//! One filterable table: the state every list tab shares. A tab has a
//! "what to show" filter (a small enum cycled with `l` or clicked), a
//! free-text query typed after `/`, the rows that survive both, and a
//! selection. The tab recomputes the rows when the data, the filter or the
//! query changes, never per frame.

use crossterm::event::{KeyCode, KeyEvent};

use super::line_edit::LineEdit;

/// A fixed set of values shown as clickable segments (filters, tabs).
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
    filter: F,
    query: LineEdit,
    /// Keys go to the query instead of the table.
    editing: bool,
    rows: Vec<R>,
    sel: usize,
}

impl<F: Cycle, R> Pane<F, R> {
    pub fn new(filter: F) -> Self {
        Pane {
            filter,
            query: LineEdit::default(),
            editing: false,
            rows: Vec::new(),
            sel: 0,
        }
    }

    pub fn filter(&self) -> F {
        self.filter
    }

    pub fn set_filter(&mut self, filter: F) {
        self.filter = filter;
        self.sel = 0;
    }

    pub fn rows(&self) -> &[R] {
        &self.rows
    }

    pub fn sel(&self) -> usize {
        self.sel
    }

    pub fn selected(&self) -> Option<&R> {
        self.rows.get(self.sel)
    }

    /// New rows; the selection stays where it was, clamped, unless
    /// `select` finds the row to stay on.
    pub fn set_rows(&mut self, rows: Vec<R>, select: impl Fn(&R) -> bool) {
        self.rows = rows;
        match self.rows.iter().position(select) {
            Some(i) => self.sel = i,
            None => self.sel = self.sel.min(self.rows.len().saturating_sub(1)),
        }
    }

    /// The query, ready for case-insensitive matching (`None` when empty).
    pub fn needle(&self) -> Option<String> {
        let q = self.query.as_str().trim().to_lowercase();
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

/// What the key and mouse handlers and the filter bar need from the current
/// tab's table without knowing its row type.
pub trait PaneOps {
    fn len(&self) -> usize;
    fn select(&mut self, i: usize);
    fn move_sel(&mut self, delta: i32);
    fn editing(&self) -> bool;
    fn start_editing(&mut self);
    /// The query, with a cursor while it is being typed.
    fn query_text(&self) -> String;
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

    fn select(&mut self, i: usize) {
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

    fn start_editing(&mut self) {
        self.editing = true;
    }

    fn query_text(&self) -> String {
        if self.editing {
            let (before, after) = self.query.split();
            format!("{before}▌{after}")
        } else {
            self.query.as_str().to_string()
        }
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
            KeyCode::Down => {
                self.move_sel(1);
                false
            }
            KeyCode::Up => {
                self.move_sel(-1);
                false
            }
            _ => {
                let changed = self.query.key(key) == Some(true);
                if changed {
                    self.sel = 0;
                }
                changed
            }
        }
    }

    fn cycle_filter(&mut self) {
        self.set_filter(self.filter.next());
    }

    fn set_filter_index(&mut self, i: usize) {
        if let Some(f) = F::ALL.get(i) {
            self.set_filter(*f);
        }
    }

    fn filter_index(&self) -> usize {
        self.filter.index()
    }

    fn filter_labels(&self) -> Vec<&'static str> {
        F::ALL.iter().map(|f| f.label()).collect()
    }
}
