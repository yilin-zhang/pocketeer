//! The one dialog that can be open: confirmations, messages, text prompts
//! (with live path suggestions), pick-one lists and checklists. The dialog
//! only collects an answer. Each prompt carries the `Step` of the flow
//! that asked, and the answer goes back to that flow (`Submitted`).

use crossterm::event::{KeyCode, KeyEvent};
use pocketeer_core::Error;

use super::{games, import, line_edit::LineEdit, memories, path_input::PathLookup, rename, saves};
use crate::tui::pending::Pending;

pub use super::path_input::Suggestion;

/// How many path suggestions a prompt lists.
pub const SUGGESTIONS_SHOWN: usize = 10;

pub enum Modal {
    /// A plan with Apply / Cancel buttons.
    Confirm {
        title: String,
        lines: Vec<String>,
        scroll: u16,
        /// Which button has keyboard focus: Apply (true) or Cancel.
        focus_apply: bool,
        pending: Pending,
    },
    /// Something to read (errors, reports, explanations).
    Info {
        title: String,
        lines: Vec<String>,
        scroll: u16,
    },
    /// A one-line text prompt; Enter submits, Esc cancels.
    Input {
        title: String,
        lines: Vec<String>,
        value: LineEdit,
        step: Step,
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
        step: Step,
    },
    /// Check any number of items; Space toggles, Enter applies.
    Toggle {
        title: String,
        items: Vec<(String, bool)>,
        cursor: usize,
        step: Step,
    },
}

/// The flow a prompt belongs to, and where in it.
pub enum Step {
    Import(import::Step),
    Rename(rename::Step),
    Saves(saves::Step),
    Memories(memories::Step),
    Cover(games::CoverStep),
}

impl Step {
    /// This prompt takes a file-system path: it gets live suggestions and
    /// Tab completion.
    pub fn completes_paths(&self) -> bool {
        matches!(
            self,
            Step::Import(import::Step::Source) | Step::Cover(games::CoverStep::Image { .. })
        )
    }
}

/// What the user answered.
pub enum Answer {
    Text(String),
    Choice(usize),
    /// One flag per checklist item, in order.
    Checked(Vec<bool>),
}

pub enum Submitted {
    /// Apply was pressed on a plan.
    Apply(Pending),
    /// A prompt was answered.
    Answer(Step, Answer),
}

pub struct Dialog {
    modal: Option<Modal>,
    paths: PathLookup,
}

impl Dialog {
    pub fn new() -> Self {
        Dialog {
            modal: None,
            paths: PathLookup::spawn(),
        }
    }

    pub fn is_open(&self) -> bool {
        self.modal.is_some()
    }

    pub fn modal(&self) -> Option<&Modal> {
        self.modal.as_ref()
    }

    pub fn close(&mut self) {
        self.modal = None;
        self.paths.cancel();
    }

    // -- opening ------------------------------------------------------------

    pub fn confirm(&mut self, title: impl Into<String>, lines: Vec<String>, pending: Pending) {
        self.open(Modal::Confirm {
            title: title.into(),
            lines,
            scroll: 0,
            focus_apply: true,
            pending,
        });
    }

    pub fn info(&mut self, title: impl Into<String>, lines: Vec<String>) {
        self.open(Modal::Info {
            title: title.into(),
            lines,
            scroll: 0,
        });
    }

    pub fn error(&mut self, title: &str, e: &Error) {
        let info = e.to_info();
        let mut lines = vec![info.message];
        if let Some(hint) = info.hint {
            lines.push(String::new());
            lines.push(hint);
        }
        self.info(title, lines);
    }

    pub fn input(
        &mut self,
        title: impl Into<String>,
        lines: Vec<String>,
        value: String,
        step: Step,
    ) {
        self.open(Modal::Input {
            title: title.into(),
            lines,
            value: LineEdit::new(value),
            step,
            suggestions: Vec::new(),
            completion: None,
            searching: false,
            tab_pending: false,
            highlight: None,
        });
        self.refresh_paths();
    }

    pub fn pick(
        &mut self,
        title: impl Into<String>,
        items: Vec<String>,
        selected: usize,
        step: Step,
    ) {
        self.open(Modal::Pick {
            title: title.into(),
            items,
            selected,
            step,
        });
    }

    /// A checklist with every item checked.
    pub fn toggle(&mut self, title: impl Into<String>, items: Vec<String>, step: Step) {
        self.open(Modal::Toggle {
            title: title.into(),
            items: items.into_iter().map(|i| (i, true)).collect(),
            cursor: 0,
            step,
        });
    }

