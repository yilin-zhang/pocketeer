//! Explicit local ROM rename. Card paths and save backups have independent identities.

use serde::Serialize;

use crate::{
    Error, Result,
    fsutil::names::{FileRename, RenameKind, apply_renames, complete_name, stem, validate_name},
    plan::{Applied, Plan},
};

use super::{Library, db, resolve_rom};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RenamePlan {
    pub rom_id: i64,
    pub game_id: i64,
    pub platform: String,
    pub old_name: String,
    pub new_name: String,
    pub old_game_name: String,
    /// Follows the file name when the game has one ROM; multi-ROM games keep theirs.
    pub new_game_name: String,
    /// Managed file move, relative to the library root (`None` for references).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub library_file: Option<FileRename>,
}

impl Plan for RenamePlan {
    /// File moves plus the index update.
    fn changes(&self) -> usize {
        1 + self.library_file.is_some() as usize
    }
}

pub fn plan(
    lib: &Library,
    platform: Option<&str>,
    spec: &str,
    new_name: &str,
) -> Result<RenamePlan> {
    let (game, rom) = resolve_rom(lib, platform, spec)?;
    let new_name = complete_name(&rom.name, new_name);
    validate_name(&new_name)?;
    if new_name == rom.name {
        return Err(Error::NameTaken {
            name: new_name,
            platform: game.platform.clone(),
            what: "that is already its name".into(),
        });
    }
    let taken = |what: &str| Error::NameTaken {
        name: new_name.clone(),
        platform: game.platform.clone(),
        what: what.to_string(),
    };
    if let Some(other) = db::find_rom_by_name(lib.conn(), &game.platform, &new_name)?
        && other.id != rom.id
    {
        return Err(taken(&format!("ROM of {}", other.name)));
    }

    let single = game.roms.len() == 1;
    let new_game_name = if single {
        stem(&new_name).to_string()
    } else {
        game.name.clone()
    };
    if new_game_name != game.name
        && let Some(other) = db::find_game_by_name(lib.conn(), &game.platform, &new_game_name)?
        && other != game.id
    {
        return Err(taken("another game"));
    }

    let library_file = match &rom.rel_path {
        Some(rel) => {
            let to = Library::rom_rel_path(&game.platform, &new_name);
            if lib.root().join(&to).exists() {
                return Err(taken("a file in the library"));
            }
            Some(FileRename {
                from: rel.clone(),
                to,
                kind: RenameKind::Rom,
            })
        }
        None => None,
    };
    Ok(RenamePlan {
        rom_id: rom.id,
        game_id: game.id,
        platform: game.platform,
        old_name: rom.name,
        new_name,
        old_game_name: game.name,
        new_game_name,
        library_file,
    })
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct RenameResult {
    pub library_renamed: usize,
}

impl Applied for RenameResult {
    fn failed(&self) -> usize {
        0
    }
}

pub fn apply(lib: &mut Library, plan: &RenamePlan) -> Result<RenameResult> {
    let root = lib.root().to_path_buf();
    let tx = lib
        .conn_mut()
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| Error::db(e, "begin rename"))?;
    let current = db::rom_name(&tx, plan.rom_id)?;
    if current != plan.old_name {
        return Err(Error::Stale {
            what: format!("ROM {}", plan.old_name),
        });
    }
    let moves: Vec<_> = plan.library_file.iter().cloned().collect();
    let renamed = apply_renames(&root, &moves)?;
    let updated = (|| -> Result<()> {
        db::rename_rom(
            &tx,
            plan.rom_id,
            &plan.new_name,
            plan.library_file.as_ref().and_then(|f| f.to.to_str()),
        )?;
        if plan.new_game_name != plan.old_game_name {
            db::rename_game(&tx, plan.game_id, &plan.new_game_name)?;
            db::reset_unfound_cover(&tx, plan.game_id)?;
        }
        tx.commit().map_err(|e| Error::db(e, "commit rename"))
    })();
    if let Err(error) = updated {
        let undo: Vec<_> = moves
            .iter()
            .rev()
            .map(|m| FileRename {
                from: m.to.clone(),
                to: m.from.clone(),
                kind: m.kind,
            })
            .collect();
        return Err(Error::ApplyFailed {
            step: renamed,
            rolled_back: apply_renames(&root, &undo).is_ok(),
            source: Box::new(error),
        });
    }
    Ok(RenameResult {
        library_renamed: renamed,
    })
}

