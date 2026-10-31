//! Save files (`Saves/`) and save states (`Memories/Save States/`) between the
//! card and the library, with history.
//!
//! The library keeps one *current* copy per file at `saves/<same path as on
//! the card>` and every superseded copy under `saves/.history/`. Sync is
//! newest-wins by modification time; the loser is never discarded.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::{
    Error, ErrorInfo, Pocket, Result,
    fsutil::{self, mtime_secs, set_mtime, size_of},
    progress::{Event, Progress},
    roms::hash::HashCache,
};

use super::{Library, db, device};

/// Card folders that hold saves. Relative paths are kept verbatim.
pub const ROOTS: [&str; 2] = ["Saves", "Memories/Save States"];

/// FAT32 stores mtimes with 2-second precision.
const MTIME_TOLERANCE: i64 = 2;

pub(crate) const MIGRATION_SAVES: &str = "
    CREATE TABLE saves (
        rel_path   TEXT PRIMARY KEY,
        size       INTEGER NOT NULL,
        sha1       TEXT NOT NULL,
        mtime      INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        device_id  TEXT
    );
    CREATE TABLE save_versions (
        id          INTEGER PRIMARY KEY,
        rel_path    TEXT NOT NULL,
        size        INTEGER NOT NULL,
        sha1        TEXT NOT NULL,
        mtime       INTEGER NOT NULL,
        stored_path TEXT NOT NULL,
        created_at  INTEGER NOT NULL
    );
    CREATE INDEX save_versions_rel ON save_versions(rel_path);
";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Both,
    Pull,
    Push,
}

#[derive(Debug, Clone, Copy)]
pub struct SavesOptions {
    pub direction: Direction,
    pub include_states: bool,
}

