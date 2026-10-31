//! Schema and queries. Plain SQL, no ORM; each schema change is a numbered
//! migration applied in order.

use std::path::PathBuf;

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::{Error, Result, fsutil};

use super::import::ImportMode;

const MIGRATIONS: &[&str] = &[
    // v1
    "
    CREATE TABLE meta (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    CREATE TABLE platforms (
        id   TEXT PRIMARY KEY,
        name TEXT
    );
    CREATE TABLE games (
        id       INTEGER PRIMARY KEY,
        platform TEXT NOT NULL REFERENCES platforms(id),
        name     TEXT NOT NULL,
        added_at INTEGER NOT NULL,
        UNIQUE(platform, name)
    );
    CREATE TABLE roms (
        id          INTEGER PRIMARY KEY,
        game_id     INTEGER NOT NULL REFERENCES games(id) ON DELETE CASCADE,
        name        TEXT NOT NULL,
        size        INTEGER NOT NULL,
        crc32       TEXT NOT NULL,
        sha1        TEXT NOT NULL,
        -- 1 when a DAT vouched for size+crc(+sha1) at import time
        verified    INTEGER NOT NULL DEFAULT 0,
        -- managed copy, relative to the library root (NULL for references)
        rel_path    TEXT,
        -- original location for reference imports (absolute)
        source_path TEXT,
        mode        TEXT NOT NULL CHECK (mode IN ('copy', 'reference', 'link')),
        added_at    INTEGER NOT NULL,
        UNIQUE(game_id, name)
    );
    CREATE INDEX roms_sha1 ON roms(sha1);
    CREATE INDEX roms_crc_size ON roms(crc32, size);
    ",
    // v2
    super::device::MIGRATION_DEVICES,
    // v3
    super::saves::MIGRATION_SAVES,
    // v4
    super::covers::MIGRATION_COVERS,
    // v5
    super::rename::MIGRATION_RENAMES,
    // v6
    "CREATE INDEX IF NOT EXISTS roms_name ON roms(name);",
];

/// Brings the schema to the current version, one migration per
/// transaction. A database from a newer pocketeer is refused rather than
/// half-understood.
pub fn migrate(conn: &mut Connection) -> Result<()> {
    conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL;")
        .map_err(|e| Error::db(e, "pragma"))?;
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(|e| Error::db(e, "user_version"))?;
    if version < 0 || version as usize > MIGRATIONS.len() {
        return Err(Error::LibraryTooNew {
            version,
            supported: MIGRATIONS.len(),
        });
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
        let next = i + 1;
        let context = || format!("migration {next}");
        let tx = conn.transaction().map_err(|e| Error::db(e, context()))?;
        tx.execute_batch(sql).map_err(|e| Error::db(e, context()))?;
        tx.pragma_update(None, "user_version", next as i64)
            .map_err(|e| Error::db(e, context()))?;
        tx.commit().map_err(|e| Error::db(e, context()))?;
    }
    Ok(())
}

/// Reads an INTEGER column holding a size or count. SQLite only has i64;
/// pocketeer never stores negatives, so a negative reads as 0.
pub(crate) fn get_u64<I: rusqlite::RowIndex>(
    r: &rusqlite::Row<'_>,
    idx: I,
) -> rusqlite::Result<u64> {
    Ok(r.get::<_, i64>(idx)?.max(0) as u64)
}

pub(crate) fn get_usize<I: rusqlite::RowIndex>(
    r: &rusqlite::Row<'_>,
    idx: I,
) -> rusqlite::Result<usize> {
    Ok(get_u64(r, idx)? as usize)
}

/// Runs a SELECT and collects every row through `f`. Any failure (prepare,
/// bind or read) is a db error with `context`.
pub(crate) fn query_rows<T>(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
    context: &str,
    f: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>> {
    let rows: rusqlite::Result<Vec<T>> =
        (|| conn.prepare_cached(sql)?.query_map(params, f)?.collect())();
    rows.map_err(|e| Error::db(e, context))
}

pub fn ensure_platform(conn: &Connection, id: &str, name: Option<&str>) -> Result<()> {
    conn.execute(
        "INSERT INTO platforms (id, name) VALUES (?1, ?2)
         ON CONFLICT(id) DO UPDATE SET name = COALESCE(excluded.name, platforms.name)",
        params![id, name],
    )
    .map_err(|e| Error::db(e, "ensure_platform"))?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Game {
    pub id: i64,
    pub platform: String,
    pub name: String,
    pub roms: Vec<Rom>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Rom {
    pub id: i64,
    pub game_id: i64,
    pub name: String,
    pub size: u64,
    pub crc32: String,
    pub sha1: String,
    pub verified: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rel_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_path: Option<PathBuf>,
    pub mode: ImportMode,
}

impl Rom {
    /// The stored crc32 (8 hex digits) as a number; `None` if the column is
    /// malformed, which only a hand-edited database can produce.
    pub fn crc32_u32(&self) -> Option<u32> {
        u32::from_str_radix(&self.crc32, 16).ok()
    }
}

/// Inserts (or finds) a game and returns its id.
pub fn upsert_game(conn: &Connection, platform: &str, name: &str) -> Result<i64> {
    conn.execute(
        "INSERT OR IGNORE INTO games (platform, name, added_at) VALUES (?1, ?2, ?3)",
        params![platform, name, fsutil::now_secs()],
    )
    .map_err(|e| Error::db(e, "insert game"))?;
    conn.query_row(
        "SELECT id FROM games WHERE platform = ?1 AND name = ?2",
        params![platform, name],
        |r| r.get(0),
    )
    .map_err(|e| Error::db(e, "select game"))
}

pub struct NewRom<'a> {
    pub game_id: i64,
    pub name: &'a str,
    pub size: u64,
    pub crc32: &'a str,
    pub sha1: &'a str,
    pub verified: bool,
    pub rel_path: Option<&'a str>,
    pub source_path: Option<&'a str>,
    pub mode: ImportMode,
}

pub fn insert_rom(conn: &Connection, rom: &NewRom<'_>) -> Result<i64> {
    conn.execute(
        "INSERT INTO roms (game_id, name, size, crc32, sha1, verified, rel_path, source_path, mode, added_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            rom.game_id,
            rom.name,
            rom.size as i64,
            rom.crc32,
            rom.sha1,
            rom.verified,
            rom.rel_path,
            rom.source_path,
            rom.mode.as_str(),
            fsutil::now_secs()
        ],
    )
    .map_err(|e| Error::db(e, "insert rom"))?;
    Ok(conn.last_insert_rowid())
}

fn rom_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Rom> {
    Ok(Rom {
        id: r.get("id")?,
        game_id: r.get("game_id")?,
        name: r.get("name")?,
        size: get_u64(r, "size")?,
        crc32: r.get("crc32")?,
        sha1: r.get("sha1")?,
        verified: r.get("verified")?,
        rel_path: r.get::<_, Option<String>>("rel_path")?.map(PathBuf::from),
        source_path: r
            .get::<_, Option<String>>("source_path")?
            .map(PathBuf::from),
        mode: {
            let raw: String = r.get("mode")?;
            ImportMode::parse(&raw).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    r.as_ref().column_index("mode").unwrap_or(0),
                    rusqlite::types::Type::Text,
                    format!("unknown import mode {raw:?}").into(),
                )
            })?
        },
    })
}

