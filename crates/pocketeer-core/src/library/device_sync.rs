//! Push a device's subscriptions (library ROMs, latest cores) onto its card.

use serde::Serialize;

use crate::{
    ErrorInfo, Pocket, Result,
    install::{self, Applied, InstallPlan, Installer, PlanOptions, Prepared},
    inventory::Inventory,
    progress::{Event, Progress},
    required,
    roms::{
        FileRef,
        hash::HashCache,
        sync::{self, SourceFile, SyncPlan, SyncResult},
    },
};

use super::{Library, db, device::Device};

#[derive(Debug, Clone, Copy)]
pub struct DeviceSyncOptions {
    pub roms: bool,
    pub cores: bool,
    /// Delete card files no library ROM maps to.
    pub delete: bool,
    /// Compare crc32, not just size, for files already on the card.
    pub verify: bool,
}

impl Default for DeviceSyncOptions {
    fn default() -> Self {
        DeviceSyncOptions {
            roms: true,
            cores: true,
            delete: false,
            verify: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RomsPart {
    pub platform: String,
    pub plan: SyncPlan,
    /// Library ROMs whose file is gone (reference imports that moved, etc.).
    pub missing_sources: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CorePart {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<InstallPlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorInfo>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct DeviceSyncSummary {
    pub rom_changes: usize,
    pub core_changes: usize,
    pub errors: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DeviceSyncPlan {
    pub device: Device,
    pub roms: Vec<RomsPart>,
    pub cores: Vec<CorePart>,
    pub summary: DeviceSyncSummary,
}

impl DeviceSyncPlan {
    pub fn changes(&self) -> usize {
        self.summary.rom_changes + self.summary.core_changes
    }
}

/// A plan plus the downloaded core zips it needs to apply.
pub struct DeviceSyncPrepared {
    pub plan: DeviceSyncPlan,
    /// Aligned with `plan.cores`.
    prepared: Vec<Option<Prepared>>,
}

/// Network resources for the cores half of a sync. `None` → cores are
/// reported as errors (inventory unavailable) rather than failing the whole plan.
pub struct CoreSources<'a> {
    pub inventory: &'a Inventory,
    pub installer: &'a Installer<'a>,
}

pub fn plan(
    lib: &Library,
    pocket: &Pocket,
    device: &Device,
    opts: DeviceSyncOptions,
    core_sources: Option<&CoreSources<'_>>,
    cache: &mut HashCache,
    progress: Progress<'_>,
) -> Result<DeviceSyncPrepared> {
    let mut out = DeviceSyncPlan {
        device: device.clone(),
        roms: Vec::new(),
        cores: Vec::new(),
        summary: DeviceSyncSummary::default(),
    };
    let mut prepared = Vec::new();

    if opts.roms {
        let protected = required::protected_paths(pocket)?;
        for platform in &device.platforms {
            progress(Event::Status(format!("Planning {platform} ROMs")));
            let games = db::list_games(lib.conn(), Some(platform), None)?;
            let mut sources = Vec::new();
            let mut missing = Vec::new();
            for g in &games {
                for rom in &g.roms {
                    let path = lib.rom_abs_path(rom);
                    if !path.is_file() {
                        missing.push(format!("{} ({})", rom.name, path.display()));
                        continue;
                    }
                    sources.push(SourceFile {
                        name: rom.name.clone(),
                        file: FileRef::plain(path),
                        size: rom.size,
                        crc32: rom.crc32_u32(),
                        label: format!("{} / {}", g.name, rom.name),
                    });
                }
            }
            let dest = Pocket::assets_rel(platform, "common");
            let renames = super::rename::pending_renames(lib.conn(), platform)?;
            let plan = sync::plan_from_sources(
                pocket,
                &dest,
                &sources,
                &renames,
                &protected,
                opts.delete,
                opts.verify,
                cache,
            )?;
            out.summary.rom_changes += plan.changes();
            out.summary.errors += missing.len();
            out.roms.push(RomsPart {
                platform: platform.clone(),
                plan,
                missing_sources: missing,
            });
        }
    }

    if opts.cores {
        let installed = crate::cores::list_installed(pocket)?;
        for id in &device.cores {
            progress(Event::Status(format!("Resolving {id}")));
            let result = match core_sources {
                None => Err(crate::Error::OfflineNoCache {
                    what: "core inventory".into(),
                }),
                Some(cs) => install::resolve(cs.inventory, id).and_then(|(core, release)| {
                    cs.installer.prepare_release(
                        core,
                        release,
                        &installed,
                        PlanOptions::default(),
                        progress,
                    )
                }),
            };
            match result {
                Ok(p) => {
                    if p.plan.changed {
                        out.summary.core_changes += 1;
                    }
                    out.cores.push(CorePart {
                        id: id.clone(),
                        plan: Some(p.plan.clone()),
                        error: None,
                    });
                    prepared.push(Some(p));
                }
                Err(e) => {
                    out.summary.errors += 1;
                    out.cores.push(CorePart {
                        id: id.clone(),
                        plan: None,
                        error: Some(e.to_info()),
                    });
                    prepared.push(None);
                }
            }
        }
    }

    cache.save_best_effort();
    Ok(DeviceSyncPrepared {
        plan: out,
        prepared,
    })
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RomsResult {
    pub platform: String,
    pub result: SyncResult,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CoreResult {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied: Option<Applied>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorInfo>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct DeviceSyncResult {
    pub roms: Vec<RomsResult>,
    pub cores: Vec<CoreResult>,
    pub failed: usize,
}

impl DeviceSyncPrepared {
    pub fn apply(
        self,
        lib: &Library,
        pocket: &Pocket,
        progress: Progress<'_>,
    ) -> Result<DeviceSyncResult> {
        let mut result = DeviceSyncResult::default();
        for part in &self.plan.roms {
            if part.plan.changes() == 0 {
                continue;
            }
            progress(Event::Status(format!("Syncing {} ROMs", part.platform)));
            let r = sync::apply_sync(pocket, &part.plan, progress)?;
            result.failed += r.failed.len();
            result.roms.push(RomsResult {
                platform: part.platform.clone(),
                result: r,
            });
        }
        for (part, prep) in self.plan.cores.iter().zip(self.prepared) {
            let Some(prep) = prep else {
                continue;
            };
            if !prep.plan.changed {
                continue;
            }
            progress(Event::Status(format!(
                "Installing {} {}",
                part.id, prep.plan.version
            )));
            match prep.apply(pocket, progress) {
                Ok(a) => result.cores.push(CoreResult {
                    id: part.id.clone(),
                    applied: Some(a),
                    error: None,
                }),
                Err(e) => {
                    result.failed += 1;
                    result.cores.push(CoreResult {
                        id: part.id.clone(),
                        applied: None,
                        error: Some(e.to_info()),
                    });
                }
            }
        }
        super::device::touch_last_sync(lib, &self.plan.device.id)?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::{device, import::ImportMode};
    use std::{fs, path::Path};

    use crate::testutil::{library_with, library_with_mode, sd};

    #[test]
    fn roms_flow_library_to_card_and_back_to_idle() {
        let (_tmp, lib) = library_with(&[("a.fkh", "hello world"), ("b.fkh", "22")]);

        let (_sd, pocket) = sd();
        let d = device::add(&lib, &pocket, "A").unwrap();
        let d = device::subscribe(&lib, &d.id, &["fkh".into()], &[], true).unwrap();
        // A stray file on the card that --delete would remove.
        fs::create_dir_all(pocket.root().join("Assets/fkh/common")).unwrap();
        fs::write(pocket.root().join("Assets/fkh/common/stray.fkh"), "x").unwrap();

        let opts = DeviceSyncOptions {
            delete: true,
            ..Default::default()
        };
        let prep = plan(
            &lib,
            &pocket,
            &d,
            opts,
            None,
            &mut HashCache::in_memory(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(prep.plan.summary.rom_changes, 3, "2 copies + 1 delete");
        assert_eq!(prep.plan.summary.core_changes, 0);
        assert!(prep.plan.cores.is_empty());

        let r = prep.apply(&lib, &pocket, &mut |_| {}).unwrap();
        assert_eq!(r.failed, 0);
        assert_eq!(r.roms[0].result.copied, 2);
        assert_eq!(r.roms[0].result.deleted, 1);
        assert_eq!(
            fs::read_to_string(pocket.root().join("Assets/fkh/common/a.fkh")).unwrap(),
            "hello world"
        );
        assert!(!pocket.root().join("Assets/fkh/common/stray.fkh").exists());
        assert!(device::get(&lib, &d.id).unwrap().last_sync.is_some());

        let prep = plan(
            &lib,
            &pocket,
            &d,
            opts,
            None,
            &mut HashCache::in_memory(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(prep.plan.changes(), 0);
    }

    #[test]
    fn missing_source_and_unavailable_inventory_are_reported_not_fatal() {
        let (tmp, lib) = library_with_mode(&[("a.fkh", "hello world")], ImportMode::Reference);
        fs::remove_file(tmp.path().join("src/a.fkh")).unwrap(); // referenced file vanishes

        let (_sd, pocket) = sd();
        let d = device::add(&lib, &pocket, "A").unwrap();
        let d =
            device::subscribe(&lib, &d.id, &["fkh".into()], &["some.Core".into()], true).unwrap();
        let prep = plan(
            &lib,
            &pocket,
            &d,
            DeviceSyncOptions::default(),
            None,
            &mut HashCache::in_memory(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(prep.plan.roms[0].missing_sources.len(), 1);
        assert_eq!(
            prep.plan.cores[0].error.as_ref().unwrap().code,
            "OFFLINE_NO_CACHE"
        );
        assert_eq!(prep.plan.summary.errors, 2);
        assert_eq!(prep.plan.changes(), 0);
        assert!(!Path::new(&pocket.root().join("Assets/fkh/common/a.fkh")).exists());
    }
}
