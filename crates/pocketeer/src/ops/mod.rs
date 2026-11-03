//! What the CLI and the TUI both do, in one place: queries that read the
//! card and the library, and the `plan_*` functions that open what a plan
//! needs from the [`Context`](context::Context) (flags, config, the card)
//! and make it. Applying a plan is core's [`plan::apply`]; whether to apply
//! (`--dry-run`, a dialog) and how to show the outcome is each front end's.
//!
//! [`plan::apply`]: pocketeer_core::plan::apply

pub mod card;
pub mod context;
pub mod cores;
pub mod library;
pub mod memories;
pub mod saves;

use std::path::Path;

use pocketeer_core::{
    ErrorInfo, Result,
    catalog::Dat,
    plan::{self, Plan, Progress, Resources},
};
use serde::Serialize;

/// Independent plans made and applied together (`cores install`, `update`,
/// `remove`, `library standardize`): each item is planned and applied on its
/// own, and a failure stays with its item instead of stopping the rest.
#[derive(Serialize)]
pub struct Batch<P: Plan> {
    pub items: Vec<BatchItem<P>>,
}

#[derive(Serialize)]
pub struct BatchItem<P: Plan> {
    /// What the user named: `id`, `id@version`, a zip path or a ROM.
    pub target: String,
    #[serde(flatten)]
    pub state: BatchState<P>,
}

/// How far one item of a [`Batch`] got.
#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BatchState<P: Plan> {
    /// It couldn't be planned.
    Unplanned {
        error: ErrorInfo,
    },
    /// Planned and not applied (yet): a dry run, or nothing to change.
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

impl<P: Plan> Batch<P> {
    /// One item per target: its plan, or why there is none.
    pub fn plan(plans: impl IntoIterator<Item = (String, Result<P>)>) -> Self {
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
        Batch { items }
    }

    /// Applies every planned item that changes something, each on its own.
    pub fn apply(self, res: &mut Resources, progress: Progress<'_>) -> Self {
        let items = self
            .items
            .into_iter()
            .map(|item| {
                let state = match item.state {
                    BatchState::Planned { mut plan } if plan.changes() > 0 => {
                        match plan::apply(res, &mut plan, progress) {
                            Ok(result) => BatchState::Applied { plan, result },
                            Err(e) => BatchState::Failed {
                                plan,
                                error: e.to_info(),
                            },
                        }
                    }
                    other => other,
                };
                BatchItem {
                    target: item.target,
                    state,
                }
            })
            .collect();
        Batch { items }
    }

    /// The plans, for asking about them (failures to plan left out).
    pub fn plans(&self) -> impl Iterator<Item = &P> {
        self.items.iter().filter_map(BatchItem::plan)
    }

    /// What applying the planned items would change.
    pub fn changes(&self) -> usize {
        self.plans().map(Plan::changes).sum()
    }

    pub fn failed(&self) -> usize {
        self.items.iter().filter(|i| i.is_failed()).count()
    }
}

impl<P: Plan> BatchItem<P> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use pocketeer_core::{
        Error,
        card::install::Changes,
        plan::{Checked, Pinned},
    };

    /// Applying fails when `fails` is set.
    #[derive(Serialize)]
    struct Fake {
        fails: bool,
        #[serde(skip)]
        pinned: Pinned,
    }

    impl Plan for Fake {
        type Output = Changes;

        fn changes(&self) -> usize {
            1
        }

        fn pinned(&self) -> &Pinned {
            &self.pinned
        }

        fn execute(&mut self, _: &mut Resources, _: Progress<'_>, _: Checked) -> Result<Changes> {
            match self.fails {
                true => Err(Error::NoHomeDir),
                false => Ok(Changes::default()),
            }
        }
    }

    fn fake(fails: bool) -> Result<Fake> {
        Ok(Fake {
            fails,
            pinned: Pinned::default(),
        })
    }

    #[test]
    fn a_failing_item_does_not_stop_the_rest_of_the_batch() {
        let plans = [fake(false), fake(true), Err(Error::NoHomeDir), fake(false)];
        let batch = Batch::plan(
            plans
                .into_iter()
                .enumerate()
                .map(|(i, p)| (i.to_string(), p)),
        );
        let mut res = Resources::new(
            || Err(Error::NoHomeDir),
            || Err(Error::NoHomeDir),
            || Err(Error::NoCacheDir),
        );
        let batch = batch.apply(&mut res, &mut |_| {});
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
        assert_eq!(batch.failed(), 2);
    }
}
