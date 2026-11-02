//! Executes a list of [`Action`]s on the SD card, safely.
//!
//! Everything is staged on the same volume first, every replaced or removed
//! file is moved aside into an undo folder, and all changes are plain renames.
//! If any step fails, the journal is replayed backwards to restore the card;
//! when everything succeeds the undo folder is deleted. Nothing lingers on
//! the card: what pocketeer removes is gone (the library holds the copies).

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{
    Error, Pocket, Result,
    plan::{Applied, Event, Progress},
};

use super::plan::Action;
use crate::fsutil;

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Changes {
    pub installed: usize,
    pub removed: usize,
    pub moved_files: usize,
}

impl Applied for Changes {
    /// Applying either succeeds or rolls back and errors.
    fn failed(&self) -> usize {
        0
    }
}

/// Something we did and know how to undo.
#[derive(Debug)]
enum Done {
    /// `dest` now holds a new file; `aside` holds what was there before.
    Placed {
        dest: PathBuf,
        aside: Option<PathBuf>,
    },
    /// `from` was moved to `aside`.
    Removed { from: PathBuf, aside: PathBuf },
    /// A file was renamed `from` → `to`; `aside` holds whatever `to` was.
    MovedFile {
        from: PathBuf,
        to: PathBuf,
        aside: Option<PathBuf>,
    },
}

/// Source of file contents for `Install` actions: writes the zip entry
/// named by `zip_path` to the given staging path.
pub type Extractor<'a> = &'a mut dyn FnMut(&str, &Path) -> Result<()>;

pub fn apply(
    pocket: &Pocket,
    label: &str,
    actions: &[Action],
    extract: Extractor<'_>,
    progress: Progress<'_>,
) -> Result<Changes> {
    let root = pocket.root();
    let staging = new_staging_dir(pocket, label);
    let undo_dir = new_undo_dir(pocket, label);

    // Stage everything first so a bad zip can't leave a half-written core.
    let installs: Vec<(&str, &Path)> = actions
        .iter()
        .filter_map(|a| match a {
            Action::Install { zip_path, dest, .. } => Some((zip_path.as_str(), dest.as_path())),
            _ => None,
        })
        .collect();
    if !installs.is_empty() {
        progress(Event::Status(format!(
            "Extracting {} files",
            installs.len()
        )));
        std::fs::create_dir_all(&staging).map_err(|e| Error::io(&staging, e))?;
        for (i, (zip_path, dest)) in installs.iter().enumerate() {
            if let Err(err) = extract(zip_path, &staging.join(dest)) {
                cleanup_staging(&staging);
                return Err(err);
            }
            progress(Event::Step {
                done: i + 1,
                total: installs.len(),
            });
        }
    }

    let total = actions
        .iter()
        .filter(|a| !matches!(a, Action::Skip { .. }))
        .count();
    progress(Event::Status(format!("Applying {total} changes")));

    let mut journal: Vec<Done> = Vec::new();
    let mut applied = Changes::default();
    let mut done = 0;

    let result: Result<()> = (|| {
        for action in actions {
            match action {
                Action::Skip { .. } => continue,
                Action::Install { dest, .. } => {
                    let staged = staging.join(dest);
                    let target = root.join(dest);
                    let aside = move_aside(root, &undo_dir, dest)?;
                    ensure_parent(&target)?;
                    rename(&staged, &target)?;
                    journal.push(Done::Placed {
                        dest: target,
                        aside,
                    });
                    applied.installed += 1;
                }
                Action::Remove { path, .. } => {
                    if let Some(aside) = move_aside(root, &undo_dir, path)? {
                        journal.push(Done::Removed {
                            from: root.join(path),
                            aside,
                        });
                        applied.removed += 1;
                    }
                }
                Action::MoveFile { from, to, .. } => {
                    let src = root.join(from);
                    if !src.is_file() {
                        continue;
                    }
                    let target = root.join(to);
                    let aside = move_aside(root, &undo_dir, to)?;
                    ensure_parent(&target)?;
                    rename(&src, &target)?;
                    journal.push(Done::MovedFile {
                        from: src,
                        to: target,
                        aside,
                    });
                    applied.moved_files += 1;
                }
                Action::MoveTree { from, to, .. } => {
                    let src_root = root.join(from);
                    if !src_root.exists() {
                        continue;
                    }
                    let files: Vec<PathBuf> = walkdir::WalkDir::new(&src_root)
                        .into_iter()
                        .filter_map(|e| e.ok())
                        .filter(|e| e.file_type().is_file())
                        .map(|e| e.into_path())
                        .collect();
                    for file in files {
                        let rel = file.strip_prefix(&src_root).expect("under src_root");
                        let target_rel = to.join(rel);
                        let target = root.join(&target_rel);
                        let aside = move_aside(root, &undo_dir, &target_rel)?;
                        ensure_parent(&target)?;
                        rename(&file, &target)?;
                        journal.push(Done::MovedFile {
                            from: file,
                            to: target,
                            aside,
                        });
                        applied.moved_files += 1;
                    }
                    remove_empty_dirs(&src_root);
                }
            }
            done += 1;
            progress(Event::Step { done, total });
        }
        Ok(())
    })();

    cleanup_staging(&staging);

    match result {
        Ok(()) => {
            cleanup_staging(&undo_dir);
            Ok(applied)
        }
        Err(err) => {
            let rolled_back = rollback(&journal).is_ok();
            if rolled_back {
                cleanup_staging(&undo_dir);
            }
            Err(Error::ApplyFailed {
                step: done + 1,
                rolled_back,
                source: Box::new(err),
            })
        }
    }
}

