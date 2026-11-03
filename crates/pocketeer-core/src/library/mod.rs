//! Managed local ROM copies, explicit card transfers, and save backups.
//!
//! ```text
//! ~/Pocketeer Library/
//! ├── library.sqlite      index (games, roms, platforms, …)
//! └── roms/<platform>/    managed ROM files, with user-controlled filenames
//! ```
//!
//! Card presence is discovered from files and joined by content hash; no
//! device registration or desired card contents are stored.

pub mod covers;
pub(crate) mod db;
pub mod identify;
pub mod import;
pub mod remove;
pub mod rename;

pub use db::{Game, Rom, SaveVersion};

use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde::Serialize;

use crate::{Error, Result};

pub const DB_FILE: &str = "library.sqlite";
const DEFAULT_DIR_NAME: &str = "Pocketeer Library";

/// `~/Pocketeer Library`, if a home directory is known.
pub fn default_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(DEFAULT_DIR_NAME))
}

pub struct Library {
    root: PathBuf,
    conn: Connection,
}

impl std::fmt::Debug for Library {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Library").field("root", &self.root).finish()
    }
}

impl Library {
    /// Opens an existing library.
    pub fn open(root: &Path) -> Result<Self> {
        let db = root.join(DB_FILE);
        if !db.is_file() {
            return Err(Error::LibraryNotFound {
                path: root.to_path_buf(),
            });
        }
        Ok(Library {
            root: root.to_path_buf(),
            conn: connect(&db)?,
        })
    }

