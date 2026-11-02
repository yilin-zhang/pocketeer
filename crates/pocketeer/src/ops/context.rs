//! Shared, lazily-built resources for a command: SD card, HTTP, cache,
//! library, config. Setting precedence: flag > environment > config file >
//! built-in default (flags and env are merged by clap already).

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, OnceLock},
};

use serde::Serialize;

use pocketeer_core::{
    Error, HashCache, Pocket, Result,
    card::install::Installer,
    catalog::inventory::{Inventory, InventoryLoad, Platforms},
    config::Config,
    library::{self, Library},
    net::{Cache, Http, Policy},
};

use crate::cli::Global;
use pocketeer_core::config::KEYS;

/// Cheap to clone: clones share the loaded config and platform names, so
/// the TUI can hand one to every job without re-reading the config file.
#[derive(Clone)]
pub struct Context {
    global: Arc<Global>,
    /// The TUI tracks the card itself and sets it here (`Some(None)` = no
    /// card); the CLI leaves this `None` and detects on demand.
    pocket: Option<Option<Pocket>>,
    config: Arc<OnceLock<Result<Config>>>,
    /// Platform names from the cached inventory only (never worth a network
    /// round-trip on their own); `None` when there is no cache.
    platforms_cached: Arc<OnceLock<Option<Platforms>>>,
}

/// What talks to the network: the HTTP client and the download cache, plus
/// whether downloads may happen at all. Build an [`Installer`] from it.
pub struct Net {
    pub http: Http,
    pub cache: Cache,
    offline: bool,
}

impl Net {
    pub fn installer<'a>(&'a self, pocket: &'a Pocket) -> Installer<'a> {
        Installer {
            pocket,
            http: &self.http,
            cache: &self.cache,
            offline: self.offline,
        }
    }
}

impl Context {
    pub fn new(global: Global) -> Self {
        Context {
            global: Arc::new(global),
            pocket: None,
            config: Arc::default(),
            platforms_cached: Arc::default(),
        }
    }

    /// Uses this card (or none) instead of detecting one.
    pub fn with_pocket(mut self, pocket: Option<Pocket>) -> Self {
        self.pocket = Some(pocket);
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
        match &self.pocket {
            Some(Some(p)) => Ok(p.clone()),
            Some(None) => Err(Error::SdNotFound {
                searched: Vec::new(),
            }),
            None => Pocket::open_or_detect(self.root_path()?.as_deref()),
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
        Ok(Net {
            http: self.http()?,
            cache: self.cache()?,
            offline: self.offline(),
        })
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

// ---------------------------------------------------------------------------
// The `config` commands: show and edit the settings file.

#[derive(Debug, Serialize)]
pub struct ConfigPathReport {
    pub path: PathBuf,
    pub exists: bool,
}

pub fn config_path(ctx: &Context) -> Result<ConfigPathReport> {
    let path = ctx.config_path()?;
    Ok(ConfigPathReport {
        exists: path.is_file(),
        path,
    })
}

#[derive(Debug, Serialize)]
pub struct ConfigShowReport {
    pub path: PathBuf,
    /// Only the requested key when one was given.
    pub values: Vec<ConfigEntry>,
}

#[derive(Debug, Serialize)]
pub struct ConfigEntry {
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

fn show(config: &Config, path: PathBuf, only: Option<&str>) -> Result<ConfigShowReport> {
    let keys: Vec<&str> = match only {
        Some(k) => vec![k],
        None => KEYS.to_vec(),
    };
    let mut values = Vec::new();
    for key in keys {
        let mut value = config.get(key)?;
        if key == "github-token"
            && let Some(v) = &value
        {
            value = Some(format!("{}…", &v[..v.len().min(4)]));
        }
        values.push(ConfigEntry {
            key: key.to_string(),
            value,
        });
    }
    Ok(ConfigShowReport { path, values })
}

pub fn config_get(ctx: &Context, key: Option<&str>) -> Result<ConfigShowReport> {
    show(ctx.config()?, ctx.config_path()?, key)
}

/// `value: None` unsets the key.
pub fn config_set(ctx: &Context, key: &str, value: Option<&str>) -> Result<ConfigShowReport> {
    let path = ctx.config_path()?;
    let mut config = ctx.config()?.clone();
    config.set(key, value)?;
    config.save(&path)?;
    show(&config, path, Some(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_errors_keep_their_codes() {
        let path = || PathBuf::from("/cfg");
        assert_eq!(config_error(&Error::NoHomeDir, path).code(), "NO_HOME_DIR");
        let io = Error::Io {
            path: "/cfg".into(),
            source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        };
        let mapped = config_error(&io, path);
        assert_eq!(mapped.code(), "IO_ERROR");
        assert!(mapped.to_string().contains("/cfg"), "{mapped}");
        let parse = Error::ConfigParse {
            path: "/cfg".into(),
            message: "bad toml".into(),
        };
        assert_eq!(config_error(&parse, path).code(), "CONFIG_PARSE_ERROR");
        assert_eq!(
            config_error(&Error::NoCacheDir, path).code(),
            "CONFIG_PARSE_ERROR"
        );
    }
}
