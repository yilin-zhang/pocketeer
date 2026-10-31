//! Copy library ROMs onto the card: plan from a list of source files, apply.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use serde::Serialize;

use crate::{
    Error, ErrorInfo, Pocket, Result, fsutil,
    progress::{Event, Progress},
};

use super::{
    FileRef,
    hash::HashCache,
    rename::{FileRename, RenameKind, apply_renames, companion_renames},
    verify::hash_ref,
};

/// One file to put on the card.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceFile {
    /// File name on the card.
    pub name: String,
    /// Absolute location of the bytes (file, or zip entry).
    pub file: FileRef,
    pub size: u64,
    /// Known crc32, so `--verify` needn't hash the source again.
    pub crc32: Option<u32>,
    /// Shown in skip reasons; usually the path relative to wherever it came from.
    pub label: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SyncAction {
    /// Copy a plain file.
    Copy {
        from: PathBuf,
        to: PathBuf,
        size: u64,
        overwrite: bool,
    },
    /// Extract one zip entry.
    Extract {
        archive: PathBuf,
        inner: String,
        to: PathBuf,
        size: u64,
        overwrite: bool,
    },
    /// A destination file with the same contents under another name: rename
    /// it in place (with its saves and save states) instead of copying.
    Rename {
        from: PathBuf,
        to: PathBuf,
        size: u64,
        companions: Vec<FileRename>,
    },
    /// Delete a destination file.
    Delete {
        path: PathBuf,
    },
    Skip {
        from: String,
        reason: SyncSkip,
    },
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SyncSkip {
    UpToDate,
    /// Two sources map to the same destination name.
    Duplicate {
        of: String,
    },
    Unreadable {
        error: ErrorInfo,
    },
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct SyncSummary {
    pub copy: usize,
    pub extract: usize,
    pub rename: usize,
    pub delete: usize,
    pub skip: usize,
    pub up_to_date: usize,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SyncPlan {
    /// Relative to the SD root.
    pub dest_dir: PathBuf,
    pub actions: Vec<SyncAction>,
    pub summary: SyncSummary,
}

impl SyncPlan {
    pub fn changes(&self) -> usize {
        self.summary.copy + self.summary.extract + self.summary.rename + self.summary.delete
    }
}

/// Keeps output (and apply order) stable: renames first, then copies/extracts
/// by destination, then deletes, then skips.
fn action_order(a: &SyncAction) -> (u8, String) {
    match a {
        SyncAction::Rename { to, .. } => (0, to.display().to_string()),
        SyncAction::Copy { to, .. } | SyncAction::Extract { to, .. } => {
            (1, to.display().to_string())
        }
        SyncAction::Delete { path } => (2, path.display().to_string()),
        SyncAction::Skip { from, .. } => (3, from.clone()),
    }
}

/// Explicit sources → card. Up to date means same size (and same crc32 with
/// `verify`). With `delete`, destination files no source claims are removed.
///
/// `renames` are `(old, new)` names the library knows about (oldest first):
/// a destination file still called `old` is renamed in place, saves included,
/// instead of being copied again and left as a stray. With `verify`, a
/// missing destination whose bytes sit there under an unknown name (same
/// size and crc32) is renamed too; that costs a read of every same-size file,
/// so it is opt-in. `protected` paths (relative to the SD root; files an
/// installed core declares, BIOS and the like) are never renamed or deleted.
#[allow(clippy::too_many_arguments)]
pub fn plan_from_sources(
    pocket: &Pocket,
    dest_rel: &Path,
    sources: &[SourceFile],
    renames: &[(String, String)],
    protected: &BTreeSet<PathBuf>,
    delete: bool,
    verify: bool,
    cache: &mut HashCache,
) -> Result<SyncPlan> {
    let dest_abs = pocket.root().join(dest_rel);
    let mut actions = Vec::new();
    let mut summary = SyncSummary::default();
    let mut claimed: BTreeMap<String, String> = BTreeMap::new();
    let source_names: BTreeSet<&str> = sources.iter().map(|s| s.name.as_str()).collect();
    let platform = platform_of(dest_rel);

    // Destination files nobody claims by name: rename candidates, then strays.
    let mut unclaimed: Vec<(String, u64)> = if dest_abs.is_dir() {
        std::fs::read_dir(&dest_abs)
            .map_err(|e| Error::io(&dest_abs, e))?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let meta = e.metadata().ok()?;
                let name = e.file_name().to_str()?.to_owned();
                (meta.is_file()
                    && !fsutil::is_hidden(&e.file_name())
                    && !source_names.contains(name.as_str())
                    && !protected.contains(&dest_rel.join(&name)))
                .then_some((name, meta.len()))
            })
            .collect()
    } else {
        Vec::new()
    };
    unclaimed.sort();

    // Known renames the card hasn't seen yet: old name present, new one absent.
    // `renamed_to` lets the loop below compare the source with the old file.
    let mut renamed_to: BTreeMap<String, String> = BTreeMap::new();
    for (old, new) in renames {
        let old_path = dest_abs.join(old);
        if !old_path.is_file() || dest_abs.join(new).exists() || renamed_to.contains_key(new) {
            continue;
        }
        if protected.contains(&dest_rel.join(old)) {
            continue;
        }
        unclaimed.retain(|(n, _)| n != old);
        let companions = platform
            .map(|p| companion_renames(pocket.root(), p, old, new))
            .unwrap_or_default();
        summary.rename += 1;
        actions.push(SyncAction::Rename {
            from: dest_rel.join(old),
            to: dest_rel.join(new),
            size: std::fs::metadata(&old_path).map(|m| m.len()).unwrap_or(0),
            companions,
        });
        renamed_to.insert(new.clone(), old.clone());
    }

    for s in sources {
        if let Some(prev) = claimed.get(&s.name) {
            summary.skip += 1;
            actions.push(SyncAction::Skip {
                from: s.label.clone(),
                reason: SyncSkip::Duplicate { of: prev.clone() },
            });
            continue;
        }
        claimed.insert(s.name.clone(), s.label.clone());

        let to = dest_rel.join(&s.name);
        // After a pending rename the bytes to compare are still under the old name.
        let existing = match renamed_to.get(&s.name) {
            Some(old) => dest_abs.join(old),
            None => dest_abs.join(&s.name),
        };
        let overwrite = existing.exists();
        if !overwrite
            && verify
            && let Some(old) = find_renamed(s, &dest_abs, &mut unclaimed, cache)
        {
            let companions = platform
                .map(|p| companion_renames(pocket.root(), p, &old, &s.name))
                .unwrap_or_default();
            summary.rename += 1;
            actions.push(SyncAction::Rename {
                from: dest_rel.join(&old),
                to,
                size: s.size,
                companions,
            });
            continue;
        }
        if overwrite {
            match up_to_date(s, &existing, verify, cache) {
                Ok(true) => {
                    summary.up_to_date += 1;
                    actions.push(SyncAction::Skip {
                        from: s.label.clone(),
                        reason: SyncSkip::UpToDate,
                    });
                    continue;
                }
                Ok(false) => {}
                Err(e) => {
                    summary.skip += 1;
                    actions.push(SyncAction::Skip {
                        from: s.label.clone(),
                        reason: SyncSkip::Unreadable { error: e.to_info() },
                    });
                    continue;
                }
            }
        }

        summary.bytes += s.size;
        match &s.file.inner {
            None => {
                summary.copy += 1;
                actions.push(SyncAction::Copy {
                    from: s.file.path.clone(),
                    to,
                    size: s.size,
                    overwrite,
                });
            }
            Some(inner) => {
                summary.extract += 1;
                actions.push(SyncAction::Extract {
                    archive: s.file.path.clone(),
                    inner: inner.clone(),
                    to,
                    size: s.size,
                    overwrite,
                });
            }
        }
    }

    if delete {
        let stray: Vec<PathBuf> = unclaimed
            .into_iter()
            .map(|(n, _)| dest_rel.join(n))
            .collect();
        summary.delete += stray.len();
        actions.extend(stray.into_iter().map(|path| SyncAction::Delete { path }));
    }

    actions.sort_by_key(action_order);
    Ok(SyncPlan {
        dest_dir: dest_rel.to_path_buf(),
        actions,
        summary,
    })
}

/// `Assets/<platform>/…` → `platform`.
fn platform_of(dest_rel: &Path) -> Option<&str> {
    let mut it = dest_rel.components();
    match (it.next(), it.next()) {
        (Some(a), Some(b)) if a.as_os_str() == "Assets" => b.as_os_str().to_str(),
        _ => None,
    }
}

/// An unclaimed destination file with the same size and crc32 as `s`. Found
/// files are taken out of `unclaimed`. Unreadable candidates are ignored.
fn find_renamed(
    s: &SourceFile,
    dest_abs: &Path,
    unclaimed: &mut Vec<(String, u64)>,
    cache: &mut HashCache,
) -> Option<String> {
    if !unclaimed.iter().any(|(_, size)| *size == s.size) {
        return None;
    }
    let src_crc = source_crc(s, cache).ok()?;
    let pos = unclaimed.iter().position(|(name, size)| {
        *size == s.size
            && cache
                .file(&dest_abs.join(name))
                .is_ok_and(|h| h.crc32 == src_crc)
    })?;
    Some(unclaimed.remove(pos).0)
}

fn source_crc(s: &SourceFile, cache: &mut HashCache) -> Result<u32> {
    match s.crc32 {
        Some(c) => Ok(c),
        None => Ok(hash_ref(&s.file, cache)?.crc32),
    }
}

fn up_to_date(
    s: &SourceFile,
    existing: &Path,
    verify: bool,
    cache: &mut HashCache,
) -> Result<bool> {
    let meta = std::fs::metadata(existing).map_err(|e| Error::io(existing, e))?;
    if meta.len() != s.size {
        return Ok(false);
    }
    if !verify {
        return Ok(true);
    }
    let dest_crc = cache.file(existing)?.crc32;
    Ok(dest_crc == source_crc(s, cache)?)
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct SyncResult {
    pub copied: usize,
    pub extracted: usize,
    pub renamed: usize,
    pub deleted: usize,
    pub bytes: u64,
    pub failed: Vec<SyncFailure>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SyncFailure {
    pub path: PathBuf,
    pub error: ErrorInfo,
}

/// Copies/extracts via a temp file next to the destination, then renames, so
/// an unplugged card never leaves a half-written ROM under its real name.
/// Overwritten files are replaced in place; deleted files are gone (the
/// library holds the copies).
pub fn apply_sync(pocket: &Pocket, plan: &SyncPlan, progress: Progress<'_>) -> Result<SyncResult> {
    let root = pocket.root();
    let mut result = SyncResult::default();
    let total = plan.changes();
    let mut done = 0;
    progress(Event::Status(format!("Syncing {total} file(s)")));

    for action in &plan.actions {
        let outcome: Result<()> = match action {
            SyncAction::Skip { .. } => continue,
            SyncAction::Copy { from, to, .. } => {
                fsutil::copy_file(from, &root.join(to)).inspect(|_| result.copied += 1)
            }
            SyncAction::Extract {
                archive, inner, to, ..
            } => fsutil::extract_zip_entry(archive, inner, &root.join(to))
                .inspect(|_| result.extracted += 1),
            SyncAction::Rename {
                from,
                to,
                companions,
                ..
            } => {
                let mut all = vec![FileRename {
                    from: from.clone(),
                    to: to.clone(),
                    kind: RenameKind::Rom,
                }];
                all.extend(companions.iter().cloned());
                apply_renames(root, &all).map(|_| result.renamed += 1)
            }
            SyncAction::Delete { path } => {
                let full = root.join(path);
                std::fs::remove_file(&full)
                    .map_err(|e| Error::io(&full, e))
                    .inspect(|_| result.deleted += 1)
            }
        };
        match (outcome, action) {
            (Ok(()), SyncAction::Copy { size, .. } | SyncAction::Extract { size, .. }) => {
                result.bytes += size
            }
            (Ok(()), _) => {}
            (Err(e), a) => result.failed.push(SyncFailure {
                path: match a {
                    SyncAction::Copy { to, .. }
                    | SyncAction::Extract { to, .. }
                    | SyncAction::Rename { to, .. } => to.clone(),
                    SyncAction::Delete { path } => path.clone(),
                    SyncAction::Skip { .. } => PathBuf::new(),
                },
                error: e.to_info(),
            }),
        }
        done += 1;
        progress(Event::Step { done, total });
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::super::{
        dat::Dat,
        verify::{hash_candidate, match_key, scan},
    };
    use super::*;

    #[derive(Debug, Clone, Default)]
    pub struct SyncOptions {
        pub platform: String,
        /// `Assets/<platform>/<core>/` instead of `common/`.
        pub core: Option<String>,
        /// Compare crc32 of existing destination files, not just size.
        pub verify_hashes: bool,
        /// Delete destination files that no source maps to.
        pub delete: bool,
    }

    impl SyncOptions {
        pub fn dest_dir(&self) -> PathBuf {
            Pocket::assets_rel(&self.platform, self.core.as_deref().unwrap_or("common"))
        }
    }

    /// Test fixture: a folder of ROMs → sources (DAT names when a DAT is given) → plan.
    pub fn plan_sync(
        pocket: &Pocket,
        src: &Path,
        opts: &SyncOptions,
        dat: Option<&Dat>,
        protected: &BTreeSet<PathBuf>,
        cache: &mut HashCache,
        progress: Progress<'_>,
    ) -> Result<SyncPlan> {
        let candidates = scan(src)?;
        progress(Event::Status(format!(
            "Planning {} file(s)",
            candidates.len()
        )));

        let mut sources = Vec::new();
        for (i, c) in candidates.iter().enumerate() {
            progress(Event::Step {
                done: i + 1,
                total: candidates.len(),
            });
            let (name, crc32) = match dat {
                Some(dat) => match hash_candidate(src, c, cache) {
                    Ok(h) => match match_key(dat, &h) {
                        Some(k) => (dat.get(k).1.name.clone(), Some(h.crc32)),
                        None => continue,
                    },
                    Err(_) => continue,
                },
                None => (c.file.display_name(), None),
            };
            sources.push(SourceFile {
                name,
                file: c.file.under(src),
                size: c.size,
                crc32,
                label: c.file.location(),
            });
        }

        plan_from_sources(
            pocket,
            &opts.dest_dir(),
            &sources,
            &[],
            protected,
            opts.delete,
            opts.verify_hashes,
            cache,
        )
    }

    use std::fs;

    use crate::testutil::{build_zip, dat, sd, write};

    fn library() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let w = |rel: &str, body: &[u8]| write(tmp.path(), rel, body);
        w("Hello World (USA).fkh", b"hello world");
        w(
            "sub/quickfox.fkh",
            b"The quick brown fox jumps over the lazy dog",
        );
        w("random.fkh", b"not in dat");
        w("other.txt", b"ignored sidecar");
        w(
            "discs.zip",
            &build_zip(&[("Two Discs (Japan) (Disc 1).bin", b"abc")]),
        );
        tmp
    }

    fn kinds(plan: &SyncPlan) -> Vec<String> {
        plan.actions
            .iter()
            .map(|a| match a {
                SyncAction::Copy { to, overwrite, .. } => format!(
                    "copy {}{}",
                    to.display(),
                    if *overwrite { " (overwrite)" } else { "" }
                ),
                SyncAction::Extract { to, .. } => format!("extract {}", to.display()),
                SyncAction::Rename {
                    from,
                    to,
                    companions,
                    ..
                } => format!(
                    "rename {} -> {} (+{})",
                    from.display(),
                    to.display(),
                    companions.len()
                ),
                SyncAction::Delete { path } => format!("delete {}", path.display()),
                SyncAction::Skip { from, reason } => format!("skip {from} {reason:?}"),
            })
            .collect()
    }

    #[test]
    fn plain_sync_copies_everything_once() {
        let (_sd, pocket) = sd();
        let lib = library();
        let opts = SyncOptions {
            platform: "fkh".into(),
            ..Default::default()
        };
        let mut cache = HashCache::in_memory();
        let plan = plan_sync(
            &pocket,
            lib.path(),
            &opts,
            None,
            &BTreeSet::new(),
            &mut cache,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(plan.dest_dir, Path::new("Assets/fkh/common"));
        assert_eq!(
            kinds(&plan),
            [
                "copy Assets/fkh/common/Hello World (USA).fkh",
                "extract Assets/fkh/common/Two Discs (Japan) (Disc 1).bin",
                "copy Assets/fkh/common/quickfox.fkh",
                "copy Assets/fkh/common/random.fkh",
            ]
        );
        assert_eq!(plan.summary.bytes, 11 + 3 + 10 + 43);

        let r = apply_sync(&pocket, &plan, &mut |_| {}).unwrap();
        assert_eq!((r.copied, r.extracted, r.deleted), (3, 1, 0));
        assert!(r.failed.is_empty());
        assert_eq!(
            fs::read_to_string(
                pocket
                    .root()
                    .join("Assets/fkh/common/Two Discs (Japan) (Disc 1).bin")
            )
            .unwrap(),
            "abc"
        );
        assert!(
            fs::read_dir(pocket.root().join("Assets/fkh/common"))
                .unwrap()
                .all(|e| !e
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".pocketeer-tmp"))
        );

        let plan2 = plan_sync(
            &pocket,
            lib.path(),
            &opts,
            None,
            &BTreeSet::new(),
            &mut cache,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(plan2.changes(), 0);
        assert_eq!(plan2.summary.up_to_date, 4);
    }

    #[test]
    fn dat_sync_renames_filters_and_deletes_strays() {
        let (_sd, pocket) = sd();
        let lib = library();
        let dat = dat();
        let common = pocket.root().join("Assets/fkh/common");
        fs::create_dir_all(&common).unwrap();
        fs::write(common.join("stray.fkh"), "old").unwrap();
        // Same size, different content.
        fs::write(common.join("Hello World (USA).fkh"), "hello worlD").unwrap();

        let opts = SyncOptions {
            platform: "fkh".into(),
            delete: true,
            verify_hashes: true,
            ..Default::default()
        };
        let mut cache = HashCache::in_memory();
        let plan = plan_sync(
            &pocket,
            lib.path(),
            &opts,
            Some(&dat),
            &BTreeSet::new(),
            &mut cache,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(
            kinds(&plan),
            [
                "copy Assets/fkh/common/Hello World (USA).fkh (overwrite)",
                "copy Assets/fkh/common/Quick Fox (Europe).fkh",
                "extract Assets/fkh/common/Two Discs (Japan) (Disc 1).bin",
                "delete Assets/fkh/common/stray.fkh",
            ]
        );
        let r = apply_sync(&pocket, &plan, &mut |_| {}).unwrap();
        assert_eq!((r.copied, r.extracted, r.deleted), (2, 1, 1));
        assert!(
            !pocket.root().join(".pocketeer").exists(),
            "nothing of ours lingers on the card"
        );
        assert_eq!(
            fs::read_to_string(common.join("Hello World (USA).fkh")).unwrap(),
            "hello world"
        );
        assert!(!common.join("stray.fkh").exists());

        // Without --verify the same-size stale copy counts as up to date.
        fs::write(common.join("Hello World (USA).fkh"), "hello worlD").unwrap();
        let opts2 = SyncOptions {
            verify_hashes: false,
            ..opts
        };
        let plan2 = plan_sync(
            &pocket,
            lib.path(),
            &opts2,
            Some(&dat),
            &BTreeSet::new(),
            &mut cache,
            &mut |_| {},
        )
        .unwrap();
        assert!(
            kinds(&plan2)
                .iter()
                .any(|k| k == "skip Hello World (USA).fkh UpToDate")
        );
    }

    #[test]
    fn renamed_files_are_renamed_in_place_with_their_saves() {
        let (_sd, pocket) = sd();
        let lib = tempfile::tempdir().unwrap();
        fs::write(lib.path().join("New Name (USA).fkh"), "hello world").unwrap();
        fs::write(lib.path().join("Other.fkh"), "hello worlD").unwrap();
        let w = |rel: &str, body: &str| write(pocket.root(), rel, body);
        // Same size as both sources; only one matches by crc. Content-based
        // detection is opt-in (`verify`) because it reads every same-size file.
        w("Assets/fkh/common/old name.fkh", "hello world");
        w("Assets/fkh/common/unrelated.fkh", "hello WORLD");
        w("Saves/fkh/common/old name.sav", "save");
        w("Memories/Save States/fkh/common/old name.0.sta", "state");
        w("Saves/fkh/common/old name two.sav", "not mine");

        let opts = SyncOptions {
            platform: "fkh".into(),
            delete: true,
            verify_hashes: true,
            ..Default::default()
        };
        let mut cache = HashCache::in_memory();
        let plan = plan_sync(
            &pocket,
            lib.path(),
            &opts,
            None,
            &BTreeSet::new(),
            &mut cache,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(
            kinds(&plan),
            [
                "rename Assets/fkh/common/old name.fkh -> Assets/fkh/common/New Name (USA).fkh (+2)",
                "copy Assets/fkh/common/Other.fkh",
                "delete Assets/fkh/common/unrelated.fkh",
            ]
        );
        assert_eq!(plan.summary.rename, 1);
        assert_eq!(plan.changes(), 3);

        let r = apply_sync(&pocket, &plan, &mut |_| {}).unwrap();
        assert_eq!((r.copied, r.renamed, r.deleted), (1, 1, 1));
        assert!(r.failed.is_empty(), "{:?}", r.failed);
        let root = pocket.root();
        assert_eq!(
            fs::read_to_string(root.join("Assets/fkh/common/New Name (USA).fkh")).unwrap(),
            "hello world"
        );
        assert!(!root.join("Assets/fkh/common/old name.fkh").exists());
        assert!(root.join("Saves/fkh/common/New Name (USA).sav").is_file());
        assert!(
            root.join("Memories/Save States/fkh/common/New Name (USA).0.sta")
                .is_file()
        );
        assert!(root.join("Saves/fkh/common/old name two.sav").is_file());

        let plan2 = plan_sync(
            &pocket,
            lib.path(),
            &opts,
            None,
            &BTreeSet::new(),
            &mut cache,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(plan2.changes(), 0);
    }

    #[test]
    fn known_renames_are_applied_without_reading_files() {
        let (_sd, pocket) = sd();
        let lib = tempfile::tempdir().unwrap();
        fs::write(lib.path().join("New.fkh"), "hello world").unwrap();
        let w = |rel: &str, body: &str| write(pocket.root(), rel, body);
        w("Assets/fkh/common/Old.fkh", "hello world");
        w("Saves/fkh/common/Old.sav", "save");
        let sources = vec![SourceFile {
            name: "New.fkh".into(),
            file: FileRef::plain(lib.path().join("New.fkh")),
            size: 11,
            crc32: None,
            label: "New.fkh".into(),
        }];
        let renames = vec![("Old.fkh".to_string(), "New.fkh".to_string())];
        let plan = plan_from_sources(
            &pocket,
            Path::new("Assets/fkh/common"),
            &sources,
            &renames,
            &BTreeSet::new(),
            true,
            false,
            &mut HashCache::in_memory(),
        )
        .unwrap();
        assert_eq!(
            kinds(&plan),
            [
                "rename Assets/fkh/common/Old.fkh -> Assets/fkh/common/New.fkh (+1)",
                "skip New.fkh UpToDate",
            ]
        );
        let r = apply_sync(&pocket, &plan, &mut |_| {}).unwrap();
        assert_eq!(r.renamed, 1);
        assert!(pocket.root().join("Assets/fkh/common/New.fkh").is_file());
        assert!(pocket.root().join("Saves/fkh/common/New.sav").is_file());

        // Already applied: nothing left to do, and no stray.
        let plan2 = plan_from_sources(
            &pocket,
            Path::new("Assets/fkh/common"),
            &sources,
            &renames,
            &BTreeSet::new(),
            true,
            false,
            &mut HashCache::in_memory(),
        )
        .unwrap();
        assert_eq!(plan2.changes(), 0);
    }

    #[test]
    fn files_a_core_declares_are_never_strays() {
        let (_sd, pocket) = sd();
        let root = pocket.root();
        let w = |rel: &str, body: &str| write(root, rel, body);
        w(
            "Cores/x.Core/core.json",
            r#"{"core":{"magic":"APF_VER_1","metadata":{"platform_ids":["fkh"],"shortname":"X","description":"","author":"x","url":"","version":"1","date_release":"2024-01-01"},"framework":{"target_product":"Analogue Pocket","version_required":"1.1","sleep_supported":false,"dock":{"supported":false,"analog_output":false},"hardware":{"link_port":false,"cartridge_adapter":-1}},"cores":[{"name":"x","id":0,"filename":"x.rbf_r"}]}}"#,
        );
        w(
            "Cores/x.Core/data.json",
            r#"{"data":{"magic":"APF_VER_1","data_slots":[{"id":0,"name":"BIOS","required":true,"parameters":"0x100","filename":"fkh_bios.bin"}]}}"#,
        );
        w("Assets/fkh/common/fkh_bios.bin", "hello world");
        w("Assets/fkh/common/stray.fkh", "zzz");
        let lib = tempfile::tempdir().unwrap();
        // Same bytes as the BIOS: must not be treated as a renamed copy of it.
        fs::write(lib.path().join("Game.fkh"), "hello world").unwrap();

        let opts = SyncOptions {
            platform: "fkh".into(),
            delete: true,
            ..Default::default()
        };
        let plan = plan_sync(
            &pocket,
            lib.path(),
            &opts,
            None,
            &crate::required::protected_paths(&pocket).unwrap(),
            &mut HashCache::in_memory(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(
            kinds(&plan),
            [
                "copy Assets/fkh/common/Game.fkh",
                "delete Assets/fkh/common/stray.fkh",
            ]
        );
    }

    #[test]
    fn core_specific_destination_and_duplicates() {
        let (_sd, pocket) = sd();
        let lib = tempfile::tempdir().unwrap();
        fs::create_dir_all(lib.path().join("a")).unwrap();
        fs::create_dir_all(lib.path().join("b")).unwrap();
        fs::write(lib.path().join("a/same.fkh"), "1").unwrap();
        fs::write(lib.path().join("b/same.fkh"), "22").unwrap();
        let opts = SyncOptions {
            platform: "fkh".into(),
            core: Some("x.Core".into()),
            ..Default::default()
        };
        let plan = plan_sync(
            &pocket,
            lib.path(),
            &opts,
            None,
            &BTreeSet::new(),
            &mut HashCache::in_memory(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(plan.dest_dir, Path::new("Assets/fkh/x.Core"));
        assert_eq!(
            kinds(&plan),
            [
                "copy Assets/fkh/x.Core/same.fkh",
                "skip b/same.fkh Duplicate { of: \"a/same.fkh\" }",
            ]
        );
    }

    #[test]
    fn sources_with_known_crc_verify_without_rehashing_source() {
        let (_sd, pocket) = sd();
        let common = pocket.root().join("Assets/fkh/common");
        fs::create_dir_all(&common).unwrap();
        fs::write(common.join("a.fkh"), "hello world").unwrap();
        let src = tempfile::tempdir().unwrap();
        fs::write(src.path().join("a.fkh"), "hello world").unwrap();
        let sources = vec![SourceFile {
            name: "a.fkh".into(),
            file: FileRef::plain(src.path().join("a.fkh")),
            size: 11,
            crc32: Some(0x0d4a1185),
            label: "a.fkh".into(),
        }];
        let plan = plan_from_sources(
            &pocket,
            Path::new("Assets/fkh/common"),
            &sources,
            &[],
            &BTreeSet::new(),
            false,
            true,
            &mut HashCache::in_memory(),
        )
        .unwrap();
        assert_eq!(plan.summary.up_to_date, 1);
        // Wrong recorded crc → treated as changed.
        let mut bad = sources.clone();
        bad[0].crc32 = Some(1);
        let plan = plan_from_sources(
            &pocket,
            Path::new("Assets/fkh/common"),
            &bad,
            &[],
            &BTreeSet::new(),
            false,
            true,
            &mut HashCache::in_memory(),
        )
        .unwrap();
        assert_eq!(plan.summary.copy, 1);
    }
}
