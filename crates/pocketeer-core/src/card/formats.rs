//! Typed readers for the JSON files found on a Pocket SD card.
//!
//! Parsers are deliberately lenient: unknown fields are ignored and optional
//! fields default, because cores in the wild don't all follow the spec.
//!
//! One file per JSON format would be five files of forty lines each; they
//! are sections here instead, in the order the card presents them:
//! `Analogue_Pocket.json`, `core.json`, `data.json`, instance JSONs,
//! `updaters.json`.

use std::path::Path;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::{Error, Result};

/// Parses JSON bytes, tolerating a UTF-8 byte order mark. `origin` names
/// the source in the error (a file path, or `zip:<entry>`).
pub(crate) fn parse_json<T: DeserializeOwned>(bytes: &[u8], origin: &Path) -> Result<T> {
    let text = String::from_utf8_lossy(bytes);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    serde_json::from_str(text).map_err(|source| Error::Parse {
        path: origin.to_path_buf(),
        source,
    })
}

/// Reads and parses a JSON file, tolerating a UTF-8 byte order mark.
pub(crate) fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = std::fs::read(path).map_err(|e| Error::io(path, e))?;
    parse_json(&bytes, path)
}

// ----------------------------------------------------------------------------
// Analogue_Pocket.json
// ----------------------------------------------------------------------------
// Firmware version decoding adapted from Pocket Sync
// (src/jotai/firmware/selectors.ts, currentFirmwareVersionSelector).
// Copyright (c) Neil Morrison — https://github.com/neil-morrison44/pocket-sync
// Licensed under AGPL-3.0. Modified for pocketeer, 2026: ported to Rust,
// short/odd values handled without panicking.

