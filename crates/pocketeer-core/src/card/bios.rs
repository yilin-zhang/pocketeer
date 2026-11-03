// The required-files algorithm (core slots + instance slots, path layout,
// dedupe preferring md5, root-file matching) is ported from Pocket Sync
// (src-tauri/src/required_files/*, root_files/mod.rs).
// Copyright (c) Neil Morrison — https://github.com/neil-morrison44/pocket-sync
// Licensed under AGPL-3.0. Modified for pocketeer, 2026: synchronous, no
// archive lookups, results as plain data.

//! Which BIOS / data files each installed core needs, and whether they're there.

use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
};

use serde::Serialize;

use crate::{
    Error, ErrorInfo, Pocket, Result,
    card::{
        cores::{CoreList, InstalledCore},
        formats::{DataJson, DataSlot, InstanceJson},
        install::{self, Action},
    },
    fsutil,
    fsutil::hash::digest_hex,
    plan::{Pinned, Progress},
};

/// Cores whose instance JSONs describe things that aren't downloadable files.
const SKIP_INSTANCE_FILES_FOR: [&str; 5] = [
    "Mazamars312.NeoGeo",
    "Mazamars312.NeoGeo_Overdrive",
    "Mazamars312.NeoGeo_Analogizer",
    "Mazamars312.PC Engine CD",
    "obsidian.Vectrex",
];

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum FileStatus {
    Present,
    /// Present, but its md5 doesn't match what the core expects.
    WrongHash {
        actual_md5: String,
    },
    /// Present, but it couldn't be read to check its md5.
    Unreadable {
        error: ErrorInfo,
    },
    Missing,
    /// Missing from its folder, but the user dropped one with this name at
    /// the SD card root.
    AtRoot {
        #[serde(flatten)]
        found: RootFile,
    },
}

