//! Card ⇄ library: the only module that knows both sides.
//!
//! Matching joins what is on a card to what the library holds by platform
//! and content hash, never by name ([`compare`]); [`to_card`] and
//! [`from_card`] copy ROMs across explicitly; [`saves`] backs up and
//! restores save files with history; [`fingerprint`] notices when either
//! side changed under the TUI. Nothing here registers devices or keeps a
//! desired card state.

pub mod from_card;
pub mod saves;
pub mod to_card;

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
};

use serde::Serialize;

use crate::{
    Pocket,
    card::roms::{CardRom, Confidence},
    library::db::Game,
};

// ---------------------------------------------------------------------------
// Matching

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

// ---------------------------------------------------------------------------
// Change detection. Directory metadata only; file contents are never read.

fn stamp(path: &Path, h: &mut impl Hasher) {
    if let Ok(meta) = std::fs::metadata(path)
        && let Ok(m) = meta.modified()
    {
        path.hash(h);
        m.hash(h);
        meta.len().hash(h);
    }
}

fn stamp_tree(dir: &Path, h: &mut impl Hasher) {
    for entry in walkdir::WalkDir::new(dir)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| e.file_type().is_dir() && !crate::fsutil::is_hidden(e.file_name()))
        .filter_map(std::result::Result::ok)
    {
        stamp(entry.path(), h);
    }
}

/// Changes when a ROM, core, save or library row was added, removed or
/// renamed by anyone (this process included). Either side may be absent:
/// no card mounted, or no library yet.
pub fn fingerprint(pocket: Option<&Pocket>, library: Option<&Path>) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    if let Some(pocket) = pocket {
        for dir in crate::card::WATCHED_DIRS {
            stamp_tree(&pocket.root().join(dir), &mut h);
        }
    }
    if let Some(lib) = library {
        for f in ["library.sqlite", "library.sqlite-wal"] {
            stamp(&lib.join(f), &mut h);
        }
    }
    h.finish()
}

#[cfg(test)]
mod matching_tests {
    use super::*;
    use crate::{
        card::roms::Index,
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

#[cfg(test)]
mod watch_tests {
    use super::*;
    use crate::testutil::{sd, write};

    #[test]
    fn changes_when_a_rom_is_added_or_renamed() {
        let (tmp, pocket) = sd();
        write(tmp.path(), "Assets/gba/common/series/region/a.gba", "1");
        let a = fingerprint(Some(&pocket), None);
        assert_eq!(
            a,
            fingerprint(Some(&pocket), None),
            "stable when nothing changed"
        );
        // Directory mtimes have 1 s granularity on some file systems; force a
        // different listing rather than relying on time passing.
        std::fs::rename(
            tmp.path().join("Assets/gba/common/series/region/a.gba"),
            tmp.path().join("Assets/gba/common/series/region/b.gba"),
        )
        .unwrap();
        let dir = tmp.path().join("Assets/gba/common/series/region");
        let f = std::fs::File::open(&dir).unwrap();
        let _ = f.set_modified(
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000),
        );
        assert_ne!(a, fingerprint(Some(&pocket), None));
    }
}
