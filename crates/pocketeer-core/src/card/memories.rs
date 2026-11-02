//! Memories: the save states the Pocket writes when you save one from its
//! menu. Each is a `.sta` file under `Memories/Save States/`, sometimes
//! with a `.sav` of the same name beside it. A state made while playing a
//! cartridge sits at the top of that folder; one made in an openFPGA core
//! sits in a subfolder and its header names the core and the ROM file.
//!
//! The header layout follows what Pocket Sync reads from real cards
//! (github.com/neil-morrison44/pocket-sync, AGPL-3.0).

use crate::{Error, Result, fsutil};
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Memory {
    /// The `.sta` file, relative to the card root.
    pub rel_path: PathBuf,
    /// Files of the same name beside it (a `.sav`), relative to the root.
    pub companions: Vec<PathBuf>,
    pub size: u64,
    pub mtime: i64,
    #[serde(flatten)]
    pub header: Header,
}

/// What a state says about where it came from.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "made_with", rename_all = "snake_case")]
pub enum Header {
    /// Made while playing a cartridge; the title it reports (empty if unknown).
    Cartridge { title: String },
    /// Made by a core running a ROM.
    Core {
        /// The ROM file name; empty if unknown.
        game: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        platform: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        core: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        core_version: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        author: Option<String>,
    },
}

impl Header {
    /// The ROM file name, or the cartridge title; empty if unknown.
    pub fn game(&self) -> &str {
        match self {
            Header::Cartridge { title } => title,
            Header::Core { game, .. } => game,
        }
    }

    pub fn platform(&self) -> Option<&str> {
        match self {
            Header::Cartridge { .. } => None,
            Header::Core { platform, .. } => platform.as_deref(),
        }
    }

    pub fn core(&self) -> Option<&str> {
        match self {
            Header::Cartridge { .. } => None,
            Header::Core { core, .. } => core.as_deref(),
        }
    }
}

/// Where the core header starts, and the size of what we read.
const CORE_HEADER: usize = 68;
const CORE_HEADER_LEN: usize = 368;
const CARTRIDGE_TITLE: std::ops::Range<usize> = 0x1c..0x1c + 320;
const HEADER_BYTES: usize = CORE_HEADER + CORE_HEADER_LEN;

/// Reads the header of a state. `cartridge` says which layout to expect:
/// the Pocket keeps cartridge states at the top of the folder.
fn read_header(bytes: &[u8], cartridge: bool) -> Header {
    let field =
        |range: std::ops::Range<usize>| bytes.get(range).map(decode_text).filter(|s| !s.is_empty());
    if cartridge {
        return Header::Cartridge {
            title: field(CARTRIDGE_TITLE).unwrap_or_default(),
        };
    }
    let at = |from: usize, to: usize| field(CORE_HEADER + from..CORE_HEADER + to);
    Header::Core {
        author: at(0, 32),
        core: at(32, 64),
        core_version: at(64, 96),
        game: at(96, 352).unwrap_or_default(),
        platform: at(352, 368),
    }
}

/// The Memories under `root` (a card, or the library's backup folder).
pub fn scan(root: &Path) -> Result<Vec<Memory>> {
    let mut found = Vec::new();
    for folder in crate::card::MEMORY_ROOTS {
        let dir = root.join(folder);
        if !dir.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(&dir)
            .min_depth(1)
            .into_iter()
            .filter_entry(|e| !fsutil::is_hidden(e.file_name()))
        {
            let entry = entry.map_err(|e| {
                Error::io(
                    e.path().unwrap_or(&dir),
                    std::io::Error::other(e.to_string()),
                )
            })?;
            let path = entry.path();
            if !entry.file_type().is_file() || !is_state(path) {
                continue;
            }
            let cartridge = entry.depth() == 1;
            found.push(read(root, path, cartridge)?);
        }
    }
    found.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(found)
}

fn is_state(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("sta"))
}

fn read(root: &Path, path: &Path, cartridge: bool) -> Result<Memory> {
    use std::io::Read;
    let mut bytes = Vec::with_capacity(HEADER_BYTES);
    std::fs::File::open(path)
        .and_then(|f| f.take(HEADER_BYTES as u64).read_to_end(&mut bytes))
        .map_err(|e| Error::io(path, e))?;
    let meta = std::fs::metadata(path).map_err(|e| Error::io(path, e))?;
    let stem = path.file_stem().unwrap_or_default();
    let mut companions = Vec::new();
    if let Some(dir) = path.parent()
        && let Ok(entries) = std::fs::read_dir(dir)
    {
        for entry in entries.flatten() {
            let other = entry.path();
            if other != path && other.is_file() && other.file_stem() == Some(stem) {
                companions.push(rel(root, &other));
            }
        }
    }
    companions.sort();
    Ok(Memory {
        rel_path: rel(root, path),
        companions,
        size: meta.len(),
        mtime: fsutil::mtime_secs(path)?,
        header: read_header(&bytes, cartridge),
    })
}

