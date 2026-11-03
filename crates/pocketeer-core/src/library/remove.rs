//! Removing ROMs from the library. Managed files go to `.trash/<timestamp>/`
//! inside the library (never erased); referenced originals are left alone.
//! Card copies are managed separately.

use std::path::PathBuf;

use serde::Serialize;

use crate::{
    Error, Result,
    plan::{Applied, Event, Failure, Pinned, Progress},
};

use super::{Library, RomRef, db};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RemoveItem {
    #[serde(flatten)]
    pub rom: RomRef,
    pub size: u64,
    /// The file, relative to the library root.
    pub file: PathBuf,
    /// Whether this is the game's last ROM (the game row goes too).
    pub last_of_game: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RemovePlan {
    pub items: Vec<RemoveItem>,
    pub bytes: u64,
    /// Where files end up, relative to the library root.
    pub trash: PathBuf,
    #[serde(skip)]
    pinned: Pinned,
}

impl RemovePlan {
    pub fn changes(&self) -> usize {
        self.items.len()
    }

    pub fn apply(&mut self, lib: &mut Library, progress: Progress<'_>) -> Result<RemoveResult> {
        self.pinned.check_library(lib)?;
        progress(Event::Status("Removing from library".into()));
        let root = lib.root().to_path_buf();
        let trash = root.join(&self.trash);
        let mut result = RemoveResult::default();
        for item in &self.items {
            let from = root.join(&item.file);
            let moved: Result<()> = if from.exists() {
                let to = trash.join(&item.file);
                std::fs::create_dir_all(to.parent().unwrap_or(&trash))
                    .map_err(|e| Error::io(&trash, e))
                    .and_then(|_| std::fs::rename(&from, &to).map_err(|e| Error::io(&from, e)))
            } else {
                Ok(())
            };
            match moved.and_then(|_| db::delete_rom(lib.conn(), item.rom.rom_id, item.rom.game_id))
            {
                Ok(game_gone) => {
                    result.removed += 1;
                    result.bytes += item.size;
                    if game_gone {
                        result.games_removed += 1;
                    }
                }
                Err(e) => result.failed.push(Failure::new(&item.file, &e)),
            }
        }
        result.trash = trash.exists().then_some(self.trash.clone());
        Ok(result)
    }
}

/// Every `spec` is a ROM id, file name or game name. A game name takes all
/// of the game's ROMs.
pub fn plan(lib: &Library, platform: Option<&str>, specs: &[String]) -> Result<RemovePlan> {
    let mut plan = RemovePlan {
        items: Vec::new(),
        bytes: 0,
        trash: Library::new_trash_dir(),
        pinned: Pinned::library(lib)?,
    };
    for spec in specs {
        let hits = super::resolve_game_roms(lib, platform, spec)?;
        for (game, rom) in hits {
            if plan.items.iter().any(|i| i.rom.rom_id == rom.id) {
                continue;
            }
            let taken_from_game = plan
                .items
                .iter()
                .filter(|i| i.rom.game_id == game.id)
                .count();
            plan.bytes += rom.size;
            plan.items.push(RemoveItem {
                rom: RomRef::of(&game, &rom),
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
    pub failed: Vec<Failure>,
}

impl Applied for RemoveResult {
    fn failures(&self) -> &[Failure] {
        &self.failed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::library_with;

    #[test]
    fn a_plan_for_another_library_is_refused() {
        let (tmp, lib) = library_with(&[("game.fkh", "ROM")]);
        let mut plan = plan(&lib, None, &["game.fkh".into()]).unwrap();
        // Another library now sits where this one was.
        let root = lib.root().to_path_buf();
        drop(lib);
        std::fs::rename(&root, tmp.path().join("old")).unwrap();
        let other = Library::init(&root).unwrap();
        assert_eq!(
            plan.apply(&mut crate::testutil::reopen(&other), &mut |_| {})
                .unwrap_err()
                .code(),
            "STALE_PLAN"
        );
        assert!(tmp.path().join("old/roms/fkh/game.fkh").is_file());
    }

    #[test]
    fn removes_to_trash_and_drops_empty_games() {
        let (_tmp, lib) = library_with(&[("Foo.fkh", "1"), ("Bar.fkh", "22")]);

        assert_eq!(
            plan(&lib, None, &["nope".into()]).unwrap_err().code(),
            "ROM_NOT_FOUND"
        );
        let mut plan = plan(&lib, None, &["Foo".into(), "foo.fkh".into()]).unwrap();
        assert_eq!(plan.items.len(), 1, "same ROM twice counts once");
        assert!(plan.items[0].last_of_game);

        let r = plan
            .apply(&mut crate::testutil::reopen(&lib), &mut |_| {})
            .unwrap();
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