/// A file at the SD card root, by itself or inside a zip there. Paths are
/// relative to the SD root.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "form", rename_all = "snake_case")]
pub enum RootFile {
    /// `bios place` can move it into the folder.
    File { path: PathBuf },
    /// Left for the user to unpack.
    Zip { zip: PathBuf, inner: String },
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RequiredFile {
    pub core_id: String,
    /// Data slot name ("BIOS", "ROM", ...).
    pub slot: String,
    /// Where the Pocket expects it, relative to the SD root.
    pub path: PathBuf,
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_md5: Option<String>,
    #[serde(flatten)]
    pub status: FileStatus,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CoreRequirements {
    pub core_id: String,
    pub files: Vec<RequiredFile>,
    /// What reading the core's declarations ran into; its file list may be
    /// incomplete because of them.
    pub notes: Vec<Note>,
}

/// Something about a core's declarations that limits what is known of the
/// files it needs.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "note", rename_all = "snake_case")]
pub enum Note {
    /// The core has no `data.json`, so it declares no files.
    NoDataJson,
    /// Its `data.json` or an instance file could not be read.
    Unreadable { error: ErrorInfo },
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Summary {
    pub present: usize,
    pub missing: usize,
    pub wrong_hash: usize,
    /// Files on the card that couldn't be read to check their md5. Only
    /// serialised when non-zero, so the usual report shape is unchanged.
    #[serde(skip_serializing_if = "is_zero")]
    pub unreadable: usize,
    /// Missing files that `bios place` can move from the SD root.
    pub placeable: usize,
    pub zipped_at_root: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct BiosReport {
    pub cores: Vec<CoreRequirements>,
    pub summary: Summary,
}

impl BiosReport {
    pub fn all_files(&self) -> impl Iterator<Item = &RequiredFile> {
        self.cores.iter().flat_map(|c| c.files.iter())
    }
}

/// Checks every installed core (or just `only`, matched case-insensitively).
pub fn check(pocket: &Pocket, installed: &CoreList, only: Option<&[String]>) -> Result<BiosReport> {
    let root_files = index_root_files(pocket.root());
    let mut cores = Vec::new();
    let mut summary = Summary::default();

    let selected: Vec<&InstalledCore> = match only {
        None => installed.cores.iter().collect(),
        Some(ids) => {
            let mut v = Vec::new();
            for id in ids {
                let core = installed
                    .get_ci(id)
                    .ok_or_else(|| Error::CoreNotInstalled { id: id.clone() })?;
                v.push(core);
            }
            v
        }
    };

    for core in selected {
        let mut req = requirements_for(pocket, core);
        for file in &mut req.files {
            file.status = status_of(pocket.root(), file, &root_files);
            match &file.status {
                FileStatus::Present => summary.present += 1,
                FileStatus::WrongHash { .. } => summary.wrong_hash += 1,
                FileStatus::Unreadable { .. } => summary.unreadable += 1,
                FileStatus::Missing => summary.missing += 1,
                FileStatus::AtRoot { found } => {
                    summary.missing += 1;
                    match found {
                        RootFile::File { .. } => summary.placeable += 1,
                        RootFile::Zip { .. } => summary.zipped_at_root += 1,
                    }
                }
            }
        }
        cores.push(req);
    }

    Ok(BiosReport { cores, summary })
}

/// Paths (relative to the SD root) of every file an installed core declares
/// in its data.json or instance files: BIOS, firmware blobs, arcade ROM sets.
/// ROM tooling treats these as the core's, never as ROMs to sync or delete.
/// Computed once per operation, not per platform.
pub fn protected_paths(pocket: &Pocket) -> Result<BTreeSet<PathBuf>> {
    Ok(crate::card::cores::list_installed(pocket)?
        .cores
        .iter()
        .flat_map(|c| requirements_for(pocket, c).files)
        .map(|f| f.path)
        .collect())
}

/// The files a core wants, before checking the disk.
fn requirements_for(pocket: &Pocket, core: &InstalledCore) -> CoreRequirements {
    let mut req = CoreRequirements {
        core_id: core.id.clone(),
        files: Vec::new(),
        notes: Vec::new(),
    };
    let data_json_path = core.path.join("data.json");
    if !data_json_path.exists() {
        req.notes.push(Note::NoDataJson);
        return req;
    }
    let data = match DataJson::read(&data_json_path) {
        Ok(d) => d,
        Err(e) => {
            req.notes.push(Note::Unreadable { error: e.to_info() });
            return req;
        }
    };
    let slots = data.data.data_slots;

    // 1. Slots with a fixed filename.
    for slot in &slots {
        let Some(filename) = &slot.filename else {
            continue;
        };
        let dir = slot_dir(core, slot);
        let mut names = vec![filename.clone()];
        names.extend(slot.alternate_filenames.clone().unwrap_or_default());
        for name in names {
            req.files.push(RequiredFile {
                core_id: core.id.clone(),
                slot: slot.name.clone(),
                path: dir.join(&name),
                required: slot.required,
                expected_md5: slot.md5.clone(),
                status: FileStatus::Missing,
            });
        }
    }

    // 2. Instance JSONs under Assets/<main platform>/<core>/.
    if !SKIP_INSTANCE_FILES_FOR.contains(&core.id.as_str())
        && let Some(platform) = core.main_platform()
    {
        let assets_dir = pocket.assets_owner_dir(platform, &core.id);
        for instance_path in find_instance_files(&assets_dir) {
            let instance = match InstanceJson::read(&instance_path) {
                Ok(i) => i,
                Err(e) => {
                    req.notes.push(Note::Unreadable { error: e.to_info() });
                    continue;
                }
            };
            for islot in &instance.instance.data_slots {
                let Some(core_slot) = slots.iter().find(|s| s.id == islot.id) else {
                    continue;
                };
                let dir = slot_dir(core, core_slot);
                let path = if instance.instance.data_path.is_empty() {
                    dir.join(&islot.filename)
                } else {
                    dir.join(&instance.instance.data_path).join(&islot.filename)
                };
                req.files.push(RequiredFile {
                    core_id: core.id.clone(),
                    slot: core_slot.name.clone(),
                    path,
                    required: core_slot.required,
                    expected_md5: islot.md5.clone(),
                    status: FileStatus::Missing,
                });
            }
        }
    }

    // Saves and configs aren't "required files" the user has to supply.
    req.files
        .retain(|f| !f.path.extension().is_some_and(|e| e == "sav" || e == "cfg"));

    // Dedupe by path, preferring the entry that knows an md5.
    req.files.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then_with(|| b.expected_md5.is_some().cmp(&a.expected_md5.is_some()))
    });
    req.files.dedup_by(|a, b| a.path == b.path);
    req
}

/// `Assets/<platform>/<core id | common>` for a slot.
fn slot_dir(core: &InstalledCore, slot: &DataSlot) -> PathBuf {
    let params = slot.params();
    let platform = core
        .platform_ids
        .get(params.platform_index)
        .or(core.platform_ids.first())
        .map(String::as_str)
        .unwrap_or("unknown");
    let owner = if params.core_specific {
        core.id.as_str()
    } else {
        crate::card::COMMON
    };
    crate::card::assets_rel(platform, owner)
}

/// `*.json` files under the core's assets folder, skipping hidden entries and
/// `_alternatives` folders.
fn find_instance_files(dir: &Path) -> Vec<PathBuf> {
    if !dir.is_dir() {
        return Vec::new();
    }
    let in_alternatives = |p: &Path| {
        p.strip_prefix(dir)
            .ok()
            .and_then(Path::parent)
            .is_some_and(|parent| {
                parent
                    .components()
                    .any(|c| c.as_os_str().to_string_lossy().starts_with("_alternatives"))
            })
    };
    let mut files: Vec<PathBuf> = fsutil::visible_files(dir)
        .filter(|p| p.extension().is_some_and(|x| x == "json") && !in_alternatives(p))
        .collect();
    files.sort();
    files
}

/// Files the user dropped at the SD root (optionally inside zips), by name.
/// A loose file wins over a zipped one of the same name.
fn index_root_files(root: &Path) -> HashMap<String, RootFile> {
    let mut map = HashMap::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return map;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let rel = PathBuf::from(name);
        if fsutil::is_hidden(&entry.file_name())
            || !path.is_file()
            || name == crate::card::POCKET_JSON
        {
            continue;
        }
        if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("zip"))
        {
            if let Ok(mut archive) = fsutil::open_zip(&path) {
                for i in 0..archive.len() {
                    if let Ok(f) = archive.by_index(i)
                        && !f.is_dir()
                        && let Some(inner) = f.enclosed_name()
                        && let Some(inner_name) = inner.file_name().and_then(|n| n.to_str())
                        && let Ok(full) = f.name()
                    {
                        map.entry(inner_name.to_string()).or_insert(RootFile::Zip {
                            zip: rel.clone(),
                            inner: full.into_owned(),
                        });
                    }
                }
            }
            continue;
        }
        map.insert(name.to_string(), RootFile::File { path: rel });
    }
    map
}