    fn open(&mut self, modal: Modal) {
        self.paths.cancel();
        self.modal = Some(modal);
    }

    // -- path suggestions ---------------------------------------------------

    /// Takes the latest suggestions for a path prompt, and finishes a Tab
    /// that was pressed before they arrived.
    pub fn pump(&mut self) {
        let Some(result) = self.paths.latest() else {
            return;
        };
        let mut complete = false;
        if let Some(Modal::Input {
            value,
            step,
            suggestions,
            completion,
            searching,
            tab_pending,
            highlight,
            ..
        }) = &mut self.modal
            && step.completes_paths()
            && value.as_str() == result.value
        {
            // While a folder is being read the last list stays up.
            if let Some(list) = result.suggestions {
                *suggestions = list;
                *highlight = highlight.filter(|h| *h < suggestions.len());
            }
            *completion = Some(result.completion);
            *searching = !result.finished;
            complete = std::mem::take(tab_pending);
        }
        if complete {
            self.key(KeyEvent::from(KeyCode::Tab));
        }
    }

    /// Asks for the suggestions of the prompt's current text.
    fn refresh_paths(&mut self) {
        if let Some(Modal::Input {
            value,
            step,
            highlight,
            completion,
            tab_pending,
            ..
        }) = &mut self.modal
            && step.completes_paths()
        {
            // The current list stays until the new one arrives (a few
            // milliseconds once the folder has been read), so typing
            // doesn't make it flicker.
            *highlight = None;
            *completion = None;
            *tab_pending = false;
            self.paths.request(value.as_str().to_string());
        }
    }

    // -- keys and buttons ---------------------------------------------------

