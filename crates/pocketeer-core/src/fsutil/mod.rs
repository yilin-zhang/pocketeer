//! Files on disk, below any notion of a card or a library: atomic writes,
//! the one clock, the "is this file content?" filters, zip access, hashing
//! ([`hash`]), ROM file-name rules and safe renames ([`names`]), and finding
//! ROM candidates in a folder ([`scan`]).

pub mod hash;
pub mod names;
pub mod scan;

use std::{
    ffi::OsStr,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{Error, Result};

/// Seconds since the epoch, now. The only clock in the crate.
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `YYYYMMDD-HHMMSS` in UTC, sortable and filesystem-safe.
pub fn timestamp() -> String {
    format_timestamp(now_secs().max(0) as u64)
}

fn format_timestamp(unix: u64) -> String {
    let days = unix / 86_400;
    let rem = unix % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{y:04}{mo:02}{d:02}-{h:02}{m:02}{s:02}")
}

// Howard Hinnant's days-to-civil algorithm (public domain).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Size and modification time of a file: the cheap check that it has not
/// been rewritten since it was last looked at. Both caches key on it; the
/// field order is part of the card cache's on-disk format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Stamp {
    pub size: u64,
    pub seconds: u64,
    pub nanos: u32,
}

impl Stamp {
    pub fn of(path: &Path) -> Result<Stamp> {
        let meta = std::fs::metadata(path).map_err(|e| Error::io(path, e))?;
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .unwrap_or_default();
        Ok(Stamp {
            size: meta.len(),
            seconds: modified.as_secs(),
            nanos: modified.subsec_nanos(),
        })
    }
}

/// Modification time of already-fetched metadata, in seconds since the
/// epoch (0 if unknown).
pub fn mtime_of(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Modification time in seconds since the epoch (0 if unknown).
pub fn mtime_secs(path: &Path) -> Result<i64> {
    let meta = std::fs::metadata(path).map_err(|e| Error::io(path, e))?;
    Ok(mtime_of(&meta))
}

pub fn set_mtime(path: &Path, secs: i64) -> Result<()> {
    let f = std::fs::File::options()
        .write(true)
        .open(path)
        .map_err(|e| Error::io(path, e))?;
    f.set_modified(UNIX_EPOCH + Duration::from_secs(secs.max(0) as u64))
        .map_err(|e| Error::io(path, e))
}

/// File size, 0 when the file can't be read.
pub fn size_of(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Number of files and total bytes under `path` (unreadable entries skipped).
pub fn dir_size(path: &Path) -> (usize, u64) {
    let mut files = 0;
    let mut bytes = 0;
    for entry in walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if let Ok(meta) = entry.metadata()
            && meta.is_file()
        {
            files += 1;
            bytes += meta.len();
        }
    }
    (files, bytes)
}

/// Markdown documents that ship with releases. Only these names: `.md` is
/// also the Mega Drive / Genesis ROM extension, so `Sonic.md` is a game.
fn is_doc(name: &str) -> bool {
    let Some(stem) = name
        .len()
        .checked_sub(3)
        .filter(|&i| name.is_char_boundary(i) && name[i..].eq_ignore_ascii_case(".md"))
        .map(|i| &name[..i])
    else {
        return false;
    };
    [
        "readme",
        "changelog",
        "changes",
        "license",
        "licence",
        "contributing",
        "credits",
        "authors",
        "notes",
        "history",
    ]
    .iter()
    .any(|d| stem.eq_ignore_ascii_case(d))
}

/// Hidden entries (`.DS_Store`, `._foo`, `.pocketeer`) are never content.
pub fn is_hidden(name: &OsStr) -> bool {
    name.to_string_lossy().starts_with('.')
}

/// Files that release zips and ROM folders commonly carry but that have no
/// business on the SD card.
pub fn is_junk(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return true;
    };
    let top_level = path.components().count() == 1;
    path.components()
        .next()
        .is_some_and(|c| c.as_os_str() == "__MACOSX")
        || name == ".DS_Store"
        || name.starts_with("._")
        || name == ".keep"
        || name == ".gitkeep"
        || is_doc(name)
        || name.ends_with(".mra")
        || (top_level && name.ends_with(".txt"))
}

