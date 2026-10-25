use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;
use tracing::debug;

#[derive(Debug, Error)]
pub enum IndexError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("network error: {}", crate::http::describe(.0))]
    Network(#[from] reqwest::Error),
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Crate not found in registry: {0}")]
    CrateNotFound(String),
    #[error(
        "`{0}` is not in the local registry cache and Igel is offline \
         (run without `--offline` once to populate it)"
    )]
    Offline(String),
    #[error("{0}")]
    Config(String),
}

pub type Result<T> = std::result::Result<T, IndexError>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IndexRecord {
    pub name: String,
    pub vers: String,
    #[serde(default)]
    pub deps: Vec<IndexDependency>,
    pub cksum: String,
    #[serde(default)]
    pub features: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub features2: Option<HashMap<String, Vec<String>>>,
    #[serde(default)]
    pub yanked: bool,
    #[serde(default)]
    pub rust_version: Option<String>,
    #[serde(default)]
    pub pubtime: Option<String>,
}

impl IndexRecord {
    pub fn all_features(&self) -> HashMap<String, Vec<String>> {
        let mut feats = self.features.clone();
        if let Some(f2) = &self.features2 {
            for (k, v) in f2 {
                feats.entry(k.clone()).or_default().extend(v.clone());
            }
        }
        feats
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IndexDependency {
    pub name: String,
    pub req: String,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub optional: bool,
    #[serde(default = "default_true")]
    pub default_features: bool,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub kind: Option<String>, // "normal", "dev", "build"
    #[serde(default)]
    pub package: Option<String>,
    #[serde(default)]
    pub registry: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Default)]
pub struct CrateIndex {
    pub name: String,
    pub versions: Vec<IndexRecord>,
}

impl CrateIndex {
    pub fn available_versions(&self) -> Vec<&IndexRecord> {
        self.versions.iter().filter(|v| !v.yanked).collect()
    }

    pub fn get_version(&self, version: &str) -> Option<&IndexRecord> {
        self.versions.iter().find(|v| v.vers == version)
    }
}

pub struct SparseIndexClient {
    base_url: String,
    cache_dir: PathBuf,
    client: crate::http::LazyClient,
    offline: bool,
    token: Option<String>,
    label: String,
}

const PREFETCH_CONCURRENCY: usize = 48;

impl SparseIndexClient {
    pub fn new(cache_dir: PathBuf) -> Self {
        Self {
            base_url: "https://index.crates.io".to_string(),
            cache_dir,
            client: crate::http::LazyClient::new(|b| {
                b.pool_max_idle_per_host(PREFETCH_CONCURRENCY)
                    .tcp_nodelay(true)
                    .connect_timeout(std::time::Duration::from_secs(15))
                    .timeout(std::time::Duration::from_secs(60))
            }),
            offline: false,
            token: None,
            label: "crates.io".to_string(),
        }
    }

    pub fn for_registry(index: &str, token: Option<String>) -> Self {
        if index == crate::source::CRATES_IO && token.is_none() {
            return Self::default_client();
        }
        let url = index
            .strip_prefix("sparse+")
            .unwrap_or(index)
            .trim_end_matches('/')
            .to_string();
        let mut client = Self::new(
            igel_common::settings::igel_home()
                .join("registry")
                .join("index")
                .join(registry_dir_key(index)),
        );
        client.base_url = url;
        client.token = token;
        client.label = index.to_string();
        client
    }

    pub fn default_client() -> Self {
        let cache_dir = igel_common::settings::igel_home()
            .join("registry")
            .join("index")
            .join("crates.io-index");
        Self::new(cache_dir)
    }

    pub fn offline(mut self, offline: bool) -> Self {
        self.offline = offline;
        self
    }

    pub fn fetch_many(&self, names: &[String]) -> Vec<Result<CrateIndex>> {
        self.fetch_many_with(names, false)
    }

    pub fn fetch_many_with(&self, names: &[String], refresh: bool) -> Vec<Result<CrateIndex>> {
        use rayon::prelude::*;
        let one = |n: &String| {
            if refresh {
                self.refresh_crate(n)
            } else {
                self.fetch_crate(n)
            }
        };
        if names.len() < 2 {
            return names.iter().map(one).collect();
        }
        match rayon::ThreadPoolBuilder::new()
            .num_threads(PREFETCH_CONCURRENCY.min(names.len()))
            .build()
        {
            Ok(pool) => pool.install(|| names.par_iter().map(one).collect()),
            Err(_) => names.iter().map(one).collect(),
        }
    }

    pub fn crate_index_path(name: &str) -> String {
        let lower = name.to_lowercase();
        match lower.len() {
            1 => format!("1/{lower}"),
            2 => format!("2/{lower}"),
            3 => {
                let first = &lower[0..1];
                format!("3/{first}/{lower}")
            }
            _ => {
                let first_two = &lower[0..2];
                let second_two = &lower[2..4];
                format!("{first_two}/{second_two}/{lower}")
            }
        }
    }

