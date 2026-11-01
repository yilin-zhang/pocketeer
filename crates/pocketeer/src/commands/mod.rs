//! One function per subcommand. Each returns `Result<T>` where `T: Serialize`;
//! `main` decides how to print it.
//!
//! Commands that change something follow one shape: plan, then apply unless
//! `--dry-run` was given. [`Staged`] is that shape; [`verdict`] is the rule
//! that turns failures into the exit status every command shares.

pub mod card;
pub mod card_import;
pub mod cores;
pub mod library;
pub mod meta;
pub mod saves;

use std::path::Path;

use pocketeer_core::{
    Error, ErrorInfo, Pocket, Result,
    clean::CleanResult,
    install::{self, Action},
    library::{
        card_import::CardImportPlan,
        import::{ImportPlan, ImportResult},
        remove::{RemovePlan, RemoveResult},
        rename::{RenamePlan, RenameResult},
        saves::{SavesPlan, SavesResult},
    },
    progress::Event,
    roms::Dat,
};
use serde::Serialize;

use crate::{cli::ApplyArgs, output::Outcome};

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// `--dry-run`: plans only.
    DryRun,
    /// The default: plans applied.
    Applied,
    /// A TUI dialog: the plan body only, no CLI footers (the buttons say
    /// what happens next). Never produced by the CLI.
    Interactive,
}

impl Mode {
    pub fn from_args(args: ApplyArgs) -> Self {
        if args.dry_run {
            Mode::DryRun
        } else {
            Mode::Applied
        }
    }
}

/// The one rule behind every command's exit status: any failure is an
/// error, otherwise success (a dry run included).
pub fn verdict(changes: usize, failed: usize) -> Option<Error> {
    (failed > 0).then_some(Error::SomeFailed {
        failed,
        total: failed + changes,
    })
}

/// Something that can be previewed and then applied.
pub trait Plan: Serialize {
    fn changes(&self) -> usize;
    /// Problems found while planning that count as failures (default none).
    fn errors(&self) -> usize {
        0
    }
}

/// What applying a plan produced.
pub trait Applied: Serialize {
    fn failed(&self) -> usize;
}

/// [`Plan`] for core plan types that count their own changes.
macro_rules! plan_by_changes {
    ($($t:ty),+ $(,)?) => { $(impl Plan for $t { fn changes(&self) -> usize { <$t>::changes(self) } })+ };
}
/// [`Applied`] for result types with a `failed` list.
macro_rules! applied_by_failed {
    ($($t:ty),+ $(,)?) => { $(impl Applied for $t { fn failed(&self) -> usize { self.failed.len() } })+ };
}
plan_by_changes!(
    ImportPlan,
    RenamePlan,
    RemovePlan,
    CardImportPlan,
    pocketeer_core::library::transfer::CopyPlan,
    pocketeer_core::roms::rename::CardRenamePlan
);
applied_by_failed!(
    ImportResult,
    RemoveResult,
    SavesResult,
    CleanResult,
    pocketeer_core::library::transfer::CopyResult
);

impl Applied for RenameResult {
    fn failed(&self) -> usize {
        0
    }
}

/// A plan plus, when it was applied, its result. Serialises as
/// `{mode, ...plan fields, result?}`.
#[derive(Debug, Serialize)]
pub struct Staged<P, R> {
    pub mode: Mode,
    #[serde(flatten)]
    pub plan: P,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<R>,
}

impl<P, R> Staged<P, R> {
    /// A plan that was not (or not yet) applied.
    pub fn new(mode: Mode, plan: P) -> Self {
        Staged {
            mode,
            plan,
            result: None,
        }
    }

    /// A plan shown in a TUI dialog before it is applied.
    pub fn preview(plan: P) -> Self {
        Self::new(Mode::Interactive, plan)
    }
}

impl<P: Plan, R: Applied> Staged<P, R> {
    pub fn changes(&self) -> usize {
        self.plan.changes()
    }

    /// Failures so far: planning problems plus, once applied, failed items.
    pub fn failed(&self) -> usize {
        self.result.as_ref().map_or(0, Applied::failed) + self.plan.errors()
    }
}

impl<P: Plan, R: Applied> Outcome for Staged<P, R> {
    fn error(&self) -> Option<Error> {
        verdict(self.changes(), self.failed())
    }
}

/// Applies `plan` with `run` unless this is a dry run or there is nothing to do.
pub fn stage<P: Plan, R: Applied>(
    apply: ApplyArgs,
    plan: P,
    run: impl FnOnce(&P) -> Result<R>,
) -> Result<Staged<P, R>> {
    let mode = Mode::from_args(apply);
    let result = if mode == Mode::Applied && plan.changes() > 0 {
        Some(run(&plan)?)
    } else {
        None
    };
    Ok(Staged { mode, plan, result })
}

/// Like [`stage`] without the apply: a plan that must not run because of
/// problems found alongside it (names that don't exist, say). Those are the
/// caller's to count as failures.
pub fn stage_if<P: Plan, R: Applied>(
    apply: ApplyArgs,
    allowed: bool,
    plan: P,
    run: impl FnOnce(&P) -> Result<R>,
) -> Result<Staged<P, R>> {
    if allowed {
        stage(apply, plan, run)
    } else {
        Ok(Staged::new(Mode::from_args(apply), plan))
    }
}

/// One member of a batch (`cores install`, `cores update`, `cores remove`):
/// the same gate as [`stage`], but a failure stays with the item instead of
/// aborting the batch.
pub fn stage_item<P: Plan, R: Applied>(
    mode: Mode,
    plan: &P,
    run: impl FnOnce(&P) -> Result<R>,
) -> (Option<R>, Option<ErrorInfo>) {
    if mode != Mode::Applied || plan.changes() == 0 {
        return (None, None);
    }
    match run(plan) {
        Ok(r) => (Some(r), None),
        Err(e) => (None, Some(e.to_info())),
    }
}

/// [`verdict`] over a batch: each item contributes its changes and whether it failed.
pub fn batch_verdict(items: impl Iterator<Item = (usize, bool)>) -> Option<Error> {
    let (mut changes, mut failed) = (0, 0);
    for (n, is_err) in items {
        changes += n;
        failed += is_err as usize;
    }
    verdict(changes, failed)
}

/// Reads the optional `--dat FILE`.
pub fn load_dat(path: Option<&Path>) -> Result<Option<Dat>> {
    path.map(Dat::read).transpose()
}

// ---------------------------------------------------------------------------
// Plain file actions (moves, deletes) that aren't a core install.

impl Applied for install::Applied {
    /// Applying either succeeds or rolls back and errors.
    fn failed(&self) -> usize {
        0
    }
}

/// Runs file actions that aren't a core install (moves, deletes); `label`
/// names the staging folder on the card.
pub fn apply_actions(
    pocket: &Pocket,
    label: &str,
    actions: &[Action],
    progress: &mut dyn FnMut(Event),
) -> Result<install::Applied> {
    progress(Event::Status(format!(
        "Applying {} file action(s)",
        actions.len()
    )));
    install::apply::apply(
        pocket,
        label,
        actions,
        &mut |_, _| unreachable!("file actions never install a core"),
        progress,
    )
}

impl Applied for pocketeer_core::roms::rename::CardRenameResult {
    fn failed(&self) -> usize {
        0
    }
}

impl Plan for SavesPlan {
    fn changes(&self) -> usize {
        self.changes()
    }
    fn errors(&self) -> usize {
        self.summary.conflicts
    }
}