const ROM_COLS: &str =
    "id, game_id, name, size, crc32, sha1, verified, rel_path, source_path, mode";
/// [`ROM_COLS`] qualified with the `r` alias used in joins.
const ROM_COLS_R: &str = "r.id, r.game_id, r.name, r.size, r.crc32, r.sha1, r.verified, r.rel_path, r.source_path, r.mode";

/// A ROM on this platform whose `column` (`sha1` or `name`) equals `value`.
fn find_rom_by(
    conn: &Connection,
    platform: &str,
    column: &'static str,
    value: &str,
) -> Result<Option<Rom>> {
    conn.query_row(
        &format!(
            "SELECT {ROM_COLS_R} FROM roms r JOIN games g ON g.id = r.game_id
             WHERE g.platform = ?1 AND r.{column} = ?2 LIMIT 1"
        ),
        params![platform, value],
        rom_from_row,
    )
    .optional()
    .map_err(|e| Error::db(e, format!("find_rom_by_{column}")))
}

/// A ROM with identical contents (sha1), on this platform.
pub fn find_rom_by_sha1(conn: &Connection, platform: &str, sha1: &str) -> Result<Option<Rom>> {
    find_rom_by(conn, platform, "sha1", sha1)
}

/// A ROM already using this file name on the platform.
pub fn find_rom_by_name(conn: &Connection, platform: &str, name: &str) -> Result<Option<Rom>> {
    find_rom_by(conn, platform, "name", name)
}

