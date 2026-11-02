//! What the TUI asks the worker to do on the user's behalf, and the plan it
//! gets back: one operation = one `Request` (how to plan it), one `Pending`
//! variant (how to preview and apply it). Adding an operation means adding
//! a variant here and a key on the tab that offers it.

use std::{
    collections::{BTreeMap, HashSet},
    path::PathBuf,
};

use pocketeer_core::{
    Result,
    card::bios::PlacePlan,
    card::clean::{self, CleanPlan},
    card::install::{Action, Prepared, UninstallPlan, plan::RemoveReason},
    library::{
        covers,
        import::{ImportPlan, apply_suggestions},
        remove::RemovePlan as LibraryRemovePlan,
        rename::RenamePlan,
    },
    plan::{Event, Plan},
    transfer::{
        from_card::CardImportPlan,
        saves::{self, SaveAction, SavesOptions, SavesPlan, TrashPlan, TrashTarget},
        to_card::CopyPlan,
    },
};

use super::worker::{Msg, applied, failed};
use crate::{
    ops::{
        Applied, Mode, Staged, apply_actions, card as card_cmd,
        card::ImportRequest as CardImportRequest, context::Context, cores as cores_cmd,
        library as library_cmd, memories as memories_cmd, saves as saves_cmd,
    },
    text,
};

/// Something to plan on the worker; the plan comes back as a [`Pending`]
/// for the user to confirm.
pub enum Request {
    ImportCover {
        game_id: i64,
        source: PathBuf,
    },
    CardRename {
        path: PathBuf,
        new_name: String,
    },
    Clean,
    Copy {
        platform: Option<String>,
        roms: Vec<String>,
    },
    /// Back up saves; `games` names the game each save belongs to.
    Saves {
        opts: SavesOptions,
        games: SaveGames,
    },
    /// Put a save (its current backup, or an older version) back on AP.
    Restore {
        rel: PathBuf,
        version: Option<i64>,
        games: SaveGames,
    },
    /// Put a Memory and the files beside it back on AP.
    RestoreMemory {
        rel: PathBuf,
    },
    /// Throw away copies of a save.
    Trash {
        rels: Vec<PathBuf>,
        target: TrashTarget,
    },
    /// Throw away a Memory, on AP (`card`) or in the library.
    TrashMemory {
        rel: PathBuf,
        card: bool,
    },
    /// Import a folder (or one ROM / zip) from this computer, each file's
    /// platform taken from its extension; `platform` takes the rest.
    ImportFolder {
        source: PathBuf,
        platform: Option<String>,
        known: Vec<String>,
    },
    Standardize,
    Rename {
        platform: String,
        spec: String,
        new_name: String,
    },
    CardImport(CardImportRequest),
    /// Rewrite an import plan with the standard names the user accepted,
    /// as (platform, ROM name) pairs.
    AcceptNames {
        import: Box<Import>,
        accepted: HashSet<(String, String)>,
    },
}

/// Card path of a save → the name of the game it belongs to.
pub type SaveGames = BTreeMap<PathBuf, String>;

/// A plan: shown in a dialog, then applied once confirmed.
pub enum Pending {
    ImportCover(pocketeer_core::library::covers::import::ImportPlan),
    CardRename(pocketeer_core::card::rename::CardRenamePlan),
    /// Cores to install, plus the requested ones that couldn't be prepared.
    Install {
        preps: Vec<Prepared>,
        errors: Vec<String>,
    },
    Remove(Vec<UninstallPlan>),
    Place(PlacePlan),
    Clean(CleanPlan),
    /// A ROM only the card has, deleted from it.
    DeleteCardRom {
        name: String,
        path: PathBuf,
    },
    Copy(CopyPlan),
    Saves(Box<SavesPlan>, SaveGames),
    Trash(Box<TrashPlan>),
    Rename(Box<RenamePlan>),
    /// Renames of existing games to standard names.
    Standardize(Vec<RenamePlan>),
    LibraryRemove(Box<LibraryRemovePlan>),
    Import(Box<Import>),
}

/// ROMs to copy into the library: one plan per platform.
pub enum Import {
    /// From a folder on this computer, with the files whose platform the
    /// extensions don't say.
    Folder(library_cmd::SortedImport),
    /// From the card.
    Card(CardImportPlan),
}

