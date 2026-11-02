//! Throwing saves away without losing them. Library copies move to the
//! library's `.trash/<timestamp>/`, like removed ROMs. A save deleted from
//! the card is archived into the library's history first, unless the
//! library already holds those exact bytes.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::Serialize;

use super::{SaveInfo, current, inspect, validate_path, versions, versions_by_id};
use crate::{
    Error, Pocket, Result,
    card::CardIdentity,
    library::{Library, db},
    plan::{Applied, Failure, Plan},
};

/// Which copies of a save to throw away.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(tag = "copy", content = "version", rename_all = "snake_case")]
pub enum TrashTarget {
    /// The file on the card (archived into the library first).
    Card,
    /// The library's current backup; older versions stay.
    Backup,
    /// One older version.
    Version(i64),
    /// Everything the library has: the backup and every older version.
    Library,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TrashFile {
    /// The save this file is a copy of, as a card path.
    pub save: PathBuf,
    /// The file: card-relative for `Card`, library-relative otherwise.
    pub path: PathBuf,
    pub size: u64,
    /// Card files only: copied into the library's history before deletion.
    pub archive_first: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrashPlan {
    pub target: TrashTarget,
    pub files: Vec<TrashFile>,
    /// Where library files go, relative to the library root.
    pub trash: PathBuf,
    #[serde(skip)]
    card: Option<(CardIdentity, BTreeMap<PathBuf, SaveInfo>)>,
    #[serde(skip)]
    library: crate::library::LibraryIdentity,
}

impl Plan for TrashPlan {
    fn changes(&self) -> usize {
        self.files.len()
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TrashResult {
    pub trashed: usize,
    pub archived: usize,
    /// Set when library files were moved there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trash: Option<PathBuf>,
    pub failed: Vec<Failure>,
}

impl Applied for TrashResult {
    fn failures(&self) -> &[Failure] {
        &self.failed
    }
}

/// Plans throwing away the `target` copies of the saves at card paths
/// `saves` (one save, or a Memory with its companion files).
pub fn plan_trash(
    lib: &Library,
    pocket: Option<&Pocket>,
    saves: &[PathBuf],
    target: TrashTarget,
) -> Result<TrashPlan> {
    let mut plan = TrashPlan {
        target,
        files: Vec::new(),
        trash: Library::new_trash_dir(),
        card: None,
        library: lib.identity()?,
    };
    for rel in saves {
        validate_path(rel)?;
    }
    match target {
        TrashTarget::Card => {
            let pocket = pocket.ok_or(Error::SdNotFound {
                searched: Vec::new(),
            })?;
            let mut observed = BTreeMap::new();
            for rel in saves {
                let Some(info) = inspect(&pocket.root().join(rel))? else {
                    continue;
                };
                let kept = inspect(&current(lib, rel))?.is_some_and(|c| c.sha1 == info.sha1)
                    || versions(lib, rel)?.iter().any(|v| v.sha1 == info.sha1);
                plan.files.push(TrashFile {
                    save: rel.clone(),
                    path: rel.clone(),
                    size: info.size,
                    archive_first: !kept,
                });
                observed.insert(rel.clone(), info);
            }
            plan.card = Some((CardIdentity::read(pocket)?, observed));
        }
        TrashTarget::Version(id) => {
            let version = versions_by_id(lib, id)?;
            if !saves.contains(&version.rel_path) {
                return Err(Error::InvalidPath {
                    path: saves.first().cloned().unwrap_or_default(),
                    reason: format!(
                        "save version {id} belongs to {}",
                        version.rel_path.display()
                    ),
                });
            }
            plan.files.push(TrashFile {
                save: version.rel_path,
                path: version.stored_path,
                size: version.size,
                archive_first: false,
            });
        }
        TrashTarget::Backup | TrashTarget::Library => {
            for rel in saves {
                let backup = Path::new("saves").join(rel);
                if let Ok(meta) = std::fs::metadata(lib.root().join(&backup)) {
                    plan.files.push(TrashFile {
                        save: rel.clone(),
                        path: backup,
                        size: meta.len(),
                        archive_first: false,
                    });
                }
                if target == TrashTarget::Library {
                    for v in versions(lib, rel)? {
                        plan.files.push(TrashFile {
                            save: rel.clone(),
                            path: v.stored_path,
                            size: v.size,
                            archive_first: false,
                        });
                    }
                }
            }
        }
    }
    Ok(plan)
}

pub fn apply_trash(
    lib: &mut Library,
    pocket: Option<&Pocket>,
    plan: &TrashPlan,
) -> Result<TrashResult> {
    lib.check(&plan.library)?;
    let mut result = TrashResult::default();
    if let Some((identity, observed)) = &plan.card {
        let pocket = pocket.ok_or(Error::SdNotFound {
            searched: Vec::new(),
        })?;
        identity.check(pocket)?;
        for file in &plan.files {
            match delete_from_card(lib, pocket, file, &observed[&file.path]) {
                Ok(archived) => {
                    result.trashed += 1;
                    result.archived += usize::from(archived);
                }
                Err(e) => result.failed.push(Failure {
                    path: file.path.clone(),
                    error: e.to_info(),
                }),
            }
        }
        return Ok(result);
    }
    let root = lib.root().to_path_buf();
    let trash = root.join(&plan.trash);
    for file in &plan.files {
        let moved = move_to_trash(&root, &trash, &file.path).and_then(|_| match plan.target {
            TrashTarget::Version(id) => db::delete_save_version(lib.conn(), id),
            TrashTarget::Library => forget_versions(lib, &file.save),
            _ => Ok(()),
        });
        match moved {
            Ok(()) => result.trashed += 1,
            Err(e) => result.failed.push(Failure {
                path: file.path.clone(),
                error: e.to_info(),
            }),
        }
    }
    result.trash = trash.exists().then(|| plan.trash.clone());
    Ok(result)
}

fn delete_from_card(
    lib: &Library,
    pocket: &Pocket,
    file: &TrashFile,
    planned: &SaveInfo,
) -> Result<bool> {
    let path = pocket.root().join(&file.path);
    if inspect(&path)?.as_ref() != Some(planned) {
        return Err(Error::Stale {
            what: file.path.display().to_string(),
        });
    }
    let archived = file.archive_first && super::archive(lib, &file.path, &path, planned)?;
    std::fs::remove_file(&path).map_err(|e| Error::io(&path, e))?;
    Ok(archived)
}

/// Forgets the history of a save once its last file is gone.
fn forget_versions(lib: &Library, save: &Path) -> Result<()> {
    for v in versions(lib, save)? {
        if !lib.root().join(&v.stored_path).exists() {
            db::delete_save_version(lib.conn(), v.id)?;
        }
    }
    if versions(lib, save)?.is_empty() && !current(lib, save).exists() {
        db::forget_save_game(lib.conn(), save)?;
    }
    Ok(())
}

fn move_to_trash(root: &Path, trash: &Path, rel: &Path) -> Result<()> {
    let from = root.join(rel);
    if !from.exists() {
        return Ok(());
    }
    let to = trash.join(rel);
    std::fs::create_dir_all(to.parent().unwrap_or(trash)).map_err(|e| Error::io(trash, e))?;
    std::fs::rename(&from, &to).map_err(|e| Error::io(&from, e))
}

#[cfg(test)]
mod tests {
    use super::super::{SavesOptions, apply, plan};
    use super::*;
    use crate::testutil::{sd, write_with_mtime as write};

    const REL: &str = "Saves/gba/common/game.sav";

    fn back_up(lib: &mut Library, pocket: &Pocket) {
        let p = plan(lib, pocket, SavesOptions::default(), &mut |_| {}).unwrap();
        apply(lib, pocket, &p, &mut |_| {}).unwrap();
    }

    #[test]
    fn trashing_each_copy_keeps_something_to_recover() {
        let (tmp, pocket) = sd();
        let mut lib = Library::init(&tmp.path().join("library")).unwrap();
        let rel = PathBuf::from(REL);
        write(pocket.root(), REL, "one", 100);
        back_up(&mut lib, &pocket);
        write(pocket.root(), REL, "two", 200);
        back_up(&mut lib, &pocket);
        write(pocket.root(), REL, "three, never backed up", 300);

        // Deleting from the card archives the unbacked bytes first.
        let p = plan_trash(
            &lib,
            Some(&pocket),
            std::slice::from_ref(&rel),
            TrashTarget::Card,
        )
        .unwrap();
        assert!(p.files[0].archive_first);
        let r = apply_trash(&mut lib, Some(&pocket), &p).unwrap();
        assert_eq!((r.trashed, r.archived), (1, 1));
        assert!(!pocket.root().join(REL).exists());
        assert_eq!(versions(&lib, &rel).unwrap().len(), 2);

        // One older version goes to the library's trash.
        let oldest = versions(&lib, &rel).unwrap().pop().unwrap();
        let p = plan_trash(
            &lib,
            None,
            std::slice::from_ref(&rel),
            TrashTarget::Version(oldest.id),
        )
        .unwrap();
        let r = apply_trash(&mut lib, None, &p).unwrap();
        let trash = lib.root().join(r.trash.unwrap());
        assert_eq!(
            std::fs::read(trash.join(&oldest.stored_path)).unwrap(),
            b"one"
        );
        assert_eq!(versions(&lib, &rel).unwrap().len(), 1);

        // Everything else in the library.
        let p = plan_trash(&lib, None, std::slice::from_ref(&rel), TrashTarget::Library).unwrap();
        assert_eq!(p.files.len(), 2);
        apply_trash(&mut lib, None, &p).unwrap();
        assert!(versions(&lib, &rel).unwrap().is_empty());
        assert!(!current(&lib, &rel).exists());
    }

    #[test]
    fn a_card_save_the_library_has_is_deleted_without_a_new_version() {
        let (tmp, pocket) = sd();
        let mut lib = Library::init(&tmp.path().join("library")).unwrap();
        write(pocket.root(), REL, "same", 100);
        back_up(&mut lib, &pocket);
        let rel = PathBuf::from(REL);
        let p = plan_trash(
            &lib,
            Some(&pocket),
            std::slice::from_ref(&rel),
            TrashTarget::Card,
        )
        .unwrap();
        assert!(!p.files[0].archive_first);
        // Changed after the plan: refused.
        write(pocket.root(), REL, "changed", 500);
        let r = apply_trash(&mut lib, Some(&pocket), &p).unwrap();
        assert_eq!(r.failed.len(), 1);
        assert!(pocket.root().join(REL).exists());
    }
}
