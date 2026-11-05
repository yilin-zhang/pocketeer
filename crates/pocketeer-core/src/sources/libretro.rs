//! Libretro as a source of DATs (libretro-database) and of covers
//! (libretro-thumbnails).

use serde::Deserialize;

use crate::{
    cover::{Cover, RemoteCoverSource},
    dat::{Dat, ParseError, RemoteDat, RemoteDatSource},
    error::BaseError,
    platform::Platform,
    sources::{get, get_json},
};

const DAT_LISTING: &str =
    "https://api.github.com/repos/libretro/libretro-database/contents/metadat/no-intro";
const DAT_FILES: &str =
    "https://raw.githubusercontent.com/libretro/libretro-database/master/metadat/no-intro";
/// One repository per system, named after its DAT with `_` for spaces.
const COVER_TREES: &str = "https://api.github.com/repos/libretro-thumbnails";
const COVER_FILES: &str = "https://raw.githubusercontent.com/libretro-thumbnails";
/// The folder of each repository with the covers; the others hold
/// screenshots, title screens and logos.
const BOXARTS: &str = "Named_Boxarts";

#[derive(Deserialize)]
struct TreeEntry {
    path: String,
    sha: String,
}

/// A folder of a repository, as GitHub's API lists it.
#[derive(Deserialize)]
struct Tree {
    tree: Vec<TreeEntry>,
}

#[derive(Debug, Clone, Copy)]
pub struct Libretro;

impl Libretro {
    /// Libretro's systems for `platform`, which name both its DATs and its
    /// cover repositories.
    fn systems(platform: &Platform) -> &'static [&'static str] {
        match platform {
            Platform::Gb => &["Nintendo - Game Boy"],
            Platform::Gbc => &["Nintendo - Game Boy Color"],
            Platform::Gba => &["Nintendo - Game Boy Advance"],
            Platform::GameGear => &["Sega - Game Gear"],
            Platform::MasterSystem => &["Sega - Master System - Mark III"],
            Platform::Nes => &["Nintendo - Nintendo Entertainment System"],
            Platform::Snes => &["Nintendo - Super Nintendo Entertainment System"],
            Platform::Genesis => &["Sega - Mega Drive - Genesis"],
            // The PC Engine core also runs SuperGrafx games (`.sgx`).
            Platform::PcEngine => &[
                "NEC - PC Engine - TurboGrafx 16",
                "NEC - PC Engine SuperGrafx",
            ],
            // Neo Geo is arcade; No-Intro has no DAT for it.
            Platform::NeoGeo | Platform::Other(_) => &[],
        }
    }
}

impl RemoteDatSource for Libretro {
    fn systems(&self, platform: &Platform) -> &'static [&'static str] {
        Libretro::systems(platform)
    }

    /// One request to GitHub's API, which allows 60 an hour without a login.
    fn list(&self) -> Result<Vec<RemoteDat>, BaseError> {
        #[derive(Deserialize)]
        struct File {
            name: String,
            sha: String,
        }
        let files: Vec<File> = get_json(DAT_LISTING)?;
        Ok(files
            .into_iter()
            .filter_map(|f| {
                Some(RemoteDat {
                    name: f.name.strip_suffix(".dat")?.to_string(),
                    version: f.sha,
                })
            })
            .collect())
    }

    fn fetch(&self, name: &str) -> Result<String, BaseError> {
        let url = format!("{DAT_FILES}/{}.dat", encode(name));
        String::from_utf8(get(&url)?).map_err(|e| BaseError::Remote {
            from: url,
            message: e.to_string(),
        })
    }

    fn parse(&self, text: &str) -> Result<Dat, ParseError> {
        Dat::parse_clrmamepro(text)
    }
}