impl Import {
    fn parts(&self) -> &[ImportPlan] {
        match self {
            Import::Folder(f) => &f.parts,
            Import::Card(c) => &c.parts,
        }
    }

    fn parts_mut(&mut self) -> &mut [ImportPlan] {
        match self {
            Import::Folder(f) => &mut f.parts,
            Import::Card(c) => &mut c.parts,
        }
    }

    /// ROMs the plans add.
    fn changes(&self) -> usize {
        self.parts().iter().map(Plan::changes).sum()
    }

    /// Every (platform, current name, suggested standard name) in the plans.
    pub fn suggestions(&self) -> Vec<(String, String, String)> {
        self.parts()
            .iter()
            .flat_map(|p| {
                p.suggestions()
                    .map(|(old, new)| (p.platform.clone(), old.to_string(), new.to_string()))
            })
            .collect()
    }
}

impl Pending {
    /// Imports add games one by one; the Games tab refreshes after each.
    pub fn is_import(&self) -> bool {
        matches!(self, Pending::Import(_))
    }

    /// Runs the planning half of `request` on the worker, and says which
    /// dialog the plan opens.
    pub fn plan(
        ctx: &Context,
        request: Request,
        progress: &mut dyn FnMut(Event),
    ) -> Result<Preview> {
        Ok(match request {
            Request::ImportCover { game_id, source } => {
                Pending::ImportCover(covers::import::plan(&ctx.library()?, game_id, &source)?)
            }
            Request::CardRename { path, new_name } => {
                Pending::CardRename(card_cmd::plan_rename(ctx, &path, &new_name)?)
            }
            Request::Clean => {
                progress(Event::Status("Scanning for junk".into()));
                Pending::Clean(clean::plan(&ctx.pocket()?, false)?)
            }
            Request::Copy { platform, roms } => Pending::Copy(card_cmd::plan_copy(
                ctx,
                platform.as_deref(),
                &roms,
                progress,
            )?),
            Request::Saves { opts, games } => {
                Pending::Saves(Box::new(saves_cmd::plan_sync(ctx, opts, progress)?), games)
            }
            Request::Restore {
                rel,
                version,
                games,
            } => Pending::Saves(
                Box::new(saves::plan_restore(
                    &ctx.library()?,
                    &ctx.pocket()?,
                    &rel,
                    version,
                )?),
                games,
            ),
            Request::RestoreMemory { rel } => {
                let rels = memories_cmd::library_files(ctx, &rel)?;
                let plan = saves::plan_restore_paths(&ctx.library()?, &ctx.pocket()?, &rels)?;
                Pending::Saves(Box::new(plan), SaveGames::new())
            }
            Request::Trash { rels, target } => {
                Pending::Trash(Box::new(saves_cmd::plan_trash(ctx, &rels, target)?))
            }
            Request::TrashMemory { rel, card } => {
                let rels = memories_cmd::files(ctx, &rel, card)?;
                let target = if card {
                    TrashTarget::Card
                } else {
                    TrashTarget::Library
                };
                Pending::Trash(Box::new(saves_cmd::plan_trash(ctx, &rels, target)?))
            }
            Request::ImportFolder {
                source,
                platform,
                known,
            } => {
                let sorted = library_cmd::plan_import_sorted(
                    ctx,
                    &source,
                    &known,
                    platform.as_deref(),
                    false,
                    progress,
                )?;
                if sorted.parts.is_empty() && !sorted.unknown.is_empty() {
                    // None of the files says its platform: ask which.
                    return Ok(Preview::AskPlatform {
                        source,
                        files: sorted.unknown.len(),
                    });
                }
                Pending::Import(Box::new(Import::Folder(sorted)))
            }
            Request::Standardize => Pending::Standardize(library_cmd::plan_standardize(ctx, None)?),
            Request::Rename {
                platform,
                spec,
                new_name,
            } => Pending::Rename(Box::new(library_cmd::plan_rename(
                ctx,
                Some(&platform),
                &spec,
                &new_name,
            )?)),
            Request::CardImport(args) => {
                let (plan, _not_found) = card_cmd::plan_import(ctx, &args, progress)?;
                Pending::Import(Box::new(Import::Card(plan)))
            }
            Request::AcceptNames {
                mut import,
                accepted,
            } => {
                let lib = ctx.library()?;
                for plan in import.parts_mut() {
                    let platform = plan.platform.clone();
                    apply_suggestions(&lib, plan, |name| {
                        accepted.contains(&(platform.clone(), name.to_string()))
                    })?;
                }
                Pending::Import(import)
            }
        }
        .preview())
    }

