//! HTTP access and an ETag-aware disk cache.
//!
//! Everything here is blocking. Long operations report progress through a
//! callback so callers (CLI spinner, TUI gauge) stay in control of rendering.

use std::{
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

use reqwest::{
    StatusCode,
    blocking::{Client, Response},
    header,
};
use serde::{Deserialize, Serialize};

use crate::{
    Error, Result,
    fsutil::write_atomic,
    plan::{Event, Progress},
};

const USER_AGENT: &str = concat!("pocketeer/", env!("CARGO_PKG_VERSION"));

/// A configured HTTP client.
#[derive(Debug, Clone)]
pub struct Http {
    client: Client,
    github_token: Option<String>,
}

pub enum Conditional {
    NotModified,
    Fresh {
        bytes: Vec<u8>,
        etag: Option<String>,
    },
}

impl Http {
    pub fn new(github_token: Option<String>) -> Result<Self> {
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|source| Error::Network {
                url: String::new(),
                source,
            })?;
        Ok(Http {
            client,
            github_token: github_token.filter(|t| !t.is_empty()),
        })
    }

    fn request(&self, url: &str) -> reqwest::blocking::RequestBuilder {
        let mut req = self.client.get(url);
        // Only ever send the token to GitHub itself.
        if let Some(token) = &self.github_token
            && is_github(url)
        {
            req = req.bearer_auth(token);
        }
        req
    }

    fn send(&self, req: reqwest::blocking::RequestBuilder, url: &str) -> Result<Response> {
        req.send().map_err(|source| Error::Network {
            url: url.to_string(),
            source,
        })
    }

    /// GET with `If-None-Match`; returns `NotModified` on 304.
    pub fn get_conditional(&self, url: &str, etag: Option<&str>) -> Result<Conditional> {
        let mut req = self.request(url);
        if let Some(etag) = etag {
            req = req.header(header::IF_NONE_MATCH, etag);
        }
        let resp = self.send(req, url)?;
        match resp.status() {
            StatusCode::NOT_MODIFIED => Ok(Conditional::NotModified),
            status if status.is_success() => {
                let etag = resp
                    .headers()
                    .get(header::ETAG)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                let bytes = resp
                    .bytes()
                    .map_err(|source| Error::Network {
                        url: url.to_string(),
                        source,
                    })?
                    .to_vec();
                Ok(Conditional::Fresh { bytes, etag })
            }
            status => Err(Error::Http {
                url: url.to_string(),
                status: status.as_u16(),
            }),
        }
    }

    /// Downloads a whole body into memory, reporting bytes so far and, if
    /// the server said, the total.
    pub fn download(
        &self,
        url: &str,
        progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<Vec<u8>> {
        let mut resp = self.send(self.request(url), url)?;
        if !resp.status().is_success() {
            return Err(Error::Http {
                url: url.to_string(),
                status: resp.status().as_u16(),
            });
        }
        let total = resp.content_length();
        let mut out = Vec::with_capacity(total.unwrap_or(0) as usize);
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = resp.read(&mut buf).map_err(|source| Error::Download {
                url: url.to_string(),
                source,
            })?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
            progress(out.len() as u64, total);
        }
        Ok(out)
    }
}

fn is_github(url: &str) -> bool {
    url.strip_prefix("https://")
        .and_then(|rest| rest.split('/').next())
        .is_some_and(|host| host == "github.com" || host.ends_with(".github.com"))
}

/// How willing we are to hit the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// Never touch the network; error if nothing is cached.
    Offline,
    /// Use the cache if younger than this, otherwise revalidate.
    MaxAge(Duration),
    /// Always revalidate (conditional GET, so cheap when unchanged).
    Refresh,
}

impl Policy {
    /// Matches the inventory server's own `cache-control: max-age=600`.
    pub const DEFAULT: Policy = Policy::MaxAge(Duration::from_secs(600));
}

