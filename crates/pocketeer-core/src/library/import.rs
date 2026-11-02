//! Bringing ROM files into the library: plan (name, match, dedupe) → apply
//! (copy + index rows, one transaction per file).
//!
//! Files are read as few times as possible, because the source may be a
//! Pocket card over USB: with a DAT every file is hashed while planning (the
//! DAT decides its name); without one, only a file whose name and size already
//! exist in the library is read at plan time (to tell "already have it" from
//! "different file, same name"), and everything else is hashed while it is
//! copied. Content duplicates found that late are dropped again.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use serde::Serialize;

use crate::{
    Error, ErrorInfo, Result,
    catalog::Dat,
    fsutil,
    fsutil::{
        hash::{HashCache, Hashes, copy_and_hash},
        scan::{Candidate, hash_candidate},
    },
    plan::{Applied, Event, Plan, Progress},
};

use super::{
    Library,
    db::{self, NewRom},
};

pub use crate::fsutil::scan::scan_source;

/// How an import is planned.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportOptions {
    /// With a DAT: skip files it doesn't know instead of importing them unverified.
    pub dat_only: bool,
    /// Display names of platforms, where known (from the inventory).
    pub platform_names: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ImportAction {
    Add {
        platform: String,
        source: PathBuf,
        #[serde(skip_serializing_if = "Option::is_none")]
        inner: Option<String>,
        game: String,
        rom: String,
        /// Managed path relative to the library root.
        dest: PathBuf,
        size: u64,
        /// Known when the file was hashed while planning; otherwise computed
        /// during the copy.
        #[serde(skip_serializing_if = "Option::is_none")]
        crc32: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        sha1: Option<String>,
        verified: bool,
        /// A standard (No-Intro style) name this file probably is, when the
        /// DAT didn't name it; see [`suggest_names`].
        #[serde(skip_serializing_if = "Option::is_none")]
        suggested: Option<String>,
    },
    Skip {
        platform: String,
        source: String,
        reason: ImportSkip,
    },
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ImportSkip {
    /// Identical contents already in the library.
    AlreadyInLibrary {
        rom: String,
    },
    /// `--dat-only` and the DAT doesn't list it.
    NotInDat,
    /// Another ROM already uses this file name on the platform.
    NameConflict {
        existing: String,
    },
    /// Two sources in this import resolve to the same name.
    Duplicate {
        of: PathBuf,
    },
    Unreadable {
        error: ErrorInfo,
    },
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct ImportSummary {
    pub add: usize,
    pub verified: usize,
    pub unverified: usize,
    pub skip: usize,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ImportPlan {
    pub dir: PathBuf,
    /// Every file's action, grouped by platform.
    pub actions: Vec<ImportAction>,
    pub summary: ImportSummary,
    /// Files whose platform their extension doesn't say, left out.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unknown: Vec<String>,
    /// Display names for the platforms it adds to, recorded on apply.
    #[serde(skip)]
    platform_names: BTreeMap<String, String>,
}

impl Plan for ImportPlan {
    fn changes(&self) -> usize {
        self.summary.add
    }
}

impl ImportPlan {
    /// The platforms the plan has files for, in order.
    pub fn platforms(&self) -> BTreeSet<&str> {
        self.actions.iter().map(ImportAction::platform).collect()
    }

    /// Every (platform, current name, suggested standard name) among the
    /// planned adds.
    pub fn suggestions(&self) -> impl Iterator<Item = (&str, &str, &str)> {
        self.actions.iter().filter_map(|a| match a {
            ImportAction::Add {
                platform,
                rom,
                suggested: Some(s),
                ..
            } => Some((platform.as_str(), rom.as_str(), s.as_str())),
            _ => None,
        })
    }
}

impl ImportAction {
    pub fn platform(&self) -> &str {
        match self {
            ImportAction::Add { platform, .. } | ImportAction::Skip { platform, .. } => platform,
        }
    }
}

/// Game name for a file the DAT doesn't know: the file stem.
pub fn fallback_game_name(rom_name: &str) -> String {
    crate::fsutil::names::stem(rom_name).to_string()
}

/// Plans importing a folder of ROMs (scanned recursively) or one ROM / zip.
/// With `platform` every file is that platform's; otherwise each file's
/// extension says (`known`: platform ids the library or card already use),
/// and the files it can't place are listed as unknown.
#[allow(clippy::too_many_arguments)]
pub fn plan_folder(
    lib: &Library,
    source: &Path,
    platform: Option<&str>,
    known: &[String],
    opts: &ImportOptions,
    dat: Option<&Dat>,
    cache: &mut HashCache,
    progress: Progress<'_>,
) -> Result<ImportPlan> {
    let (dir, candidates) = scan_source(source)?;
    let mut groups: BTreeMap<String, Vec<Candidate>> = BTreeMap::new();
    let mut unknown = Vec::new();
    for c in candidates {
        let found = match platform {
            Some(p) => Some(p.to_string()),
            None => crate::catalog::platform_of(&c.file.display_name(), known),
        };
        match found {
            Some(p) => groups.entry(p).or_default().push(c),
            None => unknown.push(c.file.location()),
        }
    }
    let mut plan = plan_import(lib, &dir, &groups, opts, dat, cache, progress)?;
    plan.unknown = unknown;
    Ok(plan)
}

/// Plans importing files given relative to `dir` (the card's root when
/// importing from it), grouped by platform. `opts` only shapes the plan:
/// applying it needs nothing else.
pub fn plan_import(
    lib: &Library,
    dir: &Path,
    groups: &BTreeMap<String, Vec<Candidate>>,
    opts: &ImportOptions,
    dat: Option<&Dat>,
    cache: &mut HashCache,
    progress: Progress<'_>,
) -> Result<ImportPlan> {
    let total = groups.values().map(Vec::len).sum();
    progress(Event::Status(format!(
        "{} {total} file(s)",
        if dat.is_some() { "Hashing" } else { "Planning" }
    )));

    let mut plan = ImportPlan {
        dir: dir.to_path_buf(),
        actions: Vec::new(),
        summary: ImportSummary::default(),
        unknown: Vec::new(),
        platform_names: opts
            .platform_names
            .iter()
            .filter(|(id, _)| groups.contains_key(*id))
            .map(|(id, name)| (id.clone(), name.clone()))
            .collect(),
    };
    let mut done = 0;
    for (platform, files) in groups {
        let mut claimed: Vec<(String, PathBuf)> = Vec::new();
        for c in files {
            done += 1;
            progress(Event::Step { done, total });
            let skip = |plan: &mut ImportPlan, reason: ImportSkip| {
                plan.summary.skip += 1;
                plan.actions.push(ImportAction::Skip {
                    platform: platform.clone(),
                    source: c.file.location(),
                    reason,
                });
            };

            // Read the file now only when its contents decide something here.
            let same_name = db::find_rom_by_name(lib.conn(), platform, &c.file.display_name())?;
            let needs_hash = dat.is_some() || same_name.as_ref().is_some_and(|r| r.size == c.size);
            let hashes = if needs_hash {
                match hash_candidate(dir, c, cache) {
                    Ok(h) => Some(h),
                    Err(e) => {
                        skip(&mut plan, ImportSkip::Unreadable { error: e.to_info() });
                        continue;
                    }
                }
            } else {
                None
            };

            // Name it: DAT first, file name otherwise.
            let dat_hit = match (dat, &hashes) {
                (Some(d), Some(h)) => d.find(h).map(|k| d.get(k)),
                _ => None,
            };
            let (game, rom, verified) = match dat_hit {
                Some((g, r)) => (g.name.clone(), r.name.clone(), true),
                None if opts.dat_only => {
                    skip(&mut plan, ImportSkip::NotInDat);
                    continue;
                }
                None => {
                    let name = c.file.display_name();
                    (fallback_game_name(&name), name, false)
                }
            };

            if let Some(h) = &hashes
                && let Some(existing) = db::find_rom_by_sha1(lib.conn(), platform, &h.sha1)?
            {
                skip(
                    &mut plan,
                    ImportSkip::AlreadyInLibrary { rom: existing.name },
                );
                continue;
            }
            if let Some(existing) = db::find_rom_by_name(lib.conn(), platform, &rom)? {
                skip(
                    &mut plan,
                    ImportSkip::NameConflict {
                        existing: existing.name,
                    },
                );
                continue;
            }
            if let Some((_, prev)) = claimed.iter().find(|(n, _)| *n == rom) {
                skip(&mut plan, ImportSkip::Duplicate { of: prev.clone() });
                continue;
            }
            claimed.push((rom.clone(), c.file.path.clone()));

            let dest = Library::rom_rel_path(platform, &rom);

            let size = hashes.as_ref().map(|h| h.size).unwrap_or(c.size);
            plan.summary.add += 1;
            plan.summary.bytes += size;
            if verified {
                plan.summary.verified += 1;
            } else {
                plan.summary.unverified += 1;
            }
            plan.actions.push(ImportAction::Add {
                platform: platform.clone(),
                source: dir.join(&c.file.path),
                inner: c.file.inner.clone(),
                game,
                rom,
                dest,
                size,
                crc32: hashes.as_ref().map(Hashes::crc32_hex),
                sha1: hashes.map(|h| h.sha1),
                verified,
                suggested: None,
            });
        }
    }
    cache.save_best_effort();
    Ok(plan)
}

/// Fills `suggested` on every unverified add whose name matches a standard
/// name in `standard` (see [`crate::catalog::libretro::suggest`]). Returns how many.
/// Only `platform`'s files: the names are per platform.
pub fn suggest_names(plan: &mut ImportPlan, platform: &str, standard: &[String]) -> usize {
    let mut n = 0;
    for a in &mut plan.actions {
        if let ImportAction::Add {
            platform: p,
            rom,
            verified: false,
            suggested,
            ..
        } = a
            && p == platform
            && let Some(s) = crate::catalog::libretro::suggest(rom, standard)
        {
            *suggested = Some(format!("{s}{}", crate::fsutil::names::extension(rom)));
            n += 1;
        }
    }
    n
}

/// Renames planned adds to their suggestion where `accept(platform, current name)`
/// says so: ROM name, game name and managed destination move together. A
/// suggestion that would collide with another planned or library name is left
/// alone.
pub fn apply_suggestions(
    lib: &Library,
    plan: &mut ImportPlan,
    accept: impl Fn(&str, &str) -> bool,
) -> Result<usize> {
    let planned: Vec<(String, String)> = plan
        .actions
        .iter()
        .filter_map(|a| match a {
            ImportAction::Add { platform, rom, .. } => Some((platform.clone(), rom.clone())),
            _ => None,
        })
        .collect();
    let mut n = 0;
    for a in &mut plan.actions {
        let ImportAction::Add {
            platform,
            rom,
            game,
            dest,
            suggested: Some(new),
            ..
        } = a
        else {
            continue;
        };
        if !accept(platform, rom) || planned.contains(&(platform.clone(), new.clone())) {
            continue;
        }
        if db::find_rom_by_name(lib.conn(), platform, new)?.is_some() {
            continue;
        }
        *game = fallback_game_name(new);
        *dest = Library::rom_rel_path(platform, new);
        *rom = new.clone();
        n += 1;
    }
    for a in &mut plan.actions {
        if let ImportAction::Add { suggested, .. } = a {
            *suggested = None;
        }
    }
    Ok(n)
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct ImportResult {
    pub added: usize,
    pub bytes: u64,
    /// Planned adds that turned out, once hashed, to be bytes the library
    /// already had under another name; nothing was kept.
    pub duplicates: Vec<ImportDuplicate>,
    pub failed: Vec<ImportFailure>,
    /// Things that went wrong around the import without failing it (a card
    /// cache that couldn't be updated, say).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(skip)]
    pub(crate) verified_sources: Vec<VerifiedSource>,
}

impl Applied for ImportResult {
    fn failed(&self) -> usize {
        self.failed.len()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct VerifiedSource {
    pub path: PathBuf,
    pub sha1: String,
    pub size: u64,
    pub modified: std::time::SystemTime,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ImportDuplicate {
    pub source: PathBuf,
    pub existing: String,
}

enum Indexed {
    Added(u64),
    Duplicate(String),
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ImportFailure {
    pub source: PathBuf,
    pub error: ErrorInfo,
}

pub fn apply_import(
    lib: &mut Library,
    plan: &ImportPlan,
    progress: Progress<'_>,
) -> Result<ImportResult> {
    let root = lib.root().to_path_buf();
    let total = plan.changes();
    progress(Event::Status(format!("Importing {total} file(s)")));
    let mut result = ImportResult::default();

    for platform in plan.platforms() {
        let name = plan.platform_names.get(platform).map(String::as_str);
        db::ensure_platform(lib.conn(), platform, name)?;
    }
    let mut done = 0;
    for action in &plan.actions {
        let ImportAction::Add {
            platform,
            source,
            inner,
            game,
            rom,
            dest,
            sha1,
            verified,
            ..
        } = action
        else {
            continue;
        };
        let source_before = if inner.is_none() {
            std::fs::metadata(source)
                .ok()
                .and_then(|m| Some((m.len(), m.modified().ok()?)))
        } else {
            None
        };
        let mut verified_sha1 = None;
        let mut placed = false;
        let indexed = (|| -> Result<Indexed> {
            let hashes = place_file(&root, source, inner.as_deref(), dest)?;
            placed = true;
            if sha1
                .as_ref()
                .is_some_and(|expected| *expected != hashes.sha1)
            {
                return Err(Error::Stale {
                    what: source.display().to_string(),
                });
            }
            verified_sha1 = Some(hashes.sha1.clone());
            let tx = lib
                .conn_mut()
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(|e| Error::db(e, "begin import"))?;
            if let Some(existing) = db::find_rom_by_sha1(&tx, platform, &hashes.sha1)? {
                return Ok(Indexed::Duplicate(existing.name));
            }
            let game_id = db::upsert_game(&tx, platform, game)?;
            db::insert_rom(
                &tx,
                &NewRom {
                    game_id,
                    name: rom,
                    size: hashes.size,
                    crc32: &hashes.crc32_hex(),
                    sha1: &hashes.sha1,
                    verified: *verified,
                    // Built from the platform id and the ROM name: UTF-8.
                    rel_path: &dest.to_string_lossy(),
                },
            )?;
            tx.commit().map_err(|e| Error::db(e, "commit import"))?;
            Ok(Indexed::Added(hashes.size))
        })();
        if placed && !matches!(indexed, Ok(Indexed::Added(_))) {
            if let Err(e) = std::fs::remove_file(root.join(dest)) {
                result.failed.push(ImportFailure {
                    source: root.join(dest),
                    error: Error::io(root.join(dest), e).to_info(),
                });
            }
        }
        if indexed.is_ok()
            && let (Some((size, modified)), Some(sha1)) = (source_before, verified_sha1)
            && std::fs::metadata(source)
                .is_ok_and(|m| m.len() == size && m.modified().ok() == Some(modified))
        {
            result.verified_sources.push(VerifiedSource {
                path: source.clone(),
                sha1,
                size,
                modified,
            });
        }
        match indexed {
            Ok(Indexed::Added(size)) => {
                result.added += 1;
                result.bytes += size;
            }
            Ok(Indexed::Duplicate(existing)) => {
                result.duplicates.push(ImportDuplicate {
                    source: source.clone(),
                    existing,
                });
            }
            Err(e) => {
                // Don't leave a managed file without an index row.
                result.failed.push(ImportFailure {
                    source: source.clone(),
                    error: e.to_info(),
                });
            }
        }
        done += 1;
        progress(Event::Step { done, total });
    }
    Ok(result)
}

/// Copies or extracts a ROM and computes its identity in the same pass.
fn place_file(root: &Path, source: &Path, inner: Option<&str>, dest: &Path) -> Result<Hashes> {
    let full = root.join(dest);
    let parent = full
        .parent()
        .ok_or_else(|| Error::NotADirectory { path: full.clone() })?;
    std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    if full.exists() {
        return Err(Error::AlreadyExists { path: full });
    }
    let mut hashes = None;
    fsutil::write_new_via_temp(&full, |out| {
        let h = match inner {
            None => {
                let mut f = std::fs::File::open(source).map_err(|e| Error::io(source, e))?;
                copy_and_hash(&mut f, out).map_err(|e| Error::io(source, e))?
            }
            Some(inner) => fsutil::with_zip_entry(source, inner, |entry| {
                copy_and_hash(entry, out).map_err(|e| Error::io(source, e))
            })?,
        };
        hashes = Some(h);
        Ok(())
    })?;
    Ok(hashes.expect("successful copy produced hashes"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use crate::testutil::{build_zip, dat, write};

    fn library_dir() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let w = |rel: &str, body: &[u8]| write(tmp.path(), rel, body);
        w("Hello World (USA).fkh", b"hello world");
        w(
            "sub/quickfox.fkh",
            b"The quick brown fox jumps over the lazy dog",
        );
        w("random.fkh", b"not in dat");
        w(
            "discs.zip",
            &build_zip(&[("Two Discs (Japan) (Disc 1).bin", b"abc")]),
        );
        tmp
    }

    fn opts(dat_only: bool) -> ImportOptions {
        ImportOptions {
            dat_only,
            platform_names: [("fkh".into(), "Fake Handheld".into())].into(),
        }
    }

    fn kinds(plan: &ImportPlan) -> Vec<String> {
        plan.actions
            .iter()
            .map(|a| match a {
                ImportAction::Add {
                    rom,
                    game,
                    verified,
                    ..
                } => {
                    format!("add {rom} [{game}] v={verified} copy")
                }
                ImportAction::Skip { source, reason, .. } => format!("skip {source} {reason:?}"),
            })
            .collect()
    }

    #[test]
    fn copy_import_with_dat_then_dedupes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lib = Library::init(&tmp.path().join("lib")).unwrap();
        let src = library_dir();
        let dat = dat();
        let o = opts(false);
        let mut cache = HashCache::in_memory();
        let plan = plan_folder(
            &lib,
            src.path(),
            Some("fkh"),
            &[],
            &o,
            Some(&dat),
            &mut cache,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(
            kinds(&plan),
            [
                "add Hello World (USA).fkh [Hello World (USA)] v=true copy",
                "add Two Discs (Japan) (Disc 1).bin [Two Discs (Japan)] v=true copy",
                "add random.fkh [random] v=false copy",
                "add Quick Fox (Europe).fkh [Quick Fox (Europe)] v=true copy",
            ]
        );
        assert_eq!(plan.summary.verified, 3);
        assert_eq!(plan.summary.unverified, 1);

        let r = apply_import(&mut lib, &plan, &mut |_| {}).unwrap();
        assert_eq!(r.added, 4);
        assert!(r.failed.is_empty());
        assert_eq!(
            fs::read_to_string(lib.root().join("roms/fkh/Two Discs (Japan) (Disc 1).bin")).unwrap(),
            "abc"
        );
        assert!(lib.root().join("roms/fkh/Quick Fox (Europe).fkh").exists());
        let st = lib.status().unwrap();
        assert_eq!((st.games, st.roms), (4, 4));
        assert_eq!(st.platforms[0].name.as_deref(), Some("Fake Handheld"));
        assert_eq!(st.platforms[0].verified, 3);

        // Importing again: everything already there.
        let plan2 = plan_folder(
            &lib,
            src.path(),
            Some("fkh"),
            &[],
            &o,
            Some(&dat),
            &mut cache,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(plan2.changes(), 0);
        assert!(plan2.actions.iter().all(|a| matches!(
            a,
            ImportAction::Skip {
                reason: ImportSkip::AlreadyInLibrary { .. },
                ..
            }
        )));

        // Same name, different bytes → conflict, not overwrite. A fresh hash
        // cache: the rewrite keeps the size and lands in the same second, so
        // the (size, mtime) stamp can't tell the files apart.
        fs::write(src.path().join("Hello World (USA).fkh"), "HELLO WORLD").unwrap();
        let plan3 = plan_folder(
            &lib,
            src.path(),
            Some("fkh"),
            &[],
            &o,
            None,
            &mut HashCache::in_memory(),
            &mut |_| {},
        )
        .unwrap();
        assert!(kinds(&plan3).iter().any(|k| k.contains("NameConflict")));
    }

    #[test]
    fn dat_only_copies_verified_roms() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lib = Library::init(&tmp.path().join("lib")).unwrap();
        let src = library_dir();
        let dat = dat();
        let o = opts(true);
        let plan = plan_folder(
            &lib,
            src.path(),
            Some("fkh"),
            &[],
            &o,
            Some(&dat),
            &mut HashCache::in_memory(),
            &mut |_| {},
        )
        .unwrap();
        let k = kinds(&plan);
        assert!(k.iter().any(|x| x == "skip random.fkh NotInDat"), "{k:?}");
        // The zipped disc can't be referenced; it's extracted instead.
        assert!(
            k.iter()
                .any(|x| x == "add Two Discs (Japan) (Disc 1).bin [Two Discs (Japan)] v=true copy"),
            "{k:?}"
        );
        assert!(
            k.iter()
                .any(|x| x == "add Hello World (USA).fkh [Hello World (USA)] v=true copy"),
            "{k:?}"
        );

        let r = apply_import(&mut lib, &plan, &mut |_| {}).unwrap();
        assert_eq!(r.added, 3);
        assert!(
            lib.root().join("roms/fkh/Hello World (USA).fkh").exists(),
            "managed copy"
        );
        let games = db::list_games(lib.conn(), Some("fkh"), Some("hello")).unwrap();
        let rom = &games[0].roms[0];
        assert_eq!(
            lib.rom_abs_path(rom),
            lib.root().join("roms/fkh/Hello World (USA).fkh")
        );
    }

    #[test]
    fn without_a_dat_nothing_is_read_while_planning_and_duplicates_drop_late() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lib = Library::init(&tmp.path().join("lib")).unwrap();
        let src = tempfile::tempdir().unwrap();
        fs::write(src.path().join("a.fkh"), "same bytes").unwrap();
        fs::write(src.path().join("b.fkh"), "same bytes").unwrap();
        let o = opts(false);
        let plan = plan_folder(
            &lib,
            src.path(),
            Some("fkh"),
            &[],
            &o,
            None,
            &mut HashCache::in_memory(),
            &mut |_| {},
        )
        .unwrap();
        // No DAT and nothing in the library yet: both planned, neither hashed.
        assert_eq!(plan.changes(), 2);
        assert!(plan.actions.iter().all(|a| matches!(
            a,
            ImportAction::Add {
                crc32: None,
                sha1: None,
                ..
            }
        )));
        let r = apply_import(&mut lib, &plan, &mut |_| {}).unwrap();
        assert_eq!(r.added, 1);
        assert_eq!(r.duplicates.len(), 1);
        assert_eq!(r.duplicates[0].existing, "a.fkh");
        assert!(!lib.root().join("roms/fkh/b.fkh").exists());
        let rom = &db::list_games(lib.conn(), None, None).unwrap()[0].roms[0];
        assert_eq!(rom.crc32.len(), 8, "hashed during the copy");
        assert_eq!(rom.sha1.len(), 40);
    }

    #[test]
    fn imported_copy_is_independent_of_source() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lib = Library::init(&tmp.path().join("lib")).unwrap();
        let src = library_dir();
        let o = opts(false);
        let plan = plan_folder(
            &lib,
            src.path(),
            Some("fkh"),
            &[],
            &o,
            None,
            &mut HashCache::in_memory(),
            &mut |_| {},
        )
        .unwrap();
        apply_import(&mut lib, &plan, &mut |_| {}).unwrap();
        let managed = lib.root().join("roms/fkh/random.fkh");
        fs::write(src.path().join("random.fkh"), "changed source").unwrap();
        assert_eq!(fs::read_to_string(&managed).unwrap(), "not in dat");
        // Without a DAT the game name is the stem.
        let games = db::list_games(lib.conn(), None, Some("random")).unwrap();
        assert_eq!(games[0].name, "random");
    }

    #[test]
    fn a_single_file_or_zip_imports_like_a_folder_of_one() {
        let lib_dir = tempfile::tempdir().unwrap();
        let lib = Library::init(lib_dir.path()).unwrap();
        let src = tempfile::tempdir().unwrap();
        write(src.path(), "Downloads/game.gba", "ROM");
        std::fs::write(
            src.path().join("Downloads/pack.zip"),
            build_zip(&[("inner.gba", b"IN")]),
        )
        .unwrap();
        let opts = ImportOptions::default();
        let mut cache = HashCache::open(None);
        for (file, expected) in [("game.gba", "game.gba"), ("pack.zip", "inner.gba")] {
            let plan = plan_folder(
                &lib,
                &src.path().join("Downloads").join(file),
                Some("gba"),
                &[],
                &opts,
                None,
                &mut cache,
                &mut |_| {},
            )
            .unwrap();
            assert_eq!(plan.dir, src.path().join("Downloads"));
            assert_eq!(plan.changes(), 1);
            assert!(matches!(&plan.actions[0], ImportAction::Add { rom, .. } if rom == expected));
        }
    }
    #[test]
    fn failed_import_preserves_existing_files_and_rolls_back_its_own_files() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lib = Library::init(&tmp.path().join("lib")).unwrap();
        let source = tmp.path().join("source");
        write(&source, "a.fkh", "new bytes");
        let options = opts(false);
        let plan = plan_folder(
            &lib,
            &source,
            Some("fkh"),
            &[],
            &options,
            None,
            &mut HashCache::in_memory(),
            &mut |_| {},
        )
        .unwrap();
        write(lib.root(), "roms/fkh/a.fkh", "existing bytes");
        let result = apply_import(&mut lib, &plan, &mut |_| {}).unwrap();
        assert_eq!(result.failed.len(), 1);
        assert_eq!(
            fs::read(lib.root().join("roms/fkh/a.fkh")).unwrap(),
            b"existing bytes"
        );
        assert!(lib.list_games(None, None).unwrap().is_empty());
        fs::remove_file(lib.root().join("roms/fkh/a.fkh")).unwrap();
        lib.conn().execute_batch("CREATE TRIGGER reject_import BEFORE INSERT ON roms BEGIN SELECT RAISE(ABORT, 'test failure'); END;").unwrap();
        let result = apply_import(&mut lib, &plan, &mut |_| {}).unwrap();
        assert_eq!(result.failed.len(), 1);
        assert!(!lib.root().join("roms/fkh/a.fkh").exists());
        assert!(lib.list_games(None, None).unwrap().is_empty());
        assert_eq!(fs::read(source.join("a.fkh")).unwrap(), b"new bytes");
    }

    #[test]
    fn dat_import_requires_the_bytes_that_were_verified_in_the_preview() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lib = Library::init(&tmp.path().join("lib")).unwrap();
        let source = tmp.path().join("source");
        write(&source, "Hello World (USA).fkh", "hello world");
        let options = opts(true);
        let plan = plan_folder(
            &lib,
            &source,
            Some("fkh"),
            &[],
            &options,
            Some(&dat()),
            &mut HashCache::in_memory(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(plan.changes(), 1);
        write(&source, "Hello World (USA).fkh", "changed ROM");
        let result = apply_import(&mut lib, &plan, &mut |_| {}).unwrap();
        assert_eq!(result.failed.len(), 1);
        assert_eq!(result.added, 0);
        assert!(lib.list_games(None, None).unwrap().is_empty());
        assert!(!lib.root().join("roms/fkh/Hello World (USA).fkh").exists());
    }
}