// ---------------------------------------------------------------------------
// Batch: every game that matches a standard name.

/// Library games whose file names aren't standard but match one: the rename
/// each would get. `standard` is looked up per platform via `names`.
pub fn standardize_plan(
    lib: &Library,
    platform: Option<&str>,
    mut names: impl FnMut(&str) -> Option<Vec<String>>,
) -> Result<Vec<RenamePlan>> {
    let games = db::list_games(lib.conn(), platform, None)?;
    let mut lists: std::collections::HashMap<String, Option<Vec<String>>> = Default::default();
    let mut out = Vec::new();
    for g in &games {
        for rom in &g.roms {
            let new_name = if let Some(name) = &rom.dat.rom_name {
                name.clone()
            } else {
                // Once a DAT has been consulted, do not guess a different
                // version from a similar filename.
                if rom.dat.status != super::identify::Status::Unchecked || rom.verified {
                    continue;
                }
                let list = lists
                    .entry(g.platform.clone())
                    .or_insert_with(|| names(&g.platform));
                let Some(list) = list else {
                    continue;
                };
                let Some(s) = crate::catalog::libretro::suggest(&rom.name, list) else {
                    continue;
                };
                format!("{s}{}", crate::fsutil::names::extension(&rom.name))
            };
            if new_name == rom.name {
                continue;
            }
            if let Ok(plan) = plan(lib, Some(&g.platform), &rom.id.to_string(), &new_name) {
                out.push(plan);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{library_with, write};

    #[test]
    fn resolves_by_id_name_or_game() {
        let (_t, lib) = library_with(&[("Foo (USA).fkh", "1"), ("Bar.fkh", "22")]);
        let (g, r) = resolve_rom(&lib, None, "foo (usa).fkh").unwrap();
        assert_eq!(g.name, "Foo (USA)");
        assert_eq!(resolve_rom(&lib, None, "Foo (USA)").unwrap().1.id, r.id);
        assert_eq!(
            resolve_rom(&lib, None, &r.id.to_string()).unwrap().1.name,
            "Foo (USA).fkh"
        );
        assert_eq!(
            resolve_rom(&lib, Some("gb"), "Foo (USA)")
                .unwrap_err()
                .code(),
            "ROM_NOT_FOUND"
        );
        assert_eq!(
            resolve_rom(&lib, None, "nope").unwrap_err().code(),
            "ROM_NOT_FOUND"
        );
    }

    #[test]
    fn local_rename_updates_the_managed_file_and_library_index() {
        let (_tmp, mut lib) = library_with(&[("Foo.fkh", "rom"), ("Bar.fkh", "other")]);
        write(lib.root(), "saves/Saves/fkh/common/Foo.sav", "backup");
        let preview = plan(&lib, None, "Foo.fkh", "New name").unwrap();
        assert_eq!(preview.new_name, "New name.fkh");
        apply(&mut lib, &preview).unwrap();
        let (game, rom) = resolve_rom(&lib, None, "New name.fkh").unwrap();
        assert_eq!(game.name, "New name");
        assert_eq!(std::fs::read(lib.rom_abs_path(&rom)).unwrap(), b"rom");
        assert_eq!(
            std::fs::read(lib.root().join("saves/Saves/fkh/common/Foo.sav")).unwrap(),
            b"backup"
        );
        assert!(plan(&lib, None, "Bar.fkh", "New name.fkh").is_err());
        assert!(apply(&mut lib, &preview).is_err());
    }

    #[test]
    fn database_failure_restores_the_original_file_and_index() {
        let (_tmp, mut lib) = library_with(&[("Foo.fkh", "rom")]);
        let preview = plan(&lib, None, "Foo.fkh", "New name").unwrap();
        lib.conn().execute_batch("CREATE TRIGGER reject_rename BEFORE UPDATE ON roms BEGIN SELECT RAISE(ABORT, 'test failure'); END;").unwrap();
        assert!(apply(&mut lib, &preview).is_err());
        let (_, rom) = resolve_rom(&lib, None, "Foo.fkh").unwrap();
        assert_eq!(std::fs::read(lib.rom_abs_path(&rom)).unwrap(), b"rom");
        assert!(!lib.root().join("roms/fkh/New name.fkh").exists());
    }
}
