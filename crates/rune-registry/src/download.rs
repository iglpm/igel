use flate2::read::GzDecoder;
use rayon::prelude::*;
use std::fs::{self, File};
use std::io::BufReader;
use std::path::{Path, PathBuf};
use tar::Archive;
use thiserror::Error;
use tracing::{debug, info};

#[derive(Debug, Error)]
pub enum DownloadError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("download failed: {}", crate::http::describe(.0))]
    Network(#[from] reqwest::Error),
    #[error(
        "Checksum mismatch for {crate_name} v{version}: expected {expected}, computed {computed}"
    )]
    ChecksumMismatch {
        crate_name: String,
        version: String,
        expected: String,
        computed: String,
    },
    #[error("Tar archive extraction error: {0}")]
    Archive(String),
    #[error("{0}")]
    Config(String),
}

pub type Result<T> = std::result::Result<T, DownloadError>;

/// Downloader for crate packages from crates.io.
pub struct CrateDownloader {
    crates_dir: PathBuf,
    src_dir: PathBuf,
    client: crate::http::LazyClient,
    offline: bool,
    backend: DlBackend,
}

/// Where the `.crate` archives of a registry come from.
enum DlBackend {
    CratesIo,
    /// An alternative sparse registry: the download URL comes from its
    /// `config.json`.
    Sparse {
        base: String,
        token: Option<String>,
        label: String,
    },
    /// `local-registry`: `.crate` files next to the index.
    LocalRegistry(PathBuf),
    /// `directory`: crates already unpacked; nothing to fetch or extract.
    Directory(PathBuf),
}

/// Downloads are latency-bound, so run many at once regardless of core count.
const DOWNLOAD_CONCURRENCY: usize = 32;

impl CrateDownloader {
    /// Creates a new `CrateDownloader` with the given base cache directory.
    pub fn new(base_cache_dir: PathBuf) -> Self {
        let crates_dir = base_cache_dir.join("cache");
        let src_dir = base_cache_dir
            .join("src")
            .join("index.crates.io-6f17d22bba15001f");
        Self {
            crates_dir,
            src_dir,
            client: crate::http::LazyClient::new(|b| {
                b.pool_max_idle_per_host(DOWNLOAD_CONCURRENCY)
                    .tcp_nodelay(true)
                    .connect_timeout(std::time::Duration::from_secs(15))
                    .timeout(std::time::Duration::from_secs(300))
            }),
            offline: false,
            backend: DlBackend::CratesIo,
        }
    }

    /// A downloader for the registry identified by `index`, honouring source
    /// replacement and authentication from configuration.
    pub fn for_registry(index: &str) -> std::result::Result<Self, crate::index::IndexError> {
        use crate::source::{Backend, CRATES_IO, defs};
        let backend = defs().backend_for(index)?;
        let mut d = Self::default_downloader();
        let key = crate::index::registry_dir_key(index);
        let home = rune_common::settings::rune_home().join("registry");
        match backend {
            Backend::Sparse { url, token } => {
                if index != CRATES_IO || url != CRATES_IO || token.is_some() {
                    d.crates_dir = home.join("cache").join(&key);
                    d.src_dir = home.join("src").join(&key);
                    d.backend = DlBackend::Sparse {
                        base: url
                            .strip_prefix("sparse+")
                            .unwrap_or(&url)
                            .trim_end_matches('/')
                            .to_string(),
                        token,
                        label: index.to_string(),
                    };
                }
            }
            Backend::LocalRegistry(dir) => {
                d.crates_dir = home.join("cache").join(&key);
                d.src_dir = home.join("src").join(&key);
                d.backend = DlBackend::LocalRegistry(dir);
            }
            Backend::Directory(dir) => d.backend = DlBackend::Directory(dir),
            Backend::GitIndex { url, token } => {
                // Downloads work as for a sparse registry, from the `dl`
                // template in the clone's `config.json`.
                d.crates_dir = home.join("cache").join(&key);
                d.src_dir = home.join("src").join(&key);
                if let Ok(clone) = crate::git_index::GitIndex::open(&url, false) {
                    let _ = std::fs::create_dir_all(&d.crates_dir);
                    let _ = std::fs::copy(
                        clone.dir().join("config.json"),
                        d.crates_dir.join("config.json"),
                    );
                }
                d.backend = DlBackend::Sparse {
                    base: url
                        .trim_start_matches("registry+")
                        .trim_end_matches('/')
                        .to_string(),
                    token,
                    label: index.to_string(),
                };
            }
        }
        Ok(d)
    }

