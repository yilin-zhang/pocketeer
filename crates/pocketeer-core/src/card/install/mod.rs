//! Installing, updating and removing cores: resolve → download → plan → apply.

pub mod apply;
pub mod plan;
pub mod zipfile;

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{
    Error, Pocket, Result, card,
    card::cores::CoreList,
    catalog::inventory::{Inventory, InventoryCore, Release, update_available},
    net::Net,
    plan::{Checked, Event, Pinned, Plan, Progress, Resources},
};

pub use apply::Changes;
pub use plan::{Action, InstallPlan, PlanOptions};
pub use zipfile::{CoreZip, ZipSource};

/// A plan plus the zip it was made from, ready to apply.
pub struct Prepared {
    pub plan: InstallPlan,
    zip: CoreZip,
    pinned: Pinned,
}

/// A prepared install reads (and reports) as its plan.
impl serde::Serialize for Prepared {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        self.plan.serialize(s)
    }
}

impl Plan for Prepared {
    type Output = Changes;

    fn changes(&self) -> usize {
        self.plan.changes()
    }

    fn pinned(&self) -> &Pinned {
        &self.pinned
    }

    fn execute(
        &mut self,
        res: &mut Resources,
        progress: Progress<'_>,
        _: Checked,
    ) -> Result<Changes> {
        progress(Event::Status(format!(
            "Installing {} {}",
            self.plan.core_id, self.plan.version
        )));
        let zip = &mut self.zip;
        apply::apply(
            res.pocket()?,
            &self.plan.core_id,
            &self.plan.actions,
            &mut |zip_path, dest| zip.extract_entry(zip_path, dest),
            progress,
        )
    }
}

/// Picks the release for `id` (`@version` suffix allowed) from the inventory.
pub fn resolve<'a>(
    inventory: &'a Inventory,
    spec: &str,
) -> Result<(&'a InventoryCore, &'a Release)> {
    let (id, version) = match spec.split_once('@') {
        Some((id, v)) => (id, Some(v)),
        None => (spec, None),
    };
    let core = inventory
        .get_ci(id)
        .ok_or_else(|| Error::CoreNotFound { id: id.to_string() })?;
    let release = match version {
        None => core.latest().ok_or_else(|| Error::NoRelease {
            id: core.id.clone(),
        })?,
        Some(v) => core
            .releases
            .iter()
            .find(|r| plan::same_version(&r.core.metadata.version, v))
            .ok_or_else(|| Error::VersionNotFound {
                id: core.id.clone(),
                version: v.to_string(),
                available: core
                    .releases
                    .iter()
                    .map(|r| r.core.metadata.version.clone())
                    .collect(),
            })?,
    };
    Ok((core, release))
}

/// Which installed cores an update is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Targets {
    /// Every core with a newer release in the inventory.
    Outdated,
    /// These cores (they must be installed).
    Ids(Vec<String>),
}

/// Plans installs on one card, downloading releases through `net`.
pub struct Installer<'a> {
    pocket: &'a Pocket,
    net: &'a Net,
}

impl<'a> Installer<'a> {
    pub fn new(pocket: &'a Pocket, net: &'a Net) -> Self {
        Installer { pocket, net }
    }

    /// Downloads (or reuses a cached download of) a release and plans it.
    pub fn prepare_release(
        &self,
        core: &InventoryCore,
        release: &Release,
        installed: &CoreList,
        opts: PlanOptions,
        progress: Progress<'_>,
    ) -> Result<Prepared> {
        let url = &release.download_url;
        let bytes =
            self.net
                .cache()
                .download(self.net.http(), url, self.net.offline(), progress)?;
        let mut zip = CoreZip::from_bytes(bytes, ZipSource::Url { url: url.clone() })?;
        let opts = PlanOptions {
            requested_id: Some(core.id.clone()),
            requires_license: release.requires_license,
            ..opts
        };
        let plan = plan::plan_install(self.pocket, &mut zip, installed, &opts)?;
        Ok(Prepared {
            plan,
            zip,
            pinned: Pinned::card(self.pocket)?,
        })
    }

    /// Resolves each of `specs` (`id` or `id@version`) against the inventory
    /// and downloads what it needs. One entry per spec, failures included.
    pub fn prepare_specs(
        &self,
        inventory: &Inventory,
        installed: &CoreList,
        specs: &[String],
        opts: &PlanOptions,
        progress: Progress<'_>,
    ) -> Vec<(String, Result<Prepared>)> {
        specs
            .iter()
            .map(|spec| {
                progress(Event::Status(format!("Resolving {spec}")));
                let prepared = resolve(inventory, spec).and_then(|(core, release)| {
                    self.prepare_release(core, release, installed, opts.clone(), progress)
                });
                (spec.clone(), prepared)
            })
            .collect()
    }

