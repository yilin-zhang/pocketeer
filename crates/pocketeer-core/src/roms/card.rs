//! What ROM-ish files are on the card: `Assets/<platform>/<common|core id>/`.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{Error, Pocket, Result, fsutil};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RomEntry {
    /// Path relative to the owner folder, including any nested directories.
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
    /// Files recursively, sorted by relative path (case-insensitive). Hidden files skipped.
    pub files: Vec<RomEntry>,
    pub bytes: u64,
    /// Number of visible descendant directories.
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
fn scan_dir(pocket: &Pocket, rel: &Path, owner: &str, recursive: bool) -> Result<Option<RomDir>> {
    let full = pocket.root().join(rel);
    match std::fs::symlink_metadata(&full) {
        Ok(meta) if meta.is_symlink() => return Ok(None),
        Ok(meta) if !meta.is_dir() => return Err(Error::NotADirectory { path: full }),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io(&full, e)),
    }
    let mut files = Vec::new();
    let mut subdirs = 0;
    for entry in walkdir::WalkDir::new(&full)
        .min_depth(1)
        .max_depth(if recursive { usize::MAX } else { 1 })
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !fsutil::is_hidden(e.file_name()))
    {
        let entry = entry.map_err(|e| {
            Error::io(
                e.path().unwrap_or(&full),
                std::io::Error::other(e.to_string()),
            )
        })?;
        if entry.file_type().is_dir() {
            subdirs += 1;
        } else if entry.file_type().is_file() {
            let meta = entry
                .metadata()
                .map_err(|e| Error::io(entry.path(), std::io::Error::other(e.to_string())))?;
            files.push(RomEntry {
                name: entry
                    .path()
                    .strip_prefix(&full)
                    .expect("descendant")
                    .to_string_lossy()
                    .into_owned(),
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
    let entries = match std::fs::read_dir(&base) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::io(&base, e)),
    };
    let mut owners = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| Error::io(&base, e))?;
        if entry
            .file_type()
            .map_err(|e| Error::io(entry.path(), e))?
            .is_dir()
            && !fsutil::is_hidden(&entry.file_name())
        {
            owners.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    // `common` first, then cores alphabetically.
    owners.sort_by_key(|o| (o != "common", o.to_lowercase()));
    let mut dirs = Vec::new();
    for owner in owners {
        if let Some(dir) = scan_dir(pocket, &Pocket::assets_rel(platform, &owner), &owner, true)? {
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
    for entry in std::fs::read_dir(&assets).map_err(|e| Error::io(&assets, e))? {
        let entry = entry.map_err(|e| Error::io(&assets, e))?;
        if fsutil::is_hidden(&entry.file_name())
            || !entry
                .file_type()
                .map_err(|e| Error::io(entry.path(), e))?
                .is_dir()
        {
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
    if let Some(mut dir) = scan_dir(pocket, Path::new("GB Studio"), "", false)? {
        dir.files.retain(|f| {
            Path::new(&f.name)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("pocket"))
        });
        dir.bytes = dir.files.iter().map(|f| f.size).sum();
        out.push(PlatformRoms {
            platform: "gbstudio".into(),
            files: dir.files.len(),
            bytes: dir.bytes,
            dirs: vec![dir],
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
        assert_eq!(gba.files, 4);
        assert_eq!(gba.bytes, 8);
        assert_eq!(gba.dirs[0].owner, "common");
        let names: Vec<&str> = gba.dirs[0].files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["A.gba", "b.gba"]);
        assert_eq!(gba.dirs[1].owner, "z.Core");
        assert_eq!(gba.dirs[1].subdirs, 1);
        assert_eq!(gba.dirs[1].files[1].name, "sub/inner.bin");
        assert_eq!(gba.dirs[1].path, Path::new("Assets/gba/z.Core"));
    }
}