/// DAT/text/JSON files that live next to ROMs (instance JSONs included) but aren't ROMs.
pub fn is_sidecar(rel: &Path) -> bool {
    rel.extension().is_some_and(|e| {
        ["dat", "xml", "txt", "nfo", "sfv", "md5", "sha1", "json"]
            .iter()
            .any(|x| e.eq_ignore_ascii_case(x))
    })
}

/// Every file under `dir`, recursively, skipping hidden files and whole
/// hidden folders. Unreadable entries are skipped, not reported. Order is
/// walkdir's (unsorted).
pub fn visible_files(dir: &Path) -> impl Iterator<Item = PathBuf> {
    walkdir::WalkDir::new(dir)
        .min_depth(1)
        .into_iter()
        .filter_entry(|e| !is_hidden(e.file_name()))
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(walkdir::DirEntry::into_path)
}

/// Writes `dest` by filling a temp file next to it and renaming, so an
/// interrupted write (unplugged card) never leaves a half file under the
/// real name. Parent folders are created.
pub fn write_via_temp(
    dest: &Path,
    fill: impl FnOnce(&mut std::fs::File) -> Result<()>,
) -> Result<()> {
    let temp = filled_temp(dest, fill)?;
    temp.persist(dest).map_err(|e| Error::io(dest, e.error))?;
    Ok(())
}

/// Publishes a new file atomically without replacing an existing destination.
pub fn write_new_via_temp(
    dest: &Path,
    fill: impl FnOnce(&mut std::fs::File) -> Result<()>,
) -> Result<()> {
    let temp = filled_temp(dest, fill)?;
    temp.persist_noclobber(dest).map_err(|e| {
        if e.error.kind() == std::io::ErrorKind::AlreadyExists {
            Error::AlreadyExists {
                path: dest.to_path_buf(),
            }
        } else {
            Error::io(dest, e.error)
        }
    })?;
    Ok(())
}

fn filled_temp(
    dest: &Path,
    fill: impl FnOnce(&mut std::fs::File) -> Result<()>,
) -> Result<tempfile::NamedTempFile> {
    let parent = dest.parent().ok_or_else(|| Error::NotADirectory {
        path: dest.to_path_buf(),
    })?;
    std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    let mut temp = tempfile::Builder::new()
        .prefix(".pocketeer-")
        .tempfile_in(parent)
        .map_err(|e| Error::io(parent, e))?;
    fill(temp.as_file_mut())?;
    temp.as_file_mut().flush().map_err(|e| Error::io(dest, e))?;
    Ok(temp)
}

/// A rename with an atomic no-overwrite guarantee (including rollback moves).
pub fn rename_no_replace(from: &Path, to: &Path) -> Result<()> {
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_vendor = "apple",
        target_os = "redox"
    ))]
    let result: std::io::Result<()> = {
        use rustix::fs::{CWD, RenameFlags, renameat_with};
        renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE).map_err(Into::into)
    };
    #[cfg(windows)]
    let result: std::io::Result<()> = {
        use std::os::windows::ffi::OsStrExt;
        let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
        let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
        // Both buffers are NUL-terminated and live for the duration of the call.
        let success = unsafe {
            windows_sys::Win32::Storage::FileSystem::MoveFileExW(from.as_ptr(), to.as_ptr(), 0)
        };
        if success == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    };
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_vendor = "apple",
        target_os = "redox",
        windows
    )))]
    let result: std::io::Result<()> = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "atomic no-replace rename is unavailable on this OS",
    ));
    result.map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            Error::AlreadyExists {
                path: to.to_path_buf(),
            }
        } else {
            Error::io(from, e)
        }
    })
}

/// Atomically replaces `path` with `bytes`.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    write_via_temp(path, |out| {
        out.write_all(bytes).map_err(|e| Error::io(path, e))
    })
}

/// Opens a zip file on disk for reading.
pub fn open_zip(archive: &Path) -> Result<zip::ZipArchive<std::fs::File>> {
    let file = std::fs::File::open(archive).map_err(|e| Error::io(archive, e))?;
    zip::ZipArchive::new(file).map_err(|e| Error::zip(e, archive.display().to_string()))
}