fn game_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Game> {
    Ok(Game {
        id: r.get(0)?,
        platform: r.get(1)?,
        name: r.get(2)?,
        roms: Vec::new(),
    })
}

/// A game's ROMs, by name.
fn roms_of(conn: &Connection, game_id: i64, context: &str) -> Result<Vec<Rom>> {
    query_rows(
        conn,
        &format!("SELECT {ROM_COLS} FROM roms WHERE game_id = ?1 ORDER BY name COLLATE NOCASE"),
        params![game_id],
        context,
        rom_from_row,
    )
}

/// Games (with their ROMs), optionally filtered, sorted by platform then name.
pub fn list_games(
    conn: &Connection,
    platform: Option<&str>,
    query: Option<&str>,
) -> Result<Vec<Game>> {
    let like = query.map(|q| format!("%{}%", q.replace('%', "\\%").replace('_', "\\_")));
    let mut games = query_rows(
        conn,
        "SELECT id, platform, name FROM games
         WHERE (?1 IS NULL OR platform = ?1)
           AND (?2 IS NULL OR name LIKE ?2 ESCAPE '\\')
         ORDER BY platform, name COLLATE NOCASE",
        params![platform, like],
        "list games",
        game_from_row,
    )?;
    for game in &mut games {
        game.roms = roms_of(conn, game.id, "list roms")?;
    }
    Ok(games)
}

pub fn get_rom(conn: &Connection, id: i64) -> Result<Option<Rom>> {
    conn.query_row(
        &format!("SELECT {ROM_COLS} FROM roms WHERE id = ?1"),
        params![id],
        rom_from_row,
    )
    .optional()
    .map_err(|e| Error::db(e, "get_rom"))
}

/// A game with its ROMs.
pub fn get_game(conn: &Connection, id: i64) -> Result<Option<Game>> {
    let base = conn
        .query_row(
            "SELECT id, platform, name FROM games WHERE id = ?1",
            params![id],
            game_from_row,
        )
        .optional()
        .map_err(|e| Error::db(e, "get_game"))?;
    let Some(mut game) = base else {
        return Ok(None);
    };
    game.roms = roms_of(conn, id, "get_game roms")?;
    Ok(Some(game))
}

