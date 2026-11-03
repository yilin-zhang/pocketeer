//! Shared application workflows and independent batch results.
//! Simple operations call core directly; front ends decide when to apply.

pub mod card;
pub mod context;
pub mod cores;
pub mod library;
pub mod memories;
pub mod saves;

use std::path::Path;

use pocketeer_core::{ErrorInfo, Result, catalog::Dat};
use serde::Serialize;

/// Independent plans made and applied together (`cores install`, `update`,
/// `remove`, `library standardize`): each item is planned and applied on its
/// own, and a failure stays with its item instead of stopping the rest.
#[derive(Serialize)]
pub struct Batch<P, R> {
    pub items: Vec<BatchItem<P, R>>,
}

#[derive(Serialize)]
pub struct BatchItem<P, R> {
    /// What the user named: `id`, `id@version`, a zip path or a ROM.
    pub target: String,
    #[serde(flatten)]
    pub state: BatchState<P, R>,
}

/// How far one item of a [`Batch`] got.
#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BatchState<P, R> {
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
        result: R,
    },
    /// Planned, but applying it failed.
    Failed {
        plan: P,
        error: ErrorInfo,
    },
}

impl<P, R> Batch<P, R> {
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
    pub fn apply(
        self,
        changes: impl Fn(&P) -> usize,
        mut execute: impl FnMut(&mut P) -> Result<R>,
    ) -> Self {
        let items = self
            .items
            .into_iter()
            .map(|item| {
                let state = match item.state {
                    BatchState::Planned { mut plan } if changes(&plan) > 0 => {
                        match execute(&mut plan) {
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
    pub fn changes(&self, count: impl Fn(&P) -> usize) -> usize {
        self.plans().map(count).sum()
    }

    pub fn failed(&self) -> usize {
        self.items.iter().filter(|i| i.is_failed()).count()
    }
}

impl<P, R> BatchItem<P, R> {
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

impl<P: Serialize, R: Serialize> pocketeer_core::plan::Applied for Batch<P, R> {
    fn failed(&self) -> usize {
        Batch::failed(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pocketeer_core::Error;

    #[test]
    fn a_failing_item_does_not_stop_the_rest_of_the_batch() {
        let plans = [Ok(false), Ok(true), Err(Error::NoHomeDir), Ok(false)];
        let batch = Batch::plan(
            plans
                .into_iter()
                .enumerate()
                .map(|(i, p)| (i.to_string(), p)),
        );
        let batch = batch.apply(
            |_| 1,
            |fails| {
                if *fails {
                    Err(Error::NoHomeDir)
                } else {
                    Ok(())
                }
            },
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
        assert_eq!(batch.failed(), 2);
    }
}