fn rollback(journal: &[Done]) -> Result<()> {
    for entry in journal.iter().rev() {
        match entry {
            Done::Placed { dest, aside } => {
                std::fs::remove_file(dest).map_err(|e| Error::io(dest, e))?;
                if let Some(b) = aside {
                    rename(b, dest)?;
                }
            }
            Done::Removed { from, aside } => {
                ensure_parent(from)?;
                rename(aside, from)?;
            }
            Done::MovedFile { from, to, aside } => {
                ensure_parent(from)?;
                rename(to, from)?;
                if let Some(b) = aside {
                    rename(b, to)?;
                }
            }
        }
    }
    Ok(())
}

/// If `<root>/<rel>` exists, moves it to `<undo_dir>/<rel>` and returns that path.
fn move_aside(root: &Path, undo_dir: &Path, rel: &Path) -> Result<Option<PathBuf>> {
    let src = root.join(rel);
    if !src.exists() {
        return Ok(None);
    }
    let dest = undo_dir.join(rel);
    ensure_parent(&dest)?;
    if dest.exists() {
        // Same relative path moved aside twice in one run (shouldn't happen); keep both.
        let alt = dest.with_file_name(format!(
            "{}.{}",
            dest.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
            std::process::id()
        ));
        rename(&src, &alt)?;
        return Ok(Some(alt));
    }
    rename(&src, &dest)?;
    Ok(Some(dest))
}

fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }
    Ok(())
}

fn rename(from: &Path, to: &Path) -> Result<()> {
    std::fs::rename(from, to).map_err(|e| Error::io(from, e))
}

/// Removes `dir` and any now-empty ancestors... but only `dir` itself and its
/// empty children; parents are left alone (they're platform folders).
fn remove_empty_dirs(dir: &Path) {
    for entry in walkdir::WalkDir::new(dir)
        .contents_first(true)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.file_type().is_dir() {
            let _ = std::fs::remove_dir(entry.path());
        }
    }
}

/// Removes the staging dir, then `staging/` and `.pocketeer/` if they are empty.
fn cleanup_staging(staging: &Path) {
    let _ = std::fs::remove_dir_all(staging);
    if let Some(parent) = staging.parent() {
        let _ = std::fs::remove_dir(parent);
        if let Some(grandparent) = parent.parent() {
            let _ = std::fs::remove_dir(grandparent);
        }
    }
}

// ---------------------------------------------------------------------------
// Staging space under the card's `.pocketeer/` folder. Nothing is kept
// here after an operation finishes: the library is the backup, the card is
// just a card.

pub fn staging_root(pocket: &Pocket) -> PathBuf {
    pocket.pocketeer_dir().join("staging")
}

/// Where files displaced by one operation wait until it succeeds (then the
/// folder is deleted) or fails (then they are moved back).
pub fn new_undo_dir(pocket: &Pocket, label: &str) -> PathBuf {
    staging_root(pocket).join(format!(
        "{}-{}-{}-undo",
        fsutil::timestamp(),
        sanitize(label),
        std::process::id()
    ))
}

/// A new, unique staging directory path for an operation (not yet created).
pub fn new_staging_dir(pocket: &Pocket, label: &str) -> PathBuf {
    staging_root(pocket).join(format!(
        "{}-{}-{}",
        fsutil::timestamp(),
        sanitize(label),
        std::process::id()
    ))
}

fn sanitize(label: &str) -> String {
    label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(60)
        .collect()
}

#[cfg(test)]
mod staging_tests {
    use super::*;

