//! Renaming a ROM on the card: its ordinary saves follow its name, the
//! card's ROM index follows, and so do the library's backups of those saves
//! (or the next backup would see them as new).

use crate::card::roms::Index;
use crate::fsutil::Stamp;
use crate::fsutil::names::{
    FileRename, apply_renames, complete_name, extension, stem, validate_name,
};
use crate::plan::{Applied, Checked, Pinned, Plan, Progress, Resources};
use crate::transfer::saves;
use crate::{Error, Result};
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Serialize)]
pub struct CardRenamePlan {
    pub rom: FileRename,
    /// The ROM's ordinary saves, which follow its name.
    pub saves: Vec<FileRename>,
    pub warnings: Vec<String>,
    #[serde(skip)]
    pinned: Pinned,
    /// The ROM as it was when planned.
    #[serde(skip)]
    stamp: Stamp,
}
impl Plan for CardRenamePlan {
    type Output = CardRenameResult;

    fn changes(&self) -> usize {
        1 + self.saves.len()
    }

    fn pinned(&self) -> &Pinned {
        &self.pinned
    }

    fn execute(
        &mut self,
        res: &mut Resources,
        _: Progress<'_>,
        _: Checked,
    ) -> Result<CardRenameResult> {
        let pocket = res.pocket()?.clone();
        let rom = &self.rom;
        let source = pocket.root().join(&rom.from);
        if Stamp::of(&source)? != self.stamp {
            return Err(Error::Stale {
                what: rom.from.display().to_string(),
            });
        }
        let mut index = Index::scan(&pocket)?;
        let renamed = apply_renames(pocket.root(), std::iter::once(rom).chain(&self.saves))?;
        let mut warnings = self.warnings.clone();
        if let Err(e) = index
            .relocate(&pocket, &rom.from, &rom.to)
            .and_then(|_| index.save(&pocket))
        {
            warnings.push(format!("Files renamed, but cache update failed: {e}"));
        }
        // Without a library there are no backups to follow.
        if !self.saves.is_empty()
            && let Ok(lib) = res.library()
        {
            let moves: Vec<_> = self
                .saves
                .iter()
                .map(|m| (m.from.clone(), m.to.clone()))
                .collect();
            saves::follow_renames(lib, &moves)?;
        }
        Ok(CardRenameResult { renamed, warnings })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CardRenameResult {
    pub renamed: usize,
    pub warnings: Vec<String>,
}

impl Applied for CardRenameResult {
    fn failed(&self) -> usize {
        0
    }
}

pub fn plan(pocket: &crate::Pocket, path: &Path, new_name: &str) -> Result<CardRenamePlan> {
    let index = Index::scan(pocket)?;
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
    let rom_move = FileRename {
        from: path.to_path_buf(),
        to: destination,
    };
    let mut saves = Vec::new();
    let save_dir = crate::card::save_dir_for(path).ok_or_else(|| Error::InvalidPath {
        path: path.to_path_buf(),
        reason: "not inside a ROM folder pocketeer knows the saves of".into(),
    })?;
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
            return Err(Error::SaveOwnerAmbiguous {
                path: path.to_path_buf(),
            });
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
                        saves.push(FileRename {
                            from: save_dir.join(name),
                            to,
                        });
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::io(&dir, e)),
        }
    }
    saves.sort_by(|a, b| a.from.cmp(&b.from));
    let warnings = vec![
        "Memories stay unchanged; loading an existing save state may require the old ROM name."
            .into(),
    ];
    Ok(CardRenamePlan {
        rom: rom_move,
        saves,
        warnings,
        pinned: Pinned::card(pocket)?,
        stamp: Stamp::of(&pocket.root().join(path))?,
    })
}

#[cfg(test)]
mod card_tests {
    use super::*;
    use crate::{
        card::roms::{Identity, Index},
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
        let sha1 = index.roms[0].identity.sha1().map(str::to_owned);
        index.save(&pocket).unwrap();
        let mut preview = plan(
            &pocket,
            Path::new("Assets/gba/common/one/game.gba"),
            "new name",
        )
        .unwrap();
        assert_eq!(preview.saves.len(), 2);
        let result = crate::testutil::apply(&mut preview, Some(&pocket), None).unwrap();
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
        // "new name.gba" now sits beside it: its saves would be shared.
        assert_eq!(
            plan(
                &pocket,
                Path::new("Assets/gba/common/one/game.v2.gba"),
                "new name.gb"
            )
            .unwrap_err()
            .code(),
            "SAVE_OWNER_AMBIGUOUS"
        );
        let index = Index::scan(&pocket).unwrap();
        let renamed = index
            .roms
            .iter()
            .find(|r| r.name == "new name.gba")
            .unwrap();
        assert!(matches!(renamed.identity, Identity::Cached { .. }));
        assert_eq!(renamed.identity.sha1(), sha1.as_deref());
    }

    #[test]
    fn rename_rejects_a_target_save_name_owned_by_another_rom() {
        let (tmp, pocket) = sd();
        write(tmp.path(), "Assets/gb/common/game.gb", "source");
        write(tmp.path(), "Assets/gb/common/OTHER.gbc", "other");
        assert_eq!(
            plan(&pocket, Path::new("Assets/gb/common/game.gb"), "other.gb")
                .unwrap_err()
                .code(),
            "SAVE_OWNER_AMBIGUOUS"
        );
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
        let mut preview = plan(&pocket, Path::new("GB Studio/demo.pocket"), "new").unwrap();
        crate::testutil::apply(&mut preview, Some(&pocket), None).unwrap();
        let scanned = Index::scan(&pocket).unwrap();
        assert_eq!(scanned.roms.len(), 1);
        assert_eq!(scanned.roms[0].name, "new.pocket");
        assert_eq!(
            std::fs::read(tmp.path().join("GB Studio/Saves/new.sav")).unwrap(),
            b"save"
        );
    }
}