/// `Analogue_Pocket.json` at the SD card root, written by the Pocket firmware.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct AnaloguePocketJson {
    #[serde(default)]
    pub product: Option<String>,
    #[serde(default)]
    pub firmware: Option<FirmwareSection>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct FirmwareSection {
    #[serde(default)]
    pub runtime: Option<Runtime>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Runtime {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub byte: Option<u32>,
    #[serde(default)]
    pub build_date: Option<String>,
}

impl AnaloguePocketJson {
    pub fn read(path: &Path) -> Result<Self> {
        read_json(path)
    }

    pub fn runtime(&self) -> Option<&Runtime> {
        self.firmware.as_ref()?.runtime.as_ref()
    }
}

/// Decodes the firmware `byte` into a human version string.
///
/// The byte's hex digits are the version: `0x22` → `"2.2"`. Betas of 1.1 used
/// a leading `b`: `0xb5` → `"1.1-beta-5"`.
pub fn firmware_version_from_byte(byte: u32) -> String {
    let hex = format!("{byte:x}");
    let mut chars = hex.chars();
    match (chars.next(), chars.as_str()) {
        (Some('b'), rest) if !rest.is_empty() => format!("1.1-beta-{rest}"),
        (Some(major), rest) if !rest.is_empty() => format!("{major}.{rest}"),
        _ => hex,
    }
}

#[cfg(test)]
mod analogue_pocket_tests {
    use super::*;

    #[test]
    fn decodes_versions() {
        assert_eq!(firmware_version_from_byte(0x22), "2.2");
        assert_eq!(firmware_version_from_byte(0x14), "1.4");
        assert_eq!(firmware_version_from_byte(0xb5), "1.1-beta-5");
        assert_eq!(firmware_version_from_byte(0x7), "7");
    }
}

// ----------------------------------------------------------------------------
// core.json
// ----------------------------------------------------------------------------
// Field layout adapted from Pocket Sync (src-tauri/src/core_json_files/core.rs).
// Copyright (c) Neil Morrison — https://github.com/neil-morrison44/pocket-sync
// Licensed under AGPL-3.0. Modified for pocketeer, 2026: optional fields made
// lenient, extra metadata fields added.

/// `Cores/<author>.<shortname>/core.json`
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct CoreJson {
    pub core: CoreSection,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct CoreSection {
    pub metadata: CoreMetadata,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct CoreMetadata {
    #[serde(default)]
    pub platform_ids: Vec<String>,
    pub shortname: String,
    pub author: String,
    pub version: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub date_release: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Framework {
    #[serde(default)]
    pub target_product: Option<String>,
    #[serde(default)]
    pub version_required: Option<String>,
    #[serde(default)]
    pub sleep_supported: Option<bool>,
}

impl CoreJson {
    pub fn read(path: &Path) -> Result<Self> {
        read_json(path)
    }
}

#[cfg(test)]
mod core_json_tests {
    use super::*;

    // Sample from Pocket Sync's test suite (agg23's Arduboy core).

    #[test]
    fn tolerates_minimal_core_json() {
        let core: CoreJson = serde_json::from_str(
            r#"{"core":{"metadata":{"shortname":"X","author":"a","version":"1"}}}"#,
        )
        .unwrap();
        assert!(core.core.metadata.platform_ids.is_empty());
        assert!(core.core.metadata.description.is_none());
    }
}

// ----------------------------------------------------------------------------
// data.json
// ----------------------------------------------------------------------------
// Data-slot layout and the parameters bitmap are ported from Pocket Sync
// (src-tauri/src/required_files/mod.rs, parameters_bitmap.rs, core_data_slots.rs).
// Copyright (c) Neil Morrison — https://github.com/neil-morrison44/pocket-sync
// Licensed under AGPL-3.0. Modified for pocketeer, 2026.

// `Cores/<id>/data.json`: the files a core can load, as "data slots".

/// Core authors write ids and bitmaps as either numbers or `"0x..."` strings.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum IntOrHex {
    Int(u32),
    Str(String),
}

impl Default for IntOrHex {
    fn default() -> Self {
        IntOrHex::Int(0)
    }
}

impl IntOrHex {
    pub fn value(&self) -> u32 {
        match self {
            IntOrHex::Int(i) => *i,
            IntOrHex::Str(s) => {
                let s = s.trim();
                match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                    Some(hex) => u32::from_str_radix(hex, 16).unwrap_or(0),
                    None => s.parse().unwrap_or(0),
                }
            }
        }
    }
}

impl PartialEq for IntOrHex {
    fn eq(&self, other: &Self) -> bool {
        self.value() == other.value()
    }
}

/// Decoded `parameters` bitmap of a data slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotParams {
    /// File lives under `Assets/<platform>/<core id>/` rather than `common/`.
    pub core_specific: bool,
    /// Slot is filled by an instance JSON rather than a direct file.
    pub instance_json: bool,
    /// Index into `core.json`'s `platform_ids`.
    pub platform_index: usize,
}

impl SlotParams {
    pub fn from_bits(bits: u32) -> Self {
        let low = bits & (1 << 24) != 0;
        let high = bits & (1 << 25) != 0;
        SlotParams {
            core_specific: bits & (1 << 1) != 0,
            instance_json: bits & (1 << 4) != 0,
            platform_index: (high as usize) << 1 | low as usize,
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct DataJson {
    pub data: DataSection,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct DataSection {
    #[serde(default)]
    pub data_slots: Vec<DataSlot>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct DataSlot {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub id: IntOrHex,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub parameters: IntOrHex,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub alternate_filenames: Option<Vec<String>>,
    #[serde(default)]
    pub md5: Option<String>,
    #[serde(default)]
    pub extensions: Vec<String>,
}

impl DataSlot {
    pub fn params(&self) -> SlotParams {
        SlotParams::from_bits(self.parameters.value())
    }
}

impl DataJson {
    pub fn read(path: &Path) -> Result<Self> {
        read_json(path)
    }
}

#[cfg(test)]
mod data_json_tests {
    use super::*;

    #[test]
    fn int_or_hex_values() {
        assert_eq!(IntOrHex::Int(7).value(), 7);
        assert_eq!(IntOrHex::Str("0x113".into()).value(), 0x113);
        assert_eq!(IntOrHex::Str("0X1".into()).value(), 1);
        assert_eq!(IntOrHex::Str("12".into()).value(), 12);
        assert_eq!(IntOrHex::Str("junk".into()).value(), 0);
        assert_eq!(IntOrHex::Int(0x10), IntOrHex::Str("0x10".into()));
    }

    #[test]
    fn parameter_bits() {
        // Values from Pocket Sync's tests.
        assert_eq!(
            SlotParams::from_bits(0),
            SlotParams {
                core_specific: false,
                instance_json: false,
                platform_index: 0
            }
        );
        assert_eq!(
            SlotParams::from_bits(0x113),
            SlotParams {
                core_specific: true,
                instance_json: true,
                platform_index: 0
            }
        );
        assert_eq!(SlotParams::from_bits(1 << 24).platform_index, 1);
        assert_eq!(SlotParams::from_bits(1 << 25).platform_index, 2);
        assert_eq!(SlotParams::from_bits(3 << 24).platform_index, 3);
    }
}

// ----------------------------------------------------------------------------
// instance JSON
// ----------------------------------------------------------------------------
// Layout ported from Pocket Sync (src-tauri/src/required_files/instance_data_slots.rs).
// Copyright (c) Neil Morrison — https://github.com/neil-morrison44/pocket-sync
// Licensed under AGPL-3.0. Modified for pocketeer, 2026.

// Instance JSON files (`Assets/<platform>/<core>/**/*.json`): one "game",
// listing the files to load into each data slot.

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct InstanceJson {
    pub instance: InstanceSection,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct InstanceSection {
    /// Subfolder (under the core's assets folder) the slot files live in.
    #[serde(default)]
    pub data_path: String,
    #[serde(default)]
    pub data_slots: Vec<InstanceSlot>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct InstanceSlot {
    #[serde(default)]
    pub id: IntOrHex,
    pub filename: String,
    #[serde(default)]
    pub md5: Option<String>,
}

impl InstanceJson {
    pub fn read(path: &Path) -> Result<Self> {
        read_json(path)
    }
}

// ----------------------------------------------------------------------------
// updaters.json
// ----------------------------------------------------------------------------
// Field layout adapted from Pocket Sync (src-tauri/src/core_json_files/updaters.rs).
// Copyright (c) Neil Morrison — https://github.com/neil-morrison44/pocket-sync
// Licensed under AGPL-3.0. Modified for pocketeer, 2026: lenient parsing.

// `Cores/<id>/updaters.json`: optional metadata a core author ships to help
// updaters, most importantly which older cores this one replaces.

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct UpdatersJson {
    /// Older cores this one supersedes. Entries that aren't well-formed
    /// objects are ignored (see [`UpdatersJson::previous_cores`]).
    #[serde(default)]
    pub previous: Vec<Value>,
}

/// A core this one replaces: its folder is `Cores/<author>.<shortname>`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PreviousCore {
    pub author: String,
    pub shortname: String,
    pub platform_id: String,
}

impl PreviousCore {
    pub fn id(&self) -> String {
        format!("{}.{}", self.author, self.shortname)
    }
}

impl UpdatersJson {
    pub fn parse(bytes: &[u8]) -> serde_json::Result<Self> {
        serde_json::from_slice(bytes)
    }

    pub fn previous_cores(&self) -> Vec<PreviousCore> {
        self.previous
            .iter()
            .filter_map(|v| serde_json::from_value(v.clone()).ok())
            .collect()
    }
}

#[cfg(test)]
mod updaters_tests {
    use super::*;

    #[test]
    fn parses_previous_and_ignores_garbage() {
        let u = UpdatersJson::parse(
            br#"{"previous":[{"author":"a","shortname":"b","platform_id":"gb"},"junk",{"author":"x"}],"license":{"filename":"beta.bin"}}"#,
        )
        .unwrap();
        let prev = u.previous_cores();
        assert_eq!(prev.len(), 1);
        assert_eq!(prev[0].id(), "a.b");
    }
}
