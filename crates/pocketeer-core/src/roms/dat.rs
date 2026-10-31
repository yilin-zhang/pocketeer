//! Logiqx XML DAT files, as published by No-Intro, Redump and friends.

use std::{collections::HashMap, path::Path};

use quick_xml::{Reader, events::Event};
use serde::Serialize;

use crate::{Error, Result};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Rom {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crc32: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha1: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Game {
    pub name: String,
    pub roms: Vec<Rom>,
}

#[derive(Debug, Clone, Default)]
pub struct Dat {
    pub name: String,
    pub games: Vec<Game>,
    /// (size, crc32) → indexes into `games` / `games[i].roms`.
    index: HashMap<(u64, u32), Vec<(usize, usize)>>,
}

/// Identifies one ROM entry in a DAT.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RomKey {
    pub game: usize,
    pub rom: usize,
}

impl Dat {
    pub fn read(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).map_err(|e| Error::io(path, e))?;
        Self::parse(&bytes, path)
    }

    pub fn parse(bytes: &[u8], origin: &Path) -> Result<Self> {
        let fail = |message: String| Error::DatParse {
            path: origin.to_path_buf(),
            message,
        };
        let mut reader = Reader::from_reader(bytes);
        let mut buf = Vec::new();

        let mut dat = Dat::default();
        let mut in_header = false;
        let mut in_header_name = false;
        let mut game: Option<Game> = None;
        let mut text_buf = String::new();

        loop {
            let event = reader
                .read_event_into(&mut buf)
                .map_err(|e| fail(format!("{e} at byte {}", reader.buffer_position())))?;
            match event {
                Event::Start(ref e) | Event::Empty(ref e) => {
                    let is_empty = matches!(event, Event::Empty(_));
                    text_buf.clear();
                    match e.name().as_ref() {
                        "header" => in_header = true,
                        "name" if in_header => in_header_name = true,
                        "game" | "machine" => {
                            let mut g = Game::default();
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == "name" {
                                    g.name = attr_value(&attr);
                                }
                            }
                            if is_empty {
                                dat.games.push(g);
                            } else {
                                game = Some(g);
                            }
                        }
                        "rom" => {
                            let Some(g) = game.as_mut() else {
                                continue;
                            };
                            let mut rom = Rom {
                                name: String::new(),
                                size: None,
                                crc32: None,
                                sha1: None,
                            };
                            for attr in e.attributes().flatten() {
                                let value = attr_value(&attr);
                                match attr.key.as_ref() {
                                    "name" => rom.name = value,
                                    "size" => rom.size = value.trim().parse().ok(),
                                    "crc" => rom.crc32 = u32::from_str_radix(value.trim(), 16).ok(),
                                    "sha1" => rom.sha1 = Some(value.trim().to_ascii_lowercase()),
                                    _ => {}
                                }
                            }
                            if !rom.name.is_empty() {
                                g.roms.push(rom);
                            }
                        }
                        _ => {}
                    }
                }
                // Text arrives in pieces: plain runs and entity references
                // (`Tom &amp; Jerry` is three events). Collect until the end tag.
                Event::Text(t) => text_buf.push_str(&unescape(t.as_ref())),
                Event::GeneralRef(r) => text_buf.push_str(&resolve_ref(&r)),
                Event::End(e) => {
                    // Trim here, not per fragment: "X &amp; Y" is three fragments.
                    let text = std::mem::take(&mut text_buf).trim().to_string();
                    match e.name().as_ref() {
                        "header" => in_header = false,
                        "game" | "machine" => {
                            if let Some(g) = game.take() {
                                dat.games.push(g);
                            }
                        }
                        "name" if in_header_name => {
                            in_header_name = false;
                            dat.name = text;
                        }
                        _ => {}
                    }
                }
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }

        if dat.games.is_empty() && dat.name.is_empty() {
            return Err(fail(
                "no <game> entries found; is this a Logiqx DAT?".into(),
            ));
        }
        dat.build_index();
        Ok(dat)
    }

    fn build_index(&mut self) {
        self.index.clear();
        for (gi, game) in self.games.iter().enumerate() {
            for (ri, rom) in game.roms.iter().enumerate() {
                if let (Some(size), Some(crc)) = (rom.size, rom.crc32) {
                    self.index.entry((size, crc)).or_default().push((gi, ri));
                }
            }
        }
    }

    /// Entries with this exact size and crc32 (usually zero or one).
    pub fn lookup(&self, size: u64, crc32: u32) -> Vec<RomKey> {
        self.index
            .get(&(size, crc32))
            .map(|v| v.iter().map(|&(game, rom)| RomKey { game, rom }).collect())
            .unwrap_or_default()
    }

    pub fn get(&self, key: RomKey) -> (&Game, &Rom) {
        let game = &self.games[key.game];
        (game, &game.roms[key.rom])
    }
}