    /// The download URL of a package of an alternative registry, from the
    /// registry's `config.json` template: `{crate}`, `{version}`, `{prefix}`,
    /// `{lowerprefix}` and `{sha256-checksum}` are substituted; a template
    /// without markers gets `/{crate}/{version}/download` appended.
    fn sparse_download_url(
        &self,
        base: &str,
        token: Option<&str>,
        name: &str,
        version: &str,
        cksum: Option<&str>,
    ) -> Result<String> {
        let config_path = self.crates_dir.join("config.json");
        let cfg: serde_json::Value = match std::fs::read_to_string(&config_path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
        {
            Some(v) => v,
            None => {
                if self.offline {
                    return Err(DownloadError::Archive(format!(
                        "the registry config of {base} is not cached and Rune is offline"
                    )));
                }
                let mut req = self.client.get(format!("{base}/config.json"));
                if let Some(t) = token {
                    req = req.header(reqwest::header::AUTHORIZATION, t);
                }
                let text = crate::http::send_with_retry(|| req.try_clone().expect("GET"))?
                    .error_for_status()?
                    .text()?;
                fs::create_dir_all(&self.crates_dir)?;
                let _ = fs::write(&config_path, &text);
                serde_json::from_str(&text).map_err(|e| {
                    DownloadError::Archive(format!("invalid config.json from {base}: {e}"))
                })?
            }
        };
        let dl = cfg
            .get("dl")
            .and_then(|v| v.as_str())
            .ok_or_else(|| DownloadError::Archive(format!("{base}/config.json has no `dl`")))?;
        let prefix = match name.len() {
            1 => "1".to_string(),
            2 => "2".to_string(),
            3 => format!("3/{}", &name[..1]),
            _ => format!("{}/{}", &name[..2], &name[2..4]),
        };
        const MARKERS: [&str; 5] = [
            "{crate}",
            "{version}",
            "{prefix}",
            "{lowerprefix}",
            "{sha256-checksum}",
        ];
        if MARKERS.iter().any(|m| dl.contains(m)) {
            Ok(dl
                .replace("{crate}", name)
                .replace("{version}", version)
                .replace("{prefix}", &prefix)
                .replace("{lowerprefix}", &prefix.to_lowercase())
                .replace("{sha256-checksum}", cksum.unwrap_or("")))
        } else {
            Ok(format!(
                "{}/{name}/{version}/download",
                dl.trim_end_matches('/')
            ))
        }
    }

    /// Default downloader using `~/.rune/registry`.
    pub fn default_downloader() -> Self {
        Self::new(rune_common::settings::rune_home().join("registry"))
    }

    /// Makes this downloader fail (rather than fetch) for crates that are not
    /// already extracted or cached locally.
    pub fn offline(mut self, offline: bool) -> Self {
        self.offline = offline;
        self
    }

    /// Downloads and extracts a crate tarball if not already present.
    /// Returns the path to the extracted crate directory (which contains its `Cargo.toml`).
    pub fn download_and_extract(
        &self,
        name: &str,
        version: &str,
        expected_cksum: Option<&str>,
    ) -> Result<PathBuf> {
        let crate_folder_name = format!("{name}-{version}");
        let extracted_path = self.src_dir.join(&crate_folder_name);

        // Vendored crates are already unpacked (and `cargo vendor` may name
        // the directory with or without the version).
        if let DlBackend::Directory(dir) = &self.backend {
            for candidate in [dir.join(&crate_folder_name), dir.join(name)] {
                if candidate.join("Cargo.toml").is_file() {
                    return Ok(candidate);
                }
            }
            return Err(DownloadError::Archive(format!(
                "{crate_folder_name} is not in the vendor directory {}",
                dir.display()
            )));
        }

        // Fast path: if extracted directory already exists with a Cargo.toml, reuse immediately
        if extracted_path.exists() && extracted_path.join("Cargo.toml").exists() {
            debug!(
                "Crate {} v{} is already cached at {}",
                name,
                version,
                extracted_path.display()
            );
            return Ok(extracted_path);
        }

        fs::create_dir_all(&self.crates_dir)?;
        fs::create_dir_all(&self.src_dir)?;

        let tarball_path = self.crates_dir.join(format!("{crate_folder_name}.crate"));

        // Download if tarball is not already downloaded
        if !tarball_path.exists() {
            if self.offline {
                return Err(DownloadError::Archive(format!(
                    "{crate_folder_name} is not in the local registry cache and Rune is offline"
                )));
            }
            if let DlBackend::LocalRegistry(dir) = &self.backend {
                let src = dir.join(format!("{crate_folder_name}.crate"));
                if !src.is_file() {
                    return Err(DownloadError::Archive(format!(
                        "{} is not in the local registry {}",
                        src.file_name().unwrap().to_string_lossy(),
                        dir.display()
                    )));
                }
                fs::copy(&src, &tarball_path)?;
            } else {
                let (url, token) = match &self.backend {
                    DlBackend::Sparse { base, token, .. } => (
                        self.sparse_download_url(
                            base,
                            token.as_deref(),
                            name,
                            version,
                            expected_cksum,
                        )?,
                        token.clone(),
                    ),
                    _ => (
                        format!("https://static.crates.io/crates/{name}/{name}-{version}.crate"),
                        None,
                    ),
                };
                debug!("Downloading crate {} from {}", crate_folder_name, url);
                use rune_common::progress::{CountingReader, Event, emit};
                emit(Event::DownloadStarted { name, version });
                struct Finished<'a>(&'a str, &'a str, bool);
                impl Drop for Finished<'_> {
                    fn drop(&mut self) {
                        emit(Event::DownloadFinished {
                            name: self.0,
                            version: self.1,
                            ok: self.2,
                        });
                    }
                }
                let mut finished = Finished(name, version, false);

                // Stream into a temp file and rename, so an interrupted download
                // can never be mistaken for a complete tarball later. A
                // connection that drops mid-body is retried like one that
                // never connected.
                let tmp_tarball = self
                    .crates_dir
                    .join(format!(".{crate_folder_name}.{}.part", std::process::id()));
                let retries = rune_common::net::global().retries;
                let mut attempt = 0;
                loop {
                    let mut resp = crate::http::send_with_retry(|| {
                        let mut request = self.client.get(&url);
                        if let Some(token) = &token {
                            request =
                                request.header(reqwest::header::AUTHORIZATION, token.as_str());
                        }
                        request
                    })?;
                    if !resp.status().is_success() {
                        let hint = match (&self.backend, resp.status().as_u16()) {
                            (DlBackend::Sparse { label, .. }, 401 | 403) => format!(
                                " (the registry `{label}` requires authentication: set \
                             `CARGO_REGISTRIES_<NAME>_TOKEN` or a token in .cargo/config.toml)"
                            ),
                            _ => String::new(),
                        };
                        return Err(DownloadError::Archive(format!(
                            "Failed to download {}: HTTP {}{hint}",
                            url,
                            resp.status()
                        )));
                    }
                    let copied = File::create(&tmp_tarball).and_then(|mut out_file| {
                        std::io::copy(&mut CountingReader(&mut resp), &mut out_file)
                    });
                    match copied {
                        Ok(_) => break,
                        Err(e) if attempt < retries => {
                            attempt += 1;
                            debug!("download of {url} broke off ({e}); retry {attempt}/{retries}");
                            std::thread::sleep(rune_common::net::retry_delay(attempt));
                        }
                        Err(e) => {
                            let _ = fs::remove_file(&tmp_tarball);
                            return Err(e.into());
                        }
                    }
                }
                fs::rename(&tmp_tarball, &tarball_path)?;
                finished.2 = true;
            }
        }

        // Verify checksum if provided
        if let Some(expected) = expected_cksum {
            let computed = compute_sha256(&tarball_path)?;
            if !computed.eq_ignore_ascii_case(expected) {
                let _ = fs::remove_file(&tarball_path);
                return Err(DownloadError::ChecksumMismatch {
                    crate_name: name.to_string(),
                    version: version.to_string(),
                    expected: expected.to_string(),
                    computed,
                });
            }
        }

        // Extract into a private staging directory, then move the finished
        // tree into place in one rename. Extracting straight into `src_dir`
        // let a crash (or a concurrent `rune` process) observe a half-written
        // crate that already had its `Cargo.toml` and treat it as complete.
        let staging = self
            .src_dir
            .join(format!(".{crate_folder_name}.{}.tmp", std::process::id()));
        if staging.exists() {
            fs::remove_dir_all(&staging)?;
        }
        fs::create_dir_all(&staging)?;
        let tar_gz = File::open(&tarball_path)?;
        let tar = GzDecoder::new(BufReader::new(tar_gz));
        let mut archive = Archive::new(tar);
        archive
            .unpack(&staging)
            .map_err(|e| DownloadError::Archive(format!("Extraction failed for {}: {e}", name)))?;

        let staged_crate = staging.join(&crate_folder_name);
        if !staged_crate.join("Cargo.toml").exists() {
            let _ = fs::remove_dir_all(&staging);
            return Err(DownloadError::Archive(format!(
                "{crate_folder_name}.crate does not contain {crate_folder_name}/Cargo.toml"
            )));
        }
        if fs::rename(&staged_crate, &extracted_path).is_err() && !extracted_path.exists() {
            let _ = fs::remove_dir_all(&staging);
            return Err(DownloadError::Archive(format!(
                "could not move {crate_folder_name} into place"
            )));
        }
        // Lost the race to another process: theirs is complete, ours is spare.
        let _ = fs::remove_dir_all(&staging);

        info!(
            "Successfully extracted {} to {}",
            crate_folder_name,
            extracted_path.display()
        );
        Ok(extracted_path)
    }

