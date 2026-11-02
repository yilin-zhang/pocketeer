//! Persistent user settings: `~/Library/Application Support/pocketeer/config.toml`.
//!
//! Every setting is also a CLI flag / environment variable; the file is the
//! lowest-priority source (flag > env > config > built-in default).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Error, Result, fsutil::write_atomic};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Config {
    /// Library folder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub library: Option<PathBuf>,
    /// Download / inventory cache folder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_dir: Option<PathBuf>,
    /// SD card root, when auto-detection isn't wanted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root: Option<PathBuf>,
    /// GitHub token for release downloads (plain text!).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub github_token: Option<String>,
}

/// A setting `pocketeer config` shows and changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigKey {
    Library,
    CacheDir,
    Root,
    GithubToken,
}

impl ConfigKey {
    pub const ALL: [ConfigKey; 4] = [
        ConfigKey::Library,
        ConfigKey::CacheDir,
        ConfigKey::Root,
        ConfigKey::GithubToken,
    ];

    /// The name on the command line and in the file.
    pub fn name(self) -> &'static str {
        match self {
            ConfigKey::Library => "library",
            ConfigKey::CacheDir => "cache-dir",
            ConfigKey::Root => "root",
            ConfigKey::GithubToken => "github-token",
        }
    }

    pub fn from_name(name: &str) -> Option<ConfigKey> {
        ConfigKey::ALL.into_iter().find(|k| k.name() == name)
    }
}

impl Config {
    pub fn default_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("pocketeer").join("config.toml"))
    }

    /// Missing file → defaults. Malformed file → error (never silently ignored).
    pub fn load(path: &Path) -> Result<Config> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(Error::io(path, e)),
        };
        toml::from_str(&text).map_err(|e| Error::ConfigParse {
            path: path.to_path_buf(),
            message: e.to_string(),
        })
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self).expect("config serializes");
        write_atomic(path, text.as_bytes())
    }

    pub fn get(&self, key: ConfigKey) -> Option<String> {
        let path = |p: &Option<PathBuf>| p.as_ref().map(|p| p.display().to_string());
        match key {
            ConfigKey::Library => path(&self.library),
            ConfigKey::CacheDir => path(&self.cache_dir),
            ConfigKey::Root => path(&self.root),
            ConfigKey::GithubToken => self.github_token.clone(),
        }
    }

    /// `None` clears the key. Paths are expanded (`~`) and made absolute.
    pub fn set(&mut self, key: ConfigKey, value: Option<&str>) {
        let path = |v: &str| -> PathBuf { absolutize(expand_tilde(v)) };
        match key {
            ConfigKey::Library => self.library = value.map(path),
            ConfigKey::CacheDir => self.cache_dir = value.map(path),
            ConfigKey::Root => self.root = value.map(path),
            ConfigKey::GithubToken => self.github_token = value.map(str::to_owned),
        }
    }
}

fn expand_tilde(v: &str) -> PathBuf {
    if let Some(rest) = v.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    if v == "~"
        && let Some(home) = dirs::home_dir()
    {
        return home;
    }
    PathBuf::from(v)
}

fn absolutize(p: PathBuf) -> PathBuf {
    if p.is_absolute() {
        p
    } else {
        std::env::current_dir().map(|c| c.join(&p)).unwrap_or(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested/config.toml");
        assert_eq!(Config::load(&path).unwrap(), Config::default());

        let mut c = Config::default();
        c.set(ConfigKey::Library, Some("/tmp/lib"));
        c.set(ConfigKey::GithubToken, Some("ghp_x"));
        c.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.library.as_deref(), Some(Path::new("/tmp/lib")));
        assert_eq!(loaded.get(ConfigKey::GithubToken).as_deref(), Some("ghp_x"));
        assert_eq!(loaded.get(ConfigKey::Root), None);

        let mut c = loaded;
        c.set(ConfigKey::GithubToken, None);
        c.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("github"), "{text}");
        assert!(text.contains("library = "), "{text}");
    }

    #[test]
    fn a_malformed_file_is_an_error_not_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "library = [1,\n").unwrap();
        assert_eq!(
            Config::load(&path).unwrap_err().code(),
            "CONFIG_PARSE_ERROR"
        );
        std::fs::write(&path, "unknown_key = 1\n").unwrap();
        assert_eq!(
            Config::load(&path).unwrap_err().code(),
            "CONFIG_PARSE_ERROR"
        );
    }
}
