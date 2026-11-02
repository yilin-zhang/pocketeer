//! The use cases, shared by the CLI and the TUI. Each function takes a
//! [`Context`](context::Context) and plain arguments (never clap types) and
//! returns `Result<T>` where `T: Serialize`; `main` and the TUI decide how to
//! show it (see `text`).
//!
//! Commands that change something follow one shape: plan, then apply unless
//! `--dry-run` was given. [`Staged`] is that shape; [`verdict`] is the rule
//! that turns failures into the exit status every command shares.

pub mod card;
pub mod context;
pub mod cores;
pub mod library;
pub mod memories;
pub mod saves;

use std::path::Path;

pub use pocketeer_core::plan::{Applied, Plan};
use pocketeer_core::{
    Error, ErrorInfo, Pocket, Result,
    card::install::{self, Action},
    catalog::Dat,
    plan::Event,
};
use serde::Serialize;

use crate::output::Outcome;

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

/// The one rule behind every command's exit status: any failure is an
/// error, otherwise success (a dry run included).
pub fn verdict(changes: usize, failed: usize) -> Option<Error> {
    (failed > 0).then_some(Error::SomeFailed {
        failed,
        total: failed + changes,
    })
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
    mode: Mode,
    plan: P,
    run: impl FnOnce(&P) -> Result<R>,
) -> Result<Staged<P, R>> {
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
    mode: Mode,
    allowed: bool,
    plan: P,
    run: impl FnOnce(&P) -> Result<R>,
) -> Result<Staged<P, R>> {
    if allowed {
        stage(mode, plan, run)
    } else {
        Ok(Staged::new(mode, plan))
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

/// Runs file actions that aren't a core install (moves, deletes); `label`
/// names the staging folder on the card.
pub fn apply_actions(
    pocket: &Pocket,
    label: &str,
    actions: &[Action],
    progress: &mut dyn FnMut(Event),
) -> Result<install::Changes> {
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
