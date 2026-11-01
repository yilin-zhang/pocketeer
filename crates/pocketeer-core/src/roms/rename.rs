//! File rename primitives and explicit card rename previews.
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
            rolled_back &=
                fsutil::rename_no_replace(&root.join(&r.to), &root.join(&r.from)).is_ok();
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
    fsutil::rename_no_replace(&from, &to)
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

#[derive(Debug, Clone, Serialize)]
pub struct CardRenamePlan {
    pub moves: Vec<FileRename>,
    pub warnings: Vec<String>,
    #[serde(skip)]
    identity: crate::sd::CardIdentity,
    #[serde(skip)]
    modified: std::time::SystemTime,
    pub size: u64,
}
impl CardRenamePlan {
    pub fn changes(&self) -> usize {
        self.moves.len()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CardRenameResult {
    pub renamed: usize,
    pub warnings: Vec<String>,
}

pub fn complete_name(old_name: &str, new_name: &str) -> String {
    if extension(new_name).is_empty() {
        format!("{new_name}{}", extension(old_name))
    } else {
        new_name.to_string()
    }
}

pub fn plan_card(pocket: &crate::Pocket, path: &Path, new_name: &str) -> Result<CardRenamePlan> {
    let index = super::index::Index::scan(pocket)?;
    let rom = index
        .roms
        .iter()
        .find(|r| r.rel_path == path)
        .ok_or_else(|| Error::RomNotFound {
            spec: path.display().to_string(),
        })?;
    let new_name = complete_name(&rom.name, new_name);
    validate_name(&new_name)?;
    let destination = path.with_file_name(&new_name);
    if pocket.root().join(&destination).exists() {
        return Err(Error::AlreadyExists { path: destination });
    }
    let mut moves = vec![FileRename {
        from: path.to_path_buf(),
        to: destination,
        kind: RenameKind::Rom,
    }];
    let save_dir = if let Ok(relative) = path.strip_prefix("Assets") {
        PathBuf::from("Saves").join(relative.parent().expect("asset parent"))
    } else if path.parent() == Some(Path::new("GB Studio")) {
        PathBuf::from("GB Studio/Saves")
    } else {
        return Err(Error::Usage {
            message: "Unsupported ROM location".into(),
            hint: None,
        });
    };
    let old_stem = stem(&rom.name);
    let new_stem = stem(&new_name);
    if old_stem != new_stem {
        if index.roms.iter().any(|r| {
            r.rel_path != path
                && r.rel_path.parent() == path.parent()
                && [old_stem, new_stem]
                    .iter()
                    .any(|s| s.eq_ignore_ascii_case(stem(&r.name)))
        }) {
            return Err(Error::Usage { message: "Another ROM in this folder shares the save filename; the save owner is ambiguous".into(), hint: None });
        }
        let dir = pocket.root().join(&save_dir);
        match std::fs::read_dir(&dir) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry.map_err(|e| Error::io(&dir, e))?;
                    if !entry
                        .file_type()
                        .map_err(|e| Error::io(entry.path(), e))?
                        .is_file()
                    {
                        continue;
                    }
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if stem(&name).eq_ignore_ascii_case(old_stem) && !extension(&name).is_empty() {
                        let suffix = extension(&name);
                        let to = save_dir.join(format!("{new_stem}{suffix}"));
                        if pocket.root().join(&to).exists() {
                            return Err(Error::AlreadyExists { path: to });
                        }
                        moves.push(FileRename {
                            from: save_dir.join(name),
                            to,
                            kind: RenameKind::Save,
                        });
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::io(&dir, e)),
        }
    }
    moves[1..].sort_by(|a, b| a.from.cmp(&b.from));
    let warnings = vec![
        "Memories stay unchanged; loading an existing save state may require the old ROM name."
            .into(),
    ];
    let full = pocket.root().join(path);
    let modified = std::fs::metadata(&full)
        .and_then(|m| m.modified())
        .map_err(|e| Error::io(&full, e))?;
    Ok(CardRenamePlan {
        moves,
        warnings,
        identity: crate::sd::CardIdentity::read(pocket)?,
        modified,
        size: rom.size,
    })
}

pub fn apply_card(pocket: &crate::Pocket, plan: &CardRenamePlan) -> Result<CardRenameResult> {
    if plan.identity != crate::sd::CardIdentity::read(pocket)? {
        return Err(Error::Usage {
            message: "Card changed since rename preview".into(),
            hint: None,
        });
    }
    let rom = &plan.moves[0];
    let source = pocket.root().join(&rom.from);
    let meta = std::fs::metadata(&source).map_err(|e| Error::io(&source, e))?;
    if meta.len() != plan.size
        || meta.modified().map_err(|e| Error::io(&source, e))? != plan.modified
    {
        return Err(Error::Usage {
            message: "ROM changed since rename preview".into(),
            hint: None,
        });
    }
    let mut index = super::index::Index::scan(pocket)?;
    let renamed = apply_renames(pocket.root(), &plan.moves)?;
    let mut warnings = plan.warnings.clone();
    if let Err(e) = index
        .relocate(pocket, &rom.from, &rom.to)
        .and_then(|_| index.save(pocket))
    {
        warnings.push(format!("Files renamed, but cache update failed: {e}"));
    }
    Ok(CardRenameResult { renamed, warnings })
}

#[cfg(test)]
mod card_tests {
    use super::*;
    use crate::{
        roms::index::{Confidence, Index},
        testutil::{sd, write},
    };