/// Streams one entry of a zip on disk through `read`. Read errors inside the
/// closure are the closure's to map; the archive and entry lookup are mapped
/// here.
pub fn with_zip_entry<T>(
    archive: &Path,
    inner: &str,
    read: impl FnOnce(&mut dyn std::io::Read) -> Result<T>,
) -> Result<T> {
    let mut zip = open_zip(archive)?;
    let mut entry = zip
        .by_name(inner)
        .map_err(|e| Error::zip(e, format!("{inner} in {}", archive.display())))?;
    read(&mut entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::write;

    #[test]
    fn temp_writes_leave_no_debris_and_file_helpers_keep_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("sub/file.bin");
        let err = write_via_temp(&dest, |_| Err(Error::NotADirectory { path: dest.clone() }))
            .unwrap_err();
        assert_eq!(err.code(), "NOT_A_DIRECTORY");
        assert!(!dest.exists());
        assert_eq!(
            std::fs::read_dir(tmp.path().join("sub")).unwrap().count(),
            0
        );

        write_atomic(&dest, b"hello").unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello");
        let copy = tmp.path().join("copy.bin");
        write_atomic(&copy, &std::fs::read(&dest).unwrap()).unwrap();
        assert_eq!(size_of(&copy), 5);
        set_mtime(&copy, 1_000_000).unwrap();
        assert_eq!(mtime_secs(&copy).unwrap(), 1_000_000);
        assert!(is_hidden(OsStr::new(".DS_Store")));
        assert!(!is_hidden(OsStr::new("game.gba")));
    }

    #[test]
    fn overlapping_writes_publish_complete_files_and_failed_writes_preserve_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("save.bin");
        write_via_temp(&dest, |out| {
            out.write_all(b"outer").map_err(|e| Error::io(&dest, e))?;
            write_atomic(&dest, b"inner")?;
            assert_eq!(std::fs::read(&dest).unwrap(), b"inner");
            Ok(())
        })
        .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"outer");
        assert!(
            write_via_temp(&dest, |out| {
                out.write_all(b"partial").map_err(|e| Error::io(&dest, e))?;
                Err(Error::io(&dest, std::io::Error::other("interrupted")))
            })
            .is_err()
        );
        assert!(
            write_new_via_temp(&dest, |out| {
                out.write_all(b"replacement")
                    .map_err(|e| Error::io(&dest, e))
            })
            .is_err()
        );
        assert_eq!(std::fs::read(&dest).unwrap(), b"outer");
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);
    }

    #[test]
    fn visible_files_skips_hidden_entries_and_folders() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.bin", "1");
        write(tmp.path(), "sub/b.bin", "22");
        write(tmp.path(), "sub/.hidden", "x");
        write(tmp.path(), ".git/c.bin", "x");
        let mut rel: Vec<String> = visible_files(tmp.path())
            .map(|p| {
                p.strip_prefix(tmp.path())
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        rel.sort();
        assert_eq!(rel, ["a.bin", "sub/b.bin"]);
        assert_eq!(dir_size(tmp.path()), (4, 5));
    }

    #[test]
    fn formats_known_timestamps() {
        assert_eq!(format_timestamp(0), "19700101-000000");
        assert_eq!(format_timestamp(1_700_000_000), "20231114-221320");
        assert_eq!(format_timestamp(951_782_400), "20000229-000000");
        assert_eq!(timestamp().len(), 15);
    }

    #[test]
    fn junk_rules() {
        for p in [
            "__MACOSX/Cores/x",
            "Cores/a.b/.DS_Store",
            "Cores/a.b/._core.json",
            "Assets/.keep",
            "Assets/gba/.gitkeep",
            "README.md",
            "Cores/a.b/CHANGELOG.md",
            "Cores/a.b/License.MD",
            "readme.txt",
            "Cores/a.b/game.mra",
        ] {
            assert!(is_junk(Path::new(p)), "{p} should be junk");
        }
        for p in [
            "Cores/a.b/core.json",
            "Cores/a.b/notes.txt",
            "Platforms/_images/gba.bin",
            "LICENSE",
            "Assets/genesis/common/Sonic The Hedgehog 2 (World) (Rev A).md",
            "Streets of Rage 2 (USA).MD",
        ] {
            assert!(!is_junk(Path::new(p)), "{p} should be kept");
        }
        assert!(is_sidecar(Path::new("x/game.json")));
        assert!(is_sidecar(Path::new("list.DAT")));
        assert!(!is_sidecar(Path::new("game.gba")));
    }
}
