//! Registry sources: crates.io, alternative registries, mirrors, vendored
//! directories, and local registries.
//!
//! A package's *identity* names the registry it was published to — `foo 1.0.0`
//! from registry A is not `foo 1.0.0` from registry B unless proven equal — and
//! that identity is what the lockfile and cache keys record. Where the bytes
//! are actually *fetched from* is a separate matter, decided by configuration:
//!
//! ```toml
//! [registries.corp]
//! index = "sparse+https://registry.corp.example/index/"
//!
//! [source.crates-io]
//! replace-with = "vendored"          # offline builds, mirrors, vendoring
//! [source.vendored]
//! directory = "vendor"               # or: local-registry = "..", registry = ".."
//! ```
//!
//! [`RegistryDefs`] reads that configuration; [`Backend`] is one concrete way to
//! get an index and packages.

use crate::index::{CrateIndex, IndexDependency, IndexError, IndexRecord, SparseIndexClient};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The canonical identity of crates.io.
pub const CRATES_IO: &str = rune_common::CRATES_IO_INDEX;

/// Everything the configuration says about registries and sources.
#[derive(Debug, Clone, Default)]
pub struct RegistryDefs {
    /// `[registries.NAME].index`.
    names: HashMap<String, String>,
    /// `[registries.NAME].token` (and `[registry].token` as `crates-io`).
    config_tokens: HashMap<String, String>,
    /// `[registries.NAME].credential-provider` (and `[registry]`'s).
    providers: HashMap<String, crate::credential::Provider>,
    /// `registry.global-credential-providers`, highest precedence last.
    global_providers: Vec<crate::credential::Provider>,
    /// `[source.NAME]`.
    sources: HashMap<String, SourceDef>,
}

#[derive(Debug, Clone, Default)]
struct SourceDef {
    replace_with: Option<String>,
    registry: Option<String>,
    directory: Option<PathBuf>,
    local_registry: Option<PathBuf>,
    git: Option<String>,
    /// `branch`/`tag`/`rev` of a git source, as a lockfile query
    /// (`branch=main`).
    git_query: Option<String>,
}

/// A concrete place to read an index and packages from.
#[derive(Debug, Clone)]
pub enum Backend {
    /// A sparse HTTP index (crates.io or an alternative registry).
    Sparse { url: String, token: Option<String> },
    /// `local-registry`: `dir/index/**` and `dir/<name>-<version>.crate`.
    LocalRegistry(PathBuf),
    /// `directory`: unpacked crates side by side (`cargo vendor`).
    Directory(PathBuf),
    /// A git repository holding the index (`registry+https://…`).
    GitIndex { url: String, token: Option<String> },
}

impl RegistryDefs {
    /// Reads the `[registries]`, `[source]` (and credentials) parts of a merged
    /// configuration table.
    pub fn from_config(root: &toml::Table) -> RegistryDefs {
        let mut defs = RegistryDefs::default();
        if let Some(regs) = root.get("registries").and_then(|v| v.as_table()) {
            for (name, v) in regs {
                let Some(t) = v.as_table() else { continue };
                if let Some(index) = t.get("index").and_then(|v| v.as_str()) {
                    defs.names.insert(name.clone(), normalize_index(index));
                }
                if let Some(token) = t.get("token").and_then(|v| v.as_str()) {
                    defs.config_tokens.insert(name.clone(), token.to_string());
                }
                if let Some(p) = t
                    .get("credential-provider")
                    .and_then(crate::credential::Provider::parse)
                {
                    defs.providers.insert(name.clone(), p);
                }
            }
        }
        if let Some(reg) = root.get("registry").and_then(|v| v.as_table()) {
            if let Some(token) = reg.get("token").and_then(|v| v.as_str()) {
                defs.config_tokens
                    .insert("crates-io".to_string(), token.to_string());
            }
            if let Some(p) = reg
                .get("credential-provider")
                .and_then(crate::credential::Provider::parse)
            {
                defs.providers.insert("crates-io".to_string(), p);
            }
            if let Some(list) = reg
                .get("global-credential-providers")
                .and_then(|v| v.as_array())
            {
                defs.global_providers = list
                    .iter()
                    .filter_map(crate::credential::Provider::parse)
                    .collect();
            }
        }
        if let Some(srcs) = root.get("source").and_then(|v| v.as_table()) {
            for (name, v) in srcs {
                let Some(t) = v.as_table() else { continue };
                let s = |k: &str| t.get(k).and_then(|v| v.as_str()).map(str::to_string);
                defs.sources.insert(
                    name.clone(),
                    SourceDef {
                        replace_with: s("replace-with"),
                        registry: s("registry").map(|r| normalize_index(&r)),
                        directory: s("directory").map(PathBuf::from),
                        local_registry: s("local-registry").map(PathBuf::from),
                        git: s("git"),
                        git_query: s("branch")
                            .map(|b| format!("branch={b}"))
                            .or_else(|| s("tag").map(|t| format!("tag={t}")))
                            .or_else(|| s("rev").map(|r| format!("rev={r}"))),
                    },
                );
            }
        }
        defs
    }

