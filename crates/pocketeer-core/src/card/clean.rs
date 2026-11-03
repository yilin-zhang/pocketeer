//! macOS leaves `._*` AppleDouble files and `.DS_Store` all over removable
//! media; the Pocket's file browser doesn't appreciate it.

use std::path::PathBuf;

use serde::Serialize;

use crate::{
    Error, Pocket, Result,
    card::POCKETEER_DIR,
    fsutil,
    plan::{Applied, Checked, Failure, Pinned, Plan, Progress, Resources},
};

/// Volume-level folders macOS creates on every card. Harmless to delete
/// (macOS recreates them), but only removed on request.
const SYSTEM_DIRS: [&str; 4] = [
    ".Spotlight-V100",
    ".fseventsd",
    ".Trashes",
    ".TemporaryItems",
];

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JunkKind {
    /// `._name` companion file (resource fork / extended attributes).
    AppleDouble,
    DsStore,
    /// `__MACOSX/` folder from an unzipped archive.
    MacosxDir,
    /// One of [`SYSTEM_DIRS`] at the card root.
    SystemDir,
    /// `.pocketeer/backup` from pocketeer versions that kept backups on the card.
    OldBackups,
}

impl JunkKind {
    /// AppleDouble files and `.DS_Store` are files; the rest are folders.
    pub fn is_dir(self) -> bool {
        !matches!(self, JunkKind::AppleDouble | JunkKind::DsStore)
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct JunkEntry {
    pub path: PathBuf,
    pub kind: JunkKind,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CleanPlan {
    pub entries: Vec<JunkEntry>,
    pub bytes: u64,
    pub include_system: bool,
    /// System folders present but left alone (when `include_system` is off).
    pub system_dirs_present: Vec<PathBuf>,
    /// Folders the scan couldn't read (permissions, vanished mid-walk); junk
    /// under them is unknown. Relative to the SD root.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<PathBuf>,
    #[serde(skip)]
    pinned: Pinned,
}

impl Plan for CleanPlan {
    type Output = CleanResult;

    fn changes(&self) -> usize {
        self.entries.len()
    }

    fn pinned(&self) -> &Pinned {
        &self.pinned
    }

    /// Deletes everything in the plan. Junk isn't backed up. Per-entry
    /// failures are reported in the result, not returned.
    fn execute(&mut self, res: &mut Resources, _: Progress<'_>, _: Checked) -> Result<CleanResult> {
        let root = res.pocket()?.root();
        let mut result = CleanResult::default();
        for entry in &self.entries {
            let full = root.join(&entry.path);
            let outcome = if entry.kind.is_dir() {
                std::fs::remove_dir_all(&full)
            } else {
                std::fs::remove_file(&full)
            };
            match outcome {
                Ok(()) => {
                    result.removed += 1;
                    result.bytes += entry.bytes;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => result.failed.push(Failure {
                    path: entry.path.clone(),
                    error: Error::io(full, e).to_info(),
                }),
            }
        }
        Ok(result)
    }
}

pub fn plan(pocket: &Pocket, include_system: bool) -> Result<CleanPlan> {
    let root = pocket.root();
    let mut plan = CleanPlan {
        entries: Vec::new(),
        bytes: 0,
        include_system,
        system_dirs_present: Vec::new(),
        skipped: Vec::new(),
        pinned: Pinned::card(pocket)?,
    };

    let old_backups = pocket.pocketeer_dir().join("backup");
    if old_backups.is_dir() {
        let bytes = fsutil::dir_size(&old_backups).1;
        plan.bytes += bytes;
        plan.entries.push(JunkEntry {
            path: PathBuf::from(POCKETEER_DIR).join("backup"),
            kind: JunkKind::OldBackups,
            bytes,
        });
    }

    let mut walker = walkdir::WalkDir::new(root).min_depth(1).into_iter();
    while let Some(entry) = walker.next() {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                let path = e.path().unwrap_or(root);
                plan.skipped
                    .push(path.strip_prefix(root).unwrap_or(path).to_path_buf());
                continue;
            }
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        let depth = entry.depth();
        let is_dir = entry.file_type().is_dir();
        let rel = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .to_path_buf();

        if depth == 1 && name == POCKETEER_DIR {
            walker.skip_current_dir();
            continue;
        }
        let kind = if is_dir && depth == 1 && SYSTEM_DIRS.contains(&name.as_str()) {
            if !include_system {
                plan.system_dirs_present.push(rel);
                walker.skip_current_dir();
                continue;
            }
            Some(JunkKind::SystemDir)
        } else if is_dir && name == "__MACOSX" {
            Some(JunkKind::MacosxDir)
        } else if !is_dir && name == ".DS_Store" {
            Some(JunkKind::DsStore)
        } else if !is_dir && name.starts_with("._") {
            Some(JunkKind::AppleDouble)
        } else {
            None
        };
        if let Some(kind) = kind {
            let bytes = if is_dir {
                fsutil::dir_size(entry.path()).1
            } else {
                entry.metadata().map(|m| m.len()).unwrap_or(0)
            };
            plan.bytes += bytes;
            plan.entries.push(JunkEntry {
                path: rel,
                kind,
                bytes,
            });
            if is_dir {
                walker.skip_current_dir();
            }
        }
    }
    plan.entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(plan)
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct CleanResult {
    pub removed: usize,
    pub bytes: u64,
    pub failed: Vec<Failure>,
}

impl Applied for CleanResult {
    fn failures(&self) -> &[Failure] {
        &self.failed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::write;

    #[test]
    fn a_swapped_card_is_refused() {
        let (_tmp, pocket) = junk_card();
        let mut plan = plan(&pocket, false).unwrap();
        let aside = tempfile::tempdir().unwrap();
        crate::testutil::swap_card(&pocket, aside.path());
        assert_eq!(
            crate::testutil::apply(&mut plan, Some(&pocket), None)
                .unwrap_err()
                .code(),
            "STALE_PLAN"
        );
        assert!(pocket.root().join("Cores/.DS_Store").exists());
    }

    /// A card littered with the junk macOS leaves behind.
    fn junk_card() -> (tempfile::TempDir, Pocket) {
        let (tmp, pocket) = crate::testutil::sd();
        for (rel, body) in [
            ("Cores/a.B/core.json", "{}"),
            ("Cores/a.B/._core.json", "xx"),
            ("Cores/.DS_Store", "x"),
            ("Assets/gba/__MACOSX/._thing", "xxx"),
            ("Assets/gba/__MACOSX/inner/._deep", "x"),
            (".Spotlight-V100/Store/db", "xxxxx"),
            (".pocketeer/staging/x/._ignored", "x"),
        ] {
            write(tmp.path(), rel, body);
        }
        (tmp, pocket)
    }

    #[test]
    fn finds_junk_and_leaves_system_dirs_unless_asked() {
        let (tmp, pocket) = junk_card();
        let p = plan(&pocket, false).unwrap();
        let paths: Vec<String> = p
            .entries
            .iter()
            .map(|e| e.path.display().to_string())
            .collect();
        assert_eq!(
            paths,
            [
                "Assets/gba/__MACOSX",
                "Cores/.DS_Store",
                "Cores/a.B/._core.json"
            ]
        );
        assert_eq!(p.bytes, 4 + 1 + 2);
        assert_eq!(p.system_dirs_present, [PathBuf::from(".Spotlight-V100")]);

        let mut p = plan(&pocket, true).unwrap();
        assert!(p.entries.iter().any(|e| e.kind == JunkKind::SystemDir));
        assert_eq!(p.bytes, 4 + 1 + 2 + 5);

        let r = crate::testutil::apply(&mut p, Some(&pocket), None).unwrap();
        assert_eq!(r.removed, 4);
        assert!(r.failed.is_empty());
        assert!(!tmp.path().join("Cores/.DS_Store").exists());
        assert!(!tmp.path().join("Assets/gba/__MACOSX").exists());
        assert!(!tmp.path().join(".Spotlight-V100").exists());
        assert!(tmp.path().join("Cores/a.B/core.json").exists());
        assert!(tmp.path().join(".pocketeer/staging/x/._ignored").exists());
        assert!(plan(&pocket, true).unwrap().entries.is_empty());
    }
}
