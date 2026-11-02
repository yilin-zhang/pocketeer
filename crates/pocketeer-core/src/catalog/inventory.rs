// The update-available rule is ported from Pocket Sync (src/hooks/useUpdateAvailable.ts);
// inventory field names follow its TypeScript types (src/types/index.ts).
// Copyright (c) Neil Morrison — https://github.com/neil-morrison44/pocket-sync
// Licensed under AGPL-3.0. Modified for pocketeer, 2026.

//! The community openFPGA core inventory and platform list.

use serde::{Deserialize, Serialize};

use crate::{
    Error, ErrorInfo, Result,
    card::formats::Framework,
    net::{Cache, Http, Policy, Source},
};

pub const CORES_URL: &str = "https://openfpga-library.github.io/analogue-pocket/api/v3/cores.json";
pub const PLATFORMS_URL: &str =
    "https://openfpga-library.github.io/analogue-pocket/api/v3/platforms.json";

#[derive(Debug, Clone, Deserialize)]
struct CoresJson {
    data: Vec<InventoryCore>,
}

#[derive(Debug, Clone, Deserialize)]
struct PlatformsJson {
    data: Vec<Platform>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct InventoryCore {
    /// `<author>.<shortname>`, same as the installed folder name.
    pub id: String,
    pub repository: Repository,
    /// Newest first. May be empty for cores with no published release.
    #[serde(default)]
    pub releases: Vec<Release>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Repository {
    #[serde(default)]
    pub platform: Option<String>,
    pub owner: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Release {
    pub download_url: String,
    #[serde(default)]
    pub requires_license: bool,
    pub core: ReleaseCore,
    #[serde(default)]
    pub data: Option<ReleaseData>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ReleaseCore {
    pub metadata: ReleaseMetadata,
    #[serde(default)]
    pub framework: Option<Framework>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ReleaseMetadata {
    #[serde(default)]
    pub platform_ids: Vec<String>,
    pub version: String,
    #[serde(default)]
    pub date_release: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ReleaseData {
    #[serde(default)]
    pub data_slots: Vec<ReleaseDataSlot>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ReleaseDataSlot {
    pub name: String,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default)]
    pub parameters: Option<SlotParameters>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct SlotParameters {
    #[serde(default)]
    pub core_specific_file: bool,
    #[serde(default)]
    pub instance_json: bool,
    #[serde(default)]
    pub platform_index: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Platform {
    pub id: String,
    pub name: String,
}

impl InventoryCore {
    pub fn latest(&self) -> Option<&Release> {
        self.releases.first()
    }

    pub fn release(&self, version: &str) -> Option<&Release> {
        self.releases
            .iter()
            .find(|r| r.core.metadata.version == version)
    }

    pub fn repo_url(&self) -> String {
        format!(
            "https://github.com/{}/{}",
            self.repository.owner, self.repository.name
        )
    }

    pub fn author(&self) -> &str {
        self.id.split_once('.').map(|(a, _)| a).unwrap_or(&self.id)
    }

    pub fn shortname(&self) -> &str {
        self.id.split_once('.').map(|(_, s)| s).unwrap_or(&self.id)
    }
}

#[derive(Debug, Clone)]
pub struct Inventory {
    pub cores: Vec<InventoryCore>,
    pub source: Source,
}

impl Inventory {
    pub fn status(&self) -> InventoryStatus {
        InventoryStatus::Loaded {
            source: self.source.clone(),
        }
    }

    pub fn load(http: &Http, cache: &Cache, policy: Policy) -> Result<Self> {
        let fetched = cache.fetch(http, "cores.json", CORES_URL, "core inventory", policy)?;
        let parsed: CoresJson =
            serde_json::from_slice(&fetched.bytes).map_err(|source| Error::ParseResponse {
                url: CORES_URL.to_string(),
                source,
            })?;
        let mut cores = parsed.data;
        cores.sort_by_key(|c| c.id.to_lowercase());
        Ok(Inventory {
            cores,
            source: fetched.source,
        })
    }

    /// Like [`Inventory::load`], but an unavailable inventory is a value, not
    /// an error, so read-only commands can still show local data.
    pub fn load_lenient(http: &Http, cache: &Cache, policy: Policy) -> InventoryLoad {
        match Self::load(http, cache, policy) {
            Ok(inv) => InventoryLoad::Loaded(inv),
            Err(err) => InventoryLoad::Unavailable(err.to_info()),
        }
    }

    pub fn get(&self, id: &str) -> Option<&InventoryCore> {
        self.cores.iter().find(|c| c.id == id)
    }

    /// Case-insensitive lookup, for hand-typed ids.
    pub fn get_ci(&self, id: &str) -> Option<&InventoryCore> {
        self.cores.iter().find(|c| c.id.eq_ignore_ascii_case(id))
    }
}

#[derive(Debug, Clone)]
pub enum InventoryLoad {
    Loaded(Inventory),
    Unavailable(ErrorInfo),
}

impl InventoryLoad {
    pub fn inventory(&self) -> Option<&Inventory> {
        match self {
            InventoryLoad::Loaded(inv) => Some(inv),
            InventoryLoad::Unavailable(_) => None,
        }
    }

    pub fn status(&self) -> InventoryStatus {
        match self {
            InventoryLoad::Loaded(inv) => inv.status(),
            InventoryLoad::Unavailable(error) => InventoryStatus::Unavailable {
                error: error.clone(),
            },
        }
    }
}

/// Whether a report had the inventory, and where it came from.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum InventoryStatus {
    Loaded {
        #[serde(flatten)]
        source: Source,
    },
    Unavailable {
        error: ErrorInfo,
    },
}

impl InventoryStatus {
    pub fn is_loaded(&self) -> bool {
        matches!(self, InventoryStatus::Loaded { .. })
    }
}

#[derive(Debug, Clone)]
pub struct Platforms {
    pub platforms: Vec<Platform>,
    pub source: Source,
}

impl Platforms {
    pub fn load(http: &Http, cache: &Cache, policy: Policy) -> Result<Self> {
        let fetched = cache.fetch(
            http,
            "platforms.json",
            PLATFORMS_URL,
            "platform list",
            policy,
        )?;
        let parsed: PlatformsJson =
            serde_json::from_slice(&fetched.bytes).map_err(|source| Error::ParseResponse {
                url: PLATFORMS_URL.to_string(),
                source,
            })?;
        Ok(Platforms {
            platforms: parsed.data,
            source: fetched.source,
        })
    }

    pub fn name(&self, id: &str) -> Option<&str> {
        self.platforms
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.name.as_str())
    }
}

/// Decides whether `latest` counts as an update over `installed`.
///
/// Core versions are free-form strings, not semver, so this is deliberately
/// loose: it's an update if the strings differ *and* the latest version doesn't
/// contain the installed one (so `"1.2.0"` vs `"v1.2.0"` or `"1.2"` is not an
/// update). Leading `v`/`V` and whitespace are ignored.
pub fn update_available(installed: &str, latest: &str) -> bool {
    let norm = |s: &str| {
        s.trim()
            .trim_start_matches(['v', 'V'])
            .trim()
            .to_ascii_lowercase()
    };
    let (i, l) = (norm(installed), norm(latest));
    if i.is_empty() {
        return !l.is_empty();
    }
    i != l && !l.contains(&i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_rule_matches_pocket_sync() {
        assert!(update_available("1.2.0", "1.3.0"));
        assert!(update_available("0.9", "1.0"));
        assert!(!update_available("1.2.0", "1.2.0"));
        assert!(!update_available("1.2.0", "v1.2.0"));
        assert!(
            !update_available("1.2", "1.2.0"),
            "latest contains installed"
        );
    }

    #[test]
    fn update_rule_edge_cases() {
        // The contains check is one-directional, as in Pocket Sync: a shorter
        // latest version that differs is reported as an update.
        assert!(update_available("1.2.0", "1.2"));
        assert!(!update_available("", ""));
        assert!(update_available("", "1.0"));
        assert!(!update_available("V2.0", "v2.0"));
    }

    #[test]
    fn parses_inventory_sample() {
        let json = include_str!("../../../../tests/fixtures/inventory/cores.json");
        let parsed: CoresJson = serde_json::from_str(json).unwrap();
        let gba = parsed
            .data
            .iter()
            .find(|c| c.id == "spiritualized.GBA")
            .unwrap();
        assert_eq!(gba.latest().unwrap().core.metadata.version, "1.3.0");
        assert_eq!(gba.release("1.2.0").unwrap().core.metadata.version, "1.2.0");
        let empty = parsed
            .data
            .iter()
            .find(|c| c.id == "nobody.Unreleased")
            .unwrap();
        assert!(empty.latest().is_none());
        assert_eq!(gba.author(), "spiritualized");
        assert_eq!(gba.shortname(), "GBA");
    }
}
