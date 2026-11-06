//! DATs, catalogues of known games, and a local copy of them to look games
//! up in.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io,
    iter::Peekable,
    path::{Path, PathBuf},
    vec,
};

use fs_err as fs;
use strum::IntoEnumIterator;

use crate::{
    error::SharedError,
    files::write_atomically,
    game::{Files, Game, Rom},
    platform::Platform,
};

/// One game in a DAT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatEntry {
    /// The standard name, e.g. `Pokemon - Emerald Version (USA, Europe)`.
    pub name: String,
    pub roms: Vec<Rom>,
}

/// One DAT file: every known game of one system.
#[derive(Debug, Clone)]
pub struct Dat {
    pub entries: Vec<DatEntry>,
}

impl Dat {
    /// The entry with this file; the first, if a file is listed under several
    /// games.
    pub fn lookup(&self, rom: &Rom) -> Option<&DatEntry> {
        self.entries.iter().find(|entry| entry.roms.contains(rom))
    }

    /// The DAT in `text`, in the clrmamepro format, which libretro and
    /// No-Intro write.
    pub fn parse_clrmamepro(text: &str) -> Result<Self, ParseError> {
        let mut tokens = tokenize(text)?.into_iter().peekable();
        let blocks = parse_block(&mut tokens, 0)?;
        if tokens.next().is_some() {
            return Err(ParseError("unexpected `)`".into()));
        }
        let mut entries = Vec::new();
        for (key, value) in &blocks {
            if let ("game", Value::Block(game)) = (key.as_str(), value) {
                entries.push(parse_entry(game)?);
            }
        }
        if entries.is_empty() {
            return Err(ParseError("no games".into()));
        }
        Ok(Self { entries })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a DAT: {0}")]
pub struct ParseError(String);

/// A DAT a source has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteDat {
    pub name: String,
    /// Whatever the source uses to tell versions apart; a different string
    /// means different contents.
    pub version: String,
}

/// Where DATs come from.
pub trait RemoteDatSource {
    /// The source's DATs that can hold games of `platform`; none if it has
    /// no DAT for it. Platforms whose cores run the same games can share
    /// one.
    fn systems(&self, platform: &Platform) -> &'static [&'static str];

    /// Every DAT the source has now.
    fn list(&self) -> Result<Vec<RemoteDat>, SharedError>;

    /// The text of the DAT called `name`.
    fn fetch(&self, name: &str) -> Result<String, SharedError>;

    fn parse(&self, text: &str) -> Result<Dat, ParseError>;
}

/// What a sync changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// Platforms with a DAT downloaded because it was missing or had changed.
    pub updated: Vec<Platform>,
    /// Platforms with a DAT the source doesn't have (any more).
    pub missing: Vec<Platform>,
}

/// The local copy of one source's DATs, on disk and parsed in memory:
///
/// ```text
/// <dir>/<DAT name>.dat
/// <dir>/versions.json   DAT name → the version it was synced at
/// ```
///
/// Only the DATs some known platform needs are kept. The files are a cache:
/// deleting them loses nothing a sync can't bring back.
pub struct DatMirror<S> {
    dir: PathBuf,
    source: S,
    /// DAT name → DAT.
    dats: HashMap<&'static str, Dat>,
}

