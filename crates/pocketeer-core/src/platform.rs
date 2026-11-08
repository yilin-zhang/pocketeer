//! Which system a game is for.

use serde::{Deserialize, Serialize};
use strum::{EnumIter, IntoEnumIterator};

/// `Platform::iter()` goes through every supported platform.
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
    /// A platform Pocketeer doesn't support, by its id.
    #[strum(disabled)]
    Unsupported(String),
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
            Platform::Unsupported(id) => id,
        }
    }

    pub fn from_id(id: &str) -> Self {
        Self::iter()
            .find(|platform| platform.id() == id)
            .unwrap_or_else(|| Platform::Unsupported(id.to_string()))
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
