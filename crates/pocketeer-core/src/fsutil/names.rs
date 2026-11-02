//! Portable ROM filename validation and rollback-safe file moves.

use std::path::{Path, PathBuf};

use serde::Serialize;

use super::{is_hidden, rename_no_replace};
use crate::{Error, Result};

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

/// File name without its extension, when it has a short one
/// (`Foo (USA).gba` → `Foo (USA)`; `Foo.v1.2 Final` stays as is).
pub fn stem(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((stem, ext))
            if !stem.is_empty()
                && !ext.is_empty()
                && (ext.len() <= 4 || ext.eq_ignore_ascii_case("pocket")) =>
        {
            stem
        }
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
    if is_hidden(std::ffi::OsStr::new(name)) {
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
        let mut rolled_back = true;
        for r in renames.iter().take(done).rev() {
            rolled_back &= rename_no_replace(&root.join(&r.to), &root.join(&r.from)).is_ok();
        }
        return Err(Error::ApplyFailed {
            step: done,
            rolled_back,
            source: Box::new(e),
        });
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
    rename_no_replace(&from, &to)
}

pub fn complete_name(old_name: &str, new_name: &str) -> String {
    if extension(new_name).is_empty() {
        format!("{new_name}{}", extension(old_name))
    } else {
        new_name.to_string()
    }
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
    fn collision_rolls_back_previous_moves_and_preserves_destinations() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a"), "first").unwrap();
        fs::write(tmp.path().join("b"), "second").unwrap();
        fs::write(tmp.path().join("occupied"), "keep").unwrap();
        let moves = [
            FileRename {
                from: "a".into(),
                to: "new".into(),
                kind: RenameKind::Rom,
            },
            FileRename {
                from: "b".into(),
                to: "occupied".into(),
                kind: RenameKind::Save,
            },
        ];
        assert!(apply_renames(tmp.path(), &moves).is_err());
        assert_eq!(fs::read(tmp.path().join("a")).unwrap(), b"first");
        assert_eq!(fs::read(tmp.path().join("b")).unwrap(), b"second");
        assert_eq!(fs::read(tmp.path().join("occupied")).unwrap(), b"keep");
        assert!(!tmp.path().join("new").exists());
    }
}