impl RemoteCoverSource for Libretro {
    fn systems(&self, platform: &Platform) -> &'static [&'static str] {
        Libretro::systems(platform)
    }

    /// The covers folder's own id, which changes with what is in it; one
    /// request to GitHub's API.
    fn version(&self, system: &str) -> Result<String, BaseError> {
        let url = format!("{COVER_TREES}/{}/git/trees/master", repository(system));
        let root: Tree = get_json(&url)?;
        root.tree
            .into_iter()
            .find(|e| e.path == BOXARTS)
            .map(|e| e.sha)
            .ok_or_else(|| BaseError::Remote {
                from: url,
                message: format!("no {BOXARTS} folder"),
            })
    }

    /// One request to GitHub's API, for the folder at `version`.
    fn names(&self, system: &str, version: &str) -> Result<Vec<String>, BaseError> {
        let url = format!("{COVER_TREES}/{}/git/trees/{version}", repository(system));
        let folder: Tree = get_json(&url)?;
        Ok(folder
            .tree
            .into_iter()
            .filter_map(|e| Some(e.path.strip_suffix(".png")?.to_string()))
            .collect())
    }

    /// Libretro writes `_` for each character a file name can't hold on
    /// some system.
    fn cover_name(&self, game_name: &str) -> String {
        game_name
            .chars()
            .map(|c| match c {
                '&' | '*' | '/' | ':' | '`' | '<' | '>' | '?' | '\\' | '|' | '"' => '_',
                c => c,
            })
            .collect()
    }

    /// Some covers are symlinks to another cover, which come as the name of
    /// that cover; those are followed, once.
    fn fetch(&self, system: &str, cover_name: &str) -> Result<Cover, BaseError> {
        let url = cover_url(system, cover_name);
        let mut bytes = get(&url)?;
        if let Some(target) = link_target(&bytes) {
            bytes = get(&cover_url(system, target))?;
        }
        Cover::from_bytes(bytes).ok_or_else(|| BaseError::Remote {
            from: url,
            message: "not an image".into(),
        })
    }
}

fn cover_url(system: &str, cover_name: &str) -> String {
    format!(
        "{COVER_FILES}/{}/master/{BOXARTS}/{}.png",
        repository(system),
        encode(cover_name)
    )
}

/// The cover that `bytes`, downloaded as a cover, name if they are a symlink:
/// one line that is the file name of a PNG beside it, and no path, since they
/// come from the network.
fn link_target(bytes: &[u8]) -> Option<&str> {
    let target = std::str::from_utf8(bytes).ok()?.strip_suffix(".png")?;
    let plain = !target.is_empty() && !target.contains(['/', '\\', '\n', '\r']);
    plain.then_some(target)
}

fn repository(system: &str) -> String {
    encode(&system.replace(' ', "_"))
}

/// `name` as one URL path segment.
fn encode(name: &str) -> String {
    name.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dat_name_is_encoded_as_one_path_segment() {
        assert_eq!(
            encode("Sega - Mega Drive - Genesis"),
            "Sega%20-%20Mega%20Drive%20-%20Genesis"
        );
    }

    #[test]
    fn a_symlink_is_the_file_name_of_a_png() {
        assert_eq!(
            link_target(b"All-Star Baseball 2004 (USA).png"),
            Some("All-Star Baseball 2004 (USA)")
        );
    }

    #[test]
    fn what_else_comes_is_not_a_symlink() {
        for bytes in [
            &b"../Named_Titles/Foo.png"[..],
            b"..\\Foo.png",
            b"one\ntwo.png",
            b".png",
            b"Foo.jpg",
            b"not a link",
            b"\x89PNG\r\n\x1a\n",
        ] {
            assert_eq!(link_target(bytes), None, "{bytes:?}");
        }
    }

    #[test]
    fn a_cover_is_named_without_what_file_names_cannot_hold() {
        assert_eq!(
            Libretro.cover_name("Mario & Luigi: Superstar Saga (USA)"),
            "Mario _ Luigi_ Superstar Saga (USA)"
        );
    }
}
