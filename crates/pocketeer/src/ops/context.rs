//! Shared, lazily-built resources for a command: SD card, HTTP, cache,
//! library, config. Setting precedence: flag > environment > config file >
//! built-in default (flags and env are merged by clap already).

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, OnceLock},
};

use pocketeer_core::{
    Error, HashCache, Pocket, Result,
    catalog::inventory::{Inventory, InventoryLoad, Platforms},
    config::Config,
    library::{self, Library},
    net::{Cache, Http, Net, Policy},
};

use crate::cli::Global;

/// Cheap to clone: clones share the loaded config and platform names, so
/// the TUI can hand one to every job without re-reading the config file.
#[derive(Clone)]
pub struct Context {
    global: Arc<Global>,
    card: CardSource,
    config: Arc<OnceLock<Result<Config>>>,
    /// Platform names from the cached inventory only (never worth a network
    /// round-trip on their own); `None` when there is no cache.
    platforms_cached: Arc<OnceLock<Option<Platforms>>>,
}

/// Where a job's card comes from: the CLI looks for one when asked; the TUI
/// tracks the card itself and says which one (or none).
#[derive(Clone)]
enum CardSource {
    Detect,
    Known(Pocket),
    Absent,
}

impl Context {
    pub fn new(global: Global) -> Self {
        Context {
            global: Arc::new(global),
            card: CardSource::Detect,
            config: Arc::default(),
            platforms_cached: Arc::default(),
        }
    }

    /// Uses this card (or none) instead of detecting one.
    pub fn with_pocket(mut self, pocket: Option<Pocket>) -> Self {
        self.card = match pocket {
            Some(p) => CardSource::Known(p),
            None => CardSource::Absent,
        };
        self
    }

    pub fn config_path(&self) -> Result<PathBuf> {
        match &self.global.config {
            Some(p) => Ok(p.clone()),
            None => Config::default_path().ok_or(Error::NoHomeDir),
        }
    }

    /// The config file, loaded once. A malformed file is an error for every
    /// command, so it can't be silently ignored.
    pub fn config(&self) -> Result<&Config> {
        self.config
            .get_or_init(|| Config::load(&self.config_path()?))
            .as_ref()
            .map_err(|e| config_error(e, || self.config_path().unwrap_or_default()))
    }

    fn root_path(&self) -> Result<Option<PathBuf>> {
        if let Some(r) = &self.global.root {
            return Ok(Some(r.clone()));
        }
        Ok(self.config()?.root.clone())
    }

    pub fn pocket(&self) -> Result<Pocket> {
        match &self.card {
            CardSource::Known(p) => Ok(p.clone()),
            CardSource::Absent => Err(Error::SdNotFound {
                searched: Vec::new(),
            }),
            CardSource::Detect => Pocket::open_or_detect(self.root_path()?.as_deref()),
        }
    }

    /// The card if there is one; for work that goes on without it.
    pub fn pocket_opt(&self) -> Option<Pocket> {
        self.pocket().ok()
    }

    fn github_token(&self) -> Result<Option<String>> {
        if let Some(t) = &self.global.github_token {
            return Ok(Some(t.clone()));
        }
        Ok(self.config()?.github_token.clone())
    }

    pub fn http(&self) -> Result<Http> {
        Http::new(self.github_token()?)
    }

    pub fn cache(&self) -> Result<Cache> {
        if let Some(dir) = &self.global.cache_dir {
            return Ok(Cache::new(dir));
        }
        if let Some(dir) = &self.config()?.cache_dir {
            return Ok(Cache::new(dir));
        }
        Cache::default_dir()
            .map(Cache::new)
            .ok_or(Error::NoCacheDir)
    }

    pub fn net(&self) -> Result<Net> {
        Ok(Net::new(self.http()?, self.cache()?, self.offline()))
    }

    /// The persistent ROM hash cache (in-memory if there is no cache dir).
    pub fn hash_cache(&self) -> HashCache {
        HashCache::open(self.cache().ok().map(|c| c.dir().to_path_buf()).as_deref())
    }

    pub fn offline(&self) -> bool {
        self.global.offline
    }

    pub fn policy(&self) -> Policy {
        if self.global.offline {
            Policy::Offline
        } else if self.global.refresh {
            Policy::Refresh
        } else {
            Policy::DEFAULT
        }
    }

    /// Inventory as a value even when unavailable (for read-only reports).
    pub fn inventory_lenient(&self) -> InventoryLoad {
        match (self.http(), self.cache()) {
            (Ok(http), Ok(cache)) => Inventory::load_lenient(&http, &cache, self.policy()),
            (Err(e), _) | (_, Err(e)) => InventoryLoad::Unavailable(e.to_info()),
        }
    }

    /// Inventory, or an error when it can't be had (for commands that need it).
    pub fn inventory(&self) -> Result<Inventory> {
        Inventory::load(&self.http()?, &self.cache()?, self.policy())
    }

    /// Platform names from the inventory, with the given policy.
    pub fn platforms(&self, policy: Policy) -> Result<Platforms> {
        Platforms::load(&self.http()?, &self.cache()?, policy)
    }

    /// A platform's display name ("Game Boy Advance" for `gba`) from the
    /// cached inventory, if we have one.
    /// Platform display names by id, from the cached inventory (empty
    /// without one).
    pub fn platform_names(&self) -> BTreeMap<String, String> {
        self.platforms_cached
            .get_or_init(|| self.platforms(Policy::Offline).ok())
            .iter()
            .flat_map(|p| &p.platforms)
            .map(|p| (p.id.clone(), p.name.clone()))
            .collect()
    }

    /// Where the library lives: flag/env, config, or `~/Pocketeer Library`.
    pub fn library_path(&self) -> Result<PathBuf> {
        if let Some(p) = &self.global.library {
            return Ok(p.clone());
        }
        if let Some(p) = &self.config()?.library {
            return Ok(p.clone());
        }
        library::default_path().ok_or(Error::NoHomeDir)
    }

    pub fn library(&self) -> Result<Library> {
        Library::open(&self.library_path()?)
    }

    /// Where libretro DATs come from.
    pub fn dat_base(&self) -> &str {
        nonempty(&self.global.dat_base).unwrap_or(pocketeer_core::catalog::libretro::DAT_BASE)
    }

    /// Where libretro thumbnails come from.
    pub fn thumbnails_base(&self) -> &str {
        nonempty(&self.global.thumbnails_base)
            .unwrap_or(pocketeer_core::catalog::libretro::THUMBNAILS_BASE)
    }
}

fn nonempty(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|v| !v.is_empty())
}

/// The stored load error, given back with its own code: errors don't clone,
/// so this rebuilds the ones that can happen while loading a config file.
/// NO_HOME_DIR stays NO_HOME_DIR, a read error stays IO_ERROR, and only a
/// malformed file (or anything unforeseen) is CONFIG_PARSE_ERROR.
fn config_error(e: &Error, path: impl FnOnce() -> PathBuf) -> Error {
    match e {
        Error::NoHomeDir => Error::NoHomeDir,
        Error::Io { path, source } => Error::Io {
            path: path.clone(),
            source: std::io::Error::new(source.kind(), source.to_string()),
        },
        Error::ConfigParse { path, message } => Error::ConfigParse {
            path: path.clone(),
            message: message.clone(),
        },
        other => Error::ConfigParse {
            path: path(),
            message: other.to_string(),
        },
    }
}
