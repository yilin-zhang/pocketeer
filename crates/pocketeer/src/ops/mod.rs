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
    plan::{Event, Progress},
};
use serde::Serialize;

use crate::output::Outcome;
use context::Context;

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// `--dry-run`: plans only.
    DryRun,
    /// The default: plans applied.
    Applied,
}

/// The one rule behind every command's exit status: any failure is an
/// error, otherwise success (a dry run included).
pub fn verdict(changes: usize, failed: usize) -> Option<Error> {
    (failed > 0).then_some(Error::SomeFailed {
        failed,
        total: failed + changes,
    })
}

/// A plan the CLI and the TUI can carry out. Every write operation's plan
/// implements it, so applying one never needs per-operation glue: `stage`
/// and `Batch` run any of them, and the TUI applies any of them the same way.
/// (`&mut self`: an install plan reads its zip as it goes.)
pub trait Apply: Plan {
    type Output: Applied;
    fn apply(&mut self, ctx: &Context, progress: Progress<'_>) -> Result<Self::Output>;
}

/// A plan plus, when it was applied, its result. Serialises as
/// `{mode, ...plan fields, result?}`.
#[derive(Serialize)]
pub struct Staged<P: Apply> {
    pub mode: Mode,
    #[serde(flatten)]
    pub plan: P,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<P::Output>,
}

impl<P: Apply> Staged<P> {
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
        Self::new(Mode::DryRun, plan)
    }

    pub fn changes(&self) -> usize {
        self.plan.changes()
    }

    /// Failures so far: planning problems plus, once applied, failed items.
    pub fn failed(&self) -> usize {
        self.result.as_ref().map_or(0, Applied::failed) + self.plan.errors()
    }
}

impl<P: Apply> Outcome for Staged<P> {
    fn error(&self) -> Option<Error> {
        verdict(self.changes(), self.failed())
    }
}

/// Applies `plan` unless this is a dry run or there is nothing to do.
pub fn stage<P: Apply>(
    ctx: &Context,
    mode: Mode,
    mut plan: P,
    progress: Progress<'_>,
) -> Result<Staged<P>> {
    let result = if mode == Mode::Applied && plan.changes() > 0 {
        Some(plan.apply(ctx, progress)?)
    } else {
        None
    };
    Ok(Staged { mode, plan, result })
}

/// Like [`stage`], for a plan that must not run because of problems found
/// alongside it (names that don't exist, say) unless `allowed`. Those
/// problems are the caller's to count as failures.
pub fn stage_if<P: Apply>(
    ctx: &Context,
    mode: Mode,
    allowed: bool,
    plan: P,
    progress: Progress<'_>,
) -> Result<Staged<P>> {
    if allowed {
        stage(ctx, mode, plan, progress)
    } else {
        Ok(Staged::new(mode, plan))
    }
}

/// A batch of independent plans (`cores install`, `update`, `remove`,
/// `library standardize`): each item is planned and applied on its own,
/// and a failure stays with its item instead of aborting the rest.
#[derive(Serialize)]
pub struct Batch<P: Apply> {
    pub mode: Mode,
    pub items: Vec<BatchItem<P>>,
}

#[derive(Serialize)]
pub struct BatchItem<P: Apply> {
    /// What the user named: `id`, `id@version`, a zip path or a ROM.
    pub target: String,
    #[serde(flatten)]
    pub state: BatchState<P>,
}

/// How far one item of a [`Batch`] got.
#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BatchState<P: Apply> {
    /// It couldn't be planned.
    Unplanned {
        error: ErrorInfo,
    },
    /// Planned and left alone: a dry run, or nothing to change.
    Planned {
        plan: P,
    },
    Applied {
        plan: P,
        result: P::Output,
    },
    /// Planned, but applying it failed.
    Failed {
        plan: P,
        error: ErrorInfo,
    },
}

impl<P: Apply> Batch<P> {
    /// Stages every planned item in turn (see [`BatchItem::stage`]).
    pub fn stage(
        ctx: &Context,
        mode: Mode,
        plans: impl IntoIterator<Item = (String, Result<P>)>,
        progress: Progress<'_>,
    ) -> Self {
        let items = plans
            .into_iter()
            .map(|(target, plan)| match plan {
                Ok(plan) => BatchItem::stage(target, ctx, mode, plan, progress),
                Err(e) => BatchItem {
                    target,
                    state: BatchState::Unplanned { error: e.to_info() },
                },
            })
            .collect();
        Batch { mode, items }
    }

