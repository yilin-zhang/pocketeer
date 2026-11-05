//! Where DATs, covers and cores come from.

pub mod libretro;
pub mod openfpga;

use std::{sync::Arc, time::Duration};

use serde::de::DeserializeOwned;

use crate::error::BaseError;

/// How long a server gets to answer at all: what else is to be done waits.
const CONNECT: Duration = Duration::from_secs(15);

pub(crate) fn get(url: &str) -> Result<Vec<u8>, BaseError> {
    let http = |source| BaseError::Http {
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

pub(crate) fn get_json<T: DeserializeOwned>(url: &str) -> Result<T, BaseError> {
    serde_json::from_slice(&get(url)?).map_err(|e| BaseError::Remote {
        from: url.into(),
        message: e.to_string(),
    })
}
