//! The community's index of openFPGA cores, as a source of cores.

use serde::Deserialize;

use crate::{
    cores::{RemoteCore, RemoteCoreSource},
    error::BasicError,
    platform::Platform,
    sources,
};

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

impl OpenFpga {
    const INDEX: &str = "https://openfpga-library.github.io/analogue-pocket/api/v3/cores.json";
}

impl RemoteCoreSource for OpenFpga {
    fn index(&self) -> Result<Vec<RemoteCore>, BasicError> {
        let listing: Listing = sources::get_json(Self::INDEX)?;
        Ok(listing.data.into_iter().filter_map(Entry::newest).collect())
    }

    fn download(&self, core: &RemoteCore) -> Result<Vec<u8>, BasicError> {
        sources::get(&core.download_url)
    }
}