fn rel(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root).unwrap_or(path).to_path_buf()
}

/// A fixed-size text field: NUL padding dropped, UTF-8 when it is valid,
/// otherwise code page 437 (the Pocket writes "Pokémon" with é as 0x82).
fn decode_text(bytes: &[u8]) -> String {
    let kept: Vec<u8> = bytes.iter().copied().filter(|b| *b != 0).collect();
    let text = match std::str::from_utf8(&kept) {
        Ok(s) => s.to_string(),
        Err(_) => kept
            .iter()
            .map(|&b| {
                if b < 0x80 {
                    b as char
                } else {
                    CP437_HIGH[(b - 0x80) as usize]
                }
            })
            .collect(),
    };
    text.trim().to_string()
}

/// Code page 437, bytes 0x80–0xFF.
const CP437_HIGH: [char; 128] = [
    'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å', //
    'É', 'æ', 'Æ', 'ô', 'ö', 'ò', 'û', 'ù', 'ÿ', 'Ö', 'Ü', '¢', '£', '¥', '₧', 'ƒ', //
    'á', 'í', 'ó', 'ú', 'ñ', 'Ñ', 'ª', 'º', '¿', '⌐', '¬', '½', '¼', '¡', '«', '»', //
    '░', '▒', '▓', '│', '┤', '╡', '╢', '╖', '╕', '╣', '║', '╗', '╝', '╜', '╛', '┐', //
    '└', '┴', '┬', '├', '─', '┼', '╞', '╟', '╚', '╔', '╩', '╦', '╠', '═', '╬', '╧', //
    '╨', '╤', '╥', '╙', '╘', '╒', '╓', '╫', '╪', '┘', '┌', '█', '▄', '▌', '▐', '▀', //
    'α', 'ß', 'Γ', 'π', 'Σ', 'σ', 'µ', 'τ', 'Φ', 'Θ', 'Ω', 'δ', '∞', 'φ', 'ε', '∩', //
    '≡', '±', '≥', '≤', '⌠', '⌡', '÷', '≈', '°', '∙', '·', '√', 'ⁿ', '²', '■', '\u{a0}',
];

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A core state as Pocket Sync documents real ones: absolute offsets,
    /// written out here rather than taken from the reader's constants.
    pub(crate) fn core_state(core: &str, game: &str, platform: &str) -> Vec<u8> {
        let mut buf = vec![0u8; 2048];
        let mut put = |at: usize, text: &[u8]| buf[at..at + text.len()].copy_from_slice(text);
        put(68, b"Spiritualized");
        put(100, core.as_bytes());
        put(132, b"1.3.0");
        put(164, game.as_bytes());
        put(420, platform.as_bytes());
        put(436, b"Game Boy");
        buf
    }

    #[test]
    fn reads_core_and_cartridge_headers() {
        let h = read_header(&core_state("GB", "SUPER MARIOLAND.gb", "gb"), false);
        assert_eq!(
            h,
            Header::Core {
                game: "SUPER MARIOLAND.gb".into(),
                core: Some("GB".into()),
                core_version: Some("1.3.0".into()),
                author: Some("Spiritualized".into()),
                // The platform name after the id never leaks in.
                platform: Some("gb".into()),
            }
        );

        let mut cart = vec![0u8; 1024];
        cart[28..32].copy_from_slice(b"Pok\x82");
        let h = read_header(&cart, true);
        assert_eq!(
            h,
            Header::Cartridge {
                title: "Poké".into()
            }
        );

        // A short or empty file gives an empty header, not an error.
        assert_eq!(read_header(&[], false).game(), "");
    }

    #[test]
    fn scans_states_with_their_companions() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("Memories/Save States");
        std::fs::create_dir_all(dir.join("Spiritualized.GB")).unwrap();
        std::fs::write(
            dir.join("Spiritualized.GB/mario_20251101.sta"),
            core_state("GB", "Mario.gb", "gb"),
        )
        .unwrap();
        std::fs::write(dir.join("cart_1.sta"), vec![0u8; 600]).unwrap();
        std::fs::write(dir.join("cart_1.sav"), b"sram").unwrap();
        std::fs::write(dir.join(".DS_Store"), b"").unwrap();
        let found = scan(tmp.path()).unwrap();
        assert_eq!(found.len(), 2);
        let core = found
            .iter()
            .find(|m| matches!(m.header, Header::Core { .. }))
            .unwrap();
        assert_eq!(
            core.rel_path,
            Path::new("Memories/Save States/Spiritualized.GB/mario_20251101.sta")
        );
        assert_eq!(core.header.game(), "Mario.gb");
        let cart = found
            .iter()
            .find(|m| matches!(m.header, Header::Cartridge { .. }))
            .unwrap();
        assert_eq!(
            cart.companions,
            [PathBuf::from("Memories/Save States/cart_1.sav")]
        );
    }
}