    pub fn cached_text(&self, name: &str) -> Option<String> {
        fs::read_to_string(self.cache_dir.join(Self::crate_index_path(name))).ok()
    }

    /// Prefers the local cache: published versions never change, so only `refresh_crate` hits the network.
    pub fn fetch_crate(&self, name: &str) -> Result<CrateIndex> {
        let cached_file = self.cache_dir.join(Self::crate_index_path(name));

        if let Some(index) = read_cached(name, &cached_file) {
            return Ok(index);
        }

        if self.offline {
            return Err(IndexError::Offline(name.to_string()));
        }
        self.refresh_crate(name)
    }

    /// Re-queries the index (with `If-None-Match`), falling back to the cache when offline.
    pub fn refresh_crate(&self, name: &str) -> Result<CrateIndex> {
        let rel_path = Self::crate_index_path(name);
        let cached_file = self.cache_dir.join(&rel_path);
        let etag_file = etag_path(&cached_file);

        if self.offline {
            return read_cached(name, &cached_file)
                .ok_or_else(|| IndexError::Offline(name.to_string()));
        }

        let url = format!("{}/{}", self.base_url, rel_path);
        let have_cache = cached_file.is_file();
        use igel_common::progress::{Event, emit};
        emit(Event::IndexStarted { name });
        struct Finished<'a>(&'a str);
        impl Drop for Finished<'_> {
            fn drop(&mut self) {
                emit(Event::IndexFinished { name: self.0 });
            }
        }
        let _finished = Finished(name);
        let etag = have_cache
            .then(|| fs::read_to_string(&etag_file).ok())
            .flatten();
        let sent = crate::http::send_with_retry(|| {
            let mut request = self.client.get(&url);
            if let Some(token) = &self.token {
                request = request.header(reqwest::header::AUTHORIZATION, token.as_str());
            }
            if let Some(etag) = &etag {
                request = request.header(reqwest::header::IF_NONE_MATCH, etag.trim());
            }
            request
        });

        // The cached copy is used instead, so a flaky network never breaks a build.
        let failure = match sent {
            Ok(resp) => {
                let status = resp.status();
                if status == reqwest::StatusCode::NOT_MODIFIED && have_cache {
                    if let Some(index) = read_cached(name, &cached_file) {
                        return Ok(index);
                    }
                    IndexError::Config(format!("the cached index entry of `{name}` vanished"))
                } else if status.is_success() {
                    let etag = resp
                        .headers()
                        .get(reqwest::header::ETAG)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    match resp.text() {
                        Ok(text) => {
                            if let Some(parent) = cached_file.parent() {
                                let _ = fs::create_dir_all(parent);
                            }
                            let tmp =
                                cached_file.with_extension(format!("{}.tmp", std::process::id()));
                            if fs::write(&tmp, &text).is_ok() {
                                let _ = fs::rename(&tmp, &cached_file);
                            }
                            match etag {
                                Some(etag) => {
                                    let _ = fs::write(&etag_file, etag);
                                }
                                None => {
                                    let _ = fs::remove_file(&etag_file);
                                }
                            }
                            return Self::parse_index_text(name, &text);
                        }
                        Err(e) => IndexError::Network(e),
                    }
                } else if status == reqwest::StatusCode::NOT_FOUND
                    || status == reqwest::StatusCode::GONE
                {
                    return Err(IndexError::CrateNotFound(name.to_string()));
                } else if status == reqwest::StatusCode::UNAUTHORIZED
                    || status == reqwest::StatusCode::FORBIDDEN
                {
                    return Err(IndexError::Config(format!(
                        "the registry `{}` requires authentication (HTTP {status}); set a token with \
                         `CARGO_REGISTRIES_<NAME>_TOKEN` or `[registries.<name>] token` / \
                         `credential-provider` in .cargo/config.toml",
                        self.label
                    )));
                } else {
                    IndexError::Config(format!(
                        "the registry `{}` answered HTTP {status} for `{name}` ({url})",
                        self.label
                    ))
                }
            }
            Err(e) => IndexError::Network(e),
        };

        if let Some(index) = read_cached(name, &cached_file) {
            debug!("using the cached index entry of `{name}`: {failure}");
            return Ok(index);
        }
        Err(failure)
    }

    pub fn parse_index_text(name: &str, text: &str) -> Result<CrateIndex> {
        let mut versions = Vec::new();
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(mut record) = serde_json::from_str::<IndexRecord>(trimmed) {
                if let Some(f2) = record.features2.take() {
                    record.features.extend(f2);
                }
                versions.push(record);
            }
        }

        if versions.is_empty() {
            return Err(IndexError::CrateNotFound(name.to_string()));
        }

        Ok(CrateIndex {
            name: name.to_string(),
            versions,
        })
    }
}