    /// Applies a confirmed plan and renders what happened with the same text
    /// the CLI prints (`Mode::Interactive`: no CLI flags in it).
    pub fn apply(self, ctx: &Context, progress: &mut dyn FnMut(Event)) -> Msg {
        match self {
            Pending::ImportCover(plan) => {
                let result = ctx
                    .library()
                    .and_then(|mut lib| covers::import::apply(&mut lib, &plan));
                shown(plan, result, |s| text::library::cover_import(&s))
            }
            Pending::CardRename(plan) => {
                let result = card_cmd::apply_rename(ctx, &plan);
                shown(plan, result, |s| text::card::rename_report(&s))
            }
            Pending::Install { preps, .. } => {
                let pocket = match ctx.pocket() {
                    Ok(p) => p,
                    Err(e) => return failed(e),
                };
                let prepared = preps
                    .into_iter()
                    .map(|p| (p.plan.core_id.clone(), Ok(p)))
                    .collect();
                let report = cores_cmd::apply_prepared(&pocket, Mode::Applied, prepared, progress);
                applied(text::cores::install_report(&report), report.failed())
            }
            Pending::Remove(plans) => {
                let pocket = match ctx.pocket() {
                    Ok(p) => p,
                    Err(e) => return failed(e),
                };
                let plans = plans
                    .into_iter()
                    .map(|p| (p.core_id.clone(), Ok(p)))
                    .collect();
                let report = cores_cmd::apply_removes(&pocket, Mode::Applied, plans, progress);
                applied(text::cores::remove_report(&report), report.failed())
            }
            Pending::Place(plan) => {
                let result = ctx
                    .pocket()
                    .and_then(|p| apply_actions(&p, "bios-place", &plan.actions, progress));
                shown(plan, result, |s| text::card::place_report(&s))
            }
            Pending::Clean(plan) => {
                let result = ctx.pocket().and_then(|p| clean::apply(&p, &plan));
                shown(plan, result, |s| text::card::clean_report(&s))
            }
            Pending::DeleteCardRom { name, path } => {
                let label = format!("delete {name}");
                let actions = [Action::Remove {
                    path,
                    reason: RemoveReason::Uninstall,
                }];
                progress(Event::Status(label.clone()));
                match ctx
                    .pocket()
                    .and_then(|p| apply_actions(&p, &label, &actions, progress))
                {
                    Ok(_) => applied(format!("✓ deleted {name} from the card"), 0),
                    Err(e) => applied(format!("✗ {label}: {e}"), 1),
                }
            }
            Pending::Copy(plan) => {
                let result = card_cmd::apply_copy(ctx, &plan, progress);
                shown(plan, result, |s| text::card::copy_report(&s))
            }
            Pending::Saves(plan, games) => {
                let result = saves_cmd::apply_sync(ctx, &plan, progress);
                shown(*plan, result, |s| {
                    text::saves::saves_sync_by_game(&s, &games)
                })
            }
            Pending::Trash(plan) => {
                let result = saves_cmd::apply_trash(ctx, &plan);
                shown(*plan, result, |s| text::saves::saves_trash(&s))
            }
            Pending::Rename(plan) => {
                progress(Event::Status(format!("Renaming {}", plan.old_name)));
                let result = library_cmd::apply_rename(ctx, &plan);
                shown(*plan, result, |s| text::library::library_rename(&s))
            }
            Pending::Standardize(renames) => {
                let result = library_cmd::apply_standardize(ctx, &renames, progress);
                shown(library_cmd::StandardizePlan { renames }, result, |s| {
                    text::library::library_standardize(&s)
                })
            }
            Pending::LibraryRemove(plan) => {
                progress(Event::Status("Removing from library".into()));
                let result = library_cmd::apply_remove(ctx, &plan);
                shown(*plan, result, |s| text::library::library_remove(&s))
            }
            Pending::Import(import) => match *import {
                Import::Card(plan) => {
                    let result = card_cmd::apply_import(ctx, &plan, progress);
                    shown(plan, result, |staged| {
                        text::card::import_report(&card_cmd::ImportReport {
                            staged,
                            not_found: Vec::new(),
                            covers: None,
                            warnings: Vec::new(),
                        })
                    })
                }
                Import::Folder(folder) => {
                    let mut lines = Vec::new();
                    let mut failed = 0;
                    for plan in folder.parts.iter().filter(|p| p.changes() > 0) {
                        match library_cmd::apply_import(ctx, plan, progress) {
                            Ok(r) => {
                                failed += r.failed.len();
                                lines.push(text::library::import_done(&plan.platform, &r));
                            }
                            Err(e) => {
                                failed += 1;
                                lines.push(format!("✗ {}: {e}", plan.platform.to_uppercase()));
                            }
                        }
                    }
                    applied(lines.join("\n"), failed)
                }
            },
        }
    }

