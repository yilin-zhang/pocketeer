//! Renaming, as a flow of prompts: which ROM (for a game with several),
//! which copy (the library's, one on AP, or adopting the other copy's
//! name), then the new name, then the plan. Standard (No-Intro) names are
//! renames too: a checklist of suggestions, then the plan.

use std::path::PathBuf;

use pocketeer_core::{library::rename::RenamePlan, names};

use super::{
    Cx,
    dialog::{Answer, Step as DialogStep},
    games::GameRef,
};
use crate::tui::{
    pending::{Pending, Request},
    worker::Job,
};

/// Where a rename is.
pub enum Step {
    /// Which ROM of a multi-ROM game.
    Rom { platform: String, roms: Vec<String> },
    /// Which copy, or which name to adopt.
    Copy(Vec<Choice>),
    /// The new name of a library ROM.
    LibraryName { platform: String, spec: String },
    /// The new name of a ROM on AP.
    CardName { path: PathBuf },
    /// Standard names to take; items align with `plans`.
    Standardize { plans: Vec<RenamePlan> },
}

/// One answer to "rename which copy?".
pub enum Choice {
    Library {
        platform: String,
        name: String,
    },
    Card {
        path: PathBuf,
        name: String,
    },
    /// The library takes the name the copy on AP has.
    AdoptCardName {
        platform: String,
        spec: String,
        new_name: String,
    },
    /// The copy on AP takes the library's name.
    AdoptLibraryName {
        path: PathBuf,
        new_name: String,
    },
}

/// `e` on the Games tab.
pub fn start(cx: &mut Cx, row: Option<GameRef>) {
    let Some(row) = row else {
        return;
    };
    if let Some(rom) = row.card_rom(cx.session).cloned() {
        choose(
            cx,
            Choice::Card {
                path: rom.rel_path,
                name: rom.name,
            },
        );
        return;
    }
    let Some(game) = row.game(cx.session) else {
        return;
    };
    let Some(rom) = game.game.roms.first() else {
        return;
    };
    let platform = game.game.platform.clone();
    if game.game.roms.len() > 1 {
        let roms: Vec<String> = game.game.roms.iter().map(|r| r.name.clone()).collect();
        let title = format!("{} has {} ROMs; which one?", game.game.name, roms.len());
        cx.dialog.pick(
            title,
            roms.clone(),
            0,
            DialogStep::Rename(Step::Rom { platform, roms }),
        );
        return;
    }
    let name = rom.name.clone();
    choose_copy(cx, platform, name);
}

/// Rename which copy? Only in the library: nothing to choose.
fn choose_copy(cx: &mut Cx, platform: String, name: String) {
    let copies = cx
        .session
        .library()
        .into_iter()
        .flat_map(|l| &l.games)
        .filter(|g| g.game.platform == platform)
        .flat_map(|g| &g.matches)
        .find(|m| m.name == name)
        .map(|m| m.copies.clone())
        .unwrap_or_default();
    if copies.is_empty() {
        choose(cx, Choice::Library { platform, name });
        return;
    }
    let several = copies.len() > 1;
    let mut items = vec!["In the library".to_string()];
    let mut choices = vec![Choice::Library {
        platform: platform.clone(),
        name: name.clone(),
    }];
    for copy in copies {
        let on_ap = if several {
            format!("On AP ({})", copy.path.display())
        } else {
            "On AP".to_string()
        };
        items.push(on_ap.clone());
        choices.push(Choice::Card {
            path: copy.path.clone(),
            name: copy.name.clone(),
        });
        if copy.name_mismatch {
            items.push(format!("{on_ap}: use the library's name"));
            choices.push(Choice::AdoptLibraryName {
                path: copy.path.clone(),
                new_name: name.clone(),
            });
            items.push(format!(
                "In the library: use the name on AP ({})",
                copy.name
            ));
            choices.push(Choice::AdoptCardName {
                platform: platform.clone(),
                spec: name.clone(),
                new_name: copy.name,
            });
        }
    }
    cx.dialog.pick(
        "Rename which copy?",
        items,
        0,
        DialogStep::Rename(Step::Copy(choices)),
    );
}

