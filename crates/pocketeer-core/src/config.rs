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

pub const KEYS: [&str; 4] = ["library", "cache-dir", "root", "github-token"];

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

    pub fn get(&self, key: &str) -> Result<Option<String>> {
        Ok(match key {
            "library" => self.library.as_ref().map(|p| p.display().to_string()),
            "cache-dir" => self.cache_dir.as_ref().map(|p| p.display().to_string()),
            "root" => self.root.as_ref().map(|p| p.display().to_string()),
            "github-token" => self.github_token.clone(),
            _ => return Err(unknown(key)),
        })
    }

    /// `None` clears the key. Paths are expanded (`~`) and made absolute.
    pub fn set(&mut self, key: &str, value: Option<&str>) -> Result<()> {
        let path = |v: &str| -> PathBuf { absolutize(expand_tilde(v)) };
        match key {
            "library" => self.library = value.map(path),
            "cache-dir" => self.cache_dir = value.map(path),
            "root" => self.root = value.map(path),
            "github-token" => self.github_token = value.map(str::to_owned),
            _ => return Err(unknown(key)),
        }
        Ok(())
    }
}

fn unknown(key: &str) -> Error {
    Error::ConfigKey {
        key: key.to_string(),
        known: KEYS.iter().map(|k| k.to_string()).collect(),
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
        c.set("library", Some("/tmp/lib")).unwrap();
        c.set("github-token", Some("ghp_x")).unwrap();
        c.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.library.as_deref(), Some(Path::new("/tmp/lib")));
        assert_eq!(
            loaded.get("github-token").unwrap().as_deref(),
            Some("ghp_x")
        );
        assert_eq!(loaded.get("root").unwrap(), None);

        let mut c = loaded;
        c.set("github-token", None).unwrap();
        c.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("github"), "{text}");
        assert!(text.contains("library = "), "{text}");
    }

    #[test]
    fn unknown_key_and_bad_file() {
        let mut c = Config::default();
        assert_eq!(c.set("nope", Some("x")).unwrap_err().code(), "CONFIG_KEY");
        assert_eq!(c.get("nope").unwrap_err().code(), "CONFIG_KEY");
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

    #[test]
    fn tilde_expands() {
        let p = expand_tilde("~/x");
        assert!(p.is_absolute());
        assert!(p.ends_with("x"));
        assert_eq!(expand_tilde("/abs"), PathBuf::from("/abs"));
    }
}