/// A binary copy (`.rbin`) beside the JSON is valid while size and mtime match; much faster to decode.
fn read_cached(name: &str, json: &Path) -> Option<CrateIndex> {
    const MAGIC: &[u8; 8] = b"IGELIDX1";
    let meta = fs::metadata(json).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as u64);
    let mut stamp = Vec::with_capacity(24);
    stamp.extend_from_slice(MAGIC);
    stamp.extend_from_slice(&meta.len().to_le_bytes());
    stamp.extend_from_slice(&mtime.to_le_bytes());
    let mut bin_path = json.as_os_str().to_owned();
    bin_path.push(".rbin");
    let bin_path = PathBuf::from(bin_path);
    if let Ok(bytes) = fs::read(&bin_path)
        && bytes.starts_with(&stamp)
        && let Ok(versions) = bincode::deserialize::<Vec<IndexRecord>>(&bytes[stamp.len()..])
    {
        return Some(CrateIndex {
            name: name.to_string(),
            versions,
        });
    }
    let text = fs::read_to_string(json).ok()?;
    let index = SparseIndexClient::parse_index_text(name, &text).ok()?;
    if let Ok(body) = bincode::serialize(&index.versions) {
        let mut out = stamp;
        out.extend_from_slice(&body);
        let tmp = bin_path.with_extension(format!("rbin.{}.tmp", std::process::id()));
        if fs::write(&tmp, &out).is_ok() {
            let _ = fs::rename(&tmp, &bin_path);
        }
    }
    Some(index)
}

pub fn registry_dir_key(index: &str) -> String {
    if index == crate::source::CRATES_IO {
        return "crates.io-index".to_string();
    }
    let host = index
        .trim_start_matches("sparse+")
        .trim_start_matches("registry+")
        .split("://")
        .nth(1)
        .unwrap_or(index)
        .split('/')
        .next()
        .unwrap_or("registry")
        .replace(':', "_");
    let hash = blake3::hash(index.as_bytes()).to_hex();
    format!("{host}-{}", &hash[..16])
}

fn etag_path(cached_file: &std::path::Path) -> PathBuf {
    let mut name = cached_file.file_name().unwrap_or_default().to_os_string();
    name.push(".etag");
    cached_file.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_client_serves_cache_and_never_fetches() {
        let dir = tempfile::tempdir().unwrap();
        let client = SparseIndexClient::new(dir.path().to_path_buf()).offline(true);

        assert!(matches!(
            client.fetch_crate("serde"),
            Err(IndexError::Offline(_))
        ));

        let path = dir
            .path()
            .join(SparseIndexClient::crate_index_path("serde"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"name":"serde","vers":"1.0.0","deps":[],"cksum":"x","features":{},"yanked":false}"#,
        )
        .unwrap();
        assert_eq!(client.fetch_crate("serde").unwrap().versions.len(), 1);
        assert_eq!(client.refresh_crate("serde").unwrap().versions.len(), 1);

        let results = client.fetch_many(&["serde".into(), "missing".into()]);
        assert!(results[0].is_ok());
        assert!(matches!(results[1], Err(IndexError::Offline(_))));
    }

    #[test]
    fn test_crate_index_path() {
        assert_eq!(SparseIndexClient::crate_index_path("a"), "1/a");
        assert_eq!(SparseIndexClient::crate_index_path("ab"), "2/ab");
        assert_eq!(SparseIndexClient::crate_index_path("abc"), "3/a/abc");
        assert_eq!(SparseIndexClient::crate_index_path("serde"), "se/rd/serde");
        assert_eq!(
            SparseIndexClient::crate_index_path("Serde_Json"),
            "se/rd/serde_json"
        );
    }

    #[test]
    fn test_parse_index_text() {
        let sample = r#"{"name":"foo","vers":"0.1.0","deps":[],"cksum":"abc123","features":{},"yanked":false}
{"name":"foo","vers":"0.2.0","deps":[{"name":"bar","req":"^1.0","features":[],"optional":false,"default_features":true}],"cksum":"def456","features":{"default":["std"],"std":[]},"yanked":false}"#;

        let index = SparseIndexClient::parse_index_text("foo", sample).unwrap();
        assert_eq!(index.name, "foo");
        assert_eq!(index.versions.len(), 2);
        assert_eq!(index.versions[1].vers, "0.2.0");
        assert_eq!(index.versions[1].deps.len(), 1);
        assert_eq!(index.versions[1].deps[0].name, "bar");
        assert_eq!(
            index.versions[1].features.get("default").unwrap(),
            &vec!["std".to_string()]
        );
    }
}