/// ROMs whose file name or game name equals `spec` (case-insensitive),
/// optionally on one platform. Each hit comes with its game.
pub fn find_roms_by_spec(
    conn: &Connection,
    platform: Option<&str>,
    spec: &str,
) -> Result<Vec<(Game, Rom)>> {
    query_rows(
        conn,
        &format!(
            "SELECT g.id AS g_id, g.platform AS g_platform, g.name AS g_name, {ROM_COLS_R}
             FROM roms r JOIN games g ON g.id = r.game_id
             WHERE (?1 IS NULL OR g.platform = ?1)
               AND (r.name = ?2 COLLATE NOCASE OR g.name = ?2 COLLATE NOCASE)
             ORDER BY g.platform, g.name COLLATE NOCASE, r.name COLLATE NOCASE"
        ),
        params![platform, spec],
        "find_roms_by_spec",
        |r| {
            // Aliased so `rom_from_row` (which reads by column name) sees the ROM's own id/name.
            let game = Game {
                id: r.get("g_id")?,
                platform: r.get("g_platform")?,
                name: r.get("g_name")?,
                roms: Vec::new(),
            };
            Ok((game, rom_from_row(r)?))
        },
    )
}

pub fn rename_rom(
    conn: &Connection,
    rom_id: i64,
    name: &str,
    rel_path: Option<&str>,
) -> Result<()> {
    conn.execute(
        "UPDATE roms SET name = ?2, rel_path = ?3 WHERE id = ?1",
        params![rom_id, name, rel_path],
    )
    .map_err(|e| Error::db(e, "rename rom"))?;
    Ok(())
}

pub fn rename_game(conn: &Connection, game_id: i64, name: &str) -> Result<()> {
    conn.execute(
        "UPDATE games SET name = ?2 WHERE id = ?1",
        params![game_id, name],
    )
    .map_err(|e| Error::db(e, "rename game"))?;
    Ok(())
}

/// Another game on the platform with this name, if any.
pub fn find_game_by_name(conn: &Connection, platform: &str, name: &str) -> Result<Option<i64>> {
    conn.query_row(
        "SELECT id FROM games WHERE platform = ?1 AND name = ?2",
        params![platform, name],
        |r| r.get(0),
    )
    .optional()
    .map_err(|e| Error::db(e, "find_game_by_name"))
}

/// Deletes a ROM row; its game goes too when no ROM is left (covers cascade).
pub fn delete_rom(conn: &Connection, rom_id: i64, game_id: i64) -> Result<bool> {
    conn.execute("DELETE FROM roms WHERE id = ?1", params![rom_id])
        .map_err(|e| Error::db(e, "delete rom"))?;
    let left: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM roms WHERE game_id = ?1",
            params![game_id],
            |r| r.get(0),
        )
        .map_err(|e| Error::db(e, "count roms"))?;
    if left == 0 {
        // The game's cover row goes with it (ON DELETE CASCADE).
        conn.execute("DELETE FROM games WHERE id = ?1", params![game_id])
            .map_err(|e| Error::db(e, "delete game"))?;
    }
    Ok(left == 0)
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PlatformStats {
    pub platform: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub games: usize,
    pub roms: usize,
    pub verified: usize,
    pub bytes: u64,
}

