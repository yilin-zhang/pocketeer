//! Where DATs, covers and cores come from.

pub mod libretro;
pub mod openfpga;

use std::{sync::Arc, time::Duration};

use serde::de::DeserializeOwned;

use crate::error::BasicError;

/// How long a server gets to accept a connection, because everything else waits
/// for it.
const CONNECT: Duration = Duration::from_secs(15);

pub(crate) fn get(url: &str) -> Result<Vec<u8>, BasicError> {
    let http = |source| BasicError::Http {
        url: url.into(),
        source: Arc::new(source),
    };
    ureq::get(url)
        .config()
        .timeout_connect(Some(CONNECT))
        .build()
        .header("User-Agent", "pocketeer")
        .call()
        .map_err(http)?
        .body_mut()
        .with_config()
        .limit(32 * 1024 * 1024)
        .read_to_vec()
        .map_err(http)
}

pub(crate) fn get_json<T: DeserializeOwned>(url: &str) -> Result<T, BasicError> {
    serde_json::from_slice(&get(url)?).map_err(|error| BasicError::BadResponse {
        from: url.into(),
        message: error.to_string(),
    })
}