fn status_of(
    root: &Path,
    file: &RequiredFile,
    root_files: &HashMap<String, RootFile>,
) -> FileStatus {
    let full = root.join(&file.path);
    if full.is_file() {
        if let Some(expected) = &file.expected_md5
            && !expected.is_empty()
        {
            return match md5_of(&full) {
                Ok(actual) if actual.eq_ignore_ascii_case(expected) => FileStatus::Present,
                Ok(actual) => FileStatus::WrongHash { actual_md5: actual },
                Err(e) => FileStatus::Unreadable { error: e.to_info() },
            };
        }
        return FileStatus::Present;
    }
    let name = file.path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    match root_files.get(name) {
        Some(found) => FileStatus::AtRoot {
            found: found.clone(),
        },
        None => FileStatus::Missing,
    }
}

fn md5_of(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path).map_err(|e| Error::io(path, e))?;
    digest_hex::<md5::Md5>(&mut file).map_err(|e| Error::io(path, e))
}

// ---------------------------------------------------------------------------
// `bios place`: root files into the folders cores expect.

/// What `bios place` would do: move root files into the folders that want them.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PlacePlan {
    #[serde(flatten)]
    pub actions: install::plan::Actions,
    /// Zipped root files we won't unpack automatically.
    pub zipped: Vec<RequiredFile>,
    #[serde(skip)]
    pinned: Pinned,
}

impl PlacePlan {
    pub fn changes(&self) -> usize {
        self.actions.len()
    }

    /// Moves the planned files.
    pub fn apply(&mut self, pocket: &Pocket, progress: Progress<'_>) -> Result<install::Changes> {
        self.pinned.check_card(pocket)?;
        install::apply::apply(
            pocket,
            "bios-place",
            &self.actions,
            &mut |_, _| unreachable!("place plans have no installs"),
            progress,
        )
    }
}

