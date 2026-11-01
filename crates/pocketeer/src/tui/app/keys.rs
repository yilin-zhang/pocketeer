//! Keyboard and mouse: tab keys, table keys, and what each dialog does with
//! a key or a click.

use super::*;

/// How many path suggestions a prompt lists.
pub const SUGGESTIONS_SHOWN: usize = 10;

impl App {
    // -----------------------------------------------------------------------
    // keys and mouse

    pub fn key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        if self.modal.is_some() {
            self.modal_key(key);
            return;
        }
        if let Some(pane) = self.pane_mut()
            && pane.editing()
        {
            if pane.edit_key(key) {
                self.rebuild_rows();
            }
            return;
        }
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('?') => self.info("Help", text_lines(HELP)),
            KeyCode::Tab => self.tab = self.tab.next(),
            KeyCode::BackTab => self.tab = self.tab.prev(),
            KeyCode::Char('1') => self.tab = Tab::Cores,
            KeyCode::Char('2') => self.tab = Tab::Bios,
            KeyCode::Char('3') => self.tab = Tab::Games,
            KeyCode::Char('4') => self.tab = Tab::Device,
            KeyCode::Char('r') if self.busy.is_none() => self.reload(),
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
                    self.rebuild_rows();
                }
            }
            KeyCode::Esc => {
                if let Some(pane) = self.pane_mut()
                    && pane.clear_query()
                {
                    self.rebuild_rows();
                }
            }
            _ if self.busy.is_some() => {
                let busy = self.busy.clone().unwrap_or_default();
                self.notice(format!("⟳ {busy} — wait for it to finish."));
            }
            // Cores
            KeyCode::Enter | KeyCode::Char('i') | KeyCode::Char('u') if self.tab == Tab::Cores => {
                self.install_or_update_selected()
            }
            KeyCode::Char('U') if self.tab == Tab::Cores => self.update_all(),
            KeyCode::Char('x') if self.tab == Tab::Cores => self.remove_selected(),
            // BIOS
            KeyCode::Char('p') if self.tab == Tab::Bios => self.place(),
            // Games
            KeyCode::Char('e') if self.tab == Tab::Games => self.rename_selected(),
            KeyCode::Char('x') if self.tab == Tab::Games => self.remove_selected_game(),
            KeyCode::Char('i') if self.tab == Tab::Games => self.import_orphans(false),
            KeyCode::Char('I') if self.tab == Tab::Games => self.import_orphans(true),
            KeyCode::Char('n') if self.tab == Tab::Games => self.init_library(),
            KeyCode::Char('a') if self.tab == Tab::Games => self.import_folder(),
            KeyCode::Char('f') if self.tab == Tab::Games => self.fetch_covers(false),
            KeyCode::Char('F') if self.tab == Tab::Games => self.fetch_covers(true),
            KeyCode::Char('C') if self.tab == Tab::Games => self.fetch_all_covers(),
            KeyCode::Char('t') if self.tab == Tab::Games => self.standardize(),
            // Games + Device
            KeyCode::Char('S') if self.tab == Tab::Games => self.copy_selected(),
            KeyCode::Char('V') if matches!(self.tab, Tab::Games | Tab::Device) => self.plan_saves(),
            // Device
            KeyCode::Char('c') if self.tab == Tab::Device => self.clean(),
            _ => {}
        }
    }

    /// `/`: type into the current tab's filter.
    fn start_search(&mut self) {
        // Searching cores is for finding ones you don't have yet.
        if self.tab == Tab::Cores && self.cores.filter == CoreFilter::Installed {
            self.cores.filter = CoreFilter::All;
            self.rebuild_rows();
        }
        if let Some(pane) = self.pane_mut() {
            pane.set_editing(true);
        }
    }

    fn move_sel(&mut self, delta: i32) {
        if let Some(pane) = self.pane_mut() {
            pane.move_sel(delta);
        }
    }

    /// Mouse: click a tab, a filter segment, the query, or a row; wheel to move.
    pub fn mouse(&mut self, m: MouseEvent) {
        let (x, y) = (m.column, m.row);
        let inside = |r: Rect| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height;
        match m.kind {
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let delta = if m.kind == MouseEventKind::ScrollDown {
                    1
                } else {
                    -1
                };
                if self.modal.is_some() {
                    let code = if delta > 0 {
                        KeyCode::Down
                    } else {
                        KeyCode::Up
                    };
                    self.modal_key(KeyEvent::from(code));
                } else {
                    self.move_sel(delta);
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some((which, span)) = self.dragging {
                    let (pos, start, len) = match which {
                        Splitter::Cols => (x, span.x, span.width),
                        Splitter::GamesRows | Splitter::DeviceRows => (y, span.y, span.height),
                    };
                    if len > 0 {
                        let pct = (pos.saturating_sub(start) as u32 * 100 / len as u32) as u16;
                        self.splits.set(which, self.tab, pct);
                    }
                }
            }
            MouseEventKind::Up(_) => self.dragging = None,
            MouseEventKind::Down(MouseButton::Left) => {
                if self.modal.is_some() {
                    let hit = self
                        .hit
                        .buttons
                        .iter()
                        .find(|(r, _)| inside(*r))
                        .map(|(_, primary)| *primary);
                    if let Some(primary) = hit {
                        self.modal_activate(primary);
                    }
                    return;
                }
                if let Some((_, span, which)) =
                    self.hit.splitters.iter().find(|(r, _, _)| inside(*r))
                {
                    self.dragging = Some((*which, *span));
                } else if inside(self.hit.tabs) {
                    // Tabs render as " title " cells with a two-cell gap.
                    let mut cursor = self.hit.tabs.x;
                    for tab in Tab::ALL.iter().copied() {
                        let width = tab.label().chars().count() as u16 + 2;
                        if x >= cursor && x < cursor + width {
                            self.tab = tab;
                            return;
                        }
                        cursor += width + 2;
                    }
                } else if inside(self.hit.query) {
                    self.start_search();
                } else if let Some((_, i)) = self.hit.segments.iter().find(|(r, _)| inside(*r)) {
                    let i = *i;
                    if let Some(pane) = self.pane_mut() {
                        pane.set_filter_index(i);
                        self.rebuild_rows();
                    }
                } else if inside(self.hit.rows) {
                    let row = (y - self.hit.rows.y) as usize + self.hit.offset;
                    if let Some(pane) = self.pane_mut() {
                        pane.set_sel(row);
                    }
                }
            }
            _ => {}
        }
    }

    fn modal_key(&mut self, key: KeyEvent) {
        let Some(modal) = self.modal.as_mut() else {
            return;
        };
        match modal {
            Modal::Input {
                value,
                kind,
                suggestions,
                highlight,
                ..
            } => {
                let paths = kind.completes_paths();
                let shown = suggestions.len().min(SUGGESTIONS_SHOWN);
                // Take a suggestion: folders keep the prompt open for the next level.
                let accept = |value: &mut String, s: &Suggestion| {
                    *value = s.path.clone();
                    if s.is_dir {
                        value.push('/');
                    }
                };
                match key.code {
                    KeyCode::Esc if highlight.is_some() => *highlight = None,
                    KeyCode::Esc => self.modal = None,
                    KeyCode::Backspace => {
                        value.pop();
                        self.refresh_input();
                    }
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        value.clear();
                        self.refresh_input();
                    }
                    KeyCode::Down if paths && shown > 0 => {
                        *highlight = Some(highlight.map_or(0, |h| (h + 1).min(shown - 1)));
                    }
                    KeyCode::Up if paths => {
                        *highlight = highlight.and_then(|h| h.checked_sub(1));
                    }
                    KeyCode::Tab if paths => {
                        match highlight.and_then(|h| suggestions.get(h)) {
                            Some(s) => accept(value, s),
                            None if suggestions.len() == 1 => accept(value, &suggestions[0]),
                            None => *value = dialogs::complete_path(value),
                        }
                        self.refresh_input();
                    }
                    KeyCode::Enter if paths && highlight.is_some() => {
                        if let Some(s) = highlight.and_then(|h| suggestions.get(h)) {
                            accept(value, s);
                        }
                        self.refresh_input();
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        value.push(c);
                        self.refresh_input();
                    }
                    KeyCode::Enter => self.modal_activate(true),
                    _ => {}
                }
            }
            Modal::Pick {
                items, selected, ..
            } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.modal = None,
                KeyCode::Down | KeyCode::Char('j') => {
                    *selected = (*selected + 1).min(items.len().saturating_sub(1))
                }
                KeyCode::Up | KeyCode::Char('k') => *selected = selected.saturating_sub(1),
                KeyCode::Enter => self.modal_activate(true),
                _ => {}
            },
            Modal::Toggle { items, cursor, .. } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.modal = None,
                KeyCode::Down | KeyCode::Char('j') => {
                    *cursor = (*cursor + 1).min(items.len().saturating_sub(1))
                }
                KeyCode::Up | KeyCode::Char('k') => *cursor = cursor.saturating_sub(1),
                KeyCode::Char(' ') => {
                    if let Some(item) = items.get_mut(*cursor) {
                        item.1 = !item.1;
                    }
                }
                KeyCode::Char('a') => {
                    let all_on = items.iter().all(|(_, on)| *on);
                    for item in items.iter_mut() {
                        item.1 = !all_on;
                    }
                }
                KeyCode::Enter => self.modal_activate(true),
                _ => {}
            },
            Modal::Confirm {
                focus_apply,
                scroll,
                ..
            } => match key.code {
                KeyCode::Left
                | KeyCode::Right
                | KeyCode::Tab
                | KeyCode::BackTab
                | KeyCode::Char('h')
                | KeyCode::Char('l') => *focus_apply = !*focus_apply,
                KeyCode::Enter => {
                    let apply = *focus_apply;
                    self.modal_activate(apply);
                }
                KeyCode::Char('y') => self.modal_activate(true),
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => self.modal = None,
                KeyCode::Down | KeyCode::Char('j') => *scroll = scroll.saturating_add(1),
                KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
                KeyCode::PageDown => *scroll = scroll.saturating_add(10),
                KeyCode::PageUp => *scroll = scroll.saturating_sub(10),
                _ => {}
            },
            Modal::Info { scroll, .. } => match key.code {
                KeyCode::Esc
                | KeyCode::Enter
                | KeyCode::Char('q')
                | KeyCode::Char('n')
                | KeyCode::Char('?') => self.modal = None,
                KeyCode::Down | KeyCode::Char('j') => *scroll = scroll.saturating_add(1),
                KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
                KeyCode::PageDown => *scroll = scroll.saturating_add(10),
                KeyCode::PageUp => *scroll = scroll.saturating_sub(10),
                _ => {}
            },
        }
    }

    /// A dialog button was pressed (keyboard or mouse): the primary one
    /// applies / submits, the other closes the dialog.
    pub fn modal_activate(&mut self, primary: bool) {
        if !primary {
            self.modal = None;
            return;
        }
        match self.modal.take() {
            Some(Modal::Confirm { pending, .. }) => {
                if pending.is_import() {
                    // New games: show each as it lands, fetch covers after the reload.
                    self.importing = true;
                    self.auto_covers_done = false;
                }
                self.submit("Applying", Job::Apply(pending));
            }
            Some(Modal::Input { value, kind, .. }) => self.submit_input(kind, value),
            Some(Modal::Pick { kind, selected, .. }) => self.submit_pick(kind, selected),
            Some(Modal::Toggle { kind, items, .. }) => self.submit_toggle(kind, items),
            Some(Modal::Info { .. }) | None => {}
        }
    }
}
