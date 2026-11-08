//! DATs, catalogs of known games, and a local copy of them to look games
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
    error::BasicError,
    game::{Files, Game, Rom},
    os,
    platform::Platform,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatEntry {
    /// The standard name, e.g. `Pokemon - Emerald Version (USA, Europe)`.
    pub name: String,
    pub roms: Vec<Rom>,
}

/// One DAT file, listing every known game of one system.
#[derive(Debug, Clone)]
pub struct Dat {
    pub entries: Vec<DatEntry>,
}

impl Dat {
    /// The entry with this file, or the first one if several games list it.
    pub fn lookup(&self, rom: &Rom) -> Option<&DatEntry> {
        self.entries.iter().find(|entry| entry.roms.contains(rom))
    }

    /// Parses a DAT in the clrmamepro format, which libretro and No-Intro use.
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteDat {
    pub name: String,
    /// Whatever the source uses to tell versions apart. A different string
    /// means different contents.
    pub version: String,
}

pub trait RemoteDatSource {
    /// The source's DATs that can hold games of `platform`, or none if it has
    /// no DAT for it. Platforms whose cores run the same games can share one.
    fn systems(&self, platform: &Platform) -> &'static [&'static str];

    fn list(&self) -> Result<Vec<RemoteDat>, BasicError>;

    fn fetch(&self, name: &str) -> Result<String, BasicError>;

    fn parse(&self, text: &str) -> Result<Dat, ParseError>;
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// Platforms with a DAT downloaded because it was missing or had changed.
    pub updated: Vec<Platform>,
    /// Platforms with a DAT the source doesn't have.
    pub missing: Vec<Platform>,
}

/// The local copy of one source's DATs, on disk and parsed in memory.
///
/// ```text
/// <dir>/<DAT name>.dat
/// <dir>/versions.json   DAT name → the version it was synced at
/// ```
///
/// Only the DATs a known platform needs are kept. The files are a cache, so
/// deleting them loses nothing a sync can't restore.
pub struct DatMirror<S> {
    dir: PathBuf,
    source: S,
    /// By DAT name.
    dats: HashMap<&'static str, Dat>,
}

impl<S: RemoteDatSource> DatMirror<S> {
    /// The DATs already in `dir`, read without going online. A missing or empty
    /// folder gives none. A file that doesn't parse is left out, and the next
    /// sync replaces it.
    pub fn open(dir: &Path, source: S) -> Result<Self, BasicError> {
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
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                };
                if let Ok(dat) = mirror.source.parse(&text) {
                    mirror.dats.insert(name, dat);
                }
            }
        }
        Ok(mirror)
    }

    /// Updates the DATs on disk and in memory, downloading those that are
    /// missing, didn't parse, or have a new version.
    pub fn sync(&mut self) -> Result<SyncReport, BasicError> {
        let listed: HashMap<String, String> = self
            .source
            .list()?
            .into_iter()
            .map(|listed_dat| (listed_dat.name, listed_dat.version))
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
                    .map_err(|error| BasicError::BadResponse {
                        from: name.into(),
                        message: error.to_string(),
                    })?;
                // Write the DAT before its version, so that if this stops in
                // between the next sync downloads it again.
                os::write_atomically(&self.dat_path(name), text.as_bytes(), None)?;
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

    fn platforms_using(&self, names: &HashSet<&str>) -> Vec<Platform> {
        Platform::iter()
            .filter(|platform| {
                self.source
                    .systems(platform)
                    .iter()
                    .any(|name| names.contains(name))
            })
            .collect()
    }

    fn dat_path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.dat"))
    }

    fn versions_path(&self) -> PathBuf {
        self.dir.join("versions.json")
    }

    /// A missing or unreadable record means nothing is known to be current.
    fn versions(&self) -> Result<BTreeMap<String, String>, BasicError> {
        let path = self.versions_path();
        match fs::read_to_string(&path) {
            Ok(text) => Ok(serde_json::from_str(&text).unwrap_or_default()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(error) => Err(error.into()),
        }
    }

    fn write_versions(&self, versions: &BTreeMap<String, String>) -> Result<(), BasicError> {
        let json = serde_json::to_string_pretty(versions).expect("a map of strings serializes");
        os::write_atomically(&self.versions_path(), json.as_bytes(), None)
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
    while let Some(character) = chars.next() {
        match character {
            character if character.is_whitespace() => {}
            '(' => tokens.push(Token::Open),
            ')' => tokens.push(Token::Close),
            // A quoted word may hold spaces, parentheses and `\"`.
            '"' => {
                let mut word = String::new();
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') if matches!(chars.peek(), Some('"' | '\\')) => {
                            word.extend(chars.next());
                        }
                        Some(character) => word.push(character),
                        None => return Err(ParseError("unterminated quote".into())),
                    }
                }
                tokens.push(Token::Word(word));
            }
            character => {
                let mut word = String::from(character);
                while let Some(&character) = chars.peek() {
                    if character.is_whitespace() || character == '(' || character == ')' {
                        break;
                    }
                    word.push(character);
                    chars.next();
                }
                tokens.push(Token::Word(word));
            }
        }
    }
    Ok(tokens)
}

