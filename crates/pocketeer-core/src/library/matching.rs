//! Content matching is independent of names, device registration and copy policy.

use super::db::Game;
use crate::roms::index::{CardRom, Confidence};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    Both,
    Local,
    Card,
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
pub struct CardMatch {
    pub path: PathBuf,
    pub name: String,
    pub confidence: Confidence,
    pub name_mismatch: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct LocalMatch {
    pub rom_id: i64,
    pub platform: String,
    pub name: String,
    pub presence: Presence,
    pub copies: Vec<CardMatch>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Comparison {
    pub local: BTreeMap<i64, LocalMatch>,
    /// One status per card file in the original scan order.
    pub card: Vec<Presence>,
}

pub fn compare<'a>(games: impl IntoIterator<Item = &'a Game>, card: &[CardRom]) -> Comparison {
    let mut by_hash: HashMap<(&str, &str), Vec<&CardRom>> = HashMap::new();
    let mut unknown = HashSet::new();
    for rom in card {
        if let Some(sha1) = &rom.sha1 {
            by_hash.entry((&rom.platform, sha1)).or_default().push(rom);
        } else {
            unknown.insert((rom.platform.as_str(), rom.size));
        }
    }
    let mut local_hashes = HashSet::new();
    let mut local = BTreeMap::new();
    for game in games {
        for rom in &game.roms {
            local_hashes.insert((game.platform.as_str(), rom.sha1.as_str()));
            let copies: Vec<_> = by_hash
                .get(&(game.platform.as_str(), rom.sha1.as_str()))
                .into_iter()
                .flatten()
                .map(|c| CardMatch {
                    path: c.rel_path.clone(),
                    name: c.name.clone(),
                    confidence: c.confidence,
                    name_mismatch: c.name != rom.name,
                })
                .collect();
            let presence = if !copies.is_empty() {
                Presence::Both
            } else if unknown.contains(&(game.platform.as_str(), rom.size)) {
                Presence::Unknown
            } else {
                Presence::Local
            };
            local.insert(
                rom.id,
                LocalMatch {
                    rom_id: rom.id,
                    platform: game.platform.clone(),
                    name: rom.name.clone(),
                    presence,
                    copies,
                },
            );
        }
    }
    let card = card
        .iter()
        .map(|rom| match &rom.sha1 {
            None => Presence::Unknown,
            Some(sha1) if local_hashes.contains(&(rom.platform.as_str(), sha1.as_str())) => {
                Presence::Both
            }
            Some(_) => Presence::Card,
        })
        .collect();
    Comparison { local, card }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        roms::index::Index,
        testutil::{library_with, sd, write},
    };

    #[test]
    fn matching_tracks_identity_uncertainty_and_every_copy() {
        let (_lib_tmp, lib) = library_with(&[("local.gba", "ROM A"), ("other.gba", "ROM B")]);
        let (tmp, pocket) = sd();
        write(tmp.path(), "Assets/fkh/common/renamed.gba", "ROM A");
        write(
            tmp.path(),
            "Assets/fkh/common/folder/duplicate.gba",
            "ROM A",
        );
        write(tmp.path(), "Assets/fkh/common/other.gba", "ROM C");
        write(tmp.path(), "Assets/gb/common/local.gba", "ROM A");
        let games = lib.list_games(None, None).unwrap();
        let mut index = Index::scan(&pocket).unwrap();
        let pending = compare(&games, &index.roms);
        assert!(
            pending
                .local
                .values()
                .all(|m| m.presence == Presence::Unknown)
        );
        assert!(pending.card.iter().all(|p| *p == Presence::Unknown));
        for i in 0..index.roms.len() {
            index.verify(&pocket, i, false, &|| false);
        }
        let matched = compare(&games, &index.roms);
        let local = matched
            .local
            .values()
            .find(|m| m.name == "local.gba")
            .unwrap();
        assert_eq!(local.presence, Presence::Both);
        assert_eq!(local.copies.len(), 2);
        assert!(local.copies.iter().all(|c| c.name_mismatch));
        assert_eq!(
            matched
                .local
                .values()
                .find(|m| m.name == "other.gba")
                .unwrap()
                .presence,
            Presence::Local
        );
        assert_eq!(
            matched
                .card
                .iter()
                .filter(|p| **p == Presence::Card)
                .count(),
            2
        );
    }
}
