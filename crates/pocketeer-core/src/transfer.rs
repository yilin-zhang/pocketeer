//! Moving games and saves between the library and the device.

use std::path::Path;

use crate::{
    device::{Device, DeviceEntry},
    error::BaseError,
    game::Save,
    library::{AddGameError, Library, LocalEntry},
};

/// What pulling a game did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullGameOutcome {
    /// The game as it is in the library now.
    Pulled(LocalEntry),
    /// The library has the game already, under whatever name.
    AlreadyHad,
}

/// What pushing a game did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushGameOutcome {
    /// The game as it is on the device now.
    Pushed(DeviceEntry),
    /// The device has the game already, here.
    AlreadyThere(DeviceEntry),
}

/// Why pushing a game pushed nothing.
#[derive(Debug, thiserror::Error)]
pub enum PushGameError {
    /// Another file is where the game would go on the device.
    #[error("not pushed: another file is where it would go")]
    NameTaken,
    #[error(transparent)]
    Other(#[from] BaseError),
}

/// What pulling a save did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullSaveOutcome {
    Pulled,
    /// The library has a version with the same contents already.
    AlreadyHad,
}

/// Why pulling a save pulled nothing.
#[derive(Debug, thiserror::Error)]
pub enum PullSaveError {
    /// The game has no save on the device.
    #[error("no save on the device")]
    NoSave,
    /// The library doesn't have the game.
    #[error("not in the library; pull the game first")]
    NotInLibrary,
    #[error(transparent)]
    Other(#[from] BaseError),
}

/// What pushing a save did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushSaveOutcome {
    /// The version pushed.
    Pushed(Save),
    /// The device's save has the same contents already.
    AlreadyThere,
}

