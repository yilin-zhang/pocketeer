//! Covers, and where to download them from.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use fs_err as fs;
use image::ImageFormat;
use serde::{Deserialize, Serialize};

use crate::{error::SharedError, files::write_atomically, platform::Platform};

/// The formats a cover can be in, in the order a game's cover is looked for.
pub const COVER_FORMATS: [ImageFormat; 3] =
    [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::WebP];

/// A game's cover image, kept as the file it came in. Drawing it is the
/// front end's job; this only knows what format it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cover {
    bytes: Vec<u8>,
    format: ImageFormat,
}

impl Cover {
    /// `bytes` as a cover, if they are a PNG, JPEG or WebP file.
    pub fn from_bytes(bytes: Vec<u8>) -> Option<Self> {
        let format = image::guess_format(&bytes).ok()?;
        COVER_FORMATS
            .contains(&format)
            .then_some(Self { bytes, format })
    }

    pub fn format(&self) -> ImageFormat {
        self.format
    }

    /// The file as it came.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Where covers come from.
pub trait RemoteCoverSource {
    /// The source's systems that can have covers of `platform`'s games;
    /// none if it has no covers for it.
    fn systems(&self, platform: &Platform) -> &'static [&'static str];

    /// Which version of `system`'s covers the source has now; a different
    /// string means different covers.
    fn version(&self, system: &str) -> Result<String, SharedError>;

    /// The name of every cover of `system` at `version`.
    fn names(&self, system: &str, version: &str) -> Result<Vec<String>, SharedError>;

    /// What the cover of the game with the standard name `game_name` is
    /// called.
    fn cover_name(&self, game_name: &str) -> String;

    fn fetch(&self, system: &str, cover_name: &str) -> Result<Cover, SharedError>;
}

#[derive(Serialize, Deserialize)]
struct Listed {
    version: String,
    names: HashSet<String>,
}

/// Which covers a source has, kept in a file so that a system's list is
/// downloaded again only when it has changed. It only saves time, so a file
/// that can't be read is empty and one that can't be written is not written.
pub struct CoverIndex<S> {
    path: PathBuf,
    source: S,
    /// System → its covers, as last listed.
    systems: HashMap<String, Listed>,
    /// The systems whose version was asked for since this was opened.
    checked: HashSet<&'static str>,
}