    /// Downloads and extracts many crates concurrently.
    ///
    /// Cold-cache resolution of a dependency graph is dominated by network
    /// round-trip latency, not bandwidth or CPU; fetching packages one at a
    /// time (as a plain sequential loop would) leaves most of that latency
    /// unhidden. Fanning the requests out across a thread pool lets the
    /// round trips overlap, which is the same trick Cargo's own libcurl
    /// multi-handle download queue relies on.
    ///
    /// Each item is `(name, version, expected_checksum)`. Returns the
    /// extracted directory for each input, in the same order, or the first
    /// error encountered.
    pub fn download_and_extract_many(
        &self,
        packages: &[(String, String, Option<String>)],
    ) -> Result<Vec<PathBuf>> {
        // Already-extracted crates need only a `stat`; keep them out of the
        // (much wider) network pool so a warm build never spins one up.
        let missing: Vec<usize> = packages
            .iter()
            .enumerate()
            .filter(|(_, (name, version, _))| {
                !matches!(self.backend, DlBackend::Directory(_))
                    && !self
                        .src_dir
                        .join(format!("{name}-{version}"))
                        .join("Cargo.toml")
                        .exists()
            })
            .map(|(i, _)| i)
            .collect();

        let mut results: Vec<Option<Result<PathBuf>>> = (0..packages.len()).map(|_| None).collect();
        if !missing.is_empty() {
            // Archives already in the cache only need unpacking.
            let to_fetch = missing
                .iter()
                .filter(|&&i| {
                    let (name, version, _) = &packages[i];
                    !self
                        .crates_dir
                        .join(format!("{name}-{version}.crate"))
                        .is_file()
                })
                .count();
            if to_fetch > 0 && !self.offline {
                rune_common::progress::emit(rune_common::progress::Event::DownloadsQueued {
                    count: to_fetch,
                });
            }
            let run = || -> Vec<(usize, Result<PathBuf>)> {
                missing
                    .par_iter()
                    .map(|&i| {
                        let (name, version, cksum) = &packages[i];
                        (
                            i,
                            self.download_and_extract(name, version, cksum.as_deref()),
                        )
                    })
                    .collect()
            };
            let fetched = match rayon::ThreadPoolBuilder::new()
                .num_threads(DOWNLOAD_CONCURRENCY.min(missing.len()))
                .build()
            {
                Ok(pool) => pool.install(run),
                Err(_) => run(),
            };
            for (i, r) in fetched {
                results[i] = Some(r);
            }
        }

        packages
            .iter()
            .enumerate()
            .map(|(i, (name, version, cksum))| match results[i].take() {
                Some(r) => r,
                None => self.download_and_extract(name, version, cksum.as_deref()),
            })
            .collect()
    }
}

