//! Content identification against cached libretro DATs. A negative result is
//! valid only for that catalog revision; transport/parse failures stay retryable.
use super::{Library, db};

use crate::{
    Error, ErrorInfo, Result,
    catalog::libretro,
    fsutil,
    net::{Cache, Http, Policy},
    plan::{Event, Progress},
};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};

/// Where a ROM stands against its platform's DAT.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DatStatus {
    #[default]
    Unchecked,
    Matched,
    NoMatch,
    Error,
    Unsupported,
    Ambiguous,
}
impl DatStatus {
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
}

/// What the DAT said about one ROM, as stored on its row.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Identification {
    pub status: DatStatus,
    pub game_name: Option<String>,
    pub rom_name: Option<String>,
    pub revision: Option<String>,
    pub source: Option<String>,
    pub checked_at: Option<i64>,
    pub error: Option<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub matched: usize,
    pub no_match: usize,
    pub ambiguous: usize,
    pub unsupported: usize,
    pub unchanged: usize,
    /// Platforms whose DAT could not be had (`DAT_UNAVAILABLE`) or came
    /// from the cache (`DAT_CACHED`).
    pub errors: Vec<ErrorInfo>,
}

pub struct Options<'a> {
    pub policy: Policy,
    pub base: &'a str,
    /// Only these platforms (all when `None`).
    pub platforms: Option<&'a [String]>,
}

impl Report {
    fn count(&mut self, status: DatStatus) {
        match status {
            DatStatus::Matched => self.matched += 1,
            DatStatus::NoMatch => self.no_match += 1,
            DatStatus::Ambiguous => self.ambiguous += 1,
            DatStatus::Unsupported => self.unsupported += 1,
            DatStatus::Unchecked | DatStatus::Error => {}
        }
    }
}

/// Identifies every ROM in the library (or on one platform) against its
/// platform's libretro DAT. One platform failing doesn't stop the others.
pub fn run(
    lib: &mut Library,
    http: &Http,
    cache: &Cache,
    opts: Options<'_>,
    progress: Progress<'_>,
) -> Result<Report> {
    let mut platforms = BTreeMap::<String, Vec<db::Rom>>::new();
    let wanted = |p: &str| opts.platforms.is_none_or(|ps| ps.iter().any(|w| w == p));
    for game in lib
        .list_games(None, None)?
        .into_iter()
        .filter(|g| wanted(&g.platform))
    {
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
        let Some(system) = crate::catalog::dat_system(&platform) else {
            mark_unsupported(lib, &roms, &mut report)?;
            continue;
        };
        let catalog = match Catalog::load(http, cache, opts.base, system, opts.policy) {
            Ok(c) => c,
            Err(e) => {
                mark_failed(lib, &roms, &e.to_string())?;
                report.errors.push(
                    Error::DatUnavailable {
                        platform,
                        source: Box::new(e),
                    }
                    .to_info(),
                );
                continue;
            }
        };
        if let Some(reason) = &catalog.fallback {
            report.errors.push(
                Error::DatCached {
                    platform: platform.clone(),
                    reason: reason.clone(),
                }
                .to_info(),
            );
        }
        for rom in &roms {
            let state = match catalog.identify(rom) {
                Verdict::New(state) => {
                    report.count(state.status);
                    state
                }
                // Same verdict; only whether the DAT came from a stale cache may differ.
                Verdict::Current => {
                    report.unchanged += 1;
                    Identification {
                        error: catalog.fallback.clone(),
                        ..rom.dat.clone()
                    }
                }
                Verdict::KeepManual => {
                    report.unchanged += 1;
                    continue;
                }
            };
            save(lib, rom, &state)?;
        }
    }
    Ok(report)
}

/// A platform without an automatic DAT: every ROM not already matched
/// (by a manual DAT, say) is marked unsupported.
fn mark_unsupported(lib: &mut Library, roms: &[db::Rom], report: &mut Report) -> Result<()> {
    for rom in roms {
        match rom.dat.status {
            DatStatus::Matched => report.unchanged += 1,
            DatStatus::Unsupported => report.unsupported += 1,
            _ => {
                let state = Identification {
                    status: DatStatus::Unsupported,
                    checked_at: Some(fsutil::now_secs()),
                    ..Default::default()
                };
                save(lib, rom, &state)?;
                report.unsupported += 1;
            }
        }
    }
    Ok(())
}

