//! Cartridge saves on the Games tab: which saves belong to which game, the
//! SAVE column, the lines of a game's saves panel, and the save actions
//! (back up, put back on AP, trash, and the question a conflict asks).
//! The data comes from the side worker after every load; nothing here
//! touches the disk.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
};

use pocketeer_core::transfer::{
    Presence,
    saves::{
        self, ConflictChoice, SaveAction, SaveReason, SaveSkip, SavesOptions, Scope, TrashTarget,
        expected_saves,
    },
};

use super::{
    Cx,
    dialog::{Answer, Step as DialogStep},
    games::GameRef,
    session::Session,
};
use crate::{
    ops::saves as saves_cmd,
    text,
    tui::{
        pending::{self, SaveGames, SaveSync},
        worker::SavesView,
    },
};

/// How a game's saves stand, worst first: what the SAVE column shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SaveMark {
    /// Changed on both sides; a backup asks which copy wins.
    Conflict,
    /// AP has a save the library doesn't have yet.
    CardNewer,
    /// Backed up and the same on AP; how many older versions there are.
    BackedUp(usize),
    /// Only the library has it (no card, or not on this card).
    LibraryOnly,
}

/// One line of a game's saves panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveLine {
    /// A save file; the lines under it are its copies.
    File(PathBuf),
    /// The copy on AP.
    Card(PathBuf),
    /// The library's current backup.
    Backup(PathBuf),
    /// An older version in the library.
    Version(PathBuf, i64),
}

impl SaveLine {
    pub fn rel(&self) -> &Path {
        match self {
            SaveLine::File(p)
            | SaveLine::Card(p)
            | SaveLine::Backup(p)
            | SaveLine::Version(p, _) => p,
        }
    }
}

/// The saves of each Games row, by card path.
#[derive(Default)]
pub struct SaveIndex {
    by_row: HashMap<GameRef, Vec<PathBuf>>,
}

impl SaveIndex {
    /// Joins saves to the Games rows: a stored link first, otherwise by
    /// name (the ROM's save folder and stem).
    pub fn build(session: &Session) -> Self {
        let mut by_key: HashMap<_, GameRef> = HashMap::new();
        let mut by_id: HashMap<i64, GameRef> = HashMap::new();
        if let Some(l) = session.library() {
            for (i, g) in l.games.iter().enumerate() {
                by_id.insert(g.game.id, GameRef::Library(i));
                let copies: Vec<PathBuf> = g
                    .matches
                    .iter()
                    .flat_map(|m| m.copies.iter().map(|c| c.path.clone()))
                    .collect();
                for key in expected_saves(&g.game, &copies) {
                    by_key.entry(key).or_insert(GameRef::Library(i));
                }
            }
        }
        if let Some(card) = session.card() {
            for (i, file) in card.files.iter().enumerate() {
                if file.presence == Presence::Both {
                    continue;
                }
                if let Some(key) = saves::rom_key(&file.rom.rel_path) {
                    by_key.entry(key).or_insert(GameRef::CardOnly(i));
                }
            }
        }
        let view = session.saves();
        let mut by_row: HashMap<GameRef, Vec<PathBuf>> = HashMap::new();
        for rel in view.saves.keys() {
            let owner = match view.owners.get(rel) {
                Some(id) => by_id.get(id).copied(),
                None => saves::save_key(rel).and_then(|k| by_key.get(&k).copied()),
            };
            if let Some(owner) = owner {
                by_row.entry(owner).or_default().push(rel.clone());
            }
        }
        SaveIndex { by_row }
    }

    /// The saves of a Games row.
    pub fn of(&self, row: GameRef) -> &[PathBuf] {
        self.by_row.get(&row).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn mark(&self, row: GameRef, view: &SavesView) -> Option<SaveMark> {
        self.of(row)
            .iter()
            .filter_map(|rel| view.saves.get(rel))
            .map(|s| match &s.action {
                SaveAction::Conflict { .. } => SaveMark::Conflict,
                SaveAction::Pull { .. } => SaveMark::CardNewer,
                SaveAction::Skip {
                    reason: SaveSkip::UpToDate,
                    ..
                } => SaveMark::BackedUp(s.versions.len()),
                _ => SaveMark::LibraryOnly,
            })
            .reduce(|a, b| match (a, b) {
                (SaveMark::BackedUp(x), SaveMark::BackedUp(y)) => SaveMark::BackedUp(x + y),
                (a, b) => a.min(b),
            })
    }

    /// A row's saves panel: each save, then its copy on AP, its backup and
    /// its older versions.
    pub fn lines(&self, row: GameRef, view: &SavesView) -> Vec<SaveLine> {
        let mut lines = Vec::new();
        for rel in self.of(row) {
            let Some(s) = view.saves.get(rel) else {
                continue;
            };
            lines.push(SaveLine::File(rel.clone()));
            if s.card.is_some() {
                lines.push(SaveLine::Card(rel.clone()));
            }
            if s.backup.is_some() {
                lines.push(SaveLine::Backup(rel.clone()));
            }
            for v in &s.versions {
                lines.push(SaveLine::Version(rel.clone(), v.id));
            }
        }
        lines
    }

    /// Every save the index knows the game of, named by that game (for the
    /// backup dialog).
    pub fn names(&self, session: &Session) -> SaveGames {
        let mut names = BTreeMap::new();
        for (row, rels) in &self.by_row {
            if let Some(name) = row.name(session) {
                for rel in rels {
                    names.insert(rel.clone(), name.clone());
                }
            }
        }
        names
    }
}

/// How a save stands, for the saves panel.
pub fn status(action: &SaveAction) -> &'static str {
    match action {
        SaveAction::Pull {
            reason: SaveReason::OnlyOnCard,
            ..
        } => "only on AP, not backed up",
        SaveAction::Pull { .. } => "AP is newer than the backup",
        SaveAction::Conflict { .. } => "changed on both sides",
        SaveAction::Skip {
            reason: SaveSkip::UpToDate,
            ..
        } => "backed up",
        SaveAction::Skip { .. } | SaveAction::Push { .. } => "only in the library",
    }
}