/// Reads `key value` and `key ( ... )` pairs up to the `)` closing this block,
/// or to the end at the top level.
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
    pairs.iter().find_map(|(pair_key, value)| match value {
        Value::Word(word) if pair_key == key => Some(word.as_str()),
        _ => None,
    })
}

fn parse_entry(pairs: &[(String, Value)]) -> Result<DatEntry, ParseError> {
    let name = word_value(pairs, "name").ok_or_else(|| ParseError("a game has no name".into()))?;
    let name = name.to_string();
    let roms = pairs
        .iter()
        .filter_map(|(key, value)| match value {
            Value::Block(rom) if key == "rom" => Some(rom),
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

/// Helpers shared by the tests of modules that look games up in DATs.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    /// A DAT that lists the GBA game with the contents `hello` as
    /// `Tetris (World)`. A library keeps it at
    /// `dats/Nintendo - Game Boy Advance.dat`.
    pub const GBA_DAT: &str = r#"clrmamepro ( name "Nintendo - Game Boy Advance" )
game ( name "Tetris (World)" rom ( name "Tetris (World).gba" size 5 crc 3610A686 ) )"#;
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use super::*;

    const PCE: &str = r#"clrmamepro ( name "PCE" ) game ( name "Bonk (USA)" rom ( name "Bonk (USA).pce" size 4 crc 0000000A ) )"#;
    const SGX: &str = r#"clrmamepro ( name "SGX" ) game ( name "Daimakaimura (Japan)" rom ( name "Daimakaimura (Japan).sgx" size 8 crc 0000000B ) )"#;
    const GB: &str = r#"clrmamepro ( name "GB" ) game ( name "Tetris (World)" rom ( name "Tetris (World).gb" size 2 crc 0000000C ) )"#;
    const SGX_NEW: &str = r#"clrmamepro ( name "SGX" ) game ( name "Daimakaimura (Japan) (Rev 1)" rom ( name "x.sgx" size 8 crc 0000000B ) )"#;

    /// An in-memory source with two DATs for the PC Engine and one shared by
    /// the Game Boy and Game Boy Color. It records what was fetched.
    #[derive(Clone, Default)]
    struct Fake {
        /// DAT name to version and text.
        dats: Rc<RefCell<HashMap<String, (String, String)>>>,
        fetched: Rc<RefCell<Vec<String>>>,
    }

    impl Fake {
        fn set(&self, name: &str, version: &str, text: &str) {
            self.dats
                .borrow_mut()
                .insert(name.into(), (version.into(), text.into()));
        }

        /// What was fetched since the last time this was called.
        fn fetched(&self) -> Vec<String> {
            self.fetched.borrow_mut().drain(..).collect()
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

        fn list(&self) -> Result<Vec<RemoteDat>, BasicError> {
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

        fn fetch(&self, name: &str) -> Result<String, BasicError> {
            self.fetched.borrow_mut().push(name.into());
            Ok(self.dats.borrow()[name].1.clone())
        }

        fn parse(&self, text: &str) -> Result<Dat, ParseError> {
            Dat::parse_clrmamepro(text)
        }
    }

    fn game(platform: Platform, size: u64, crc32: u32) -> Game {
        Game {
            platform,
            files: Files::Single(Rom { size, crc32 }),
        }
    }

    fn pce(size: u64, crc32: u32) -> Game {
        game(Platform::PcEngine, size, crc32)
    }

    fn open(dir: &Path, fake: &Fake) -> DatMirror<Fake> {
        DatMirror::open(dir, fake.clone()).unwrap()
    }

    /// The name the mirror gives `game`, if any.
    fn name_of(mirror: &DatMirror<Fake>, game: &Game) -> Option<String> {
        mirror.lookup(game).map(|entry| entry.name.clone())
    }

    /// A fake source with all three DATs and a mirror synced from it.
    fn synced() -> (tempfile::TempDir, Fake, DatMirror<Fake>) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::default();
        fake.set("pce", "1", PCE);
        fake.set("sgx", "1", SGX);
        fake.set("gb", "1", GB);
        let mut mirror = open(dir.path(), &fake);
        mirror.sync().unwrap();
        fake.fetched();
        (dir, fake, mirror)
    }

    #[test]
    fn dat_mirror_syncs_and_looks_up() {
        let (dir, fake, mut mirror) = synced();
        // A game is found in any DAT of its platform.
        assert_eq!(name_of(&mirror, &pce(4, 0xA)).unwrap(), "Bonk (USA)");
        assert_eq!(
            name_of(&mirror, &pce(8, 0xB)).unwrap(),
            "Daimakaimura (Japan)"
        );
        // Only the same platform matches.
        assert_eq!(name_of(&mirror, &game(Platform::Gba, 4, 0xA)), None);
        // The DATs are kept on disk.
        let offline = open(dir.path(), &Fake::default());
        assert_eq!(name_of(&offline, &pce(4, 0xA)).unwrap(), "Bonk (USA)");
        // Only a changed DAT is fetched again.
        fake.set("sgx", "2", SGX_NEW);
        let report = mirror.sync().unwrap();
        assert_eq!(fake.fetched(), ["sgx"]);
        assert_eq!(report.updated, [Platform::PcEngine]);
        assert_eq!(
            name_of(&mirror, &pce(8, 0xB)).unwrap(),
            "Daimakaimura (Japan) (Rev 1)"
        );
        // A DAT shared by two platforms is fetched once and serves both.
        fake.set("gb", "2", GB);
        let report = mirror.sync().unwrap();
        assert_eq!(fake.fetched(), ["gb"]);
        assert_eq!(report.updated, [Platform::Gb, Platform::Gbc]);
        let gbc = game(Platform::Gbc, 2, 0xC);
        assert_eq!(name_of(&mirror, &gbc).unwrap(), "Tetris (World)");
    }

    #[test]
    fn dat_mirror_survives_bad_files_and_source() {
        let (dir, fake, _) = synced();
        // A corrupt DAT on disk is left out and fetched again.
        fs::write(dir.path().join("pce.dat"), "game (").unwrap();
        let mut mirror = open(dir.path(), &fake);
        assert_eq!(name_of(&mirror, &pce(4, 0xA)), None);
        mirror.sync().unwrap();
        assert_eq!(fake.fetched(), ["pce"]);
        assert_eq!(name_of(&mirror, &pce(4, 0xA)).unwrap(), "Bonk (USA)");
        // A DAT gone from the source is reported, and the copy is kept.
        fake.dats.borrow_mut().remove("sgx");
        let report = mirror.sync().unwrap();
        assert_eq!(report.missing, [Platform::PcEngine]);
        assert_eq!(
            name_of(&mirror, &pce(8, 0xB)).unwrap(),
            "Daimakaimura (Japan)"
        );
        // A corrupt download is an error, and the old DAT stays, in memory and
        // on disk.
        fake.set("sgx", "2", "game (");
        assert!(matches!(mirror.sync(), Err(BasicError::BadResponse { .. })));
        assert_eq!(
            name_of(&mirror, &pce(8, 0xB)).unwrap(),
            "Daimakaimura (Japan)"
        );
        let offline = open(dir.path(), &Fake::default());
        assert_eq!(
            name_of(&offline, &pce(8, 0xB)).unwrap(),
            "Daimakaimura (Japan)"
        );
    }

    mod parsing {
        use super::*;

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

game (
	name "Undumped (Japan)"
	rom ( name "Undumped (Japan).gba" size 8 )
	rom ( name "Undumped (Japan).sav" crc 0000000B )
	rom ( name "Undumped (Japan) (Track 2).gba" size 2 crc 0000000C )
)
"#;

        #[test]
        fn parses_game_entries() {
            let dat = Dat::parse_clrmamepro(GBA).unwrap();
            assert_eq!(
                dat.entries,
                [
                    DatEntry {
                        name: "Pokemon - Emerald Version (USA, Europe)".into(),
                        roms: vec![Rom {
                            size: 16777216,
                            crc32: 0x1F1C08FB,
                        }],
                    },
                    DatEntry {
                        name: r#"Tom & "Jerry" (Proto)"#.into(),
                        roms: vec![Rom {
                            size: 4,
                            crc32: 0xA,
                        }],
                    },
                    // ROMs without a size or crc can't be matched.
                    DatEntry {
                        name: "Undumped (Japan)".into(),
                        roms: vec![Rom {
                            size: 2,
                            crc32: 0xC,
                        }],
                    },
                ]
            );
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