    /// The directory a git dependency is read from instead of its repository,
    /// when a `[source."git+…"]` with this URL and reference is replaced by a
    /// `directory` source (what `cargo vendor` / `rune vendor` configure).
    pub fn git_replacement(&self, url: &str, query: Option<&str>) -> Option<PathBuf> {
        let same_url = |a: &str, b: &str| {
            let norm = |u: &str| u.trim_end_matches('/').trim_end_matches(".git").to_string();
            norm(a) == norm(b)
        };
        let (_, def) = self.sources.iter().find(|(_, d)| {
            d.git.as_deref().is_some_and(|g| same_url(g, url)) && d.git_query.as_deref() == query
        })?;
        let mut next = def.replace_with.clone()?;
        for _ in 0..8 {
            let d = self.sources.get(&next)?;
            if let Some(dir) = &d.directory {
                return Some(dir.clone());
            }
            next = d.replace_with.clone()?;
        }
        None
    }

    /// The identity (index URL) of the registry named `name`; `None` is crates.io.
    pub fn index_for_name(&self, name: Option<&str>) -> Result<String, IndexError> {
        match name {
            None | Some("crates-io") => Ok(CRATES_IO.to_string()),
            Some(n) => self.names.get(n).cloned().ok_or_else(|| {
                IndexError::Config(format!(
                    "the registry `{n}` is not defined; add `[registries.{n}] index = \"...\"` \
                     to .cargo/config.toml"
                ))
            }),
        }
    }

    /// The source name configuration uses for a registry identity.
    fn source_name(&self, index: &str) -> String {
        if index == CRATES_IO {
            return "crates-io".to_string();
        }
        self.names
            .iter()
            .find(|(_, v)| v.as_str() == index)
            .map(|(k, _)| k.clone())
            .unwrap_or_else(|| index.to_string())
    }

    /// Where the packages of the registry identified by `index` come from,
    /// after applying `replace-with`.
    pub fn backend_for(&self, index: &str) -> Result<Backend, IndexError> {
        let original = self.source_name(index);
        let mut name = original.clone();
        for _ in 0..8 {
            match self.sources.get(&name).and_then(|s| s.replace_with.clone()) {
                Some(next) => name = next,
                None => break,
            }
        }
        if let Some(def) = self.sources.get(&name) {
            if name != original || def.registry.is_some() {
                if let Some(dir) = &def.directory {
                    return Ok(Backend::Directory(dir.clone()));
                }
                if let Some(dir) = &def.local_registry {
                    return Ok(Backend::LocalRegistry(dir.clone()));
                }
                if let Some(url) = &def.registry {
                    let token = self.token_for(url)?;
                    return Ok(if url.starts_with("registry+") {
                        Backend::GitIndex {
                            url: url.clone(),
                            token,
                        }
                    } else {
                        Backend::Sparse {
                            url: url.clone(),
                            token,
                        }
                    });
                }
                if def.git.is_some() {
                    return Err(IndexError::Config(format!(
                        "source `{name}` is a git source; git-based registries and \
                         replacement by a git source are not supported (use `registry`, \
                         `directory` or `local-registry`)"
                    )));
                }
            } else if let Some(dir) = &def.directory {
                return Ok(Backend::Directory(dir.clone()));
            } else if let Some(dir) = &def.local_registry {
                return Ok(Backend::LocalRegistry(dir.clone()));
            }
        }
        if index.starts_with("registry+") {
            return Ok(Backend::GitIndex {
                url: index.to_string(),
                token: self.token_for(index)?,
            });
        }
        Ok(Backend::Sparse {
            url: index.to_string(),
            token: self.token_for(index)?,
        })
    }

