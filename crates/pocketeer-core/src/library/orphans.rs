//! ROMs that are on a card but not in the library, and importing them.
//!
//! A card file counts as "in the library" when a library ROM on the same
//! platform has the same name and size, or (hashing only then) the same
//! crc32 under another name — that one is a pending rename, not an orphan.
//! Files an installed core declares (BIOS etc.) are the core's, not ROMs.

use std::{
    collections::{BTreeSet, HashMap},
    path::PathBuf,
};

use serde::Serialize;

use crate::{
    Pocket, Result,
    fsutil::{is_junk, is_sidecar},
    progress::{Event, Progress},
    required,
    roms::{FileRef, card, dat::Dat, hash::HashCache, verify::Candidate},
};

use super::{
    Library, db,
    import::{self, ImportMode, ImportOptions, ImportPlan},
};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CardOnlyRom {
    pub platform: String,
    /// `common` or a core id.
    pub owner: String,
    /// Relative to the SD root.
    pub rel_path: PathBuf,
    pub name: String,
    pub size: u64,
    /// A library ROM has this name but different bytes.
    pub name_in_library: bool,
}

/// Every ROM-ish file on the card: `Assets/<platform>/<owner>/*`, minus files
/// installed cores declare (BIOS…), sidecars (instance JSONs…) and junk.
pub fn scan_card(pocket: &Pocket, platform: Option<&str>) -> Result<Vec<CardOnlyRom>> {
    let declared = required::protected_paths(pocket)?;
    let mut out = Vec::new();
    for plat in card::scan(pocket)? {
        if platform.is_some_and(|p| p != plat.platform) {
            continue;
        }
        for dir in &plat.dirs {
            for f in &dir.files {
                let rel = dir.path.join(&f.name);
                if declared.contains(&rel) || is_sidecar(&rel) || is_junk(&rel) {
                    continue;
                }
                out.push(CardOnlyRom {
                    platform: plat.platform.clone(),
                    owner: dir.owner.clone(),
                    rel_path: rel,
                    name: f.name.clone(),
                    size: f.size,
                    name_in_library: false,
                });
            }
        }
    }
    Ok(out)
}

/// Card files under `Assets/<platform>/<owner>/` the library doesn't hold,
/// by name and size only: nothing is read from the card. A file that a
/// recorded rename maps onto a library ROM counts as that ROM (it gets
/// renamed on the next sync).
pub fn list(lib: &Library, pocket: &Pocket, platform: Option<&str>) -> Result<Vec<CardOnlyRom>> {
    let mut out = Vec::new();
    let mut index: Option<PlatformIndex> = None;
    for mut rom in scan_card(pocket, platform)? {
        if index.as_ref().is_none_or(|i| i.platform != rom.platform) {
            index = Some(PlatformIndex::load(lib, &rom.platform)?);
        }
        let idx = index.as_ref().expect("set above");
        if idx.by_name.get(&rom.name) == Some(&rom.size) {
            continue;
        }
        if idx
            .renamed
            .get(&rom.name)
            .is_some_and(|new| idx.by_name.get(new) == Some(&rom.size))
        {
            continue;
        }
        rom.name_in_library = idx.by_name.contains_key(&rom.name);
        out.push(rom);
    }
    Ok(out)
}

/// Like [`list`], then reads every candidate whose size matches a library
/// ROM and drops the ones with the same crc32 (renamed copies the library
/// doesn't know about). Slow over USB; opt-in.
pub fn list_verified(
    lib: &Library,
    pocket: &Pocket,
    platform: Option<&str>,
    cache: &mut HashCache,
    progress: Progress<'_>,
) -> Result<Vec<CardOnlyRom>> {
    let candidates = list(lib, pocket, platform)?;
    let total = candidates.len();
    let mut out = Vec::new();
    let mut index: Option<PlatformIndex> = None;
    for (i, rom) in candidates.into_iter().enumerate() {
        if index.as_ref().is_none_or(|x| x.platform != rom.platform) {
            index = Some(PlatformIndex::load(lib, &rom.platform)?);
        }
        let idx = index.as_ref().expect("set above");
        let Some(crcs) = idx.by_size.get(&rom.size) else {
            out.push(rom);
            continue;
        };
        progress(Event::Step { done: i + 1, total });
        progress(Event::Status(format!(
            "Hashing {} ({}/{total})",
            rom.name,
            i + 1
        )));
        let same = cache
            .file(&pocket.root().join(&rom.rel_path))
            .is_ok_and(|h| crcs.contains(&h.crc32));
        if !same {
            out.push(rom);
        }
    }
    cache.save_best_effort();
    Ok(out)
}

