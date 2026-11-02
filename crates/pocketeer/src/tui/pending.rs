//! What the TUI asks the worker to do on the user's behalf, and the plan it
//! gets back: one operation = one `Request` (how to plan it), one `Pending`
//! variant (how to preview and apply it). Adding an operation means adding
//! a variant here and a key in `app/keys.rs`; nothing else.

use std::{collections::HashSet, path::PathBuf};

use pocketeer_core::{
    Result,
    card::bios::PlacePlan,
    card::clean::{self, CleanPlan},
    card::install::{Action, Prepared, UninstallPlan},
    library::{
        covers,
        import::{ImportAction, ImportOptions, ImportPlan, apply_suggestions},
        remove::RemovePlan as LibraryRemovePlan,
        rename::RenamePlan,
    },
    plan::{Event, Plan},
    transfer::{
        from_card::CardImportPlan,
        saves::{SaveAction, SavesPlan},
        to_card::CopyPlan,
    },
};

use super::worker::{Msg, applied, failed};
use crate::{
    ops::{
        Applied, Mode, Staged, apply_actions, card as card_cmd,
        card::ImportRequest as CardImportRequest, context::Context, cores as cores_cmd,
        library as library_cmd, library::ImportRequest, saves as saves_cmd,
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
    Saves,
    Import(ImportRequest),
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
        import: Box<Pending>,
        accepted: HashSet<(String, String)>,
    },
}

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
    /// Arbitrary file actions (currently: deleting ROM files from the card).
    Files {
        label: String,
        actions: Vec<Action>,
    },
    Copy(CopyPlan),
    Saves(Box<SavesPlan>),
    Rename(Box<RenamePlan>),
    /// Renames of existing games to standard names.
    Standardize(Vec<RenamePlan>),
    LibraryRemove(Box<LibraryRemovePlan>),
    CardImport(Box<CardImportPlan>),
    /// Import from a folder on this computer.
    Import(Box<(ImportOptions, ImportPlan)>),
}

impl Pending {
    /// Imports add games one by one; the Games tab refreshes after each.
    pub fn is_import(&self) -> bool {
        matches!(self, Pending::Import(_) | Pending::CardImport(_))
    }

    /// Every (platform, current name, suggested standard name) an import
    /// plan carries; empty for anything that isn't an import.
    pub fn import_suggestions(&self) -> Vec<(String, String, String)> {
        let mut out = Vec::new();
        let mut collect = |platform: &str, plan: &ImportPlan| {
            for a in &plan.actions {
                if let ImportAction::Add {
                    rom,
                    suggested: Some(s),
                    ..
                } = a
                {
                    out.push((platform.to_string(), rom.clone(), s.clone()));
                }
            }
        };
        match self {
            Pending::Import(b) => collect(&b.0.platform, &b.1),
            Pending::CardImport(c) => {
                for part in &c.parts {
                    collect(&part.platform, &part.plan);
                }
            }
            _ => {}
        }
        out
    }

