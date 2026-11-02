//! Installed cores (`Cores/<author>.<shortname>/`) and how they compare to
//! the inventory.

use std::path::PathBuf;

use serde::Serialize;

use crate::{
    Error, ErrorInfo, Pocket, Result,
    card::formats::{CoreJson, CoreMetadata},
    catalog::inventory::{
        Inventory, InventoryCore, InventoryLoad, InventoryStatus, Release, update_available,
    },
    fsutil,
};

/// A core folder whose `core.json` parsed successfully.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct InstalledCore {
    /// Folder name, `<author>.<shortname>`. This is the id the inventory uses.
    pub id: String,
    pub author: String,
    pub shortname: String,
    pub version: String,
    pub platform_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date_release: Option<String>,
    pub path: PathBuf,
}

/// A core folder pocketeer couldn't read. Reported, not fatal.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct BrokenCore {
    pub id: String,
    pub path: PathBuf,
    pub error: ErrorInfo,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct CoreList {
    pub cores: Vec<InstalledCore>,
    pub broken: Vec<BrokenCore>,
}

impl InstalledCore {
    fn new(id: String, path: PathBuf, metadata: CoreMetadata) -> Self {
        InstalledCore {
            id,
            author: metadata.author,
            shortname: metadata.shortname,
            version: metadata.version,
            platform_ids: metadata.platform_ids,
            description: metadata.description,
            date_release: metadata.date_release,
            path,
        }
    }

    pub fn main_platform(&self) -> Option<&str> {
        self.platform_ids.first().map(String::as_str)
    }
}

impl CoreList {
    pub fn get(&self, id: &str) -> Option<&InstalledCore> {
        self.cores.iter().find(|c| c.id == id)
    }

    pub fn get_ci(&self, id: &str) -> Option<&InstalledCore> {
        self.cores.iter().find(|c| c.id.eq_ignore_ascii_case(id))
    }
}

/// Lists every core folder under `Cores/`, sorted by id (case-insensitive).
///
/// Hidden entries (including macOS `._*` AppleDouble files) are skipped. A
/// folder with a missing or invalid `core.json` lands in [`CoreList::broken`]
/// instead of failing the whole listing.
pub fn list_installed(pocket: &Pocket) -> Result<CoreList> {
    let cores_dir = pocket.cores_dir();
    let entries = std::fs::read_dir(&cores_dir).map_err(|e| Error::io(&cores_dir, e))?;

    let mut list = CoreList::default();
    for entry in entries {
        let entry = entry.map_err(|e| Error::io(&cores_dir, e))?;
        let path = entry.path();
        let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if fsutil::is_hidden(&entry.file_name()) || !path.is_dir() {
            continue;
        }

        match read_core_json(&path) {
            Ok(json) => list
                .cores
                .push(InstalledCore::new(id, path, json.core.metadata)),
            Err(err) => list.broken.push(BrokenCore {
                id,
                path,
                error: err.to_info(),
            }),
        }
    }

    list.cores.sort_by_key(|c| c.id.to_lowercase());
    list.broken.sort_by_key(|c| c.id.to_lowercase());
    Ok(list)
}

/// Reads `<core folder>/core.json`, distinguishing "missing" from "invalid".
fn read_core_json(core_dir: &std::path::Path) -> Result<CoreJson> {
    let core_json = core_dir.join("core.json");
    if !core_json.exists() {
        return Err(Error::CoreJsonMissing {
            path: core_dir.to_path_buf(),
        });
    }
    CoreJson::read(&core_json)
}

/// The newest release the inventory knows about for a core.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LatestRelease {
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date_release: Option<String>,
    pub download_url: String,
    pub requires_license: bool,
}

impl From<&Release> for LatestRelease {
    fn from(r: &Release) -> Self {
        LatestRelease {
            version: r.core.metadata.version.clone(),
            date_release: r.core.metadata.date_release.clone(),
            download_url: r.download_url.clone(),
            requires_license: r.requires_license,
        }
    }
}

/// A core as pocketeer sees it: on the card, in the inventory, or both.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CoreView {
    pub id: String,
    pub platform_ids: Vec<String>,
    /// What is on the card, when it's installed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed: Option<InstalledCore>,
    /// `false` when the inventory doesn't list it (hand-installed or renamed).
    pub in_inventory: bool,
    /// The inventory's newest release, when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest: Option<LatestRelease>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_url: Option<String>,
    /// Installed, and the latest release is newer.
    pub update_available: bool,
}