/// What the library holds for one platform, keyed two ways for cheap matching.
struct PlatformIndex {
    platform: String,
    /// file name → size
    by_name: HashMap<String, u64>,
    /// size → crc32s of every ROM that size
    by_size: HashMap<u64, Vec<u32>>,
    /// old file name → current name, from the rename log (chains followed)
    renamed: HashMap<String, String>,
}

impl PlatformIndex {
    fn load(lib: &Library, platform: &str) -> Result<Self> {
        let games = db::list_games(lib.conn(), Some(platform), None)?;
        let mut renamed: HashMap<String, String> = HashMap::new();
        for (old, new) in super::rename::pending_renames(lib.conn(), platform)? {
            // A→B then B→C: anything that pointed at B now points at C.
            for v in renamed.values_mut() {
                if *v == old {
                    *v = new.clone();
                }
            }
            renamed.insert(old, new);
        }
        let mut idx = PlatformIndex {
            platform: platform.to_string(),
            by_name: HashMap::new(),
            by_size: HashMap::new(),
            renamed,
        };
        for r in games.iter().flat_map(|g| g.roms.iter()) {
            idx.by_name.insert(r.name.clone(), r.size);
            if let Some(crc) = r.crc32_u32() {
                idx.by_size.entry(r.size).or_default().push(crc);
            }
        }
        Ok(idx)
    }
}

/// One import plan per platform, copying the given card files into the library.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CardImportPlan {
    pub parts: Vec<CardImportPart>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CardImportPart {
    pub platform: String,
    #[serde(skip)]
    pub options: ImportOptions,
    #[serde(flatten)]
    pub plan: ImportPlan,
}

impl CardImportPlan {
    pub fn changes(&self) -> usize {
        self.parts.iter().map(|p| p.plan.changes()).sum()
    }
}

/// Plans importing `roms` (from [`list`]) into the library. With a DAT the
/// files take DAT names; the card's copies are then renamed by the next sync.
pub fn plan_import(
    lib: &Library,
    pocket: &Pocket,
    roms: &[CardOnlyRom],
    dat: Option<&Dat>,
    platform_names: &dyn Fn(&str) -> Option<String>,
    cache: &mut HashCache,
    progress: Progress<'_>,
) -> Result<CardImportPlan> {
    let platforms: BTreeSet<&str> = roms.iter().map(|r| r.platform.as_str()).collect();
    let mut parts = Vec::new();
    for platform in platforms {
        let candidates: Vec<Candidate> = roms
            .iter()
            .filter(|r| r.platform == platform)
            .map(|r| Candidate {
                file: FileRef::plain(r.rel_path.clone()),
                size: r.size,
            })
            .collect();
        let options = ImportOptions {
            platform: platform.to_string(),
            platform_name: platform_names(platform),
            mode: ImportMode::Copy,
            dat_only: false,
        };
        let plan = import::plan_import_candidates(
            lib,
            pocket.root(),
            &candidates,
            &options,
            dat,
            cache,
            progress,
        )?;
        parts.push(CardImportPart {
            platform: platform.to_string(),
            options,
            plan,
        });
    }
    Ok(CardImportPlan { parts })
}

/// What importing one platform's part of a [`CardImportPlan`] produced.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PlatformImport {
    pub platform: String,
    pub result: import::ImportResult,
}