    /// The token for the web API of the registry with this index (publishing,
    /// yanking, owners): for crates.io `CARGO_REGISTRY_TOKEN`, then its
    /// credential providers; for others as for the index.
    pub fn api_token(&self, index: &str) -> Result<Option<String>, IndexError> {
        if index != CRATES_IO {
            return self.token_for(index);
        }
        if let Ok(t) = std::env::var("CARGO_REGISTRY_TOKEN") {
            return Ok(Some(t));
        }
        self.provider_token("crates-io", index)
    }

    /// The name configuration knows the registry with this index by
    /// (`crates-io` for crates.io).
    pub fn name_of(&self, index: &str) -> String {
        self.source_name(index)
    }

    /// The credential providers that apply to a registry, in the order they
    /// are asked: its own, else the global ones (last first), else
    /// `cargo:token`.
    pub fn providers_for(&self, name: &str) -> Vec<crate::credential::Provider> {
        if let Some(p) = self.providers.get(name) {
            return vec![p.clone()];
        }
        if !self.global_providers.is_empty() {
            return self.global_providers.iter().rev().cloned().collect();
        }
        vec![crate::credential::Provider::Token]
    }

    /// The index URL credential providers are given for a registry: crates.io's
    /// canonical one, a git index without its `registry+`.
    pub fn provider_index_url(index: &str) -> String {
        if index == CRATES_IO {
            "https://github.com/rust-lang/crates.io-index".to_string()
        } else {
            index.strip_prefix("registry+").unwrap_or(index).to_string()
        }
    }

    /// The authentication token for the registry with this index, if any:
    /// `CARGO_REGISTRIES_<NAME>_TOKEN`, then its credential providers.
    /// Anonymous registries (and crates.io's index) need none.
    fn token_for(&self, index: &str) -> Result<Option<String>, IndexError> {
        let Some((name, _)) = self.names.iter().find(|(_, v)| v.as_str() == index) else {
            return Ok(None);
        };
        let env = format!(
            "CARGO_REGISTRIES_{}_TOKEN",
            name.chars()
                .map(|c| if c.is_alphanumeric() {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                })
                .collect::<String>()
        );
        if let Ok(t) = std::env::var(env) {
            return Ok(Some(t));
        }
        self.provider_token(name, index)
    }

    /// Asks the registry's credential providers, in order, once per process.
    fn provider_token(&self, name: &str, index: &str) -> Result<Option<String>, IndexError> {
        static CACHE: std::sync::OnceLock<std::sync::Mutex<HashMap<String, Option<String>>>> =
            std::sync::OnceLock::new();
        let cache = CACHE.get_or_init(Default::default);
        // Everything the answer depends on.
        let key = format!(
            "{name}\0{index}\0{:?}\0{:?}",
            self.providers_for(name),
            self.config_tokens.get(name)
        );
        if let Some(hit) = cache.lock().unwrap().get(&key) {
            return Ok(hit.clone());
        }
        let url = Self::provider_index_url(index);
        let registry = crate::credential::Registry {
            name,
            index_url: &url,
        };
        let mut found = None;
        for provider in self.providers_for(name) {
            let token = crate::credential::get(
                &provider,
                &registry,
                self.config_tokens.get(name).map(String::as_str),
                || read_credentials_file(name),
            )
            .map_err(|e| IndexError::Config(format!("registry `{name}`: {e}")))?;
            if token.is_some() {
                found = token;
                break;
            }
        }
        cache.lock().unwrap().insert(key, found.clone());
        Ok(found)
    }
}

/// `sparse+https://x/` and `https://x/` (git index) keep their spelling;
/// only stray whitespace and a missing trailing slash on sparse URLs go.
fn normalize_index(index: &str) -> String {
    let index = index.trim();
    if index.starts_with("sparse+") && !index.ends_with('/') {
        format!("{index}/")
    } else if index.starts_with("sparse+") || index.starts_with("registry+") {
        index.to_string()
    } else {
        // Without `sparse+`, an index URL names a git repository; Cargo
        // spells such a registry `registry+<url>` (in lockfiles too).
        format!("registry+{}", index.trim_end_matches('/'))
    }
}

