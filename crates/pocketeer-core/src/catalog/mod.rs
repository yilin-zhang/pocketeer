//! External knowledge about cores and ROMs: openFPGA's core inventory
//! ([`inventory`]), DAT files ([`dat`]), libretro's DAT and thumbnail
//! repositories ([`libretro`]), and the table mapping Pocket platform ids to
//! the names those sources use. Nothing here reads a card or a library.

pub mod dat;
pub mod inventory;
pub mod libretro;
pub use dat::Dat;

/// ROM file extensions that name their platform without doubt, beyond the
/// ones that already are a platform id (`gba`, `nes`, …).
const EXTENSIONS: &[(&str, &str)] = &[
    ("sfc", "snes"),
    ("smc", "snes"),
    ("md", "genesis"),
    ("gen", "genesis"),
    ("smd", "genesis"),
    ("lnx", "lynx"),
    ("pocket", "gbstudio"),
];

/// The platform a ROM file belongs to, judging by its extension: one of
/// `known` (platform ids the library or the card's cores use) when the
/// extension is that id, or an unambiguous extension from a fixed table.
pub fn platform_of(file_name: &str, known: &[String]) -> Option<String> {
    let ext = file_name.rsplit_once('.')?.1.to_lowercase();
    if let Some((_, p)) = EXTENSIONS.iter().find(|(e, _)| *e == ext) {
        return Some((*p).to_string());
    }
    let builtin = [
        "gb", "gbc", "gba", "nes", "snes", "sms", "gg", "pce", "lynx",
    ];
    (builtin.contains(&ext.as_str()) || known.contains(&ext)).then_some(ext)
}

/// Pocket platform id → libretro-thumbnails repository name.
pub const SYSTEMS: &[(&str, &str)] = &[
    ("2600", "Atari - 2600"),
    ("5200", "Atari - 5200"),
    ("7800", "Atari - 7800"),
    ("lynx", "Atari - Lynx"),
    ("arduboy", "Arduboy Inc - Arduboy"),
    ("coleco", "Coleco - ColecoVision"),
    ("colecovision", "Coleco - ColecoVision"),
    ("intv", "Mattel - Intellivision"),
    ("intellivision", "Mattel - Intellivision"),
    ("gb", "Nintendo - Game Boy"),
    ("gbc", "Nintendo - Game Boy Color"),
    ("gba", "Nintendo - Game Boy Advance"),
    ("nes", "Nintendo - Nintendo Entertainment System"),
    ("snes", "Nintendo - Super Nintendo Entertainment System"),
    ("sgb", "Nintendo - Game Boy"),
    ("poke_mini", "Nintendo - Pokemon Mini"),
    ("pokemini", "Nintendo - Pokemon Mini"),
    ("vb", "Nintendo - Virtual Boy"),
    ("pce", "NEC - PC Engine - TurboGrafx 16"),
    ("pcecd", "NEC - PC Engine CD - TurboGrafx-CD"),
    ("sg1000", "Sega - SG-1000"),
    ("sms", "Sega - Master System - Mark III"),
    ("gg", "Sega - Game Gear"),
    ("genesis", "Sega - Mega Drive - Genesis"),
    ("md", "Sega - Mega Drive - Genesis"),
    ("32x", "Sega - 32X"),
    ("ng", "SNK - Neo Geo"),
    ("ngp", "SNK - Neo Geo Pocket"),
    ("ngpc", "SNK - Neo Geo Pocket Color"),
    ("wonderswan", "Bandai - WonderSwan"),
    ("wsc", "Bandai - WonderSwan Color"),
    ("supervision", "Watara - Supervision"),
    ("gamate", "Bit Corporation - Gamate"),
    ("gameandwatch", "Handheld Electronic Game"),
    ("psx", "Sony - PlayStation"),
    ("amiga", "Commodore - Amiga"),
    ("c64", "Commodore - 64"),
    ("vectrex", "GCE - Vectrex"),
    ("odyssey2", "Magnavox - Odyssey2"),
    ("channel_f", "Fairchild - Channel F"),
];

pub fn thumbnail_system(platform: &str) -> Option<&'static str> {
    SYSTEMS
        .iter()
        .find(|(id, _)| id.eq_ignore_ascii_case(platform))
        .map(|(_, s)| *s)
}

/// Only mapped cartridge systems with an upstream No-Intro DAT. Other
/// platforms are explicitly unsupported, not negative ROM matches.
pub fn dat_system(platform: &str) -> Option<&'static str> {
    match platform.to_ascii_lowercase().as_str() {
        "pcecd" | "psx" | "ng" | "gamate" | "gameandwatch" => None,
        _ => thumbnail_system(platform),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platforms_come_from_unambiguous_extensions() {
        let known = vec!["wonderswan".to_string()];
        assert_eq!(platform_of("Metroid.GBA", &known).as_deref(), Some("gba"));
        assert_eq!(platform_of("Sonic.md", &known).as_deref(), Some("genesis"));
        assert_eq!(
            platform_of("x.wonderswan", &known).as_deref(),
            Some("wonderswan")
        );
        assert_eq!(platform_of("disc.bin", &known), None);
        assert_eq!(platform_of("noext", &known), None);
    }
}
