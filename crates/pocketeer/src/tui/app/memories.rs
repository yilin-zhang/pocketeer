//! The Memories tab: the save states made from the Pocket's menu, on the
//! card and in the library, and the keys that back them up, put them back
//! on AP and throw them away.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent};
use pocketeer_core::transfer::saves::{SavesOptions, Scope};

use super::{
    Cx,
    dialog::{Answer, Step as DialogStep},
    pane::{Cycle, Pane, PaneOps, matches},
    session::Session,
};
use crate::{
    ops::{
        memories::{self as memories_cmd, MemoryEntry, Place},
        saves as saves_cmd,
    },
    tui::pending::{self, SaveGames, SaveSync},
};

/// Which Memories to list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryFilter {
    All,
    OnCard,
    NotBackedUp,
    LibraryOnly,
}

impl Cycle for MemoryFilter {
    const ALL: &'static [Self] = &[
        MemoryFilter::All,
        MemoryFilter::OnCard,
        MemoryFilter::NotBackedUp,
        MemoryFilter::LibraryOnly,
    ];

    fn label(self) -> &'static str {
        match self {
            MemoryFilter::All => "all",
            MemoryFilter::OnCard => "on card",
            MemoryFilter::NotBackedUp => "not backed up",
            MemoryFilter::LibraryOnly => "library only",
        }
    }
}

pub struct Memories {
    /// Rows are indexes into the session's Memories.
    pane: Pane<MemoryFilter, usize>,
}

impl Memories {
    pub fn new() -> Self {
        Memories {
            pane: Pane::new(MemoryFilter::All),
        }
    }

    pub fn pane(&self) -> &Pane<MemoryFilter, usize> {
        &self.pane
    }

    pub fn pane_mut(&mut self) -> &mut dyn PaneOps {
        &mut self.pane
    }

    pub fn selected<'s>(&self, s: &'s Session) -> Option<&'s MemoryEntry> {
        s.saves().memories.get(*self.pane.selected()?)
    }

    pub fn rebuild(&mut self, s: &Session) {
        let q = self.pane.needle();
        let filter = self.pane.filter();
        let rows = s
            .saves()
            .memories
            .iter()
            .enumerate()
            .filter(|(_, m)| match filter {
                MemoryFilter::All => true,
                MemoryFilter::OnCard => m.place.on_card(),
                MemoryFilter::NotBackedUp => m.place.needs_backup(),
                MemoryFilter::LibraryOnly => m.place == Place::Library,
            })
            .filter(|(_, m)| {
                let h = &m.memory.header;
                matches(
                    q.as_deref(),
                    &[h.game(), h.core().unwrap_or(""), h.platform().unwrap_or("")],
                )
            })
            .map(|(i, _)| i)
            .collect();
        self.pane.set_rows(rows, |_| false);
    }

    pub(super) fn key(&mut self, key: KeyEvent, cx: &mut Cx) {
        match key.code {
            KeyCode::Char('b') => self.back_up_selected(cx),
            KeyCode::Char('B') => back_up(cx, None),
            KeyCode::Char('p') => self.restore(cx),
            KeyCode::Char('x') => self.trash(cx),
            _ => {}
        }
    }

    /// `b`: the selected Memory and the files beside it.
    fn back_up_selected(&self, cx: &mut Cx) {
        let Some(m) = self.selected(cx.session) else {
            return;
        };
        if !m.place.on_card() {
            cx.session.notice("This Memory is only in the library.");
            return;
        }
        let mut only = vec![m.memory.rel_path.clone()];
        only.extend(m.memory.companions.iter().cloned());
        back_up(cx, Some(only));
    }

    /// `p`: put the library's copy back on AP.
    fn restore(&self, cx: &mut Cx) {
        let Some(m) = self.selected(cx.session) else {
            return;
        };
        if !m.place.in_library() {
            cx.session
                .notice("The library has no copy of this Memory; b backs it up.");
            return;
        }
        let rel = m.memory.rel_path.clone();
        if cx.session.require_card().is_some() && cx.session.require_library().is_some() {
            cx.session.plan("Planning restore", move |ctx, _| {
                let plan = memories_cmd::plan_restore(ctx, &rel)?;
                Ok(pending::preview(SaveSync {
                    plan,
                    games: SaveGames::new(),
                }))
            });
        }
    }

    /// `x`: throw it away; asks which copy when there are two.
    fn trash(&self, cx: &mut Cx) {
        let Some(m) = self.selected(cx.session) else {
            return;
        };
        let rel = m.memory.rel_path.clone();
        match (m.place.on_card(), m.place.in_library()) {
            (true, true) => cx.dialog.pick(
                "Throw away which copy of this Memory?",
                vec![
                    "Delete it from AP (the library keeps its copy)".into(),
                    "Move the library's copy to its trash (AP keeps it)".into(),
                ],
                0,
                DialogStep::Memories(Step::Trash { rel }),
            ),
            (card, _) => trash(cx, rel, card),
        }
    }
}

/// Back up Memories from AP: `only` these files, or every new or changed one.
fn back_up(cx: &mut Cx, only: Option<Vec<PathBuf>>) {
    if cx.session.require_card().is_none() || cx.session.require_library().is_none() {
        return;
    }
    let opts = SavesOptions {
        scope: Scope::Memories,
        only,
    };
    cx.session.plan("Comparing Memories", move |ctx, progress| {
        let plan = saves_cmd::plan_sync(ctx, opts, progress)?;
        Ok(pending::preview(SaveSync {
            plan,
            games: SaveGames::new(),
        }))
    });
}

fn trash(cx: &mut Cx, rel: PathBuf, card: bool) {
    if card && cx.session.require_card().is_none() {
        return;
    }
    if cx.session.require_library().is_some() {
        cx.session.plan("Planning", move |ctx, _| {
            Ok(pending::preview(memories_cmd::plan_trash(ctx, &rel, card)?))
        });
    }
}

/// Which copy of a Memory to throw away.
pub enum Step {
    Trash { rel: PathBuf },
}

pub fn answer(cx: &mut Cx, step: Step, answer: Answer) {
    let (Step::Trash { rel }, Answer::Choice(i)) = (step, answer) else {
        return;
    };
    trash(cx, rel, i == 0);
}
