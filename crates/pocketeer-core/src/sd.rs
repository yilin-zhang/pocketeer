// The "does this folder look like a Pocket" rule is adapted from Pocket Sync
// (src-tauri/src/checks.rs).
// Copyright (c) Neil Morrison — https://github.com/neil-morrison44/pocket-sync
// Licensed under AGPL-3.0. Modified for pocketeer, 2026.

use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// Entries that must exist at the root of a Pocket SD card.
pub const ROOT_MARKERS: [&str; 3] = ["Analogue_Pocket.json", "Assets", "Cores"];

/// pocketeer's own folder at the card root: install staging and the device
/// marker. Nothing else lives there, and nothing stays after an operation.
pub const POCKETEER_DIR: &str = ".pocketeer";

/// Where removable volumes show up, per OS. Each entry is a folder whose
/// direct children are mount points (`/Volumes/POCKET`, `/media/me/POCKET`).
pub fn mount_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if cfg!(target_os = "macos") {
        dirs.push(PathBuf::from("/Volumes"));
    } else if cfg!(target_os = "linux") {
        if let Some(home) = dirs::home_dir()
            && let Some(user) = home.file_name()
        {
            dirs.push(PathBuf::from("/media").join(user));
            dirs.push(PathBuf::from("/run/media").join(user));
        }
        dirs.push(PathBuf::from("/media"));
        dirs.push(PathBuf::from("/mnt"));
    }
    dirs
}

/// Windows has no mount folder; every drive letter is a candidate root.
fn drive_roots() -> Vec<PathBuf> {
    if !cfg!(target_os = "windows") {
        return Vec::new();
    }
    (b'A'..=b'Z')
        .map(|c| PathBuf::from(format!("{}:\\", c as char)))
        .filter(|p| p.is_dir())
        .collect()
}

/// Returns the [`ROOT_MARKERS`] missing from `path`; empty means it looks like a Pocket.
pub fn missing_markers(path: &Path) -> Vec<String> {
    ROOT_MARKERS
        .iter()
        .filter(|marker| !path.join(marker).exists())
        .map(|marker| marker.to_string())
        .collect()
}

/// A validated Pocket SD card root.
#[derive(Debug, Clone)]
pub struct Pocket {
    root: PathBuf,
}

