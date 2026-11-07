//! The community's index of openFPGA cores, as a source of cores.

use serde::Deserialize;

use crate::{
    cores::{RemoteCore, RemoteCoreSource},
    error::BasicError,
    platform::Platform,
    sources::{get, get_json},
};

const INDEX: &str = "https://openfpga-library.github.io/analogue-pocket/api/v3/cores.json";

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
    /// Newest first. A core may have none.
    #[serde(default)]
    releases: Vec<Release>,
}

impl Entry {
    fn newest(self) -> Option<RemoteCore> {
        let release = self.releases.into_iter().next()?;
        let metadata = release.core.metadata;
        Some(RemoteCore {
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
    fn index(&self) -> Result<Vec<RemoteCore>, BasicError> {
        let listing: Listing = get_json(INDEX)?;
        Ok(listing.data.into_iter().filter_map(Entry::newest).collect())
    }

    fn download(&self, core: &RemoteCore) -> Result<Vec<u8>, BasicError> {
        get(&core.download_url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cores of a listing with these entries, as JSON.
    fn cores_of(entries: &str) -> Vec<RemoteCore> {
        let listing: Listing = serde_json::from_str(&format!(r#"{{"data":[{entries}]}}"#)).unwrap();
        listing.data.into_iter().filter_map(Entry::newest).collect()
    }

    #[test]
    fn uses_newest_release_of_core() {
        let cores = cores_of(
            r#"{"id":"a.B","releases":[
                {"download_url":"new.zip","core":{"metadata":{"version":"2","platform_ids":["gba"]}}},
                {"download_url":"old.zip","core":{"metadata":{"version":"1","platform_ids":["gba"]}}}
            ]}"#,
        );
        assert_eq!(
            cores,
            [RemoteCore {
                id: "a.B".into(),
                version: "2".into(),
                platforms: vec![Platform::Gba],
                download_url: "new.zip".into(),
            }]
        );
    }

    #[test]
    fn leaves_out_core_without_releases() {
        assert_eq!(cores_of(r#"{"id":"c.D","releases":[]}"#), []);
    }
}