    /// Runs the planning half of `request` on the worker.
    pub fn plan(
        ctx: &Context,
        request: Request,
        progress: &mut dyn FnMut(Event),
    ) -> Result<Pending> {
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
            Request::Saves => Pending::Saves(Box::new(saves_cmd::plan_sync(ctx, true, progress)?)),
            Request::Import(args) => {
                Pending::Import(Box::new(library_cmd::plan_import(ctx, &args, progress)?))
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
                Pending::CardImport(Box::new(plan))
            }
            Request::AcceptNames {
                mut import,
                accepted,
            } => {
                let lib = ctx.library()?;
                let accept = |platform: &str, name: &str| {
                    accepted.contains(&(platform.to_string(), name.to_string()))
                };
                match import.as_mut() {
                    Pending::Import(b) => {
                        let platform = b.0.platform.clone();
                        apply_suggestions(&lib, &mut b.1, |name| accept(&platform, name))?;
                    }
                    Pending::CardImport(c) => {
                        for part in &mut c.parts {
                            let platform = part.platform.clone();
                            apply_suggestions(&lib, &mut part.plan, |name| {
                                accept(&platform, name)
                            })?;
                        }
                    }
                    _ => {}
                }
                *import
            }
        })
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
            Pending::Files { label, actions } => {
                progress(Event::Status(label.clone()));
                match ctx
                    .pocket()
                    .and_then(|p| apply_actions(&p, &label, &actions, progress))
                {
                    Ok(a) => applied(format!("✓ {label}: {} file(s) deleted", a.removed), 0),
                    Err(e) => applied(format!("✗ {label}: {e}"), 1),
                }
            }
            Pending::Copy(plan) => {
                let result = card_cmd::apply_copy(ctx, &plan, progress);
                shown(plan, result, |s| text::card::copy_report(&s))
            }
            Pending::Saves(plan) => {
                let result = saves_cmd::apply_sync(ctx, &plan, progress);
                shown(*plan, result, |s| text::saves::saves_sync(&s))
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
            Pending::CardImport(plan) => {
                let result = card_cmd::apply_import(ctx, &plan, progress);
                shown(*plan, result, |staged| {
                    text::card::import_report(&card_cmd::ImportReport {
                        staged,
                        not_found: Vec::new(),
                        covers: None,
                        warnings: Vec::new(),
                    })
                })
            }
            Pending::Import(boxed) => {
                let (options, plan) = *boxed;
                let result = library_cmd::apply_import(ctx, &options, &plan, progress);
                shown(plan, result, |staged| {
                    text::library::library_import(&library_cmd::ImportReport {
                        staged,
                        covers: None,
                        warnings: Vec::new(),
                    })
                })
            }
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
            pending,
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
            Pending::Files { label, actions } => confirm(
                format!("{label}?"),
                actions
                    .iter()
                    .filter_map(|a| match a {
                        Action::Remove { path, .. } => Some(format!("  - {}", path.display())),
                        _ => None,
                    })
                    .collect(),
                Pending::Files { label, actions },
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
            Pending::Saves(plan) => {
                if let Some(index) = plan
                    .actions
                    .iter()
                    .position(|a| matches!(a, SaveAction::Conflict { .. }))
                {
                    return SaveConflict { plan, index };
                }
                let lines = text_lines(&text::saves::saves_sync(&Staged::preview((*plan).clone())));
                if plan.changes() == 0 {
                    info("Save backup review", lines)
                } else {
                    confirm(
                        format!(
                            "Back up {}, restore {} save(s)?",
                            plan.summary.pull, plan.summary.push
                        ),
                        lines,
                        Pending::Saves(plan),
                    )
                }
            }
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
            Pending::LibraryRemove(plan) => confirm(
                format!("Remove {} ROM(s) from the library?", plan.changes()),
                text_lines(&text::library::library_remove(&Staged::preview(
                    (*plan).clone(),
                ))),
                Pending::LibraryRemove(plan),
            ),
            import @ (Pending::Import(_) | Pending::CardImport(_))
                if !import.import_suggestions().is_empty() =>
            {
                PickImportNames(import)
            }
            Pending::Import(b) => {
                let lines = text_lines(&text::library::library_import(
                    &library_cmd::ImportReport::preview(b.1.clone()),
                ));
                match b.1.changes() {
                    0 => info("Nothing to import", lines),
                    n => confirm(
                        format!("Import {n} ROM(s) into the library?"),
                        lines,
                        Pending::Import(b),
                    ),
                }
            }
            Pending::CardImport(plan) => {
                let lines = text_lines(&text::card::import_report(
                    &card_cmd::ImportReport::preview((*plan).clone()),
                ));
                match plan.changes() {
                    0 => info("Nothing to import", lines),
                    n => confirm(
                        format!("Copy {n} ROM(s) into the library?"),
                        lines,
                        Pending::CardImport(plan),
                    ),
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
        pending: Pending,
    },
    /// A save changed on both sides: which copy wins (`index` into the plan).
    SaveConflict { plan: Box<SavesPlan>, index: usize },
    /// Standard-name renames to tick.
    PickRenames(Vec<RenamePlan>),
    /// An import whose files match standard names: which names to take.
    PickImportNames(Pending),
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
