//! Cores on a device and in the online index, their release zips, and the
//! BIOS files they load.

use std::{
    collections::HashMap,
    fmt,
    io::{self, Cursor, Read},
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime},
};

use fs_err::{self as fs, File};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use zip::ZipArchive;

use crate::{error::BasicError, os, platform::Platform};

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

/// The newest release of a core, as the index has it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteCore {
    pub id: String,
    pub version: String,
    pub platforms: Vec<Platform>,
    pub download_url: String,
}

impl RemoteCore {
    /// The cores Pocketeer installs for now. Each comes alone in its zip and needs
    /// nothing beyond `Cores/` and `Platforms/`, so there are no ROMs, saves or
    /// licenses to find.
    pub const SUPPORTED_IDS: &[&str] = &[
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

    pub fn is_supported(id: &str) -> bool {
        Self::SUPPORTED_IDS.contains(&id)
    }
}

/// One core, on the device, in the index, or both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreRow {
    Installed(InstalledCore),
    Available(RemoteCore),
    Both {
        installed: InstalledCore,
        latest: RemoteCore,
    },
}

impl CoreRow {
    /// One row for each core in `remote_cores`, paired with its copy in
    /// `installed` if there is one, followed by a row for each remaining
    /// installed core.
    pub(crate) fn pair(
        mut installed: Vec<InstalledCore>,
        remote_cores: Vec<RemoteCore>,
    ) -> Vec<Self> {
        let mut rows = Vec::new();
        for latest in remote_cores {
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
            | CoreRow::Available(RemoteCore { id, .. })
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

    pub fn latest(&self) -> Option<&RemoteCore> {
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

/// Every core, as far as it could be found out.
#[derive(Debug)]
pub struct CoreList {
    pub rows: Vec<CoreRow>,
    /// Why the index couldn't be fetched, if it couldn't.
    pub index_failure: Option<BasicError>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallCoreOutcome {
    /// Installed at this version.
    Installed(String),
    AlreadyCurrent,
}

#[derive(Debug, thiserror::Error)]
pub enum InstallCoreError {
    /// It isn't one of [`RemoteCore::SUPPORTED_IDS`], or the index doesn't have it.
    #[error("not a core that can be installed")]
    NotSupported,
    #[error("not a zip of this core: {0}")]
    NotACoreZip(String),
    #[error(transparent)]
    Other(#[from] BasicError),
}

#[derive(Debug, thiserror::Error)]
pub enum RemoveCoreError {
    #[error("not on the device")]
    NotInstalled,
    #[error(transparent)]
    Other(#[from] BasicError),
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
    /// The file exists at this size, but the core won't load it.
    WrongSize(u64),
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

/// As `<platform>/<path>`, the way the command line, the log and messages
/// name it.
impl fmt::Display for BiosRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.platform.id(), self.path.display())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PutBiosError {
    #[error("no core on the device loads this file")]
    NotDeclared,
    #[error("the device has this file already")]
    AlreadyThere,
    #[error("the file has {actual} bytes, and the core takes {expected}")]
    WrongSize { expected: u64, actual: u64 },
    #[error(transparent)]
    Other(#[from] BasicError),
}

pub trait RemoteCoreSource {
    /// Every core with a release, at its newest.
    fn index(&self) -> Result<Vec<RemoteCore>, BasicError>;

    /// The bytes of `core`'s release zip.
    fn download(&self, core: &RemoteCore) -> Result<Vec<u8>, BasicError>;
}

/// The index, cached in a file. The file only saves time, so one that can't
/// be read counts as empty and a failed write is ignored.
pub struct CoreIndex<S> {
    path: PathBuf,
    source: S,
    cores: Vec<RemoteCore>,
    /// When the index was last fetched, including failed attempts. It
    /// starts from what the file says, if the file can be read.
    asked: Option<SystemTime>,
}

impl<S: RemoteCoreSource> CoreIndex<S> {
    /// How long a fetched index is used before it is fetched again.
    const MAX_AGE: Duration = Duration::from_secs(60 * 60);

    /// The index kept at `path`, without going online.
    pub fn open(path: &Path, source: S) -> Self {
        let cores: Option<Vec<RemoteCore>> = fs::read(path)
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

    /// The cores in [`RemoteCore::SUPPORTED_IDS`] that the index has, in that order.
    ///
    /// An old index is fetched again. If that fails, the cached copy is
    /// used, and if there is none the error is returned.
    pub fn supported_cores(&mut self) -> Result<Vec<RemoteCore>, BasicError> {
        let fresh = |asked: SystemTime| asked.elapsed().is_ok_and(|age| age < Self::MAX_AGE);
        if !self.asked.is_some_and(fresh) {
            self.asked = Some(SystemTime::now());
            match self.source.index() {
                Ok(cores) => {
                    if let Ok(json) = serde_json::to_vec(&cores) {
                        let _ = os::write_atomically(&self.path, &json, None);
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
        let by_id: HashMap<&str, &RemoteCore> = self
            .cores
            .iter()
            .map(|core| (core.id.as_str(), core))
            .collect();
        Ok(RemoteCore::SUPPORTED_IDS
            .iter()
            .filter_map(|id| by_id.get(id).map(|core| (*core).clone()))
            .collect())
    }

    /// Downloads the release zip of `core`.
    pub fn download(&self, core: &RemoteCore) -> Result<CoreZip, InstallCoreError> {
        CoreZip::parse(self.source.download(core)?, &core.id)
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
    /// The largest zip Pocketeer accepts. The cores in [`RemoteCore::SUPPORTED_IDS`] take a few
    /// MB, and the zips are hosted by third parties.
    const MOST_BYTES: u64 = 64 * 1024 * 1024;

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
        if total > Self::MOST_BYTES {
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
struct CoreMetadata {
    version: String,
    #[serde(default)]
    platform_ids: Vec<String>,
}

/// Parses the part at `pointer` of a core's JSON file, which may start with a
/// byte order mark.
fn parse_json<T: DeserializeOwned>(bytes: &[u8], pointer: &str) -> Option<T> {
    let text = std::str::from_utf8(bytes).ok()?;
    let json: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
    T::deserialize(json.pointer(pointer)?).ok()
}

pub(crate) fn parse_core_json(bytes: &[u8]) -> Option<(String, Vec<Platform>)> {
    let metadata: CoreMetadata = parse_json(bytes, "/core/metadata")?;
    let platforms = metadata.platform_ids.iter();
    Some((
        metadata.version.clone(),
        platforms.map(|id| Platform::from_id(id)).collect(),
    ))
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
    /// [`RemoteCore::SUPPORTED_IDS`] has.
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
        if own || matches!(platform, Platform::Unsupported(_)) || !plain || name.starts_with('.') {
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
    let slots: Vec<DataSlot> = parse_json(bytes, "/data/data_slots").unwrap_or_default();
    slots
        .iter()
        .filter_map(|slot| slot.to_core_file(platforms))
        .collect()
}

/// Helpers shared by the tests of modules that install cores.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use std::io::Write;

    use zip::{ZipWriter, write::SimpleFileOptions};

    use super::RemoteCore;
    use crate::platform::Platform;

    /// The `core.json` of a core at `version` for the platform `platform_id`.
    pub fn core_json(version: &str, platform_id: &str) -> String {
        format!(
            r#"{{"core":{{"metadata":{{"version":"{version}","platform_ids":["{platform_id}"]}}}}}}"#
        )
    }

    pub fn zip_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut zip = ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, bytes) in files {
            zip.start_file(*name, SimpleFileOptions::default()).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    pub fn data_json(slots: &str) -> String {
        format!(r#"{{"data":{{"data_slots":[{slots}]}}}}"#)
    }

    /// The zip of a GBA core `a.B` at `version`.
    pub fn release(version: &str) -> Vec<u8> {
        zip_of(&[
            ("Cores/a.B/core.json", core_json(version, "gba").as_bytes()),
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

    /// The index's entry for the Game Boy core `id` at `version`, downloaded
    /// from `core.zip`.
    pub fn remote_core(id: &str, version: &str) -> RemoteCore {
        RemoteCore {
            id: id.into(),
            version: version.into(),
            platforms: vec![Platform::Gb],
            download_url: "core.zip".into(),
        }
    }

    /// The index file a library keeps, holding `Spiritualized.GB` at 2.0,
    /// downloaded from `download_url`.
    pub fn index_file(download_url: &str) -> Vec<u8> {
        let core = RemoteCore {
            download_url: download_url.into(),
            ..remote_core("Spiritualized.GB", "2.0")
        };
        serde_json::to_vec(&[core]).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::{testing::*, *};
    use crate::os::testing::{put, put_at};

    mod core_files {
        use super::*;

        /// What these slots of a core for GBA declare.
        fn files(slots: &str) -> Vec<CoreFile> {
            parse_core_files(&[Platform::Gba], data_json(slots).as_bytes())
        }

        fn gba(path: &str, required: bool, size_exact: Option<u64>) -> CoreFile {
            CoreFile {
                platform: Platform::Gba,
                path: PathBuf::from(path),
                required,
                size_exact,
            }
        }

        #[test]
        fn parses_data_json_slots() {
            for (slot, expected) in [
                (
                    r#"{"id":"0x103","name":"BIOS","filename":"gba_bios.bin","required":true,
                        "parameters":137,"nonvolatile":false,"size_exact":16384,
                        "extensions":["bin"]}"#,
                    vec![gba("gba_bios.bin", true, Some(16384))],
                ),
                (r#"{"filename":"x.bin"}"#, vec![gba("x.bin", false, None)]),
                (
                    r#"{"filename":"x.bin","parameters":"0x89","size_exact":"0x4000"}"#,
                    vec![gba("x.bin", false, Some(16384))],
                ),
                // Zero doesn't mean an empty file, because cartridge slots use
                // it too.
                (
                    r#"{"filename":"x.bin","size_exact":0}"#,
                    vec![gba("x.bin", false, None)],
                ),
                // The core's own file, a save, and a cartridge slot aren't
                // files the user provides.
                (r#"{"filename":"x.bin","parameters":2}"#, vec![]),
                (r#"{"filename":"x.sav","nonvolatile":true}"#, vec![]),
                (
                    r#"{"name":"Cartridge","required":true,"parameters":"0x109"}"#,
                    vec![],
                ),
            ] {
                assert_eq!(files(slot), expected, "{slot}");
            }
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
            let unsupported = [Platform::Unsupported("../Saves".into())];
            let json = data_json(r#"{"filename":"x.bin"}"#);
            assert_eq!(parse_core_files(&unsupported, json.as_bytes()), []);
        }
    }

    fn installed(version: Option<&str>) -> InstalledCore {
        InstalledCore {
            id: "a.B".into(),
            version: version.map(String::from),
            platforms: Vec::new(),
        }
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
            let row = CoreRow::Both {
                installed: installed(on_device),
                latest: remote_core("a.B", newest),
            };
            assert_eq!(row.has_update(), expected, "{on_device:?} and {newest}");
        }
    }

    mod zips {
        use super::*;

        #[test]
        fn parses_core_json_with_byte_order_mark() {
            let json = format!("\u{feff}{}", core_json("1.2.0", "gba"));
            let bytes = zip_of(&[("Cores/a.B/core.json", json.as_bytes())]);
            assert_eq!(CoreZip::parse(bytes, "a.B").unwrap().version(), "1.2.0");
        }

        #[test]
        fn rejects_what_is_not_zip_of_this_core() {
            let core_json = core_json("1", "gba");
            for (bytes, reason) in [
                (
                    zip_of(&[
                        ("Cores/a.B/core.json", core_json.as_bytes()),
                        ("../../evil", b"x"),
                    ]),
                    "../../evil goes outside the device",
                ),
                (
                    zip_of(&[("Cores/other.C/core.json", core_json.as_bytes())]),
                    "no Cores/a.B/core.json",
                ),
                (
                    zip_of(&[("Cores/a.B/core.json", b"{ not json")]),
                    "its core.json can't be read",
                ),
            ] {
                match CoreZip::parse(bytes, "a.B") {
                    Err(InstallCoreError::NotACoreZip(why)) => assert_eq!(why, reason),
                    Ok(_) => panic!("{reason}: it was taken"),
                    Err(error) => panic!("{reason}: {error}"),
                }
            }
        }
    }

    mod index {
        use super::*;

        /// A source with `Spiritualized.GB` at this version, or failing while
        /// it has none. It counts its calls.
        struct Fake {
            version: Cell<Option<&'static str>>,
            asked: Cell<u32>,
        }

        impl Fake {
            fn new(version: Option<&'static str>) -> Self {
                Fake {
                    version: Cell::new(version),
                    asked: Cell::new(0),
                }
            }
        }

        impl RemoteCoreSource for &Fake {
            fn index(&self) -> Result<Vec<RemoteCore>, BasicError> {
                self.asked.set(self.asked.get() + 1);
                match self.version.get() {
                    Some(version) => Ok(vec![
                        remote_core("Spiritualized.GB", version),
                        remote_core("someone.Else", "1"),
                    ]),
                    None => Err(BasicError::BadResponse {
                        from: "cores.json".into(),
                        message: "not found".into(),
                    }),
                }
            }

            fn download(&self, _: &RemoteCore) -> Result<Vec<u8>, BasicError> {
                unreachable!()
            }
        }

        fn versions(cores: &[RemoteCore]) -> Vec<&str> {
            cores.iter().map(|core| core.version.as_str()).collect()
        }

        /// A file holding `Spiritualized.GB` at 1.0, written long ago.
        fn old_file(dir: &Path) -> PathBuf {
            let cores = [remote_core("Spiritualized.GB", "1.0")];
            let json = serde_json::to_vec(&cores).unwrap();
            put_at(dir, "cores.json", &json, 1_000)
        }

        #[test]
        fn core_index_caches_for_an_hour() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("cores.json");
            let fake = Fake::new(Some("2.0"));
            // Only the supported cores come back.
            let got = CoreIndex::open(&path, &fake).supported_cores().unwrap();
            assert_eq!(got, [remote_core("Spiritualized.GB", "2.0")]);
            assert_eq!(fake.asked.get(), 1);
            // Within the hour, reopening doesn't fetch again.
            let again = CoreIndex::open(&path, &fake).supported_cores().unwrap();
            assert_eq!(versions(&again), ["2.0"]);
            assert_eq!(fake.asked.get(), 1);
            // An old file is fetched again.
            fake.version.set(Some("3.0"));
            let got = CoreIndex::open(&old_file(dir.path()), &fake).supported_cores();
            assert_eq!(versions(&got.unwrap()), ["3.0"]);
            assert_eq!(fake.asked.get(), 2);
            // So is a corrupt one, though written just now.
            let path = put(dir.path(), "cores.json", b"{ not json");
            let got = CoreIndex::open(&path, &fake).supported_cores();
            assert_eq!(versions(&got.unwrap()), ["3.0"]);
            assert_eq!(fake.asked.get(), 3);
        }

        #[test]
        fn core_index_works_offline() {
            let dir = tempfile::tempdir().unwrap();
            let fake = Fake::new(None);
            // The old copy is used, and the source isn't asked again.
            let mut index = CoreIndex::open(&old_file(dir.path()), &fake);
            assert_eq!(versions(&index.supported_cores().unwrap()), ["1.0"]);
            assert_eq!(versions(&index.supported_cores().unwrap()), ["1.0"]);
            assert_eq!(fake.asked.get(), 1);
            // With nothing kept, the failure is returned.
            let mut index = CoreIndex::open(&dir.path().join("none.json"), &fake);
            assert!(matches!(
                index.supported_cores(),
                Err(BasicError::BadResponse { .. })
            ));
        }
    }
}