/// The DAT couldn't be had: record the error, but never discard an
/// established result because the server is down.
fn mark_failed(lib: &mut Library, roms: &[db::Rom], error: &str) -> Result<()> {
    for rom in roms {
        let mut state = rom.dat.clone();
        if matches!(
            state.status,
            DatStatus::Unchecked | DatStatus::Error | DatStatus::Unsupported
        ) {
            state.status = DatStatus::Error;
        }
        state.error = Some(error.to_string());
        save(lib, rom, &state)?;
    }
    Ok(())
}

/// What a catalog says about one ROM.
enum Verdict {
    /// A fresh identification to store.
    New(Identification),
    /// Already checked against this revision.
    Current,
    /// Matched by a manual DAT that this catalog can't confirm: left alone.
    KeepManual,
}

/// One platform's DAT, indexed by (size, SHA-1).
struct Catalog {
    source: String,
    revision: String,
    /// Why the cached copy was used instead of a fresh one, if it was.
    fallback: Option<String>,
    /// Every distinct (game name, ROM name) with that content.
    by_content: HashMap<(u64, String), Vec<(String, String)>>,
}

impl Catalog {
    fn load(http: &Http, cache: &Cache, base: &str, system: &str, policy: Policy) -> Result<Self> {
        let loaded = libretro::load_dat(http, cache, base, system, policy)?;
        let fallback = match loaded.source {
            crate::net::Source::Cache {
                fallback_reason, ..
            } => fallback_reason,
            _ => None,
        };
        let mut by_content = HashMap::<_, Vec<(String, String)>>::new();
        for game in loaded.dat.games {
            for rom in game.roms {
                if let (Some(size), Some(sha1)) = (rom.size, rom.sha1) {
                    let names = by_content.entry((size, sha1)).or_default();
                    let name = (game.name.clone(), rom.name);
                    if !names.contains(&name) {
                        names.push(name);
                    }
                }
            }
        }
        Ok(Catalog {
            source: libretro::dat_url(base, system),
            revision: loaded.revision,
            fallback,
            by_content,
        })
    }

    fn identify(&self, rom: &db::Rom) -> Verdict {
        if rom.dat.revision.as_deref() == Some(&self.revision)
            && rom.dat.source.as_deref() == Some(&self.source)
        {
            return Verdict::Current;
        }
        let mut state = Identification {
            revision: Some(self.revision.clone()),
            source: Some(self.source.clone()),
            checked_at: Some(fsutil::now_secs()),
            error: self.fallback.clone(),
            ..Default::default()
        };
        let names = self
            .by_content
            .get(&(rom.size, rom.sha1.to_ascii_lowercase()));
        match names.map(Vec::as_slice).unwrap_or_default() {
            [(game, name)] => {
                state.status = DatStatus::Matched;
                state.game_name = Some(game.clone());
                state.rom_name = Some(name.clone());
            }
            [] => state.status = DatStatus::NoMatch,
            _ => state.status = DatStatus::Ambiguous,
        }
        if rom.dat.source.as_deref() == Some("manual") && state.status != DatStatus::Matched {
            return Verdict::KeepManual;
        }
        Verdict::New(state)
    }
}