fn attr_value(attr: &quick_xml::events::attributes::Attribute<'_>) -> String {
    // Attribute values are always normalised; the XML version only affects
    // which whitespace characters count, and DATs are XML 1.0.
    attr.normalized_value(quick_xml::XmlVersion::Explicit1_0)
        .map(|v| v.into_owned())
        .unwrap_or_default()
}

/// Resolves `&#NN;` / `&#xHH;` and the five predefined entities.
fn resolve_ref(r: &quick_xml::events::BytesRef<'_>) -> String {
    if let Ok(Some(c)) = r.resolve_char_ref() {
        return c.to_string();
    }
    match r.as_ref() {
        "amp" => "&".into(),
        "lt" => "<".into(),
        "gt" => ">".into(),
        "quot" => "\"".into(),
        "apos" => "'".into(),
        other => format!("&{other};"),
    }
}

/// The five XML entities; DAT names use `&amp;` constantly ("Tom &amp; Jerry").
fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../../../tests/fixtures/roms/test.dat");

    #[test]
    fn parses_fixture() {
        let dat = Dat::parse(FIXTURE.as_bytes(), Path::new("test.dat")).unwrap();
        assert_eq!(dat.name, "Test - Fake Handheld");
        assert_eq!(dat.games.len(), 3);
        assert_eq!(dat.games.iter().map(|g| g.roms.len()).sum::<usize>(), 4);
        let keys = dat.lookup(11, 0x0d4a1185);
        assert_eq!(keys.len(), 1);
        let (game, rom) = dat.get(keys[0]);
        assert_eq!(game.name, "Hello World (USA)");
        assert_eq!(rom.name, "Hello World (USA).fkh");
        assert_eq!(
            rom.sha1.as_deref(),
            Some("2aae6c35c94fcfb415dbe95f408b9ce91ee846ed")
        );
        // lowercase crc in the file parses too
        assert_eq!(dat.lookup(43, 0x414fa339).len(), 1);
        assert!(dat.lookup(43, 0x414fa338).is_empty());
    }

    #[test]
    fn handles_entities_and_empty_game_tags() {
        let xml = r#"<datafile><header><name>X &amp; Y caf&#233; &#x41;</name><description>ignored</description></header>
            <game name="Tom &amp; Jerry (USA)"><description>Tom &amp; Jerry (USA)</description>
            <rom name="Tom &amp; Jerry (USA).gb" size="1" crc="00000000"/></game>
            <machine name="Empty"><description>caf&#233; &#x41;</description></machine></datafile>"#;
        let dat = Dat::parse(xml.as_bytes(), Path::new("x.dat")).unwrap();
        assert_eq!(dat.name, "X & Y café A");
        assert_eq!(dat.games[0].name, "Tom & Jerry (USA)");
        assert_eq!(dat.games[0].roms[0].name, "Tom & Jerry (USA).gb");
        assert_eq!(dat.games[1].name, "Empty");
        assert!(dat.games[1].roms.is_empty());
    }

    #[test]
    fn rejects_non_dat() {
        let err = Dat::parse(b"<html><body>nope</body></html>", Path::new("x.dat")).unwrap_err();
        assert_eq!(err.code(), "DAT_PARSE_ERROR");
        let err = Dat::parse(b"<datafile><game", Path::new("x.dat")).unwrap_err();
        assert_eq!(err.code(), "DAT_PARSE_ERROR");
    }
}