    /// A key while the dialog is open; `Some` when it was answered.
    pub fn key(&mut self, key: KeyEvent) -> Option<Submitted> {
        let modal = self.modal.as_mut()?;
        match modal {
            Modal::Input {
                value,
                step,
                suggestions,
                highlight,
                completion,
                tab_pending,
                ..
            } => {
                let paths = step.completes_paths();
                let shown = suggestions.len();
                // Take a suggestion: folders keep the prompt open for the next level.
                let accept = |value: &mut LineEdit, s: &Suggestion| {
                    value.set(if s.is_dir {
                        format!("{}/", s.path)
                    } else {
                        s.path.clone()
                    });
                };
                match key.code {
                    KeyCode::Esc if highlight.is_some() => *highlight = None,
                    KeyCode::Esc => self.close(),
                    KeyCode::Down if paths && shown > 0 => {
                        *highlight = Some(highlight.map_or(0, |h| (h + 1).min(shown - 1)));
                    }
                    KeyCode::Up if paths => *highlight = highlight.and_then(|h| h.checked_sub(1)),
                    KeyCode::Tab if paths => {
                        match highlight.and_then(|h| suggestions.get(h)) {
                            Some(s) => accept(value, s),
                            None if shown == 1 => accept(value, &suggestions[0]),
                            None => {
                                let Some(completed) = completion.as_ref() else {
                                    *tab_pending = true;
                                    return None;
                                };
                                value.set(completed.clone());
                            }
                        }
                        self.refresh_paths();
                    }
                    KeyCode::Enter if paths && highlight.is_some() => {
                        if let Some(s) = highlight.and_then(|h| suggestions.get(h)) {
                            accept(value, s);
                        }
                        self.refresh_paths();
                    }
                    KeyCode::Enter => return self.press(true),
                    _ => {
                        if value.key(key) == Some(true) {
                            self.refresh_paths();
                        }
                    }
                }
            }
            Modal::Pick {
                items, selected, ..
            } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.close(),
                KeyCode::Down | KeyCode::Char('j') => {
                    *selected = (*selected + 1).min(items.len().saturating_sub(1))
                }
                KeyCode::Up | KeyCode::Char('k') => *selected = selected.saturating_sub(1),
                KeyCode::Enter => return self.press(true),
                _ => {}
            },
            Modal::Toggle { items, cursor, .. } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.close(),
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
                KeyCode::Enter => return self.press(true),
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
                    return self.press(apply);
                }
                KeyCode::Char('y') => return self.press(true),
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => self.close(),
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
                | KeyCode::Char('?') => self.close(),
                KeyCode::Down | KeyCode::Char('j') => *scroll = scroll.saturating_add(1),
                KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
                KeyCode::PageDown => *scroll = scroll.saturating_add(10),
                KeyCode::PageUp => *scroll = scroll.saturating_sub(10),
                _ => {}
            },
        }
        None
    }

    /// A button was pressed (keyboard or mouse): the primary one applies or
    /// submits, the other closes the dialog.
    pub fn press(&mut self, primary: bool) -> Option<Submitted> {
        let modal = self.modal.take();
        self.paths.cancel();
        if !primary {
            return None;
        }
        match modal? {
            Modal::Confirm { pending, .. } => Some(Submitted::Apply(pending)),
            Modal::Input { value, step, .. } => Some(Submitted::Answer(
                step,
                Answer::Text(value.as_str().to_string()),
            )),
            Modal::Pick { step, selected, .. } => {
                Some(Submitted::Answer(step, Answer::Choice(selected)))
            }
            Modal::Toggle { step, items, .. } => Some(Submitted::Answer(
                step,
                Answer::Checked(items.into_iter().map(|(_, on)| on).collect()),
            )),
            Modal::Info { .. } => None,
        }
    }

    #[cfg(test)]
    pub fn set_suggestions(&mut self, list: Vec<Suggestion>) {
        if let Some(Modal::Input { suggestions, .. }) = &mut self.modal {
            *suggestions = list;
        }
    }

    #[cfg(test)]
    pub fn text(&self) -> Option<&str> {
        match &self.modal {
            Some(Modal::Input { value, .. }) => Some(value.as_str()),
            _ => None,
        }
    }

    #[cfg(test)]
    pub fn highlight(&self) -> Option<usize> {
        match &self.modal {
            Some(Modal::Input { highlight, .. }) => *highlight,
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn path_prompt(dialog: &mut Dialog, value: String) {
        dialog.input("Import", vec![], value, Step::Import(import::Step::Source));
    }

    fn suggestions(n: usize, prefix: &str) -> Vec<Suggestion> {
        (0..n)
            .map(|i| Suggestion {
                path: format!("/{prefix}{i}"),
                name: format!("{prefix}{i}"),
                is_dir: false,
                matched: None,
            })
            .collect()
    }

    #[test]
    fn tab_pressed_before_results_arrive_is_applied_when_ready() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("roms")).unwrap();
        let mut dialog = Dialog::new();
        path_prompt(&mut dialog, format!("{}/ro", tmp.path().display()));
        // Results only land in `pump`, so right now the lookup is pending
        // and Tab has to wait for it.
        dialog.key(KeyEvent::from(KeyCode::Tab));
        assert!(matches!(
            dialog.modal(),
            Some(Modal::Input {
                tab_pending: true,
                ..
            })
        ));
        let expected = format!("{}/roms/", tmp.path().display());
        let deadline = Instant::now() + Duration::from_secs(5);
        while dialog.text() != Some(expected.as_str()) {
            assert!(Instant::now() < deadline, "Tab was lost");
            std::thread::sleep(Duration::from_millis(5));
            dialog.pump();
        }
    }

    #[test]
    fn every_suggestion_can_be_picked_even_past_the_visible_rows() {
        let mut dialog = Dialog::new();
        path_prompt(&mut dialog, "/".into());
        dialog.set_suggestions(suggestions(20, "image"));
        for _ in 0..15 {
            dialog.key(KeyEvent::from(KeyCode::Down));
        }
        dialog.key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(dialog.text(), Some("/image14"));
        // With a row picked, Esc drops the pick first; then it closes.
        dialog.key(KeyEvent::from(KeyCode::Down));
        dialog.key(KeyEvent::from(KeyCode::Esc));
        assert!(dialog.is_open() && dialog.highlight().is_none());
        dialog.key(KeyEvent::from(KeyCode::Esc));
        assert!(!dialog.is_open());
    }

    #[test]
    fn answers_go_back_with_the_step_that_asked() {
        let mut dialog = Dialog::new();
        dialog.toggle(
            "Rename?",
            vec!["a".into(), "b".into()],
            Step::Rename(rename::Step::Standardize { plans: Vec::new() }),
        );
        dialog.key(KeyEvent::from(KeyCode::Down));
        dialog.key(KeyEvent::from(KeyCode::Char(' ')));
        let Some(Submitted::Answer(Step::Rename(_), Answer::Checked(on))) =
            dialog.key(KeyEvent::from(KeyCode::Enter))
        else {
            panic!("no answer");
        };
        assert_eq!(on, [true, false]);
        assert!(!dialog.is_open());
        // Cancel closes without an answer.
        dialog.pick(
            "Which?",
            vec!["x".into()],
            0,
            Step::Memories(memories::Step::Trash {
                rel: "Memories/x.sta".into(),
            }),
        );
        assert!(dialog.press(false).is_none());
        assert!(!dialog.is_open());
    }
}
