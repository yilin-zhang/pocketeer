//! ROMs that are on a card but not in the library, and importing them.
//!
//! Full hashes identify content; unresolved files remain candidates until checked.
//! Files an installed core declares (BIOS etc.) are the core's, not ROMs.

use std::collections::BTreeSet;

use serde::Serialize;

use crate::{
    Pocket, Result,
    card::roms::CardRom,
    catalog::Dat,
    fsutil::{
        hash::HashCache,
        scan::{Candidate, FileRef},
    },
    plan::{Applied, Plan, Progress},
};

use crate::library::{
    Library,
    import::{self, ImportOptions, ImportPlan},
};

/// Card files not yet matched to library content, including unresolved files.
pub fn list(lib: &Library, pocket: &Pocket, platform: Option<&str>) -> Result<Vec<CardRom>> {
    let index = crate::card::roms::Index::scan(pocket)?;
    unmatched(lib, &index, platform)
}

fn unmatched(
    lib: &Library,
    index: &crate::card::roms::Index,
    platform: Option<&str>,
) -> Result<Vec<CardRom>> {
    let games = lib.list_games(platform, None)?;
    let comparison = crate::transfer::compare(&games, &index.roms);
    Ok(index
        .roms
        .iter()
        .zip(comparison.card)
        .filter(|(rom, presence)| {
            *presence != crate::transfer::Presence::Both
                && platform.is_none_or(|p| p == rom.platform)
        })
        .map(|(rom, _)| rom.clone())
        .collect())
}

/// One import plan per platform, copying the given card files into the library.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CardImportPlan {
    #[serde(skip)]
    identity: crate::card::CardIdentity,
    /// One plan per platform.
    pub parts: Vec<ImportPlan>,
}

impl Plan for CardImportPlan {
    fn changes(&self) -> usize {
        self.parts.iter().map(|p| p.changes()).sum()
    }
}

/// Plans importing `roms` (from [`list`]) into the library. With a DAT the
/// files take DAT names locally; the card keeps its existing names.
pub fn plan_import(
    lib: &Library,
    pocket: &Pocket,
    roms: &[CardRom],
    dat: Option<&Dat>,
    options_for: &dyn Fn(&str) -> ImportOptions,
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
        parts.push(import::plan_import_candidates(
            lib,
            pocket.root(),
            &candidates,
            &options_for(platform),
            dat,
            cache,
            progress,
        )?);
    }
    Ok(CardImportPlan {
        parts,
        identity: crate::card::CardIdentity::read(pocket)?,
    })
}

/// What importing one platform's part of a [`CardImportPlan`] produced.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PlatformImport {
    pub platform: String,
    pub result: import::ImportResult,
    pub warnings: Vec<String>,
}

impl Applied for Vec<PlatformImport> {
    fn failed(&self) -> usize {
        self.iter().map(|p| p.result.failed.len()).sum()
    }
}

pub fn apply_import(
    lib: &mut Library,
    pocket: &Pocket,
    plan: &CardImportPlan,
    progress: Progress<'_>,
) -> Result<Vec<PlatformImport>> {
    plan.identity.check(pocket)?;
    let mut out = Vec::new();
    let mut index = crate::card::roms::Index::scan(pocket)?;
    for part in &plan.parts {
        if part.changes() == 0 {
            continue;
        }
        let result = import::apply_import(lib, part, progress)?;
        let mut warnings = Vec::new();
        for verified in &result.verified_sources {
            let Ok(relative) = verified.path.strip_prefix(pocket.root()) else {
                continue;
            };
            if std::fs::metadata(&verified.path).is_ok_and(|m| {
                m.len() == verified.size && m.modified().ok() == Some(verified.modified)
            }) {
                if let Err(e) = index.record_copy(pocket, relative, &verified.sha1) {
                    warnings.push(format!("Imported ROM, but cache was not updated: {e}"));
                }
            }
        }
        if let Err(e) = index.save(pocket) {
            warnings.push(format!("Cache not saved: {e}"));
        }
        out.push(PlatformImport {
            platform: part.platform.clone(),
            result,
            warnings,
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
        write(root, "Assets/fkh/common/old name.fkh", "22"); // same content under a different name
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
        let names = |v: &[CardRom]| -> Vec<String> {
            v.iter()
                .map(|r| format!("{}/{}", r.platform, r.name))
                .collect()
        };
        assert_eq!(list(&lib, &pocket, None).unwrap().len(), 6);
        let mut index = crate::card::roms::Index::scan(&pocket).unwrap();
        for i in 0..index.roms.len() {
            index.verify(&pocket, i, false, &|| false);
        }
        index.save(&pocket).unwrap();
        let all = list(&lib, &pocket, None).unwrap();
        assert_eq!(
            names(&all),
            [
                "fkh/In Library (v2).fkh",
                "fkh/Only Here.fkh",
                "fkh/game.fkh",
                "gb/Other.gb"
            ]
        );
        assert_eq!(all[2].owner, "x.Core");
        assert_eq!(list(&lib, &pocket, Some("gb")).unwrap().len(), 1);
        let plan = plan_import(
            &lib,
            &pocket,
            &all,
            None,
            &|platform| ImportOptions {
                platform: platform.into(),
                platform_name: None,
                dat_only: false,
            },
            &mut cache,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(plan.parts.len(), 2);
        assert_eq!(plan.changes(), 4);
        let results = apply_import(&mut lib, &pocket, &plan, &mut |_| {}).unwrap();
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