/// Plans moving the root files `report` (of `pocket`) found into place.
pub fn plan_place(pocket: &Pocket, report: &BiosReport) -> Result<PlacePlan> {
    let mut actions = Vec::new();
    let mut zipped = Vec::new();
    let mut seen_sources: Vec<PathBuf> = Vec::new();
    for file in report.all_files() {
        match &file.status {
            FileStatus::AtRoot {
                found: RootFile::File { path: root_file },
            } => {
                // A root file wanted by several cores is moved once; the rest
                // would need copies, which `place` doesn't do (yet).
                if seen_sources.contains(root_file) {
                    continue;
                }
                seen_sources.push(root_file.clone());
                actions.push(Action::MoveFile {
                    from: root_file.clone(),
                    to: file.path.clone(),
                    reason: install::plan::MoveReason::PlaceRootFile,
                });
            }
            FileStatus::AtRoot {
                found: RootFile::Zip { .. },
            } => zipped.push(file.clone()),
            _ => {}
        }
    }
    Ok(PlacePlan {
        actions: actions.into(),
        zipped,
        pinned: Pinned::card(pocket)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        card::cores,
        testutil::{build_zip, write},
    };
    use std::fs;

    const GBA_CORE: &str = r#"{"core":{"metadata":{"platform_ids":["gba"],"shortname":"GBA","author":"sp","version":"1"}}}"#;
    const GBA_DATA: &str = r#"{"data":{"data_slots":[
        {"name":"BIOS","id":1,"required":true,"parameters":"0x1","filename":"gba_bios.bin","md5":"600fe3d2f68c966cbcd93cdd96390822"},
        {"name":"ROM","id":2,"required":true,"parameters":"0x100","extensions":["gba"]},
        {"name":"Save","id":3,"parameters":"0x102","filename":"x.sav"}
    ]}}"#;
    const ARD_CORE: &str = r#"{"core":{"metadata":{"platform_ids":["arduboy"],"shortname":"Arduboy","author":"agg23","version":"1"}}}"#;
    const ARD_DATA: &str = r#"{"data":{"data_slots":[
        {"name":"Game","id":0,"required":true,"parameters":"0x113","extensions":["json"]},
        {"name":"HEX","id":1,"required":true,"parameters":"0x101","extensions":["hex"]}
    ]}}"#;
    const ARD_INSTANCE: &str =
        r#"{"instance":{"data_path":"games","data_slots":[{"id":1,"filename":"game1.hex"}]}}"#;

    /// Two cores with data.json (one with an instance file), one without.
    fn bios_card() -> (tempfile::TempDir, Pocket) {
        let (tmp, pocket) = crate::testutil::sd();
        for (rel, body) in [
            ("Cores/sp.GBA/core.json", GBA_CORE),
            ("Cores/sp.GBA/data.json", GBA_DATA),
            ("Cores/agg23.Arduboy/core.json", ARD_CORE),
            ("Cores/agg23.Arduboy/data.json", ARD_DATA),
            ("Cores/nodata.Core/core.json", GBA_CORE),
            ("Assets/arduboy/agg23.Arduboy/game1.json", ARD_INSTANCE),
            (
                "Assets/arduboy/agg23.Arduboy/_alternatives/alt.json",
                ARD_INSTANCE,
            ),
            ("Assets/arduboy/agg23.Arduboy/.hidden.json", ARD_INSTANCE),
        ] {
            write(tmp.path(), rel, body);
        }
        (tmp, pocket)
    }

    #[test]
    fn placing_on_a_swapped_card_is_refused() {
        let (tmp, pocket) = bios_card();
        fs::write(tmp.path().join("game1.hex"), "hex").unwrap();
        let mut plan = plan_place(&pocket, &report(&pocket)).unwrap();
        assert_eq!(plan.actions.len(), 1);
        let aside = tempfile::tempdir().unwrap();
        crate::testutil::swap_card(&pocket, aside.path());
        let err = plan.apply(&pocket, &mut |_| {}).unwrap_err();
        assert_eq!(err.code(), "STALE_PLAN");
        assert!(pocket.root().join("game1.hex").exists());
    }

    fn report(pocket: &Pocket) -> BiosReport {
        let installed = cores::list_installed(pocket).unwrap();
        check(pocket, &installed, None).unwrap()
    }

    fn file<'a>(r: &'a BiosReport, path: &str) -> &'a RequiredFile {
        r.all_files()
            .find(|f| f.path == Path::new(path))
            .unwrap_or_else(|| panic!("no {path} in {r:#?}"))
    }

    #[test]
    fn collects_core_and_instance_slots() {
        let (_tmp, pocket) = bios_card();
        let r = report(&pocket);
        assert_eq!(r.cores.len(), 3);
        let gba = file(&r, "Assets/gba/common/gba_bios.bin");
        assert!(gba.required);
        assert_eq!(gba.status, FileStatus::Missing);
        assert!(gba.expected_md5.is_some());
        // Instance slot resolves via data_path and the core slot's dir, once.
        let hex = file(&r, "Assets/arduboy/common/games/game1.hex");
        assert_eq!(hex.slot, "HEX");
        assert_eq!(
            r.all_files()
                .filter(|f| f.path.ends_with("game1.hex"))
                .count(),
            1
        );
        // .sav is not a user-supplied file.
        assert!(r.all_files().all(|f| !f.path.ends_with("x.sav")));
        let nodata = r.cores.iter().find(|c| c.core_id == "nodata.Core").unwrap();
        assert_eq!(nodata.notes, [Note::NoDataJson]);
        assert_eq!(r.summary.missing, 2);
    }

    #[test]
    fn present_wrong_hash_and_root_files() {
        let (tmp, pocket) = bios_card();
        fs::create_dir_all(tmp.path().join("Assets/gba/common")).unwrap();
        fs::write(tmp.path().join("Assets/gba/common/gba_bios.bin"), "WRONG").unwrap();
        fs::write(tmp.path().join("game1.hex"), "hex").unwrap();
        let r = report(&pocket);
        assert!(matches!(
            file(&r, "Assets/gba/common/gba_bios.bin").status,
            FileStatus::WrongHash { .. }
        ));
        assert_eq!(
            file(&r, "Assets/arduboy/common/games/game1.hex").status,
            FileStatus::AtRoot {
                found: RootFile::File {
                    path: "game1.hex".into()
                }
            }
        );
        assert_eq!(r.summary.wrong_hash, 1);
        assert_eq!(r.summary.placeable, 1);

        fs::write(
            tmp.path().join("Assets/gba/common/gba_bios.bin"),
            "GBA BIOS",
        )
        .unwrap();
        let r = report(&pocket);
        assert_eq!(
            file(&r, "Assets/gba/common/gba_bios.bin").status,
            FileStatus::Present
        );

        let plan = plan_place(&pocket, &r).unwrap();
        assert_eq!(plan.actions.len(), 1);
        assert!(matches!(
            &plan.actions[0],
            crate::card::install::Action::MoveFile { from, to, .. }
                if from == Path::new("game1.hex") && to == Path::new("Assets/arduboy/common/games/game1.hex")
        ));
    }

    #[test]
    fn root_zip_contents_are_reported_not_placed() {
        let (tmp, pocket) = bios_card();
        let zip = build_zip(&[("bios/gba_bios.bin", b"GBA BIOS")]);
        fs::write(tmp.path().join("bios.zip"), zip).unwrap();
        let r = report(&pocket);
        assert_eq!(
            file(&r, "Assets/gba/common/gba_bios.bin").status,
            FileStatus::AtRoot {
                found: RootFile::Zip {
                    zip: "bios.zip".into(),
                    inner: "bios/gba_bios.bin".into()
                }
            }
        );
        let plan = plan_place(&pocket, &r).unwrap();
        assert!(plan.actions.is_empty());
        assert_eq!(plan.zipped.len(), 1);
    }

    #[test]
    fn only_filter_and_unknown_id() {
        let (_tmp, pocket) = bios_card();
        let installed = cores::list_installed(&pocket).unwrap();
        let r = check(&pocket, &installed, Some(&["SP.gba".to_string()])).unwrap();
        assert_eq!(r.cores.len(), 1);
        assert_eq!(r.cores[0].core_id, "sp.GBA");
        let err = check(&pocket, &installed, Some(&["nope.Core".to_string()])).unwrap_err();
        assert_eq!(err.code(), "CORE_NOT_INSTALLED");
    }
}
