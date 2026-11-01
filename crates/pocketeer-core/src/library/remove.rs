//! Removing ROMs from the library. Managed files go to `.trash/<timestamp>/`
//! inside the library (never erased); referenced originals are left alone.
//! Card copies are managed separately.

use std::path::PathBuf;

use serde::Serialize;

use crate::{Error, Result, fsutil::timestamp};

use super::{Library, db};

pub const TRASH_DIR: &str = ".trash";

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RemoveItem {
    pub rom_id: i64,
    pub game_id: i64,
    pub platform: String,
    pub game: String,
    pub name: String,
    pub size: u64,
    /// Managed file, relative to the library root; `None` for references.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<PathBuf>,
    /// Whether this is the game's last ROM (the game row goes too).
    pub last_of_game: bool,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct RemovePlan {
    pub items: Vec<RemoveItem>,
    pub bytes: u64,
    /// Where files end up, relative to the library root.
    pub trash: PathBuf,
}

impl RemovePlan {
    pub fn changes(&self) -> usize {
        self.items.len()
    }
}

/// Every `spec` is a ROM id, file name or game name. A game name takes all
/// of the game's ROMs.
pub fn plan(lib: &Library, platform: Option<&str>, specs: &[String]) -> Result<RemovePlan> {
    let mut plan = RemovePlan {
        trash: PathBuf::from(TRASH_DIR).join(timestamp()),
        ..Default::default()
    };
    for spec in specs {
        let hits = super::resolve_game_roms(lib, platform, spec)?;
        for (game, rom) in hits {
            if plan.items.iter().any(|i| i.rom_id == rom.id) {
                continue;
            }
            let taken_from_game = plan.items.iter().filter(|i| i.game_id == game.id).count();
            plan.bytes += rom.size;
            plan.items.push(RemoveItem {
                rom_id: rom.id,
                game_id: game.id,
                platform: game.platform.clone(),
                game: game.name.clone(),
                name: rom.name.clone(),
                size: rom.size,
                file: rom.rel_path.clone(),
                last_of_game: taken_from_game + 1 == game.roms.len(),
            });
        }
    }
    Ok(plan)
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct RemoveResult {
    pub removed: usize,
    pub games_removed: usize,
    pub bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trash: Option<PathBuf>,
    pub failed: Vec<RemoveFailure>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RemoveFailure {
    pub name: String,
    pub error: crate::ErrorInfo,
}

pub fn apply(lib: &mut Library, plan: &RemovePlan) -> Result<RemoveResult> {
    let root = lib.root().to_path_buf();
    let trash = root.join(&plan.trash);
    let mut result = RemoveResult::default();
    for item in &plan.items {
        let moved: Result<()> = match &item.file {
            Some(rel) => {
                let from = root.join(rel);
                if from.exists() {
                    let to = trash.join(rel);
                    std::fs::create_dir_all(to.parent().unwrap_or(&trash))
                        .map_err(|e| Error::io(&trash, e))
                        .and_then(|_| std::fs::rename(&from, &to).map_err(|e| Error::io(&from, e)))
                } else {
                    Ok(())
                }
            }
            None => Ok(()),
        };
        match moved.and_then(|_| db::delete_rom(lib.conn(), item.rom_id, item.game_id)) {
            Ok(game_gone) => {
                result.removed += 1;
                result.bytes += item.size;
                if game_gone {
                    result.games_removed += 1;
                }
            }
            Err(e) => result.failed.push(RemoveFailure {
                name: item.name.clone(),
                error: e.to_info(),
            }),
        }
    }
    result.trash = trash.exists().then_some(plan.trash.clone());
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::library_with;

    #[test]
    fn removes_to_trash_and_drops_empty_games() {
        let (_tmp, mut lib) = library_with(&[("Foo.fkh", "1"), ("Bar.fkh", "22")]);

        assert_eq!(
            plan(&lib, None, &["nope".into()]).unwrap_err().code(),
            "ROM_NOT_FOUND"
        );
        let plan = plan(&lib, None, &["Foo".into(), "foo.fkh".into()]).unwrap();
        assert_eq!(plan.items.len(), 1, "same ROM twice counts once");
        assert!(plan.items[0].last_of_game);

        let r = apply(&mut lib, &plan).unwrap();
        assert_eq!((r.removed, r.games_removed, r.bytes), (1, 1, 1));
        assert!(r.failed.is_empty());
        assert!(!lib.root().join("roms/fkh/Foo.fkh").exists());
        assert!(
            lib.root()
                .join(&plan.trash)
                .join("roms/fkh/Foo.fkh")
                .is_file()
        );
        assert_eq!(db::list_games(lib.conn(), None, None).unwrap().len(), 1);
    }
}
