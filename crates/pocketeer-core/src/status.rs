//! Summary of a Pocket SD card: firmware, disk space, installed cores.

use std::path::PathBuf;

use serde::Serialize;

use crate::{
    ErrorInfo, Pocket, Result, cores,
    formats::{AnaloguePocketJson, firmware_version_from_byte},
};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Status {
    pub root: PathBuf,
    pub product: Option<String>,
    pub firmware: Option<Firmware>,
    pub disk: Option<DiskUsage>,
    pub installed_cores: usize,
    pub broken_cores: usize,
    /// Non-fatal problems found while gathering the status.
    pub warnings: Vec<ErrorInfo>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Firmware {
    /// Decoded version, e.g. `"2.2"`.
    pub version: Option<String>,
    pub byte: Option<u32>,
    pub name: Option<String>,
    pub build_date: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct DiskUsage {
    pub total_bytes: u64,
    pub available_bytes: u64,
}

/// Gathers the status. Only fails if `Cores/` itself can't be read; problems
/// with `Analogue_Pocket.json` or disk stats become warnings.
pub fn status(pocket: &Pocket) -> Result<Status> {
    Ok(Status::read(pocket, &cores::list_installed(pocket)?))
}

impl Status {
    /// Reads firmware and disk status, reusing the caller's installed-core scan.
    pub fn read(pocket: &Pocket, list: &cores::CoreList) -> Self {
        let mut warnings = Vec::new();

        let pocket_json = match AnaloguePocketJson::read(&pocket.analogue_pocket_json()) {
            Ok(json) => json,
            Err(err) => {
                warnings.push(err.to_info());
                AnaloguePocketJson::default()
            }
        };
        let firmware = pocket_json.runtime().map(|rt| Firmware {
            version: rt.byte.map(firmware_version_from_byte),
            byte: rt.byte,
            name: rt.name.clone(),
            build_date: rt.build_date.clone(),
        });

        let disk = match (
            fs2::total_space(pocket.root()),
            fs2::available_space(pocket.root()),
        ) {
            (Ok(total_bytes), Ok(available_bytes)) => Some(DiskUsage {
                total_bytes,
                available_bytes,
            }),
            (Err(e), _) | (_, Err(e)) => {
                warnings.push(crate::Error::io(pocket.root(), e).to_info());
                None
            }
        };

        Status {
            root: pocket.root().to_path_buf(),
            product: pocket_json.product,
            firmware,
            disk,
            installed_cores: list.cores.len(),
            broken_cores: list.broken.len(),
            warnings,
        }
    }
}
