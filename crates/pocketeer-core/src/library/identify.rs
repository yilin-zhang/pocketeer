//! Content identification against cached libretro DATs. A negative result is
//! valid only for that catalog revision; transport/parse failures stay retryable.
use super::{Library, covers, db};
use crate::{
    Error, Result, fsutil,
    net::{Cache, Http, Policy},
    progress::{Event, Progress},
    roms::Dat,
};
use rusqlite::params;
use serde::Serialize;
use sha1::{Digest, Sha1};
use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    time::Duration,
};

pub const BASE: &str =
    "https://raw.githubusercontent.com/libretro/libretro-database/master/metadat/no-intro";
pub const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    #[default]
    Unchecked,
    Matched,
    NoMatch,
    Error,
    Unsupported,
    Ambiguous,
}
impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unchecked => "unchecked",
            Self::Matched => "matched",
            Self::NoMatch => "no_match",
            Self::Error => "error",
            Self::Unsupported => "unsupported",
            Self::Ambiguous => "ambiguous",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Unchecked => "not checked",
            Self::Matched => "matched",
            Self::NoMatch => "no match in DAT",
            Self::Error => "failed; will retry",
            Self::Unsupported => "no automatic DAT source",
            Self::Ambiguous => "multiple DAT matches",
        }
    }
    pub(crate) fn parse(value: &str) -> rusqlite::Result<Self> {
        match value {
            "unchecked" => Ok(Self::Unchecked),
            "matched" => Ok(Self::Matched),
            "no_match" => Ok(Self::NoMatch),
            "error" => Ok(Self::Error),
            "unsupported" => Ok(Self::Unsupported),
            "ambiguous" => Ok(Self::Ambiguous),
            _ => Err(rusqlite::Error::InvalidQuery),
        }
    }
}
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Identification {
    pub status: Status,
    pub game_name: Option<String>,
    pub rom_name: Option<String>,
    pub revision: Option<String>,
    pub source: Option<String>,
    pub checked_at: Option<i64>,
    pub error: Option<String>,
}

/// Only mapped cartridge systems with an upstream No-Intro DAT. Other
/// platforms are explicitly unsupported, not negative ROM matches.
pub fn system_for(platform: &str) -> Option<&'static str> {
    match platform.to_ascii_lowercase().as_str() {
        "pcecd" | "psx" | "ng" | "gamate" | "gameandwatch" => None,
        _ => covers::system_for(platform),
    }
}
pub fn url(base: &str, system: &str) -> String {
    format!(
        "{}/{}.dat",
        base.trim_end_matches('/'),
        system.replace(' ', "%20")
    )
}
pub fn cache_key(system: &str) -> String {
    format!("dat-{:x}.dat", Sha1::digest(system.as_bytes()))
}

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub matched: usize,
    pub no_match: usize,
    pub ambiguous: usize,
    pub unsupported: usize,
    pub unchanged: usize,
    pub errors: Vec<String>,
}
impl Report {
    pub fn summary(&self) -> String {
        format!(
            "DAT: {} matched, {} no match, {} ambiguous, {} unsupported, {} unchanged, {} errors",
            self.matched,
            self.no_match,
            self.ambiguous,
            self.unsupported,
            self.unchanged,
            self.errors.len()
        )
    }
}

pub struct Options<'a> {
    pub policy: Policy,
    pub base: &'a str,
    pub platform: Option<&'a str>,
}

