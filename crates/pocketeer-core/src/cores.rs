//! Cores on a device and in the online inventory, their release zips, and the
//! BIOS files they load.

use std::{
    collections::HashMap,
    io::{self, Cursor, Read},
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime},
};

use fs_err::{self as fs, File};
use serde::{Deserialize, Serialize};
use zip::ZipArchive;

use crate::{error::SharedError, files::write_atomically, platform::Platform};

/// The cores pocketeer installs for now. Each comes alone in its zip and needs
/// nothing beyond `Cores/` and `Platforms/`, so there are no ROMs, saves or
/// licenses to find.
pub const SUPPORTED: &[&str] = &[
    "Spiritualized.GB",
    "budude2.GB",
    "Spiritualized.GBC",
    "budude2.GBC",
    "Spiritualized.GBA",
    "K3V.GBA",
    "mincer_ray.GBA",
    "Spiritualized.GG",
    "drizzt.GG",
    "Spiritualized.SMS",
    "drizzt.SMS",
    "Spiritualized.Genesis",
    "ericlewis.Genesis",
    "drizzt.MegaDrive",
    "agg23.PC Engine",
    "agg23.SNES",
];

/// The largest zip pocketeer accepts. The cores above take a few MB, and the
/// zips are hosted by third parties.
const MOST_IN_A_ZIP: u64 = 64 * 1024 * 1024;

/// How long a fetched inventory is used before it is fetched again.
const INVENTORY_AGE: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledCore {
    /// The name of its folder, `<author>.<name>`.
    pub id: String,
    /// None if its `core.json` can't be read, for example after an unfinished
    /// install.
    pub version: Option<String>,
    pub platforms: Vec<Platform>,
}

impl InstalledCore {
    /// Whether it is at `version`, ignoring case, surrounding spaces and a
    /// leading `v`.
    pub(crate) fn is_at_version(&self, version: &str) -> bool {
        let plain = |version: &str| version.trim().trim_start_matches(['v', 'V']).to_lowercase();
        self.version.as_deref().map(plain) == Some(plain(version))
    }
}

/// The newest release of a core, as the inventory has it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryCore {
    pub id: String,
    pub version: String,
    pub platforms: Vec<Platform>,
    pub download_url: String,
}

/// One core, on the device, in the inventory, or both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreRow {
    Installed(InstalledCore),
    Available(InventoryCore),
    Both {
        installed: InstalledCore,
        latest: InventoryCore,
    },
}

impl CoreRow {
    /// One row for each core in `latest`, paired with its copy in `installed`
    /// if there is one, followed by a row for each remaining installed core.
    pub(crate) fn pair(mut installed: Vec<InstalledCore>, latest: Vec<InventoryCore>) -> Vec<Self> {
        let mut rows = Vec::new();
        for latest in latest {
            let on_device = installed.iter().position(|core| core.id == latest.id);
            rows.push(match on_device {
                Some(i) => CoreRow::Both {
                    installed: installed.remove(i),
                    latest,
                },
                None => CoreRow::Available(latest),
            });
        }
        rows.extend(installed.into_iter().map(CoreRow::Installed));
        rows
    }

    pub fn id(&self) -> &str {
        match self {
            CoreRow::Installed(InstalledCore { id, .. })
            | CoreRow::Available(InventoryCore { id, .. })
            | CoreRow::Both {
                installed: InstalledCore { id, .. },
                ..
            } => id,
        }
    }

    pub fn installed(&self) -> Option<&InstalledCore> {
        match self {
            CoreRow::Installed(installed) | CoreRow::Both { installed, .. } => Some(installed),
            CoreRow::Available(_) => None,
        }
    }

    pub fn latest(&self) -> Option<&InventoryCore> {
        match self {
            CoreRow::Available(latest) | CoreRow::Both { latest, .. } => Some(latest),
            CoreRow::Installed(_) => None,
        }
    }

