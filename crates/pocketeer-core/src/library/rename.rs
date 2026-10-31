//! Renaming a library ROM. The display name *is* the file name: the game
//! row, the ROM row, the managed file, the card's copy and every save keyed
//! to the old name move together.
//!
//! When no card is mounted (or another card has the old name), the rename is
//! remembered in the `renames` table and `device sync` applies it to any card
//! that still carries the old name: no file is read, just renamed.

use std::path::{Path, PathBuf};

use rusqlite::params;
use serde::Serialize;

use crate::{
    Error, Pocket, Result, fsutil,
    roms::{
        card,
        rename::{
            FileRename, RenameKind, apply_renames, companion_renames, extension, stem,
            validate_name,
        },
    },
};

use super::{Library, covers, db, resolve_rom, saves};

pub(crate) const MIGRATION_RENAMES: &str = "
    CREATE TABLE renames (
        id         INTEGER PRIMARY KEY,
        platform   TEXT NOT NULL,
        old_name   TEXT NOT NULL,
        new_name   TEXT NOT NULL,
        created_at INTEGER NOT NULL
    );
    CREATE INDEX renames_platform ON renames(platform, id);
";

/// Remembers a rename so cards that still have `old_name` get it on sync.
pub fn record_rename(
    conn: &rusqlite::Connection,
    platform: &str,
    old: &str,
    new: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO renames (platform, old_name, new_name, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![platform, old, new, fsutil::now_secs()],
    )
    .map_err(|e| Error::db(e, "record rename"))?;
    Ok(())
}

/// Every rename ever made on a platform, oldest first (so chains A→B→C replay
/// in order). Cards apply the ones whose old name they still carry.
pub fn pending_renames(
    conn: &rusqlite::Connection,
    platform: &str,
) -> Result<Vec<(String, String)>> {
    db::query_rows(
        conn,
        "SELECT old_name, new_name FROM renames WHERE platform = ?1 ORDER BY id",
        params![platform],
        "pending renames",
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
}

/// What a card contributes to the rename.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CardPart {
    pub root: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// ROM copies and their saves, relative to the card root.
    pub renames: Vec<FileRename>,
}

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
    /// Save mirror moves, relative to the library root.
    pub library_saves: Vec<FileRename>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card: Option<CardPart>,
}

impl RenamePlan {
    /// File moves plus the index update.
    pub fn changes(&self) -> usize {
        1 + self.library_file.is_some() as usize
            + self.library_saves.len()
            + self.card.as_ref().map(|c| c.renames.len()).unwrap_or(0)
    }
}

/// `new_name` without an extension inherits the old one.
fn complete_name(old_name: &str, new_name: &str) -> String {
    let new_name = new_name.trim();
    if extension(new_name).is_empty() {
        format!("{new_name}{}", extension(old_name))
    } else {
        new_name.to_string()
    }
}