/// Downloads and extracts packages from several registries at once.
///
/// Each item is `(name, version, checksum, registry index)`; the result is
/// in the same order. Packages of one registry are fetched together (in
/// parallel), each registry with its own source replacement, credentials and
/// cache directory.
pub fn download_and_extract_grouped(
    packages: &[(String, String, Option<String>, String)],
    offline: bool,
) -> Result<Vec<PathBuf>> {
    let mut by_registry: std::collections::BTreeMap<&str, Vec<usize>> = Default::default();
    for (i, (_, _, _, index)) in packages.iter().enumerate() {
        by_registry.entry(index.as_str()).or_default().push(i);
    }
    let mut out: Vec<Option<PathBuf>> = vec![None; packages.len()];
    for (index, positions) in by_registry {
        let downloader = CrateDownloader::for_registry(index)
            .map_err(|e| DownloadError::Config(e.to_string()))?
            .offline(offline);
        let batch: Vec<(String, String, Option<String>)> = positions
            .iter()
            .map(|&i| {
                let (n, v, c, _) = &packages[i];
                (n.clone(), v.clone(), c.clone())
            })
            .collect();
        for (dir, &i) in downloader
            .download_and_extract_many(&batch)?
            .into_iter()
            .zip(&positions)
        {
            out[i] = Some(dir);
        }
    }
    Ok(out
        .into_iter()
        .map(|d| d.expect("every package was downloaded"))
        .collect())
}