// ---------------------------------------------------------------------------
// actions

/// Back up cartridge saves from AP: `only` these, or all of them.
pub fn back_up(cx: &mut Cx, only: Option<Vec<PathBuf>>, games: SaveGames) {
    if cx.session.require_card().is_none() || cx.session.require_library().is_none() {
        return;
    }
    let opts = SavesOptions {
        scope: Scope::Saves,
        only,
    };
    cx.session.plan("Comparing saves", move |ctx, progress| {
        let plan = saves_cmd::plan_sync(ctx, opts, progress)?;
        Ok(pending::preview(SaveSync { plan, games }))
    });
}

/// Put a backup or an older version back on AP.
pub fn restore(cx: &mut Cx, line: &SaveLine, games: SaveGames) {
    let version = match line {
        SaveLine::Card(_) => {
            cx.session
                .notice("That is the copy on AP; pick a backup or an older version.");
            return;
        }
        SaveLine::Version(_, id) => Some(*id),
        SaveLine::File(rel) | SaveLine::Backup(rel) => {
            if cx
                .session
                .saves()
                .saves
                .get(rel)
                .is_none_or(|s| s.backup.is_none())
            {
                cx.session.notice(
                    "The library has no current backup of this save; pick an older version.",
                );
                return;
            }
            None
        }
    };
    if cx.session.require_card().is_none() || cx.session.require_library().is_none() {
        return;
    }
    let rel = line.rel().to_path_buf();
    cx.session.plan("Planning restore", move |ctx, _| {
        let plan = saves::plan_restore(&ctx.library()?, &ctx.pocket()?, &rel, version)?;
        Ok(pending::preview(SaveSync { plan, games }))
    });
}

/// Throw away one copy (a file heading means everything the library has of it).
pub fn trash(cx: &mut Cx, line: &SaveLine) {
    let target = match line {
        SaveLine::Card(_) => {
            if cx.session.require_card().is_none() {
                return;
            }
            TrashTarget::Card
        }
        SaveLine::Backup(_) => TrashTarget::Backup,
        SaveLine::Version(_, id) => TrashTarget::Version(*id),
        SaveLine::File(_) => TrashTarget::Library,
    };
    if cx.session.require_library().is_none() {
        return;
    }
    let rels = vec![line.rel().to_path_buf()];
    cx.session.plan("Planning", move |ctx, _| {
        Ok(pending::preview(saves_cmd::plan_trash(ctx, &rels, target)?))
    });
}

// ---------------------------------------------------------------------------
// conflicts

/// Where a backup with conflicts is: which conflict is being asked about.
pub enum Step {
    Conflict { backup: Box<SaveSync>, index: usize },
}

/// The first conflict in `plan`: which copy wins. The answer re-plans the rest.
pub fn ask_conflict(cx: &mut Cx, backup: Box<SaveSync>, index: usize) {
    let Some(SaveAction::Conflict {
        rel,
        card,
        computer,
        ..
    }) = backup.plan.action(index)
    else {
        return;
    };
    let file = rel.file_name().unwrap_or_default().to_string_lossy();
    let what = match backup.games.get(rel) {
        Some(game) if !file.starts_with(game.as_str()) => format!("{game}: {file}"),
        Some(_) => file.into_owned(),
        None => rel.display().to_string(),
    };
    let items = vec![
        format!(
            "Use AP → backup ({}; {})",
            text::bytes(card.size),
            text::time_ago(card.mtime)
        ),
        format!(
            "Use computer → restore AP ({}; {})",
            text::bytes(computer.size),
            text::time_ago(computer.mtime)
        ),
        "Leave both unchanged".into(),
    ];
    cx.dialog.pick(
        format!("Save conflict: {what}"),
        items,
        2,
        DialogStep::Saves(Step::Conflict { backup, index }),
    );
}

pub fn answer(cx: &mut Cx, step: Step, answer: Answer) {
    let (Step::Conflict { mut backup, index }, Answer::Choice(choice)) = (step, answer) else {
        return;
    };
    backup.plan.resolve(
        index,
        match choice {
            0 => ConflictChoice::Card,
            1 => ConflictChoice::Computer,
            _ => ConflictChoice::Skip,
        },
    );
    cx.planned(*backup);
}
