//! Box art from the libretro-thumbnails project, matched by DAT game name.
//!
//! libretro names its folders after No-Intro system names and its files after
//! No-Intro game names (with a few characters replaced), so a library imported
//! with a DAT can fetch covers without any guessing.

pub mod import;

use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};
use serde::Serialize;

use crate::{
    Error, Result, fsutil,
    net::Http,
    progress::{Event, Progress},
};

use super::{Library, db};

pub(crate) const MIGRATION_COVERS: &str = "
    CREATE TABLE covers (
        game_id    INTEGER PRIMARY KEY REFERENCES games(id) ON DELETE CASCADE,
        rel_path   TEXT,
        status     TEXT NOT NULL CHECK (status IN ('ok', 'none', 'error')),
        fetched_at INTEGER NOT NULL
    );
";

const BASE: &str = "https://raw.githubusercontent.com/libretro-thumbnails";

/// Pocket platform id → libretro-thumbnails repository name.
pub const SYSTEMS: &[(&str, &str)] = &[
    ("2600", "Atari - 2600"),
    ("5200", "Atari - 5200"),
    ("7800", "Atari - 7800"),
    ("lynx", "Atari - Lynx"),
    ("arduboy", "Arduboy Inc - Arduboy"),
    ("coleco", "Coleco - ColecoVision"),
    ("colecovision", "Coleco - ColecoVision"),
    ("intv", "Mattel - Intellivision"),
    ("intellivision", "Mattel - Intellivision"),
    ("gb", "Nintendo - Game Boy"),
    ("gbc", "Nintendo - Game Boy Color"),
    ("gba", "Nintendo - Game Boy Advance"),
    ("nes", "Nintendo - Nintendo Entertainment System"),
    ("snes", "Nintendo - Super Nintendo Entertainment System"),
    ("sgb", "Nintendo - Game Boy"),
    ("poke_mini", "Nintendo - Pokemon Mini"),
    ("pokemini", "Nintendo - Pokemon Mini"),
    ("vb", "Nintendo - Virtual Boy"),
    ("pce", "NEC - PC Engine - TurboGrafx 16"),
    ("pcecd", "NEC - PC Engine CD - TurboGrafx-CD"),
    ("sg1000", "Sega - SG-1000"),
    ("sms", "Sega - Master System - Mark III"),
    ("gg", "Sega - Game Gear"),
    ("genesis", "Sega - Mega Drive - Genesis"),
    ("md", "Sega - Mega Drive - Genesis"),
    ("32x", "Sega - 32X"),
    ("ng", "SNK - Neo Geo"),
    ("ngp", "SNK - Neo Geo Pocket"),
    ("ngpc", "SNK - Neo Geo Pocket Color"),
    ("wonderswan", "Bandai - WonderSwan"),
    ("wsc", "Bandai - WonderSwan Color"),
    ("supervision", "Watara - Supervision"),
    ("gamate", "Bit Corporation - Gamate"),
    ("gameandwatch", "Handheld Electronic Game"),
    ("psx", "Sony - PlayStation"),
    ("amiga", "Commodore - Amiga"),
    ("c64", "Commodore - 64"),
    ("vectrex", "GCE - Vectrex"),
    ("odyssey2", "Magnavox - Odyssey2"),
    ("channel_f", "Fairchild - Channel F"),
];

pub fn system_for(platform: &str) -> Option<&'static str> {
    SYSTEMS
        .iter()
        .find(|(id, _)| id.eq_ignore_ascii_case(platform))
        .map(|(_, s)| *s)
}

/// libretro replaces characters that are awkward in file names with `_`.
pub fn thumbnail_name(game: &str) -> String {
    game.chars()
        .map(|c| match c {
            '&' | '*' | '/' | ':' | '`' | '<' | '>' | '?' | '\\' | '|' | '"' => '_',
            c => c,
        })
        .collect::<String>()
        + ".png"
}

fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `https://raw.githubusercontent.com/libretro-thumbnails/<repo>/master/Named_Boxarts/<name>.png`.
/// The GitHub repositories are named with underscores (`Nintendo_-_Game_Boy_Advance`),
/// unlike the system names libretro shows elsewhere.
pub fn url_with_base(base: &str, system: &str, game: &str) -> String {
    format!(
        "{}/{}/master/Named_Boxarts/{}",
        base.trim_end_matches('/'),
        encode(&system.replace(' ', "_")),
        encode(&thumbnail_name(game))
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverStatus {
    /// Downloaded and on disk.
    Ok,
    /// libretro has no cover for this name (404); not retried unless asked.
    None,
    /// Network or disk error; retried next time.
    Error,
}

impl CoverStatus {
    fn as_str(self) -> &'static str {
        match self {
            CoverStatus::Ok => "ok",
            CoverStatus::None => "none",
            CoverStatus::Error => "error",
        }
    }
    fn parse(s: &str) -> CoverStatus {
        match s {
            "ok" => CoverStatus::Ok,
            "none" => CoverStatus::None,
            _ => CoverStatus::Error,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CoverRecord {
    pub game_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rel_path: Option<PathBuf>,
    pub status: CoverStatus,
    pub fetched_at: i64,
    pub imported: bool,
}

pub fn record(conn: &Connection, game_id: i64) -> Result<Option<CoverRecord>> {
    use rusqlite::OptionalExtension;
    conn.query_row(
        "SELECT game_id, rel_path, status, fetched_at, imported FROM covers WHERE game_id = ?1",
        params![game_id],
        |r| {
            Ok(CoverRecord {
                game_id: r.get(0)?,
                rel_path: r.get::<_, Option<String>>(1)?.map(PathBuf::from),
                status: CoverStatus::parse(&r.get::<_, String>(2)?),
                fetched_at: r.get(3)?,
                imported: r.get(4)?,
            })
        },
    )
    .optional()
    .map_err(|e| Error::db(e, "cover record"))
}

fn upsert(
    conn: &Connection,
    game_id: i64,
    rel: Option<&Path>,
    status: CoverStatus,
) -> Result<bool> {
    let changed = conn
        .execute(
            "INSERT INTO covers (game_id, rel_path, status, fetched_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(game_id) DO UPDATE SET rel_path = excluded.rel_path,
            status = excluded.status, fetched_at = excluded.fetched_at
         WHERE covers.imported = 0",
            params![
                game_id,
                rel.map(|p| p.to_string_lossy().into_owned()),
                status.as_str(),
                fsutil::now_secs()
            ],
        )
        .map_err(|e| Error::db(e, "upsert cover"))?;
    Ok(changed > 0)
}

// Check the title and record the download in the same transaction: a DAT
// result or rename arriving during HTTP must not attach the old title's art.
fn record_download(
    conn: &Connection,
    game: &db::Game,
    rel: Option<&Path>,
    status: CoverStatus,
) -> Result<bool> {
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| Error::db(e, "begin cover result"))?;
    let current = db::get_game(&tx, game.id)?;
    if current
        .as_ref()
        .is_none_or(|g| super::identify::cover_name(g) != super::identify::cover_name(game))
    {
        return Ok(false);
    }
    let changed = upsert(&tx, game.id, rel, status)?;
    tx.commit()
        .map_err(|e| Error::db(e, "commit cover result"))?;
    Ok(changed)
}

/// Forgets a `none`/`error` record so the next fetch tries again (after a
/// game was renamed, say). A fetched cover is kept.
pub fn reset_unfound(conn: &Connection, game_id: i64) -> Result<()> {
    conn.execute(
        "DELETE FROM covers WHERE game_id = ?1 AND status != 'ok'",
        params![game_id],
    )
    .map_err(|e| Error::db(e, "reset cover"))?;
    Ok(())
}

/// Absolute path of a game's cover if it was fetched.
pub fn path_for(lib: &Library, game_id: i64) -> Result<Option<PathBuf>> {
    Ok(record(lib.conn(), game_id)?
        .filter(|r| r.status == CoverStatus::Ok)
        .and_then(|r| r.rel_path)
        .map(|rel| lib.root().join(rel))
        .filter(|p| p.is_file()))
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct FetchSummary {
    pub fetched: usize,
    pub already: usize,
    pub not_found: usize,
    pub errors: usize,
    pub unsupported_platform: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FetchItem {
    pub platform: String,
    pub game: String,
    pub status: CoverStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct FetchReport {
    pub summary: FetchSummary,
    /// Only games that were actually attempted this run.
    pub items: Vec<FetchItem>,
    pub unsupported_platforms: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct FetchOptions {
    pub platform: Option<String>,
    /// Retry games previously marked `none`.
    pub retry_missing: bool,
    /// Override the libretro system name (for platforms not in [`SYSTEMS`]).
    pub system: Option<String>,
}

/// Downloads covers for library games that don't have one yet.
pub fn fetch(
    lib: &Library,
    http: &Http,
    opts: &FetchOptions,
    progress: Progress<'_>,
) -> Result<FetchReport> {
    fetch_with_base(lib, http, opts, BASE, progress)
}

/// Concurrent requests while fetching covers.
const PARALLEL: usize = 6;

/// One cover: GET it and write it atomically. 404 means libretro has none.
fn fetch_one(http: &Http, url: &str, full: &Path) -> (CoverStatus, Option<String>) {
    match http.get_conditional(url, None) {
        Ok(crate::net::Conditional::Fresh { bytes, .. }) => {
            match crate::fsutil::write_atomic(full, &bytes) {
                Ok(()) => (CoverStatus::Ok, None),
                Err(e) => (CoverStatus::Error, Some(e.to_string())),
            }
        }
        Ok(crate::net::Conditional::NotModified) => {
            (CoverStatus::Error, Some("unexpected 304".into()))
        }
        Err(Error::Http { status: 404, .. }) => (CoverStatus::None, None),
        Err(e) => (CoverStatus::Error, Some(e.to_string())),
    }
}

/// Like [`fetch`] with a different host (tests, mirrors).
pub fn fetch_with_base(
    lib: &Library,
    http: &Http,
    opts: &FetchOptions,
    base: &str,
    progress: Progress<'_>,
) -> Result<FetchReport> {
    let games = db::list_games(lib.conn(), opts.platform.as_deref(), None)?;
    let mut report = FetchReport::default();
    let todo: Vec<_> = games
        .iter()
        .filter(|g| match record(lib.conn(), g.id).ok().flatten() {
            Some(r) if r.status == CoverStatus::Ok => {
                report.summary.already += 1;
                false
            }
            Some(r) if r.status == CoverStatus::None && !opts.retry_missing => {
                report.summary.not_found += 1;
                false
            }
            _ => true,
        })
        .collect();
    progress(Event::Status(format!("Fetching {} cover(s)", todo.len())));

    // Games whose platform has no libretro mapping are reported, not fetched.
    let mut jobs: Vec<(usize, String, PathBuf)> = Vec::new();
    for (i, g) in todo.iter().enumerate() {
        match opts.system.as_deref().or_else(|| system_for(&g.platform)) {
            Some(system) => {
                let url = url_with_base(base, system, super::identify::cover_name(g));
                let rel = PathBuf::from("covers")
                    .join(&g.platform)
                    .join(thumbnail_name(super::identify::cover_name(g)));
                jobs.push((i, url, rel));
            }
            None => {
                report.summary.unsupported_platform += 1;
                if !report.unsupported_platforms.contains(&g.platform) {
                    report.unsupported_platforms.push(g.platform.clone());
                }
            }
        }
    }

    // A few requests in flight at once: one small PNG per game, and most of
    // the time goes to latency. Results come back on a channel so the
    // database and the progress callback stay on this thread.
    let total = jobs.len();
    let root = lib.root().to_path_buf();
    let next = std::sync::Mutex::new(0usize);
    let (tx, rx) = std::sync::mpsc::channel::<(usize, CoverStatus, Option<String>)>();
    std::thread::scope(|scope| {
        for _ in 0..PARALLEL.min(total.max(1)) {
            let (tx, next, jobs, root) = (tx.clone(), &next, &jobs, &root);
            scope.spawn(move || {
                loop {
                    let idx = {
                        let mut n = next.lock().unwrap_or_else(|e| e.into_inner());
                        let i = *n;
                        *n += 1;
                        i
                    };
                    let Some((game_idx, url, rel)) = jobs.get(idx) else {
                        break;
                    };
                    let outcome = fetch_one(http, url, &root.join(rel));
                    if tx.send((*game_idx, outcome.0, outcome.1)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);

        let mut done = 0;
        for (game_idx, status, message) in rx {
            let g = &todo[game_idx];
            done += 1;
            progress(Event::Step { done, total });
            progress(Event::Status(format!(
                "Fetching covers {done}/{total}  {}",
                g.name
            )));
            let rel = jobs
                .iter()
                .find(|(i, _, _)| *i == game_idx)
                .map(|(_, _, rel)| rel.clone())
                .unwrap_or_default();
            match record_download(
                lib.conn(),
                g,
                (status == CoverStatus::Ok).then_some(rel.as_path()),
                status,
            ) {
                Ok(false) => {
                    report.summary.already += 1;
                    continue;
                }
                Ok(true) => match status {
                    CoverStatus::Ok => report.summary.fetched += 1,
                    CoverStatus::None => report.summary.not_found += 1,
                    CoverStatus::Error => report.summary.errors += 1,
                },
                Err(e) => {
                    report.summary.errors += 1;
                    report.items.push(FetchItem {
                        platform: g.platform.clone(),
                        game: g.name.clone(),
                        status: CoverStatus::Error,
                        message: Some(e.to_string()),
                    });
                    continue;
                }
            }
            report.items.push(FetchItem {
                platform: g.platform.clone(),
                game: g.name.clone(),
                status,
                message,
            });
        }
    });
    report
        .items
        .sort_by(|a, b| (&a.platform, &a.game).cmp(&(&b.platform, &b.game)));
    Ok(report)
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct CoverStats {
    pub platform: String,
    pub games: usize,
    pub with_cover: usize,
    pub not_found: usize,
    pub supported: bool,
}

pub fn stats(lib: &Library) -> Result<Vec<CoverStats>> {
    db::query_rows(
        lib.conn(),
        "SELECT g.platform,
                COUNT(*),
                SUM(CASE WHEN c.status = 'ok' THEN 1 ELSE 0 END),
                SUM(CASE WHEN c.status = 'none' THEN 1 ELSE 0 END)
         FROM games g LEFT JOIN covers c ON c.game_id = g.id
         GROUP BY g.platform ORDER BY g.platform",
        [],
        "cover stats",
        |r| {
            let platform: String = r.get(0)?;
            Ok(CoverStats {
                supported: system_for(&platform).is_some(),
                platform,
                games: db::get_usize(r, 1)?,
                with_cover: db::get_usize(r, 2)?,
                not_found: db::get_usize(r, 3)?,
            })
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_urls_follow_libretro_conventions() {
        assert_eq!(thumbnail_name("Tom & Jerry (USA)"), "Tom _ Jerry (USA).png");
        assert_eq!(
            thumbnail_name("Legend of Zelda, The (USA)"),
            "Legend of Zelda, The (USA).png"
        );
        assert_eq!(
            url_with_base(BASE, "Nintendo - Game Boy Advance", "Metroid Fusion (USA)"),
            "https://raw.githubusercontent.com/libretro-thumbnails/Nintendo_-_Game_Boy_Advance/master/Named_Boxarts/Metroid%20Fusion%20%28USA%29.png"
        );
        assert_eq!(system_for("GBA"), Some("Nintendo - Game Boy Advance"));
        assert_eq!(system_for("nope"), None);
    }

    #[test]
    fn stale_download_cannot_replace_a_new_catalog_title() {
        let (_tmp, lib) = crate::testutil::library_with(&[("game.gba", "ROM")]);
        let game = lib.list_games(None, None).unwrap().remove(0);
        let mut server = mockito::Server::new();
        let old = server
            .mock("GET", "/Sys/master/Named_Boxarts/game.png")
            .with_body(b"old art")
            .create();
        let mut changed = false;
        fetch_with_base(&lib, &Http::new(None).unwrap(), &FetchOptions { system: Some("Sys".into()), ..Default::default() }, &server.url(), &mut |event| {
            if !changed && matches!(event, Event::Status(_)) {
                lib.conn().execute("UPDATE roms SET dat_status='matched',dat_game_name='Identified title' WHERE game_id=?1", [game.id]).unwrap();
                changed = true;
            }
        }).unwrap();
        old.assert();
        assert!(record(lib.conn(), game.id).unwrap().is_none());
        let new = server
            .mock("GET", "/Sys/master/Named_Boxarts/Identified%20title.png")
            .with_body(b"correct art")
            .create();
        fetch_with_base(
            &lib,
            &Http::new(None).unwrap(),
            &FetchOptions {
                system: Some("Sys".into()),
                ..Default::default()
            },
            &server.url(),
            &mut |_| {},
        )
        .unwrap();
        new.assert();
        assert_eq!(
            std::fs::read(path_for(&lib, game.id).unwrap().unwrap()).unwrap(),
            b"correct art"
        );
    }

    #[test]
    fn imported_cover_wins_even_when_a_download_was_already_planned() {
        let (tmp, lib) = crate::testutil::library_with(&[("game.gba", "ROM")]);
        let game = lib.list_games(None, None).unwrap().remove(0);
        let source = tmp.path().join("manual.png");
        image::RgbImage::from_pixel(2, 2, image::Rgb([10, 20, 30]))
            .save(&source)
            .unwrap();
        let mut server = mockito::Server::new();
        let download = server
            .mock("GET", "/Sys/master/Named_Boxarts/game.png")
            .with_body(b"downloaded art")
            .expect(1)
            .create();
        let mut imported = false;
        let report = fetch_with_base(
            &lib,
            &Http::new(None).unwrap(),
            &FetchOptions {
                system: Some("Sys".into()),
                ..Default::default()
            },
            &server.url(),
            &mut |event| {
                if !imported && matches!(event, Event::Status(_)) {
                    let mut other = Library::open(lib.root()).unwrap();
                    let plan = import::plan(&other, game.id, &source).unwrap();
                    import::apply(&mut other, &plan).unwrap();
                    imported = true;
                }
            },
        )
        .unwrap();
        download.assert();
        assert_eq!(report.summary.already, 1);
        let kept = path_for(&lib, game.id).unwrap().unwrap();
        assert_eq!(std::fs::read(kept).unwrap(), std::fs::read(source).unwrap());
        assert!(record(lib.conn(), game.id).unwrap().unwrap().imported);
    }

    #[test]
    fn fetch_records_ok_none_and_skips_done() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::init(&tmp.path().join("lib")).unwrap();
        db::ensure_platform(lib.conn(), "gba", None).unwrap();
        let hit = db::upsert_game(lib.conn(), "gba", "Hit (USA)").unwrap();
        let miss = db::upsert_game(lib.conn(), "gba", "Miss (USA)").unwrap();
        db::ensure_platform(lib.conn(), "weird", None).unwrap();
        db::upsert_game(lib.conn(), "weird", "X").unwrap();

        let mut server = mockito::Server::new();
        let base = server.url();
        let ok = server
            .mock("GET", "/Sys/master/Named_Boxarts/Hit%20%28USA%29.png")
            .with_body(b"PNG!")
            .create();
        let nf = server
            .mock("GET", "/Sys/master/Named_Boxarts/Miss%20%28USA%29.png")
            .with_status(404)
            .create();

        // Point the fetcher at the mock by overriding the system *and* base.
        let http = Http::new(None).unwrap();
        let opts = FetchOptions {
            platform: Some("gba".into()),
            retry_missing: false,
            system: Some("Sys".into()),
        };
        let report = fetch_with_base(&lib, &http, &opts, &base, &mut |_| {}).unwrap();
        ok.assert();
        nf.assert();
        assert_eq!(report.summary.fetched, 1);
        assert_eq!(report.summary.not_found, 1);
        assert_eq!(
            std::fs::read(lib.root().join("covers/gba/Hit (USA).png")).unwrap(),
            b"PNG!"
        );
        assert!(path_for(&lib, hit).unwrap().is_some());
        assert!(path_for(&lib, miss).unwrap().is_none());

        // Second run: nothing attempted (ok kept, none not retried).
        let report = fetch_with_base(&lib, &http, &opts, &base, &mut |_| {}).unwrap();
        assert!(report.items.is_empty());
        assert_eq!(report.summary.already, 1);
        assert_eq!(report.summary.not_found, 1);

        // Unsupported platform without an override is reported, not fatal.
        let all = FetchOptions::default();
        let report = fetch_with_base(&lib, &http, &all, &base, &mut |_| {}).unwrap();
        assert_eq!(report.unsupported_platforms, ["weird"]);
        let st = stats(&lib).unwrap();
        assert_eq!(st[0].with_cover, 1);
        assert!(!st[1].supported);
    }
}
