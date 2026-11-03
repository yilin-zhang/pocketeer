//! ROMs that are on a card but not in the library, and importing them.
//!
//! Full hashes identify content; unresolved files remain candidates until checked.
//! Files an installed core declares (BIOS etc.) are the core's, not ROMs.

use std::{collections::BTreeMap, path::PathBuf};

use serde::Serialize;

use crate::{
    Pocket, Result,
    card::roms::CardRom,
    catalog::Dat,
    fsutil::{
        Stamp,
        hash::HashCache,
        scan::{Candidate, FileRef},
    },
    plan::{Pinned, Progress},
};

use crate::library::{
    Library,
    import::{self, ImportOptions, ImportPlan, ImportResult},
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
    let comparison = crate::transfer::compare(&games, index.roms());
    Ok(index
        .roms()
        .iter()
        .zip(comparison.card)
        .filter(|(rom, presence)| {
            *presence != crate::transfer::Presence::Both
                && platform.is_none_or(|p| p == rom.platform)
        })
        .map(|(rom, _)| rom.clone())
        .collect())
}

/// Copying card files into the library: an import plan whose files are on
/// the card, for the card it was made on.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CardImportPlan {
    /// The card and the library.
    #[serde(skip)]
    pinned: Pinned,
    #[serde(flatten)]
    pub plan: ImportPlan,
    /// Paths asked for that aren't card-only ROMs; nothing runs while any are.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub not_found: Vec<PathBuf>,
}

impl CardImportPlan {
    pub fn changes(&self) -> usize {
        self.plan.changes()
    }

    pub fn errors(&self) -> usize {
        self.not_found.len()
    }

    /// Copies the planned files into the library, then records their hashes
    /// in the card's ROM cache so it needn't read them again.
    pub fn apply(
        &mut self,
        lib: &mut Library,
        pocket: &Pocket,
        progress: Progress<'_>,
    ) -> Result<ImportResult> {
        self.pinned.check_card(pocket)?;
        self.pinned.check_library(lib)?;
        let mut result = self.plan.apply(lib, progress)?;
        let mut index = crate::card::roms::Index::scan(pocket)?;
        for verified in &result.verified_sources {
            let Ok(relative) = verified.path.strip_prefix(pocket.root()) else {
                continue;
            };
            if Stamp::of(&verified.path).ok() == Some(verified.stamp) {
                index.record_copy(pocket, relative, &verified.sha1);
            }
        }
        index.save(pocket);
        result.warnings.extend(index.take_warnings());
        Ok(result)
    }
}

/// Plans importing `roms` (from [`list`]) into the library. With a DAT the
/// files take DAT names locally; the card keeps its existing names.
/// `not_found`: paths the caller asked for that `roms` doesn't have.
#[allow(clippy::too_many_arguments)]
pub fn plan_import(
    lib: &Library,
    pocket: &Pocket,
    roms: &[CardRom],
    not_found: Vec<PathBuf>,
    dat: Option<&Dat>,
    opts: &ImportOptions,
    cache: &mut HashCache,
    progress: Progress<'_>,
) -> Result<CardImportPlan> {
    let mut groups: BTreeMap<String, Vec<Candidate>> = BTreeMap::new();
    for r in roms {
        groups
            .entry(r.platform.clone())
            .or_default()
            .push(Candidate {
                file: FileRef::plain(r.rel_path.clone()),
                size: r.size,
            });
    }
    let plan = import::plan_import(lib, pocket.root(), &groups, opts, dat, cache, progress)?;
    Ok(CardImportPlan {
        pinned: Pinned::library(lib)?.and_card(pocket)?,
        plan,
        not_found,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::roms::Depth;

    use crate::testutil::{library_with, sd, write};

    #[test]
    fn lists_then_imports_card_only_roms() {
        let (_tmp, lib) = library_with(&[
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
        index.check(&pocket, Depth::Sample, &mut |_| {});
        index.save(&pocket);
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
        let mut plan = plan_import(
            &lib,
            &pocket,
            &all,
            Vec::new(),
            None,
            &ImportOptions::default(),
            &mut cache,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(plan.plan.platforms().len(), 2);
        assert_eq!(plan.changes(), 4);
        let result = plan
            .apply(&mut crate::testutil::reopen(&lib), &pocket, &mut |_| {})
            .unwrap();
        assert_eq!(result.added, 4);
        assert!(lib.root().join("roms/fkh/Only Here.fkh").is_file());
        assert!(lib.root().join("roms/gb/Other.gb").is_file());
        assert!(
            root.join("Assets/fkh/common/Only Here.fkh").is_file(),
            "card untouched"
        );

        let after = list(&lib, &pocket, None).unwrap();
        assert!(after.is_empty(), "{after:?}");
    }
    #[test]
    fn card_import_checks_both_targets_before_copying() {
        let (_tmp, mut lib) = crate::testutil::library_with(&[]);
        let (_card, pocket) = crate::testutil::sd();
        crate::testutil::write(pocket.root(), "Assets/gba/common/game.gba", "ROM");
        let roms = list(&lib, &pocket, None).unwrap();
        let mut plan = plan_import(
            &lib,
            &pocket,
            &roms,
            Vec::new(),
            None,
            &ImportOptions::default(),
            &mut HashCache::in_memory(),
            &mut |_| {},
        )
        .unwrap();
        let (_other, mut other) = crate::testutil::library_with(&[]);
        assert_eq!(
            plan.apply(&mut other, &pocket, &mut |_| {})
                .unwrap_err()
                .code(),
            "STALE_PLAN"
        );
        assert!(other.list_games(None, None).unwrap().is_empty());
        let aside = tempfile::tempdir().unwrap();
        crate::testutil::swap_card(&pocket, aside.path());
        assert_eq!(
            plan.apply(&mut lib, &pocket, &mut |_| {})
                .unwrap_err()
                .code(),
            "STALE_PLAN"
        );
        assert!(lib.list_games(None, None).unwrap().is_empty());
    }
}