impl Pocket {
    /// Opens an explicit path, checking that it looks like a Pocket SD card.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let root = path.into();
        if !root.exists() {
            return Err(Error::RootNotFound { path: root });
        }
        let missing = missing_markers(&root);
        if !missing.is_empty() {
            return Err(Error::NotAPocket {
                path: root,
                missing,
            });
        }
        Ok(Pocket { root })
    }

    /// Finds the single mounted Pocket SD card among the OS's mount points.
    pub fn detect() -> Result<Self> {
        let dirs = mount_dirs();
        let mut candidates: Vec<PathBuf> = dirs
            .iter()
            .filter(|d| d.is_dir())
            .flat_map(|d| children(d))
            .chain(drive_roots())
            .filter(|path| missing_markers(path).is_empty())
            .collect();
        candidates.sort();
        candidates.dedup();
        pick(candidates, dirs)
    }

    /// Finds the single Pocket SD card among the direct children of `mount_dir`.
    #[cfg(test)]
    pub(crate) fn detect_in(mount_dir: &Path) -> Result<Self> {
        if !mount_dir.is_dir() {
            return Err(Error::NotADirectory {
                path: mount_dir.to_path_buf(),
            });
        }
        let mut candidates: Vec<PathBuf> = children(mount_dir)
            .filter(|path| missing_markers(path).is_empty())
            .collect();
        candidates.sort();
        pick(candidates, vec![mount_dir.to_path_buf()])
    }

    /// Uses `root` if given, otherwise auto-detects.
    pub fn open_or_detect(root: Option<&Path>) -> Result<Self> {
        match root {
            Some(path) => Self::open(path),
            None => Self::detect(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn cores_dir(&self) -> PathBuf {
        self.root.join("Cores")
    }

    pub fn assets_dir(&self) -> PathBuf {
        self.root.join("Assets")
    }

    /// `Assets/<platform>/<owner>` relative to the SD root, where `owner` is
    /// `common` or a core id. The one place that layout is spelled out.
    pub fn assets_rel(platform: &str, owner: &str) -> PathBuf {
        PathBuf::from("Assets").join(platform).join(owner)
    }

    /// Absolute [`Pocket::assets_rel`].
    pub fn assets_owner_dir(&self, platform: &str, owner: &str) -> PathBuf {
        self.root.join(Self::assets_rel(platform, owner))
    }

    /// [`POCKETEER_DIR`] on this card.
    pub fn pocketeer_dir(&self) -> PathBuf {
        self.root.join(POCKETEER_DIR)
    }

    pub fn analogue_pocket_json(&self) -> PathBuf {
        self.root.join("Analogue_Pocket.json")
    }
}

/// Readable direct subdirectories; unreadable volumes are skipped rather than
/// failing the whole search.
fn children(dir: &Path) -> impl Iterator<Item = PathBuf> {
    std::fs::read_dir(dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
}

fn pick(mut candidates: Vec<PathBuf>, searched: Vec<PathBuf>) -> Result<Pocket> {
    match candidates.len() {
        0 => Err(Error::SdNotFound { searched }),
        1 => Ok(Pocket {
            root: candidates.remove(0),
        }),
        _ => Err(Error::MultipleSdCards { candidates }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_pocket(dir: &Path) {
        fs::create_dir_all(dir.join("Assets")).unwrap();
        fs::create_dir_all(dir.join("Cores")).unwrap();
        fs::write(dir.join("Analogue_Pocket.json"), "{}").unwrap();
    }

    #[test]
    fn open_rejects_folder_missing_markers() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("Cores")).unwrap();
        let err = Pocket::open(tmp.path()).unwrap_err();
        assert_eq!(err.code(), "NOT_A_POCKET");
        match err {
            Error::NotAPocket { missing, .. } => {
                assert_eq!(missing, vec!["Analogue_Pocket.json", "Assets"])
            }
            other => panic!("unexpected error {other:?}"),
        }
    }

    #[test]
    fn open_reports_missing_path() {
        let err = Pocket::open("/definitely/not/here").unwrap_err();
        assert_eq!(err.code(), "ROOT_NOT_FOUND");
    }

    #[test]
    fn detect_finds_single_pocket() {
        let mounts = tempfile::tempdir().unwrap();
        fs::create_dir(mounts.path().join("USB_STICK")).unwrap();
        make_pocket(&mounts.path().join("POCKET"));
        let pocket = Pocket::detect_in(mounts.path()).unwrap();
        assert_eq!(pocket.root(), mounts.path().join("POCKET"));
    }

    #[test]
    fn detect_reports_none() {
        let mounts = tempfile::tempdir().unwrap();
        fs::create_dir(mounts.path().join("USB_STICK")).unwrap();
        assert_eq!(
            Pocket::detect_in(mounts.path()).unwrap_err().code(),
            "SD_NOT_FOUND"
        );
    }

    #[test]
    fn detect_refuses_to_guess_between_two() {
        let mounts = tempfile::tempdir().unwrap();
        make_pocket(&mounts.path().join("A"));
        make_pocket(&mounts.path().join("B"));
        let err = Pocket::detect_in(mounts.path()).unwrap_err();
        assert_eq!(err.code(), "MULTIPLE_SD_CARDS");
    }

    #[test]
    fn mount_dirs_match_the_host_os() {
        let dirs = mount_dirs();
        if cfg!(target_os = "macos") {
            assert_eq!(dirs, [PathBuf::from("/Volumes")]);
        } else if cfg!(target_os = "linux") {
            assert!(dirs.contains(&PathBuf::from("/mnt")));
        } else {
            assert!(dirs.is_empty());
        }
    }
}
