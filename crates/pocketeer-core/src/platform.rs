//! Which system a game is for.

use serde::{Deserialize, Serialize};
use strum::{EnumIter, IntoEnumIterator};

/// `Platform::iter()` goes through every named platform.
#[derive(Debug, Clone, PartialEq, Eq, Hash, EnumIter, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum Platform {
    Gb,
    Gbc,
    Gba,
    GameGear,
    MasterSystem,
    Nes,
    Snes,
    Genesis,
    PcEngine,
    NeoGeo,
    /// An id no known platform has.
    #[strum(disabled)]
    Other(String),
}

impl Platform {
    /// The platform of a ROM file with this extension, if it can only be one.
    pub fn from_extension(extension: &str) -> Option<Self> {
        Some(match extension.to_lowercase().as_str() {
            "gb" => Self::Gb,
            "gbc" => Self::Gbc,
            "gba" => Self::Gba,
            "gg" => Self::GameGear,
            "sms" => Self::MasterSystem,
            "nes" => Self::Nes,
            "sfc" | "smc" => Self::Snes,
            "md" | "gen" | "smd" => Self::Genesis,
            "pce" | "sgx" => Self::PcEngine,
            _ => return None,
        })
    }

    /// The id on the device.
    pub fn id(&self) -> &str {
        match self {
            Platform::Gb => "gb",
            Platform::Gbc => "gbc",
            Platform::Gba => "gba",
            Platform::GameGear => "gg",
            Platform::MasterSystem => "sms",
            Platform::Nes => "nes",
            Platform::Snes => "snes",
            Platform::Genesis => "genesis",
            Platform::PcEngine => "pce",
            Platform::NeoGeo => "ng",
            Platform::Other(id) => id,
        }
    }

    pub fn from_id(id: &str) -> Self {
        Self::iter()
            .find(|p| p.id() == id)
            .unwrap_or_else(|| Platform::Other(id.to_string()))
    }
}

impl From<String> for Platform {
    fn from(id: String) -> Self {
        Self::from_id(&id)
    }
}

impl From<Platform> for String {
    fn from(platform: Platform) -> Self {
        platform.id().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_extension_to_platform_ignoring_case() {
        assert_eq!(Platform::from_extension("gba"), Some(Platform::Gba));
        assert_eq!(Platform::from_extension("SFC"), Some(Platform::Snes));
        assert_eq!(Platform::from_extension("sgx"), Some(Platform::PcEngine));
        assert_eq!(Platform::from_extension("md"), Some(Platform::Genesis));
    }

    #[test]
    fn maps_ambiguous_extension_to_no_platform() {
        assert_eq!(Platform::from_extension("bin"), None);
        assert_eq!(Platform::from_extension("zip"), None);
        assert_eq!(Platform::from_extension(""), None);
    }

    #[test]
    fn keeps_unknown_id() {
        let p = Platform::from_id("arduboy");
        assert_eq!(p, Platform::Other("arduboy".into()));
        assert_eq!(p.id(), "arduboy");
    }

    #[test]
    fn iterates_only_named_platforms() {
        assert!(!Platform::iter().any(|p| matches!(p, Platform::Other(_))));
    }

    #[test]
    fn finds_every_named_platform_by_id() {
        for p in Platform::iter() {
            assert_eq!(Platform::from_id(p.id()), p);
        }
    }
}
