//! Talking to the operating system: files on disk and mounted volumes.

use std::{
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    process::Command,
    time::SystemTime,
};

use fs_err as fs;

use crate::error::BasicError;

/// What is in a folder, by name, sorted.
#[derive(Default)]
pub(crate) struct Listing {
    pub(crate) dirs: Vec<String>,
    pub(crate) files: Vec<String>,
}

/// What is in `dir`, or nothing if it doesn't exist. Hidden entries (macOS
/// leaves `._` files on devices) and symlinks are left out.
pub(crate) fn list_dir(dir: &Path) -> Result<Listing, BasicError> {
    let read = match fs::read_dir(dir) {
        Ok(read) => read,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Listing::default()),
        Err(error) => return Err(error.into()),
    };
    let (mut dirs, mut files) = (Vec::new(), Vec::new());
    for entry in read {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let file_type = entry.file_type()?;
        if name.starts_with('.') {
            continue;
        }
        if file_type.is_dir() {
            dirs.push(name);
        } else if file_type.is_file() {
            files.push(name);
        }
    }
    dirs.sort();
    files.sort();
    Ok(Listing { dirs, files })
}

/// Every visible file under `dir`, at any depth, as a path from `dir`.
pub(crate) fn list_files_recursive(dir: &Path) -> Result<Vec<PathBuf>, BasicError> {
    let Listing { dirs, files } = list_dir(dir)?;
    let mut paths: Vec<PathBuf> = files.into_iter().map(PathBuf::from).collect();
    for sub in dirs {
        for path in list_files_recursive(&dir.join(&sub))? {
            paths.push(Path::new(&sub).join(path));
        }
    }
    paths.sort();
    Ok(paths)
}

/// Whether a file can be called `name` in a folder. Other names would put it
/// elsewhere, or hide it from [`list_dir`] so that it can't be found again.
pub(crate) fn is_valid_file_name(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('.') && !name.contains(['/', '\\'])
}

/// Whether a file can have `name` on a FAT file system, which devices use.
/// FAT doesn't allow these characters, and macOS silently writes something
/// else instead of refusing them.
pub(crate) fn is_valid_fat_name(name: &str) -> bool {
    is_valid_file_name(name) && !name.contains([':', '?', '*', '"', '<', '>', '|'])
}

/// `name` without its extension.
pub(crate) fn stem(name: &str) -> &str {
    Path::new(name)
        .file_stem()
        .and_then(|file_stem| file_stem.to_str())
        .unwrap_or(name)
}

/// Replaces the file at `path` with `bytes`, last written at `modified` if
/// given.
pub(crate) fn write_atomically(
    path: &Path,
    bytes: &[u8],
    modified: Option<SystemTime>,
) -> Result<(), BasicError> {
    replace_atomically(path, |tmp| fs::write(tmp, bytes), modified)
}

/// Replaces the file at `to` with a copy of `from`, without holding all of
/// it in memory.
pub(crate) fn copy_atomically(from: &Path, to: &Path) -> Result<(), BasicError> {
    replace_atomically(to, |tmp| fs::copy(from, tmp).map(drop), None)
}

/// Renames each pair of old and new paths in turn. If one fails, the earlier
/// ones are put back, so either all are renamed or none, unless the process
/// dies on the way. Nothing is copied, so each path stays on its volume.
pub(crate) fn safe_rename_all(renames: &[(PathBuf, PathBuf)]) -> Result<(), BasicError> {
    for (i, (from, to)) in renames.iter().enumerate() {
        if let Err(error) = fs::rename(from, to) {
            for (from, to) in renames[..i].iter().rev() {
                let _ = fs::rename(to, from);
            }
            return Err(error.into());
        }
    }
    Ok(())
}

/// Replaces the file at `path` with what `fill` writes to a hidden file beside
/// it, so `path` is never half written.
///
/// If this fails the hidden file is removed. If the process dies first,
/// [`list_dir`] doesn't see it and the next write to `path` replaces it.
fn replace_atomically(
    path: &Path,
    fill: impl FnOnce(&Path) -> io::Result<()>,
    modified: Option<SystemTime>,
) -> Result<(), BasicError> {
    let dir = path.parent().expect("files are written into a folder");
    fs::create_dir_all(dir)?;
    let mut name = OsString::from(".");
    name.push(path.file_name().expect("files have a name"));
    name.push(".part");
    let tmp = path.with_file_name(name);
    let replaced = fill(&tmp)
        .and_then(|()| match modified {
            Some(modified) => fs::File::options()
                .write(true)
                .open(&tmp)
                .and_then(|file| file.set_modified(modified)),
            None => Ok(()),
        })
        .and_then(|()| fs::rename(&tmp, path));
    if replaced.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    Ok(replaced?)
}