    #[test]
    fn rename_moves_only_the_selected_copy_and_its_mirrored_saves() {
        let (tmp, pocket) = sd();
        for folder in ["one", "two"] {
            write(
                tmp.path(),
                &format!("Assets/gba/common/{folder}/game.gba"),
                "ROM",
            );
            write(
                tmp.path(),
                &format!("Saves/gba/common/{folder}/game.sav"),
                "save",
            );
        }
        write(tmp.path(), "Saves/gba/common/one/game.rtc", "clock");
        write(tmp.path(), "Memories/Save States/game.state", "state");
        write(tmp.path(), "Assets/gba/common/one/game.v2.gba", "other ROM");
        write(tmp.path(), "Saves/gba/common/one/game.v2.sav", "other save");
        let mut index = Index::scan(&pocket).unwrap();
        for i in 0..index.roms.len() {
            index.verify(&pocket, i, false, &|| false);
        }
        let sha1 = index.roms[0].sha1.clone();
        index.save(&pocket).unwrap();
        let preview = plan_card(
            &pocket,
            Path::new("Assets/gba/common/one/game.gba"),
            "new name",
        )
        .unwrap();
        assert_eq!(preview.moves.len(), 3);
        let result = apply_card(&pocket, &preview).unwrap();
        assert_eq!(result.renamed, 3);
        assert_eq!(
            std::fs::read(tmp.path().join("Memories/Save States/game.state")).unwrap(),
            b"state"
        );
        assert_eq!(result.warnings, preview.warnings);
        assert_eq!(
            std::fs::read(tmp.path().join("Saves/gba/common/one/new name.sav")).unwrap(),
            b"save"
        );
        assert_eq!(
            std::fs::read(tmp.path().join("Saves/gba/common/two/game.sav")).unwrap(),
            b"save"
        );
        assert!(tmp.path().join("Assets/gba/common/two/game.gba").exists());
        assert_eq!(
            std::fs::read(tmp.path().join("Saves/gba/common/one/game.v2.sav")).unwrap(),
            b"other save"
        );
        assert!(
            plan_card(
                &pocket,
                Path::new("Assets/gba/common/one/game.v2.gba"),
                "new name.gb"
            )
            .is_err()
        );
        let index = Index::scan(&pocket).unwrap();
        let renamed = index
            .roms
            .iter()
            .find(|r| r.name == "new name.gba")
            .unwrap();
        assert_eq!(renamed.confidence, Confidence::Cached);
        assert_eq!(renamed.sha1, sha1);
    }

    #[test]
    fn rename_rejects_a_target_save_name_owned_by_another_rom() {
        let (tmp, pocket) = sd();
        write(tmp.path(), "Assets/gb/common/game.gb", "source");
        write(tmp.path(), "Assets/gb/common/OTHER.gbc", "other");
        assert!(plan_card(&pocket, Path::new("Assets/gb/common/game.gb"), "other.gb").is_err());
        assert_eq!(
            std::fs::read(tmp.path().join("Assets/gb/common/game.gb")).unwrap(),
            b"source"
        );
    }

    #[test]
    fn gbstudio_scans_only_supported_root_files_and_renames_its_save() {
        let (tmp, pocket) = sd();
        write(tmp.path(), "GB Studio/demo.pocket", "ROM");
        write(
            tmp.path(),
            "GB Studio/folder/ignored.pocket",
            "not browsable",
        );
        write(tmp.path(), "GB Studio/Saves/demo.sav", "save");
        let index = Index::scan(&pocket).unwrap();
        assert_eq!(index.roms.len(), 1);
        assert_eq!(index.roms[0].platform, "gbstudio");
        let preview = plan_card(&pocket, Path::new("GB Studio/demo.pocket"), "new").unwrap();
        apply_card(&pocket, &preview).unwrap();
        let scanned = Index::scan(&pocket).unwrap();
        assert_eq!(scanned.roms.len(), 1);
        assert_eq!(scanned.roms[0].name, "new.pocket");
        assert_eq!(
            std::fs::read(tmp.path().join("GB Studio/Saves/new.sav")).unwrap(),
            b"save"
        );
    }
}
