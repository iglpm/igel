use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum IndexError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Crate not found in registry: {0}")]
    CrateNotFound(String),
    #[error(
        "`{0}` is not in the local registry cache and Rune is offline \
         (run without `--offline` once to populate it)"
    )]
    Offline(String),
    #[error("{0}")]
    Config(String),
}

pub type Result<T> = std::result::Result<T, IndexError>;

/// A single version entry in the crates.io sparse index.
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
}

impl IndexRecord {
    /// Combines features and features2 into a single unified map.
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

/// A dependency specification inside an index record.
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
    /// The index of the registry this dependency comes from, when it is not
    /// the registry of the depending package (`None` means the same one).
    #[serde(default)]
    pub registry: Option<String>,
}

fn default_true() -> bool {
    true
}

/// In-memory representation of all published versions of a crate.
#[derive(Debug, Clone, Default)]
pub struct CrateIndex {
    pub name: String,
    pub versions: Vec<IndexRecord>,
}

impl CrateIndex {
    /// Returns all un-yanked versions sorted in index order.
    pub fn available_versions(&self) -> Vec<&IndexRecord> {
        self.versions.iter().filter(|v| !v.yanked).collect()
    }

    /// Finds a specific version record.
    pub fn get_version(&self, version: &str) -> Option<&IndexRecord> {
        self.versions.iter().find(|v| v.vers == version)
    }
}

/// Client for the crates.io Sparse Index (`https://index.crates.io/`).
pub struct SparseIndexClient {
    base_url: String,
    cache_dir: PathBuf,
    client: reqwest::blocking::Client,
    /// Never touch the network: serve only what is already cached.
    offline: bool,
    /// Sent as the `Authorization` header (private registries).
    token: Option<String>,
    /// For messages: the registry's identity.
    label: String,
}

/// How many index requests to keep in flight at once. Index files are tiny
/// and the cost is almost entirely round-trip latency, so far more than the
/// CPU count pays off.
const PREFETCH_CONCURRENCY: usize = 48;

impl SparseIndexClient {
    /// Creates a new `SparseIndexClient`.
    pub fn new(cache_dir: PathBuf) -> Self {
        Self {
            base_url: "https://index.crates.io".to_string(),
            cache_dir,
            client: reqwest::blocking::Client::builder()
                .user_agent("rune (github.com/jonas/rune)")
                .pool_max_idle_per_host(PREFETCH_CONCURRENCY)
                .tcp_nodelay(true)
                .connect_timeout(std::time::Duration::from_secs(15))
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .unwrap_or_else(|_| reqwest::blocking::Client::new()),
            offline: false,
            token: None,
            label: "crates.io".to_string(),
        }
    }