/// `$CARGO_HOME/credentials.toml`: `[registries.NAME] token = "..."`, or
/// `[registry] token` for crates.io.
fn read_credentials_file(name: &str) -> Option<String> {
    read_credentials(|table| {
        let entry = if name == "crates-io" {
            table.get("registry")
        } else {
            table.get("registries").and_then(|r| r.get(name))
        };
        entry
            .and_then(|r| r.get("token"))
            .and_then(|t| t.as_str())
            .map(str::to_string)
    })
}

/// `$CARGO_HOME` (else `~/.cargo`): where Cargo keeps credentials.
pub fn cargo_home() -> Option<PathBuf> {
    std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| home::home_dir().map(|h| h.join(".cargo")))
}

/// The first answer `pick` finds in `credentials.toml` (or the older
/// `credentials`).
fn read_credentials(pick: impl Fn(&toml::Table) -> Option<String>) -> Option<String> {
    let home = cargo_home()?;
    ["credentials.toml", "credentials"].iter().find_map(|file| {
        let text = std::fs::read_to_string(home.join(file)).ok()?;
        pick(&text.parse::<toml::Table>().ok()?)
    })
}

// ---------------------------------------------------------------------------
// Process-wide configuration
// ---------------------------------------------------------------------------

static DEFS: std::sync::OnceLock<std::sync::Mutex<RegistryDefs>> = std::sync::OnceLock::new();

/// Installs the registry configuration for this process.
pub fn configure(defs: RegistryDefs) {
    let cell = DEFS.get_or_init(Default::default);
    *cell.lock().unwrap() = defs;
}

/// The installed registry configuration.
pub fn defs() -> RegistryDefs {
    DEFS.get_or_init(Default::default).lock().unwrap().clone()
}

// ---------------------------------------------------------------------------
// Index access
// ---------------------------------------------------------------------------

/// Reads the index of one registry, whatever backs it.
pub enum IndexClient {
    Sparse(SparseIndexClient),
    /// A local registry or vendored directory, read straight from disk.
    Disk(DiskIndex),
    /// A clone of a git-based index.
    Git(crate::git_index::GitIndex),
}

impl IndexClient {
    /// A client for the registry identified by `index`.
    pub fn for_registry(index: &str, offline: bool) -> Result<IndexClient, IndexError> {
        Ok(match defs().backend_for(index)? {
            Backend::Sparse { url, token } => {
                IndexClient::Sparse(SparseIndexClient::for_registry(&url, token).offline(offline))
            }
            Backend::LocalRegistry(dir) => IndexClient::Disk(DiskIndex::LocalRegistry(dir)),
            Backend::Directory(dir) => IndexClient::Disk(DiskIndex::Directory(dir)),
            Backend::GitIndex { url, .. } => {
                IndexClient::Git(crate::git_index::GitIndex::open(&url, offline)?)
            }
        })
    }

    pub fn fetch_crate(&self, name: &str) -> Result<CrateIndex, IndexError> {
        match self {
            IndexClient::Sparse(c) => c.fetch_crate(name),
            IndexClient::Disk(d) => d.read(name),
            IndexClient::Git(g) => g.read(name),
        }
    }

    pub fn refresh_crate(&self, name: &str) -> Result<CrateIndex, IndexError> {
        match self {
            IndexClient::Sparse(c) => c.refresh_crate(name),
            IndexClient::Disk(d) => d.read(name),
            IndexClient::Git(g) => g.refresh(name),
        }
    }

    pub fn fetch_many_with(
        &self,
        names: &[String],
        refresh: bool,
    ) -> Vec<Result<CrateIndex, IndexError>> {
        match self {
            IndexClient::Sparse(c) => c.fetch_many_with(names, refresh),
            IndexClient::Disk(d) => names.iter().map(|n| d.read(n)).collect(),
            IndexClient::Git(g) => names
                .iter()
                .map(|n| if refresh { g.refresh(n) } else { g.read(n) })
                .collect(),
        }
    }
}

pub enum DiskIndex {
    LocalRegistry(PathBuf),
    Directory(PathBuf),
}

