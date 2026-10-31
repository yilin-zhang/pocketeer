//! What ROM-ish files are on the card: `Assets/<platform>/<common|core id>/`.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{Error, Pocket, Result, fsutil};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RomEntry {
    pub name: String,
    pub size: u64,
}

/// One folder under a platform: `common` or a core's own folder.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RomDir {
    /// `common` or `<author>.<core>`.
    pub owner: String,
    /// Relative to the SD root.
    pub path: PathBuf,
    /// Top-level files only, sorted by name (case-insensitive). Hidden files skipped.
    pub files: Vec<RomEntry>,
    pub bytes: u64,
    /// Subfolders (e.g. instance JSON trees) are counted, not listed.
    pub subdirs: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PlatformRoms {
    pub platform: String,
    pub dirs: Vec<RomDir>,
    pub files: usize,
    pub bytes: u64,
}

/// Lists one directory as a [`RomDir`]; `None` if it doesn't exist.
pub fn read_dir(pocket: &Pocket, rel: &Path, owner: &str) -> Result<Option<RomDir>> {
    let full = pocket.root().join(rel);
    if !full.is_dir() {
        return Ok(None);
    }
    let mut files = Vec::new();
    let mut subdirs = 0;
    for entry in std::fs::read_dir(&full)
        .map_err(|e| Error::io(&full, e))?
        .filter_map(|e| e.ok())
    {
        if fsutil::is_hidden(&entry.file_name()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            subdirs += 1;
        } else if meta.is_file() {
            files.push(RomEntry {
                name,
                size: meta.len(),
            });
        }
    }
    files.sort_by_key(|f| f.name.to_lowercase());
    Ok(Some(RomDir {
        owner: owner.to_string(),
        path: rel.to_path_buf(),
        bytes: files.iter().map(|f| f.size).sum(),
        files,
        subdirs,
    }))
}

/// The `common` and per-core folders of one platform (empty if none exist).
pub fn platform_dirs(pocket: &Pocket, platform: &str) -> Result<Vec<RomDir>> {
    let base = pocket.assets_dir().join(platform);
    if !base.is_dir() {
        return Ok(Vec::new());
    }
    let mut owners: Vec<String> = std::fs::read_dir(&base)
        .map_err(|e| Error::io(&base, e))?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir() && !fsutil::is_hidden(&e.file_name()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    // `common` first, then cores alphabetically.
    owners.sort_by_key(|o| (o != "common", o.to_lowercase()));
    let mut dirs = Vec::new();
    for owner in owners {
        if let Some(dir) = read_dir(pocket, &Pocket::assets_rel(platform, &owner), &owner)? {
            dirs.push(dir);
        }
    }
    Ok(dirs)
}

/// Every platform folder under `Assets/`, with its `common` and per-core
/// folders. Platforms whose folders hold no files at all are still listed
/// (they exist because a core was installed), so the user sees the gap.
pub fn scan(pocket: &Pocket) -> Result<Vec<PlatformRoms>> {
    let assets = pocket.assets_dir();
    let mut out = Vec::new();
    if !assets.is_dir() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(&assets)
        .map_err(|e| Error::io(&assets, e))?
        .filter_map(|e| e.ok())
    {
        if fsutil::is_hidden(&entry.file_name()) || !entry.path().is_dir() {
            continue;
        }
        let platform = entry.file_name().to_string_lossy().into_owned();
        let dirs = platform_dirs(pocket, &platform)?;
        out.push(PlatformRoms {
            files: dirs.iter().map(|d| d.files.len()).sum(),
            bytes: dirs.iter().map(|d| d.bytes).sum(),
            platform,
            dirs,
        });
    }
    out.sort_by_key(|p| p.platform.to_lowercase());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{sd, write};
    use std::fs;

    #[test]
    fn scans_platforms_common_first() {
        let (tmp, pocket) = sd();
        let w = |rel: &str, body: &str| write(tmp.path(), rel, body);
        w("Assets/gba/common/b.gba", "22");
        w("Assets/gba/common/A.gba", "1");
        w("Assets/gba/common/.DS_Store", "x");
        w("Assets/gba/z.Core/game.json", "{}");
        w("Assets/gba/z.Core/sub/inner.bin", "xxx");
        w("Assets/arduboy/common/.gitkeep", "");
        fs::create_dir_all(tmp.path().join("Assets/.hidden/common")).unwrap();

        let plats = scan(&pocket).unwrap();
        assert_eq!(plats.len(), 2);
        assert_eq!(plats[0].platform, "arduboy");
        assert_eq!(plats[0].files, 0);
        let gba = &plats[1];
        assert_eq!(gba.platform, "gba");
        assert_eq!(gba.files, 3);
        assert_eq!(gba.bytes, 5);
        assert_eq!(gba.dirs[0].owner, "common");
        let names: Vec<&str> = gba.dirs[0].files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["A.gba", "b.gba"]);
        assert_eq!(gba.dirs[1].owner, "z.Core");
        assert_eq!(gba.dirs[1].subdirs, 1);
        assert_eq!(gba.dirs[1].path, Path::new("Assets/gba/z.Core"));
    }
}