/// Every volume mounted now, by each system's conventions. On Windows these are
/// the drives, and elsewhere the folders inside the folders that volumes are
/// mounted in.
pub(crate) fn mounted_volumes() -> Vec<PathBuf> {
    let in_dirs = |dirs: &[PathBuf]| -> Vec<PathBuf> {
        dirs.iter()
            .filter_map(|dir| fs::read_dir(dir).ok())
            .flatten()
            .filter_map(|entry| entry.ok().map(|dir_entry| dir_entry.path()))
            .collect()
    };
    match std::env::consts::OS {
        "macos" => in_dirs(&[PathBuf::from("/Volumes")]),
        "linux" => {
            let user = std::env::var("USER").unwrap_or_default();
            in_dirs(&[
                Path::new("/media").join(&user),
                Path::new("/run/media").join(&user),
                PathBuf::from("/media"),
                PathBuf::from("/mnt"),
            ])
        }
        "windows" => (b'A'..=b'Z')
            .map(|drive_letter| PathBuf::from(format!("{}:\\", drive_letter as char)))
            .collect(),
        _ => Vec::new(),
    }
}

/// Has the system eject the volume at `root`, as its file manager would. If
/// it doesn't, the error is what it said.
pub(crate) fn eject_volume(root: &Path) -> Result<(), String> {
    match std::env::consts::OS {
        "macos" => run_command(Command::new("diskutil").arg("eject").arg(root)),
        "linux" => {
            let mut findmnt = Command::new("findmnt");
            findmnt.args(["-n", "-o", "SOURCE", "--target"]).arg(root);
            let block = run_command(&mut findmnt)?;
            let block = block.trim();
            run_command(Command::new("udisksctl").args(["unmount", "-b", block]))?;
            run_command(Command::new("udisksctl").args(["power-off", "-b", block]))
        }
        "windows" => {
            // The drive, e.g. `E:`, as Explorer's Eject wants it.
            let drive = root.to_string_lossy();
            let drive = drive.trim_end_matches(['\\', '/']);
            let script = format!(
                "(New-Object -ComObject Shell.Application).Namespace(17).ParseName('{drive}').InvokeVerb('Eject')"
            );
            run_command(Command::new("powershell").args(["-NoProfile", "-Command", &script]))
        }
        os => Err(format!("ejecting is not supported on {os}")),
    }
    .map(drop)
}

/// Runs `command` and returns what it printed if it succeeded, or else what
/// it said.
fn run_command(command: &mut Command) -> Result<String, String> {
    let out = command.output().map_err(|error| error.to_string())?;
    if !out.status.success() {
        let said = [&out.stderr[..], &out.stdout[..]].concat();
        return Err(String::from_utf8_lossy(&said).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Helpers shared by the tests of modules that keep files on disk.
#[cfg(test)]
pub(crate) mod testing {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;

    pub(crate) fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    /// Writes `bytes` to `path` under `root`, making its folders.
    pub(crate) fn put(root: &Path, path: &str, bytes: &[u8]) -> PathBuf {
        let full = root.join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(&full, bytes).unwrap();
        full
    }

    /// Like [`put`], last written at `secs`.
    pub(crate) fn put_at(root: &Path, path: &str, bytes: &[u8], secs: u64) -> PathBuf {
        let full = put(root, path, bytes);
        fs::File::options()
            .write(true)
            .open(&full)
            .unwrap()
            .set_modified(at(secs))
            .unwrap();
        full
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything in `dir`, hidden or not, sorted.
    fn all_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn leaves_no_temp_file_after_replacing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Foo.gba");
        write_atomically(&path, b"old", None).unwrap();
        write_atomically(&path, b"new", None).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(all_in(dir.path()), ["Foo.gba"]);
    }

    #[test]
    fn hides_and_replaces_temp_file_of_dead_process() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".Foo.gba.part"), b"half").unwrap();
        assert_eq!(list_dir(dir.path()).unwrap().files, Vec::<String>::new());
        write_atomically(&dir.path().join("Foo.gba"), b"whole", None).unwrap();
        assert_eq!(all_in(dir.path()), ["Foo.gba"]);
    }

    #[test]
    fn keeps_old_file_and_removes_temp_after_failed_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Foo.gba");
        fs::write(&path, b"old").unwrap();
        let failed = replace_atomically(
            &path,
            |tmp| {
                fs::write(tmp, b"half").unwrap();
                Err(io::Error::other("full"))
            },
            None,
        );
        assert!(matches!(failed, Err(BasicError::Io(_))));
        assert_eq!(fs::read(&path).unwrap(), b"old");
        assert_eq!(all_in(dir.path()), ["Foo.gba"]);
    }

    #[test]
    fn rejects_characters_fat_cannot_hold() {
        for name in [
            "a:b.gba", "a?.gba", "a*.gba", "a\".gba", "a<.gba", "a>.gba", "a|.gba",
        ] {
            assert!(!is_valid_fat_name(name), "{name}");
        }
        assert!(!is_valid_fat_name("../x.gba"));
        assert!(is_valid_fat_name(
            "Legend of Zelda, The - It's (USA) [!].gba"
        ));
    }
}
