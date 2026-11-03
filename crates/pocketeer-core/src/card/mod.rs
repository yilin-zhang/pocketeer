// The "does this folder look like a Pocket" rule is adapted from Pocket Sync
// (src-tauri/src/checks.rs).
// Copyright (c) Neil Morrison — https://github.com/neil-morrison44/pocket-sync
// Licensed under AGPL-3.0. Modified for pocketeer, 2026.

pub mod bios;
pub mod clean;
pub mod cores;
pub mod formats;
pub mod install;
pub mod memories;
pub mod roms;

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{Error, ErrorInfo, Result};

use formats::{AnaloguePocketJson, firmware_version_from_byte};

// ---------------------------------------------------------------------------
// Layout. Every path pocketeer reads or writes on a card is spelled out
// here and nowhere else.

pub const ASSETS_DIR: &str = "Assets";
pub const CORES_DIR: &str = "Cores";
pub const SAVES_DIR: &str = "Saves";
pub const PLATFORMS_DIR: &str = "Platforms";
const SETTINGS_DIR: &str = "Settings";
const PRESETS_DIR: &str = "Presets";
pub const MEMORIES_DIR: &str = "Memories";
/// The two folders Memories are written to.
pub const MEMORY_ROOTS: [&str; 2] = ["Memories/Save States", "Memories/Beta"];
/// GB Studio games live at the root, not under `Assets/`.
pub const GB_STUDIO_DIR: &str = "GB Studio";
/// And their saves beside them, not under `Saves/`.
const GB_STUDIO_SAVES_DIR: &str = "GB Studio/Saves";
/// The platform id pocketeer gives GB Studio `.pocket` files.
pub const GB_STUDIO_PLATFORM: &str = "gbstudio";
pub const POCKET_JSON: &str = "Analogue_Pocket.json";

/// Entries that must exist at the root of a Pocket SD card.
const ROOT_MARKERS: [&str; 3] = [POCKET_JSON, ASSETS_DIR, CORES_DIR];

/// pocketeer's folder for disposable ROM cache and install staging.
pub const POCKETEER_DIR: &str = ".pocketeer";

/// Where save files and save states live.
pub const SAVE_ROOTS: [&str; 4] = [
    SAVES_DIR,
    GB_STUDIO_SAVES_DIR,
    MEMORY_ROOTS[0],
    MEMORY_ROOTS[1],
];

/// Folders whose metadata changes when a ROM, core or save is added,
/// removed or renamed.
pub const WATCHED_DIRS: [&str; 6] = [
    ASSETS_DIR,
    CORES_DIR,
    SAVES_DIR,
    GB_STUDIO_DIR,
    MEMORY_ROOTS[0],
    MEMORY_ROOTS[1],
];

/// The folder under `Assets/<platform>/` and `Saves/<platform>/` shared by
/// every core of the platform; the others are named after one core.
pub const COMMON: &str = "common";

/// `Assets/<platform>/<owner>`, where `owner` is [`COMMON`] or a core id.
pub fn assets_rel(platform: &str, owner: &str) -> PathBuf {
    PathBuf::from(ASSETS_DIR).join(platform).join(owner)
}

/// `Saves/<platform>/<owner>`.
pub fn saves_rel(platform: &str, owner: &str) -> PathBuf {
    PathBuf::from(SAVES_DIR).join(platform).join(owner)
}

/// `Cores/<id>`.
pub fn core_rel(core_id: &str) -> PathBuf {
    PathBuf::from(CORES_DIR).join(core_id)
}

/// `Settings/<id>`.
pub fn settings_rel(core_id: &str) -> PathBuf {
    PathBuf::from(SETTINGS_DIR).join(core_id)
}

/// The folders that are a core's own: `Cores/`, `Presets/` and `Settings/`.
pub fn core_dirs(core_id: &str) -> [PathBuf; 3] {
    [
        core_rel(core_id),
        PathBuf::from(PRESETS_DIR).join(core_id),
        settings_rel(core_id),
    ]
}

/// A core's per-platform data: its own `Assets/` and `Saves/` folders.
pub fn core_platform_dirs(platform: &str, core_id: &str) -> [PathBuf; 2] {
    [assets_rel(platform, core_id), saves_rel(platform, core_id)]
}

/// Where a ROM the user adds for `platform` goes: the shared `common`
/// folder, or the root `GB Studio/` folder for GB Studio games.
pub fn rom_dir(platform: &str) -> PathBuf {
    if platform == GB_STUDIO_PLATFORM {
        PathBuf::from(GB_STUDIO_DIR)
    } else {
        assets_rel(platform, COMMON)
    }
}

