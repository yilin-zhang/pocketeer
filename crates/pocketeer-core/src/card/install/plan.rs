// Install rules (Platforms/_archive redirect, moved-file cleanup, core
// replacement moves) are ported from Pocket Sync (src-tauri/src/install_zip/mod.rs).
// Copyright (c) Neil Morrison — https://github.com/neil-morrison44/pocket-sync
// Licensed under AGPL-3.0. Modified for pocketeer, 2026: expressed as a pure
// plan instead of interleaved prompts and writes.

//! What an install *would* do, as data.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use serde::Serialize;

use crate::{
    Error, Pocket, Result, card,
    card::{cores::CoreList, formats::PreviousCore},
    catalog::inventory::update_available,
    fsutil::is_junk,
    plan::Plan,
};

use super::zipfile::{CoreZip, ZipSource};

/// Top-level folders where a file keeps its name but may move between
/// subfolders across releases; stale copies elsewhere are removed.
const MOVE_AWARE_ROOTS: [&str; 4] = [card::ASSETS_DIR, card::CORES_DIR, "Input", "Interact"];

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    /// Write a file from the zip to `dest` (relative to the SD root).
    Install {
        zip_path: String,
        dest: PathBuf,
        /// `dest` already exists and will be backed up first.
        overwrite: bool,
        size: u64,
    },
    /// Delete `path` (file or directory).
    Remove { path: PathBuf, reason: RemoveReason },
    /// Move every file under `from` to the same place under `to`.
    MoveTree {
        from: PathBuf,
        to: PathBuf,
        reason: MoveReason,
    },
    /// Rename one file `from` → `to` (both relative to the SD root).
    MoveFile {
        from: PathBuf,
        to: PathBuf,
        reason: MoveReason,
    },
    /// A zip entry we deliberately don't install.
    Skip {
        zip_path: String,
        reason: SkipReason,
    },
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemoveReason {
    /// Same file name now lives at `replaced_by`; this is the old location.
    MovedFile { replaced_by: PathBuf },
    /// This core is superseded by the one being installed.
    ReplacedCore { by: String },
    /// Explicit uninstall.
    Uninstall,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MoveReason {
    ReplacedCore {
        from: String,
        to: String,
    },
    /// `bios place`: a file dropped at the SD root goes where a core wants it.
    PlaceRootFile,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SkipReason {
    /// `__MACOSX/`, `.DS_Store`, `*.md`, ...
    Junk,
    /// `--keep-platforms`: user's existing platform data wins.
    KeepPlatformData,
    /// The destination is an existing directory.
    ExistingDirectory,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Summary {
    pub install: usize,
    pub overwrite: usize,
    pub remove: usize,
    pub move_tree: usize,
    pub skip: usize,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct InstallPlan {
    pub core_id: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_version: Option<String>,
    pub source: ZipSource,
    pub requires_license: bool,
    /// Other cores shipped in the same zip (rare; installed alongside).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub also_installs: Vec<String>,
    /// Older cores this install supersedes and removes.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub replaces: Vec<String>,
    /// `false` means "already at this version; nothing to do".
    pub changed: bool,
    pub summary: Summary,
    pub actions: Vec<Action>,
}

impl Plan for InstallPlan {
    fn changes(&self) -> usize {
        self.changed as usize
    }
}

#[derive(Debug, Clone, Default)]
pub struct PlanOptions {
    /// Plan the install even if this version is already installed.
    pub force: bool,
    /// Don't overwrite existing `Platforms/` files (user customisations).
    pub keep_platforms: bool,
    /// Which core in the zip the user asked for, if any (case-insensitive).
    pub requested_id: Option<String>,
    pub requires_license: bool,
}

/// Same version, ignoring the cosmetic differences [`update_available`] ignores.
pub(crate) fn same_version(a: &str, b: &str) -> bool {
    !update_available(a, b) && !update_available(b, a)
}

pub fn plan_install(
    pocket: &Pocket,
    zip: &mut CoreZip,
    installed: &CoreList,
    opts: &PlanOptions,
) -> Result<InstallPlan> {
    let ids = zip.core_ids();
    if ids.is_empty() {
        return Err(Error::NotACoreZip {
            origin: zip.describe(),
        });
    }
    let core_id = match &opts.requested_id {
        Some(req) => ids
            .iter()
            .find(|id| id.eq_ignore_ascii_case(req))
            .cloned()
            .ok_or_else(|| Error::ZipCoreMismatch {
                requested: req.clone(),
                found: ids.clone(),
            })?,
        None => ids[0].clone(),
    };
    let also_installs: Vec<String> = ids.iter().filter(|id| **id != core_id).cloned().collect();

    let core_json = zip.core_json(&core_id)?;
    let version = core_json.core.metadata.version.clone();
    let new_platform = core_json
        .core
        .metadata
        .platform_ids
        .first()
        .cloned()
        .unwrap_or_default();
    let installed_version = installed.get(&core_id).map(|c| c.version.clone());

    let mut plan = InstallPlan {
        core_id: core_id.clone(),
        version: version.clone(),
        installed_version: installed_version.clone(),
        source: zip.source.clone(),
        requires_license: opts.requires_license,
        also_installs,
        replaces: Vec::new(),
        changed: true,
        summary: Summary::default(),
        actions: Vec::new(),
    };

    if !opts.force
        && let Some(cur) = &installed_version
        && same_version(cur, &version)
    {
        plan.changed = false;
        return Ok(plan);
    }

    let root = pocket.root();
    let mut dests: BTreeSet<PathBuf> = BTreeSet::new();

    // 1. Files from the zip.
    for entry in zip.entries.clone() {
        if is_junk(&entry.path) {
            plan.actions.push(Action::Skip {
                zip_path: entry.name,
                reason: SkipReason::Junk,
            });
            continue;
        }
        let dest = redirect_platform_json(root, &entry.path);
        let full = root.join(&dest);
        if full.is_dir() {
            plan.actions.push(Action::Skip {
                zip_path: entry.name,
                reason: SkipReason::ExistingDirectory,
            });
            continue;
        }
        let exists = full.exists();
        if opts.keep_platforms && exists && dest.starts_with(card::PLATFORMS_DIR) {
            plan.actions.push(Action::Skip {
                zip_path: entry.name,
                reason: SkipReason::KeepPlatformData,
            });
            continue;
        }
        dests.insert(dest.clone());
        plan.actions.push(Action::Install {
            zip_path: entry.name,
            dest,
            overwrite: exists,
            size: entry.size,
        });
    }

    // 2. Stale copies of files that moved between subfolders.
    let mut removals: BTreeSet<PathBuf> = BTreeSet::new();
    for dest in &dests {
        for stale in find_moved_copies(root, dest) {
            if !dests.contains(&stale) {
                removals.insert(stale.clone());
                plan.actions.push(Action::Remove {
                    path: stale,
                    reason: RemoveReason::MovedFile {
                        replaced_by: dest.clone(),
                    },
                });
            }
        }
    }

    // 3. Cores this one replaces.
    if let Some(updaters) = zip.updaters(&core_id) {
        for prev in updaters.previous_cores() {
            let prev_id = prev.id();
            if prev_id == core_id
                || !root
                    .join(card::core_rel(&prev_id))
                    .join("core.json")
                    .exists()
            {
                continue;
            }
            plan.replaces.push(prev_id.clone());
            for (from, to) in replacement_moves(&prev, &core_id, &new_platform) {
                if from != to && root.join(&from).exists() {
                    plan.actions.push(Action::MoveTree {
                        from,
                        to,
                        reason: MoveReason::ReplacedCore {
                            from: prev_id.clone(),
                            to: core_id.clone(),
                        },
                    });
                }
            }
            let old_core_dir = card::core_rel(&prev_id);
            if !removals.contains(&old_core_dir) {
                plan.actions.push(Action::Remove {
                    path: old_core_dir,
                    reason: RemoveReason::ReplacedCore {
                        by: core_id.clone(),
                    },
                });
            }
        }
    }

    plan.summary = summarize(&plan.actions);
    Ok(plan)
}

/// `Platforms/<x>.json` goes to `Platforms/_archive/` if the user parked it there.
fn redirect_platform_json(root: &Path, rel: &Path) -> PathBuf {
    if let Ok(stripped) = rel.strip_prefix(card::PLATFORMS_DIR)
        && stripped.components().count() == 1
        && rel.extension().is_some_and(|e| e == "json")
    {
        let archived = PathBuf::from(card::PLATFORMS_DIR)
            .join("_archive")
            .join(stripped);
        if root.join(&archived).exists() {
            return archived;
        }
    }
    rel.to_path_buf()
}

/// Under the file's `Assets/<platform>` / `Cores/<id>` / ... subtree, other
/// files with the same name are assumed to be the previous location.
fn find_moved_copies(root: &Path, dest: &Path) -> Vec<PathBuf> {
    let Some(file_name) = dest.file_name() else {
        return Vec::new();
    };
    let mut comps = dest.components();
    let (Some(top), Some(second)) = (comps.next(), comps.next()) else {
        return Vec::new();
    };
    if !MOVE_AWARE_ROOTS.iter().any(|r| top.as_os_str() == *r) {
        return Vec::new();
    }
    let subtree = PathBuf::from(top.as_os_str()).join(second.as_os_str());
    let full_subtree = root.join(&subtree);
    if !full_subtree.is_dir() {
        return Vec::new();
    }
    walkdir::WalkDir::new(&full_subtree)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file() && e.file_name() == file_name)
        .filter_map(|e| e.path().strip_prefix(root).ok().map(Path::to_path_buf))
        .filter(|p| p != dest)
        .collect()
}

/// Folders that follow a core when it is replaced by a renamed successor.
fn replacement_moves(
    prev: &PreviousCore,
    new_id: &str,
    new_platform: &str,
) -> Vec<(PathBuf, PathBuf)> {
    let prev_id = prev.id();
    let old = prev.platform_id.as_str();
    vec![
        (
            card::assets_rel(old, &prev_id),
            card::assets_rel(new_platform, new_id),
        ),
        (
            card::assets_rel(old, "common"),
            card::assets_rel(new_platform, "common"),
        ),
        (
            card::saves_rel(old, &prev_id),
            card::saves_rel(new_platform, new_id),
        ),
        (
            card::saves_rel(old, "common"),
            card::saves_rel(new_platform, "common"),
        ),
        (card::settings_rel(&prev_id), card::settings_rel(new_id)),
    ]
}

pub fn summarize(actions: &[Action]) -> Summary {
    let mut s = Summary::default();
    for a in actions {
        match a {
            Action::Install {
                overwrite, size, ..
            } => {
                s.install += 1;
                if *overwrite {
                    s.overwrite += 1;
                }
                s.bytes += size;
            }
            Action::Remove { .. } => s.remove += 1,
            Action::MoveTree { .. } | Action::MoveFile { .. } => s.move_tree += 1,
            Action::Skip { .. } => s.skip += 1,
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        card::cores,
        testutil::{CORE_JSON, build_zip, sd, write},
    };

    fn gba_zip(extra: &[(&str, &[u8])]) -> CoreZip {
        let mut files: Vec<(&str, &[u8])> = vec![
            ("Cores/spiritualized.GBA/core.json", CORE_JSON.as_bytes()),
            ("Cores/spiritualized.GBA/gba.rev", b"bits"),
            ("Platforms/gba.json", b"{}"),
            ("Platforms/_images/gba.bin", b"img"),
            ("__MACOSX/._x", b""),
            ("README.md", b"x"),
        ];
        files.extend_from_slice(extra);
        CoreZip::from_bytes(
            build_zip(&files),
            ZipSource::File {
                path: "gba.zip".into(),
            },
        )
        .unwrap()
    }

    fn plan(pocket: &Pocket, zip: &mut CoreZip, opts: &PlanOptions) -> InstallPlan {
        let installed = cores::list_installed(pocket).unwrap();
        plan_install(pocket, zip, &installed, opts).unwrap()
    }

    #[test]
    fn fresh_install_plans_files_and_skips_junk() {
        let (_tmp, pocket) = sd();
        let p = plan(&pocket, &mut gba_zip(&[]), &PlanOptions::default());
        assert_eq!(p.core_id, "spiritualized.GBA");
        assert_eq!(p.version, "1.3.0");
        assert!(p.changed);
        assert_eq!(p.installed_version, None);
        assert_eq!(p.summary.install, 4);
        assert_eq!(p.summary.overwrite, 0);
        assert_eq!(p.summary.skip, 2);
        assert_eq!(p.summary.bytes, CORE_JSON.len() as u64 + 4 + 2 + 3);
        assert!(
            p.actions
                .iter()
                .all(|a| !matches!(a, Action::Remove { .. }))
        );
    }

    #[test]
    fn same_version_is_a_noop_unless_forced() {
        let (tmp, pocket) = sd();
        write(tmp.path(), "Cores/spiritualized.GBA/core.json", CORE_JSON);
        let p = plan(&pocket, &mut gba_zip(&[]), &PlanOptions::default());
        assert!(!p.changed);
        assert!(p.actions.is_empty());
        assert_eq!(p.installed_version.as_deref(), Some("1.3.0"));

        let p = plan(
            &pocket,
            &mut gba_zip(&[]),
            &PlanOptions {
                force: true,
                ..Default::default()
            },
        );
        assert!(p.changed);
        assert_eq!(p.summary.overwrite, 1);
    }

    #[test]
    fn upgrade_overwrites_and_prunes_moved_files() {
        let (tmp, pocket) = sd();
        write(
            tmp.path(),
            "Cores/spiritualized.GBA/core.json",
            CORE_JSON.replace("1.3.0", "1.2.0"),
        );
        // Old release kept the bitstream in a subfolder.
        write(tmp.path(), "Cores/spiritualized.GBA/old/gba.rev", "old");
        // Same name under a *different* core must not be touched.
        write(tmp.path(), "Cores/other.Core/gba.rev", "theirs");

        let p = plan(&pocket, &mut gba_zip(&[]), &PlanOptions::default());
        assert!(p.changed);
        assert_eq!(p.installed_version.as_deref(), Some("1.2.0"));
        let removes: Vec<&Action> = p
            .actions
            .iter()
            .filter(|a| matches!(a, Action::Remove { .. }))
            .collect();
        assert_eq!(removes.len(), 1);
        match removes[0] {
            Action::Remove {
                path,
                reason: RemoveReason::MovedFile { replaced_by },
            } => {
                assert_eq!(path, Path::new("Cores/spiritualized.GBA/old/gba.rev"));
                assert_eq!(replaced_by, Path::new("Cores/spiritualized.GBA/gba.rev"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn platform_json_follows_user_into_archive_and_keep_platforms_skips() {
        let (tmp, pocket) = sd();
        write(tmp.path(), "Platforms/_archive/gba.json", "{\"mine\":1}");
        write(tmp.path(), "Platforms/_images/gba.bin", "myimg");

        let p = plan(&pocket, &mut gba_zip(&[]), &PlanOptions::default());
        let dest_of = |zp: &str| {
            p.actions.iter().find_map(|a| match a {
                Action::Install {
                    zip_path,
                    dest,
                    overwrite,
                    ..
                } if zip_path == zp => Some((dest.clone(), *overwrite)),
                _ => None,
            })
        };
        assert_eq!(
            dest_of("Platforms/gba.json").unwrap(),
            (PathBuf::from("Platforms/_archive/gba.json"), true)
        );
        assert_eq!(
            dest_of("Platforms/_images/gba.bin").unwrap(),
            (PathBuf::from("Platforms/_images/gba.bin"), true)
        );

        let p = plan(
            &pocket,
            &mut gba_zip(&[]),
            &PlanOptions {
                keep_platforms: true,
                ..Default::default()
            },
        );
        let kept = p
            .actions
            .iter()
            .filter(|a| {
                matches!(
                    a,
                    Action::Skip {
                        reason: SkipReason::KeepPlatformData,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(kept, 2);
    }

    #[test]
    fn replacement_moves_assets_saves_settings_and_removes_old_core() {
        let (tmp, pocket) = sd();
        let old_core = r#"{"core":{"metadata":{"platform_ids":["gb_old"],"shortname":"Old","author":"someone","version":"0.1"}}}"#;
        write(tmp.path(), "Cores/someone.Old/core.json", old_core);
        write(tmp.path(), "Assets/gb_old/someone.Old/game.json", "{}");
        write(tmp.path(), "Assets/gb_old/common/bios.bin", "b");
        write(tmp.path(), "Saves/gb_old/common/game.sav", "s");
        write(tmp.path(), "Settings/someone.Old/x.bin", "x");
        let updaters =
            br#"{"previous":[{"author":"someone","shortname":"Old","platform_id":"gb_old"}]}"#;

        let p = plan(
            &pocket,
            &mut gba_zip(&[("Cores/spiritualized.GBA/updaters.json", updaters)]),
            &PlanOptions::default(),
        );
        assert_eq!(p.replaces, ["someone.Old"]);
        let moves: Vec<(String, String)> = p
            .actions
            .iter()
            .filter_map(|a| match a {
                Action::MoveTree { from, to, .. } => {
                    Some((from.display().to_string(), to.display().to_string()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            moves,
            [
                (
                    "Assets/gb_old/someone.Old".to_string(),
                    "Assets/gba/spiritualized.GBA".to_string()
                ),
                (
                    "Assets/gb_old/common".to_string(),
                    "Assets/gba/common".to_string()
                ),
                (
                    "Saves/gb_old/common".to_string(),
                    "Saves/gba/common".to_string()
                ),
                (
                    "Settings/someone.Old".to_string(),
                    "Settings/spiritualized.GBA".to_string()
                ),
            ]
        );
        assert!(p.actions.iter().any(|a| matches!(
            a,
            Action::Remove { path, reason: RemoveReason::ReplacedCore { .. } } if path == Path::new("Cores/someone.Old")
        )));
        assert_eq!(p.summary.move_tree, 4);
    }

    #[test]
    fn requested_id_must_match_zip() {
        let (_tmp, pocket) = sd();
        let installed = CoreList::default();
        let err = plan_install(
            &pocket,
            &mut gba_zip(&[]),
            &installed,
            &PlanOptions {
                requested_id: Some("other.Core".into()),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(err.code(), "ZIP_CORE_MISMATCH");

        let ok = plan_install(
            &pocket,
            &mut gba_zip(&[]),
            &installed,
            &PlanOptions {
                requested_id: Some("SPIRITUALIZED.gba".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(ok.core_id, "spiritualized.GBA");
    }

    #[test]
    fn zip_without_core_is_rejected() {
        let (_tmp, pocket) = sd();
        let mut zip = CoreZip::from_bytes(
            build_zip(&[("Platforms/gba.json", b"{}")]),
            ZipSource::File {
                path: "x.zip".into(),
            },
        )
        .unwrap();
        let err = plan_install(
            &pocket,
            &mut zip,
            &CoreList::default(),
            &PlanOptions::default(),
        )
        .unwrap_err();
        assert_eq!(err.code(), "NOT_A_CORE_ZIP");
    }
}