impl<S: RemoteDatSource> DatMirror<S> {
    /// The DATs already in `dir`, without going online; none if it is empty
    /// or doesn't exist. A file that doesn't parse is left out, and the next
    /// sync replaces it.
    pub fn open(dir: &Path, source: S) -> Result<Self, SharedError> {
        let mut mirror = Self {
            dir: dir.to_path_buf(),
            source,
            dats: HashMap::new(),
        };
        for platform in Platform::iter() {
            for &name in mirror.source.systems(&platform) {
                if mirror.dats.contains_key(name) {
                    continue;
                }
                let path = mirror.dat_path(name);
                let text = match fs::read_to_string(&path) {
                    Ok(text) => text,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e.into()),
                };
                if let Ok(dat) = mirror.source.parse(&text) {
                    mirror.dats.insert(name, dat);
                }
            }
        }
        Ok(mirror)
    }

    /// Brings the DATs up to date, on disk and in memory: downloads those
    /// that are missing, didn't parse, or have a new version.
    pub fn sync(&mut self) -> Result<SyncReport, SharedError> {
        let listed: HashMap<String, String> = self
            .source
            .list()?
            .into_iter()
            .map(|r| (r.name, r.version))
            .collect();
        let mut versions = self.versions()?;
        let mut updated = HashSet::new();
        let mut missing = HashSet::new();
        for platform in Platform::iter() {
            for &name in self.source.systems(&platform) {
                if updated.contains(name) || missing.contains(name) {
                    continue;
                }
                let Some(version) = listed.get(name) else {
                    missing.insert(name);
                    continue;
                };
                if versions.get(name) == Some(version) && self.dats.contains_key(name) {
                    continue;
                }
                let text = self.source.fetch(name)?;
                let dat = self
                    .source
                    .parse(&text)
                    .map_err(|e| SharedError::BadResponse {
                        from: name.into(),
                        message: e.to_string(),
                    })?;
                // The DAT first, then its version: stopped in between, the
                // next sync downloads it again.
                write_atomically(&self.dat_path(name), text.as_bytes(), None)?;
                versions.insert(name.to_string(), version.clone());
                self.write_versions(&versions)?;
                self.dats.insert(name, dat);
                updated.insert(name);
            }
        }
        Ok(SyncReport {
            updated: self.platforms_using(&updated),
            missing: self.platforms_using(&missing),
        })
    }

    /// What the DATs call `game`.
    pub fn lookup(&self, game: &Game) -> Option<&DatEntry> {
        let Files::Single(rom) = &game.files;
        self.source
            .systems(&game.platform)
            .iter()
            .filter_map(|name| self.dats.get(name))
            .find_map(|dat| dat.lookup(rom))
    }

    /// The platforms with any of these DATs.
    fn platforms_using(&self, names: &HashSet<&str>) -> Vec<Platform> {
        Platform::iter()
            .filter(|p| self.source.systems(p).iter().any(|n| names.contains(n)))
            .collect()
    }

    fn dat_path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.dat"))
    }

    fn versions_path(&self) -> PathBuf {
        self.dir.join("versions.json")
    }

    /// A missing or unreadable record means nothing is known to be current.
    fn versions(&self) -> Result<BTreeMap<String, String>, SharedError> {
        let path = self.versions_path();
        match fs::read_to_string(&path) {
            Ok(text) => Ok(serde_json::from_str(&text).unwrap_or_default()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(e.into()),
        }
    }

    fn write_versions(&self, versions: &BTreeMap<String, String>) -> Result<(), SharedError> {
        let json = serde_json::to_string_pretty(versions).expect("a map of strings serializes");
        write_atomically(&self.versions_path(), json.as_bytes(), None)
    }
}

// The clrmamepro format.

#[derive(Debug, PartialEq)]
enum Token {
    Word(String),
    Open,
    Close,
}

enum Value {
    Word(String),
    Block(Vec<(String, Self)>),
}

fn tokenize(text: &str) -> Result<Vec<Token>, ParseError> {
    let mut tokens = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {}
            '(' => tokens.push(Token::Open),
            ')' => tokens.push(Token::Close),
            // Quoted: may hold spaces, parentheses and `\"`.
            '"' => {
                let mut word = String::new();
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') if matches!(chars.peek(), Some('"' | '\\')) => {
                            word.extend(chars.next());
                        }
                        Some(c) => word.push(c),
                        None => return Err(ParseError("unterminated quote".into())),
                    }
                }
                tokens.push(Token::Word(word));
            }
            c => {
                let mut word = String::from(c);
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() || c == '(' || c == ')' {
                        break;
                    }
                    word.push(c);
                    chars.next();
                }
                tokens.push(Token::Word(word));
            }
        }
    }
    Ok(tokens)
}