impl CoreView {
    /// `installed` and `entry` describe the same core; at least one is there.
    fn new(installed: Option<InstalledCore>, entry: Option<&InventoryCore>) -> Self {
        let release = entry.and_then(InventoryCore::latest);
        let latest = release.map(LatestRelease::from);
        let update_available = match (&installed, &latest) {
            (Some(i), Some(l)) => update_available(&i.version, &l.version),
            _ => false,
        };
        let (id, platform_ids) = match (&installed, entry) {
            (Some(i), _) => (i.id.clone(), i.platform_ids.clone()),
            (None, Some(e)) => (
                e.id.clone(),
                release
                    .map(|r| r.core.metadata.platform_ids.clone())
                    .unwrap_or_default(),
            ),
            (None, None) => unreachable!("a core view of nothing"),
        };
        CoreView {
            id,
            platform_ids,
            installed,
            in_inventory: entry.is_some(),
            latest,
            repo_url: entry.map(InventoryCore::repo_url),
            update_available,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CoreListReport {
    pub cores: Vec<CoreView>,
    pub broken: Vec<BrokenCore>,
    pub inventory: InventoryStatus,
}

/// Installed cores annotated with inventory data (when available).
pub fn report(pocket: &Pocket, inventory: &InventoryLoad) -> Result<CoreListReport> {
    Ok(list_installed(pocket)?.report(inventory))
}

impl CoreList {
    /// Annotates this scan without rereading installed definitions.
    pub fn report(self, inventory: &InventoryLoad) -> CoreListReport {
        let inv = inventory.inventory();
        CoreListReport {
            cores: self
                .cores
                .into_iter()
                .map(|c| {
                    let entry = inv.and_then(|inv| inv.get(&c.id));
                    CoreView::new(Some(c), entry)
                })
                .collect(),
            broken: self.broken,
            inventory: inventory.status(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SearchFilter<'a> {
    /// Case-insensitive substring of the id.
    pub query: Option<&'a str>,
    /// Exact platform id.
    pub platform: Option<&'a str>,
    /// Include cores that are already installed.
    pub include_installed: bool,
    /// Include cores with no published release.
    pub include_unreleased: bool,
}

/// Inventory cores matching `filter`, with what is installed of them.
pub fn search(inventory: &Inventory, installed: &CoreList, filter: &SearchFilter) -> Vec<CoreView> {
    let query = filter.query.map(str::to_ascii_lowercase);
    inventory
        .cores
        .iter()
        .filter_map(|c| {
            let latest = c.latest();
            if latest.is_none() && !filter.include_unreleased {
                return None;
            }
            let platform_ids = latest
                .map(|r| r.core.metadata.platform_ids.clone())
                .unwrap_or_default();
            if let Some(p) = filter.platform
                && !platform_ids.iter().any(|x| x == p)
            {
                return None;
            }
            if let Some(q) = &query
                && !c.id.to_ascii_lowercase().contains(q)
            {
                return None;
            }
            let local = installed.get(&c.id);
            if local.is_some() && !filter.include_installed {
                return None;
            }
            Some(CoreView::new(local.cloned(), Some(c)))
        })
        .collect()
}

/// Everything known about one core id.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CoreInfo {
    #[serde(flatten)]
    pub core: CoreView,
    /// The inventory's whole entry, every release included.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inventory: Option<InventoryCore>,
    pub inventory_status: InventoryStatus,
}

/// Looks up `id` locally and in the inventory (both case-insensitively).
pub fn info(pocket: &Pocket, inventory: &InventoryLoad, id: &str) -> Result<CoreInfo> {
    let list = list_installed(pocket)?;
    let installed = list.get_ci(id).cloned();
    let entry = inventory
        .inventory()
        .and_then(|inv| {
            installed
                .as_ref()
                .and_then(|i| inv.get(&i.id))
                .or_else(|| inv.get_ci(id))
        })
        .cloned();
    if installed.is_none() && entry.is_none() {
        return Err(Error::CoreNotFound { id: id.to_string() });
    }
    Ok(CoreInfo {
        core: CoreView::new(installed, entry.as_ref()),
        inventory: entry,
        inventory_status: inventory.status(),
    })
}
