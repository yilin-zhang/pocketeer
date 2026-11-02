//! Box art for library games: the records, automatic downloads from
//! libretro-thumbnails (with protection against stale results), and the
//! copied local artwork in [`import`].

pub mod import;

pub use super::db::{CoverRecord, CoverStats, CoverStatus};

use crate::library::{Library, db};
use crate::{
    Error, Result,
    catalog::{libretro, thumbnail_system as system_for},
    net::Http,
    plan::{Event, Progress},
};
use rusqlite::Connection;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Absolute path of a game's cover if it was fetched.
pub fn path_for(lib: &Library, game_id: i64) -> Result<Option<PathBuf>> {
    Ok(db::cover_record(lib.conn(), game_id)?
        .filter(|r| r.status == CoverStatus::Ok)
        .and_then(|r| r.rel_path)
        .map(|rel| lib.root().join(rel))
        .filter(|p| p.is_file()))
}

pub fn stats(lib: &Library) -> Result<Vec<CoverStats>> {
    db::cover_stats(lib.conn())
}

/// Use a catalog title only when every ROM in this game agrees. Local file
/// and game titles remain user-controlled and are never renamed here.
pub fn cover_name(game: &db::Game) -> &str {
    let Some(first) = game.roms.first().and_then(|r| r.dat.game_name.as_deref()) else {
        return &game.name;
    };
    if game.roms.iter().all(|r| {
        r.dat.status == crate::library::identify::DatStatus::Matched
            && r.dat.game_name.as_deref() == Some(first)
    }) {
        first
    } else {
        &game.name
    }
}

// ---------------------------------------------------------------------------
// Downloads

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
        .is_none_or(|g| cover_name(g) != cover_name(game))
    {
        return Ok(false);
    }
    let changed = db::upsert_cover(&tx, game.id, rel, status)?;
    tx.commit()
        .map_err(|e| Error::db(e, "commit cover result"))?;
    Ok(changed)
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
    /// Only these platforms (all when `None`).
    pub platforms: Option<Vec<String>>,
    /// Retry games previously marked `none`.
    pub retry_missing: bool,
    /// Override the libretro system name (for platforms not in [`crate::catalog::SYSTEMS`]).
    pub system: Option<String>,
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
        Err(Error::Http { status: 404, .. }) => (CoverStatus::NotFound, None),
        Err(e) => (CoverStatus::Error, Some(e.to_string())),
    }
}

/// Downloads covers for library games that don't have one yet, from
/// `base` ([`libretro::THUMBNAILS_BASE`] or a mirror).
pub fn fetch(
    lib: &Library,
    http: &Http,
    opts: &FetchOptions,
    base: &str,
    progress: Progress<'_>,
) -> Result<FetchReport> {
    let games: Vec<_> = db::list_games(lib.conn(), None, None)?
        .into_iter()
        .filter(|g| {
            opts.platforms
                .as_ref()
                .is_none_or(|ps| ps.contains(&g.platform))
        })
        .collect();
    let mut report = FetchReport::default();
    let todo: Vec<_> = games
        .iter()
        .filter(
            |g| match db::cover_record(lib.conn(), g.id).ok().flatten() {
                Some(r) if r.status == CoverStatus::Ok => {
                    report.summary.already += 1;
                    false
                }
                Some(r) if r.status == CoverStatus::NotFound && !opts.retry_missing => {
                    report.summary.not_found += 1;
                    false
                }
                _ => true,
            },
        )
        .collect();
    progress(Event::Status(format!("Fetching {} cover(s)", todo.len())));

    // Games whose platform has no libretro mapping are reported, not fetched.
    let mut jobs: Vec<(usize, String, PathBuf)> = Vec::new();
    for (i, g) in todo.iter().enumerate() {
        match opts.system.as_deref().or_else(|| system_for(&g.platform)) {
            Some(system) => {
                let url = libretro::thumbnail_url(base, system, cover_name(g));
                let rel = PathBuf::from("covers")
                    .join(&g.platform)
                    .join(libretro::thumbnail_name(cover_name(g)));
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
                    CoverStatus::NotFound => report.summary.not_found += 1,
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

#[cfg(test)]
mod tests {
    use super::*;

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
        fetch(&lib, &Http::new(None).unwrap(), &FetchOptions { system: Some("Sys".into()), ..Default::default() }, &server.url(), &mut |event| {
            if !changed && matches!(event, Event::Status(_)) {
                lib.conn().execute("UPDATE roms SET dat_status='matched',dat_game_name='Identified title' WHERE game_id=?1", [game.id]).unwrap();
                changed = true;
            }
        }).unwrap();
        old.assert();
        assert!(db::cover_record(lib.conn(), game.id).unwrap().is_none());
        let new = server
            .mock("GET", "/Sys/master/Named_Boxarts/Identified%20title.png")
            .with_body(b"correct art")
            .create();
        fetch(
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
        let report = fetch(
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
        assert!(
            db::cover_record(lib.conn(), game.id)
                .unwrap()
                .unwrap()
                .imported
        );
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
            platforms: Some(vec!["gba".into()]),
            retry_missing: false,
            system: Some("Sys".into()),
        };
        let report = fetch(&lib, &http, &opts, &base, &mut |_| {}).unwrap();
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
        let report = fetch(&lib, &http, &opts, &base, &mut |_| {}).unwrap();
        assert!(report.items.is_empty());
        assert_eq!(report.summary.already, 1);
        assert_eq!(report.summary.not_found, 1);

        // Unsupported platform without an override is reported, not fatal.
        let all = FetchOptions::default();
        let report = fetch(&lib, &http, &all, &base, &mut |_| {}).unwrap();
        assert_eq!(report.unsupported_platforms, ["weird"]);
        let st = stats(&lib).unwrap();
        assert_eq!(st[0].with_cover, 1);
        assert!(!st[1].supported);
    }
}