    /// Whether the device has it at another version than the newest.
    pub fn has_update(&self) -> bool {
        match self {
            CoreRow::Both { installed, latest } => !installed.is_at_version(&latest.version),
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallCoreOutcome {
    Installed { version: String },
    AlreadyCurrent,
}

#[derive(Debug, thiserror::Error)]
pub enum InstallCoreError {
    /// It isn't one of [`SUPPORTED`], or the inventory doesn't have it.
    #[error("not a core that can be installed")]
    NotSupported,
    #[error("not a zip of this core: {0}")]
    NotACoreZip(String),
    #[error(transparent)]
    Shared(#[from] SharedError),
}

#[derive(Debug, thiserror::Error)]
pub enum RemoveCoreError {
    #[error("not on the device")]
    NotInstalled,
    #[error(transparent)]
    Shared(#[from] SharedError),
}

/// A file a core on the device says it loads, which only the user can provide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CoreFile {
    pub(crate) platform: Platform,
    /// The path under `common/`.
    pub(crate) path: PathBuf,
    pub(crate) required: bool,
    /// The only size the core takes.
    pub(crate) size_exact: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BiosStatus {
    Present,
    /// The file exists, but the core won't load it at this size.
    WrongSize {
        size: u64,
    },
    Missing,
}

/// A file that one or more cores on the device need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BiosRow {
    pub platform: Platform,
    /// The path under `common/`.
    pub path: PathBuf,
    /// The cores that load it, and whether each one requires it.
    pub needed_by: Vec<(String, bool)>,
    /// None if no core sets a size, or if cores set different ones.
    pub size_exact: Option<u64>,
    pub status: BiosStatus,
}

impl BiosRow {
    /// Whether any core requires it.
    pub fn is_required(&self) -> bool {
        self.needed_by.iter().any(|(_, required)| *required)
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum PutBiosError {
    #[error("no core on the device loads this file")]
    NotDeclared,
    #[error("the device has this file already")]
    AlreadyThere,
    #[error("the file has {actual} bytes, and the core takes {expected}")]
    WrongSize { expected: u64, actual: u64 },
    #[error(transparent)]
    Shared(#[from] SharedError),
}

pub trait RemoteCoreSource {
    /// Every core with a release, at its newest.
    fn inventory(&self) -> Result<Vec<InventoryCore>, SharedError>;

    fn download(&self, url: &str) -> Result<Vec<u8>, SharedError>;
}

/// The inventory, cached in a file. The file only saves time, so one that can't
/// be read counts as empty and a failed write is ignored.
pub struct CoreInventory<S> {
    path: PathBuf,
    source: S,
    cores: Vec<InventoryCore>,
    /// When the inventory was last fetched, including failed attempts. It
    /// starts from what the file says, if the file can be read.
    asked: Option<SystemTime>,
}

impl<S: RemoteCoreSource> CoreInventory<S> {
    /// The inventory kept at `path`, without going online.
    pub fn open(path: &Path, source: S) -> Self {
        let cores: Option<Vec<InventoryCore>> = fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        let asked = cores.as_ref().and_then(|_| {
            let meta = fs::metadata(path).ok()?;
            meta.modified().ok()
        });
        Self {
            path: path.to_path_buf(),
            source,
            cores: cores.unwrap_or_default(),
            asked,
        }
    }

    /// The cores in [`SUPPORTED`] that the inventory has, in that order.
    ///
    /// An old inventory is fetched again. If that fails, the cached copy is
    /// used, and if there is none the error is returned.
    pub fn supported_cores(&mut self) -> Result<Vec<InventoryCore>, SharedError> {
        let fresh = |asked: SystemTime| asked.elapsed().is_ok_and(|age| age < INVENTORY_AGE);
        if !self.asked.is_some_and(fresh) {
            self.asked = Some(SystemTime::now());
            match self.source.inventory() {
                Ok(cores) => {
                    if let Ok(json) = serde_json::to_vec(&cores) {
                        let _ = write_atomically(&self.path, &json, None);
                    }
                    self.cores = cores;
                }
                Err(error) if self.cores.is_empty() => {
                    self.asked = None;
                    return Err(error);
                }
                Err(_) => {}
            }
        }
        let by_id: HashMap<&str, &InventoryCore> = self
            .cores
            .iter()
            .map(|core| (core.id.as_str(), core))
            .collect();
        Ok(SUPPORTED
            .iter()
            .filter_map(|id| by_id.get(id).map(|core| (*core).clone()))
            .collect())
    }

    pub fn download(&self, url: &str) -> Result<Vec<u8>, SharedError> {
        self.source.download(url)
    }
}

/// A core's release zip after validation. Every path in it stays inside the
/// install location, and it holds this core at the version its `core.json`
/// states.
pub struct CoreZip {
    version: String,
    archive: ZipArchive<Cursor<Vec<u8>>>,
    /// Each file's install path under the device root and its name in the zip,
    /// with `core.json` last.
    files: Vec<(PathBuf, String)>,
}

impl CoreZip {
    pub fn parse(bytes: Vec<u8>, id: &str) -> Result<Self, InstallCoreError> {
        let bad = |why: String| InstallCoreError::NotACoreZip(why);
        let mut archive =
            ZipArchive::new(Cursor::new(bytes)).map_err(|error| bad(error.to_string()))?;
        let core = Path::new("Cores").join(id);
        let core_json = core.join("core.json");
        let (mut files, mut json_name, mut total) = (Vec::new(), None, 0);
        for i in 0..archive.len() {
            let file = archive
                .by_index(i)
                .map_err(|error| bad(error.to_string()))?;
            let name = file
                .name()
                .map_err(|error| bad(error.to_string()))?
                .into_owned();
            let Some(path) = file.enclosed_name() else {
                return Err(bad(format!("{name} goes outside the device")));
            };
            total += file.size();
            if file.is_dir() || is_hidden(&path) {
                continue;
            }
            if path == core_json {
                json_name = Some((path, name));
            } else if path.starts_with(&core) || path.starts_with("Platforms") {
                files.push((path, name));
            }
        }
        if total > MOST_IN_A_ZIP {
            return Err(bad(format!("it holds {total} bytes")));
        }
        let Some((json_path, json_name)) = json_name else {
            return Err(bad(format!("no {}", core_json.display())));
        };
        let mut json = Vec::new();
        archive
            .by_name(&json_name)
            .map_err(|error| bad(error.to_string()))?
            .read_to_end(&mut json)
            .map_err(|error| bad(error.to_string()))?;
        let (version, _) =
            parse_core_json(&json).ok_or_else(|| bad("its core.json can't be read".to_string()))?;
        files.push((json_path, json_name));
        Ok(Self {
            version,
            archive,
            files,
        })
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub(crate) fn files(&self) -> Vec<(PathBuf, String)> {
        self.files.clone()
    }

    /// Writes the file called `name` to `to`, creating its parent folders.
    pub(crate) fn extract(&mut self, name: &str, to: &Path) -> io::Result<()> {
        let mut file = self.archive.by_name(name).map_err(io::Error::other)?;
        if let Some(dir) = to.parent() {
            fs::create_dir_all(dir)?;
        }
        io::copy(&mut file, &mut File::create(to)?).map(drop)
    }
}

/// Whether any part of `path` is hidden or is a macOS zip leftover.
fn is_hidden(path: &Path) -> bool {
    path.components().any(|component| match component {
        Component::Normal(part) => {
            let part = part.to_string_lossy();
            part.starts_with('.') || part == "__MACOSX"
        }
        _ => false,
    })
}

#[derive(Deserialize)]
struct CoreJson {
    core: CoreSection,
}

#[derive(Deserialize)]
struct CoreSection {
    metadata: CoreMetadata,
}

#[derive(Deserialize)]
struct CoreMetadata {
    version: String,
    #[serde(default)]
    platform_ids: Vec<String>,
}

/// Parses a core's JSON file, which may start with a byte order mark.
fn parse_json<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Option<T> {
    let text = std::str::from_utf8(bytes).ok()?;
    serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()
}

pub(crate) fn parse_core_json(bytes: &[u8]) -> Option<(String, Vec<Platform>)> {
    let core: CoreJson = parse_json(bytes)?;
    let metadata = core.core.metadata;
    let platforms = metadata.platform_ids.iter();
    Some((
        metadata.version.clone(),
        platforms.map(|id| Platform::from_id(id)).collect(),
    ))
}

#[derive(Deserialize)]
struct DataJson {
    data: DataSection,
}

#[derive(Deserialize)]
struct DataSection {
    data_slots: Vec<DataSlot>,
}

/// A data slot, in the terms of Analogue's docs, which is a file the core
/// loads.
#[derive(Deserialize)]
struct DataSlot {
    /// Only slots with a fixed file have a name.
    filename: Option<String>,
    #[serde(default)]
    required: bool,
    parameters: Option<Number>,
    /// Whether the core also writes it, which makes it a save.
    #[serde(default)]
    nonvolatile: bool,
    size_exact: Option<Number>,
}

impl DataSlot {
    /// The file this slot stands for, if any.
    ///
    /// The name and platform come from a third party's zip and become part of a
    /// path, so only plain names and known platforms are accepted. Only files
    /// in `common/` count, not files in the core's own folder, which no core in
    /// [`SUPPORTED`] has.
    fn to_core_file(&self, platforms: &[Platform]) -> Option<CoreFile> {
        let name = self.filename.as_deref().filter(|_| !self.nonvolatile)?;
        let parameters = match &self.parameters {
            Some(parameters) => parameters.value()?,
            None => 0,
        };
        // Bits 24 and 25 pick one of the core's platforms.
        let platform = platforms.get((parameters >> 24 & 0b11) as usize)?;
        let mut parts = Path::new(name).components();
        let plain = matches!(parts.next(), Some(Component::Normal(_))) && parts.next().is_none();
        // Bit 1 says the file is the core's own.
        let own = parameters & 2 != 0;
        if own || matches!(platform, Platform::Other(_)) || !plain || name.starts_with('.') {
            return None;
        }
        Some(CoreFile {
            platform: platform.clone(),
            path: PathBuf::from(name),
            required: self.required,
            // Zero doesn't mean an empty file, because cartridge slots use it
            // too.
            size_exact: (self.size_exact.as_ref())
                .and_then(Number::value)
                .filter(|&size| size > 0),
        })
    }
}

/// A number in a `data.json`, written either as a number or as a hex string.
#[derive(Deserialize)]
#[serde(untagged)]
enum Number {
    Plain(u64),
    Written(String),
}

impl Number {
    fn value(&self) -> Option<u64> {
        match self {
            Number::Plain(number) => Some(*number),
            Number::Written(text) => match text.strip_prefix("0x") {
                Some(hex) => u64::from_str_radix(hex, 16).ok(),
                None => text.parse().ok(),
            },
        }
    }
}

/// The files a core for `platforms` loads according to its `data.json`, or none
/// if it can't be read.
pub(crate) fn parse_core_files(platforms: &[Platform], bytes: &[u8]) -> Vec<CoreFile> {
    let data: Option<DataJson> = parse_json(bytes);
    let slots = data.map_or(Vec::new(), |data_file| data_file.data.data_slots);
    slots
        .iter()
        .filter_map(|slot| slot.to_core_file(platforms))
        .collect()
}

/// Helpers shared by the tests of modules that install cores.
#[cfg(test)]
pub(crate) mod testing {
    use std::io::Write;

    use zip::{ZipWriter, write::SimpleFileOptions};

    pub(crate) fn core_json(version: &str) -> String {
        format!(r#"{{"core":{{"metadata":{{"version":"{version}","platform_ids":["gba"]}}}}}}"#)
    }

    pub(crate) fn zip_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut zip = ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, bytes) in files {
            zip.start_file(*name, SimpleFileOptions::default()).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    pub(crate) fn data_json(slots: &str) -> String {
        format!(r#"{{"data":{{"data_slots":[{slots}]}}}}"#)
    }

    /// The zip of a core `a.B` at `version`.
    pub(crate) fn release(version: &str) -> Vec<u8> {
        zip_of(&[
            ("Cores/a.B/core.json", core_json(version).as_bytes()),
            ("Cores/a.B/core.rbf_r", version.as_bytes()),
            ("Platforms/gba.json", b"{}"),
            ("Platforms/_images/gba.bin", b"image"),
            ("README.md", b"readme"),
            ("docs/more.txt", b"more"),
            ("Saves/gba/common/x.sav", b"save"),
            ("Assets/gba/common/x.bin", b"asset"),
            ("Cores/other.C/core.json", b"{}"),
            ("Cores/a.B/.DS_Store", b"junk"),
            ("__MACOSX/Cores/a.B/core.rbf_r", b"junk"),
        ])
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::{testing::*, *};
    use crate::files::testing::{put, put_at};

    mod core_files {
        use super::*;

        /// What these slots of a core for GBA declare.
        fn files(slots: &str) -> Vec<CoreFile> {
            parse_core_files(&[Platform::Gba], data_json(slots).as_bytes())
        }

        #[test]
        fn parses_real_slot() {
            assert_eq!(
                files(
                    r#"{"id":"0x103","name":"BIOS","filename":"gba_bios.bin","required":true,
                        "parameters":137,"nonvolatile":false,"size_exact":16384,
                        "extensions":["bin"]}"#
                ),
                [CoreFile {
                    platform: Platform::Gba,
                    path: PathBuf::from("gba_bios.bin"),
                    required: true,
                    size_exact: Some(16384),
                }]
            );
        }

        #[test]
        fn defaults_missing_fields_to_optional_and_any_size() {
            assert_eq!(
                files(r#"{"filename":"x.bin"}"#),
                [CoreFile {
                    platform: Platform::Gba,
                    path: PathBuf::from("x.bin"),
                    required: false,
                    size_exact: None,
                }]
            );
        }

        #[test]
        fn parses_numbers_written_as_hex() {
            let file = files(r#"{"filename":"x.bin","parameters":"0x89","size_exact":"0x4000"}"#);
            assert_eq!(file[0].path, Path::new("x.bin"));
            assert_eq!(file[0].size_exact, Some(16384));
        }

        #[test]
        fn leaves_out_core_specific_save_and_unnamed_slots() {
            for slot in [
                r#"{"filename":"x.bin","parameters":2}"#,
                r#"{"filename":"x.sav","nonvolatile":true}"#,
                r#"{"name":"Cartridge","required":true,"parameters":"0x109"}"#,
            ] {
                assert_eq!(files(slot), [], "{slot}");
            }
        }

        #[test]
        fn treats_zero_size_as_no_required_size() {
            let file = files(r#"{"filename":"x.bin","size_exact":0}"#);
            assert_eq!(file[0].size_exact, None);
        }

        #[test]
        fn picks_platform_by_parameters_index() {
            let json = data_json(r#"{"filename":"x.bin","parameters":16777216}"#);
            let pick = |platforms: &[Platform]| parse_core_files(platforms, json.as_bytes());
            let picked = pick(&[Platform::Gb, Platform::Gba]);
            assert_eq!(picked[0].platform, Platform::Gba);
            assert_eq!(pick(&[Platform::Gb]), []);
            // The bits above those aren't the index.
            let json = data_json(r#"{"filename":"x.bin","parameters":"0x4000000"}"#);
            let files = parse_core_files(&[Platform::Gba], json.as_bytes());
            assert_eq!(files[0].platform, Platform::Gba);
        }

        #[test]
        fn rejects_paths_outside_dir_and_unlisted_platforms() {
            for name in ["../x.bin", "a/x.bin", "..", ".", "", "/x.bin", ".x.bin"] {
                let slot = format!(r#"{{"filename":"{name}"}}"#);
                assert_eq!(files(&slot), [], "{name}");
            }
            let other = [Platform::Other("../Saves".into())];
            let json = data_json(r#"{"filename":"x.bin"}"#);
            assert_eq!(parse_core_files(&other, json.as_bytes()), []);
        }

        #[test]
        fn returns_nothing_for_unreadable_json() {
            assert_eq!(parse_core_files(&[Platform::Gba], b"{ not json"), []);
            assert_eq!(parse_core_files(&[Platform::Gba], b""), []);
        }
    }

    fn installed(version: Option<&str>) -> InstalledCore {
        InstalledCore {
            id: "a.B".into(),
            version: version.map(String::from),
            platforms: Vec::new(),
        }
    }

    fn latest(id: &str, version: &str) -> InventoryCore {
        InventoryCore {
            id: id.into(),
            version: version.into(),
            platforms: vec![Platform::Gba],
            download_url: "https://example.com/x.zip".into(),
        }
    }

    mod rows {
        use super::*;

        fn update_available(on_device: Option<&str>, newest: &str) -> bool {
            CoreRow::Both {
                installed: installed(on_device),
                latest: latest("a.B", newest),
            }
            .has_update()
        }

        #[test]
        fn decides_update_by_comparing_versions() {
            for (on_device, newest, expected) in [
                (Some("1.2.0"), "1.3.0", true),
                // Any difference counts, whichever is newer, because versions
                // aren't always numbers.
                (Some("1.3.0"), "1.2.0", true),
                (Some("1.2.0"), "1.2.0", false),
                (Some("v1.2.0"), "1.2.0", false),
                (Some(" 1.2B "), "V1.2b", false),
                (None, "1.2.0", true),
            ] {
                assert_eq!(
                    update_available(on_device, newest),
                    expected,
                    "{on_device:?} and {newest}"
                );
            }
        }

        #[test]
        fn has_no_update_unless_on_both_sides() {
            assert!(!CoreRow::Installed(installed(Some("1.0"))).has_update());
            assert!(!CoreRow::Available(latest("a.B", "1.0")).has_update());
        }
    }

    mod zips {
        use super::*;

        fn paths(zip: &CoreZip) -> Vec<String> {
            let files = zip.files();
            files
                .iter()
                .map(|(path, _)| path.display().to_string())
                .collect()
        }

        fn why_not(bytes: Vec<u8>) -> String {
            match CoreZip::parse(bytes, "a.B") {
                Err(InstallCoreError::NotACoreZip(why)) => why,
                Ok(_) => panic!("it was taken"),
                Err(error) => panic!("{error}"),
            }
        }

        #[test]
        fn keeps_only_core_and_platform_files_with_core_json_last() {
            let zip = CoreZip::parse(release("1.2.0"), "a.B").unwrap();
            assert_eq!(zip.version(), "1.2.0");
            assert_eq!(
                paths(&zip),
                [
                    "Cores/a.B/core.rbf_r",
                    "Platforms/gba.json",
                    "Platforms/_images/gba.bin",
                    "Cores/a.B/core.json",
                ]
            );
        }

        #[test]
        fn parses_core_json_with_byte_order_mark() {
            let json = format!("\u{feff}{}", core_json("1.2.0"));
            let bytes = zip_of(&[("Cores/a.B/core.json", json.as_bytes())]);
            assert_eq!(CoreZip::parse(bytes, "a.B").unwrap().version(), "1.2.0");
        }

        #[test]
        fn rejects_what_is_not_zip_of_this_core() {
            let core_json = core_json("1");
            for (bytes, reason) in [
                (
                    zip_of(&[
                        ("Cores/a.B/core.json", core_json.as_bytes()),
                        ("../../evil", b"x"),
                    ]),
                    "outside",
                ),
                (
                    zip_of(&[("Cores/other.C/core.json", core_json.as_bytes())]),
                    "core.json",
                ),
                (
                    zip_of(&[("Cores/a.B/core.json", b"{ not json")]),
                    "can't be read",
                ),
                (b"hello".to_vec(), ""),
            ] {
                let why = why_not(bytes);
                assert!(why.contains(reason), "{why}");
            }
        }
    }

    mod inventory {
        use super::*;

        /// A source with `Spiritualized.GB` at this version, or failing. It
        /// counts its calls.
        struct Fake {
            version: Option<&'static str>,
            asked: Cell<u32>,
        }

        impl Fake {
            fn at(version: &'static str) -> Self {
                Fake {
                    version: Some(version),
                    asked: Cell::new(0),
                }
            }

            fn failing() -> Self {
                Fake {
                    version: None,
                    asked: Cell::new(0),
                }
            }
        }

        impl RemoteCoreSource for &Fake {
            fn inventory(&self) -> Result<Vec<InventoryCore>, SharedError> {
                self.asked.set(self.asked.get() + 1);
                match self.version {
                    Some(version) => Ok(vec![
                        latest("Spiritualized.GB", version),
                        latest("someone.Else", "1"),
                    ]),
                    None => Err(SharedError::NoDevice),
                }
            }

            fn download(&self, _: &str) -> Result<Vec<u8>, SharedError> {
                unreachable!()
            }
        }

        fn versions(cores: &[InventoryCore]) -> Vec<&str> {
            cores.iter().map(|core| core.version.as_str()).collect()
        }

        /// A file holding `Spiritualized.GB` at 1.0, written long ago.
        fn old_file(dir: &Path) -> PathBuf {
            let cores = [latest("Spiritualized.GB", "1.0")];
            let json = serde_json::to_vec(&cores).unwrap();
            put_at(dir, "cores.json", &json, 1_000)
        }

        #[test]
        fn returns_supported_cores_but_keeps_all_in_file() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("cores.json");
            let fake = Fake::at("2.0");
            let got = CoreInventory::open(&path, &fake).supported_cores().unwrap();
            assert_eq!(got.len(), 1);
            assert_eq!(got[0].id, "Spiritualized.GB");
            let kept: Vec<InventoryCore> =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            assert_eq!(kept.len(), 2);
        }

        #[test]
        fn does_not_fetch_again_within_the_hour() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("cores.json");
            let fake = Fake::at("2.0");
            CoreInventory::open(&path, &fake).supported_cores().unwrap();
            let again = CoreInventory::open(&path, &fake).supported_cores().unwrap();
            assert_eq!(versions(&again), ["2.0"]);
            assert_eq!(fake.asked.get(), 1);
        }

        #[test]
        fn fetches_again_when_file_is_old() {
            let dir = tempfile::tempdir().unwrap();
            let fake = Fake::at("2.0");
            let got = CoreInventory::open(&old_file(dir.path()), &fake).supported_cores();
            assert_eq!(versions(&got.unwrap()), ["2.0"]);
            assert_eq!(fake.asked.get(), 1);
        }

        #[test]
        fn uses_old_inventory_without_retrying_when_fetch_fails() {
            let dir = tempfile::tempdir().unwrap();
            let fake = Fake::failing();
            let mut inventory = CoreInventory::open(&old_file(dir.path()), &fake);
            assert_eq!(versions(&inventory.supported_cores().unwrap()), ["1.0"]);
            assert_eq!(versions(&inventory.supported_cores().unwrap()), ["1.0"]);
            assert_eq!(fake.asked.get(), 1);
        }

        #[test]
        fn returns_error_when_fetch_fails_with_nothing_kept() {
            let dir = tempfile::tempdir().unwrap();
            let fake = Fake::failing();
            let mut inventory = CoreInventory::open(&dir.path().join("cores.json"), &fake);
            assert!(inventory.supported_cores().is_err());
        }

        #[test]
        fn ignores_corrupt_inventory_file() {
            let dir = tempfile::tempdir().unwrap();
            // Written just now, so only being corrupt makes it fetch again.
            let path = put(dir.path(), "cores.json", b"{ not json");
            let fake = Fake::at("2.0");
            let got = CoreInventory::open(&path, &fake).supported_cores().unwrap();
            assert_eq!(versions(&got), ["2.0"]);
        }
    }
}