/// `key value` and `key ( ... )` pairs up to the `)` that closes this block
/// (or the end, at the top).
fn parse_block(
    tokens: &mut Peekable<vec::IntoIter<Token>>,
    depth: usize,
) -> Result<Vec<(String, Value)>, ParseError> {
    let mut pairs = Vec::new();
    loop {
        // A `)` too many is left for the caller to see.
        if depth == 0 && matches!(tokens.peek(), Some(Token::Close)) {
            return Ok(pairs);
        }
        let key = match tokens.next() {
            None if depth == 0 => return Ok(pairs),
            None => return Err(ParseError("unclosed `(`".into())),
            Some(Token::Close) => return Ok(pairs),
            Some(Token::Open) => return Err(ParseError("`(` without a key".into())),
            Some(Token::Word(key)) => key,
        };
        let value = match tokens.next() {
            Some(Token::Word(value)) => Value::Word(value),
            Some(Token::Open) => Value::Block(parse_block(tokens, depth + 1)?),
            Some(Token::Close) | None => {
                return Err(ParseError(format!("`{key}` has no value")));
            }
        };
        pairs.push((key, value));
    }
}

fn word_value<'a>(pairs: &'a [(String, Value)], key: &str) -> Option<&'a str> {
    pairs.iter().find_map(|(k, v)| match v {
        Value::Word(w) if k == key => Some(w.as_str()),
        _ => None,
    })
}

fn parse_entry(pairs: &[(String, Value)]) -> Result<DatEntry, ParseError> {
    let name = word_value(pairs, "name").ok_or_else(|| ParseError("a game has no name".into()))?;
    let name = name.to_string();
    let roms = pairs
        .iter()
        .filter_map(|(k, v)| match v {
            Value::Block(rom) if k == "rom" => Some(rom),
            _ => None,
        })
        // A ROM without a size or crc (one nobody has dumped) can't be
        // matched, so it is left out.
        .filter_map(|rom| Some((word_value(rom, "size")?, word_value(rom, "crc")?)))
        .map(|(size, crc)| {
            let bad = |what: &str| ParseError(format!("{name}: bad {what} `{size}`/`{crc}`"));
            Ok(Rom {
                size: size.parse().map_err(|_| bad("size"))?,
                crc32: u32::from_str_radix(crc, 16).map_err(|_| bad("crc"))?,
            })
        })
        .collect::<Result<_, _>>()?;
    Ok(DatEntry { name, roms })
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use super::*;

    const PCE: &str = r#"clrmamepro ( name "PCE" ) game ( name "Bonk (USA)" rom ( name "Bonk (USA).pce" size 4 crc 0000000A ) )"#;
    const SGX: &str = r#"clrmamepro ( name "SGX" ) game ( name "Daimakaimura (Japan)" rom ( name "Daimakaimura (Japan).sgx" size 8 crc 0000000B ) )"#;
    const GB: &str = r#"clrmamepro ( name "GB" ) game ( name "Tetris (World)" rom ( name "Tetris (World).gb" size 2 crc 0000000C ) )"#;
    const SGX_NEW: &str = r#"clrmamepro ( name "SGX" ) game ( name "Daimakaimura (Japan) (Rev 1)" rom ( name "x.sgx" size 8 crc 0000000B ) )"#;

    /// A source held in memory, with two DATs for the PC Engine and one the
    /// Game Boy and Game Boy Color share; it records what was fetched.
    #[derive(Clone, Default)]
    struct Fake {
        /// DAT name → (version, text).
        dats: Rc<RefCell<HashMap<String, (String, String)>>>,
        fetched: Rc<RefCell<Vec<String>>>,
    }

    impl Fake {
        fn set(&self, name: &str, version: &str, text: &str) {
            self.dats
                .borrow_mut()
                .insert(name.into(), (version.into(), text.into()));
        }
    }

    impl RemoteDatSource for Fake {
        fn systems(&self, platform: &Platform) -> &'static [&'static str] {
            match platform {
                Platform::PcEngine => &["pce", "sgx"],
                Platform::Gb | Platform::Gbc => &["gb"],
                _ => &[],
            }
        }

        fn list(&self) -> Result<Vec<RemoteDat>, SharedError> {
            Ok(self
                .dats
                .borrow()
                .iter()
                .map(|(name, (version, _))| RemoteDat {
                    name: name.clone(),
                    version: version.clone(),
                })
                .collect())
        }

        fn fetch(&self, name: &str) -> Result<String, SharedError> {
            self.fetched.borrow_mut().push(name.into());
            Ok(self.dats.borrow()[name].1.clone())
        }

        fn parse(&self, text: &str) -> Result<Dat, ParseError> {
            Dat::parse_clrmamepro(text)
        }
    }

    fn pce(size: u64, crc32: u32) -> Game {
        Game {
            platform: Platform::PcEngine,
            files: Files::Single(Rom { size, crc32 }),
        }
    }

    fn open(dir: &Path, fake: &Fake) -> DatMirror<Fake> {
        DatMirror::open(dir, fake.clone()).unwrap()
    }

    /// A fake source with both DATs and a mirror synced from it.
    fn synced() -> (tempfile::TempDir, Fake, DatMirror<Fake>) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::default();
        fake.set("pce", "1", PCE);
        fake.set("sgx", "1", SGX);
        fake.set("gb", "1", GB);
        let mut mirror = open(dir.path(), &fake);
        mirror.sync().unwrap();
        (dir, fake, mirror)
    }

    const GBA: &str = r#"