/// Why pushing a save pushed nothing.
#[derive(Debug, Clone, thiserror::Error)]
pub enum PushSaveError {
    /// The device's save is not in the library; it was left as it is.
    #[error("not pushed: the device's save isn't in the library")]
    NotBackedUp,
    /// The library doesn't have this version of its save.
    #[error("the library has no such save")]
    NoSuchVersion,
    /// Several versions of its save were written in the second asked for.
    #[error("more than one version of the save was written at {at}")]
    AmbiguousVersion { at: String },
    #[error(transparent)]
    Other(#[from] BaseError),
}

/// Copies `on_device` into the library as the game called `name`. Its save
/// stays on the device; [`pull_save`] brings it.
pub(crate) fn pull_game(
    library: &mut Library,
    device: &Device,
    on_device: &DeviceEntry,
    name: &str,
) -> Result<PullGameOutcome, AddGameError> {
    if library.entry_for(&on_device.game).is_some() {
        return Ok(PullGameOutcome::AlreadyHad);
    }
    let from = device.game_path(&on_device.game.platform, &on_device.path);
    Ok(PullGameOutcome::Pulled(library.add_game(
        &on_device.game,
        name,
        &from,
    )?))
}

/// Copies `entry` onto the device, under its name in `common/`, unless the
/// device has it already, as its last scan found it. Its save stays in the
/// library; [`push_save`] takes it.
pub(crate) fn push_game(
    library: &Library,
    device: &mut Device,
    entry: &LocalEntry,
) -> Result<PushGameOutcome, PushGameError> {
    if let Some(there) = device.entry_for(&entry.game) {
        return Ok(PushGameOutcome::AlreadyThere(there.clone()));
    }
    let path = Path::new(&entry.name);
    if device.game_path(&entry.game.platform, path).exists() {
        return Err(PushGameError::NameTaken);
    }
    let pushed = device.put_game(&entry.game, path, &library.game_path(entry))?;
    Ok(PushGameOutcome::Pushed(pushed))
}

/// Keeps `on_device`'s save in the library as a new version of its save.
pub(crate) fn pull_save(
    library: &mut Library,
    device: &Device,
    on_device: &DeviceEntry,
) -> Result<PullSaveOutcome, PullSaveError> {
    let from = device.save_path(on_device);
    if !from.is_file() {
        return Err(PullSaveError::NoSave);
    }
    let Some(entry) = library.entry_for(&on_device.game) else {
        return Err(PullSaveError::NotInLibrary);
    };
    let (platform, name) = (entry.game.platform.clone(), entry.name.clone());
    Ok(match library.add_save(&platform, &name, &from)? {
        Some(_) => PullSaveOutcome::Pulled,
        None => PullSaveOutcome::AlreadyHad,
    })
}

/// Makes `version`, a version of the library's save for the game, the save
/// of `on_device`.
pub(crate) fn push_save(
    library: &Library,
    device: &mut Device,
    on_device: &DeviceEntry,
    version: &Save,
) -> Result<PushSaveOutcome, PushSaveError> {
    let Some(entry) = library.entry_for(&on_device.game) else {
        return Err(BaseError::NotInLibrary.into());
    };
    let Some(file) = library.file_of_save(entry, version)? else {
        return Err(PushSaveError::NoSuchVersion);
    };
    if let Some(there) = device.read_save(on_device)? {
        if there.has_same_contents_as(version) {
            return Ok(PushSaveOutcome::AlreadyThere);
        }
        if !entry.save.as_ref().is_some_and(|h| h.contains(&there)) {
            return Err(PushSaveError::NotBackedUp);
        }
    }
    device.put_save(on_device, &file)?;
    Ok(PushSaveOutcome::Pushed(version.clone()))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::files::testing::{at, put_at};

    /// Tetris on a device and in a library, which has its save as `kept` if
    /// given; the device has `save_there` as its save if given.
    struct Setup {
        _dirs: (tempfile::TempDir, tempfile::TempDir),
        library: Library,
        device: Device,
        on_device: DeviceEntry,
    }

    fn setup(kept: Option<&[u8]>, save_there: Option<&[u8]>) -> Setup {
        let (lib_dir, device_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (lib, root) = (lib_dir.path(), device_dir.path());
        put_at(lib, "roms/gba/Tetris.gba", b"hello", 1);
        if let Some(bytes) = kept {
            put_at(
                lib,
                "saves/gba/Tetris/1970-01-01T00-16-40.sav",
                bytes,
                1_000,
            );
        }
        put_at(root, "Analogue_Pocket.json", b"{}", 1);
        fs::create_dir(root.join("Cores")).unwrap();
        put_at(root, "Assets/gba/common/Tetris.gba", b"hello", 1);
        if let Some(bytes) = save_there {
            put_at(root, "Saves/gba/common/Tetris.sav", bytes, 2_000);
        }
        let library = Library::open(lib).unwrap();
        let mut device = Device::open(root).unwrap();
        device.scan().unwrap();
        let on_device = device.entries()[0].clone();
        Setup {
            _dirs: (lib_dir, device_dir),
            library,
            device,
            on_device,
        }
    }

    /// The library's newest version of Tetris's save.
    fn current(s: &Setup) -> Save {
        s.library.entries()[0]
            .save
            .as_ref()
            .unwrap()
            .current
            .clone()
    }

    fn kept(s: &Setup) -> Vec<u64> {
        let entry = &s.library.entries()[0];
        entry
            .save
            .iter()
            .flat_map(|h| h.iter().map(|v| v.size))
            .collect()
    }

    mod saves {
        use super::*;

        #[test]
        fn pulls_new_save() {
            let mut s = setup(Some(b"a"), Some(b"bb"));
            let pulled = pull_save(&mut s.library, &s.device, &s.on_device).unwrap();
            assert_eq!(pulled, PullSaveOutcome::Pulled);
            assert_eq!(kept(&s), [2, 1]);
        }

        #[test]
        fn rejects_pull_without_save_or_game() {
            let mut s = setup(None, None);
            let pulled = pull_save(&mut s.library, &s.device, &s.on_device);
            assert!(matches!(pulled, Err(PullSaveError::NoSave)));
            let mut s = setup(None, Some(b"a"));
            fs::remove_file(s.library.root().join("roms/gba/Tetris.gba")).unwrap();
            s.library.reload().unwrap();
            let pulled = pull_save(&mut s.library, &s.device, &s.on_device);
            assert!(matches!(pulled, Err(PullSaveError::NotInLibrary)));
        }

        #[test]
        fn pushes_kept_version_over_backed_up_save() {
            let mut s = setup(Some(b"a"), Some(b"bb"));
            pull_save(&mut s.library, &s.device, &s.on_device).unwrap();
            let old = s.library.entries()[0].save.as_ref().unwrap().versions[0].clone();
            let pushed = push_save(&s.library, &mut s.device, &s.on_device, &old).unwrap();
            assert_eq!(pushed, PushSaveOutcome::Pushed(old.clone()));
            let now = s.device.read_save(&s.on_device).unwrap().unwrap();
            assert_eq!(now, old);
            // The device now holds a version the library has.
            let pulled = pull_save(&mut s.library, &s.device, &s.on_device).unwrap();
            assert_eq!(pulled, PullSaveOutcome::AlreadyHad);
        }

        #[test]
        fn rejects_push_over_save_not_backed_up() {
            let mut s = setup(Some(b"a"), Some(b"bb"));
            let kept = current(&s);
            let pushed = push_save(&s.library, &mut s.device, &s.on_device, &kept);
            assert!(matches!(pushed, Err(PushSaveError::NotBackedUp)));
            let now = s.device.read_save(&s.on_device).unwrap().unwrap();
            assert_eq!(now.size, 2);
        }

        #[test]
        fn pushes_onto_device_without_save() {
            let mut s = setup(Some(b"a"), None);
            let kept = current(&s);
            let pushed = push_save(&s.library, &mut s.device, &s.on_device, &kept).unwrap();
            assert_eq!(pushed, PushSaveOutcome::Pushed(kept.clone()));
            assert_eq!(s.device.read_save(&s.on_device).unwrap(), Some(kept));
        }

        #[test]
        fn reports_already_there_when_device_has_same_save() {
            let mut s = setup(Some(b"a"), Some(b"a"));
            let kept = current(&s);
            let pushed = push_save(&s.library, &mut s.device, &s.on_device, &kept).unwrap();
            assert_eq!(pushed, PushSaveOutcome::AlreadyThere);
        }

        #[test]
        fn rejects_version_not_in_library() {
            let mut s = setup(Some(b"a"), None);
            let unknown = Save::from_bytes(b"zzz", at(5));
            let pushed = push_save(&s.library, &mut s.device, &s.on_device, &unknown);
            assert!(matches!(pushed, Err(PushSaveError::NoSuchVersion)));
        }
    }

    mod games {
        use super::*;

        #[test]
        fn pulls_under_given_name() {
            let mut s = setup(None, Some(b"a"));
            fs::remove_file(s.library.root().join("roms/gba/Tetris.gba")).unwrap();
            s.library.reload().unwrap();
            let pulled = pull_game(
                &mut s.library,
                &s.device,
                &s.on_device,
                "Tetris (World).gba",
            );
            assert!(matches!(pulled.unwrap(), PullGameOutcome::Pulled(_)));
            let entry = &s.library.entries()[0];
            assert_eq!(entry.name, "Tetris (World).gba");
            assert_eq!(entry.game, s.on_device.game);
            assert_eq!(entry.save, None);
        }

        #[test]
        fn skips_pull_of_game_in_library() {
            let mut s = setup(None, None);
            let pulled = pull_game(&mut s.library, &s.device, &s.on_device, "Other.gba");
            assert_eq!(pulled.unwrap(), PullGameOutcome::AlreadyHad);
            assert_eq!(s.library.entries().len(), 1);
        }

        #[test]
        fn pushes_game_device_lacks() {
            let mut s = setup(None, None);
            fs::remove_file(s.device.root().join("Assets/gba/common/Tetris.gba")).unwrap();
            s.device.scan().unwrap();
            let entry = &s.library.entries()[0];
            let pushed = push_game(&s.library, &mut s.device, entry).unwrap();
            assert!(matches!(pushed, PushGameOutcome::Pushed(_)));
        }

        #[test]
        fn reports_game_device_has_in_any_folder() {
            let mut s = setup(None, None);
            fs::create_dir_all(s.device.root().join("Assets/gba/common/Puzzle")).unwrap();
            fs::rename(
                s.device.root().join("Assets/gba/common/Tetris.gba"),
                s.device.root().join("Assets/gba/common/Puzzle/Tetris.gba"),
            )
            .unwrap();
            s.device.scan().unwrap();
            let entries = s.device.entries().to_vec();
            let entry = &s.library.entries()[0];
            let pushed = push_game(&s.library, &mut s.device, entry).unwrap();
            assert_eq!(pushed, PushGameOutcome::AlreadyThere(entries[0].clone()));
        }

        #[test]
        fn rejects_push_over_other_file() {
            let mut s = setup(None, None);
            put_at(
                s.device.root(),
                "Assets/gba/common/Tetris.gba",
                b"another",
                1,
            );
            s.device.scan().unwrap();
            let entry = &s.library.entries()[0];
            let pushed = push_game(&s.library, &mut s.device, entry);
            assert!(matches!(pushed, Err(PushGameError::NameTaken)));
            let on_disk = fs::read(s.device.root().join("Assets/gba/common/Tetris.gba")).unwrap();
            assert_eq!(on_disk, b"another");
        }
    }
}