/// The folder holding the ordinary saves of a ROM at `rom_rel`:
/// `Assets/<p>/<owner>/…` → `Saves/<p>/<owner>/…`, `GB Studio/x` →
/// `GB Studio/Saves`. `None` for a path that is not a ROM location.
pub fn save_dir_for(rom_rel: &Path) -> Option<PathBuf> {
    if let Ok(relative) = rom_rel.strip_prefix(ASSETS_DIR) {
        Some(PathBuf::from(SAVES_DIR).join(relative.parent()?))
    } else if rom_rel.parent() == Some(Path::new(GB_STUDIO_DIR)) {
        Some(PathBuf::from(GB_STUDIO_SAVES_DIR))
    } else {
        None
    }
}

/// Where removable volumes show up, per OS. Each entry is a folder whose
/// direct children are mount points (`/Volumes/POCKET`, `/media/me/POCKET`).
fn mount_dirs() -> Vec<PathBuf> {
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
fn missing_markers(path: &Path) -> Vec<String> {
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
        self.root.join(CORES_DIR)
    }

    pub fn assets_dir(&self) -> PathBuf {
        self.root.join(ASSETS_DIR)
    }

    /// Absolute [`assets_rel`].
    pub fn assets_owner_dir(&self, platform: &str, owner: &str) -> PathBuf {
        self.root.join(assets_rel(platform, owner))
    }

    /// [`POCKETEER_DIR`] on this card.
    pub fn pocketeer_dir(&self) -> PathBuf {
        self.root.join(POCKETEER_DIR)
    }

    pub fn analogue_pocket_json(&self) -> PathBuf {
        self.root.join(POCKET_JSON)
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

// ---------------------------------------------------------------------------
// Status: firmware, disk space, installed cores.

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Status {
    pub root: PathBuf,
    pub product: Option<String>,
    pub firmware: Option<Firmware>,
    pub disk: Option<DiskUsage>,
    pub installed_cores: usize,
    pub broken_cores: usize,
    /// Non-fatal problems found while gathering the status.
    pub warnings: Vec<ErrorInfo>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Firmware {
    /// Decoded version, e.g. `"2.2"`.
    pub version: Option<String>,
    pub byte: Option<u32>,
    pub name: Option<String>,
    pub build_date: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct DiskUsage {
    pub total_bytes: u64,
    pub available_bytes: u64,
}

/// Gathers the status. Only fails if `Cores/` itself can't be read; problems
/// with `Analogue_Pocket.json` or disk stats become warnings.
pub fn status(pocket: &Pocket) -> Result<Status> {
    Ok(Status::read(pocket, &cores::list_installed(pocket)?))
}

impl Status {
    /// Reads firmware and disk status, reusing the caller's installed-core scan.
    pub fn read(pocket: &Pocket, list: &cores::CoreList) -> Self {
        let mut warnings = Vec::new();

        let pocket_json = match AnaloguePocketJson::read(&pocket.analogue_pocket_json()) {
            Ok(json) => json,
            Err(err) => {
                warnings.push(err.to_info());
                AnaloguePocketJson::default()
            }
        };
        let firmware = pocket_json.runtime().map(|rt| Firmware {
            version: rt.byte.map(firmware_version_from_byte),
            byte: rt.byte,
            name: rt.name.clone(),
            build_date: rt.build_date.clone(),
        });

        let disk = match (
            fs2::total_space(pocket.root()),
            fs2::available_space(pocket.root()),
        ) {
            (Ok(total_bytes), Ok(available_bytes)) => Some(DiskUsage {
                total_bytes,
                available_bytes,
            }),
            (Err(e), _) | (_, Err(e)) => {
                warnings.push(crate::Error::io(pocket.root(), e).to_info());
                None
            }
        };

        Status {
            root: pocket.root().to_path_buf(),
            product: pocket_json.product,
            firmware,
            disk,
            installed_cores: list.cores.len(),
            broken_cores: list.broken.len(),
            warnings,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::card_at;
    use std::fs;

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
        card_at(&mounts.path().join("POCKET"));
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
        card_at(&mounts.path().join("A"));
        card_at(&mounts.path().join("B"));
        let err = Pocket::detect_in(mounts.path()).unwrap_err();
        assert_eq!(err.code(), "MULTIPLE_SD_CARDS");
    }
}
