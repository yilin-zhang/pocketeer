// The junk-file filter and Platforms/_archive rule are ported from Pocket Sync
// (src/utils/filterFiles.ts, src-tauri/src/install_zip/mod.rs).
// Copyright (c) Neil Morrison — https://github.com/neil-morrison44/pocket-sync
// Licensed under AGPL-3.0. Modified for pocketeer, 2026.

//! Reading a core release zip without trusting its paths.

use std::{
    io::{Cursor, Read},
    path::{Component, Path, PathBuf},
};

use serde::Serialize;
use zip::ZipArchive;

use crate::{
    Error, Result,
    card::formats::{CoreJson, UpdatersJson, parse_json},
};

/// Where a zip came from, for reports.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ZipSource {
    Url { url: String },
    File { path: PathBuf },
}

impl std::fmt::Display for ZipSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ZipSource::Url { url } => f.write_str(url),
            ZipSource::File { path } => write!(f, "{}", path.display()),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ZipEntry {
    /// Name exactly as stored in the zip (forward slashes).
    pub name: String,
    /// Sanitised relative path: no `..`, no root, no drive.
    pub path: PathBuf,
    pub size: u64,
}

pub struct CoreZip {
    archive: ZipArchive<Cursor<Vec<u8>>>,
    /// File entries only (directories are implied by their contents).
    pub entries: Vec<ZipEntry>,
    pub source: ZipSource,
}

impl std::fmt::Debug for CoreZip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoreZip")
            .field("source", &self.source)
            .field("entries", &self.entries.len())
            .finish()
    }
}

impl CoreZip {
    pub fn from_bytes(bytes: Vec<u8>, source: ZipSource) -> Result<Self> {
        let context = source.to_string();
        let mut archive =
            ZipArchive::new(Cursor::new(bytes)).map_err(|e| Error::zip(e, &context))?;
        let mut entries = Vec::with_capacity(archive.len());
        for i in 0..archive.len() {
            let file = archive.by_index(i).map_err(|e| Error::zip(e, &context))?;
            if file.is_dir() {
                continue;
            }
            let name = file
                .name()
                .map_err(|e| Error::zip(e, &context))?
                .into_owned();
            let path = file
                .enclosed_name()
                .filter(|p| is_plain_relative(p))
                .ok_or_else(|| Error::UnsafeZipPath { name: name.clone() })?;
            entries.push(ZipEntry {
                name,
                path,
                size: file.size(),
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(CoreZip {
            archive,
            entries,
            source,
        })
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).map_err(|e| Error::io(path, e))?;
        Self::from_bytes(
            bytes,
            ZipSource::File {
                path: path.to_path_buf(),
            },
        )
    }

    /// `"<entry> in <source>"`, the context every zip error here carries.
    fn context(&self, name: &str) -> String {
        format!("{name} in {}", self.source)
    }

    fn read_entry(&mut self, name: &str) -> Result<Vec<u8>> {
        let context = self.context(name);
        let mut file = self
            .archive
            .by_name(name)
            .map_err(|e| Error::zip(e, &context))?;
        let mut out = Vec::with_capacity(file.size() as usize);
        file.read_to_end(&mut out)
            .map_err(|e| Error::zip(zip::result::ZipError::Io(e), &context))?;
        Ok(out)
    }

    /// Writes one entry to `dest`, creating parent directories.
    pub fn extract_entry(&mut self, name: &str, dest: &Path) -> Result<()> {
        let context = self.context(name);
        crate::fsutil::write_via_temp(dest, |out| {
            let mut file = self
                .archive
                .by_name(name)
                .map_err(|e| Error::zip(e, context))?;
            std::io::copy(&mut file, out).map_err(|e| Error::io(dest, e))?;
            Ok(())
        })
    }

    /// Ids of cores shipped in this zip (`Cores/<id>/core.json`), sorted.
    pub fn core_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .entries
            .iter()
            .filter_map(|e| {
                let mut comps = e.path.components();
                match (comps.next(), comps.next(), comps.next(), comps.next()) {
                    (
                        Some(Component::Normal(cores)),
                        Some(Component::Normal(id)),
                        Some(Component::Normal(file)),
                        None,
                    ) if cores == crate::card::CORES_DIR && file == "core.json" => {
                        id.to_str().map(str::to_owned)
                    }
                    _ => None,
                }
            })
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }

    pub fn core_json(&mut self, id: &str) -> Result<CoreJson> {
        let name = format!("Cores/{id}/core.json");
        let bytes = self.read_entry(&name)?;
        parse_json(&bytes, Path::new(&format!("zip:{name}")))
    }

    pub fn updaters(&mut self, id: &str) -> Option<UpdatersJson> {
        let name = format!("Cores/{id}/updaters.json");
        if !self.entries.iter().any(|e| e.name == name) {
            return None;
        }
        let bytes = self.read_entry(&name).ok()?;
        UpdatersJson::parse(&bytes).ok()
    }
}

fn is_plain_relative(p: &Path) -> bool {
    !p.as_os_str().is_empty() && p.components().all(|c| matches!(c, Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{CORE_JSON, build_zip};

    #[test]
    fn lists_files_and_core_ids() {
        let bytes = build_zip(&[
            ("Cores/", b""),
            ("Cores/spiritualized.GBA/core.json", CORE_JSON.as_bytes()),
            ("Cores/spiritualized.GBA/gba.rev", b"bits"),
            ("Platforms/gba.json", b"{}"),
            ("README.md", b"hi"),
        ]);
        let mut zip = CoreZip::from_bytes(
            bytes,
            ZipSource::File {
                path: "x.zip".into(),
            },
        )
        .unwrap();
        assert_eq!(zip.entries.len(), 4, "directory entries are dropped");
        assert_eq!(zip.core_ids(), ["spiritualized.GBA"]);
        assert_eq!(
            zip.core_json("spiritualized.GBA")
                .unwrap()
                .core
                .metadata
                .version,
            "1.3.0"
        );
        assert!(zip.updaters("spiritualized.GBA").is_none());
        assert_eq!(
            zip.read_entry("Cores/spiritualized.GBA/gba.rev").unwrap(),
            b"bits"
        );
    }

    #[test]
    fn rejects_zip_slip() {
        let bytes = build_zip(&[("../evil", b"x")]);
        let err = CoreZip::from_bytes(
            bytes,
            ZipSource::File {
                path: "x.zip".into(),
            },
        )
        .unwrap_err();
        assert_eq!(err.code(), "UNSAFE_ZIP_PATH");
    }
}
