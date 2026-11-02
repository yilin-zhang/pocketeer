//! Backup-first saves. Current files are the source of truth; the database
//! indexes immutable history. A restore always archives the card copy first.
use crate::library::{Library, db};

pub use crate::library::db::SaveVersion;
use crate::{
    Error, Pocket, Result,
    fsutil::hash::{copy_and_hash, hash_file},
    fsutil::{self, mtime_secs, set_mtime},
    plan::{Applied, Event, Failure, Plan, Progress},
};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    fs::File,
    path::{Component, Path, PathBuf},
};

const ROOTS: [&str; 4] = crate::card::SAVE_ROOTS;
const MTIME_TOLERANCE: i64 = 2;
mod owners;
mod trash;
pub use owners::{Owners, SaveKey, expected_saves, link_owners, rom_key, save_key};
pub use trash::{TrashPlan, TrashResult, TrashTarget, apply_trash, plan_trash};

/// Which save files a backup looks at.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Scope {
    /// Cartridge saves and Memories.
    #[default]
    All,
    /// Cartridge saves (`Saves/`, `GB Studio/Saves/`) only.
    Saves,
    /// Memories (save states) only.
    Memories,
}

impl Scope {
    fn covers(self, rel: &Path) -> bool {
        let memory = rel.starts_with(crate::card::MEMORIES_DIR);
        match self {
            Scope::All => true,
            Scope::Saves => !memory,
            Scope::Memories => memory,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SavesOptions {
    pub scope: Scope,
    /// Only these card paths (one game's saves); `None` for every one.
    pub only: Option<Vec<PathBuf>>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SaveInfo {
    pub size: u64,
    pub sha1: String,
    pub mtime: i64,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConflictChoice {
    Card,
    Computer,
    Skip,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SaveAction {
    Pull {
        rel: PathBuf,
        size: u64,
        reason: SaveReason,
    },
    Push {
        rel: PathBuf,
        source: PathBuf,
        identity: SaveInfo,
    },
    Conflict {
        rel: PathBuf,
        card: SaveInfo,
        computer: SaveInfo,
        reason: SaveReason,
    },
    Skip {
        rel: PathBuf,
        reason: SaveSkip,
    },
}

impl SaveAction {
    pub fn path(&self) -> &Path {
        match self {
            Self::Pull { rel, .. }
            | Self::Push { rel, .. }
            | Self::Conflict { rel, .. }
            | Self::Skip { rel, .. } => rel,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SaveReason {
    OnlyOnCard,
    CardNewer,
    ComputerNewer,
    DifferentContent,
    UserChoice,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SaveSkip {
    UpToDate,
    OnlyInLibrary,
    LeftUnchanged,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct SavesSummary {
    pub pull: usize,
    pub push: usize,
    pub up_to_date: usize,
    pub retained: usize,
    pub conflicts: usize,
    pub bytes: u64,
}
#[derive(Debug, Clone, PartialEq)]
struct Observed {
    card: Option<SaveInfo>,
    computer: Option<SaveInfo>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SavesPlan {
    // Private: `summary` counts `actions`, and `recount` keeps it so.
    actions: Vec<SaveAction>,
    summary: SavesSummary,
    #[serde(skip)]
    observed: BTreeMap<PathBuf, Observed>,
    #[serde(skip)]
    library: crate::library::LibraryIdentity,
    #[serde(skip)]
    card_identity: crate::card::CardIdentity,
}
impl Plan for SavesPlan {
    fn changes(&self) -> usize {
        self.summary.pull + self.summary.push
    }
    /// Unresolved conflicts count as failures: the CLI exits 1 on them.
    fn errors(&self) -> usize {
        self.summary.conflicts
    }
}

impl SavesPlan {
    pub fn actions(&self) -> &[SaveAction] {
        &self.actions
    }

    pub fn summary(&self) -> &SavesSummary {
        &self.summary
    }

    pub fn resolve(&mut self, i: usize, choice: ConflictChoice) {
        let SaveAction::Conflict {
            rel,
            card,
            computer,
            ..
        } = self.actions[i].clone()
        else {
            return;
        };
        self.actions[i] = match choice {
            ConflictChoice::Card => SaveAction::Pull {
                rel,
                size: card.size,
                reason: SaveReason::UserChoice,
            },
            ConflictChoice::Computer => SaveAction::Push {
                source: self.library.root().join("saves").join(&rel),
                rel,
                identity: computer,
            },
            ConflictChoice::Skip => SaveAction::Skip {
                rel,
                reason: SaveSkip::LeftUnchanged,
            },
        };
        self.recount();
    }
    fn recount(&mut self) {
        self.summary = SavesSummary::default();
        for action in &self.actions {
            match action {
                SaveAction::Pull { size, .. } => {
                    self.summary.pull += 1;
                    self.summary.bytes += size;
                }
                SaveAction::Push { identity, .. } => {
                    self.summary.push += 1;
                    self.summary.bytes += identity.size;
                }
                SaveAction::Conflict { .. } => self.summary.conflicts += 1,
                SaveAction::Skip {
                    reason: SaveSkip::UpToDate,
                    ..
                } => self.summary.up_to_date += 1,
                SaveAction::Skip { .. } => self.summary.retained += 1,
            }
        }
    }
}

fn validate_path(rel: &Path) -> Result<()> {
    if rel.components().any(|c| !matches!(c, Component::Normal(_)))
        || !ROOTS
            .iter()
            .any(|r| rel.starts_with(r) && rel != Path::new(r))
    {
        return Err(Error::InvalidPath {
            path: rel.to_path_buf(),
            reason: "expected a card-relative save path inside Saves, GB Studio/Saves or Memories"
                .into(),
        });
    }
    Ok(())
}

/// Whether a file under one of the save roots is the Pocket's. Under
/// `Saves/` that means `Saves/<platform>/…` with an openFPGA platform id
/// (lowercase letters, digits and `_`). Other firmware sharing the card
/// keeps its own folders there (Onion OS: `Saves/CurrentProfile/`,
/// `Saves/GuestProfile/`, `Saves/Common/`) and those are left alone.
fn is_pocket_save(rel: &Path) -> bool {
    let Ok(rest) = rel.strip_prefix(crate::card::SAVES_DIR) else {
        return true;
    };
    let mut parts = rest.components();
    match (parts.next(), parts.next()) {
        (Some(Component::Normal(platform)), Some(_)) => platform.to_str().is_some_and(|p| {
            !p.is_empty()
                && p.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        }),
        _ => false,
    }
}

fn files(root: &Path, scope: Scope) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for folder in ROOTS {
        if !scope.covers(Path::new(folder)) {
            continue;
        }
        let dir = root.join(folder);
        match std::fs::symlink_metadata(&dir) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(Error::io(&dir, e)),
            Ok(m) if m.file_type().is_symlink() => continue,
            Ok(_) => {}
        }
        for entry in walkdir::WalkDir::new(&dir)
            .min_depth(1)
            .into_iter()
            .filter_entry(|e| {
                // A folder that isn't a platform's is never walked into.
                let skipped_dir = e.depth() == 1
                    && e.file_type().is_dir()
                    && e.path()
                        .strip_prefix(root)
                        .is_ok_and(|rel| !is_pocket_save(&rel.join("x")));
                !fsutil::is_hidden(e.file_name()) && !skipped_dir
            })
        {
            let entry = entry.map_err(|e| {
                Error::io(
                    e.path().unwrap_or(&dir),
                    std::io::Error::other(e.to_string()),
                )
            })?;
            let rel = entry.path().strip_prefix(root).expect("descendant");
            if entry.file_type().is_file() && is_pocket_save(rel) {
                paths.push(rel.to_path_buf());
            }
        }
    }
    paths.sort();
    Ok(paths)
}

fn current(lib: &Library, rel: &Path) -> PathBuf {
    lib.root().join("saves").join(rel)
}

fn inspect(path: &Path) -> Result<Option<SaveInfo>> {
    let before = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io(path, e)),
    };
    let hashes = hash_file(path)?;
    let after = std::fs::metadata(path).map_err(|e| Error::io(path, e))?;
    if before.len() != after.len()
        || before.modified().map_err(|e| Error::io(path, e))?
            != after.modified().map_err(|e| Error::io(path, e))?
    {
        return Err(Error::Changed {
            path: path.to_path_buf(),
        });
    }
    Ok(Some(SaveInfo {
        size: hashes.size,
        sha1: hashes.sha1,
        mtime: mtime_secs(path)?,
    }))
}

pub fn plan(
    lib: &Library,
    pocket: &Pocket,
    opts: SavesOptions,
    progress: Progress<'_>,
) -> Result<SavesPlan> {
    let mut paths = files(pocket.root(), opts.scope)?;
    paths.extend(files(&lib.root().join("saves"), opts.scope)?);
    paths.sort();
    paths.dedup();
    if let Some(only) = &opts.only {
        paths.retain(|p| only.contains(p));
    }
    let mut plan = SavesPlan {
        actions: Vec::new(),
        summary: SavesSummary::default(),
        observed: BTreeMap::new(),
        library: lib.identity()?,
        card_identity: crate::card::CardIdentity::read(pocket)?,
    };
    for (i, rel) in paths.iter().enumerate() {
        progress(Event::Status(format!("Comparing {}", rel.display())));
        let card = inspect(&pocket.root().join(rel))?;
        let computer = inspect(&current(lib, rel))?;
        let Some(action) = decide(rel, &card, &computer) else {
            continue;
        };
        plan.observed
            .insert(rel.clone(), Observed { card, computer });
        plan.actions.push(action);
        progress(Event::Step {
            done: i + 1,
            total: paths.len(),
        });
    }
    plan.recount();
    Ok(plan)
}

/// What a backup does with one save, given both copies.
fn decide(rel: &Path, card: &Option<SaveInfo>, computer: &Option<SaveInfo>) -> Option<SaveAction> {
    Some(match (&card, &computer) {
        (Some(c), None) => SaveAction::Pull {
            rel: rel.to_path_buf(),
            size: c.size,
            reason: SaveReason::OnlyOnCard,
        },
        (None, Some(_)) => SaveAction::Skip {
            rel: rel.to_path_buf(),
            reason: SaveSkip::OnlyInLibrary,
        },
        (Some(c), Some(l)) if c.sha1 == l.sha1 => SaveAction::Skip {
            rel: rel.to_path_buf(),
            reason: SaveSkip::UpToDate,
        },
        (Some(c), Some(l)) if c.mtime > l.mtime.saturating_add(MTIME_TOLERANCE) => {
            SaveAction::Pull {
                rel: rel.to_path_buf(),
                size: c.size,
                reason: SaveReason::CardNewer,
            }
        }
        (Some(c), Some(l)) => SaveAction::Conflict {
            rel: rel.to_path_buf(),
            card: c.clone(),
            computer: l.clone(),
            reason: if l.mtime > c.mtime.saturating_add(MTIME_TOLERANCE) {
                SaveReason::ComputerNewer
            } else {
                SaveReason::DifferentContent
            },
        },
        (None, None) => return None,
    })
}

/// One save as the card and the library have it, for showing (nothing is
/// planned or changed).
#[derive(Debug, Clone, Serialize)]
pub struct SaveOverview {
    pub rel_path: PathBuf,
    pub card: Option<SaveInfo>,
    pub backup: Option<SaveInfo>,
    /// Older versions, newest first.
    pub versions: Vec<SaveVersion>,
    /// What a backup would do now.
    pub action: SaveAction,
}

/// Every save in `scope` on the card (when given) and in the library,
/// including saves only older versions are left of.
pub fn overview(lib: &Library, pocket: Option<&Pocket>, scope: Scope) -> Result<Vec<SaveOverview>> {
    let mut paths = files(&lib.root().join("saves"), scope)?;
    if let Some(pocket) = pocket {
        paths.extend(files(pocket.root(), scope)?);
    }
    paths.extend(
        db::save_version_paths(lib.conn())?
            .into_iter()
            .filter(|p| scope.covers(p) && is_pocket_save(p)),
    );
    paths.sort();
    paths.dedup();
    let mut out = Vec::new();
    for rel in paths {
        let card = match pocket {
            Some(p) => inspect(&p.root().join(&rel))?,
            None => None,
        };
        let backup = inspect(&current(lib, &rel))?;
        let action = decide(&rel, &card, &backup).unwrap_or(SaveAction::Skip {
            rel: rel.clone(),
            reason: SaveSkip::OnlyInLibrary,
        });
        out.push(SaveOverview {
            versions: versions(lib, &rel)?,
            rel_path: rel,
            card,
            backup,
            action,
        });
    }
    Ok(out)
}

pub fn plan_restore(
    lib: &Library,
    pocket: &Pocket,
    rel: &Path,
    version: Option<i64>,
) -> Result<SavesPlan> {
    validate_path(rel)?;
    let source = match version {
        None => current(lib, rel),
        Some(id) => {
            let v = versions_by_id(lib, id)?;
            if v.rel_path != rel {
                return Err(Error::InvalidPath {
                    path: rel.to_path_buf(),
                    reason: format!("save version {id} belongs to {}", v.rel_path.display()),
                });
            }
            let stored = lib.root().join(v.stored_path);
            if !inspect(&stored)?
                .is_some_and(|actual| actual.sha1 == v.sha1 && actual.size == v.size)
            {
                return Err(Error::Mismatch {
                    path: stored,
                    what: "historical save is missing or corrupt".into(),
                });
            }
            stored
        }
    };
    let identity = inspect(&source)?.ok_or_else(|| Error::RootNotFound {
        path: source.clone(),
    })?;
    let observed = Observed {
        card: inspect(&pocket.root().join(rel))?,
        computer: inspect(&current(lib, rel))?,
    };
    let mut plan = SavesPlan {
        actions: vec![SaveAction::Push {
            rel: rel.to_path_buf(),
            source,
            identity,
        }],
        summary: SavesSummary::default(),
        observed: BTreeMap::from([(rel.to_path_buf(), observed)]),
        library: lib.identity()?,
        card_identity: crate::card::CardIdentity::read(pocket)?,
    };
    plan.recount();
    Ok(plan)
}

/// Restores the library's current copy of each of `rels` (a Memory and
/// the files beside it) in one plan.
pub fn plan_restore_paths(lib: &Library, pocket: &Pocket, rels: &[PathBuf]) -> Result<SavesPlan> {
    let mut plans = rels.iter().map(|rel| plan_restore(lib, pocket, rel, None));
    let mut plan = plans.next().ok_or_else(|| Error::InvalidPath {
        path: PathBuf::new(),
        reason: "nothing to restore".into(),
    })??;
    for other in plans {
        let other = other?;
        plan.actions.extend(other.actions);
        plan.observed.extend(other.observed);
    }
    plan.recount();
    Ok(plan)
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct SavesResult {
    pub pulled: usize,
    pub pushed: usize,
    pub versions_created: usize,
    pub bytes: u64,
    pub failed: Vec<Failure>,
}

impl Applied for SavesResult {
    fn failures(&self) -> &[Failure] {
        &self.failed
    }
}
pub fn apply(
    lib: &mut Library,
    pocket: &Pocket,
    plan: &SavesPlan,
    progress: Progress<'_>,
) -> Result<SavesResult> {
    let mut result = SavesResult::default();
    let mut done = 0;
    for action in &plan.actions {
        let pull = match action {
            SaveAction::Pull { .. } => true,
            SaveAction::Push { .. } => false,
            SaveAction::Conflict { .. } | SaveAction::Skip { .. } => continue,
        };
        let rel = action.path();
        validate_path(rel)?;
        match copy_one(lib, pocket, plan, action) {
            Ok((archived, size)) => {
                if pull {
                    result.pulled += 1;
                } else {
                    result.pushed += 1;
                }
                result.versions_created += usize::from(archived);
                result.bytes += size;
            }
            Err(e) => result.failed.push(Failure {
                path: rel.to_path_buf(),
                error: e.to_info(),
            }),
        }
        done += 1;
        progress(Event::Step {
            done,
            total: plan.changes(),
        });
    }
    Ok(result)
}

/// Copies one save across (card → library for a pull, the other way for a
/// push) once both sides are confirmed to be as planned. The copy it
/// overwrites is archived first. Returns whether an archive was made, and
/// the bytes copied.
fn copy_one(
    lib: &Library,
    pocket: &Pocket,
    plan: &SavesPlan,
    action: &SaveAction,
) -> Result<(bool, u64)> {
    let rel = action.path();
    lib.check(&plan.library)?;
    plan.card_identity.check(pocket)?;
    let observed = plan.observed.get(rel).expect("planned action");
    let on_card = pocket.root().join(rel);
    let in_library = current(lib, rel);
    if observed.card != inspect(&on_card)? || observed.computer != inspect(&in_library)? {
        return Err(Error::Stale {
            what: rel.display().to_string(),
        });
    }
    let (source, destination, identity, replaced) = match action {
        SaveAction::Pull { .. } => (
            on_card.as_path(),
            in_library.as_path(),
            observed.card.as_ref().expect("pull source"),
            &observed.computer,
        ),
        SaveAction::Push {
            source, identity, ..
        } => {
            if inspect(source)?.as_ref() != Some(identity) {
                return Err(Error::Stale {
                    what: source.display().to_string(),
                });
            }
            (
                source.as_path(),
                on_card.as_path(),
                identity,
                &observed.card,
            )
        }
        SaveAction::Conflict { .. } | SaveAction::Skip { .. } => return Ok((false, 0)),
    };
    let archived = match replaced {
        Some(old) => archive(lib, rel, destination, old)?,
        None => false,
    };
    copy_checked(source, destination, identity)?;
    Ok((archived, identity.size))
}

fn copy_checked(source: &Path, destination: &Path, identity: &SaveInfo) -> Result<()> {
    fsutil::write_via_temp(destination, |out| {
        let mut input = File::open(source).map_err(|e| Error::io(source, e))?;
        let hashes = copy_and_hash(&mut input, out).map_err(|e| Error::io(source, e))?;
        if hashes.sha1 != identity.sha1 || hashes.size != identity.size {
            return Err(Error::Changed {
                path: source.to_path_buf(),
            });
        }
        Ok(())
    })?;
    set_mtime(destination, identity.mtime)
}

fn archive(lib: &Library, rel: &Path, source: &Path, identity: &SaveInfo) -> Result<bool> {
    for version in versions(lib, rel)? {
        if version.sha1 == identity.sha1
            && inspect(&lib.root().join(version.stored_path))?
                .is_some_and(|v| v.sha1 == identity.sha1)
        {
            return Ok(false);
        }
    }
    let stored_rel = PathBuf::from("saves/.history")
        .join(rel)
        .join(&identity.sha1);
    let stored = lib.root().join(&stored_rel);
    match inspect(&stored)? {
        Some(existing) if existing.sha1 == identity.sha1 && existing.size == identity.size => {}
        Some(_) => {
            return Err(Error::Mismatch {
                path: stored,
                what: "existing history file has different content".into(),
            });
        }
        None => fsutil::write_new_via_temp(&stored, |out| {
            let mut input = File::open(source).map_err(|e| Error::io(source, e))?;
            let hashes = copy_and_hash(&mut input, out).map_err(|e| Error::io(source, e))?;
            if hashes.sha1 != identity.sha1 || hashes.size != identity.size {
                return Err(Error::Changed {
                    path: source.to_path_buf(),
                });
            }
            Ok(())
        })?,
    }
    set_mtime(&stored, identity.mtime)?;
    db::insert_save_version(
        lib.conn(),
        rel,
        identity.size,
        &identity.sha1,
        identity.mtime,
        &stored_rel,
    )?;
    Ok(true)
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SaveEntry {
    pub rel_path: PathBuf,
    pub size: u64,
    pub mtime: i64,
    pub versions: usize,
}

pub fn list(lib: &Library) -> Result<Vec<SaveEntry>> {
    let counts = db::save_version_counts(lib.conn())?;
    files(&lib.root().join("saves"), Scope::All)?
        .into_iter()
        .map(|rel| {
            let path = current(lib, &rel);
            let size = std::fs::metadata(&path)
                .map_err(|e| Error::io(&path, e))?
                .len();
            let versions = counts
                .get(rel.to_string_lossy().as_ref())
                .copied()
                .unwrap_or(0);
            Ok(SaveEntry {
                size,
                mtime: mtime_secs(&path)?,
                versions,
                rel_path: rel,
            })
        })
        .collect()
}
/// Moves the library's backup and history of saves that were renamed on
/// the card (`from` → `to`, card paths), so the next backup still pairs
/// them. Returns how many saves moved.
pub fn follow_renames(lib: &Library, moves: &[(PathBuf, PathBuf)]) -> Result<usize> {
    let mut moved = 0;
    for (from, to) in moves {
        validate_path(from)?;
        validate_path(to)?;
        let history = |rel: &Path| PathBuf::from("saves/.history").join(rel);
        let mut any = false;
        for (a, b) in [
            (current(lib, from), current(lib, to)),
            (lib.root().join(history(from)), lib.root().join(history(to))),
        ] {
            if a.exists() {
                if b.exists() {
                    return Err(Error::AlreadyExists { path: b });
                }
                std::fs::create_dir_all(b.parent().expect("inside the library"))
                    .map_err(|e| Error::io(&b, e))?;
                std::fs::rename(&a, &b).map_err(|e| Error::io(&a, e))?;
                any = true;
            }
        }
        db::rename_save(lib.conn(), from, to, &history(to))?;
        moved += usize::from(any);
    }
    Ok(moved)
}

pub fn versions(lib: &Library, rel: &Path) -> Result<Vec<SaveVersion>> {
    db::save_versions(lib.conn(), rel)
}

pub fn versions_by_id(lib: &Library, version_id: i64) -> Result<SaveVersion> {
    db::save_version(lib.conn(), version_id)?.ok_or(Error::SaveVersionNotFound { id: version_id })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{sd, write_with_mtime as write};
    const REL: &str = "Saves/gba/common/nested/game.sav";
    fn plan_backup(lib: &Library, pocket: &Pocket) -> SavesPlan {
        plan(lib, pocket, SavesOptions::default(), &mut |_| {}).unwrap()
    }

    #[test]
    fn backup_keeps_history_and_requires_a_choice_for_computer_newer() {
        let (tmp, pocket) = sd();
        let mut lib = Library::init(&tmp.path().join("library")).unwrap();
        write(pocket.root(), REL, "AP one", 100);
        write(lib.root(), "saves/Saves/gb/common/offline.sav", "keep", 200);
        let plan = plan_backup(&lib, &pocket);
        assert_eq!(
            (plan.summary.pull, plan.summary.push, plan.summary.retained),
            (1, 0, 1)
        );
        let result = apply(&mut lib, &pocket, &plan, &mut |_| {}).unwrap();
        assert_eq!(result.pulled, 1);
        assert!(!pocket.root().join("Saves/gb/common/offline.sav").exists());
        write(pocket.root(), REL, "AP two", 300);
        let plan = plan_backup(&lib, &pocket);
        apply(&mut lib, &pocket, &plan, &mut |_| {}).unwrap();
        let history = versions(&lib, Path::new(REL)).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(
            std::fs::read(lib.root().join(&history[0].stored_path)).unwrap(),
            b"AP one"
        );
        write(lib.root(), &format!("saves/{REL}"), "PC new", 400);
        let mut plan = plan_backup(&lib, &pocket);
        assert_eq!(plan.summary.conflicts, 1);
        assert_eq!(plan.changes(), 0);
        apply(&mut lib, &pocket, &plan, &mut |_| {}).unwrap();
        assert_eq!(std::fs::read(pocket.root().join(REL)).unwrap(), b"AP two");
        let i = plan
            .actions
            .iter()
            .position(|a| matches!(a, SaveAction::Conflict { .. }))
            .unwrap();
        plan.resolve(i, ConflictChoice::Computer);
        let result = apply(&mut lib, &pocket, &plan, &mut |_| {}).unwrap();
        assert_eq!(result.pushed, 1);
        assert_eq!(std::fs::read(pocket.root().join(REL)).unwrap(), b"PC new");
        assert!(
            versions(&lib, Path::new(REL))
                .unwrap()
                .iter()
                .any(|v| std::fs::read(lib.root().join(&v.stored_path)).unwrap() == b"AP two")
        );
    }

    #[test]
    fn renamed_card_saves_take_their_backup_and_history_along() {
        let (tmp, pocket) = sd();
        let mut lib = Library::init(&tmp.path().join("library")).unwrap();
        write(pocket.root(), REL, "one", 100);
        let plan = plan_backup(&lib, &pocket);
        apply(&mut lib, &pocket, &plan, &mut |_| {}).unwrap();
        write(pocket.root(), REL, "two", 200);
        let plan = plan_backup(&lib, &pocket);
        apply(&mut lib, &pocket, &plan, &mut |_| {}).unwrap();
        let to = PathBuf::from("Saves/gba/common/nested/renamed.sav");
        std::fs::rename(pocket.root().join(REL), pocket.root().join(&to)).unwrap();
        assert_eq!(
            follow_renames(&lib, &[(REL.into(), to.clone())]).unwrap(),
            1
        );
        let history = versions(&lib, &to).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(
            std::fs::read(lib.root().join(&history[0].stored_path)).unwrap(),
            b"one"
        );
        let plan = plan_backup(&lib, &pocket);
        assert_eq!(plan.summary.up_to_date, 1);
        assert_eq!(plan.actions.len(), 1);
    }

    #[test]
    fn memories_are_their_own_scope_and_restore_with_their_companions() {
        let (tmp, pocket) = sd();
        let mut lib = Library::init(&tmp.path().join("library")).unwrap();
        let sta = "Memories/Save States/cart_1.sta";
        let sav = "Memories/Save States/cart_1.sav";
        write(pocket.root(), REL, "game save", 100);
        write(pocket.root(), sta, "state", 100);
        write(pocket.root(), sav, "sram", 100);
        let memories = SavesOptions {
            scope: Scope::Memories,
            only: None,
        };
        let planned = plan(&lib, &pocket, memories, &mut |_| {}).unwrap();
        let paths: Vec<&Path> = planned.actions.iter().map(SaveAction::path).collect();
        assert_eq!(paths, [Path::new(sav), Path::new(sta)]);
        apply(&mut lib, &pocket, &planned, &mut |_| {}).unwrap();
        // Cartridge saves alone leave Memories out.
        let saves = SavesOptions {
            scope: Scope::Saves,
            only: None,
        };
        let plan_saves = plan(&lib, &pocket, saves, &mut |_| {}).unwrap();
        assert_eq!(plan_saves.actions.len(), 1);
        assert_eq!(plan_saves.actions[0].path(), Path::new(REL));

        // Both files go back together; the card's copies are archived first.
        write(pocket.root(), sta, "newer state", 200);
        std::fs::remove_file(pocket.root().join(sav)).unwrap();
        let rels = [PathBuf::from(sta), PathBuf::from(sav)];
        let restore = plan_restore_paths(&lib, &pocket, &rels).unwrap();
        assert_eq!(restore.summary.push, 2);
        apply(&mut lib, &pocket, &restore, &mut |_| {}).unwrap();
        assert_eq!(std::fs::read(pocket.root().join(sta)).unwrap(), b"state");
        assert_eq!(std::fs::read(pocket.root().join(sav)).unwrap(), b"sram");
        assert!(
            versions(&lib, Path::new(sta))
                .unwrap()
                .iter()
                .any(|v| std::fs::read(lib.root().join(&v.stored_path)).unwrap() == b"newer state")
        );
    }

    #[test]
    fn other_firmware_folders_under_saves_are_not_pocket_saves() {
        // A card shared with a Miyoo Mini: Onion OS writes its profiles here.
        let (tmp, pocket) = sd();
        let lib = Library::init(&tmp.path().join("library")).unwrap();
        for rel in [
            "Saves/CurrentProfile/saves/mGBA/Zelda.srm",
            "Saves/GuestProfile/config/mGBA/mGBA.cfg",
            "Saves/Common/battery_logs/battery_logs.sqlite",
            "Saves/stray.sav",
            REL,
            "Saves/gb_old/common/game.sav",
            "GB Studio/Saves/demo.sav",
        ] {
            write(pocket.root(), rel, "x", 100);
        }
        let mut seen: Vec<String> = plan_backup(&lib, &pocket)
            .actions
            .iter()
            .map(|a| a.path().display().to_string())
            .collect();
        seen.sort();
        assert_eq!(
            seen,
            [
                "GB Studio/Saves/demo.sav",
                "Saves/gb_old/common/game.sav",
                REL,
            ]
        );
    }

    #[test]
    fn overview_shows_each_save_as_both_sides_have_it() {
        let (tmp, pocket) = sd();
        let mut lib = Library::init(&tmp.path().join("library")).unwrap();
        write(pocket.root(), REL, "one", 100);
        let plan = plan_backup(&lib, &pocket);
        apply(&mut lib, &pocket, &plan, &mut |_| {}).unwrap();
        write(pocket.root(), REL, "two", 300);
        write(lib.root(), "saves/Saves/gb/common/offline.sav", "kept", 100);
        let seen = overview(&lib, Some(&pocket), Scope::Saves).unwrap();
        assert_eq!(seen.len(), 2);
        let newer = seen.iter().find(|s| s.rel_path == Path::new(REL)).unwrap();
        assert!(matches!(
            newer.action,
            SaveAction::Pull {
                reason: SaveReason::CardNewer,
                ..
            }
        ));
        assert_eq!(newer.card.as_ref().unwrap().mtime, 300);
        assert_eq!(newer.backup.as_ref().unwrap().mtime, 100);
        // Without a card everything is library-only, and nothing is read from it.
        let offline = overview(&lib, None, Scope::Saves).unwrap();
        assert!(offline.iter().all(|s| s.card.is_none()
            && matches!(
                s.action,
                SaveAction::Skip {
                    reason: SaveSkip::OnlyInLibrary,
                    ..
                }
            )));
    }

    #[test]
    fn a_swapped_card_is_refused_between_plan_and_apply() {
        let (_card, pocket) = sd();
        let elsewhere = tempfile::tempdir().unwrap();
        let mut lib = Library::init(&elsewhere.path().join("library")).unwrap();
        write(pocket.root(), REL, "one", 100);
        let plan = plan_backup(&lib, &pocket);
        crate::testutil::swap_card(&pocket, elsewhere.path());
        let result = apply(&mut lib, &pocket, &plan, &mut |_| {}).unwrap();
        assert_eq!(result.failed.len(), 1);
        assert_eq!(result.failed[0].error.code, "STALE_PLAN");
        assert!(!current(&lib, Path::new(REL)).exists());
    }

    #[test]
    fn equal_metadata_different_bytes_conflicts_and_stale_preview_is_rejected() {
        let (tmp, pocket) = sd();
        let mut lib = Library::init(&tmp.path().join("library")).unwrap();
        write(pocket.root(), REL, "AAAA", 100);
        write(lib.root(), &format!("saves/{REL}"), "BBBB", 100);
        let mut plan = plan_backup(&lib, &pocket);
        assert_eq!(plan.summary.conflicts, 1);
        plan.resolve(0, ConflictChoice::Card);
        write(lib.root(), &format!("saves/{REL}"), "CCCC", 100);
        let result = apply(&mut lib, &pocket, &plan, &mut |_| {}).unwrap();
        assert_eq!(result.failed.len(), 1);
        assert_eq!(
            std::fs::read(current(&lib, Path::new(REL))).unwrap(),
            b"CCCC"
        );
        let mut plan = plan_backup(&lib, &pocket);
        plan.resolve(0, ConflictChoice::Skip);
        assert_eq!(plan.summary.conflicts, 0);
        assert_eq!(plan.summary.retained, 1);
    }

    #[test]
    fn restoring_history_archives_ap_first_and_rejects_corrupt_history() {
        let (tmp, pocket) = sd();
        let mut lib = Library::init(&tmp.path().join("library")).unwrap();
        write(pocket.root(), REL, "old", 100);
        let plan = plan_backup(&lib, &pocket);
        apply(&mut lib, &pocket, &plan, &mut |_| {}).unwrap();
        write(pocket.root(), REL, "new", 200);
        let plan = plan_backup(&lib, &pocket);
        apply(&mut lib, &pocket, &plan, &mut |_| {}).unwrap();
        let old = versions(&lib, Path::new(REL)).unwrap().remove(0);
        write(pocket.root(), REL, "unbacked AP", 300);
        let plan = plan_restore(&lib, &pocket, Path::new(REL), Some(old.id)).unwrap();
        let result = apply(&mut lib, &pocket, &plan, &mut |_| {}).unwrap();
        assert_eq!(result.pushed, 1);
        assert_eq!(std::fs::read(pocket.root().join(REL)).unwrap(), b"old");
        assert!(
            versions(&lib, Path::new(REL))
                .unwrap()
                .iter()
                .any(|v| std::fs::read(lib.root().join(&v.stored_path)).unwrap() == b"unbacked AP")
        );
        std::fs::write(lib.root().join(old.stored_path), "corrupt").unwrap();
        let code = |r: Result<SavesPlan>| r.unwrap_err().code();
        assert_eq!(
            code(plan_restore(&lib, &pocket, Path::new(REL), Some(old.id))),
            "CONTENT_MISMATCH"
        );
        assert_eq!(
            code(plan_restore(&lib, &pocket, Path::new("../outside"), None)),
            "INVALID_PATH"
        );
    }
}
