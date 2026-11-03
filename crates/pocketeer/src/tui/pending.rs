//! Confirmation dialogs retain concrete work for the main worker.

use std::path::PathBuf;

use pocketeer_core::{
    Result,
    card::{
        bios::PlacePlan,
        clean::CleanPlan,
        install::{Changes, Prepared, UninstallPlan},
        roms::DeletePlan,
    },
    library::{
        covers::import::CoverImportPlan,
        remove::RemovePlan as LibraryRemovePlan,
        rename::{RenamePlan, RenameResult},
    },
    plan::{Applied, Progress},
    transfer::{
        rename::CardRenamePlan,
        saves::{TrashPlan, TrashTarget},
        to_card::CopyPlan,
    },
};

use super::app::{Import, SaveSync};
use super::worker::{Msg, applied, failed};
use crate::{
    ops::{Batch, context::Context},
    text::{self, Text},
};

/// Planning work for the worker: the plan, and the dialog it opens.
pub type Planner = Box<dyn FnOnce(&Context, Progress<'_>) -> Result<Preview> + Send>;

/// Work retained by a confirmation dialog and consumed on the main worker.
type Work = Box<dyn FnOnce(&Context, Progress<'_>) -> Msg + Send>;

pub struct Pending {
    pub adds_games: bool,
    run: Work,
}

impl Pending {
    pub fn apply(self, ctx: &Context, progress: Progress<'_>) -> Msg {
        (self.run)(ctx, progress)
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
    SaveConflict { backup: Box<SaveSync>, index: usize },
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

pub(super) fn confirm<R: Text + Applied>(
    title: impl Into<String>,
    lines: Vec<String>,
    run: impl FnOnce(&Context, Progress<'_>) -> Result<R> + Send + 'static,
) -> Preview {
    Preview::Confirm {
        title: title.into(),
        lines,
        pending: Pending {
            adds_games: false,
            run: Box::new(move |ctx, progress| match run(ctx, progress) {
                Ok(result) => applied(result.text(), result.failed()),
                Err(e) => failed(e),
            }),
        },
    }
}

pub(super) fn info(title: &str, lines: Vec<String>) -> Preview {
    Preview::Info {
        title: title.into(),
        lines,
    }
}

// ---------------------------------------------------------------------------
// Plans asked about over their own text.

pub fn cover(mut plan: CoverImportPlan) -> Preview {
    let title = match plan.replaces_cover() {
        true => "Replace this game's cover?",
        false => "Import this game's cover?",
    };
    confirm(title, text_lines(&plan.text()), move |ctx, progress| {
        plan.apply(&mut ctx.library()?, progress)
    })
}

pub fn card_rename(mut plan: CardRenamePlan) -> Preview {
    confirm(
        "Rename this AP ROM and its saves?",
        text_lines(&plan.text()),
        move |ctx, progress| plan.apply(&ctx.pocket()?, ctx.library().ok().as_ref(), progress),
    )
}

pub fn clean(mut plan: CleanPlan) -> Preview {
    if plan.entries.is_empty() {
        return Preview::Notice("No macOS junk on the card.".into());
    }
    let title = format!(
        "Delete {} junk item(s), {}?",
        plan.entries.len(),
        text::bytes(plan.bytes)
    );
    confirm(title, text_lines(&plan.text()), move |ctx, progress| {
        plan.apply(&ctx.pocket()?, progress)
    })
}

pub fn copy(mut plan: CopyPlan) -> Preview {
    match plan.changes() {
        0 => info("Already on this card", text_lines(&plan.text())),
        n => confirm(
            format!("Copy {n} ROM(s) to this card?"),
            text_lines(&plan.text()),
            move |ctx, progress| plan.apply(&ctx.pocket()?, progress),
        ),
    }
}

pub fn rename(mut plan: RenamePlan) -> Preview {
    confirm(
        format!("Rename to {}?", plan.new_name),
        text_lines(&plan.text()),
        move |ctx, progress| plan.apply(&mut ctx.library()?, progress),
    )
}

pub fn place(mut plan: PlacePlan) -> Preview {
    if !plan.actions.is_empty() {
        return confirm(
            format!("Move {} file(s) into place?", plan.actions.len()),
            text_lines(&plan.text()),
            move |ctx, progress| plan.apply(&ctx.pocket()?, progress),
        );
    }
    if plan.zipped.is_empty() {
        return Preview::Notice("No file at the SD card root matches a missing file.".into());
    }
    let lines = plan
        .zipped
        .iter()
        .map(|z| {
            format!(
                "{} is inside a zip at the root; unzip it first.",
                z.path.display()
            )
        })
        .collect();
    info("Nothing to place yet", lines)
}

// ---------------------------------------------------------------------------
// Plans with a dialog of their own.

pub fn remove(mut plan: LibraryRemovePlan) -> Preview {
    let games: std::collections::BTreeSet<&str> =
        plan.items.iter().map(|i| i.rom.game.as_str()).collect();
    let title = match (games.len(), games.iter().next()) {
        (1, Some(game)) => format!("Remove {game}?"),
        _ => format!("Remove {} games?", games.len()),
    };
    let mut lines: Vec<String> = if plan.items.len() > 1 {
        plan.items
            .iter()
            .map(|i| format!("{}  {}", i.rom.name, text::bytes(i.size)))
            .collect()
    } else {
        Vec::new()
    };
    lines.push("It goes to .trash/ in the library. Copies on the card stay.".into());
    confirm(title, lines, move |ctx, progress| {
        plan.apply(&mut ctx.library()?, progress)
    })
}

pub fn trash(mut plan: TrashPlan) -> Preview {
    if plan.files.is_empty() {
        return Preview::Notice("Nothing to throw away.".into());
    }
    let title = match plan.target {
        TrashTarget::Card => format!("Delete {} file(s) from AP?", plan.files.len()),
        _ => format!("Move {} file(s) to the trash?", plan.files.len()),
    };
    let lines = trash_lines(&plan);
    confirm(title, lines, move |ctx, progress| {
        let card = if plan.target == TrashTarget::Card {
            Some(ctx.pocket()?)
        } else {
            None
        };
        plan.apply(&mut ctx.library()?, card.as_ref(), progress)
    })
}

/// The files a trash plan throws away, by name, and where they go.
fn trash_lines(plan: &TrashPlan) -> Vec<String> {
    let name = |p: &std::path::Path| {
        p.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    };
    let mut lines: Vec<String> = plan
        .files
        .iter()
        .map(|f| match plan.target {
            TrashTarget::Card => name(&f.path),
            _ if f.path.starts_with("saves/.history") => {
                format!("{}  older version, {}", name(&f.save), text::bytes(f.size))
            }
            _ => format!("{}  backup, {}", name(&f.save), text::bytes(f.size)),
        })
        .collect();
    lines.push(String::new());
    lines.push(match plan.target {
        TrashTarget::Card if plan.files.iter().any(|f| f.archive_first) => {
            "The library keeps a copy first.".into()
        }
        TrashTarget::Card => "The library already has a copy.".into(),
        _ => "They go to .trash/ in the library.".into(),
    });
    lines
}

pub fn install(plan: Batch<Prepared, Changes>) -> Preview {
    let lines = text_lines(&plan.text());
    match plan.plans().filter(|p| p.changes() > 0).count() {
        0 => info("Nothing to do", lines),
        n => confirm(
            format!("Install {n} core(s)?"),
            lines,
            move |ctx, progress| {
                Ok(plan.apply(Prepared::changes, |p| p.apply(&ctx.pocket()?, progress)))
            },
        ),
    }
}

pub fn uninstall(plan: Batch<UninstallPlan, Changes>) -> Preview {
    let ids: Vec<&str> = plan.items.iter().map(|i| i.target.as_str()).collect();
    let title = format!("Remove {}?", ids.join(", "));
    let lines = text_lines(&plan.text());
    confirm(title, lines, move |ctx, progress| {
        Ok(plan.apply(UninstallPlan::changes, |p| {
            p.apply(&ctx.pocket()?, progress)
        }))
    })
}

/// The standard names the user ticked.
pub fn renames(plan: Batch<RenamePlan, RenameResult>) -> Preview {
    let title = format!("Rename {} game(s) to standard names?", plan.items.len());
    let lines = text_lines(&plan.text());
    confirm(title, lines, move |ctx, progress| {
        Ok(plan.apply(RenamePlan::changes, |p| {
            p.apply(&mut ctx.library()?, progress)
        }))
    })
}

pub fn delete(mut plan: DeletePlan) -> Preview {
    let name = plan.path.file_name().unwrap_or_default().to_string_lossy();
    let title = format!("Delete {name} from the card?");
    let lines = vec![
        format!("  - {}", plan.path.display()),
        "The library has no copy; i copies it there first.".into(),
    ];
    Preview::Confirm {
        title,
        lines,
        pending: Pending {
            adds_games: false,
            run: Box::new(move |ctx, progress| {
                let result = ctx.pocket().and_then(|card| plan.apply(&card, progress));
                match result {
                    Ok(_) => applied(
                        format!("✓ deleted {} from the card", plan.path.display()),
                        0,
                    ),
                    Err(e) => failed(e),
                }
            }),
        },
    }
}
