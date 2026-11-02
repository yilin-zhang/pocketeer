//! Explicit library → card copies. The card's contents never imply a subscription.

use crate::library::{Library, resolve_rom};
use crate::{
    Error, Pocket, Result,
    card::roms::Index,
    fsutil,
    fsutil::hash::copy_and_hash,
    plan::{Applied, Event, Failure, Plan, Progress},
};
use serde::Serialize;
use std::{fs::File, path::PathBuf};

#[derive(Debug, Clone, Serialize)]
pub struct CopyFile {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub sha1: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CopyPlan {
    #[serde(skip)]
    identity: crate::card::CardIdentity,
    pub files: Vec<CopyFile>,
    pub already_present: Vec<PathBuf>,
    pub warnings: Vec<String>,
}
impl Plan for CopyPlan {
    fn changes(&self) -> usize {
        self.files.len()
    }
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct CopyResult {
    pub copied: usize,
    pub failed: Vec<Failure>,
    pub warnings: Vec<String>,
}

impl Applied for CopyResult {
    fn failures(&self) -> &[Failure] {
        &self.failed
    }
}

pub fn plan(
    lib: &Library,
    pocket: &Pocket,
    platform: Option<&str>,
    specs: &[String],
    progress: Progress<'_>,
) -> Result<CopyPlan> {
    let mut selected = Vec::new();
    for spec in specs {
        let (game, rom) = resolve_rom(lib, platform, spec)?;
        if !selected
            .iter()
            .any(|(_, r): &(crate::library::db::Game, crate::library::db::Rom)| r.id == rom.id)
        {
            selected.push((game, rom));
        }
    }
    let mut index = Index::scan(pocket)?;
    let mut checked = vec![false; index.roms.len()];
    let mut plan = CopyPlan {
        identity: crate::card::CardIdentity::read(pocket)?,
        files: Vec::new(),
        already_present: Vec::new(),
        warnings: index.warning.take().into_iter().collect(),
    };
    for (game, rom) in selected {
        // Confirm a known match first. Only unresolved identities can hide
        // another copy; equal capacity alone is common across unrelated ROMs.
        let mut candidates: Vec<_> = index
            .roms
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                c.platform == game.platform
                    && c.size == rom.size
                    && (c.sha1.is_none() || c.sha1.as_deref() == Some(&rom.sha1))
            })
            .map(|(i, c)| (c.sha1.is_none(), i))
            .collect();
        candidates.sort_unstable();
        let mut found = None;
        for (_, i) in candidates {
            if !checked[i] {
                progress(Event::Status(format!(
                    "Checking existing copy {}",
                    index.roms[i].rel_path.display()
                )));
                index.verify(pocket, i, false, &|| false);
                checked[i] = true;
            }
            if index.roms[i].sha1.as_deref() == Some(&rom.sha1) {
                found = Some(index.roms[i].rel_path.clone());
                break;
            }
        }
        if let Some(path) = found {
            plan.already_present.push(path);
            continue;
        }
        if let Some(error) = index
            .roms
            .iter()
            .find(|c| c.platform == game.platform && c.size == rom.size && c.error.is_some())
        {
            return Err(Error::Mismatch {
                path: pocket.root().join(&error.rel_path),
                what: error.error.clone().unwrap_or_default(),
            });
        }
        crate::fsutil::names::validate_name(&game.platform)?;
        crate::fsutil::names::validate_name(&rom.name)?;
        let destination = crate::card::rom_dir(&game.platform).join(&rom.name);
        if pocket.root().join(&destination).exists() {
            return Err(Error::AlreadyExists { path: destination });
        }
        plan.files.push(CopyFile {
            source: lib.rom_abs_path(&rom),
            destination,
            sha1: rom.sha1,
            size: rom.size,
        });
    }
    if let Err(e) = index.save(pocket) {
        plan.warnings.push(format!("Cache not saved: {e}"));
    }
    Ok(plan)
}

