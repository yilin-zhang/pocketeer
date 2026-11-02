//! Backup-first saves. Current files are the source of truth; the database
//! indexes immutable history. A restore always archives the card copy first.
use crate::library::{Library, db};

pub use crate::library::db::SaveVersion;
use crate::{
    Error, ErrorInfo, Pocket, Result,
    fsutil::hash::{copy_and_hash, hash_file},
    fsutil::{self, mtime_secs, set_mtime},
    plan::{Applied, Event, Plan, Progress},
};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    fs::File,
    path::{Component, Path, PathBuf},
};

pub const ROOTS: [&str; 4] = crate::card::SAVE_ROOTS;
const MTIME_TOLERANCE: i64 = 2;
#[derive(Debug, Clone, Copy)]
pub struct SavesOptions {
    pub include_states: bool,
}
impl Default for SavesOptions {
    fn default() -> Self {
        Self {
            include_states: true,
        }
    }
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
    pub actions: Vec<SaveAction>,
    pub summary: SavesSummary,
    #[serde(skip)]
    observed: BTreeMap<PathBuf, Observed>,
    #[serde(skip)]
    library_root: PathBuf,
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
                source: self.library_root.join("saves").join(&rel),
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
        return Err(Error::Usage {
            message: "Expected a card-relative save path within Saves, GB Studio/Saves or Memories"
                .into(),
            hint: None,
        });
    }
    Ok(())
}