    /// A client for an alternative sparse registry. Its cache lives in its own
    /// directory, so registries never see each other's index files.
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
            rune_common::settings::rune_home()
                .join("registry")
                .join("index")
                .join(registry_dir_key(index)),
        );
        client.base_url = url;
        client.token = token;
        client.label = index.to_string();
        client
    }

    /// Default client targeting `~/.rune/registry/index/crates.io-index`
    /// (or `$RUNE_HOME/registry/...`).
    pub fn default_client() -> Self {
        let cache_dir = rune_common::settings::rune_home()
            .join("registry")
            .join("index")
            .join("crates.io-index");
        Self::new(cache_dir)
    }

    /// Makes this client serve only from the local cache.
    pub fn offline(mut self, offline: bool) -> Self {
        self.offline = offline;
        self
    }

    /// Fetches many crates' indexes concurrently, in the same order as
    /// `names`. Cached entries return immediately; the rest overlap their
    /// network round trips instead of paying them one after another, which is
    /// what dominated cold resolution time.
    pub fn fetch_many(&self, names: &[String]) -> Vec<Result<CrateIndex>> {
        self.fetch_many_with(names, false)
    }

    /// Like [`Self::fetch_many`]; with `refresh`, every crate is re-queried
    /// over the network (conditionally, see [`Self::refresh_crate`]).
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

    /// Computes the sparse index sharding path for a crate name according to the RFC.
    /// - 1 char: `1/{name}`
    /// - 2 chars: `2/{name}`
    /// - 3 chars: `3/{first_char}/{name}`
    /// - 4+ chars: `{first_2}/{next_2}/{name}`
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

    /// Fetches the crate index, preferring the local cache.
    ///
    /// Mirrors Cargo's own behavior: the registry index is only queried over
    /// the network when no local copy exists yet. A crate's published
    /// versions never change in place on crates.io, so once an index entry
    /// has been fetched it remains valid for resolution purposes; only
    /// `refresh_crate` (used by `rune update`/`rune add`) re-queries the
    /// network to discover newly published versions. This is what removes
    /// per-build network round-trips from the common "nothing changed"
    /// rebuild path.
    pub fn fetch_crate(&self, name: &str) -> Result<CrateIndex> {
        let cached_file = self.cache_dir.join(Self::crate_index_path(name));

        if let Ok(content) = fs::read_to_string(&cached_file) {
            if let Ok(index) = Self::parse_index_text(name, &content) {
                return Ok(index);
            }
        }

        if self.offline {
            return Err(IndexError::Offline(name.to_string()));
        }
        self.refresh_crate(name)
    }

    /// Forces a network refresh of the sparse index entry for `name`,
    /// updating the local cache. Falls back to the local cache if the
    /// network is unreachable.
    ///
    /// Uses a conditional request (`If-None-Match`) when the cached copy has a
    /// stored ETag, so refreshing an unchanged crate — the common case for
    /// `rune update` — costs a tiny `304 Not Modified` instead of the full
    /// file.
    pub fn refresh_crate(&self, name: &str) -> Result<CrateIndex> {
        let rel_path = Self::crate_index_path(name);
        let cached_file = self.cache_dir.join(&rel_path);
        let etag_file = etag_path(&cached_file);

        if self.offline {
            return match fs::read_to_string(&cached_file) {
                Ok(content) => Self::parse_index_text(name, &content),
                Err(_) => Err(IndexError::Offline(name.to_string())),
            };
        }

        let url = format!("{}/{}", self.base_url, rel_path);
        let mut request = self.client.get(&url);
        if let Some(token) = &self.token {
            request = request.header(reqwest::header::AUTHORIZATION, token.as_str());
        }
        let have_cache = cached_file.is_file();
        if have_cache {
            if let Ok(etag) = fs::read_to_string(&etag_file) {
                request = request.header(reqwest::header::IF_NONE_MATCH, etag.trim());
            }
        }

        if let Ok(resp) = request.send() {
            let status = resp.status();
            if status == reqwest::StatusCode::NOT_MODIFIED && have_cache {
                if let Ok(content) = fs::read_to_string(&cached_file) {
                    return Self::parse_index_text(name, &content);
                }
            } else if status.is_success() {
                let etag = resp
                    .headers()
                    .get(reqwest::header::ETAG)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                if let Ok(text) = resp.text() {
                    if let Some(parent) = cached_file.parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                    // Write to a temp name and rename: parallel fetches and
                    // concurrent `rune` processes share this cache.
                    let tmp = cached_file.with_extension(format!("{}.tmp", std::process::id()));
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
            }
        }

        // Fallback to local cache if offline or network failed
        if let Ok(content) = fs::read_to_string(&cached_file) {
            return Self::parse_index_text(name, &content);
        }

        Err(IndexError::CrateNotFound(name.to_string()))
    }

    /// Parses a line-delimited JSON sparse index file.
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

/// A directory name for a registry's caches: readable host plus a hash of the
/// full identity, so two registries never collide.
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

        // Not cached: a clear offline error, not a network attempt or a
        // misleading "crate not found".
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
