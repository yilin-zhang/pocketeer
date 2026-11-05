//! The community's inventory of openFPGA cores, as a source of cores.

use serde::Deserialize;

use crate::{
    cores::{InventoryCore, RemoteCoreSource},
    error::BaseError,
    platform::Platform,
    sources::{get, get_json},
};

const INVENTORY: &str = "https://openfpga-library.github.io/analogue-pocket/api/v3/cores.json";

#[derive(Deserialize)]
struct Metadata {
    version: String,
    #[serde(default)]
    platform_ids: Vec<String>,
}

#[derive(Deserialize)]
struct ReleaseCore {
    metadata: Metadata,
}

#[derive(Deserialize)]
struct Release {
    download_url: String,
    core: ReleaseCore,
}

#[derive(Deserialize)]
struct Entry {
    id: String,
    /// Newest first; a core may have none.
    #[serde(default)]
    releases: Vec<Release>,
}

impl Entry {
    fn newest(self) -> Option<InventoryCore> {
        let release = self.releases.into_iter().next()?;
        let metadata = release.core.metadata;
        Some(InventoryCore {
            id: self.id,
            version: metadata.version,
            platforms: metadata
                .platform_ids
                .iter()
                .map(|id| Platform::from_id(id))
                .collect(),
            download_url: release.download_url,
        })
    }
}

#[derive(Deserialize)]
struct Listing {
    data: Vec<Entry>,
}

#[derive(Debug, Clone, Copy)]
pub struct OpenFpga;

impl RemoteCoreSource for OpenFpga {
    fn inventory(&self) -> Result<Vec<InventoryCore>, BaseError> {
        let listing: Listing = get_json(INVENTORY)?;
        Ok(listing.data.into_iter().filter_map(Entry::newest).collect())
    }

    fn download(&self, url: &str) -> Result<Vec<u8>, BaseError> {
        get(url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_core_is_its_newest_release_and_one_with_none_is_left_out() {
        let listing: Listing = serde_json::from_str(
            r#"{"data":[
                {"id":"a.B","releases":[
                    {"download_url":"new.zip","core":{"metadata":{"version":"2","platform_ids":["gba"]}}},
                    {"download_url":"old.zip","core":{"metadata":{"version":"1","platform_ids":["gba"]}}}
                ]},
                {"id":"c.D","releases":[]}
            ]}"#,
        )
        .unwrap();
        let cores: Vec<_> = listing.data.into_iter().filter_map(Entry::newest).collect();
        assert_eq!(
            cores,
            [InventoryCore {
                id: "a.B".into(),
                version: "2".into(),
                platforms: vec![Platform::Gba],
                download_url: "new.zip".into(),
            }]
        );
    }
}