/// Where cached bytes came from.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum Source {
    Network,
    Cache {
        age_secs: u64,
        /// Set when we wanted fresh data but fell back to the cache.
        #[serde(skip_serializing_if = "Option::is_none")]
        fallback_reason: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct Fetched {
    pub bytes: Vec<u8>,
    pub source: Source,
}

#[derive(Debug, Serialize, Deserialize)]
struct Meta {
    url: String,
    etag: Option<String>,
    fetched_at: u64,
}

/// A directory of cached HTTP bodies, each with a sidecar `<key>.meta.json`.
#[derive(Debug, Clone)]
pub struct Cache {
    dir: PathBuf,
}

impl Cache {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Cache { dir: dir.into() }
    }

    /// `~/Library/Caches/pocketeer` on macOS.
    pub fn default_dir() -> Option<PathBuf> {
        dirs::cache_dir().map(|d| d.join("pocketeer"))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn download_path(&self, url: &str) -> PathBuf {
        self.dir.join("downloads").join(download_file_name(url))
    }

    /// Puts a body in the download cache as if fetched from `url`.
    pub fn store_download(&self, url: &str, bytes: &[u8]) -> Result<()> {
        write_atomic(&self.download_path(url), bytes)
    }

    /// Downloads `url` into `<cache>/downloads/`, or reuses the file already
    /// there. Release assets are immutable per URL, so no revalidation.
    pub fn download(
        &self,
        http: &Http,
        url: &str,
        offline: bool,
        progress: Progress<'_>,
    ) -> Result<Vec<u8>> {
        let file = self.download_path(url);
        if let Ok(bytes) = std::fs::read(&file)
            && !bytes.is_empty()
        {
            progress(Event::Status(format!(
                "Using cached download {}",
                file.file_name().and_then(|n| n.to_str()).unwrap_or("")
            )));
            return Ok(bytes);
        }
        if offline {
            return Err(Error::OfflineNoCache {
                what: format!("download of {url}"),
            });
        }
        let bytes = http.download(url, &mut |received, total| {
            progress(Event::Download {
                url: url.to_string(),
                received,
                total,
            })
        })?;
        write_atomic(&file, &bytes)?;
        Ok(bytes)
    }
}

/// `<hash of url>-<basename>`: unique per URL, still recognisable.
fn download_file_name(url: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    url.hash(&mut h);
    let base: String = url
        .rsplit('/')
        .next()
        .unwrap_or("download")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .take(80)
        .collect();
    format!(
        "{:016x}-{}",
        h.finish(),
        if base.is_empty() {
            "download.zip".into()
        } else {
            base
        }
    )
}

impl Cache {
    fn body_path(&self, key: &str) -> PathBuf {
        self.dir.join(key)
    }

    fn meta_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.meta.json"))
    }

    fn read(&self, key: &str) -> Option<(Vec<u8>, Meta)> {
        let bytes = std::fs::read(self.body_path(key)).ok()?;
        let meta: Meta = serde_json::from_slice(&std::fs::read(self.meta_path(key)).ok()?).ok()?;
        Some((bytes, meta))
    }

    /// Stores a body as if fetched from `url` just now.
    pub fn store(&self, key: &str, url: &str, bytes: &[u8], etag: Option<String>) -> Result<()> {
        let meta = Meta {
            url: url.to_string(),
            etag,
            fetched_at: crate::fsutil::now_secs() as u64,
        };
        write_atomic(&self.body_path(key), bytes)?;
        write_atomic(
            &self.meta_path(key),
            &serde_json::to_vec(&meta).expect("meta"),
        )
    }

    /// Touches the sidecar so a 304 counts as a fresh validation.
    fn touch(&self, key: &str, meta: &Meta) -> Result<()> {
        let meta = Meta {
            fetched_at: crate::fsutil::now_secs() as u64,
            url: meta.url.clone(),
            etag: meta.etag.clone(),
        };
        write_atomic(
            &self.meta_path(key),
            &serde_json::to_vec(&meta).expect("meta"),
        )
    }

    /// Fetches `url` according to `policy`, caching under `key`.
    ///
    /// `what` names the resource in error messages ("core inventory").
    pub fn fetch(
        &self,
        http: &Http,
        key: &str,
        url: &str,
        what: &str,
        policy: Policy,
    ) -> Result<Fetched> {
        let cached = self.read(key).filter(|(_, meta)| meta.url == url);
        let age = cached.as_ref().map(|(_, meta)| {
            Duration::from_secs((crate::fsutil::now_secs() as u64).saturating_sub(meta.fetched_at))
        });

        match policy {
            Policy::Offline => {
                return cached
                    .map(|(bytes, _)| Fetched {
                        bytes,
                        source: Source::Cache {
                            age_secs: age.unwrap_or_default().as_secs(),
                            fallback_reason: None,
                        },
                    })
                    .ok_or_else(|| Error::OfflineNoCache {
                        what: what.to_string(),
                    });
            }
            Policy::MaxAge(max) => {
                if let (Some((bytes, _)), Some(age)) = (&cached, age)
                    && age <= max
                {
                    return Ok(Fetched {
                        bytes: bytes.clone(),
                        source: Source::Cache {
                            age_secs: age.as_secs(),
                            fallback_reason: None,
                        },
                    });
                }
            }
            Policy::Refresh => {}
        }

        let etag = cached.as_ref().and_then(|(_, meta)| meta.etag.as_deref());
        match http.get_conditional(url, etag) {
            Ok(Conditional::Fresh { bytes, etag }) => {
                self.store(key, url, &bytes, etag)?;
                Ok(Fetched {
                    bytes,
                    source: Source::Network,
                })
            }
            Ok(Conditional::NotModified) => {
                // Server said our copy is current. If we somehow have no copy, refetch unconditionally.
                match cached {
                    Some((bytes, meta)) => {
                        self.touch(key, &meta)?;
                        Ok(Fetched {
                            bytes,
                            source: Source::Network,
                        })
                    }
                    None => match http.get_conditional(url, None)? {
                        Conditional::Fresh { bytes, etag } => {
                            self.store(key, url, &bytes, etag)?;
                            Ok(Fetched {
                                bytes,
                                source: Source::Network,
                            })
                        }
                        Conditional::NotModified => Err(Error::Http {
                            url: url.to_string(),
                            status: 304,
                        }),
                    },
                }
            }
            Err(err) => match cached {
                // Network trouble but we have something: use it, and say so.
                Some((bytes, _)) => Ok(Fetched {
                    bytes,
                    source: Source::Cache {
                        age_secs: age.unwrap_or_default().as_secs(),
                        fallback_reason: Some(err.to_string()),
                    },
                }),
                None => Err(err),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_detection() {
        assert!(is_github("https://github.com/a/b/releases/download/x.zip"));
        assert!(is_github("https://api.github.com/repos"));
        assert!(!is_github("https://openfpga-library.github.io/x.json"));
        assert!(!is_github("https://evil.com/github.com/"));
    }

    #[test]
    fn cache_roundtrip_with_etag_and_offline() {
        let mut server = mockito::Server::new();
        let url = format!("{}/cores.json", server.url());
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::new(tmp.path());
        let http = Http::new(None).unwrap();

        let first = server
            .mock("GET", "/cores.json")
            .with_header("etag", "\"v1\"")
            .with_body(r#"{"n":1}"#)
            .expect(1)
            .create();
        let got = cache
            .fetch(&http, "cores.json", &url, "test", Policy::Refresh)
            .unwrap();
        assert_eq!(got.bytes, br#"{"n":1}"#);
        assert_eq!(got.source, Source::Network);
        first.assert();

        // Within max-age: served from cache, no request.
        let got = cache
            .fetch(&http, "cores.json", &url, "test", Policy::DEFAULT)
            .unwrap();
        // A second may tick over between the two fetches.
        assert!(matches!(
            got.source,
            Source::Cache {
                age_secs: 0..=1,
                ..
            }
        ));

        // Refresh sends If-None-Match and honours 304.
        let second = server
            .mock("GET", "/cores.json")
            .match_header("if-none-match", "\"v1\"")
            .with_status(304)
            .expect(1)
            .create();
        let got = cache
            .fetch(&http, "cores.json", &url, "test", Policy::Refresh)
            .unwrap();
        assert_eq!(got.bytes, br#"{"n":1}"#);
        assert_eq!(got.source, Source::Network);
        second.assert();

        // Offline works once cached.
        let got = cache
            .fetch(&http, "cores.json", &url, "test", Policy::Offline)
            .unwrap();
        assert_eq!(got.bytes, br#"{"n":1}"#);
    }

    #[test]
    fn offline_without_cache_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::new(tmp.path());
        let http = Http::new(None).unwrap();
        let err = cache
            .fetch(
                &http,
                "k",
                "http://127.0.0.1:9/x",
                "inventory",
                Policy::Offline,
            )
            .unwrap_err();
        assert_eq!(err.code(), "OFFLINE_NO_CACHE");
    }

    #[test]
    fn network_failure_falls_back_to_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::new(tmp.path());
        let http = Http::new(None).unwrap();
        // Port 9 (discard) refuses connections quickly.
        let url = "http://127.0.0.1:9/x";
        cache.store("k", url, b"old", None).unwrap();
        let got = cache.fetch(&http, "k", url, "t", Policy::Refresh).unwrap();
        assert_eq!(got.bytes, b"old");
        assert!(matches!(
            got.source,
            Source::Cache {
                fallback_reason: Some(_),
                ..
            }
        ));
    }
}
