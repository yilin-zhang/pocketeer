//! External knowledge about cores and ROMs: openFPGA's core inventory
//! ([`inventory`]), DAT files ([`dat`]), libretro's DAT and thumbnail
//! repositories ([`libretro`]), and the table mapping Pocket platform ids to
//! the names those sources use. Nothing here reads a card or a library.

pub mod dat;
pub mod inventory;
pub mod libretro;
pub use dat::Dat;

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
