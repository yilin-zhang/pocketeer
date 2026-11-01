//! The parenthesized DAT format used by libretro-database. Parse structure,
//! not lines: quoted names can contain parentheses, spaces and escaped quotes.
use super::{Dat, Game, Rom};
use crate::{Error, Result};
use std::path::Path;

#[derive(Debug, PartialEq)]
enum Token {
    Word(String),
    Open,
    Close,
}

pub(super) fn parse(bytes: &[u8], origin: &Path) -> Result<Dat> {
    let fail = |message: &str| Error::DatParse {
        path: origin.into(),
        message: message.into(),
    };
    let text = std::str::from_utf8(bytes).map_err(|_| fail("DAT is not UTF-8"))?;
    let mut chars = text.chars().peekable();
    let mut tokens = Vec::new();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {}
            '(' => tokens.push(Token::Open),
            ')' => tokens.push(Token::Close),
            '"' => {
                let mut value = String::new();
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') if matches!(chars.peek(), Some('"' | '\\')) => {
                            value.push(chars.next().unwrap())
                        }
                        Some(c) => value.push(c),
                        None => return Err(fail("Unterminated quoted string")),
                    }
                }
                tokens.push(Token::Word(value));
            }
            c => {
                let mut value = String::from(c);
                while chars
                    .peek()
                    .is_some_and(|c| !c.is_whitespace() && !matches!(c, '(' | ')'))
                {
                    value.push(chars.next().unwrap());
                }
                tokens.push(Token::Word(value));
            }
        }
    }
    let mut dat = Dat::default();
    let mut stack = Vec::<String>::new();
    let mut game: Option<Game> = None;
    let mut rom: Option<Rom> = None;
    let mut pos = 0;
    while pos < tokens.len() {
        match &tokens[pos] {
            Token::Close => {
                match stack.pop().as_deref() {
                    Some("rom") => game
                        .as_mut()
                        .ok_or_else(|| fail("ROM outside game"))?
                        .roms
                        .push(rom.take().ok_or_else(|| fail("Missing ROM"))?),
                    Some("game" | "machine") => dat
                        .games
                        .push(game.take().ok_or_else(|| fail("Missing game"))?),
                    Some(_) => {}
                    None => return Err(fail("Unexpected closing parenthesis")),
                }
                pos += 1;
            }
            Token::Word(key) => {
                match tokens.get(pos + 1) {
                    Some(Token::Open) => {
                        if stack.len() >= 16 {
                            return Err(fail("DAT nesting too deep"));
                        }
                        if matches!(key.as_str(), "game" | "machine") {
                            if !stack.is_empty() {
                                return Err(fail("Nested game"));
                            }
                            game = Some(Game::default());
                        } else if key == "rom" {
                            if !matches!(stack.last().map(String::as_str), Some("game" | "machine"))
                            {
                                return Err(fail("ROM outside game"));
                            }
                            rom = Some(Rom {
                                name: String::new(),
                                size: None,
                                crc32: None,
                                sha1: None,
                            });
                        }
                        stack.push(key.clone());
                    }
                    Some(Token::Word(value)) => match stack.last().map(String::as_str) {
                        Some("clrmamepro") if key == "name" => dat.name = value.clone(),
                        Some("game" | "machine") if key == "name" => {
                            game.as_mut().unwrap().name = value.clone()
                        }
                        Some("rom") => {
                            let rom = rom.as_mut().unwrap();
                            match key.as_str() {
                                "name" => rom.name = value.clone(),
                                "size" => {
                                    rom.size =
                                        Some(value.parse().map_err(|_| fail("Invalid ROM size"))?)
                                }
                                "crc" => {
                                    rom.crc32 = Some(
                                        u32::from_str_radix(value, 16)
                                            .map_err(|_| fail("Invalid CRC"))?,
                                    )
                                }
                                "sha1" => {
                                    if value.len() != 40
                                        || !value.bytes().all(|b| b.is_ascii_hexdigit())
                                    {
                                        return Err(fail("Invalid SHA-1"));
                                    }
                                    rom.sha1 = Some(value.to_ascii_lowercase());
                                }
                                _ => {}
                            }
                        }
                        Some(_) => {}
                        None => return Err(fail("Value outside DAT block")),
                    },
                    _ => return Err(fail("Expected value or block")),
                }
                pos += 2;
            }
            _ => return Err(fail("Unexpected opening parenthesis")),
        }
    }
    if !stack.is_empty() || dat.games.is_empty() {
        return Err(fail("Incomplete or empty DAT"));
    }
    dat.build_index();
    Ok(dat)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_real_format_without_line_or_parenthesis_assumptions() {
        let input = br#"clrmamepro ( name "Nintendo - Game Boy Advance" version "2026.10.07" )
        game ( name "Game (USA) \"quoted\"" region "USA"
          rom ( name "Game (USA).gba" size 11 crc 0D4A1185 sha1 2AAE6C35C94FCFB415DBE95F408B9CE91EE846ED ) )"#;
        let dat = Dat::parse(input, Path::new("libretro.dat")).unwrap();
        assert_eq!(dat.games[0].name, "Game (USA) \"quoted\"");
        assert_eq!(dat.lookup(11, 0x0d4a1185).len(), 1);
        assert_eq!(
            dat.games[0].roms[0].sha1.as_deref(),
            Some("2aae6c35c94fcfb415dbe95f408b9ce91ee846ed")
        );
        for bad in [
            "game (",
            "game ( name \"unterminated",
            "game ( rom ( size nope ) )",
            "game ( rom ( sha1 123 ) )",
            "clrmamepro ( name empty )",
            "game ( game ( ) )",
        ] {
            assert!(
                Dat::parse(bad.as_bytes(), Path::new("bad.dat")).is_err(),
                "{bad}"
            );
        }
    }
}
