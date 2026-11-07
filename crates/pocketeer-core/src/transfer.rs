//! Moving games and saves between the library and the device.

use std::path::Path;

use crate::{
    device::{Device, DeviceEntry},
    error::BasicError,
    library::{AddGameError, Library, LibrarySave, LocalEntry},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullGameOutcome {
    /// The game as it is in the library now.
    Pulled(LocalEntry),
    /// The library already has the game, under whatever name.
    AlreadyHad,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushGameOutcome {
    /// The game as it is on the device now.
    Pushed(DeviceEntry),
    /// The device already has the game, at this entry.
    AlreadyThere(DeviceEntry),
}

#[derive(Debug, thiserror::Error)]
pub enum PushGameError {
    #[error("not pushed: another file is where it would go")]
    NameTaken,
    #[error(transparent)]
    Other(#[from] BasicError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullSaveOutcome {
    Pulled,
    /// The library already has a version with the same contents.
    AlreadyHad,
    /// The game has no save on the device.
    NoSave,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushSaveOutcome {
    /// The version pushed.
    Pushed(LibrarySave),
    /// The device's save already has the same contents.
    AlreadyThere,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum PushSaveError {
    /// The device's save isn't in the library, so it was left alone.
    #[error("not pushed: the device's save isn't in the library")]
    NotBackedUp,
    #[error("the library has no such save")]
    NoSuchVersion,
    #[error(transparent)]
    Other(#[from] BasicError),
}

/// Copies `on_device` into the library as the game called `name`. Its save
/// stays on the device until [`pull_save`] brings it.
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

/// Copies `entry` onto the device under its name in `common/`, unless the last
/// scan found it there already. Its save stays in the library until
/// [`push_save`] takes it.
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

/// Keeps `on_device`'s save in the library as a new version.
pub(crate) fn pull_save(
    library: &mut Library,
    device: &Device,
    on_device: &DeviceEntry,
) -> Result<PullSaveOutcome, BasicError> {
    let from = device.save_path(on_device);
    if !from.is_file() {
        return Ok(PullSaveOutcome::NoSave);
    }
    let entry = library
        .entry_for(&on_device.game)
        .ok_or(BasicError::NotInLibrary)?;
    let (platform, name) = (entry.game.platform.clone(), entry.name.clone());
    Ok(match library.add_save(&platform, &name, &from)? {
        Some(_) => PullSaveOutcome::Pulled,
        None => PullSaveOutcome::AlreadyHad,
    })
}

/// Makes `version`, a version of the game's save in the library, the save of
/// `on_device`.
pub(crate) fn push_save(
    library: &Library,
    device: &mut Device,
    on_device: &DeviceEntry,
    version: &LibrarySave,
) -> Result<PushSaveOutcome, PushSaveError> {
    let entry = library
        .entry_for(&on_device.game)
        .ok_or(BasicError::NotInLibrary)?;
    let Some(file) = library.file_of_version(entry, version) else {
        return Err(PushSaveError::NoSuchVersion);
    };
    if let Some(there) = device.read_save(on_device)? {
        if there.has_same_contents_as(&version.save) {
            return Ok(PushSaveOutcome::AlreadyThere);
        }
        if !entry
            .save
            .as_ref()
            .is_some_and(|saves| saves.contains(&there))
        {
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
    use crate::game::Save;
    use crate::os::testing::{at, put_at};

    /// Tetris on a device and in a library. The library has `kept` as its save
    /// and the device has `save_there`, each if given.
    struct Setup {
        _dirs: (tempfile::TempDir, tempfile::TempDir),
        library: Library,
        device: Device,
        on_device: DeviceEntry,
    }

    impl Setup {
        fn pull_save(&mut self) -> Result<PullSaveOutcome, BasicError> {
            pull_save(&mut self.library, &self.device, &self.on_device)
        }

        fn push_save(&mut self, version: &LibrarySave) -> Result<PushSaveOutcome, PushSaveError> {
            push_save(&self.library, &mut self.device, &self.on_device, version)
        }
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
    fn current(setup: &Setup) -> LibrarySave {
        setup.library.entries()[0]
            .save
            .as_ref()
            .unwrap()
            .newest()
            .clone()
    }

    fn kept(setup: &Setup) -> Vec<u64> {
        let entry = &setup.library.entries()[0];
        entry
            .save
            .iter()
            .flat_map(|saves| saves.iter().map(|version| version.save.size))
            .collect()
    }

    mod saves {
        use super::*;

        #[test]
        fn pulls_new_save() {
            let mut setup = setup(Some(b"a"), Some(b"bb"));
            let pulled = setup.pull_save().unwrap();
            assert_eq!(pulled, PullSaveOutcome::Pulled);
            assert_eq!(kept(&setup), [2, 1]);
        }

        #[test]
        fn reports_no_save_and_rejects_pull_without_game() {
            let mut without_save = setup(None, None);
            let pulled = pull_save(
                &mut without_save.library,
                &without_save.device,
                &without_save.on_device,
            );
            assert!(matches!(pulled, Ok(PullSaveOutcome::NoSave)));
            let mut without_game = setup(None, Some(b"a"));
            fs::remove_file(without_game.library.root().join("roms/gba/Tetris.gba")).unwrap();
            without_game.library.reload().unwrap();
            let pulled = pull_save(
                &mut without_game.library,
                &without_game.device,
                &without_game.on_device,
            );
            assert!(matches!(pulled, Err(BasicError::NotInLibrary)));
        }

        #[test]
        fn pushes_kept_version_over_backed_up_save() {
            let mut setup = setup(Some(b"a"), Some(b"bb"));
            setup.pull_save().unwrap();
            let saves = setup.library.entries()[0].save.clone().unwrap();
            let old = saves.iter().nth(1).unwrap().clone();
            let pushed = setup.push_save(&old).unwrap();
            assert_eq!(pushed, PushSaveOutcome::Pushed(old.clone()));
            let now = setup.device.read_save(&setup.on_device).unwrap().unwrap();
            assert_eq!(now, old.save);
            // The device now holds a version the library has.
            let pulled = setup.pull_save().unwrap();
            assert_eq!(pulled, PullSaveOutcome::AlreadyHad);
        }

        #[test]
        fn rejects_push_over_save_not_backed_up() {
            let mut setup = setup(Some(b"a"), Some(b"bb"));
            let kept = current(&setup);
            let pushed = setup.push_save(&kept);
            assert!(matches!(pushed, Err(PushSaveError::NotBackedUp)));
            let now = setup.device.read_save(&setup.on_device).unwrap().unwrap();
            assert_eq!(now.size, 2);
        }

        #[test]
        fn pushes_onto_device_without_save() {
            let mut setup = setup(Some(b"a"), None);
            let kept = current(&setup);
            let pushed = setup.push_save(&kept).unwrap();
            assert_eq!(pushed, PushSaveOutcome::Pushed(kept.clone()));
            assert_eq!(
                setup.device.read_save(&setup.on_device).unwrap(),
                Some(kept.save)
            );
        }

        #[test]
        fn reports_already_there_when_device_has_same_save() {
            let mut setup = setup(Some(b"a"), Some(b"a"));
            let kept = current(&setup);
            let pushed = setup.push_save(&kept).unwrap();
            assert_eq!(pushed, PushSaveOutcome::AlreadyThere);
        }

        #[test]
        fn rejects_push_of_game_not_in_library() {
            let mut setup = setup(Some(b"a"), None);
            let kept = current(&setup);
            fs::remove_file(setup.library.root().join("roms/gba/Tetris.gba")).unwrap();
            setup.library.reload().unwrap();
            let pushed = setup.push_save(&kept);
            assert!(matches!(
                pushed,
                Err(PushSaveError::Other(BasicError::NotInLibrary))
            ));
        }

        #[test]
        fn rejects_version_not_in_library() {
            let mut setup = setup(Some(b"a"), None);
            let unknown = LibrarySave {
                name: "zzz".to_string(),
                save: Save::from_bytes(b"zzz", at(5)),
            };
            let pushed = setup.push_save(&unknown);
            assert!(matches!(pushed, Err(PushSaveError::NoSuchVersion)));
        }
    }

    mod games {
        use super::*;

        #[test]
        fn pulls_under_given_name() {
            let mut setup = setup(None, Some(b"a"));
            fs::remove_file(setup.library.root().join("roms/gba/Tetris.gba")).unwrap();
            setup.library.reload().unwrap();
            let pulled = pull_game(
                &mut setup.library,
                &setup.device,
                &setup.on_device,
                "Tetris (World).gba",
            );
            assert!(matches!(pulled.unwrap(), PullGameOutcome::Pulled(_)));
            let entry = &setup.library.entries()[0];
            assert_eq!(entry.name, "Tetris (World).gba");
            assert_eq!(entry.game, setup.on_device.game);
            assert_eq!(entry.save, None);
        }

        #[test]
        fn skips_pull_of_game_in_library() {
            let mut setup = setup(None, None);
            let pulled = pull_game(
                &mut setup.library,
                &setup.device,
                &setup.on_device,
                "Other.gba",
            );
            assert_eq!(pulled.unwrap(), PullGameOutcome::AlreadyHad);
            assert_eq!(setup.library.entries().len(), 1);
        }

        #[test]
        fn pushes_game_device_lacks() {
            let mut setup = setup(None, None);
            fs::remove_file(setup.device.root().join("Assets/gba/common/Tetris.gba")).unwrap();
            setup.device.scan().unwrap();
            let entry = &setup.library.entries()[0];
            let pushed = push_game(&setup.library, &mut setup.device, entry).unwrap();
            assert!(matches!(pushed, PushGameOutcome::Pushed(_)));
            let on_device = setup.device.root().join("Assets/gba/common/Tetris.gba");
            assert_eq!(fs::read(on_device).unwrap(), b"hello");
        }

        #[test]
        fn reports_game_device_has_in_any_folder() {
            let mut setup = setup(None, None);
            fs::create_dir_all(setup.device.root().join("Assets/gba/common/Puzzle")).unwrap();
            fs::rename(
                setup.device.root().join("Assets/gba/common/Tetris.gba"),
                setup
                    .device
                    .root()
                    .join("Assets/gba/common/Puzzle/Tetris.gba"),
            )
            .unwrap();
            setup.device.scan().unwrap();
            let entries = setup.device.entries().to_vec();
            let entry = &setup.library.entries()[0];
            let pushed = push_game(&setup.library, &mut setup.device, entry).unwrap();
            assert_eq!(pushed, PushGameOutcome::AlreadyThere(entries[0].clone()));
        }

        #[test]
        fn rejects_push_over_other_file() {
            let mut setup = setup(None, None);
            put_at(
                setup.device.root(),
                "Assets/gba/common/Tetris.gba",
                b"another",
                1,
            );
            setup.device.scan().unwrap();
            let entry = &setup.library.entries()[0];
            let pushed = push_game(&setup.library, &mut setup.device, entry);
            assert!(matches!(pushed, Err(PushGameError::NameTaken)));
            let on_disk =
                fs::read(setup.device.root().join("Assets/gba/common/Tetris.gba")).unwrap();
            assert_eq!(on_disk, b"another");
        }
    }
}
