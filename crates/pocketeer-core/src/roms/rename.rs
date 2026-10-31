//! Renaming a ROM file and everything the Pocket keys to its name.
//!
//! The Pocket stores a game's save as `Saves/<platform>/<owner>/<stem>.sav`
//! and its save states under `Memories/Save States/<platform>/…/<stem>*`,
//! where `<stem>` is the ROM file name without extension. Renaming the ROM
//! alone would orphan them, so every rename here carries its companions.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{Error, Result, fsutil};

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RenameKind {
    Rom,
    Save,
    SaveState,
}

/// One file move, both paths relative to the same root (SD card or library).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FileRename {
    pub from: PathBuf,
    pub to: PathBuf,
    pub kind: RenameKind,
}

/// Folders (relative to a root) whose files are named after the ROM.
const COMPANION_ROOTS: [(&str, RenameKind); 2] = [
    ("Saves", RenameKind::Save),
    ("Memories/Save States", RenameKind::SaveState),
];

/// File name without its extension, when it has a short one
/// (`Foo (USA).gba` → `Foo (USA)`; `Foo.v1.2 Final` stays as is).
pub fn stem(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() && ext.len() <= 4 => stem,
        _ => name,
    }
}

/// Extension (with the dot) by the same rule as [`stem`].
pub fn extension(name: &str) -> &str {
    &name[stem(name).len()..]
}

/// Something that can't be a file name on a FAT card.
pub fn validate_name(name: &str) -> Result<()> {
    let bad = |reason: &str| Error::InvalidName {
        name: name.to_string(),
        reason: reason.to_string(),
    };
    if name.trim().is_empty() {
        return Err(bad("empty"));
    }
    if name != name.trim() {
        return Err(bad("leading or trailing whitespace"));
    }
    if fsutil::is_hidden(std::ffi::OsStr::new(name)) {
        return Err(bad("hidden file (starts with '.')"));
    }
    if name.contains(['/', '\\', '\0', ':', '*', '?', '"', '<', '>', '|']) {
        return Err(bad(
            "contains a character FAT32 forbids (/ \\ : * ? \" < > |)",
        ));
    }
    if name.ends_with('.') {
        return Err(bad("ends with '.'"));
    }
    Ok(())
}

/// Saves and save states under `root` that belong to `old_name` on
/// `platform`, each mapped to its new name. Matches `<stem>.<anything>` so
/// `Foo.sav`, `Foo.1.sta` move but `Foo Bar.sav` doesn't.
pub fn companion_renames(
    root: &Path,
    platform: &str,
    old_name: &str,
    new_name: &str,
) -> Vec<FileRename> {
    let old_stem = stem(old_name);
    let new_stem = stem(new_name);
    let mut out = Vec::new();
    if old_stem == new_stem {
        return out;
    }
    for (base, kind) in COMPANION_ROOTS {
        let dir = root.join(base).join(platform);
        if !dir.is_dir() {
            continue;
        }
        for path in fsutil::visible_files(&dir) {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let Some(rest) = name.strip_prefix(old_stem) else {
                continue;
            };
            if !rest.starts_with('.') {
                continue;
            }
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            let to = rel.with_file_name(format!("{new_stem}{rest}"));
            out.push(FileRename {
                from: rel.to_path_buf(),
                to,
                kind,
            });
        }
    }
    out.sort_by(|a, b| a.from.cmp(&b.from));
    out
}

/// Renames in order, all or nothing: the first failure moves every rename
/// already done back, then is returned. A target that already exists is a
/// failure, never an overwrite. Returns how many files were renamed.
pub fn apply_renames(root: &Path, renames: &[FileRename]) -> Result<usize> {
    let mut done = 0;
    let result = renames.iter().try_for_each(|r| {
        rename_one(root, r)?;
        done += 1;
        Ok(())
    });
    if let Err(e) = result {
        for r in renames.iter().take(done).rev() {
            let _ = std::fs::rename(root.join(&r.to), root.join(&r.from));
        }
        return Err(e);
    }
    Ok(done)
}

fn rename_one(root: &Path, r: &FileRename) -> Result<()> {
    let from = root.join(&r.from);
    let to = root.join(&r.to);
    if to.exists() {
        return Err(Error::AlreadyExists { path: to });
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }
    std::fs::rename(&from, &to).map_err(|e| Error::io(&from, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn stems_and_extensions() {
        assert_eq!(stem("Foo (USA).gba"), "Foo (USA)");
        assert_eq!(extension("Foo (USA).gba"), ".gba");
        assert_eq!(stem("Foo.v1.2 Final"), "Foo.v1.2 Final");
        assert_eq!(extension("Foo.v1.2 Final"), "");
        assert_eq!(stem(".hidden"), ".hidden");
        assert_eq!(stem("noext"), "noext");
        assert_eq!(stem("a.b"), "a");
    }

    #[test]
    fn validates_names() {
        assert!(validate_name("Foo (USA).gba").is_ok());
        for bad in ["", "  ", " x.gba", ".x.gba", "a/b.gba", "a:b", "x."] {
            assert_eq!(
                validate_name(bad).unwrap_err().code(),
                "INVALID_NAME",
                "{bad:?}"
            );
        }
    }

    #[test]
    fn companions_follow_the_stem_only() {
        let tmp = tempfile::tempdir().unwrap();
        let w = |rel: &str| {
            let p = tmp.path().join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, "x").unwrap();
        };
        w("Saves/gba/common/Foo.sav");
        w("Saves/gba/common/Foo Bar.sav");
        w("Saves/gba/x.Core/Foo.sav");
        w("Saves/gb/common/Foo.sav");
        w("Memories/Save States/gba/common/Foo.1.sta");
        w("Memories/Save States/gba/common/.Foo.1.sta");

        let r = companion_renames(tmp.path(), "gba", "Foo.gba", "Bar (USA).gba");
        let moves: Vec<(String, String)> = r
            .iter()
            .map(|m| (m.from.display().to_string(), m.to.display().to_string()))
            .collect();
        assert_eq!(
            moves,
            [
                (
                    "Memories/Save States/gba/common/Foo.1.sta".to_string(),
                    "Memories/Save States/gba/common/Bar (USA).1.sta".to_string()
                ),
                (
                    "Saves/gba/common/Foo.sav".to_string(),
                    "Saves/gba/common/Bar (USA).sav".to_string()
                ),
                (
                    "Saves/gba/x.Core/Foo.sav".to_string(),
                    "Saves/gba/x.Core/Bar (USA).sav".to_string()
                ),
            ]
        );
        assert_eq!(r[0].kind, RenameKind::SaveState);
        assert!(companion_renames(tmp.path(), "gba", "Foo.gba", "Foo.bin").is_empty());

        assert_eq!(apply_renames(tmp.path(), &r).unwrap(), 3);
        assert!(tmp.path().join("Saves/gba/common/Bar (USA).sav").is_file());
        assert!(tmp.path().join("Saves/gba/common/Foo Bar.sav").is_file());

        // Refuses to overwrite, and puts back what it had already moved.
        w("Saves/gba/common/Foo.sav");
        w("Saves/gb/common/Foo.sav");
        w("Memories/Save States/gba/common/Foo.1.sta");
        let again = companion_renames(tmp.path(), "gba", "Foo.gba", "Bar (USA).gba");
        assert_eq!(again.len(), 2);
        let err = apply_renames(tmp.path(), &again).unwrap_err();
        assert_eq!(err.code(), "ALREADY_EXISTS");
        assert!(
            tmp.path()
                .join("Memories/Save States/gba/common/Foo.1.sta")
                .is_file(),
            "first rename rolled back"
        );
    }
}