impl<S: RemoteCoverSource> CoverIndex<S> {
    /// The index kept at `path`, without going online.
    pub fn open(path: &Path, source: S) -> Self {
        let systems = fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            path: path.to_path_buf(),
            source,
            systems,
            checked: HashSet::new(),
        }
    }

    /// The cover of the game of `platform` with the standard name
    /// `game_name`; none if the source has none. Each system's version is
    /// asked for once while this is open, so the lists may be as old as
    /// that.
    pub fn fetch(
        &mut self,
        platform: &Platform,
        game_name: &str,
    ) -> Result<Option<Cover>, SharedError> {
        let cover_name = self.source.cover_name(game_name);
        for &system in self.source.systems(platform) {
            self.update_list_of(system)?;
            if self.systems[system].names.contains(&cover_name) {
                return self.source.fetch(system, &cover_name).map(Some);
            }
        }
        Ok(None)
    }

    /// Brings `system`'s list up to date, if it wasn't since this was opened.
    fn update_list_of(&mut self, system: &'static str) -> Result<(), SharedError> {
        if self.checked.contains(system) {
            return Ok(());
        }
        let version = self.source.version(system)?;
        if self
            .systems
            .get(system)
            .is_none_or(|l| l.version != version)
        {
            let names = self.source.names(system, &version)?;
            let listed = Listed {
                version,
                names: names.into_iter().collect(),
            };
            self.systems.insert(system.to_string(), listed);
            if let Ok(json) = serde_json::to_vec(&self.systems) {
                let _ = write_atomically(&self.path, &json, None);
            }
        }
        self.checked.insert(system);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use image::RgbaImage;

    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        RgbaImage::new(width, height)
            .write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        bytes
    }

    #[test]
    fn accepts_png() {
        let cover = Cover::from_bytes(png(3, 2)).unwrap();
        assert_eq!(cover.format(), ImageFormat::Png);
    }

    #[test]
    fn rejects_other_formats() {
        assert_eq!(Cover::from_bytes(b"GIF89a....".to_vec()), None);
        assert_eq!(Cover::from_bytes(b"hello".to_vec()), None);
    }

    /// A source held in memory, with two systems for the PC Engine; it
    /// records what it was asked.
    #[derive(Clone, Default)]
    struct Fake {
        systems: Rc<RefCell<HashMap<String, Listed>>>,
        asked: Rc<RefCell<Vec<String>>>,
    }

    impl Fake {
        fn set(&self, system: &str, version: &str, names: &[&str]) {
            let listed = Listed {
                version: version.into(),
                names: names.iter().map(|n| n.to_string()).collect(),
            };
            self.systems.borrow_mut().insert(system.into(), listed);
        }

        /// What it was asked since the last time this was called.
        fn asked(&self) -> Vec<String> {
            self.asked.borrow_mut().drain(..).collect()
        }
    }

    impl RemoteCoverSource for Fake {
        fn systems(&self, platform: &Platform) -> &'static [&'static str] {
            match platform {
                Platform::PcEngine => &["pce", "sgx"],
                _ => &[],
            }
        }

        fn version(&self, system: &str) -> Result<String, SharedError> {
            self.asked.borrow_mut().push(format!("version {system}"));
            Ok(self.systems.borrow()[system].version.clone())
        }

        fn names(&self, system: &str, version: &str) -> Result<Vec<String>, SharedError> {
            self.asked.borrow_mut().push(format!("names {system}"));
            let listed = &self.systems.borrow()[system];
            assert_eq!(listed.version, version);
            Ok(listed.names.iter().cloned().collect())
        }

        fn cover_name(&self, game_name: &str) -> String {
            game_name.replace(':', "_")
        }

        fn fetch(&self, system: &str, cover_name: &str) -> Result<Cover, SharedError> {
            self.asked
                .borrow_mut()
                .push(format!("fetch {system} {cover_name}"));
            Ok(Cover::from_bytes(png(1, 1)).unwrap())
        }
    }

    fn fake() -> Fake {
        let fake = Fake::default();
        fake.set("pce", "1", &["Bonk"]);
        fake.set("sgx", "1", &["Daimakaimura_ Rev 1"]);
        fake
    }

    #[test]
    fn searches_each_system_of_platform() {
        let dir = tempfile::tempdir().unwrap();
        let fake = fake();
        let mut index = CoverIndex::open(&dir.path().join("covers.json"), fake.clone());
        let found = index.fetch(&Platform::PcEngine, "Daimakaimura: Rev 1");
        assert!(found.unwrap().is_some());
        assert_eq!(
            fake.asked(),
            [
                "version pce",
                "names pce",
                "version sgx",
                "names sgx",
                "fetch sgx Daimakaimura_ Rev 1"
            ]
        );
        assert_eq!(index.fetch(&Platform::PcEngine, "Other").unwrap(), None);
        assert_eq!(index.fetch(&Platform::Gba, "Bonk").unwrap(), None);
    }

    #[test]
    fn asks_for_version_once_while_open() {
        let dir = tempfile::tempdir().unwrap();
        let fake = fake();
        let mut index = CoverIndex::open(&dir.path().join("covers.json"), fake.clone());
        index.fetch(&Platform::PcEngine, "Bonk").unwrap();
        fake.asked();
        index.fetch(&Platform::PcEngine, "Bonk").unwrap();
        assert_eq!(fake.asked(), ["fetch pce Bonk"]);
    }

    #[test]
    fn refetches_list_only_when_version_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("covers.json");
        let fake = fake();
        CoverIndex::open(&path, fake.clone())
            .fetch(&Platform::PcEngine, "Bonk")
            .unwrap();
        fake.asked();
        CoverIndex::open(&path, fake.clone())
            .fetch(&Platform::PcEngine, "Bonk")
            .unwrap();
        assert_eq!(fake.asked(), ["version pce", "fetch pce Bonk"]);
        fake.set("pce", "2", &["Bonk", "Bonk 2"]);
        let found = CoverIndex::open(&path, fake.clone()).fetch(&Platform::PcEngine, "Bonk 2");
        assert!(found.unwrap().is_some());
        assert_eq!(
            fake.asked(),
            ["version pce", "names pce", "fetch pce Bonk 2"]
        );
    }

    #[test]
    fn ignores_corrupt_index_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("covers.json");
        fs::write(&path, b"{ not json").unwrap();
        let mut index = CoverIndex::open(&path, fake());
        assert!(index.fetch(&Platform::PcEngine, "Bonk").unwrap().is_some());
    }
}
