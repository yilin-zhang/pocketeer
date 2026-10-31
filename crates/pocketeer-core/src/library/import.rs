//! Bringing ROM files into the library: plan (name, match, dedupe) → apply
//! (copy/link/reference + index rows, one transaction).
//!
//! Files are read as few times as possible, because the source may be a
//! Pocket card over USB: with a DAT every file is hashed while planning (the
//! DAT decides its name); without one, only a file whose name and size already
//! exist in the library is read at plan time (to tell "already have it" from
//! "different file, same name"), and everything else is hashed while it is
//! copied. Content duplicates found that late are dropped again.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{
    Error, ErrorInfo, Result, fsutil,
    progress::{Event, Progress},
    roms::{
        dat::Dat,
        hash::{HashCache, Hashes, copy_and_hash, hash_file},
        verify::{Candidate, hash_candidate, match_key, scan_source},
    },
};

use super::{
    Library,
    db::{self, NewRom},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportMode {
    /// Copy the file into `roms/<platform>/`.
    Copy,
    /// Index the file where it is; nothing is copied.
    Reference,
    /// Hard-link into the library (falls back to copy across volumes).
    Link,
}

impl ImportMode {
    /// The string stored in the `roms.mode` column (same as the serde form).
    pub fn as_str(self) -> &'static str {
        match self {
            ImportMode::Copy => "copy",
            ImportMode::Reference => "reference",
            ImportMode::Link => "link",
        }
    }

    pub fn parse(s: &str) -> Option<ImportMode> {
        match s {
            "copy" => Some(ImportMode::Copy),
            "reference" => Some(ImportMode::Reference),
            "link" => Some(ImportMode::Link),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportOptions {
    pub platform: String,
    /// Display name for the platform, if known (from the inventory).
    pub platform_name: Option<String>,
    pub mode: ImportMode,
    /// With a DAT: skip files it doesn't know instead of importing them unverified.
    pub dat_only: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ImportAction {
    Add {
        source: PathBuf,
        #[serde(skip_serializing_if = "Option::is_none")]
        inner: Option<String>,
        game: String,
        rom: String,
        /// Managed path relative to the library root; `None` for references.
        #[serde(skip_serializing_if = "Option::is_none")]
        dest: Option<PathBuf>,
        size: u64,
        /// Known when the file was hashed while planning; otherwise computed
        /// during the copy.
        #[serde(skip_serializing_if = "Option::is_none")]
        crc32: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        sha1: Option<String>,
        verified: bool,
        mode: ImportMode,
        /// A standard (No-Intro style) name this file probably is, when the
        /// DAT didn't name it; see [`suggest_names`].
        #[serde(skip_serializing_if = "Option::is_none")]
        suggested: Option<String>,
    },
    Skip {
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
    pub platform: String,
    pub mode: ImportMode,
    pub actions: Vec<ImportAction>,
    pub summary: ImportSummary,
}

impl ImportPlan {
    pub fn changes(&self) -> usize {
        self.summary.add
    }
}

/// Game name for a file the DAT doesn't know: the file stem.
pub fn fallback_game_name(rom_name: &str) -> String {
    crate::roms::rename::stem(rom_name).to_string()
}

/// Plans importing every ROM-ish file under `dir` (recursively, zips included).
/// `source` is a folder of ROMs (scanned recursively) or one ROM / zip.
pub fn plan_import(
    lib: &Library,
    source: &Path,
    opts: &ImportOptions,
    dat: Option<&Dat>,
    cache: &mut HashCache,
    progress: Progress<'_>,
) -> Result<ImportPlan> {
    let (dir, candidates) = scan_source(source)?;
    plan_import_candidates(lib, &dir, &candidates, opts, dat, cache, progress)
}

/// Plans importing specific files, given relative to `dir` (the card's root
/// when importing from a device).
pub fn plan_import_candidates(
    lib: &Library,
    dir: &Path,
    candidates: &[Candidate],
    opts: &ImportOptions,
    dat: Option<&Dat>,
    cache: &mut HashCache,
    progress: Progress<'_>,
) -> Result<ImportPlan> {
    let total = candidates.len();
    progress(Event::Status(format!(
        "{} {total} file(s)",
        if dat.is_some() { "Hashing" } else { "Planning" }
    )));

    let mut plan = ImportPlan {
        dir: dir.to_path_buf(),
        platform: opts.platform.clone(),
        mode: opts.mode,
        actions: Vec::new(),
        summary: ImportSummary::default(),
    };
    let mut claimed: Vec<(String, PathBuf)> = Vec::new();

    for (i, c) in candidates.iter().enumerate() {
        progress(Event::Step { done: i + 1, total });
        let skip = |plan: &mut ImportPlan, reason: ImportSkip| {
            plan.summary.skip += 1;
            plan.actions.push(ImportAction::Skip {
                source: c.file.location(),
                reason,
            });
        };

        // Read the file now only when its contents decide something here.
        let same_name = db::find_rom_by_name(lib.conn(), &opts.platform, &c.file.display_name())?;
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
            (Some(d), Some(h)) => match_key(d, h).map(|k| d.get(k)),
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
            && let Some(existing) = db::find_rom_by_sha1(lib.conn(), &opts.platform, &h.sha1)?
        {
            skip(
                &mut plan,
                ImportSkip::AlreadyInLibrary { rom: existing.name },
            );
            continue;
        }
        if let Some(existing) = db::find_rom_by_name(lib.conn(), &opts.platform, &rom)? {
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

        // References can't point inside a zip; those are always extracted.
        let mode = if opts.mode == ImportMode::Reference && c.file.inner.is_some() {
            ImportMode::Copy
        } else {
            opts.mode
        };
        let dest =
            (mode != ImportMode::Reference).then(|| Library::rom_rel_path(&opts.platform, &rom));

        let size = hashes.as_ref().map(|h| h.size).unwrap_or(c.size);
        plan.summary.add += 1;
        plan.summary.bytes += size;
        if verified {
            plan.summary.verified += 1;
        } else {
            plan.summary.unverified += 1;
        }
        plan.actions.push(ImportAction::Add {
            source: dir.join(&c.file.path),
            inner: c.file.inner.clone(),
            game,
            rom,
            dest,
            size,
            crc32: hashes.as_ref().map(Hashes::crc32_hex),
            sha1: hashes.map(|h| h.sha1),
            verified,
            mode,
            suggested: None,
        });
    }
    cache.save_best_effort();
    Ok(plan)
}

/// Fills `suggested` on every unverified add whose name matches a standard
/// name in `standard` (see [`super::names::suggest`]). Returns how many.
pub fn suggest_names(plan: &mut ImportPlan, standard: &[String]) -> usize {
    let mut n = 0;
    for a in &mut plan.actions {
        if let ImportAction::Add {
            rom,
            verified: false,
            suggested,
            ..
        } = a
            && let Some(s) = super::names::suggest(rom, standard)
        {
            *suggested = Some(format!("{s}{}", crate::roms::rename::extension(rom)));
            n += 1;
        }
    }
    n
}

/// Renames planned adds to their suggestion where `accept(current name)`
/// says so: ROM name, game name and managed destination move together. A
/// suggestion that would collide with another planned or library name is left
/// alone.
pub fn apply_suggestions(
    lib: &Library,
    plan: &mut ImportPlan,
    accept: impl Fn(&str) -> bool,
) -> Result<usize> {
    let planned: Vec<String> = plan
        .actions
        .iter()
        .filter_map(|a| match a {
            ImportAction::Add { rom, .. } => Some(rom.clone()),
            _ => None,
        })
        .collect();
    let platform = plan.platform.clone();
    let mut n = 0;
    for a in &mut plan.actions {
        let ImportAction::Add {
            rom,
            game,
            dest,
            suggested: Some(new),
            ..
        } = a
        else {
            continue;
        };
        if !accept(rom) || planned.contains(new) {
            continue;
        }
        if db::find_rom_by_name(lib.conn(), &platform, new)?.is_some() {
            continue;
        }
        *game = fallback_game_name(new);
        if dest.is_some() {
            *dest = Some(Library::rom_rel_path(&platform, new));
        }
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
    opts: &ImportOptions,
    plan: &ImportPlan,
    progress: Progress<'_>,
) -> Result<ImportResult> {
    let root = lib.root().to_path_buf();
    let total = plan.changes();
    progress(Event::Status(format!("Importing {total} file(s)")));
    let mut result = ImportResult::default();

    db::ensure_platform(lib.conn(), &opts.platform, opts.platform_name.as_deref())?;
    let tx = lib
        .conn_mut()
        .transaction()
        .map_err(|e| Error::db(e, "begin import"))?;

    let mut done = 0;
    for action in &plan.actions {
        let ImportAction::Add {
            source,
            inner,
            game,
            rom,
            dest,
            size,
            crc32,
            sha1,
            verified,
            mode,
            ..
        } = action
        else {
            continue;
        };
        let placed: Result<Option<Hashes>> = match (mode, dest) {
            (ImportMode::Reference, _) | (_, None) => Ok(None),
            (_, Some(dest)) => place_file(&root, source, inner.as_deref(), dest, *mode),
        };
        let indexed = placed.and_then(|placed_hashes| {
            // Hashes from planning, from the copy, or (links/references) from the file.
            let (crc32, sha1, size, late) = match (crc32, sha1) {
                (Some(c), Some(h)) => (c.clone(), h.clone(), *size, false),
                _ => {
                    let h = match placed_hashes {
                        Some(h) => h,
                        None => match dest {
                            Some(d) => hash_file(&root.join(d))?,
                            None => hash_file(source)?,
                        },
                    };
                    (h.crc32_hex(), h.sha1, h.size, true)
                }
            };
            if late && let Some(existing) = db::find_rom_by_sha1(&tx, &opts.platform, &sha1)? {
                return Ok(Indexed::Duplicate(existing.name));
            }
            let game_id = db::upsert_game(&tx, &opts.platform, game)?;
            db::insert_rom(
                &tx,
                &NewRom {
                    game_id,
                    name: rom,
                    size,
                    crc32: &crc32,
                    sha1: &sha1,
                    verified: *verified,
                    rel_path: dest.as_ref().and_then(|d| d.to_str()),
                    source_path: (*mode == ImportMode::Reference)
                        .then_some(source.to_str())
                        .flatten(),
                    mode: *mode,
                },
            )?;
            Ok(Indexed::Added(size))
        });
        match indexed {
            Ok(Indexed::Added(size)) => {
                result.added += 1;
                result.bytes += size;
            }
            Ok(Indexed::Duplicate(existing)) => {
                if let Some(d) = dest {
                    let _ = std::fs::remove_file(root.join(d));
                }
                result.duplicates.push(ImportDuplicate {
                    source: source.clone(),
                    existing,
                });
            }
            Err(e) => {
                // Don't leave a managed file without an index row.
                if let Some(d) = dest {
                    let _ = std::fs::remove_file(root.join(d));
                }
                result.failed.push(ImportFailure {
                    source: source.clone(),
                    error: e.to_info(),
                });
            }
        }
        done += 1;
        progress(Event::Step { done, total });
    }
    tx.commit().map_err(|e| Error::db(e, "commit import"))?;
    Ok(result)
}

/// Copies, extracts or hard-links `source` to `<root>/<dest>` via a temp
/// file. Copies and extractions hash the bytes on the way through and return
/// them; a hard link returns `None` (the caller hashes the linked file).
fn place_file(
    root: &Path,
    source: &Path,
    inner: Option<&str>,
    dest: &Path,
    mode: ImportMode,
) -> Result<Option<Hashes>> {
    let full = root.join(dest);
    let parent = full
        .parent()
        .ok_or_else(|| Error::NotADirectory { path: full.clone() })?;
    std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    if full.exists() {
        return Err(Error::AlreadyExists { path: full });
    }
    if mode == ImportMode::Link && inner.is_none() && std::fs::hard_link(source, &full).is_ok() {
        return Ok(None);
    }
    let mut hashes = None;
    fsutil::write_via_temp(&full, |out| {
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
    Ok(hashes)
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

    fn opts(mode: ImportMode, dat_only: bool) -> ImportOptions {
        ImportOptions {
            platform: "fkh".into(),
            platform_name: Some("Fake Handheld".into()),
            mode,
            dat_only,
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
                    mode,
                    ..
                } => {
                    format!("add {rom} [{game}] v={verified} {}", mode.as_str())
                }
                ImportAction::Skip { source, reason } => format!("skip {source} {reason:?}"),
            })
            .collect()
    }

    #[test]
    fn copy_import_with_dat_then_dedupes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lib = Library::init(&tmp.path().join("lib")).unwrap();
        let src = library_dir();
        let dat = dat();
        let o = opts(ImportMode::Copy, false);
        let mut cache = HashCache::in_memory();
        let plan = plan_import(&lib, src.path(), &o, Some(&dat), &mut cache, &mut |_| {}).unwrap();
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

        let r = apply_import(&mut lib, &o, &plan, &mut |_| {}).unwrap();
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
        let plan2 = plan_import(&lib, src.path(), &o, Some(&dat), &mut cache, &mut |_| {}).unwrap();
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
        let plan3 = plan_import(
            &lib,
            src.path(),
            &o,
            None,
            &mut HashCache::in_memory(),
            &mut |_| {},
        )
        .unwrap();
        assert!(kinds(&plan3).iter().any(|k| k.contains("NameConflict")));
    }

    #[test]
    fn dat_only_and_reference_modes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lib = Library::init(&tmp.path().join("lib")).unwrap();
        let src = library_dir();
        let dat = dat();
        let o = opts(ImportMode::Reference, true);
        let plan = plan_import(
            &lib,
            src.path(),
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
                .any(|x| x == "add Hello World (USA).fkh [Hello World (USA)] v=true reference"),
            "{k:?}"
        );

        let r = apply_import(&mut lib, &o, &plan, &mut |_| {}).unwrap();
        assert_eq!(r.added, 3);
        assert!(
            !lib.root().join("roms/fkh/Hello World (USA).fkh").exists(),
            "referenced, not copied"
        );
        let games = db::list_games(lib.conn(), Some("fkh"), Some("hello")).unwrap();
        let rom = &games[0].roms[0];
        assert_eq!(rom.mode, ImportMode::Reference);
        assert_eq!(
            lib.rom_abs_path(rom),
            src.path().join("Hello World (USA).fkh")
        );
    }

    #[test]
    fn without_a_dat_nothing_is_read_while_planning_and_duplicates_drop_late() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lib = Library::init(&tmp.path().join("lib")).unwrap();
        let src = tempfile::tempdir().unwrap();
        fs::write(src.path().join("a.fkh"), "same bytes").unwrap();
        fs::write(src.path().join("b.fkh"), "same bytes").unwrap();
        let o = opts(ImportMode::Copy, false);
        let plan = plan_import(
            &lib,
            src.path(),
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
        let r = apply_import(&mut lib, &o, &plan, &mut |_| {}).unwrap();
        assert_eq!(r.added, 1);
        assert_eq!(r.duplicates.len(), 1);
        assert_eq!(r.duplicates[0].existing, "a.fkh");
        assert!(!lib.root().join("roms/fkh/b.fkh").exists());
        let rom = &db::list_games(lib.conn(), None, None).unwrap()[0].roms[0];
        assert_eq!(rom.crc32.len(), 8, "hashed during the copy");
        assert_eq!(rom.sha1.len(), 40);
    }

    #[test]
    fn link_mode_links_or_copies() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lib = Library::init(&tmp.path().join("lib")).unwrap();
        let src = library_dir();
        let o = opts(ImportMode::Link, false);
        let plan = plan_import(
            &lib,
            src.path(),
            &o,
            None,
            &mut HashCache::in_memory(),
            &mut |_| {},
        )
        .unwrap();
        apply_import(&mut lib, &o, &plan, &mut |_| {}).unwrap();
        let managed = lib.root().join("roms/fkh/random.fkh");
        assert_eq!(fs::read_to_string(&managed).unwrap(), "not in dat");
        // Without a DAT the game name is the stem.
        let games = db::list_games(lib.conn(), None, Some("random")).unwrap();
        assert_eq!(games[0].name, "random");
    }

    #[test]
    fn fallback_names() {
        assert_eq!(fallback_game_name("Foo (USA).gba"), "Foo (USA)");
        assert_eq!(fallback_game_name("noext"), "noext");
        assert_eq!(
            fallback_game_name("weird.longextension"),
            "weird.longextension"
        );
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
        let opts = ImportOptions {
            platform: "gba".into(),
            platform_name: None,
            mode: ImportMode::Copy,
            dat_only: false,
        };
        let mut cache = HashCache::open(None);
        for (file, expected) in [("game.gba", "game.gba"), ("pack.zip", "inner.gba")] {
            let plan = plan_import(
                &lib,
                &src.path().join("Downloads").join(file),
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
}