    /// What the batch would do, applying nothing (a dry run).
    pub fn planned(plans: impl IntoIterator<Item = (String, Result<P>)>) -> Self {
        let items = plans
            .into_iter()
            .map(|(target, plan)| BatchItem {
                target,
                state: match plan {
                    Ok(plan) => BatchState::Planned { plan },
                    Err(e) => BatchState::Unplanned { error: e.to_info() },
                },
            })
            .collect();
        Batch {
            mode: Mode::DryRun,
            items,
        }
    }

    /// The items that have a plan, to apply them.
    pub fn into_plans(self) -> impl Iterator<Item = (String, P)> {
        self.items.into_iter().filter_map(|i| match i.state {
            BatchState::Unplanned { .. } => None,
            BatchState::Planned { plan }
            | BatchState::Applied { plan, .. }
            | BatchState::Failed { plan, .. } => Some((i.target, plan)),
        })
    }

    pub fn failed(&self) -> usize {
        self.items.iter().filter(|i| i.is_failed()).count()
    }
}

impl<P: Apply> Outcome for Batch<P> {
    fn error(&self) -> Option<Error> {
        let changes = self
            .items
            .iter()
            .filter_map(BatchItem::plan)
            .map(Plan::changes);
        verdict(changes.sum(), self.failed())
    }
}

impl<P: Apply> BatchItem<P> {
    /// The same gate as [`stage`], but a failure to apply stays with the item.
    pub fn stage(
        target: String,
        ctx: &Context,
        mode: Mode,
        mut plan: P,
        progress: Progress<'_>,
    ) -> Self {
        let state = if mode != Mode::Applied || plan.changes() == 0 {
            BatchState::Planned { plan }
        } else {
            match plan.apply(ctx, progress) {
                Ok(result) => BatchState::Applied { plan, result },
                Err(e) => BatchState::Failed {
                    plan,
                    error: e.to_info(),
                },
            }
        };
        BatchItem { target, state }
    }

    pub fn is_failed(&self) -> bool {
        matches!(
            self.state,
            BatchState::Unplanned { .. } | BatchState::Failed { .. }
        )
    }

    pub fn plan(&self) -> Option<&P> {
        match &self.state {
            BatchState::Unplanned { .. } => None,
            BatchState::Planned { plan }
            | BatchState::Applied { plan, .. }
            | BatchState::Failed { plan, .. } => Some(plan),
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Global;

    /// Applying fails when `fails` is set.
    #[derive(Serialize)]
    struct Fake {
        fails: bool,
    }

    impl Plan for Fake {
        fn changes(&self) -> usize {
            1
        }
    }

    impl Apply for Fake {
        type Output = install::Changes;
        fn apply(&mut self, _: &Context, _: Progress<'_>) -> Result<install::Changes> {
            match self.fails {
                true => Err(Error::NoHomeDir),
                false => Ok(install::Changes::default()),
            }
        }
    }

    fn run(mode: Mode, plans: Vec<Result<Fake>>) -> Batch<Fake> {
        let ctx = Context::new(Global::default());
        let named = plans
            .into_iter()
            .enumerate()
            .map(|(i, p)| (i.to_string(), p));
        Batch::stage(&ctx, mode, named, &mut |_| {})
    }

    #[test]
    fn a_failing_item_does_not_stop_the_rest_of_the_batch() {
        let batch = run(
            Mode::Applied,
            vec![
                Ok(Fake { fails: false }),
                Ok(Fake { fails: true }),
                Err(Error::NoHomeDir),
                Ok(Fake { fails: false }),
            ],
        );
        let states: Vec<_> = batch
            .items
            .iter()
            .map(|i| match i.state {
                BatchState::Applied { .. } => "applied",
                BatchState::Failed { .. } => "failed",
                BatchState::Unplanned { .. } => "unplanned",
                BatchState::Planned { .. } => "planned",
            })
            .collect();
        assert_eq!(states, ["applied", "failed", "unplanned", "applied"]);
        assert_eq!(batch.error().unwrap().code(), "SOME_FAILED");
    }

    #[test]
    fn a_dry_run_applies_nothing() {
        let batch = run(Mode::DryRun, vec![Ok(Fake { fails: true })]);
        assert!(matches!(batch.items[0].state, BatchState::Planned { .. }));
        assert!(batch.error().is_none());
    }
}