fn save(lib: &mut Library, rom: &db::Rom, state: &Identification) -> Result<()> {
    if &rom.dat == state {
        return Ok(());
    }
    let tx = lib
        .conn_mut()
        .transaction()
        .map_err(|e| Error::db(e, "begin DAT result"))?;
    let changed = db::save_identification(&tx, rom, state)?;
    if changed && rom.dat.game_name != state.game_name {
        db::delete_catalog_cover(&tx, rom.game_id)?;
    }
    tx.commit().map_err(|e| Error::db(e, "commit DAT result"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::covers;
    const XML: &[u8] = include_bytes!("../../../../tests/fixtures/roms/test.dat");

    fn setup() -> (tempfile::TempDir, Library, Cache) {
        let (tmp, lib) = crate::testutil::library_on(
            "gba",
            &[("my title.gba", "hello world"), ("unknown.gba", "homebrew")],
        );
        let cache = Cache::new(tmp.path().join("cache"));
        (tmp, lib, cache)
    }
    fn store(cache: &Cache, bytes: &[u8]) {
        let system = crate::catalog::dat_system("gba").unwrap();
        cache
            .store(
                &libretro::dat_cache_key(system),
                &libretro::dat_url(libretro::DAT_BASE, system),
                bytes,
                None,
            )
            .unwrap();
    }
    fn offline(lib: &mut Library, cache: &Cache) -> Report {
        run(
            lib,
            &Http::new(None).unwrap(),
            cache,
            Options {
                policy: Policy::Offline,
                base: libretro::DAT_BASE,
                platforms: None,
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
        assert_eq!(rom(&lib, "my title.gba").dat.status, DatStatus::Unchecked);
        store(&cache, XML);
        let report = offline(&mut lib, &cache);
        assert_eq!((report.matched, report.no_match), (1, 1));
        let before = rom(&lib, "my title.gba");
        assert_eq!(before.dat.game_name.as_deref(), Some("Hello World (USA)"));
        assert!(lib.root().join(&before.rel_path).exists());
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
        let plans = super::super::rename::standardize_plan(&lib, None, |_| {
            panic!("must not guess names after DAT lookup")
        })
        .unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].new_name, "Corrected Name (USA).fkh");
        assert_eq!(rom(&lib, "unknown.gba").dat.status, DatStatus::NoMatch);
    }

    #[test]
    fn failure_is_retryable_and_does_not_erase_previous_results() {
        let (_tmp, mut lib, cache) = setup();
        let report = offline(&mut lib, &cache);
        assert_eq!(report.errors.len(), 1);
        assert_eq!(rom(&lib, "my title.gba").dat.status, DatStatus::Error);
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
                platforms: None,
            },
            &mut |_| {},
        )
        .unwrap();
        failed.assert();
        assert_eq!(r.errors.len(), 1);
        let after = rom(&lib, "my title.gba").dat;
        assert_eq!(after.status, DatStatus::Matched);
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
                platforms: None,
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
        assert_eq!(rom(&lib, "my title.gba").dat.status, DatStatus::NoMatch);
        let duplicated = xml.replace("</datafile>", "<game name=\"Other version\"><rom name=\"Other.gba\" size=\"11\" sha1=\"2aae6c35c94fcfb415dbe95f408b9ce91ee846ed\"/></game></datafile>");
        store(&cache, duplicated.as_bytes());
        offline(&mut lib, &cache);
        let state = rom(&lib, "my title.gba").dat;
        assert_eq!(state.status, DatStatus::Ambiguous);
        assert!(state.game_name.is_none());
        store(&cache, b"clrmamepro ( name \"broken\" ) game (");
        let report = offline(&mut lib, &cache);
        assert_eq!(report.errors.len(), 1);
        assert_eq!(rom(&lib, "my title.gba").dat.status, DatStatus::Ambiguous);
    }

    #[test]
    fn manual_dat_match_is_preserved_when_automatic_catalog_lacks_it() {
        let (_tmp, mut lib, cache) = setup();
        let manual = Identification {
            status: DatStatus::Matched,
            game_name: Some("Custom verified title".into()),
            source: Some("manual".into()),
            ..Default::default()
        };
        db::save_identification(lib.conn(), &rom(&lib, "unknown.gba"), &manual).unwrap();
        store(&cache, XML);
        let report = offline(&mut lib, &cache);
        assert_eq!(
            (report.matched, report.no_match, report.unchanged),
            (1, 0, 1)
        );
        assert_eq!(rom(&lib, "unknown.gba").dat.status, DatStatus::Matched);
    }

    #[test]
    fn recognition_keeps_manual_covers_but_retries_old_automatic_titles() {
        let (tmp, mut lib, cache) = setup();
        let id = rom(&lib, "my title.gba").game_id;
        db::upsert_cover(
            lib.conn(),
            id,
            Some(std::path::Path::new("covers/old.png")),
            covers::CoverStatus::Ok,
        )
        .unwrap();
        store(&cache, XML);
        offline(&mut lib, &cache);
        assert!(db::cover(lib.conn(), id).unwrap().is_none());
        let game = db::get_game(lib.conn(), id).unwrap().unwrap();
        assert_eq!(covers::cover_name(&game), "Hello World (USA)");
        let source = tmp.path().join("cover.png");
        image::RgbImage::from_pixel(2, 2, image::Rgb([1, 2, 3]))
            .save(&source)
            .unwrap();
        let mut plan = covers::import::plan(&lib, id, &source).unwrap();
        plan.apply(&mut crate::testutil::reopen(&lib), &mut |_| {})
            .unwrap();
        let before = db::cover(lib.conn(), id).unwrap();
        store(
            &cache,
            String::from_utf8(XML.to_vec())
                .unwrap()
                .replace("Hello World (USA)", "Renamed")
                .as_bytes(),
        );
        offline(&mut lib, &cache);
        assert_eq!(db::cover(lib.conn(), id).unwrap(), before);
    }
}
