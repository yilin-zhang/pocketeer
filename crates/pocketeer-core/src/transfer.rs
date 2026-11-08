//! Moving games and saves between the library and the device.

use std::path::Path;

use crate::{
    device::{Device, DeviceEntry},
    error::BasicError,
    library::{AddGameError, AddGameOutcome, Library, LibrarySave, LocalEntry},
};

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

#[derive(Debug, thiserror::Error)]
pub enum PushSaveError {
    /// The device's save isn't in the library, so it was left alone.
    #[error("not pushed: the device's save isn't in the library")]
    NotBackedUp,
    #[error("the library has no such save")]
    NoSuchVersion,
    #[error(transparent)]
    Other(#[from] BasicError),
}

/// Copies `device_entry` into the library under its file name on the device,
/// unless the library has the game already. Its save stays on the device until
/// [`pull_save`] brings it.
pub(crate) fn pull_game(
    library: &mut Library,
    device: &Device,
    device_entry: &DeviceEntry,
) -> Result<AddGameOutcome, AddGameError> {
    let name = device_entry
        .path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let from = device.game_path(&device_entry.game.platform, &device_entry.path);
    library.add_game(&device_entry.game, &name, &from)
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

/// Keeps `device_entry`'s save in the library as a new version.
pub(crate) fn pull_save(
    library: &mut Library,
    device: &Device,
    device_entry: &DeviceEntry,
) -> Result<PullSaveOutcome, BasicError> {
    let from = device.save_path(device_entry);
    if !from.is_file() {
        return Ok(PullSaveOutcome::NoSave);
    }
    let entry = library
        .entry_for(&device_entry.game)
        .ok_or(BasicError::NotInLibrary)?;
    let (platform, name) = (entry.game.platform.clone(), entry.name.clone());
    Ok(match library.add_save(&platform, &name, &from)? {
        Some(_) => PullSaveOutcome::Pulled,
        None => PullSaveOutcome::AlreadyHad,
    })
}

/// Makes `version`, a version of the game's save in the library, the save of
/// `device_entry`.
pub(crate) fn push_save(
    library: &Library,
    device: &mut Device,
    device_entry: &DeviceEntry,
    version: &LibrarySave,
) -> Result<PushSaveOutcome, PushSaveError> {
    let entry = library
        .entry_for(&device_entry.game)
        .ok_or(BasicError::NotInLibrary)?;
    let Some(file) = library.file_of_version(entry, version) else {
        return Err(PushSaveError::NoSuchVersion);
    };
    if let Some(there) = device.read_save(device_entry)? {
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
    device.put_save(device_entry, &file)?;
    Ok(PushSaveOutcome::Pushed(version.clone()))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::device::testing::make_empty_device;
    use crate::game::Save;
    use crate::os::testing::{at, put_at};
    use crate::platform::Platform;

    /// Tetris on a device and in a library. The library has `kept` as its save
    /// and the device has `save_there`, each if given.
    struct Setup {
        _dirs: (tempfile::TempDir, tempfile::TempDir),
        library: Library,
        device: Device,
        device_entry: DeviceEntry,
    }

    impl Setup {
        fn pull_save(&mut self) -> Result<PullSaveOutcome, BasicError> {
            pull_save(&mut self.library, &self.device, &self.device_entry)
        }

        fn push_save(&mut self, version: &LibrarySave) -> Result<PushSaveOutcome, PushSaveError> {
            push_save(&self.library, &mut self.device, &self.device_entry, version)
        }

        /// The device's save as it is now.
        fn save_there(&self) -> Vec<u8> {
            fs::read(self.device.save_path(&self.device_entry)).unwrap()
        }

        /// The sizes of the library's versions, newest first.
        fn kept(&self) -> Vec<u64> {
            let entry = &self.library.entries()[0];
            entry
                .save
                .iter()
                .flat_map(|saves| saves.iter().map(|version| version.save.size))
                .collect()
        }

        /// Removes Tetris from the library.
        fn remove_from_library(&mut self) {
            fs::remove_file(self.library.root().join("roms/gba/Tetris.gba")).unwrap();
            self.library.reload().unwrap();
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
        make_empty_device(root);
        put_at(root, "Assets/gba/common/Tetris.gba", b"hello", 1);
        if let Some(bytes) = save_there {
            put_at(root, "Saves/gba/common/Tetris.sav", bytes, 2_000);
        }
        let library = Library::open(lib).unwrap();
        let mut device = Device::open(root).unwrap();
        device.scan().unwrap();
        let device_entry = device.entries()[0].clone();
        Setup {
            _dirs: (lib_dir, device_dir),
            library,
            device,
            device_entry,
        }
    }

    #[test]
    fn saves_round_trip_between_library_and_device() {
        let mut setup = setup(Some(b"one"), None);
        let library_root = setup.library.root().to_path_buf();
        put_at(
            &library_root,
            "saves/gba/Tetris/1970-01-01T00-16-40-2.sav",
            b"two",
            1_000,
        );
        setup.library.reload().unwrap();
        let saves = setup.library.entries()[0].save.clone().unwrap();
        let one = saves.version_named("1970-01-01T00-16-40").unwrap().clone();
        let two = saves
            .version_named("1970-01-01T00-16-40-2")
            .unwrap()
            .clone();
        // A device without a save has nothing to pull.
        assert_eq!(setup.pull_save().unwrap(), PullSaveOutcome::NoSave);
        // A version goes where the Pocket looks for the save, keeping its
        // time, and is listed as a scan finds it.
        assert_eq!(
            setup.push_save(&one).unwrap(),
            PushSaveOutcome::Pushed(one.clone())
        );
        assert_eq!(
            setup.device.save_path(&setup.device_entry),
            setup.device.root().join("Saves/gba/common/Tetris.sav")
        );
        assert_eq!(setup.save_there(), b"one");
        let there = setup.device.read_save(&setup.device_entry).unwrap();
        assert_eq!(there, Some(one.save.clone()));
        assert_eq!(setup.device.entries()[0].save, Some(one.save.clone()));
        setup.device.scan().unwrap();
        assert_eq!(setup.device.entries()[0].save, Some(one.save.clone()));
        // The library has those contents already, and so does the device.
        assert_eq!(setup.pull_save().unwrap(), PullSaveOutcome::AlreadyHad);
        assert_eq!(setup.kept(), [3, 3]);
        assert_eq!(
            setup.push_save(&one).unwrap(),
            PushSaveOutcome::AlreadyThere
        );
        // Versions of the same second are told apart by name.
        setup.push_save(&two).unwrap();
        assert_eq!(setup.save_there(), b"two");
        setup.push_save(&one).unwrap();
        assert_eq!(setup.save_there(), b"one");
        // A save made on the device since isn't replaced until it is pulled.
        let device_root = setup.device.root().to_path_buf();
        put_at(
            &device_root,
            "Saves/gba/common/Tetris.sav",
            b"three!",
            2_000,
        );
        assert!(matches!(
            setup.push_save(&two),
            Err(PushSaveError::NotBackedUp)
        ));
        assert_eq!(setup.save_there(), b"three!");
        assert_eq!(setup.pull_save().unwrap(), PullSaveOutcome::Pulled);
        assert_eq!(setup.kept(), [6, 3, 3]);
        assert_eq!(
            setup.push_save(&two).unwrap(),
            PushSaveOutcome::Pushed(two.clone())
        );
        assert_eq!(setup.save_there(), b"two");
        // A version the library doesn't have isn't pushed.
        let unknown = LibrarySave {
            name: "zzz".to_string(),
            save: Save::from_bytes(b"zzz", at(5)),
        };
        assert!(matches!(
            setup.push_save(&unknown),
            Err(PushSaveError::NoSuchVersion)
        ));
        assert_eq!(setup.save_there(), b"two");
    }

    #[test]
    fn refuses_saves_of_game_not_in_library() {
        let mut setup = setup(Some(b"a"), Some(b"bb"));
        let kept = setup.library.entries()[0]
            .save
            .as_ref()
            .unwrap()
            .newest()
            .clone();
        setup.remove_from_library();
        assert!(matches!(setup.pull_save(), Err(BasicError::NotInLibrary)));
        assert!(matches!(
            setup.push_save(&kept),
            Err(PushSaveError::Other(BasicError::NotInLibrary))
        ));
        assert_eq!(setup.save_there(), b"bb");
    }

    #[test]
    fn games_move_between_library_and_device() {
        let mut setup = setup(None, None);
        setup.remove_from_library();
        let root = setup.device.root().to_path_buf();
        let common = root.join("Assets/gba/common");
        fs::create_dir(common.join("Puzzle")).unwrap();
        fs::rename(common.join("Tetris.gba"), common.join("Puzzle/Tetris.gba")).unwrap();
        put_at(&root, "Saves/gba/common/Puzzle/Tetris.sav", b"a", 2_000);
        setup.device.scan().unwrap();
        let device_entry = setup.device.entries()[0].clone();
        // A game is pulled under its file name, without its folder or its
        // save.
        let pulled = pull_game(&mut setup.library, &setup.device, &device_entry);
        assert!(matches!(pulled.unwrap(), AddGameOutcome::Added(_)));
        let entry = setup.library.entries()[0].clone();
        assert_eq!(entry.name, "Tetris.gba");
        assert_eq!(entry.game, device_entry.game);
        assert_eq!(entry.save, None);
        let pulled = pull_game(&mut setup.library, &setup.device, &device_entry);
        assert_eq!(pulled.unwrap(), AddGameOutcome::AlreadyHad(entry.clone()));
        // The device has it in a folder, so it isn't pushed again.
        let pushed = push_game(&setup.library, &mut setup.device, &entry).unwrap();
        assert_eq!(pushed, PushGameOutcome::AlreadyThere(device_entry));
        // Another file where a game would go is left alone.
        put_at(setup.library.root(), "roms/gba/B.gba", b"bbb", 1);
        setup.library.reload().unwrap();
        let b = setup.library.entry_named(&Platform::Gba, "B.gba").cloned();
        let b = b.unwrap();
        let to = common.join("B.gba");
        put_at(&root, "Assets/gba/common/B.gba", b"another", 1);
        setup.device.scan().unwrap();
        let pushed = push_game(&setup.library, &mut setup.device, &b);
        assert!(matches!(pushed, Err(PushGameError::NameTaken)));
        assert_eq!(fs::read(&to).unwrap(), b"another");
        // Once it is gone, the game is copied and listed at once.
        fs::remove_file(&to).unwrap();
        setup.device.scan().unwrap();
        let pushed = push_game(&setup.library, &mut setup.device, &b).unwrap();
        let PushGameOutcome::Pushed(pushed) = pushed else {
            panic!("{pushed:?}")
        };
        assert_eq!(pushed.path, Path::new("B.gba"));
        assert_eq!(fs::read(&to).unwrap(), b"bbb");
        let listed = setup.device.entries().to_vec();
        assert!(listed.contains(&pushed));
        // Its hash is recorded, so the next scan doesn't read it: other
        // contents of the same size and time still count as the game.
        let time = fs::metadata(&to).unwrap().modified().unwrap();
        fs::write(&to, b"ccc").unwrap();
        let file = fs::File::options().write(true).open(&to).unwrap();
        file.set_modified(time).unwrap();
        setup.device.scan().unwrap();
        assert_eq!(setup.device.entries(), listed);
    }
}
