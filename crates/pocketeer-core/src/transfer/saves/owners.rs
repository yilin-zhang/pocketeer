//! Which game a cartridge save belongs to. The Pocket names a save after
//! its ROM: `Assets/gba/common/Game.gba` saves to
//! `Saves/fkh/common/Game.sav`. So a save belongs to a game when it sits
//! in the save folder of one of the game's ROMs (where the library would
//! copy it, or where a copy was found on the card) with the same stem.
//!
//! Once matched, the library keeps the link, so renaming the game in the
//! library doesn't lose its saves. Memories are matched by their header
//! instead (see `card::memories`).

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use crate::{
    Result,
    card::{rom_dir, save_dir_for},
    fsutil::names::stem,
    library::{Library, db, db::Game},
};

/// A save folder and a lowercase file stem: what a ROM and its saves share.
pub type SaveKey = (PathBuf, String);

/// The key of a save file at card path `rel`.
pub fn save_key(rel: &Path) -> Option<SaveKey> {
    let name = rel.file_name()?.to_str()?;
    Some((rel.parent()?.to_path_buf(), stem(name).to_lowercase()))
}

/// The key a ROM at card path `rom_rel` gives its saves.
pub fn rom_key(rom_rel: &Path) -> Option<SaveKey> {
    let name = rom_rel.file_name()?.to_str()?;
    Some((save_dir_for(rom_rel)?, stem(name).to_lowercase()))
}

/// Every key a library game's saves can have: where the library copies
/// each ROM, plus `card_copies` (where copies were found on the card).
pub fn expected_saves(game: &Game, card_copies: &[PathBuf]) -> BTreeSet<SaveKey> {
    game.roms
        .iter()
        .map(|r| rom_dir(&game.platform).join(&r.name))
        .chain(card_copies.iter().cloned())
        .filter_map(|p| rom_key(&p))
        .collect()
}

/// Save card paths per library game id; saves no game owns are under `None`.
pub type Owners = BTreeMap<Option<i64>, Vec<PathBuf>>;

/// Links every cartridge save the library holds to the game it belongs to,
/// unless it is linked already, and returns who owns what. `copies` gives
/// the card paths found for each game id.
pub fn link_owners(
    lib: &Library,
    games: &[Game],
    copies: &BTreeMap<i64, Vec<PathBuf>>,
) -> Result<Owners> {
    let mut by_key: BTreeMap<SaveKey, i64> = BTreeMap::new();
    for game in games {
        let found = copies.get(&game.id).map(Vec::as_slice).unwrap_or(&[]);
        for key in expected_saves(game, found) {
            by_key.entry(key).or_insert(game.id);
        }
    }
    let mut linked = db::save_games(lib.conn())?;
    let mut owners = Owners::new();
    for rel in library_saves(lib)? {
        let owner = match linked.remove(&rel) {
            Some(owner) => owner,
            None => {
                let found = save_key(&rel).and_then(|k| by_key.get(&k).copied());
                if let Some(id) = found {
                    db::set_save_game(lib.conn(), &rel, id)?;
                }
                found
            }
        };
        owners.entry(owner).or_default().push(rel);
    }
    Ok(owners)
}

/// Card paths of the cartridge saves the library has: current backups and
/// paths that only have history left.
fn library_saves(lib: &Library) -> Result<BTreeSet<PathBuf>> {
    let mut paths: BTreeSet<PathBuf> =
        super::files(&lib.root().join("saves"), super::Scope::Saves)?
            .into_iter()
            .collect();
    paths.extend(
        db::save_version_paths(lib.conn())?
            .into_iter()
            .filter(|p| super::Scope::Saves.covers(p)),
    );
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{library_with, write_with_mtime as write};

    #[test]
    fn saves_are_linked_by_name_and_the_link_survives_a_rename() {
        let (_tmp, mut lib) = library_with(&[("Metroid Fusion (USA).fkh", "rom")]);
        write(
            lib.root(),
            "saves/Saves/fkh/common/metroid fusion (usa).sav",
            "save",
            100,
        );
        write(lib.root(), "saves/Saves/fkh/common/Other.sav", "x", 100);
        // A copy on the card under another name owns its saves too.
        write(lib.root(), "saves/Saves/fkh/common/sub/MF.sav", "y", 100);
        let games = lib.list_games(None, None).unwrap();
        let id = games[0].id;
        let copies = BTreeMap::from([(id, vec![PathBuf::from("Assets/fkh/common/sub/MF.fkh")])]);
        let owners = link_owners(&lib, &games, &copies).unwrap();
        assert_eq!(
            owners[&Some(id)],
            [
                PathBuf::from("Saves/fkh/common/metroid fusion (usa).sav"),
                PathBuf::from("Saves/fkh/common/sub/MF.sav"),
            ]
        );
        assert_eq!(owners[&None], [PathBuf::from("Saves/fkh/common/Other.sav")]);

        // Renamed in the library: the stored link still holds.
        let plan =
            crate::library::rename::plan(&lib, Some("fkh"), "Metroid Fusion (USA).fkh", "Renamed")
                .unwrap();
        crate::library::rename::apply(&mut lib, &plan).unwrap();
        let games = lib.list_games(None, None).unwrap();
        assert_eq!(games[0].roms[0].name, "Renamed.fkh");
        let owners = link_owners(&lib, &games, &BTreeMap::new()).unwrap();
        assert_eq!(owners[&Some(id)].len(), 2);
    }
}