pub fn platform_stats(conn: &Connection) -> Result<Vec<PlatformStats>> {
    query_rows(
        conn,
        "SELECT p.id, p.name,
                (SELECT COUNT(*) FROM games g WHERE g.platform = p.id),
                (SELECT COUNT(*) FROM roms r JOIN games g ON g.id = r.game_id WHERE g.platform = p.id),
                (SELECT COUNT(*) FROM roms r JOIN games g ON g.id = r.game_id WHERE g.platform = p.id AND r.verified = 1),
                (SELECT COALESCE(SUM(r.size), 0) FROM roms r JOIN games g ON g.id = r.game_id WHERE g.platform = p.id)
         FROM platforms p ORDER BY p.id",
        [],
        "platform_stats",
        |r| {
            Ok(PlatformStats {
                platform: r.get(0)?,
                name: r.get(1)?,
                games: get_usize(r, 2)?,
                roms: get_usize(r, 3)?,
                verified: get_usize(r, 4)?,
                bytes: get_u64(r, 5)?,
            })
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        migrate(&mut c).unwrap();
        c
    }

    fn user_version(c: &Connection) -> i64 {
        c.query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn migrations_are_idempotent() {
        let mut c = conn();
        migrate(&mut c).unwrap();
        assert_eq!(user_version(&c) as usize, MIGRATIONS.len());
        assert_eq!(ROM_COLS_R, format!("r.{}", ROM_COLS.replace(", ", ", r.")));
    }

    #[test]
    fn newer_schema_is_refused() {
        let mut c = conn();
        c.pragma_update(None, "user_version", MIGRATIONS.len() as i64 + 1)
            .unwrap();
        let err = migrate(&mut c).unwrap_err();
        assert_eq!(err.code(), "LIBRARY_TOO_NEW");
    }

    /// Released migrations are frozen: a schema change is a *new* entry in
    /// `MIGRATIONS`, never an edit to an old one (libraries in the wild have
    /// already run them). Editing one changes its checksum and fails here;
    /// appending one means adding its checksum to the list.
    #[test]
    fn released_migrations_are_frozen() {
        const PINNED: [u32; 6] = [
            0xb785_fcea,
            0x7e87_79a0,
            0x5aef_96de,
            0x8559_3bf3,
            0x2d5b_2239,
            0xc69f_adbb,
        ];
        let actual: Vec<u32> = MIGRATIONS
            .iter()
            .map(|sql| {
                let normalized: String = sql.split_whitespace().collect::<Vec<_>>().join(" ");
                crc32fast::hash(normalized.as_bytes())
            })
            .collect();
        let formatted: Vec<String> = actual.iter().map(|c| format!("{c:#010x}")).collect();
        assert_eq!(
            actual.len(),
            PINNED.len(),
            "new migration added; pin it: {formatted:?}"
        );
        for (i, (a, p)) in actual.iter().zip(PINNED).enumerate() {
            assert_eq!(
                *a,
                p,
                "migration v{} was edited; add a new one instead ({formatted:?})",
                i + 1
            );
        }
    }

    #[test]
    fn games_and_roms_roundtrip() {
        let c = conn();
        ensure_platform(&c, "gba", None).unwrap();
        ensure_platform(&c, "gba", Some("Game Boy Advance")).unwrap();
        let g = upsert_game(&c, "gba", "Hello (USA)").unwrap();
        assert_eq!(
            upsert_game(&c, "gba", "Hello (USA)").unwrap(),
            g,
            "same game, same id"
        );
        insert_rom(
            &c,
            &NewRom {
                game_id: g,
                name: "Hello (USA).gba",
                size: 11,
                crc32: "0d4a1185",
                sha1: "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed",
                verified: true,
                rel_path: Some("roms/gba/Hello (USA).gba"),
                source_path: None,
                mode: ImportMode::Copy,
            },
        )
        .unwrap();
        assert!(
            find_rom_by_sha1(&c, "gba", "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed")
                .unwrap()
                .is_some()
        );
        assert!(
            find_rom_by_sha1(&c, "gb", "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed")
                .unwrap()
                .is_none()
        );
        assert!(
            find_rom_by_name(&c, "gba", "Hello (USA).gba")
                .unwrap()
                .is_some()
        );

        let games = list_games(&c, None, None).unwrap();
        assert_eq!(games.len(), 1);
        assert!(games[0].roms[0].verified);
        assert_eq!(list_games(&c, Some("gb"), None).unwrap().len(), 0);
        assert_eq!(list_games(&c, None, Some("hello")).unwrap().len(), 1);
        assert_eq!(
            list_games(&c, None, Some("100%")).unwrap().len(),
            0,
            "wildcards escaped"
        );

        let stats = platform_stats(&c).unwrap();
        assert_eq!(stats[0].name.as_deref(), Some("Game Boy Advance"));
        assert_eq!(
            (
                stats[0].games,
                stats[0].roms,
                stats[0].verified,
                stats[0].bytes
            ),
            (1, 1, 1, 11)
        );
    }
}