impl DiskIndex {
    fn read(&self, name: &str) -> Result<CrateIndex, IndexError> {
        match self {
            DiskIndex::LocalRegistry(dir) => {
                let path = dir
                    .join("index")
                    .join(SparseIndexClient::crate_index_path(name));
                let text = std::fs::read_to_string(&path)
                    .map_err(|_| IndexError::CrateNotFound(name.to_string()))?;
                SparseIndexClient::parse_index_text(name, &text)
            }
            DiskIndex::Directory(dir) => directory_index(dir, name),
        }
    }
}

/// Builds the index of a vendored crate from the manifests on disk: every
/// `dir/<name>-<version>/` (or `dir/<name>/`) that declares `name`.
fn directory_index(dir: &Path, name: &str) -> Result<CrateIndex, IndexError> {
    let mut versions = Vec::new();
    for entry in std::fs::read_dir(dir)?.flatten() {
        let path = entry.path();
        let Ok(text) = std::fs::read_to_string(path.join("Cargo.toml")) else {
            continue;
        };
        let Ok(manifest) = text.parse::<toml::Table>() else {
            continue;
        };
        let Some(pkg) = manifest.get("package").and_then(|p| p.as_table()) else {
            continue;
        };
        if pkg.get("name").and_then(|n| n.as_str()) != Some(name) {
            continue;
        }
        let Some(vers) = pkg.get("version").and_then(|v| v.as_str()) else {
            continue;
        };
        // `.cargo-checksum.json` carries the registry checksum of the package.
        let cksum = std::fs::read_to_string(path.join(".cargo-checksum.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| {
                v.get("package")
                    .and_then(|p| p.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_default();

        let mut deps = Vec::new();
        let mut add = |table: Option<&toml::Table>, kind: &str, target: Option<&str>| {
            let Some(table) = table else { return };
            for (alias, spec) in table {
                let (req, features, optional, default_features, package) = match spec {
                    toml::Value::String(s) => (s.clone(), Vec::new(), false, true, None),
                    toml::Value::Table(t) => (
                        t.get("version")
                            .and_then(|v| v.as_str())
                            .unwrap_or("*")
                            .to_string(),
                        t.get("features")
                            .and_then(|f| f.as_array())
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str().map(str::to_string))
                                    .collect()
                            })
                            .unwrap_or_default(),
                        t.get("optional").and_then(|v| v.as_bool()).unwrap_or(false),
                        t.get("default-features")
                            .or_else(|| t.get("default_features"))
                            .and_then(|v| v.as_bool())
                            .unwrap_or(true),
                        t.get("package")
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                    ),
                    _ => continue,
                };
                deps.push(IndexDependency {
                    name: alias.clone(),
                    req,
                    features,
                    optional,
                    default_features,
                    target: target.map(str::to_string),
                    kind: Some(kind.to_string()),
                    package,
                    registry: None,
                });
            }
        };
        let tbl = |k: &str| manifest.get(k).and_then(|v| v.as_table());
        add(tbl("dependencies"), "normal", None);
        add(tbl("build-dependencies"), "build", None);
        add(tbl("dev-dependencies"), "dev", None);
        if let Some(targets) = tbl("target") {
            for (t, v) in targets {
                let Some(v) = v.as_table() else { continue };
                add(
                    v.get("dependencies").and_then(|d| d.as_table()),
                    "normal",
                    Some(t),
                );
                add(
                    v.get("build-dependencies").and_then(|d| d.as_table()),
                    "build",
                    Some(t),
                );
                add(
                    v.get("dev-dependencies").and_then(|d| d.as_table()),
                    "dev",
                    Some(t),
                );
            }
        }
        let features = tbl("features")
            .map(|f| {
                f.iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            v.as_array()
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|x| x.as_str().map(str::to_string))
                                        .collect()
                                })
                                .unwrap_or_default(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        versions.push(IndexRecord {
            name: name.to_string(),
            vers: vers.to_string(),
            deps,
            cksum,
            features,
            features2: None,
            yanked: false,
            rust_version: pkg
                .get("rust-version")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        });
    }
    if versions.is_empty() {
        return Err(IndexError::CrateNotFound(name.to_string()));
    }
    versions.sort_by(|a, b| a.vers.cmp(&b.vers));
    Ok(CrateIndex {
        name: name.to_string(),
        versions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defs(toml: &str) -> RegistryDefs {
        RegistryDefs::from_config(&toml.parse::<toml::Table>().unwrap())
    }

    #[test]
    fn named_registries_resolve_to_their_index() {
        let d = defs("[registries.corp]\nindex = \"sparse+https://r.example/index\"\n");
        assert_eq!(
            d.index_for_name(Some("corp")).unwrap(),
            "sparse+https://r.example/index/"
        );
        assert_eq!(d.index_for_name(None).unwrap(), CRATES_IO);
        assert!(d.index_for_name(Some("nope")).is_err());
    }

    #[test]
    fn crates_io_is_replaced_by_a_vendor_directory_but_keeps_its_identity() {
        let d = defs(
            "[source.crates-io]\nreplace-with = \"vendored\"\n[source.vendored]\ndirectory = \"/v\"\n",
        );
        match d.backend_for(CRATES_IO).unwrap() {
            Backend::Directory(p) => assert_eq!(p, PathBuf::from("/v")),
            other => panic!("{other:?}"),
        }
        // Unreplaced registries are untouched.
        let d2 = defs("[registries.a]\nindex = \"sparse+https://a/\"\n");
        assert!(matches!(
            d2.backend_for("sparse+https://a/").unwrap(),
            Backend::Sparse { .. }
        ));
    }

    #[test]
    fn replacement_chains_and_local_registries() {
        let d = defs(
            "[source.crates-io]\nreplace-with = \"mirror\"\n[source.mirror]\nreplace-with = \"local\"\n[source.local]\nlocal-registry = \"/lr\"\n",
        );
        assert!(matches!(
            d.backend_for(CRATES_IO).unwrap(),
            Backend::LocalRegistry(p) if p == Path::new("/lr")
        ));
    }

    #[test]
    fn tokens_come_from_config_env_or_a_credential_command() {
        let d = defs(
            "[registries.a]\nindex = \"sparse+https://a/\"\ntoken = \"cfg-token\"\n\
             [registries.b]\nindex = \"sparse+https://b/\"\ncredential-provider = [\"cargo:token-from-stdout\", \"echo\", \"from-command\"]\n\
             [registries.c]\nindex = \"sparse+https://c/\"\n",
        );
        let token = |index: &str| match d.backend_for(index).unwrap() {
            Backend::Sparse { token, .. } => token,
            other => panic!("{other:?}"),
        };
        assert_eq!(token("sparse+https://a/").as_deref(), Some("cfg-token"));
        assert_eq!(token("sparse+https://b/").as_deref(), Some("from-command"));
        assert_eq!(token("sparse+https://c/"), None);
    }

    #[test]
    fn git_indexes_are_read_from_a_clone_and_named_like_cargo() {
        let d = defs("[registries.corp]\nindex = \"https://git.example.com/index\"\n");
        // A URL without `sparse+` is a git index, identified as Cargo writes
        // it in lockfiles.
        let index = d.index_for_name(Some("corp")).unwrap();
        assert_eq!(index, "registry+https://git.example.com/index");
        assert!(matches!(
            d.backend_for(&index).unwrap(),
            Backend::GitIndex { url, .. } if url == index
        ));
    }

    #[test]
    fn directory_sources_synthesize_an_index_from_manifests() {
        let dir = tempfile::tempdir().unwrap();
        let krate = dir.path().join("foo-1.2.3");
        std::fs::create_dir_all(&krate).unwrap();
        std::fs::write(
            krate.join("Cargo.toml"),
            "[package]\nname = \"foo\"\nversion = \"1.2.3\"\n[dependencies]\nbar = { version = \"1\", optional = true }\n[features]\nx = [\"dep:bar\"]\n[target.'cfg(unix)'.dependencies]\nlibc = \"0.2\"\n",
        )
        .unwrap();
        std::fs::write(
            krate.join(".cargo-checksum.json"),
            r#"{"package":"abc123","files":{}}"#,
        )
        .unwrap();
        let idx = directory_index(dir.path(), "foo").unwrap();
        let rec = &idx.versions[0];
        assert_eq!(rec.vers, "1.2.3");
        assert_eq!(rec.cksum, "abc123");
        assert!(rec.deps.iter().any(|d| d.name == "bar" && d.optional));
        assert!(
            rec.deps
                .iter()
                .any(|d| d.name == "libc" && d.target.as_deref() == Some("cfg(unix)"))
        );
        assert_eq!(rec.features["x"], ["dep:bar"]);
        assert!(directory_index(dir.path(), "missing").is_err());
    }
}