pub fn run(
    lib: &mut Library,
    http: &Http,
    cache: &Cache,
    opts: Options<'_>,
    progress: Progress<'_>,
) -> Result<Report> {
    let Options {
        policy,
        base,
        platform,
    } = opts;
    let mut platforms = BTreeMap::<String, Vec<db::Rom>>::new();
    for game in lib.list_games(platform, None)? {
        platforms
            .entry(game.platform)
            .or_default()
            .extend(game.roms);
    }
    let mut report = Report::default();
    for (platform, roms) in platforms {
        progress(Event::Status(format!(
            "Identifying {platform} ROMs against DAT"
        )));
        let Some(system) = system_for(&platform) else {
            for rom in &roms {
                if rom.dat.status == Status::Matched {
                    report.unchanged += 1;
                    continue;
                }
                if rom.dat.status == Status::Unsupported {
                    report.unsupported += 1;
                    continue;
                }
                save(
                    lib,
                    rom,
                    &Identification {
                        status: Status::Unsupported,
                        checked_at: Some(fsutil::now_secs()),
                        ..Default::default()
                    },
                )?;
                report.unsupported += 1;
            }
            continue;
        };
        let source = url(base, system);
        let loaded = cache
            .fetch(http, &cache_key(system), &source, "ROM DAT", policy)
            .and_then(|f| {
                let dat = Dat::parse(&f.bytes, Path::new(&source))?;
                // A parsed metadata-only/empty file is not evidence of no match.
                if !dat
                    .games
                    .iter()
                    .flat_map(|g| &g.roms)
                    .any(|r| r.size.is_some() && r.sha1.is_some())
                {
                    return Err(Error::DatParse {
                        path: source.clone().into(),
                        message: "DAT has no size + SHA-1 entries".into(),
                    });
                }
                Ok((dat, format!("{:x}", Sha1::digest(&f.bytes)), f.source))
            });
        let (dat, revision, fetched_source) = match loaded {
            Ok(data) => data,
            Err(e) => {
                let error = e.to_string();
                for rom in &roms {
                    let mut state = rom.dat.clone();
                    // Never discard an established result because the server is down.
                    if matches!(
                        state.status,
                        Status::Unchecked | Status::Error | Status::Unsupported
                    ) {
                        state.status = Status::Error;
                    }
                    state.error = Some(error.clone());
                    save(lib, rom, &state)?;
                }
                report.errors.push(format!("{platform}: {error}"));
                continue;
            }
        };
        let fallback = match fetched_source {
            crate::net::Source::Cache {
                fallback_reason, ..
            } => fallback_reason,
            _ => None,
        };
        if let Some(error) = &fallback {
            report
                .errors
                .push(format!("{platform}: using cached DAT: {error}"));
        }
        let mut index = HashMap::<(u64, String), Vec<(&str, &str)>>::new();
        for game in &dat.games {
            for rom in &game.roms {
                if let (Some(size), Some(sha1)) = (rom.size, &rom.sha1) {
                    let names = index.entry((size, sha1.clone())).or_default();
                    let name = (game.name.as_str(), rom.name.as_str());
                    if !names.contains(&name) {
                        names.push(name);
                    }
                }
            }
        }
        for rom in &roms {
            if rom.dat.revision.as_deref() == Some(&revision)
                && rom.dat.source.as_deref() == Some(&source)
            {
                let mut state = rom.dat.clone();
                if state.error != fallback {
                    state.error = fallback.clone();
                    save(lib, rom, &state)?;
                }
                report.unchanged += 1;
                continue;
            }
            let matches = index.get(&(rom.size, rom.sha1.to_ascii_lowercase()));
            let mut state = Identification {
                revision: Some(revision.clone()),
                source: Some(source.clone()),
                checked_at: Some(fsutil::now_secs()),
                error: fallback.clone(),
                ..Default::default()
            };
            match matches.map(Vec::as_slice).unwrap_or_default() {
                [(game, name)] => {
                    state.status = Status::Matched;
                    state.game_name = Some((*game).into());
                    state.rom_name = Some((*name).into());
                }
                [] => {
                    state.status = Status::NoMatch;
                }
                _ => {
                    state.status = Status::Ambiguous;
                }
            }
            // A manually supplied DAT remains valid if the automatic catalog lacks it.
            if rom.dat.source.as_deref() == Some("manual") && state.status != Status::Matched {
                report.unchanged += 1;
                continue;
            }
            match state.status {
                Status::Matched => report.matched += 1,
                Status::NoMatch => report.no_match += 1,
                Status::Ambiguous => report.ambiguous += 1,
                _ => unreachable!("lookup has exactly three outcomes"),
            }
            save(lib, rom, &state)?;
        }
    }
    Ok(report)
}

fn save(lib: &mut Library, rom: &db::Rom, state: &Identification) -> Result<()> {
    if &rom.dat == state {
        return Ok(());
    }
    let tx = lib
        .conn_mut()
        .transaction()
        .map_err(|e| Error::db(e, "begin DAT result"))?;
    let changed = tx.execute("UPDATE roms SET dat_status=?2,dat_game_name=?3,dat_rom_name=?4,dat_revision=?5,dat_source=?6,dat_checked_at=?7,dat_error=?8,verified=?9 WHERE id=?1 AND sha1=?10 AND size=?11",
        params![rom.id,state.status.as_str(),state.game_name,state.rom_name,state.revision,state.source,state.checked_at,state.error,state.status == Status::Matched,rom.sha1,rom.size as i64])
        .map_err(|e| Error::db(e, "save DAT result"))?;
    if changed > 0 && rom.dat.game_name != state.game_name {
        tx.execute(
            "DELETE FROM covers WHERE game_id=?1 AND imported=0",
            params![rom.game_id],
        )
        .map_err(|e| Error::db(e, "invalidate previous catalog cover"))?;
    }
    tx.commit().map_err(|e| Error::db(e, "commit DAT result"))
}