fn files(root: &Path, include_states: bool) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for folder in ROOTS {
        if !include_states && folder.starts_with(crate::card::MEMORIES_DIR) {
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
            .filter_entry(|e| !fsutil::is_hidden(e.file_name()))
        {
            let entry = entry.map_err(|e| {
                Error::io(
                    e.path().unwrap_or(&dir),
                    std::io::Error::other(e.to_string()),
                )
            })?;
            if entry.file_type().is_file() {
                paths.push(
                    entry
                        .path()
                        .strip_prefix(root)
                        .expect("descendant")
                        .to_path_buf(),
                );
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
    let mut paths = files(pocket.root(), opts.include_states)?;
    paths.extend(files(&lib.root().join("saves"), opts.include_states)?);
    paths.sort();
    paths.dedup();
    let mut plan = SavesPlan {
        actions: Vec::new(),
        summary: SavesSummary::default(),
        observed: BTreeMap::new(),
        library_root: lib.root().to_path_buf(),
        card_identity: crate::card::CardIdentity::read(pocket)?,
    };
    for (i, rel) in paths.iter().enumerate() {
        progress(Event::Status(format!("Comparing {}", rel.display())));
        let card = inspect(&pocket.root().join(rel))?;
        let computer = inspect(&current(lib, rel))?;
        let action = match (&card, &computer) {
            (Some(c), None) => SaveAction::Pull {
                rel: rel.clone(),
                size: c.size,
                reason: SaveReason::OnlyOnCard,
            },
            (None, Some(_)) => SaveAction::Skip {
                rel: rel.clone(),
                reason: SaveSkip::OnlyInLibrary,
            },
            (Some(c), Some(l)) if c.sha1 == l.sha1 => SaveAction::Skip {
                rel: rel.clone(),
                reason: SaveSkip::UpToDate,
            },
            (Some(c), Some(l)) if c.mtime > l.mtime.saturating_add(MTIME_TOLERANCE) => {
                SaveAction::Pull {
                    rel: rel.clone(),
                    size: c.size,
                    reason: SaveReason::CardNewer,
                }
            }
            (Some(c), Some(l)) => SaveAction::Conflict {
                rel: rel.clone(),
                card: c.clone(),
                computer: l.clone(),
                reason: if l.mtime > c.mtime.saturating_add(MTIME_TOLERANCE) {
                    SaveReason::ComputerNewer
                } else {
                    SaveReason::DifferentContent
                },
            },
            (None, None) => continue,
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
                return Err(Error::Usage {
                    message: "Version belongs to a different save path".into(),
                    hint: None,
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
        library_root: lib.root().to_path_buf(),
        card_identity: crate::card::CardIdentity::read(pocket)?,
    };
    plan.recount();
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

impl Applied for SavesResult {
    fn failed(&self) -> usize {
        self.failed.len()
    }
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
    progress: Progress<'_>,
) -> Result<SavesResult> {
    let mut result = SavesResult::default();
    let mut done = 0;
    for action in &plan.actions {
        if matches!(
            action,
            SaveAction::Skip { .. } | SaveAction::Conflict { .. }
        ) {
            continue;
        }
        let rel = action.path();
        validate_path(rel)?;
        let outcome = (|| -> Result<(bool, u64)> {
            if lib.root() != plan.library_root {
                return Err(Error::Stale {
                    what: "the library".into(),
                });
            }
            plan.card_identity.check(pocket)?;
            let observed = plan.observed.get(rel).expect("planned action");
            if observed.card != inspect(&pocket.root().join(rel))?
                || observed.computer != inspect(&current(lib, rel))?
            {
                return Err(Error::Stale {
                    what: rel.display().to_string(),
                });
            }
            match action {
                SaveAction::Pull { .. } => {
                    let source = pocket.root().join(rel);
                    let destination = current(lib, rel);
                    let identity = observed.card.as_ref().expect("pull source");
                    let archived = if let Some(old) = &observed.computer {
                        archive(lib, rel, &destination, old)?
                    } else {
                        false
                    };
                    copy_checked(&source, &destination, identity)?;
                    Ok((archived, identity.size))
                }
                SaveAction::Push {
                    source, identity, ..
                } => {
                    if inspect(source)?.as_ref() != Some(identity) {
                        return Err(Error::Stale {
                            what: source.display().to_string(),
                        });
                    }
                    let destination = pocket.root().join(rel);
                    let archived = if let Some(old) = &observed.card {
                        archive(lib, rel, &destination, old)?
                    } else {
                        false
                    };
                    copy_checked(source, &destination, identity)?;
                    Ok((archived, identity.size))
                }
                _ => unreachable!(),
            }
        })();
        match outcome {
            Ok((archived, size)) => {
                if matches!(action, SaveAction::Pull { .. }) {
                    result.pulled += 1;
                } else {
                    result.pushed += 1;
                }
                result.versions_created += usize::from(archived);
                result.bytes += size;
            }
            Err(e) => result.failed.push(SaveFailure {
                rel: rel.to_path_buf(),
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
    files(&lib.root().join("saves"), true)?
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
    fn backup(lib: &Library, pocket: &Pocket) -> SavesPlan {
        plan(lib, pocket, SavesOptions::default(), &mut |_| {}).unwrap()
    }

    #[test]
    fn backup_keeps_history_and_requires_a_choice_for_computer_newer() {
        let (tmp, pocket) = sd();
        let mut lib = Library::init(&tmp.path().join("library")).unwrap();
        write(pocket.root(), REL, "AP one", 100);
        write(lib.root(), "saves/Saves/gb/common/offline.sav", "keep", 200);
        let plan = backup(&lib, &pocket);
        assert_eq!(
            (plan.summary.pull, plan.summary.push, plan.summary.retained),
            (1, 0, 1)
        );
        let result = apply(&mut lib, &pocket, &plan, &mut |_| {}).unwrap();
        assert_eq!(result.pulled, 1);
        assert!(!pocket.root().join("Saves/gb/common/offline.sav").exists());
        write(pocket.root(), REL, "AP two", 300);
        let plan = backup(&lib, &pocket);
        apply(&mut lib, &pocket, &plan, &mut |_| {}).unwrap();
        let history = versions(&lib, Path::new(REL)).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(
            std::fs::read(lib.root().join(&history[0].stored_path)).unwrap(),
            b"AP one"
        );
        write(lib.root(), &format!("saves/{REL}"), "PC new", 400);
        let mut plan = backup(&lib, &pocket);
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
    fn equal_metadata_different_bytes_conflicts_and_stale_preview_is_rejected() {
        let (tmp, pocket) = sd();
        let mut lib = Library::init(&tmp.path().join("library")).unwrap();
        write(pocket.root(), REL, "AAAA", 100);
        write(lib.root(), &format!("saves/{REL}"), "BBBB", 100);
        let mut plan = backup(&lib, &pocket);
        assert_eq!(plan.summary.conflicts, 1);
        plan.resolve(0, ConflictChoice::Card);
        write(lib.root(), &format!("saves/{REL}"), "CCCC", 100);
        let result = apply(&mut lib, &pocket, &plan, &mut |_| {}).unwrap();
        assert_eq!(result.failed.len(), 1);
        assert_eq!(
            std::fs::read(current(&lib, Path::new(REL))).unwrap(),
            b"CCCC"
        );
        let mut plan = backup(&lib, &pocket);
        plan.resolve(0, ConflictChoice::Skip);
        assert_eq!(plan.summary.conflicts, 0);
        assert_eq!(plan.summary.retained, 1);
    }

    #[test]
    fn restoring_history_archives_ap_first_and_rejects_corrupt_history() {
        let (tmp, pocket) = sd();
        let mut lib = Library::init(&tmp.path().join("library")).unwrap();
        write(pocket.root(), REL, "old", 100);
        let plan = backup(&lib, &pocket);
        apply(&mut lib, &pocket, &plan, &mut |_| {}).unwrap();
        write(pocket.root(), REL, "new", 200);
        let plan = backup(&lib, &pocket);
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
        assert!(plan_restore(&lib, &pocket, Path::new(REL), Some(old.id)).is_err());
        assert!(plan_restore(&lib, &pocket, Path::new("../outside"), None).is_err());
    }
}