    /// Creates a new library at `root` (the folder may exist but must not
    /// already hold a library).
    pub fn init(root: &Path) -> Result<Self> {
        let db = root.join(DB_FILE);
        if db.exists() {
            return Err(Error::LibraryExists {
                path: root.to_path_buf(),
            });
        }
        std::fs::create_dir_all(root.join("roms")).map_err(|e| Error::io(root, e))?;
        Ok(Library {
            root: root.to_path_buf(),
            conn: connect(&db)?,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// A new folder in the library's trash, relative to its root, for one
    /// operation's discarded files.
    pub(crate) fn new_trash_dir() -> PathBuf {
        PathBuf::from(".trash").join(crate::fsutil::timestamp())
    }

    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Games (with their ROMs), optionally filtered by platform and a
    /// case-insensitive name substring; sorted by platform then name.
    pub fn list_games(&self, platform: Option<&str>, query: Option<&str>) -> Result<Vec<db::Game>> {
        db::list_games(&self.conn, platform, query)
    }

    /// Per-platform counts and bytes.
    pub fn platform_stats(&self) -> Result<Vec<PlatformStats>> {
        db::platform_stats(&self.conn)
    }

    pub(crate) fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// Where a managed ROM for `platform` named `name` lives, relative to root.
    pub fn rom_rel_path(platform: &str, name: &str) -> PathBuf {
        PathBuf::from("roms").join(platform).join(name)
    }

    /// Absolute path of a ROM's managed copy.
    pub fn rom_abs_path(&self, rom: &db::Rom) -> PathBuf {
        self.root.join(&rom.rel_path)
    }

    pub fn status(&self) -> Result<Status> {
        let platforms = db::platform_stats(&self.conn)?;
        Ok(Status {
            root: self.root.clone(),
            games: platforms.iter().map(|p| p.games).sum(),
            roms: platforms.iter().map(|p| p.roms).sum(),
            bytes: platforms.iter().map(|p| p.bytes).sum(),
            platforms,
        })
    }
}

/// Opens (or creates) the database file and brings its schema up to date.
fn connect(db: &Path) -> Result<Connection> {
    let mut conn = Connection::open(db).map_err(|e| Error::db(e, "open"))?;
    // Two processes/threads may hold the library at once (the TUI fetches
    // covers in the background); wait instead of failing with SQLITE_BUSY.
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|e| Error::db(e, "busy_timeout"))?;
    db::migrate(&mut conn)?;
    Ok(conn)
}

// Turning what a user typed into library rows: a numeric ROM id, a ROM file
// name, or a game name (both case-insensitive), optionally on one platform.

/// Every ROM `spec` names, each with its full game (ROM list included).
/// Empty when nothing matches.
fn find_roms(lib: &Library, platform: Option<&str>, spec: &str) -> Result<Vec<(Game, Rom)>> {
    if let Ok(id) = spec.parse::<i64>()
        && let Some(rom) = db::get_rom(lib.conn(), id)?
        && let Some(game) = db::get_game(lib.conn(), rom.game_id)?
        && platform.is_none_or(|p| p == game.platform)
    {
        return Ok(vec![(game, rom)]);
    }
    let hits = db::find_roms_by_spec(lib.conn(), platform, spec)?;
    let mut out = Vec::with_capacity(hits.len());
    let mut last: Option<Game> = None;
    for (game, rom) in hits {
        // The join gives the game without its ROM list; load it once per game.
        if last.as_ref().is_none_or(|g| g.id != game.id) {
            last = Some(db::get_game(lib.conn(), game.id)?.unwrap_or(game));
        }
        out.push((last.clone().expect("set above"), rom));
    }
    Ok(out)
}

fn candidates(hits: &[(Game, Rom)]) -> Vec<String> {
    hits.iter()
        .map(|(g, r)| format!("{}/{} (id {})", g.platform, r.name, r.id))
        .collect()
}

/// Exactly one ROM, or an error naming the alternatives.
pub fn resolve_rom(lib: &Library, platform: Option<&str>, spec: &str) -> Result<(Game, Rom)> {
    let mut hits = find_roms(lib, platform, spec)?;
    match hits.len() {
        0 => Err(Error::RomNotFound {
            spec: spec.to_string(),
        }),
        1 => Ok(hits.remove(0)),
        _ => Err(Error::RomAmbiguous {
            spec: spec.to_string(),
            candidates: candidates(&hits),
        }),
    }
}

/// One ROM, or every ROM of one game (a game name matches all of them).
/// The same name on several platforms is ambiguous.
pub fn resolve_game_roms(
    lib: &Library,
    platform: Option<&str>,
    spec: &str,
) -> Result<Vec<(Game, Rom)>> {
    let hits = find_roms(lib, platform, spec)?;
    if hits.is_empty() {
        return Err(Error::RomNotFound {
            spec: spec.to_string(),
        });
    }
    let mut games: Vec<i64> = hits.iter().map(|(g, _)| g.id).collect();
    games.dedup();
    if games.len() > 1 {
        return Err(Error::RomAmbiguous {
            spec: spec.to_string(),
            candidates: candidates(&hits),
        });
    }
    Ok(hits)
}

/// Which ROM a plan is about, named as it was when planned.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RomRef {
    pub rom_id: i64,
    pub game_id: i64,
    pub platform: String,
    pub game: String,
    pub name: String,
}

impl RomRef {
    pub(crate) fn of(game: &Game, rom: &Rom) -> Self {
        RomRef {
            rom_id: rom.id,
            game_id: game.id,
            platform: game.platform.clone(),
            game: game.name.clone(),
            name: rom.name.clone(),
        }
    }
}

/// One platform of the library: what `library status` and `covers status`
/// both show.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PlatformStats {
    pub platform: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub games: usize,
    pub roms: usize,
    /// ROMs a DAT matched.
    pub verified: usize,
    pub bytes: u64,
    /// Games with a cover on disk.
    pub covers: usize,
    /// Games libretro has no cover for.
    pub covers_not_found: usize,
    /// Whether libretro-thumbnails has a folder for this platform; without
    /// one, fetching needs `--libretro-system`.
    pub covers_mapped: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Status {
    pub root: PathBuf,
    pub games: usize,
    pub roms: usize,
    pub bytes: u64,
    pub platforms: Vec<PlatformStats>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_then_open_and_refuse_double_init() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("lib");
        assert_eq!(
            Library::open(&root).unwrap_err().code(),
            "LIBRARY_NOT_FOUND"
        );
        let lib = Library::init(&root).unwrap();
        assert!(root.join(DB_FILE).is_file());
        assert!(root.join("roms").is_dir());
        drop(lib);
        assert_eq!(Library::init(&root).unwrap_err().code(), "LIBRARY_EXISTS");
        let lib = Library::open(&root).unwrap();
        let st = lib.status().unwrap();
        assert_eq!(st.roms, 0);
        assert!(st.platforms.is_empty());
    }
}
