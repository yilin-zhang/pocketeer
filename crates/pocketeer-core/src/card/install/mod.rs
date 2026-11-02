//! Installing, updating and removing cores: resolve → download → plan → apply.

pub mod apply;
pub mod plan;
pub mod zipfile;

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{
    Error, Pocket, Result, card,
    card::cores::CoreList,
    catalog::inventory::{Inventory, InventoryCore, Release},
    net::{Cache, Http},
    plan::{Event, Plan, Progress},
};

pub use apply::Changes;
pub use plan::{Action, InstallPlan, PlanOptions};
pub use zipfile::{CoreZip, ZipSource};

/// A plan plus the zip it was made from, ready to apply.
pub struct Prepared {
    pub plan: InstallPlan,
    zip: CoreZip,
}

impl Prepared {
    pub fn apply(mut self, pocket: &Pocket, progress: Progress<'_>) -> Result<Changes> {
        let label = self.plan.core_id.clone();
        let zip = &mut self.zip;
        apply::apply(
            pocket,
            &label,
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

pub struct Installer<'a> {
    pub pocket: &'a Pocket,
    pub http: &'a Http,
    pub cache: &'a Cache,
    /// Only use already-downloaded zips.
    pub offline: bool,
}

impl Installer<'_> {
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
        let bytes = self
            .cache
            .download(self.http, url, self.offline, progress)?;
        let mut zip = CoreZip::from_bytes(bytes, ZipSource::Url { url: url.clone() })?;
        let opts = PlanOptions {
            requested_id: Some(core.id.clone()),
            requires_license: release.requires_license,
            ..opts
        };
        let plan = plan::plan_install(self.pocket, &mut zip, installed, &opts)?;
        Ok(Prepared { plan, zip })
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
        Ok(Prepared { plan, zip })
    }
}

/// What `cores remove` would do.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct UninstallPlan {
    pub core_id: String,
    pub installed_version: String,
    pub purge: bool,
    pub actions: Vec<Action>,
    pub summary: plan::Summary,
}

impl Plan for UninstallPlan {
    fn changes(&self) -> usize {
        self.actions.len()
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
        summary: plan::summarize(&actions),
        actions,
    })
}

pub fn apply_remove(
    pocket: &Pocket,
    plan: &UninstallPlan,
    progress: Progress<'_>,
) -> Result<Changes> {
    apply::apply(
        pocket,
        &format!("remove-{}", plan.core_id),
        &plan.actions,
        &mut |_, _| unreachable!("remove plans have no installs"),
        progress,
    )
}
