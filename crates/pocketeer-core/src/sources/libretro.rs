//! Libretro as a source of DATs (libretro-database) and of covers
//! (libretro-thumbnails).

use serde::Deserialize;
use serde_json::Value;

use crate::{
    cover::{Cover, RemoteCoverSource},
    dat::{Dat, ParseError, RemoteDat, RemoteDatSource},
    error::BasicError,
    platform::Platform,
    sources,
};

#[derive(Debug, Clone, Copy)]
pub struct Libretro;

impl Libretro {
    const DAT_LISTING: &str =
        "https://api.github.com/repos/libretro/libretro-database/contents/metadat/no-intro";
    const DAT_FILES: &str =
        "https://raw.githubusercontent.com/libretro/libretro-database/master/metadat/no-intro";
    /// One repository per system, named after its DAT with `_` for spaces.
    const COVER_TREES: &str = "https://api.github.com/repos/libretro-thumbnails";
    const COVER_FILES: &str = "https://raw.githubusercontent.com/libretro-thumbnails";
    /// The folder in each repository that holds the covers. The others hold
    /// screenshots, title screens and logos.
    const BOXARTS: &str = "Named_Boxarts";

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
            // Neo Geo is arcade, and No-Intro has no DAT for it.
            Platform::NeoGeo | Platform::Unsupported(_) => &[],
        }
    }

    fn cover_url(system: &str, cover_name: &str) -> String {
        format!(
            "{}/{}/master/{}/{}.png",
            Self::COVER_FILES,
            Self::repository(system),
            Self::BOXARTS,
            Self::encode(cover_name)
        )
    }

    /// The cover that `bytes` point to if they are a symlink. A symlink is one
    /// line naming a PNG beside it, with no path, since the data comes from the
    /// network.
    fn link_target(bytes: &[u8]) -> Option<&str> {
        let target = std::str::from_utf8(bytes).ok()?.strip_suffix(".png")?;
        let plain = !target.is_empty() && !target.contains(['/', '\\', '\n', '\r']);
        plain.then_some(target)
    }

    fn repository(system: &str) -> String {
        Self::encode(&system.replace(' ', "_"))
    }

    /// `name` as one URL path segment.
    fn encode(name: &str) -> String {
        name.bytes()
            .map(|byte| match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    (byte as char).to_string()
                }
                _ => format!("%{byte:02X}"),
            })
            .collect()
    }
}

impl RemoteDatSource for Libretro {
    fn systems(&self, platform: &Platform) -> &'static [&'static str] {
        Libretro::systems(platform)
    }

    /// One request to GitHub's API, which allows 60 an hour without a login.
    fn list(&self) -> Result<Vec<RemoteDat>, BasicError> {
        #[derive(Deserialize)]
        struct File {
            name: String,
            sha: String,
        }
        let files: Vec<File> = sources::get_json(Self::DAT_LISTING)?;
        Ok(files
            .into_iter()
            .filter_map(|file| {
                Some(RemoteDat {
                    name: file.name.strip_suffix(".dat")?.to_string(),
                    version: file.sha,
                })
            })
            .collect())
    }

    fn fetch(&self, name: &str) -> Result<String, BasicError> {
        let url = format!("{}/{}.dat", Self::DAT_FILES, Self::encode(name));
        String::from_utf8(sources::get(&url)?).map_err(|error| BasicError::BadResponse {
            from: url,
            message: error.to_string(),
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

    /// The covers folder's own id, which changes with its contents. This takes
    /// one request to GitHub's API.
    fn version(&self, system: &str) -> Result<String, BasicError> {
        let url = format!(
            "{}/{}/git/trees/master",
            Self::COVER_TREES,
            Self::repository(system)
        );
        // GitHub lists a folder as `{"tree": [{"path", "sha", …}, …]}`.
        let root: Value = sources::get_json(&url)?;
        root["tree"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|entry| entry["path"] == Self::BOXARTS)
            .and_then(|entry| entry["sha"].as_str())
            .map(str::to_string)
            .ok_or_else(|| BasicError::BadResponse {
                from: url,
                message: format!("no {} folder", Self::BOXARTS),
            })
    }

    /// One request to GitHub's API, for the folder at `version`.
    fn names(&self, system: &str, version: &str) -> Result<Vec<String>, BasicError> {
        let url = format!(
            "{}/{}/git/trees/{version}",
            Self::COVER_TREES,
            Self::repository(system)
        );
        let dir: Value = sources::get_json(&url)?;
        let entries = dir["tree"]
            .as_array()
            .ok_or_else(|| BasicError::BadResponse {
                from: url,
                message: "no list of files".into(),
            })?;
        Ok(entries
            .iter()
            .filter_map(|entry| Some(entry["path"].as_str()?.strip_suffix(".png")?.to_string()))
            .collect())
    }

    /// Libretro writes `_` for each character a file name can't hold on
    /// some system.
    fn cover_name(&self, game_name: &str) -> String {
        game_name
            .chars()
            .map(|character| match character {
                '&' | '*' | '/' | ':' | '`' | '<' | '>' | '?' | '\\' | '|' | '"' => '_',
                character => character,
            })
            .collect()
    }

    /// Some covers are symlinks to another cover, which arrive as that cover's
    /// name. They are followed once.
    fn fetch(&self, system: &str, cover_name: &str) -> Result<Cover, BasicError> {
        let url = Self::cover_url(system, cover_name);
        let mut bytes = sources::get(&url)?;
        if let Some(target) = Self::link_target(&bytes) {
            bytes = sources::get(&Self::cover_url(system, target))?;
        }
        Cover::from_bytes(bytes).ok_or_else(|| BasicError::BadResponse {
            from: url,
            message: "not an image".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_name_as_one_path_segment() {
        assert_eq!(
            Libretro::encode("Sega - Mega Drive - Genesis"),
            "Sega%20-%20Mega%20Drive%20-%20Genesis"
        );
        assert_eq!(
            Libretro::encode("Tom & Jerry (USA)/x"),
            "Tom%20%26%20Jerry%20%28USA%29%2Fx"
        );
    }

    #[test]
    fn follows_only_plain_symlink_targets() {
        assert_eq!(
            Libretro::link_target(b"All-Star Baseball 2004 (USA).png"),
            Some("All-Star Baseball 2004 (USA)")
        );
        for bytes in [
            &b"../Named_Titles/Foo.png"[..],
            b"..\\Foo.png",
            b"one\ntwo.png",
            b".png",
            b"Foo.jpg",
            b"not a link",
            b"\x89PNG\r\n\x1a\n",
        ] {
            assert_eq!(Libretro::link_target(bytes), None, "{bytes:?}");
        }
    }
}