fn choose(cx: &mut Cx, choice: Choice) {
    match choice {
        Choice::Library { platform, name } => {
            let single = cx
                .session
                .library()
                .into_iter()
                .flat_map(|l| &l.games)
                .find(|g| g.game.platform == platform && g.game.roms.iter().any(|r| r.name == name))
                .is_some_and(|g| g.game.roms.len() == 1);
            cx.dialog.input(
                format!("Rename {name}"),
                vec![if single {
                    "Leave the extension out to keep it.".into()
                } else {
                    "Leave the extension out to keep it. The game keeps its title.".into()
                }],
                names::stem(&name).to_string(),
                DialogStep::Rename(Step::LibraryName {
                    platform,
                    spec: name,
                }),
            );
        }
        Choice::Card { path, name } => cx.dialog.input(
            format!("Rename {name} on AP"),
            vec!["Leave the extension out to keep it.".into()],
            names::stem(&name).to_string(),
            DialogStep::Rename(Step::CardName { path }),
        ),
        Choice::AdoptCardName {
            platform,
            spec,
            new_name,
        } => cx.session.submit(
            "Planning local rename",
            Job::Plan(Request::Rename {
                platform,
                spec,
                new_name,
            }),
        ),
        Choice::AdoptLibraryName { path, new_name } => cx.session.submit(
            "Planning AP rename",
            Job::Plan(Request::CardRename { path, new_name }),
        ),
    }
}

/// `t`: offer standard (No-Intro) names for games whose file names aren't.
pub fn standardize(cx: &mut Cx) {
    if cx.session.require_library().is_some() {
        cx.session
            .submit("Matching standard names", Job::Plan(Request::Standardize));
    }
}

/// Standard names were found: a checklist.
pub fn offer_standard_names(cx: &mut Cx, plans: Vec<RenamePlan>) {
    let items = plans
        .iter()
        .map(|p| format!("[{}] {}  →  {}", p.platform, p.old_name, p.new_name))
        .collect();
    cx.dialog.toggle(
        "Rename to standard names?",
        items,
        DialogStep::Rename(Step::Standardize { plans }),
    );
}

pub fn answer(cx: &mut Cx, step: Step, answer: Answer) {
    match (step, answer) {
        (Step::Rom { platform, roms }, Answer::Choice(i)) => {
            if let Some(name) = roms.into_iter().nth(i) {
                choose_copy(cx, platform, name);
            }
        }
        (Step::Copy(choices), Answer::Choice(i)) => {
            if let Some(choice) = choices.into_iter().nth(i) {
                choose(cx, choice);
            }
        }
        (Step::LibraryName { platform, spec }, Answer::Text(new_name)) => {
            if !new_name.trim().is_empty() {
                cx.session.submit(
                    "Planning rename",
                    Job::Plan(Request::Rename {
                        platform,
                        spec,
                        new_name,
                    }),
                );
            }
        }
        (Step::CardName { path }, Answer::Text(new_name)) => cx.session.submit(
            "Planning AP rename",
            Job::Plan(Request::CardRename { path, new_name }),
        ),
        (Step::Standardize { plans }, Answer::Checked(on)) => {
            let chosen: Vec<RenamePlan> = plans
                .into_iter()
                .zip(on)
                .filter(|(_, on)| *on)
                .map(|(p, _)| p)
                .collect();
            if chosen.is_empty() {
                return;
            }
            let lines = chosen
                .iter()
                .map(|p| format!("  [{}] {}  →  {}", p.platform, p.old_name, p.new_name))
                .collect();
            cx.dialog.confirm(
                format!("Rename {} game(s) to standard names?", chosen.len()),
                lines,
                Pending::Standardize(chosen),
            );
        }
        _ => {}
    }
}