    #[test]
    fn sanitizes_labels() {
        assert_eq!(sanitize("agg23.Arduboy"), "agg23.Arduboy");
        assert_eq!(sanitize("a b/c"), "a_b_c");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::install::plan::{MoveReason, RemoveReason};
    use std::{collections::HashMap, fs};

    use crate::testutil::{sd, write};

    fn read(root: &Path, rel: &str) -> Option<String> {
        fs::read_to_string(root.join(rel)).ok()
    }

    fn extractor(
        contents: HashMap<&'static str, &'static str>,
    ) -> impl FnMut(&str, &Path) -> Result<()> {
        move |zip_path, dest| {
            let body = contents.get(zip_path).ok_or_else(|| Error::UnsafeZipPath {
                name: zip_path.into(),
            })?;
            fs::create_dir_all(dest.parent().unwrap()).unwrap();
            fs::write(dest, body).unwrap();
            Ok(())
        }
    }

    fn install(zip_path: &str, dest: &str, overwrite: bool) -> Action {
        Action::Install {
            zip_path: zip_path.into(),
            dest: dest.into(),
            overwrite,
            size: 1,
        }
    }

    #[test]
    fn installs_backs_up_removes_and_moves() {
        let (tmp, pocket) = sd();
        let root = tmp.path();
        write(root, "Cores/a.B/core.json", "old");
        write(root, "Cores/a.B/sub/a.rev", "stale");
        write(root, "Assets/p/old.Core/game.json", "g");
        write(root, "Assets/p/old.Core/deep/inst.json", "i");

        let actions = vec![
            install("Cores/a.B/core.json", "Cores/a.B/core.json", true),
            install("Cores/a.B/a.rev", "Cores/a.B/a.rev", false),
            Action::Remove {
                path: "Cores/a.B/sub/a.rev".into(),
                reason: RemoveReason::MovedFile {
                    replaced_by: "Cores/a.B/a.rev".into(),
                },
            },
            Action::MoveTree {
                from: "Assets/p/old.Core".into(),
                to: "Assets/p/a.B".into(),
                reason: MoveReason::ReplacedCore {
                    from: "old.Core".into(),
                    to: "a.B".into(),
                },
            },
            Action::Skip {
                zip_path: "README.md".into(),
                reason: crate::card::install::plan::SkipReason::Junk,
            },
        ];
        let mut ex = extractor(HashMap::from([
            ("Cores/a.B/core.json", "new"),
            ("Cores/a.B/a.rev", "bits"),
        ]));
        let mut events = Vec::new();
        let applied = apply(&pocket, "a.B", &actions, &mut ex, &mut |e| events.push(e)).unwrap();

        assert_eq!(applied.installed, 2);
        assert_eq!(applied.removed, 1);
        assert_eq!(applied.moved_files, 2);
        assert_eq!(read(root, "Cores/a.B/core.json").as_deref(), Some("new"));
        assert_eq!(read(root, "Cores/a.B/a.rev").as_deref(), Some("bits"));
        assert!(!root.join("Cores/a.B/sub/a.rev").exists());
        assert!(
            !root.join("Assets/p/old.Core").exists(),
            "emptied source dir removed"
        );
        assert_eq!(read(root, "Assets/p/a.B/game.json").as_deref(), Some("g"));
        assert_eq!(
            read(root, "Assets/p/a.B/deep/inst.json").as_deref(),
            Some("i")
        );

        assert!(
            !root.join(".pocketeer/staging").exists(),
            "staging and undo folders cleaned up"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Status(s) if s.starts_with("Applying 4")))
        );
    }

    #[test]
    fn failure_rolls_back_everything() {
        let (tmp, pocket) = sd();
        let root = tmp.path();
        write(root, "Cores/a.B/core.json", "old");
        write(root, "Cores/a.B/gone.bin", "keepme");

        // Second install's staging succeeds but the rename will fail because
        // we make its destination parent a *file*.
        write(root, "Cores/a.B/blocker", "file-not-dir");
        let actions = vec![
            install("Cores/a.B/core.json", "Cores/a.B/core.json", true),
            Action::Remove {
                path: "Cores/a.B/gone.bin".into(),
                reason: RemoveReason::Uninstall,
            },
            install("x", "Cores/a.B/blocker/inside.bin", false),
        ];
        let mut ex = extractor(HashMap::from([("Cores/a.B/core.json", "new"), ("x", "y")]));
        let err = apply(&pocket, "a.B", &actions, &mut ex, &mut |_| {}).unwrap_err();
        match &err {
            Error::ApplyFailed {
                rolled_back, step, ..
            } => {
                assert!(*rolled_back, "{err}");
                assert_eq!(*step, 3);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(read(root, "Cores/a.B/core.json").as_deref(), Some("old"));
        assert_eq!(read(root, "Cores/a.B/gone.bin").as_deref(), Some("keepme"));
        assert!(
            !root.join(".pocketeer/staging").exists()
                || fs::read_dir(root.join(".pocketeer/staging"))
                    .unwrap()
                    .next()
                    .is_none(),
            "undo folder removed after a successful rollback"
        );
    }

    #[test]
    fn bad_zip_entry_fails_before_touching_the_card() {
        let (tmp, pocket) = sd();
        let root = tmp.path();
        write(root, "Cores/a.B/core.json", "old");
        let actions = vec![install("missing", "Cores/a.B/core.json", true)];
        let mut ex = extractor(HashMap::new());
        let err = apply(&pocket, "a.B", &actions, &mut ex, &mut |_| {}).unwrap_err();
        assert_eq!(err.code(), "UNSAFE_ZIP_PATH");
        assert_eq!(read(root, "Cores/a.B/core.json").as_deref(), Some("old"));
        assert!(!root.join(".pocketeer").exists() || !root.join(".pocketeer/staging").exists());
    }
}