/// Helper function to download and extract a crate using the default cache location.
pub fn download_and_extract_crate(
    name: &str,
    version: &str,
    expected_cksum: Option<&str>,
) -> Result<PathBuf> {
    CrateDownloader::default_downloader().download_and_extract(name, version, expected_cksum)
}

/// The SHA-256 of a file, hex encoded (what `.cargo-checksum.json` records).
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    compute_sha256(path).map_err(|e| std::io::Error::other(e.to_string()))
}

/// Computes the SHA-256 hash of a file as a hex string.
fn compute_sha256(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut file = File::open(path)?;
    let mut hasher = Sha256Hasher::new();
    let mut buffer = [0u8; 16384];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher.finalize_hex())
}

/// Minimal internal SHA-256 implementation to avoid heavy extra dependencies.
struct Sha256Hasher {
    state: [u32; 8],
    buffer: [u8; 64],
    buffer_len: usize,
    total_len: u64,
}

impl Sha256Hasher {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buffer: [0u8; 64],
            buffer_len: 0,
            total_len: 0,
        }
    }

    fn update(&mut self, data: &[u8]) {
        self.total_len += data.len() as u64;
        let mut offset = 0;
        while offset < data.len() {
            let to_copy = std::cmp::min(data.len() - offset, 64 - self.buffer_len);
            self.buffer[self.buffer_len..self.buffer_len + to_copy]
                .copy_from_slice(&data[offset..offset + to_copy]);
            self.buffer_len += to_copy;
            offset += to_copy;

            if self.buffer_len == 64 {
                self.process_block();
                self.buffer_len = 0;
            }
        }
    }

    fn process_block(&mut self) {
        let mut w = [0u32; 64];
        for (word, bytes) in w.iter_mut().zip(self.buffer.as_chunks::<4>().0) {
            *word = u32::from_be_bytes(*bytes);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;

        for (k, wi) in Self::K.iter().zip(w.iter()) {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(*k)
                .wrapping_add(*wi);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
        self.state[4] = self.state[4].wrapping_add(e);
        self.state[5] = self.state[5].wrapping_add(f);
        self.state[6] = self.state[6].wrapping_add(g);
        self.state[7] = self.state[7].wrapping_add(h);
    }

    fn finalize_hex(mut self) -> String {
        let bit_len = self.total_len * 8;
        self.buffer[self.buffer_len] = 0x80;
        self.buffer_len += 1;

        if self.buffer_len > 56 {
            for i in self.buffer_len..64 {
                self.buffer[i] = 0;
            }
            self.process_block();
            self.buffer_len = 0;
        }

        for i in self.buffer_len..56 {
            self.buffer[i] = 0;
        }

        self.buffer[56..64].copy_from_slice(&bit_len.to_be_bytes());
        self.process_block();

        let mut hex = String::with_capacity(64);
        for val in self.state {
            hex.push_str(&format!("{:08x}", val));
        }
        hex
    }
}
