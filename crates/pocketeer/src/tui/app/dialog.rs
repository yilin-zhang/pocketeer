//! The one dialog that can be open: confirmations, messages, text prompts
//! (with live path suggestions), pick-one lists and checklists. The dialog
//! only collects an answer. Each prompt carries the `Step` of the flow
//! that asked, and the answer goes back to that flow (`Submitted`).

use crossterm::event::{KeyCode, KeyEvent};
use pocketeer_core::Error;

use super::{
    games, import,
    line_edit::LineEdit,
    memories,
    path_input::{PathCompletion, PathLookup},
    rename, saves,
};
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
        pending: Box<dyn Pending>,
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
        /// Path prompts only: what the typed path matches.
        paths: Option<PathCompletion>,
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
    Apply(Box<dyn Pending>),
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

    pub fn confirm(
        &mut self,
        title: impl Into<String>,
        lines: Vec<String>,
        pending: Box<dyn Pending>,
    ) {
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
            paths: step.completes_paths().then(PathCompletion::default),
            step,
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
            paths: Some(paths),
            ..
        }) = &mut self.modal
            && value.as_str() == result.value
        {
            complete = paths.take(result);
        }
        if complete {
            self.key(KeyEvent::from(KeyCode::Tab));
        }
    }

    /// Asks for the suggestions of the prompt's current text.
    fn refresh_paths(&mut self) {
        if let Some(Modal::Input {
            value,
            paths: Some(paths),
            ..
        }) = &mut self.modal
        {
            paths.reset();
            self.paths.request(value.as_str().to_string());
        }
    }

    // -- keys and buttons ---------------------------------------------------

    /// A key while the dialog is open; `Some` when it was answered.
    pub fn key(&mut self, key: KeyEvent) -> Option<Submitted> {
        let modal = self.modal.as_mut()?;
        match modal {
            Modal::Input { value, paths, .. } => match (key.code, paths) {
                (KeyCode::Esc, Some(p)) if p.highlight.is_some() => p.highlight = None,
                (KeyCode::Esc, _) => self.close(),
                (KeyCode::Down, Some(p)) => p.down(),
                (KeyCode::Up, Some(p)) => p.up(),
                (KeyCode::Tab, Some(p)) => {
                    if let Some(text) = p.tab() {
                        value.set(text);
                        self.refresh_paths();
                    }
                }
                (KeyCode::Enter, Some(p)) if p.highlight.is_some() => {
                    if let Some(text) = p.tab() {
                        value.set(text);
                    }
                    self.refresh_paths();
                }
                (KeyCode::Enter, _) => return self.press(true),
                _ => {
                    if value.key(key) == Some(true) {
                        self.refresh_paths();
                    }
                }
            },
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
        if let Some(Modal::Input {
            paths: Some(paths), ..
        }) = &mut self.modal
        {
            paths.suggestions = list;
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
            Some(Modal::Input { paths, .. }) => paths.as_ref().and_then(|p| p.highlight),
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