pub fn apply_import(
    lib: &mut Library,
    plan: &CardImportPlan,
    progress: Progress<'_>,
) -> Result<Vec<PlatformImport>> {
    let mut out = Vec::new();
    for part in &plan.parts {
        if part.plan.changes() == 0 {
            continue;
        }
        let result = import::apply_import(lib, &part.options, &part.plan, progress)?;
        out.push(PlatformImport {
            platform: part.platform.clone(),
            result,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::testutil::{library_with, sd, write};

    #[test]
    fn lists_then_imports_card_only_roms() {
        let (_tmp, mut lib) = library_with(&[
            ("In Library.fkh", "hello world"),
            ("Renamed Later.fkh", "22"),
        ]);

        let (_sd, pocket) = sd();
        let root = pocket.root();
        write(root, "Assets/fkh/common/In Library.fkh", "hello world");
        write(root, "Assets/fkh/common/old name.fkh", "22"); // same bytes → pending rename
        write(root, "Assets/fkh/common/Only Here.fkh", "only here");
        write(root, "Assets/fkh/common/In Library (v2).fkh", "hello wOrld"); // same size, other crc
        write(root, "Assets/fkh/x.Core/game.fkh", "core folder");
        write(root, "Assets/gb/common/Other.gb", "gb");
        write(
            root,
            "Cores/x.Core/core.json",
            r#"{"core":{"magic":"APF_VER_1","metadata":{"platform_ids":["fkh"],"shortname":"X","author":"x","version":"1"},"cores":[{"name":"x","id":0,"filename":"x.rev"}]}}"#,
        );
        write(
            root,
            "Cores/x.Core/data.json",
            r#"{"data":{"magic":"APF_VER_1","data_slots":[{"id":0,"name":"BIOS","required":true,"parameters":"0x100","filename":"fkh_bios.bin"}]}}"#,
        );
        write(root, "Assets/fkh/common/fkh_bios.bin", "bios bytes");

        let mut cache = HashCache::in_memory();
        // Without reading files, "old name" (same bytes as a library ROM) is
        // still an orphan…
        let names = |v: &[CardOnlyRom]| -> Vec<String> {
            v.iter()
                .map(|r| format!("{}/{}", r.platform, r.name))
                .collect()
        };
        assert_eq!(
            names(&list(&lib, &pocket, None).unwrap()),
            [
                "fkh/In Library (v2).fkh",
                "fkh/old name.fkh",
                "fkh/Only Here.fkh",
                "fkh/game.fkh",
                "gb/Other.gb",
            ]
        );
        // …unless the library remembers the rename…
        super::super::rename::record_rename(lib.conn(), "fkh", "old name.fkh", "Renamed Later.fkh")
            .unwrap();
        let all = list(&lib, &pocket, None).unwrap();
        assert_eq!(
            names(&all),
            [
                "fkh/In Library (v2).fkh",
                "fkh/Only Here.fkh",
                "fkh/game.fkh",
                "gb/Other.gb",
            ]
        );
        assert_eq!(all[2].owner, "x.Core");
        // …or the caller pays for a content check.
        lib.conn().execute("DELETE FROM renames", []).unwrap();
        let verified = list_verified(&lib, &pocket, None, &mut cache, &mut |_| {}).unwrap();
        assert_eq!(names(&verified), names(&all));
        let only_gb = list(&lib, &pocket, Some("gb")).unwrap();
        assert_eq!(only_gb.len(), 1);
        super::super::rename::record_rename(lib.conn(), "fkh", "old name.fkh", "Renamed Later.fkh")
            .unwrap();

        let plan = plan_import(
            &lib,
            &pocket,
            &all,
            None,
            &|_| None,
            &mut cache,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(plan.parts.len(), 2);
        assert_eq!(plan.changes(), 4);
        let results = apply_import(&mut lib, &plan, &mut |_| {}).unwrap();
        assert_eq!(results.iter().map(|p| p.result.added).sum::<usize>(), 4);
        assert!(lib.root().join("roms/fkh/Only Here.fkh").is_file());
        assert!(lib.root().join("roms/gb/Other.gb").is_file());
        assert!(
            root.join("Assets/fkh/common/Only Here.fkh").is_file(),
            "card untouched"
        );

        let after = list(&lib, &pocket, None).unwrap();
        assert!(after.is_empty(), "{after:?}");
    }
}