    /// How this plan is shown before it is applied: the dialog to open,
    /// with its text rendered the same way the CLI prints it. A plan that
    /// needs a choice first (save conflicts, standard names) opens that
    /// dialog and comes back here once it is made.
    pub fn preview(self) -> Preview {
        use Preview::*;
        let confirm = |title: String, lines: Vec<String>, pending: Pending| Confirm {
            title,
            lines,
            pending: Box::new(pending),
        };
        let info = |title: &str, lines: Vec<String>| Info {
            title: title.into(),
            lines,
        };
        match self {
            Pending::ImportCover(plan) => confirm(
                if plan.previous.as_ref().is_some_and(|p| p.rel_path.is_some()) {
                    "Replace this game's cover?".into()
                } else {
                    "Import this game's cover?".into()
                },
                text_lines(&text::library::cover_import(&Staged::preview(plan.clone()))),
                Pending::ImportCover(plan),
            ),
            Pending::CardRename(plan) => confirm(
                "Rename this AP ROM and its saves?".into(),
                text_lines(&text::card::rename_report(&Staged::preview(plan.clone()))),
                Pending::CardRename(plan),
            ),
            Pending::Install { preps, errors } => {
                let mut text: Vec<String> = errors.clone();
                text.extend(
                    preps
                        .iter()
                        .map(|p| text::cores::install_plan_text(&p.plan)),
                );
                let lines = text_lines(&text.join("\n"));
                match preps.iter().filter(|p| p.plan.changed).count() {
                    0 => info("Nothing to do", lines),
                    n => confirm(
                        format!("Install {n} core(s)?"),
                        lines,
                        Pending::Install { preps, errors },
                    ),
                }
            }
            Pending::Remove(plans) => confirm(
                format!(
                    "Remove {}?",
                    plans
                        .iter()
                        .map(|p| p.core_id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                text_lines(
                    &plans
                        .iter()
                        .map(text::cores::remove_plan_text)
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                Pending::Remove(plans),
            ),
            Pending::Place(plan) => confirm(
                format!("Move {} file(s) into place?", plan.actions.len()),
                text_lines(&text::card::place_report(&Staged::preview(plan.clone()))),
                Pending::Place(plan),
            ),
            Pending::Clean(plan) if plan.entries.is_empty() => {
                Notice("No macOS junk on the card.".into())
            }
            Pending::Clean(plan) => confirm(
                format!(
                    "Delete {} junk item(s), {}?",
                    plan.entries.len(),
                    text::bytes(plan.bytes)
                ),
                text_lines(&text::card::clean_report(&Staged::preview(plan.clone()))),
                Pending::Clean(plan),
            ),
            Pending::DeleteCardRom { name, path } => confirm(
                format!("Delete {name} from the card?"),
                vec![format!("  - {}", path.display())],
                Pending::DeleteCardRom { name, path },
            ),
            Pending::Copy(plan) => {
                let lines = text_lines(&text::card::copy_report(&Staged::preview(plan.clone())));
                match plan.changes() {
                    0 => info("Already on this card", lines),
                    n => confirm(
                        format!("Copy {n} ROM(s) to this card?"),
                        lines,
                        Pending::Copy(plan),
                    ),
                }
            }
            Pending::Saves(plan, games) => {
                if let Some(index) = plan
                    .actions
                    .iter()
                    .position(|a| matches!(a, SaveAction::Conflict { .. }))
                {
                    return SaveConflict { plan, games, index };
                }
                let lines = text::saves::saves_dialog(&plan, &games);
                let s = &plan.summary;
                let title = match (s.pull, s.push) {
                    (0, 0) => return Notice("Saves are already backed up.".into()),
                    (n, 0) => format!("Back up {n} save(s)?"),
                    (0, n) => format!("Put {n} file(s) back on AP?"),
                    (a, b) => format!("Back up {a}, put {b} back on AP?"),
                };
                confirm(title, lines, Pending::Saves(plan, games))
            }
            Pending::Trash(plan) if plan.files.is_empty() => {
                Notice("Nothing to throw away.".into())
            }
            Pending::Trash(plan) => confirm(
                match plan.target {
                    TrashTarget::Card => format!("Delete {} file(s) from AP?", plan.files.len()),
                    _ => format!("Move {} file(s) to the trash?", plan.files.len()),
                },
                text::saves::trash_dialog(&plan),
                Pending::Trash(plan),
            ),
            Pending::Rename(plan) => confirm(
                format!("Rename to {}?", plan.new_name),
                text_lines(&text::library::library_rename(&Staged::preview(
                    (*plan).clone(),
                ))),
                Pending::Rename(plan),
            ),
            Pending::Standardize(plans) if plans.is_empty() => info(
                "Standard names",
                vec![
                    "Every game either has a standard name already or matches none.".into(),
                    "(Games verified against a DAT are never renamed.)".into(),
                ],
            ),
            Pending::Standardize(plans) => PickRenames(plans),
            Pending::LibraryRemove(plan) => {
                let games: std::collections::BTreeSet<&str> =
                    plan.items.iter().map(|i| i.game.as_str()).collect();
                let title = match (games.len(), games.iter().next()) {
                    (1, Some(game)) => format!("Remove {game}?"),
                    _ => format!("Remove {} games?", games.len()),
                };
                let mut lines: Vec<String> = if plan.items.len() > 1 {
                    plan.items
                        .iter()
                        .map(|i| format!("{}  {}", i.name, text::bytes(i.size)))
                        .collect()
                } else {
                    Vec::new()
                };
                lines.push("It goes to .trash/ in the library. Copies on the card stay.".into());
                confirm(title, lines, Pending::LibraryRemove(plan))
            }
            Pending::Import(import) if !import.suggestions().is_empty() => PickImportNames(import),
            Pending::Import(import) => {
                let (lines, title) = match &*import {
                    Import::Folder(f) => (
                        text::library::import_brief(&f.parts, &f.unknown),
                        format!("Import {} ROM(s)?", import.changes()),
                    ),
                    Import::Card(c) => (
                        text::library::import_brief(&c.parts, &[]),
                        format!("Copy {} ROM(s) into the library?", import.changes()),
                    ),
                };
                if import.changes() == 0 {
                    info("Nothing new to import", lines)
                } else {
                    confirm(title, lines, Pending::Import(import))
                }
            }
        }
    }
}

/// The dialog a plan opens before it is applied.
pub enum Preview {
    /// Nothing to do: one line for the status bar.
    Notice(String),
    /// Nothing to do, but worth reading.
    Info { title: String, lines: Vec<String> },
    /// Apply / Cancel.
    Confirm {
        title: String,
        lines: Vec<String>,
        pending: Box<Pending>,
    },
    /// A save changed on both sides: which copy wins (`index` into the plan).
    SaveConflict {
        plan: Box<SavesPlan>,
        games: SaveGames,
        index: usize,
    },
    /// Standard-name renames to tick.
    PickRenames(Vec<RenamePlan>),
    /// An import whose files match standard names: which names to take.
    PickImportNames(Box<Import>),
    /// An import whose files don't say their platform.
    AskPlatform { source: PathBuf, files: usize },
}

pub(super) fn text_lines(text: &str) -> Vec<String> {
    text.lines().map(str::to_owned).collect()
}

/// A plan and what applying it produced, rendered like the CLI would.
fn shown<P: Plan, R: Applied>(
    plan: P,
    result: Result<R>,
    render: impl FnOnce(Staged<P, R>) -> String,
) -> Msg {
    match result {
        Ok(r) => {
            let staged = Staged {
                mode: Mode::Interactive,
                plan,
                result: Some(r),
            };
            let failed = staged.failed();
            applied(render(staged), failed)
        }
        Err(e) => failed(e),
    }
}