/// Use a catalog title only when every ROM in this game agrees. Local file
/// and game titles remain user-controlled and are never renamed here.
pub fn cover_name(game: &db::Game) -> &str {
    let Some(first) = game.roms.first().and_then(|r| r.dat.game_name.as_deref()) else {
        return &game.name;
    };
    if game
        .roms
        .iter()
        .all(|r| r.dat.status == Status::Matched && r.dat.game_name.as_deref() == Some(first))
    {
        first
    } else {
        &game.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const XML: &[u8] = include_bytes!("../../../../tests/fixtures/roms/test.dat");

    fn setup() -> (tempfile::TempDir, Library, Cache) {
        let (tmp, lib) = crate::testutil::library_with(&[
            ("my title.gba", "hello world"),
            ("unknown.gba", "homebrew"),
        ]);
        db::ensure_platform(lib.conn(), "gba", None).unwrap();
        lib.conn()
            .execute("UPDATE games SET platform='gba'", [])
            .unwrap();
        let cache = Cache::new(tmp.path().join("cache"));
        (tmp, lib, cache)
    }
    fn store(cache: &Cache, bytes: &[u8]) {
        let system = system_for("gba").unwrap();
        cache
            .store(&cache_key(system), &url(BASE, system), bytes, None)
            .unwrap();
    }
    fn offline(lib: &mut Library, cache: &Cache) -> Report {
        run(
            lib,
            &Http::new(None).unwrap(),
            cache,
            Options {
                policy: Policy::Offline,
                base: BASE,
                platform: None,
            },
            &mut |_| {},
        )
        .unwrap()
    }
    fn rom(lib: &Library, name: &str) -> db::Rom {
        db::find_rom_by_name(lib.conn(), "gba", name)
            .unwrap()
            .unwrap()
    }

    #[test]
    fn cached_identification_keeps_names_and_rematches_only_after_revision_changes() {
        let (_tmp, mut lib, cache) = setup();
        assert_eq!(rom(&lib, "my title.gba").dat.status, Status::Unchecked);
        store(&cache, XML);
        let report = offline(&mut lib, &cache);
        assert_eq!((report.matched, report.no_match), (1, 1));
        let before = rom(&lib, "my title.gba");
        assert_eq!(before.dat.game_name.as_deref(), Some("Hello World (USA)"));
        assert!(lib.root().join(before.rel_path.as_ref().unwrap()).exists());
        assert_eq!(offline(&mut lib, &cache).unchanged, 2);
        assert_eq!(rom(&lib, "my title.gba").dat, before.dat);
        let updated = String::from_utf8(XML.to_vec())
            .unwrap()
            .replace("Hello World (USA)", "Corrected Name (USA)");
        store(&cache, updated.as_bytes());
        assert_eq!(offline(&mut lib, &cache).matched, 1);
        assert_eq!(
            rom(&lib, "my title.gba").dat.game_name.as_deref(),
            Some("Corrected Name (USA)")
        );
        let plans = super::super::names::standardize_plan(&lib, None, |_| {
            panic!("must not guess names after DAT lookup")
        })
        .unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].new_name, "Corrected Name (USA).fkh");
        assert_eq!(rom(&lib, "unknown.gba").dat.status, Status::NoMatch);
    }

    #[test]
    fn failure_is_retryable_and_does_not_erase_previous_results() {
        let (_tmp, mut lib, cache) = setup();
        let report = offline(&mut lib, &cache);
        assert_eq!(report.errors.len(), 1);
        assert_eq!(rom(&lib, "my title.gba").dat.status, Status::Error);
        store(&cache, XML);
        offline(&mut lib, &cache);
        let before = rom(&lib, "my title.gba").dat;
        let mut server = mockito::Server::new();
        let failed = server
            .mock("GET", "/Nintendo%20-%20Game%20Boy%20Advance.dat")
            .with_status(503)
            .expect(1)
            .create();
        let r = run(
            &mut lib,
            &Http::new(None).unwrap(),
            &cache,
            Options {
                policy: Policy::Refresh,
                base: &server.url(),
                platform: None,
            },
            &mut |_| {},
        )
        .unwrap();
        failed.assert();
        assert_eq!(r.errors.len(), 1);
        let after = rom(&lib, "my title.gba").dat;
        assert_eq!(after.status, Status::Matched);
        assert_eq!(after.game_name, before.game_name);
        assert!(after.error.is_some());
        failed.remove();
        let success = server
            .mock("GET", "/Nintendo%20-%20Game%20Boy%20Advance.dat")
            .with_body(XML)
            .expect(1)
            .create();
        let r = run(
            &mut lib,
            &Http::new(None).unwrap(),
            &cache,
            Options {
                policy: Policy::Refresh,
                base: &server.url(),
                platform: None,
            },
            &mut |_| {},
        )
        .unwrap();
        success.assert();
        assert_eq!(r.matched, 1);
        assert!(rom(&lib, "my title.gba").dat.error.is_none());
    }

    #[test]
    fn sha1_is_required_and_ambiguous_names_are_not_guessed() {
        let (_tmp, mut lib, cache) = setup();
        let xml = String::from_utf8(XML.to_vec()).unwrap();
        store(
            &cache,
            xml.replace(
                "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed",
                "0000000000000000000000000000000000000000",
            )
            .as_bytes(),
        );
        offline(&mut lib, &cache);
        assert_eq!(rom(&lib, "my title.gba").dat.status, Status::NoMatch);
        let duplicated = xml.replace("</datafile>", "<game name=\"Other version\"><rom name=\"Other.gba\" size=\"11\" sha1=\"2aae6c35c94fcfb415dbe95f408b9ce91ee846ed\"/></game></datafile>");
        store(&cache, duplicated.as_bytes());
        offline(&mut lib, &cache);
        let state = rom(&lib, "my title.gba").dat;
        assert_eq!(state.status, Status::Ambiguous);
        assert!(state.game_name.is_none());
        store(&cache, b"clrmamepro ( name \"broken\" ) game (");
        let report = offline(&mut lib, &cache);
        assert_eq!(report.errors.len(), 1);
        assert_eq!(rom(&lib, "my title.gba").dat.status, Status::Ambiguous);
    }

    #[test]
    fn manual_dat_match_is_preserved_when_automatic_catalog_lacks_it() {
        let (_tmp, mut lib, cache) = setup();
        lib.conn().execute("UPDATE roms SET dat_status='matched',dat_source='manual',dat_game_name='Custom verified title',verified=1 WHERE name='unknown.gba'", []).unwrap();
        store(&cache, XML);
        let report = offline(&mut lib, &cache);
        assert_eq!(
            (report.matched, report.no_match, report.unchanged),
            (1, 0, 1)
        );
        assert_eq!(rom(&lib, "unknown.gba").dat.status, Status::Matched);
    }

    #[test]
    fn recognition_keeps_manual_covers_but_retries_old_automatic_titles() {
        let (tmp, mut lib, cache) = setup();
        let id = rom(&lib, "my title.gba").game_id;
        lib.conn().execute("INSERT INTO covers (game_id,rel_path,status,fetched_at) VALUES (?1,'covers/old.png','ok',0)", [id]).unwrap();
        store(&cache, XML);
        offline(&mut lib, &cache);
        assert!(covers::record(lib.conn(), id).unwrap().is_none());
        let game = db::get_game(lib.conn(), id).unwrap().unwrap();
        assert_eq!(cover_name(&game), "Hello World (USA)");
        let source = tmp.path().join("cover.png");
        image::RgbImage::from_pixel(2, 2, image::Rgb([1, 2, 3]))
            .save(&source)
            .unwrap();
        let plan = covers::import::plan(&lib, id, &source).unwrap();
        covers::import::apply(&mut lib, &plan).unwrap();
        let before = covers::record(lib.conn(), id).unwrap();
        store(
            &cache,
            String::from_utf8(XML.to_vec())
                .unwrap()
                .replace("Hello World (USA)", "Renamed")
                .as_bytes(),
        );
        offline(&mut lib, &cache);
        assert_eq!(covers::record(lib.conn(), id).unwrap(), before);
    }
}