pub fn plan(
    lib: &Library,
    pocket: Option<&Pocket>,
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
    let library_saves = companion_renames(
        &lib.root().join("saves"),
        &game.platform,
        &rom.name,
        &new_name,
    )
    .into_iter()
    .map(|r| FileRename {
        from: Path::new("saves").join(r.from),
        to: Path::new("saves").join(r.to),
        kind: r.kind,
    })
    .collect();

    let card = match pocket {
        None => None,
        Some(pocket) => {
            let mut renames = Vec::new();
            for dir in card::platform_dirs(pocket, &game.platform)? {
                if !dir.files.iter().any(|f| f.name == rom.name) {
                    continue;
                }
                let to = dir.path.join(&new_name);
                if pocket.root().join(&to).exists() {
                    return Err(taken(&format!(
                        "a file on the card in {}",
                        dir.path.display()
                    )));
                }
                renames.push(FileRename {
                    from: dir.path.join(&rom.name),
                    to,
                    kind: RenameKind::Rom,
                });
            }
            if renames.is_empty() {
                None
            } else {
                renames.extend(companion_renames(
                    pocket.root(),
                    &game.platform,
                    &rom.name,
                    &new_name,
                ));
                Some(CardPart {
                    root: pocket.root().to_path_buf(),
                    device: super::device::read_marker(pocket)?.map(|m| m.name),
                    renames,
                })
            }
        }
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
        library_saves,
        card,
    })
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct RenameResult {
    pub library_renamed: usize,
    pub card_renamed: usize,
    pub failed: Vec<RenameFailure>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RenameFailure {
    pub path: PathBuf,
    pub error: crate::ErrorInfo,
}

/// Library first (file, then index in one transaction), card second. A card
/// failure leaves the library renamed; the next sync finishes the card.
pub fn apply(
    lib: &mut Library,
    pocket: Option<&Pocket>,
    plan: &RenamePlan,
) -> Result<RenameResult> {
    let mut result = RenameResult::default();
    let root = lib.root().to_path_buf();

    // One atomic batch: a failed save move puts the ROM file back too, so
    // the index still matches the disk.
    let library_moves: Vec<FileRename> = plan
        .library_file
        .iter()
        .chain(&plan.library_saves)
        .cloned()
        .collect();
    result.library_renamed = apply_renames(&root, &library_moves)?;

    let tx = lib
        .conn_mut()
        .transaction()
        .map_err(|e| Error::db(e, "begin rename"))?;
    db::rename_rom(
        &tx,
        plan.rom_id,
        &plan.new_name,
        plan.library_file.as_ref().and_then(|f| f.to.to_str()),
    )?;
    if plan.new_game_name != plan.old_game_name {
        db::rename_game(&tx, plan.game_id, &plan.new_game_name)?;
    }
    for r in &plan.library_saves {
        // Save rows are keyed by the card-relative path (without `saves/`).
        saves::rename_path(&tx, strip_saves(&r.from), strip_saves(&r.to))?;
    }
    record_rename(&tx, &plan.platform, &plan.old_name, &plan.new_name)?;
    if plan.new_game_name != plan.old_game_name {
        // A cover that wasn't found under the old name may exist under the new one.
        covers::reset_unfound(&tx, plan.game_id)?;
    }
    tx.commit().map_err(|e| Error::db(e, "commit rename"))?;

    if let (Some(card), Some(pocket)) = (&plan.card, pocket) {
        match apply_renames(pocket.root(), &card.renames) {
            Ok(n) => result.card_renamed = n,
            Err(e) => result.failed.push(RenameFailure {
                path: failing_path(&e)
                    .and_then(|p| p.strip_prefix(pocket.root()).ok())
                    .map(Path::to_path_buf)
                    .unwrap_or_default(),
                error: e.to_info(),
            }),
        }
    }
    Ok(result)
}

fn strip_saves(p: &Path) -> &Path {
    p.strip_prefix("saves").unwrap_or(p)
}

/// The file a rename failure is about, when the error names one.
fn failing_path(e: &Error) -> Option<&Path> {
    match e {
        Error::Io { path, .. } | Error::AlreadyExists { path } | Error::NotADirectory { path } => {
            Some(path)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{library_with, sd, write};

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
    fn renames_library_and_card_together() {
        let (_t, mut lib) = library_with(&[("Foo (USA).fkh", "hello world"), ("Bar.fkh", "22")]);
        let (_sd, pocket) = sd();
        write(
            pocket.root(),
            "Assets/fkh/common/Foo (USA).fkh",
            "hello world",
        );
        write(pocket.root(), "Saves/fkh/common/Foo (USA).sav", "s");
        write(pocket.root(), "Saves/fkh/common/Bar.sav", "s");
        write(lib.root(), "saves/Saves/fkh/common/Foo (USA).sav", "s");
        lib.conn()
            .execute(
                "INSERT INTO saves (rel_path, size, sha1, mtime, updated_at) VALUES ('Saves/fkh/common/Foo (USA).sav', 1, 'x', 0, 0)",
                [],
            )
            .unwrap();

        // Extension is inherited.
        let plan = plan(&lib, Some(&pocket), None, "Foo (USA)", "Foo (Europe)").unwrap();
        assert_eq!(plan.new_name, "Foo (Europe).fkh");
        assert_eq!(plan.new_game_name, "Foo (Europe)");
        assert_eq!(
            plan.library_file.as_ref().unwrap().to,
            Path::new("roms/fkh/Foo (Europe).fkh")
        );
        assert_eq!(plan.library_saves.len(), 1);
        let card = plan.card.as_ref().unwrap();
        assert_eq!(card.renames.len(), 2, "{:?}", card.renames);
        assert_eq!(plan.changes(), 5);

        let r = apply(&mut lib, Some(&pocket), &plan).unwrap();
        assert!(r.failed.is_empty());
        assert_eq!((r.library_renamed, r.card_renamed), (2, 2));
        assert!(lib.root().join("roms/fkh/Foo (Europe).fkh").is_file());
        assert!(
            lib.root()
                .join("saves/Saves/fkh/common/Foo (Europe).sav")
                .is_file()
        );
        assert!(
            pocket
                .root()
                .join("Assets/fkh/common/Foo (Europe).fkh")
                .is_file()
        );
        assert!(
            pocket
                .root()
                .join("Saves/fkh/common/Foo (Europe).sav")
                .is_file()
        );
        assert!(pocket.root().join("Saves/fkh/common/Bar.sav").is_file());
        let (g, rom) = resolve_rom(&lib, None, "Foo (Europe).fkh").unwrap();
        assert_eq!(g.name, "Foo (Europe)");
        assert_eq!(
            rom.rel_path.as_deref(),
            Some(Path::new("roms/fkh/Foo (Europe).fkh"))
        );
        let saved: String = lib
            .conn()
            .query_row("SELECT rel_path FROM saves", [], |r| r.get(0))
            .unwrap();
        assert_eq!(saved, "Saves/fkh/common/Foo (Europe).sav");

        // Conflicts.
        assert_eq!(
            super::plan(&lib, None, None, "Bar.fkh", "Foo (Europe).fkh")
                .unwrap_err()
                .code(),
            "NAME_TAKEN"
        );
        assert_eq!(
            super::plan(&lib, None, None, "Bar.fkh", "a/b")
                .unwrap_err()
                .code(),
            "INVALID_NAME"
        );
        assert_eq!(
            super::plan(&lib, None, None, "Bar.fkh", "Bar")
                .unwrap_err()
                .code(),
            "NAME_TAKEN"
        );
        // No card: library only.
        let p2 = super::plan(&lib, None, None, "Bar.fkh", "Baz").unwrap();
        assert!(p2.card.is_none());
        assert_eq!(p2.changes(), 2);
    }
}