impl Default for SavesOptions {
    fn default() -> Self {
        SavesOptions {
            direction: Direction::Both,
            include_states: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SaveAction {
    /// Card → library.
    Pull {
        rel: PathBuf,
        size: u64,
        reason: SaveReason,
    },
    /// Library → card.
    Push {
        rel: PathBuf,
        size: u64,
        reason: SaveReason,
    },
    Skip {
        rel: PathBuf,
        reason: SaveSkip,
    },
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SaveReason {
    OnlyOnCard,
    OnlyInLibrary,
    CardNewer,
    LibraryNewer,
    /// Same mtime, different bytes: the card wins (that's where you played).
    Tie,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SaveSkip {
    UpToDate,
    /// Would go the other way than `--pull` / `--push` allows.
    DirectionExcluded,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct SavesSummary {
    pub pull: usize,
    pub push: usize,
    pub up_to_date: usize,
    pub excluded: usize,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SavesPlan {
    pub direction: Direction,
    pub actions: Vec<SaveAction>,
    pub summary: SavesSummary,
}

impl SavesPlan {
    pub fn changes(&self) -> usize {
        self.summary.pull + self.summary.push
    }
}

/// Save files on the card, as paths relative to the SD root.
pub fn card_files(pocket: &Pocket, include_states: bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in ROOTS {
        if root != "Saves" && !include_states {
            continue;
        }
        let dir = pocket.root().join(root);
        if !dir.is_dir() {
            continue;
        }
        for path in fsutil::visible_files(&dir) {
            if let Ok(rel) = path.strip_prefix(pocket.root()) {
                out.push(rel.to_path_buf());
            }
        }
    }
    out.sort();
    out
}

fn lib_current(lib: &Library, rel: &Path) -> PathBuf {
    lib.root().join("saves").join(rel)
}

fn in_scope(rel: &Path, include_states: bool) -> bool {
    include_states || rel.starts_with("Saves")
}

pub fn plan(
    lib: &Library,
    pocket: &Pocket,
    opts: SavesOptions,
    cache: &mut HashCache,
    progress: Progress<'_>,
) -> Result<SavesPlan> {
    let on_card = card_files(pocket, opts.include_states);
    let in_lib: Vec<PathBuf> = list(lib)?
        .into_iter()
        .map(|s| s.rel_path)
        .filter(|r| in_scope(r, opts.include_states))
        .collect();
    let mut all: Vec<PathBuf> = on_card.iter().chain(in_lib.iter()).cloned().collect();
    all.sort();
    all.dedup();
    progress(Event::Status(format!(
        "Comparing {} save file(s)",
        all.len()
    )));

    let can_pull = opts.direction != Direction::Push;
    let can_push = opts.direction != Direction::Pull;
    let mut plan = SavesPlan {
        direction: opts.direction,
        actions: Vec::new(),
        summary: SavesSummary::default(),
    };

    for (i, rel) in all.iter().enumerate() {
        progress(Event::Step {
            done: i + 1,
            total: all.len(),
        });
        let card_path = pocket.root().join(rel);
        let lib_path = lib_current(lib, rel);
        let (card_exists, lib_exists) = (card_path.is_file(), lib_path.is_file());

        let decision: Option<(bool, SaveReason, u64)> = match (card_exists, lib_exists) {
            (true, false) => Some((true, SaveReason::OnlyOnCard, size_of(&card_path))),
            (false, true) => Some((false, SaveReason::OnlyInLibrary, size_of(&lib_path))),
            (false, false) => None,
            (true, true) => {
                let a = cache.file(&card_path)?;
                let b = cache.file(&lib_path)?;
                if a.sha1 == b.sha1 {
                    plan.summary.up_to_date += 1;
                    plan.actions.push(SaveAction::Skip {
                        rel: rel.clone(),
                        reason: SaveSkip::UpToDate,
                    });
                    continue;
                }
                let (cm, lm) = (mtime_secs(&card_path)?, mtime_secs(&lib_path)?);
                if cm > lm + MTIME_TOLERANCE {
                    Some((true, SaveReason::CardNewer, a.size))
                } else if lm > cm + MTIME_TOLERANCE {
                    Some((false, SaveReason::LibraryNewer, b.size))
                } else {
                    Some((true, SaveReason::Tie, a.size))
                }
            }
        };
        let Some((pull, reason, size)) = decision else {
            continue;
        };
        if (pull && !can_pull) || (!pull && !can_push) {
            plan.summary.excluded += 1;
            plan.actions.push(SaveAction::Skip {
                rel: rel.clone(),
                reason: SaveSkip::DirectionExcluded,
            });
            continue;
        }
        plan.summary.bytes += size;
        if pull {
            plan.summary.pull += 1;
            plan.actions.push(SaveAction::Pull {
                rel: rel.clone(),
                size,
                reason,
            });
        } else {
            plan.summary.push += 1;
            plan.actions.push(SaveAction::Push {
                rel: rel.clone(),
                size,
                reason,
            });
        }
    }
    cache.save_best_effort();
    Ok(plan)
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct SavesResult {
    pub pulled: usize,
    pub pushed: usize,
    pub versions_created: usize,
    pub bytes: u64,
    pub failed: Vec<SaveFailure>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SaveFailure {
    pub rel: PathBuf,
    pub error: ErrorInfo,
}

pub fn apply(
    lib: &mut Library,
    pocket: &Pocket,
    plan: &SavesPlan,
    cache: &mut HashCache,
    progress: Progress<'_>,
) -> Result<SavesResult> {
    let device_id = device::read_marker(pocket)?.map(|m| m.id);
    let total = plan.changes();
    progress(Event::Status(format!("Syncing {total} save file(s)")));
    let mut result = SavesResult::default();
    let mut done = 0;

    for action in &plan.actions {
        let (rel, size, outcome, counter) = match action {
            SaveAction::Skip { .. } => continue,
            SaveAction::Pull { rel, size, .. } => (
                rel,
                size,
                pull_one(lib, pocket, rel, device_id.as_deref(), cache),
                &mut result.pulled,
            ),
            SaveAction::Push { rel, size, .. } => (
                rel,
                size,
                push_one(lib, pocket, rel, cache),
                &mut result.pushed,
            ),
        };
        match outcome {
            Ok(created) => {
                *counter += 1;
                result.bytes += size;
                if created {
                    result.versions_created += 1;
                }
            }
            Err(e) => result.failed.push(SaveFailure {
                rel: rel.clone(),
                error: e.to_info(),
            }),
        }
        done += 1;
        progress(Event::Step { done, total });
    }
    cache.save_best_effort();
    Ok(result)
}

/// Copies a card file into the library as the new current; the previous
/// current (if different) becomes a version. Returns whether a version was made.
fn pull_one(
    lib: &mut Library,
    pocket: &Pocket,
    rel: &Path,
    device_id: Option<&str>,
    cache: &mut HashCache,
) -> Result<bool> {
    let card_path = pocket.root().join(rel);
    let current = lib_current(lib, rel);
    let new = cache.file(&card_path)?;
    let mtime = mtime_secs(&card_path)?;

    let mut versioned = false;
    if current.is_file() {
        let old = cache.file(&current)?;
        if old.sha1 != new.sha1 {
            archive_current(lib, rel, &current, &old.sha1, old.size)?;
            versioned = true;
        }
    }
    fsutil::copy_file(&card_path, &current)?;
    set_mtime(&current, mtime)?;
    upsert(lib.conn(), rel, new.size, &new.sha1, mtime, device_id)?;
    Ok(versioned)
}

/// Where a superseded copy of `rel` with these bytes is kept, relative to
/// the library root: `saves/.history/<rel>/<timestamp>-<sha1[..8]>-<name>`.
fn history_path(rel: &Path, sha1: &str) -> PathBuf {
    let stem = rel
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    PathBuf::from("saves/.history").join(rel).join(format!(
        "{}-{}-{}",
        fsutil::timestamp(),
        &sha1[..8.min(sha1.len())],
        stem
    ))
}

fn insert_version(
    conn: &Connection,
    rel: &Path,
    size: u64,
    sha1: &str,
    mtime: i64,
    stored_rel: &Path,
) -> Result<()> {
    conn.execute(
        "INSERT INTO save_versions (rel_path, size, sha1, mtime, stored_path, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            rel.to_string_lossy(),
            size as i64,
            sha1,
            mtime,
            stored_rel.to_string_lossy(),
            fsutil::now_secs()
        ],
    )
    .map_err(|e| Error::db(e, "insert save version"))?;
    Ok(())
}

/// Moves the current copy into `.history` and records it.
fn archive_current(lib: &Library, rel: &Path, current: &Path, sha1: &str, size: u64) -> Result<()> {
    let mtime = mtime_secs(current)?;
    let stored_rel = history_path(rel, sha1);
    let stored = lib.root().join(&stored_rel);
    if let Some(dir) = stored.parent() {
        std::fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
    }
    std::fs::rename(current, &stored).map_err(|e| Error::io(current, e))?;
    insert_version(lib.conn(), rel, size, sha1, mtime, &stored_rel)
}

/// Overwrites the card's copy with the library's current one. If the card's
/// bytes aren't anywhere in the library yet, they are copied into the
/// history first, so a push never destroys a save. Returns whether a
/// version was created.
fn push_one(lib: &Library, pocket: &Pocket, rel: &Path, cache: &mut HashCache) -> Result<bool> {
    let current = lib_current(lib, rel);
    let card_path = pocket.root().join(rel);
    let mut versioned = false;
    if card_path.is_file() {
        let old = cache.file(&card_path)?;
        let current_sha1 = cache.file(&current)?.sha1;
        if old.sha1 != current_sha1 && !is_version(lib, rel, &old.sha1)? {
            let stored_rel = history_path(rel, &old.sha1);
            let stored = lib.root().join(&stored_rel);
            fsutil::copy_file(&card_path, &stored)?;
            let mtime = mtime_secs(&card_path)?;
            set_mtime(&stored, mtime)?;
            insert_version(lib.conn(), rel, old.size, &old.sha1, mtime, &stored_rel)?;
            versioned = true;
        }
    }
    fsutil::copy_file(&current, &card_path)?;
    set_mtime(&card_path, mtime_secs(&current)?)?;
    Ok(versioned)
}

/// Whether the history already holds these bytes for `rel`.
fn is_version(lib: &Library, rel: &Path, sha1: &str) -> Result<bool> {
    let n: i64 = lib
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM save_versions WHERE rel_path = ?1 AND sha1 = ?2",
            params![rel.to_string_lossy(), sha1],
            |r| r.get(0),
        )
        .map_err(|e| Error::db(e, "is version"))?;
    Ok(n > 0)
}

fn upsert(
    conn: &Connection,
    rel: &Path,
    size: u64,
    sha1: &str,
    mtime: i64,
    device_id: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO saves (rel_path, size, sha1, mtime, updated_at, device_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(rel_path) DO UPDATE SET size = excluded.size, sha1 = excluded.sha1,
            mtime = excluded.mtime, updated_at = excluded.updated_at, device_id = excluded.device_id",
        params![rel.to_string_lossy(), size as i64, sha1, mtime, fsutil::now_secs(), device_id],
    )
    .map_err(|e| Error::db(e, "upsert save"))?;
    Ok(())
}

/// Re-keys the current row and every version of a save after its ROM was
/// renamed. Paths are card-relative, as stored.
pub fn rename_path(conn: &Connection, from: &Path, to: &Path) -> Result<()> {
    let (from, to) = (from.to_string_lossy(), to.to_string_lossy());
    for table in ["saves", "save_versions"] {
        conn.execute(
            &format!("UPDATE {table} SET rel_path = ?2 WHERE rel_path = ?1"),
            params![from, to],
        )
        .map_err(|e| Error::db(e, "rename save"))?;
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SaveEntry {
    pub rel_path: PathBuf,
    pub size: u64,
    pub sha1: String,
    /// mtime of the current copy (seconds since the epoch).
    pub mtime: i64,
    pub updated_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    pub versions: usize,
}

pub fn list(lib: &Library) -> Result<Vec<SaveEntry>> {
    db::query_rows(
        lib.conn(),
        "SELECT s.rel_path, s.size, s.sha1, s.mtime, s.updated_at, s.device_id,
                (SELECT COUNT(*) FROM save_versions v WHERE v.rel_path = s.rel_path)
         FROM saves s ORDER BY s.rel_path",
        [],
        "list saves",
        |r| {
            Ok(SaveEntry {
                rel_path: PathBuf::from(r.get::<_, String>(0)?),
                size: db::get_u64(r, 1)?,
                sha1: r.get(2)?,
                mtime: r.get(3)?,
                updated_at: r.get(4)?,
                device_id: r.get(5)?,
                versions: db::get_usize(r, 6)?,
            })
        },
    )
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SaveVersion {
    pub id: i64,
    pub rel_path: PathBuf,
    pub size: u64,
    pub sha1: String,
    pub mtime: i64,
    pub stored_path: PathBuf,
    pub created_at: i64,
}

pub fn versions(lib: &Library, rel: &Path) -> Result<Vec<SaveVersion>> {
    db::query_rows(
        lib.conn(),
        "SELECT id, rel_path, size, sha1, mtime, stored_path, created_at
         FROM save_versions WHERE rel_path = ?1 ORDER BY created_at DESC, id DESC",
        params![rel.to_string_lossy()],
        "list versions",
        version_from_row,
    )
}

fn version_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SaveVersion> {
    Ok(SaveVersion {
        id: r.get(0)?,
        rel_path: PathBuf::from(r.get::<_, String>(1)?),
        size: db::get_u64(r, 2)?,
        sha1: r.get(3)?,
        mtime: r.get(4)?,
        stored_path: PathBuf::from(r.get::<_, String>(5)?),
        created_at: r.get(6)?,
    })
}

pub fn versions_by_id(lib: &Library, version_id: i64) -> Result<SaveVersion> {
    lib.conn()
        .query_row(
            "SELECT id, rel_path, size, sha1, mtime, stored_path, created_at FROM save_versions WHERE id = ?1",
            params![version_id],
            version_from_row,
        )
        .optional()
        .map_err(|e| Error::db(e, "get version"))?
        .ok_or(Error::SaveVersionNotFound { id: version_id })
}

/// Makes a stored version the current copy (the old current becomes a
/// version). Push it to the card afterwards with `saves sync --push`.
pub fn restore(lib: &mut Library, version_id: i64, cache: &mut HashCache) -> Result<SaveVersion> {
    let v = versions_by_id(lib, version_id)?;
    let current = lib_current(lib, &v.rel_path);
    if current.is_file() {
        let old = cache.file(&current)?;
        if old.sha1 != v.sha1 {
            archive_current(lib, &v.rel_path, &current, &old.sha1, old.size)?;
        }
    }
    let stored = lib.root().join(&v.stored_path);
    fsutil::copy_file(&stored, &current)?;
    // Restored saves get a fresh mtime so the next sync pushes them.
    let now = fsutil::now_secs();
    set_mtime(&current, now)?;
    upsert(lib.conn(), &v.rel_path, v.size, &v.sha1, now, None)?;
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use crate::testutil::{sd, write_with_mtime as write};

    fn kinds(plan: &SavesPlan) -> Vec<String> {
        plan.actions
            .iter()
            .map(|a| match a {
                SaveAction::Pull { rel, reason, .. } => {
                    format!("pull {} {reason:?}", rel.display())
                }
                SaveAction::Push { rel, reason, .. } => {
                    format!("push {} {reason:?}", rel.display())
                }
                SaveAction::Skip { rel, reason } => format!("skip {} {reason:?}", rel.display()),
            })
            .collect()
    }

    #[test]
    fn newest_wins_and_history_is_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lib = Library::init(&tmp.path().join("lib")).unwrap();
        let (_sd, pocket) = sd();
        let t0 = 1_700_000_000;
        write(pocket.root(), "Saves/gba/common/zelda.sav", "save-v1", t0);
        write(pocket.root(), "Memories/Save States/gba/x.sta", "state", t0);
        write(pocket.root(), "Saves/.hidden", "no", t0);

        let mut cache = HashCache::in_memory();
        let p = plan(
            &lib,
            &pocket,
            SavesOptions::default(),
            &mut cache,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(
            kinds(&p),
            [
                "pull Memories/Save States/gba/x.sta OnlyOnCard",
                "pull Saves/gba/common/zelda.sav OnlyOnCard",
            ]
        );
        let r = apply(&mut lib, &pocket, &p, &mut cache, &mut |_| {}).unwrap();
        assert_eq!((r.pulled, r.pushed, r.versions_created), (2, 0, 0));
        assert_eq!(
            fs::read_to_string(lib.root().join("saves/Saves/gba/common/zelda.sav")).unwrap(),
            "save-v1"
        );
        assert_eq!(list(&lib).unwrap().len(), 2);

        // Nothing to do now.
        let p = plan(
            &lib,
            &pocket,
            SavesOptions::default(),
            &mut cache,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(p.changes(), 0);
        assert_eq!(p.summary.up_to_date, 2);

        // Play on the card: newer save → pulled, old copy becomes a version.
        write(
            pocket.root(),
            "Saves/gba/common/zelda.sav",
            "save-v2",
            t0 + 100,
        );
        let p = plan(
            &lib,
            &pocket,
            SavesOptions::default(),
            &mut HashCache::in_memory(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(kinds(&p)[1], "pull Saves/gba/common/zelda.sav CardNewer");
        let r = apply(
            &mut lib,
            &pocket,
            &p,
            &mut HashCache::in_memory(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(r.versions_created, 1);
        let vs = versions(&lib, Path::new("Saves/gba/common/zelda.sav")).unwrap();
        assert_eq!(vs.len(), 1);
        assert_eq!(
            fs::read_to_string(lib.root().join(&vs[0].stored_path)).unwrap(),
            "save-v1"
        );

        // Library newer (e.g. restored) → pushed, card copy backed up.
        fs::write(
            lib.root().join("saves/Saves/gba/common/zelda.sav"),
            "save-v3",
        )
        .unwrap();
        set_mtime(
            &lib.root().join("saves/Saves/gba/common/zelda.sav"),
            t0 + 500,
        )
        .unwrap();
        let p = plan(
            &lib,
            &pocket,
            SavesOptions::default(),
            &mut HashCache::in_memory(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(kinds(&p)[1], "push Saves/gba/common/zelda.sav LibraryNewer");
        let r = apply(
            &mut lib,
            &pocket,
            &p,
            &mut HashCache::in_memory(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(r.pushed, 1);
        assert_eq!(
            fs::read_to_string(pocket.root().join("Saves/gba/common/zelda.sav")).unwrap(),
            "save-v3"
        );
        // The card's v2 wasn't anywhere in the library: it became a version.
        let kept: Vec<String> = versions(&lib, Path::new("Saves/gba/common/zelda.sav"))
            .unwrap()
            .iter()
            .map(|v| fs::read_to_string(lib.root().join(&v.stored_path)).unwrap())
            .collect();
        assert!(kept.contains(&"save-v2".to_string()), "{kept:?}");
        assert!(!pocket.root().join(".pocketeer/backup").exists());
        assert_eq!(
            mtime_secs(&pocket.root().join("Saves/gba/common/zelda.sav")).unwrap(),
            t0 + 500
        );

        // Tie (same mtime, different bytes): card wins.
        fs::write(pocket.root().join("Saves/gba/common/zelda.sav"), "save-v4").unwrap();
        set_mtime(&pocket.root().join("Saves/gba/common/zelda.sav"), t0 + 501).unwrap();
        let p = plan(
            &lib,
            &pocket,
            SavesOptions::default(),
            &mut HashCache::in_memory(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(kinds(&p)[1], "pull Saves/gba/common/zelda.sav Tie");

        // Direction filters.
        let p = plan(
            &lib,
            &pocket,
            SavesOptions {
                direction: Direction::Push,
                include_states: false,
            },
            &mut HashCache::in_memory(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(
            kinds(&p),
            ["skip Saves/gba/common/zelda.sav DirectionExcluded"]
        );
    }

    #[test]
    fn restore_makes_version_current_and_pushable() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lib = Library::init(&tmp.path().join("lib")).unwrap();
        let (_sd, pocket) = sd();
        let t0 = 1_700_000_000;
        write(pocket.root(), "Saves/gb/common/a.sav", "one", t0);
        let mut cache = HashCache::in_memory();
        let p = plan(
            &lib,
            &pocket,
            SavesOptions::default(),
            &mut cache,
            &mut |_| {},
        )
        .unwrap();
        apply(&mut lib, &pocket, &p, &mut cache, &mut |_| {}).unwrap();
        write(pocket.root(), "Saves/gb/common/a.sav", "two", t0 + 10);
        let p = plan(
            &lib,
            &pocket,
            SavesOptions::default(),
            &mut HashCache::in_memory(),
            &mut |_| {},
        )
        .unwrap();
        apply(
            &mut lib,
            &pocket,
            &p,
            &mut HashCache::in_memory(),
            &mut |_| {},
        )
        .unwrap();

        let vs = versions(&lib, Path::new("Saves/gb/common/a.sav")).unwrap();
        assert_eq!(vs.len(), 1);
        let restored = restore(&mut lib, vs[0].id, &mut HashCache::in_memory()).unwrap();
        assert_eq!(restored.sha1, vs[0].sha1);
        assert_eq!(
            fs::read_to_string(lib.root().join("saves/Saves/gb/common/a.sav")).unwrap(),
            "one"
        );
        // "two" is now a version too.
        assert_eq!(
            versions(&lib, Path::new("Saves/gb/common/a.sav"))
                .unwrap()
                .len(),
            2
        );
        // And the next sync pushes the restored save to the card.
        let p = plan(
            &lib,
            &pocket,
            SavesOptions::default(),
            &mut HashCache::in_memory(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(kinds(&p), ["push Saves/gb/common/a.sav LibraryNewer"]);
        assert_eq!(
            restore(&mut lib, 9999, &mut HashCache::in_memory())
                .unwrap_err()
                .code(),
            "SAVE_VERSION_NOT_FOUND"
        );
    }
}