    /// Prepares the latest release of each installed core `targets` names.
    /// A named core that isn't installed is an entry with its error.
    pub fn prepare_updates(
        &self,
        inventory: &Inventory,
        installed: &CoreList,
        targets: &Targets,
        opts: &PlanOptions,
        progress: Progress<'_>,
    ) -> Vec<(String, Result<Prepared>)> {
        let mut out = Vec::new();
        let ids: Vec<String> = match targets {
            Targets::Outdated => installed
                .cores
                .iter()
                .filter(|c| {
                    inventory
                        .get(&c.id)
                        .and_then(|e| e.latest())
                        .is_some_and(|l| update_available(&c.version, &l.core.metadata.version))
                })
                .map(|c| c.id.clone())
                .collect(),
            Targets::Ids(ids) => ids
                .iter()
                .filter_map(|id| match installed.get_ci(id) {
                    Some(c) => Some(c.id.clone()),
                    None => {
                        out.push((id.clone(), Err(Error::CoreNotInstalled { id: id.clone() })));
                        None
                    }
                })
                .collect(),
        };
        out.extend(self.prepare_specs(inventory, installed, &ids, opts, progress));
        out
    }

    /// Plans an install from a zip on disk.
    pub fn prepare_file(
        &self,
        path: &Path,
        installed: &CoreList,
        opts: PlanOptions,
        progress: Progress<'_>,
    ) -> Result<Prepared> {
        progress(Event::Status(format!("Reading {}", path.display())));
        let mut zip = CoreZip::from_file(path)?;
        let plan = plan::plan_install(self.pocket, &mut zip, installed, &opts)?;
        Ok(Prepared {
            plan,
            zip,
            pinned: Pinned::card(self.pocket)?,
        })
    }
}

/// What `cores remove` would do.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct UninstallPlan {
    pub core_id: String,
    pub installed_version: String,
    pub purge: bool,
    #[serde(flatten)]
    pub actions: plan::Actions,
    #[serde(skip)]
    pinned: Pinned,
}

impl Plan for UninstallPlan {
    type Output = Changes;

    fn changes(&self) -> usize {
        self.actions.len()
    }

    fn pinned(&self) -> &Pinned {
        &self.pinned
    }

    fn execute(
        &mut self,
        res: &mut Resources,
        progress: Progress<'_>,
        _: Checked,
    ) -> Result<Changes> {
        progress(Event::Status(format!("Removing {}", self.core_id)));
        apply::apply(
            res.pocket()?,
            &format!("remove-{}", self.core_id),
            &self.actions,
            &mut |_, _| unreachable!("remove plans have no installs"),
            progress,
        )
    }
}

/// Pocket Sync removes `Cores/`, `Presets/` and `Settings/` for the core;
/// `purge` additionally removes its `Assets/` and `Saves/` folders.
pub fn plan_remove(
    pocket: &Pocket,
    installed: &CoreList,
    id: &str,
    purge: bool,
) -> Result<UninstallPlan> {
    let core = installed
        .get_ci(id)
        .ok_or_else(|| Error::CoreNotInstalled { id: id.to_string() })?;
    let root = pocket.root();
    let mut paths: Vec<PathBuf> = card::core_dirs(&core.id).into();
    if purge {
        for platform in &core.platform_ids {
            paths.extend(card::core_platform_dirs(platform, &core.id));
        }
    }
    let actions: Vec<Action> = paths
        .into_iter()
        .filter(|p| root.join(p).exists())
        .map(|path| Action::Remove {
            path,
            reason: plan::RemoveReason::Uninstall,
        })
        .collect();
    Ok(UninstallPlan {
        core_id: core.id.clone(),
        installed_version: core.version.clone(),
        purge,
        actions: actions.into(),
        pinned: Pinned::card(pocket)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{CORE_JSON, sd, swap_card, write};

    #[test]
    fn removing_from_a_swapped_card_is_refused() {
        let (_tmp, pocket) = sd();
        write(
            pocket.root(),
            "Cores/spiritualized.GBA/core.json",
            CORE_JSON,
        );
        let installed = crate::card::cores::list_installed(&pocket).unwrap();
        let mut plan = plan_remove(&pocket, &installed, "spiritualized.GBA", false).unwrap();
        let aside = tempfile::tempdir().unwrap();
        swap_card(&pocket, aside.path());
        let err = crate::testutil::apply(&mut plan, Some(&pocket), None).unwrap_err();
        assert_eq!(err.code(), "STALE_PLAN");
        assert!(pocket.root().join("Cores/spiritualized.GBA").is_dir());
    }
}