clrmamepro (
	name "Nintendo - Game Boy Advance"
	version "2026.10.07"
	extensions "gba"
)

game (
	name "Pokemon - Emerald Version (USA, Europe)"
	region "USA"
	serial "BPEE"
	rom ( name "Pokemon - Emerald Version (USA, Europe).gba" size 16777216 crc 1F1C08FB md5 605B89B67018ABCEA91E693A4DD25BE3 sha1 F3AE088181BF583E55DAF962A92BB46F4F1D07B7 serial "BPEE" )
)

game (
	name "Tom & \"Jerry\" (Proto)"
	rom ( name "Tom & \"Jerry\" (Proto).gba" size 4 crc 0000000A )
)
"#;

    mod mirror {
        use super::*;

        #[test]
        fn finds_game_in_any_dat_of_platform() {
            let (_dir, _, mirror) = synced();
            assert_eq!(mirror.lookup(&pce(4, 0xA)).unwrap().name, "Bonk (USA)");
            assert_eq!(
                mirror.lookup(&pce(8, 0xB)).unwrap().name,
                "Daimakaimura (Japan)"
            );
        }

        #[test]
        fn finds_nothing_for_unlisted_game() {
            let (_dir, _, mirror) = synced();
            assert_eq!(mirror.lookup(&pce(4, 0xB)), None);
            let gba = Game {
                platform: Platform::Gba,
                files: Files::Single(Rom {
                    size: 4,
                    crc32: 0xA,
                }),
            };
            assert_eq!(mirror.lookup(&gba), None);
        }

        #[test]
        fn opens_synced_dats_without_source() {
            let (dir, _, _) = synced();
            let offline = open(dir.path(), &Fake::default());
            assert_eq!(offline.lookup(&pce(4, 0xA)).unwrap().name, "Bonk (USA)");
        }

        #[test]
        fn opens_missing_dir_as_empty() {
            let dir = tempfile::tempdir().unwrap();
            let mirror = open(&dir.path().join("nope"), &Fake::default());
            assert_eq!(mirror.lookup(&pce(4, 0xA)), None);
        }

        #[test]
        fn fetches_only_new_or_changed_dats() {
            let (_dir, fake, mut mirror) = synced();
            fake.fetched.borrow_mut().clear();
            fake.set("sgx", "2", SGX_NEW);
            let report = mirror.sync().unwrap();
            assert_eq!(*fake.fetched.borrow(), ["sgx"]);
            assert_eq!(report.updated, [Platform::PcEngine]);
            assert_eq!(
                mirror.lookup(&pce(8, 0xB)).unwrap().name,
                "Daimakaimura (Japan) (Rev 1)"
            );
        }

        #[test]
        fn refetches_corrupt_local_dat() {
            let (dir, fake, _) = synced();
            fs::write(dir.path().join("pce.dat"), "game (").unwrap();
            let mut mirror = open(dir.path(), &fake);
            assert_eq!(mirror.lookup(&pce(4, 0xA)), None);
            fake.fetched.borrow_mut().clear();
            mirror.sync().unwrap();
            assert_eq!(*fake.fetched.borrow(), ["pce"]);
            assert_eq!(mirror.lookup(&pce(4, 0xA)).unwrap().name, "Bonk (USA)");
        }

        #[test]
        fn reports_and_keeps_dat_gone_from_source() {
            let (dir, fake, _) = synced();
            fake.dats.borrow_mut().remove("sgx");
            let mut mirror = open(dir.path(), &fake);
            let report = mirror.sync().unwrap();
            assert_eq!(report.missing, [Platform::PcEngine]);
            assert!(mirror.lookup(&pce(8, 0xB)).is_some());
        }

        #[test]
        fn reports_corrupt_download_and_keeps_old_dat() {
            let (_dir, fake, mut mirror) = synced();
            fake.set("sgx", "2", "game (");
            assert!(matches!(
                mirror.sync(),
                Err(SharedError::BadResponse { .. })
            ));
            assert_eq!(
                mirror.lookup(&pce(8, 0xB)).unwrap().name,
                "Daimakaimura (Japan)"
            );
        }

        #[test]
        fn fetches_shared_dat_once_and_reports_every_platform() {
            let (_dir, fake, mut mirror) = synced();
            fake.fetched.borrow_mut().clear();
            fake.set("gb", "2", GB);
            let report = mirror.sync().unwrap();
            assert_eq!(*fake.fetched.borrow(), ["gb"]);
            assert_eq!(report.updated, [Platform::Gb, Platform::Gbc]);
            let gbc = Game {
                platform: Platform::Gbc,
                files: Files::Single(Rom {
                    size: 2,
                    crc32: 0xC,
                }),
            };
            assert_eq!(mirror.lookup(&gbc).unwrap().name, "Tetris (World)");
        }
    }

    mod parsing {
        use super::*;

        #[test]
        fn parses_game_entries() {
            let dat = Dat::parse_clrmamepro(GBA).unwrap();
            assert_eq!(
                dat.entries[0],
                DatEntry {
                    name: "Pokemon - Emerald Version (USA, Europe)".into(),
                    roms: vec![Rom {
                        size: 16777216,
                        crc32: 0x1F1C08FB,
                    }],
                }
            );
        }

        #[test]
        fn keeps_quotes_and_symbols_in_names() {
            let dat = Dat::parse_clrmamepro(GBA).unwrap();
            assert_eq!(dat.entries[1].name, r#"Tom & "Jerry" (Proto)"#);
        }

        #[test]
        fn rejects_malformed_dats() {
            for bad in [
                "game (",
                r#"game ( name "unterminated"#,
                r#"game ( name "x" rom ( name "x" size nope crc 0 ) )"#,
                "game ( name )",
                "clrmamepro ( name empty )",
                ")",
            ] {
                assert!(Dat::parse_clrmamepro(bad).is_err(), "{bad}");
            }
        }
    }
}