pub fn apply(pocket: &Pocket, plan: &CopyPlan, progress: Progress<'_>) -> Result<CopyResult> {
    plan.identity.check(pocket)?;
    let mut index = Index::scan(pocket)?;
    let mut result = CopyResult::default();
    for (i, file) in plan.files.iter().enumerate() {
        progress(Event::Status(format!(
            "Copying {}",
            file.destination.display()
        )));
        let dest = pocket.root().join(&file.destination);
        let copied = fsutil::write_new_via_temp(&dest, |out| {
            let mut input = File::open(&file.source).map_err(|e| Error::io(&file.source, e))?;
            let hashes = copy_and_hash(&mut input, out).map_err(|e| Error::io(&file.source, e))?;
            if hashes.sha1 != file.sha1 || hashes.size != file.size {
                return Err(Error::Mismatch {
                    path: file.source.clone(),
                    what: "differs from its library identity; reimport it before copying".into(),
                });
            }
            Ok(())
        });
        match copied {
            Ok(()) => {
                result.copied += 1;
                if let Err(e) = index.record_copy(pocket, &file.destination, &file.sha1) {
                    result
                        .warnings
                        .push(format!("Copied ROM, but could not update its cache: {e}"));
                }
            }
            Err(e) => result.failed.push(Failure::new(&file.destination, &e)),
        }
        progress(Event::Step {
            done: i + 1,
            total: plan.files.len(),
        });
    }
    if let Err(e) = index.save(pocket) {
        result.warnings.push(format!("Cache not saved: {e}"));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{library_with, sd, write};

    #[test]
    fn confirmed_copy_does_not_require_checking_unrelated_same_size_roms() {
        let (_tmp, lib) = library_with(&[("local.fkh", "ROM")]);
        let (tmp, pocket) = sd();
        let preview = plan(&lib, &pocket, None, &["local.fkh".into()], &mut |_| {}).unwrap();
        apply(&pocket, &preview, &mut |_| {}).unwrap();
        write(tmp.path(), "Assets/fkh/common/a-unresolved.fkh", "???");

        let preview = plan(&lib, &pocket, None, &["local.fkh".into()], &mut |_| {}).unwrap();
        assert!(preview.files.is_empty());
        assert_eq!(
            preview.already_present,
            [PathBuf::from("Assets/fkh/common/local.fkh")]
        );
        let index = Index::scan(&pocket).unwrap();
        let unresolved = index
            .roms
            .iter()
            .find(|r| r.name == "a-unresolved.fkh")
            .unwrap();
        assert_eq!(
            unresolved.confidence,
            crate::card::roms::Confidence::Pending
        );
    }

    #[test]
    fn explicit_copy_updates_identity_and_recognizes_renamed_copies() {
        let (_tmp, lib) = library_with(&[("local.fkh", "ROM")]);
        let (tmp, pocket) = sd();
        let preview = plan(&lib, &pocket, None, &["local.fkh".into()], &mut |_| {}).unwrap();
        assert_eq!(preview.files.len(), 1);
        let result = apply(&pocket, &preview, &mut |_| {}).unwrap();
        assert_eq!(result.copied, 1);
        assert!(result.failed.is_empty());
        assert!(result.warnings.is_empty());
        assert_eq!(
            Index::scan(&pocket).unwrap().roms[0].confidence,
            crate::card::roms::Confidence::Cached
        );
        std::fs::rename(
            tmp.path().join("Assets/fkh/common/local.fkh"),
            tmp.path().join("Assets/fkh/common/renamed.fkh"),
        )
        .unwrap();
        let preview = plan(&lib, &pocket, None, &["local.fkh".into()], &mut |_| {}).unwrap();
        assert!(preview.files.is_empty());
        assert_eq!(
            preview.already_present,
            [PathBuf::from("Assets/fkh/common/renamed.fkh")]
        );
        write(tmp.path(), "Assets/fkh/common/local.fkh", "other content");
        let result = apply(
            &pocket,
            &CopyPlan {
                identity: crate::card::CardIdentity::read(&pocket).unwrap(),
                files: vec![CopyFile {
                    source: lib.root().join("roms/fkh/local.fkh"),
                    destination: "Assets/fkh/common/local.fkh".into(),
                    sha1: "invalid".into(),
                    size: 3,
                }],
                already_present: Vec::new(),
                warnings: Vec::new(),
            },
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(result.copied, 0);
        assert_eq!(result.failed.len(), 1);
        assert_eq!(
            std::fs::read(tmp.path().join("Assets/fkh/common/local.fkh")).unwrap(),
            b"other content"
        );
    }
}
