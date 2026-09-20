use cfg_expr::expr::TargetMatcher;
use rune_manifest::dependency::{DependencyKind, DependencySpec};
use rune_registry::source::{CRATES_IO, IndexClient};
use rune_registry::{CrateIndex, IndexRecord};
use semver::{Version, VersionReq};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use thiserror::Error;

use crate::features::DynamicFeatureResolver;
use crate::lockfile::{LockDecl, LockPackage, Lockfile};

#[derive(Debug, Error)]
pub enum ResolverError {
    #[error("Failed to find compatible version for `{name}` matching `{req}`")]
    NoMatchingVersion { name: String, req: String },
    #[error(transparent)]
    Registry(#[from] rune_registry::index::IndexError),
    #[error("Manifest parse error: {0}")]
    Manifest(String),
    #[error("Git dependency error: {0}")]
    Git(#[from] rune_registry::git::GitError),
}

pub type Result<T> = std::result::Result<T, ResolverError>;

/// One dependency waiting to be resolved: package name, requirement,
/// requested features, default-features, a local directory when it is not a
/// registry package, and the identity (index URL) of the registry it comes from.
type QueueItem = (
    String,
    VersionReq,
    Vec<String>,
    bool,
    Option<PathBuf>,
    String,
);

/// A fully resolved package in the dependency graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPackage {
    pub name: String,
    pub version: Version,
    pub source: PackageSource,
    pub checksum: Option<String>,
    pub dependencies: Vec<ResolvedDependencyRef>,
    pub features: Vec<String>,
    /// The package's `[features]` table (registry packages; local ones are
    /// read from their manifests).
    pub declared_features: HashMap<String, Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageSource {
    /// A registry package; the string is the canonical index URL.
    Registry(String),
    Path(PathBuf),
    /// A git package pinned to an exact commit.
    Git {
        url: String,
        /// `branch=..`, `tag=..` or `rev=..`, as written in the manifest.
        reference: Option<String>,
        commit: String,
    },
}

impl PackageSource {
    /// crates.io.
    pub fn crates_io() -> Self {
        Self::Registry(rune_common::CRATES_IO_INDEX.to_string())
    }

    pub fn is_registry(&self) -> bool {
        matches!(self, Self::Registry(_))
    }

    /// The `source` string of a lockfile entry (`None` for local packages).
    pub fn lock_source(&self) -> Option<String> {
        match self {
            Self::Registry(index) => Some(if index == rune_common::CRATES_IO_INDEX {
                // Cargo's canonical spelling of crates.io, whatever protocol
                // is used to talk to it.
                "registry+https://github.com/rust-lang/crates.io-index".to_string()
            } else {
                index.clone()
            }),
            Self::Path(_) => None,
            Self::Git {
                url,
                reference,
                commit,
            } => Some(match reference {
                Some(r) => format!("git+{url}?{r}#{commit}"),
                None => format!("git+{url}#{commit}"),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedDependencyRef {
    pub name: String,
    pub req: String,
    pub kind: DependencyKind,
    pub optional: bool,
    pub features: Vec<String>,
    pub default_features: bool,
    pub package: Option<String>,
    /// `cfg(unix)` / a triple, if the dependency is platform-gated.
    pub target: Option<String>,
    /// The registry it is declared to come from, when not the depending
    /// package's own (`registry = "..."`, or the index's per-dependency field).
    pub registry: Option<String>,
}

/// Result of complete workspace & registry dependency resolution.
#[derive(Debug, Clone, Default)]
pub struct ResolvedGraph {
    pub root_packages: Vec<String>,
    pub packages: HashMap<String, ResolvedPackage>,
}

impl ResolvedGraph {
    /// Converts the resolved graph into a lockfile.
    pub fn to_lockfile(&self) -> Lockfile {
        let mut lockfile = Lockfile::new();

        for pkg in self.packages.values() {
            let source_str = pkg.source.lock_source();

            let mut deps = Vec::new();
            let mut decls = Vec::new();
            for dep in &pkg.dependencies {
                // `dep.name` is the local extern alias (e.g. `webpki-roots`
                // depends on `rustls-pki-types` under the name `pki-types`);
                // matching against it directly only works for dependencies
                // that aren't renamed. Search by the real package name when
                // `package = "..."` is set, or the lockfile silently loses
                // the edge for every renamed dependency.
                let actual_name = dep.package.as_deref().unwrap_or(&dep.name);
                let registry = dep.registry.clone().unwrap_or_else(|| match &pkg.source {
                    PackageSource::Registry(r) => r.clone(),
                    _ => CRATES_IO.to_string(),
                });
                let target = self.find_matching_in(actual_name, &dep.req, &registry);
                if let Some(target) = target {
                    deps.push(format!("{} {}", target.name, target.version));
                }
                if pkg.source.is_registry() {
                    decls.push(LockDecl {
                        alias: dep.name.clone(),
                        package: dep.package.clone(),
                        kind: match dep.kind {
                            DependencyKind::Normal => 0,
                            DependencyKind::Dev => 1,
                            DependencyKind::Build => 2,
                        },
                        target: dep.target.clone(),
                        optional: dep.optional,
                        features: dep.features.clone(),
                        default_features: dep.default_features,
                        resolved: target.map(|t| format!("{} {}", t.name, t.version)),
                    });
                }
            }
            deps.sort();
            deps.dedup();
            let mut feature_table: Vec<(String, Vec<String>)> = pkg
                .declared_features
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            feature_table.sort();

            lockfile.add_or_replace(LockPackage {
                name: pkg.name.clone(),
                version: pkg.version.to_string(),
                source: source_str,
                checksum: pkg.checksum.clone(),
                dependencies: deps,
                features: pkg.features.clone(),
                decls,
                feature_table,
            });
        }

        lockfile
    }

    /// Like [`Self::find_matching`], but a registry package must come from
    /// `registry` (patched local and git packages match regardless: they stand
    /// in for whatever registry the patch replaces).
    pub fn find_matching_in(
        &self,
        name: &str,
        req_str: &str,
        registry: &str,
    ) -> Option<&ResolvedPackage> {
        let req =
            VersionReq::from_str(req_str).unwrap_or_else(|_| VersionReq::from_str("*").unwrap());
        self.packages
            .values()
            .filter(|p| p.name == name && req.matches(&p.version))
            .filter(|p| match &p.source {
                PackageSource::Registry(r) => r == registry,
                _ => true,
            })
            .max_by(|a, b| a.version.cmp(&b.version))
    }

    /// Finds a resolved package matching name and version requirement.
    ///
    /// When several resolved versions satisfy the requirement (two dependents
    /// asking `^1` and `>=1.2`, say) the *highest* wins, so the answer never
    /// depends on `HashMap` iteration order.
    pub fn find_matching(&self, name: &str, req_str: &str) -> Option<&ResolvedPackage> {
        let req =
            VersionReq::from_str(req_str).unwrap_or_else(|_| VersionReq::from_str("*").unwrap());
        self.packages
            .values()
            .filter(|p| p.name == name && req.matches(&p.version))
            .max_by(|a, b| a.version.cmp(&b.version))
    }
}

/// The core dependency resolver.
pub struct DependencyResolver {
    /// One index client per registry (by identity), created on first use.
    clients: HashMap<String, IndexClient>,
    /// `(registry, crate name)` → its published versions.
    cached_indices: HashMap<(String, String), CrateIndex>,
    /// Prefer versions whose `rust-version` this compiler satisfies.
    msrv: Option<Version>,
    /// When true, always re-query the registry index over the network
    /// instead of trusting the local cache. Used by `rune update`/`rune add`,
    /// which need to discover newly published versions; left off for
    /// `rune build`/`rune check`, where the local cache is always sufficient
    /// once a package has been resolved once.
    refresh: bool,
    /// Never touch the network.
    offline: bool,
    /// `[patch]` / `[replace]` substitutions.
    patches: crate::patch::Patches,
}

impl Default for DependencyResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl DependencyResolver {
    /// Creates a resolver that trusts the local index cache.
    pub fn new() -> Self {
        Self {
            clients: HashMap::new(),
            cached_indices: HashMap::new(),
            msrv: None,
            refresh: false,
            offline: false,
            patches: Default::default(),
        }
    }

    /// Applies `[patch]` and `[replace]` substitutions.
    pub fn with_patches(mut self, patches: crate::patch::Patches) -> Self {
        self.patches = patches;
        self
    }

    /// Chooses, among the versions a requirement allows, the newest whose
    /// `rust-version` fits `msrv` — falling back to the newest overall when
    /// none does (resolver 3's `incompatible-rust-versions = "fallback"`).
    pub fn with_msrv(mut self, msrv: Option<Version>) -> Self {
        self.msrv = msrv;
        self
    }

    /// Creates a default resolver that trusts the local index cache
    /// (no network access unless a package has never been seen before).
    pub fn default_resolver() -> Self {
        Self::new()
    }

    /// Creates a resolver that always re-queries the registry index over
    /// the network, used for `rune update`/`rune add` where discovering
    /// newly published versions is the point of the operation.
    pub fn refreshing_resolver() -> Self {
        let mut resolver = Self::new();
        resolver.refresh = true;
        resolver
    }

    /// Makes the resolver use only the local index cache.
    pub fn offline(mut self, offline: bool) -> Self {
        self.offline = offline;
        self.clients.clear();
        self
    }

    fn client(&mut self, registry: &str) -> Result<&IndexClient> {
        if !self.clients.contains_key(registry) {
            let client = IndexClient::for_registry(registry, self.offline)?;
            self.clients.insert(registry.to_string(), client);
        }
        Ok(&self.clients[registry])
    }

    /// Fetches the index of every not-yet-loaded registry crate in `wanted`
    /// (`(registry, name)`) concurrently, so the sequential resolution walk
    /// below hits memory.
    fn prefetch<'a>(&mut self, wanted: impl Iterator<Item = (&'a str, &'a str)>) {
        let mut by_registry: HashMap<String, Vec<String>> = HashMap::new();
        for (registry, name) in wanted {
            if !self
                .cached_indices
                .contains_key(&(registry.to_string(), name.to_string()))
            {
                by_registry
                    .entry(registry.to_string())
                    .or_default()
                    .push(name.to_string());
            }
        }
        if by_registry.is_empty() {
            return;
        }
        let _timer = rune_common::timing::phase("index prefetch");
        let refresh = self.refresh;
        for (registry, mut names) in by_registry {
            names.sort();
            names.dedup();
            let Ok(client) = self.client(&registry) else {
                continue; // reported when an item actually needs this registry
            };
            let results = client.fetch_many_with(&names, refresh);
            for (name, result) in names.into_iter().zip(results) {
                // Errors are deliberately not recorded: the item that actually
                // needs this crate re-fetches it and reports the real error.
                if let Ok(index) = result {
                    self.cached_indices.insert((registry.clone(), name), index);
                }
            }
        }
    }

    /// Resolves dependencies for a set of root manifests and optional existing lockfile.
    pub fn resolve(
        &mut self,
        project_root: &Path,
        root_dependencies: &[(String, DependencySpec)],
        existing_lock: Option<&Lockfile>,
    ) -> Result<ResolvedGraph> {
        self.resolve_with_workspace(project_root, root_dependencies, existing_lock, None)
    }

    /// Same as [`Self::resolve`], but also applies `[workspace.dependencies]`
    /// inheritance (`dep.workspace = true`) to every local `path`
    /// dependency's own manifest as it's reached during the transitive walk
    /// — not just to `root_dependencies`, which the caller has typically
    /// already merged itself. Without this, a workspace member reached only
    /// transitively (e.g. crate A depends on crate B, and only B declares
    /// `some-dep.workspace = true`) has that inheritance silently reverted
    /// back to `some-dep`'s unmerged defaults the moment the resolver reads
    /// B's manifest directly, even though the caller merged correctly for
    /// its own root-level dependencies.
    pub fn resolve_with_workspace(
        &mut self,
        project_root: &Path,
        root_dependencies: &[(String, DependencySpec)],
        existing_lock: Option<&Lockfile>,
        workspace: Option<&rune_manifest::workspace::WorkspaceManifest>,
    ) -> Result<ResolvedGraph> {
        // Target-gated dependencies (`[target.'cfg(...)'.dependencies]`) are
        // evaluated against the host triple. Cross-compilation with
        // `--target` doesn't yet feed a different triple through here (a
        // pre-existing gap, not introduced by this), so a cross build may
        // still pull host-only dependency edges; same-target (the common
        // case, including this project's own self-hosted build) resolves
        // correctly.
        // Like `Cargo.lock`, the resolution covers *every* platform: which
        // target-gated dependencies a build actually compiles is decided when
        // the build is planned, against the platform it is built for.

        let mut resolved_packages: HashMap<String, ResolvedPackage> = HashMap::new();
        let mut feature_resolver = DynamicFeatureResolver::new();
        let mut queue: VecDeque<QueueItem> = VecDeque::new();
        let mut visited: HashSet<(String, String)> = HashSet::new();

        let mut root_names = Vec::new();

        // 1. Enqueue all root dependencies
        for (name, spec) in root_dependencies {
            root_names.push(name.clone());
            let req = spec
                .version
                .clone()
                .unwrap_or_else(|| VersionReq::from_str("*").unwrap());
            let local_dir = spec.path.as_ref().map(|p| project_root.join(p));
            let registry = rune_registry::source::defs()
                .index_for_name(spec.registry.as_deref())
                .map_err(ResolverError::Registry)?;
            queue.push_back((
                name.clone(),
                req,
                spec.features.clone(),
                spec.default_features,
                local_dir,
                registry,
            ));
        }

        // 2. BFS Transitive Resolution Loop
        //
        // Draining the queue and discovering newly-activated optional
        // dependencies are two distinct passes, run in alternation rather
        // than interleaved per-item. `DynamicFeatureResolver::resolve()`
        // re-runs a fixed-point computation over *every* package registered
        // so far, and the scan below it walks *every* resolved package's
        // *every* dependency — both O(graph size so far), not O(1). The
        // previous version called this after inserting each single new
        // package, so total resolver work scaled with (dependency edges) ×
        // (graph size) — quadratic, and the dominant cost of resolving any
        // graph with a few hundred packages. Running it once per BFS round
        // (whenever the queue would otherwise be empty) instead of once per
        // edge cuts that to (BFS depth) × (graph size), since a round only
        // ends when there's genuinely nothing left to discover without it.
        loop {
            // Process the queue one generation at a time: everything queued
            // so far is known before any of it is resolved, so all the index
            // lookups it needs can be issued concurrently up front.
            while !queue.is_empty() {
                let generation: Vec<_> = queue.drain(..).collect();
                self.prefetch(
                    generation
                        .iter()
                        .filter(|(_, _, _, _, local_dir, _)| local_dir.is_none())
                        .map(|(name, _, _, _, _, registry)| (registry.as_str(), name.as_str())),
                );
                for (name, req, features, default_features, local_dir, registry) in generation {
                    resolve_one_queue_item(
                        name,
                        req,
                        features,
                        default_features,
                        local_dir,
                        registry,
                        &mut resolved_packages,
                        &mut feature_resolver,
                        &mut queue,
                        &mut visited,
                        project_root,
                        workspace,
                        existing_lock,
                        self,
                    )?;
                }
            }

            let activation_timer = rune_common::timing::phase("optional-dep activation");
            let more = activate_newly_enabled_optional_deps(
                &resolved_packages,
                &mut feature_resolver,
                &mut queue,
            );
            drop(activation_timer);
            if !more {
                break;
            }
        }

        // 3. Unify features across resolved packages
        let _timer = rune_common::timing::phase("feature unification");
        let feature_results = feature_resolver.resolve();
        for (pkg_id, resolved_pkg) in &mut resolved_packages {
            if let Some(feats) = feature_results.get(pkg_id) {
                resolved_pkg.features = feats.active_features.clone();
            }
        }

        Ok(ResolvedGraph {
            root_packages: root_names,
            packages: resolved_packages,
        })
    }

    fn get_or_fetch_index(&mut self, registry: &str, name: &str) -> Result<&CrateIndex> {
        let key = (registry.to_string(), name.to_string());
        if !self.cached_indices.contains_key(&key) {
            let refresh = self.refresh;
            let client = self.client(registry)?;
            let index = if refresh {
                client.refresh_crate(name)?
            } else {
                client.fetch_crate(name)?
            };
            self.cached_indices.insert(key.clone(), index);
        }
        Ok(&self.cached_indices[&key])
    }
}

#[allow(clippy::too_many_arguments)]
fn resolve_one_queue_item(
    name: String,
    req: VersionReq,
    features: Vec<String>,
    default_features: bool,
    local_dir: Option<PathBuf>,
    registry: String,
    resolved_packages: &mut HashMap<String, ResolvedPackage>,
    feature_resolver: &mut DynamicFeatureResolver,
    queue: &mut VecDeque<QueueItem>,
    visited: &mut HashSet<(String, String)>,
    project_root: &Path,
    workspace: Option<&rune_manifest::workspace::WorkspaceManifest>,
    existing_lock: Option<&Lockfile>,
    resolver: &mut DependencyResolver,
) -> Result<()> {
    // If local path dependency
    if let Some(dir) = local_dir {
        let manifest_path = rune_common::manifest_file::manifest_in(&dir);
        if let Ok(manifest) = rune_manifest::Manifest::from_path(&manifest_path) {
            // A package inherits from *its own* workspace — the one it is a
            // member of — which for a path dependency next door or a git
            // repository is not the project being built.
            let canon = dir.canonicalize().unwrap_or_else(|_| dir.clone());
            let enclosing = rune_manifest::find_enclosing_workspace(&canon, &manifest);
            let ws_ref = enclosing.as_ref().map(|(_, w)| w).or(workspace);
            let ws_root: PathBuf = enclosing
                .as_ref()
                .map(|(r, _)| r.clone())
                .unwrap_or_else(|| project_root.to_path_buf());
            if let Some(pkg) = manifest.package {
                let version: Version = pkg
                    .version_in(ws_ref)
                    .unwrap_or_else(|| Version::new(0, 0, 0));

                let pkg_id = format!("{} {}", name, version);
                feature_resolver.register_package_features(&pkg_id, manifest.features);
                let aliases: HashMap<String, String> = manifest
                    .dependencies
                    .values()
                    .filter_map(|dep| {
                        dep.spec
                            .package
                            .clone()
                            .map(|real| (dep.name.clone(), real))
                    })
                    .collect();
                feature_resolver.register_package_aliases(&pkg_id, aliases);
                feature_resolver.request_features(&pkg_id, &features, default_features);

                let key = (name.clone(), version.to_string());
                if visited.contains(&key) {
                    return Ok(());
                }
                visited.insert(key);

                let mut dep_refs = Vec::new();
                // Sorted: `HashMap` order is randomized per process and would
                // otherwise make the order dependencies are resolved in (and
                // so the lockfile's contents on ties) vary run to run.
                let mut member_deps: Vec<_> = manifest.dependencies.values().collect();
                member_deps.sort_by(|a, b| (&a.name, a.kind as u8).cmp(&(&b.name, b.kind as u8)));
                for dep_spec in member_deps {
                    let spec = rune_manifest::effective_spec(dep_spec, &canon, &ws_root, ws_ref);
                    let dep_req = spec
                        .version
                        .clone()
                        .unwrap_or_else(|| VersionReq::from_str("*").unwrap());

                    let actual_pkg_name = spec
                        .package
                        .clone()
                        .unwrap_or_else(|| dep_spec.name.clone());
                    // Where the dependency lives on disk, if not in a registry.
                    let child_registry = rune_registry::source::defs()
                        .index_for_name(spec.registry.as_deref())
                        .map_err(ResolverError::Registry)?;
                    let child_local_dir = if let Some(p) = &spec.path {
                        Some(p.clone())
                    } else if let (Some(url), false) = (&spec.git, spec.optional) {
                        Some(git_package_dir(&spec, url, &actual_pkg_name)?)
                    } else {
                        None
                    };
                    dep_refs.push(ResolvedDependencyRef {
                        name: dep_spec.name.clone(),
                        req: dep_req.to_string(),
                        kind: dep_spec.kind,
                        optional: spec.optional,
                        features: spec.features.clone(),
                        default_features: spec.default_features,
                        package: spec.package.clone(),
                        target: dep_spec.target.clone(),
                        registry: spec.registry.as_ref().map(|_| child_registry.clone()),
                    });

                    // Development dependencies appear in the lockfile but
                    // are resolved as roots by the caller, never followed
                    // transitively.
                    if !spec.optional && dep_spec.kind != DependencyKind::Dev {
                        queue.push_back((
                            actual_pkg_name,
                            dep_req,
                            spec.features.clone(),
                            spec.default_features,
                            child_local_dir,
                            child_registry,
                        ));
                    }
                }

                let source = match rune_registry::git::locate_dir(&canon)
                    .or_else(|| rune_registry::git::locate_dir(&dir))
                {
                    Some(co) => PackageSource::Git {
                        url: co.url.clone(),
                        reference: co.reference.lock_query(),
                        commit: co.commit.clone(),
                    },
                    None => PackageSource::Path(dir),
                };
                resolved_packages.insert(
                    pkg_id,
                    ResolvedPackage {
                        name,
                        version,
                        source,
                        checksum: None,
                        dependencies: dep_refs,
                        features: Vec::new(),
                        declared_features: HashMap::new(),
                    },
                );
                return Ok(());
            }
        }
    }

    // A patch replaces the registry package outright: resolve the patch's
    // own package (and its dependencies) instead.
    let patch_source = if registry == CRATES_IO {
        crate::patch::PatchSource::CratesIo
    } else {
        crate::patch::PatchSource::Registry(registry.clone())
    };
    for entry in resolver.patches.find(&name, &patch_source) {
        match entry.locate(&req) {
            Ok(Some((dir, _))) => {
                return resolve_one_queue_item(
                    name,
                    req,
                    features,
                    default_features,
                    Some(dir),
                    registry.clone(),
                    resolved_packages,
                    feature_resolver,
                    queue,
                    visited,
                    project_root,
                    workspace,
                    existing_lock,
                    resolver,
                );
            }
            Ok(None) => {}
            Err(e) => return Err(ResolverError::Manifest(e)),
        }
    }

    // If already resolved with a version that matches req, reuse and request features
    if let Some((pkg_id, _existing)) = resolved_packages.iter().find(|(_, p)| {
        p.name == name
            && req.matches(&p.version)
            && match &p.source {
                PackageSource::Registry(r) => *r == registry,
                _ => true,
            }
    }) {
        feature_resolver.request_features(pkg_id, &features, default_features);
        return Ok(());
    }

    // Check if existing lockfile has a pinned version that satisfies the
    // requirement. A lockfile can pin several versions of one crate (e.g.
    // `getrandom` 0.2 and 0.3 for different dependents), so pick the highest
    // pinned version *that matches this requirement* rather than the first
    // entry with the right name.
    let mut chosen_record = None;
    if let Some(lock) = existing_lock {
        let wanted_source = PackageSource::Registry(registry.clone()).lock_source();
        let pinned = lock
            .packages
            .iter()
            .filter(|p| p.name == name && p.source == wanted_source)
            .filter_map(|p| Version::parse(&p.version).ok())
            .filter(|v| req.matches(v))
            .max();
        if let Some(ver) = pinned {
            if let Ok(index) = resolver.get_or_fetch_index(&registry, &name) {
                if let Some(rec) = index.get_version(&ver.to_string()) {
                    chosen_record = Some(rec.clone());
                }
            }
        }
    }

    // If not found in lockfile, query index for latest matching version
    if chosen_record.is_none() {
        let msrv = resolver.msrv.clone();
        let index = resolver.get_or_fetch_index(&registry, &name)?;
        let mut matches: Vec<&IndexRecord> = index
            .available_versions()
            .into_iter()
            .filter(|rec| {
                if let Ok(ver) = Version::parse(&rec.vers) {
                    req.matches(&ver)
                } else {
                    false
                }
            })
            .collect();

        // Sort descending to choose the latest version
        matches.sort_by(|a, b| {
            let va = Version::parse(&a.vers).unwrap_or_else(|_| Version::new(0, 0, 0));
            let vb = Version::parse(&b.vers).unwrap_or_else(|_| Version::new(0, 0, 0));
            vb.cmp(&va)
        });

        // Prefer the newest release this compiler can build, when asked to.
        let compatible = msrv.as_ref().and_then(|m| {
            matches.iter().copied().find(|rec| {
                rec.rust_version
                    .as_deref()
                    .and_then(rune_common::toolchain::parse_rust_version)
                    .is_none_or(|need| need <= *m)
            })
        });
        if let Some(best) = compatible.or_else(|| matches.first().copied()) {
            chosen_record = Some(best.clone());
        } else if !resolver.refresh && !resolver.offline {
            // The cached index may simply predate the release this manifest
            // asks for (e.g. `serde = "1.0.999"` typed by hand). Cargo would
            // have queried the registry; do the same once before giving up.
            resolver
                .cached_indices
                .remove(&(registry.clone(), name.clone()));
            resolver.refresh = true;
            let retry = resolver.get_or_fetch_index(&registry, &name).map(|i| {
                i.available_versions()
                    .into_iter()
                    .filter(|rec| Version::parse(&rec.vers).is_ok_and(|v| req.matches(&v)))
                    .max_by_key(|rec| Version::parse(&rec.vers).ok())
                    .cloned()
            });
            resolver.refresh = false;
            match retry {
                Ok(Some(rec)) => chosen_record = Some(rec),
                _ => {
                    return Err(ResolverError::NoMatchingVersion {
                        name,
                        req: req.to_string(),
                    });
                }
            }
        } else {
            return Err(ResolverError::NoMatchingVersion {
                name,
                req: req.to_string(),
            });
        }
    }

    let record = chosen_record.unwrap();
    let parsed_version =
        Version::parse(&record.vers).map_err(|e| ResolverError::Manifest(e.to_string()))?;

    let pkg_id = format!("{} {}", name, parsed_version);
    feature_resolver.register_package_features(&pkg_id, record.all_features());
    let aliases: HashMap<String, String> = record
        .deps
        .iter()
        .filter_map(|dep| {
            dep.package
                .as_ref()
                .map(|real| (dep.name.clone(), real.clone()))
        })
        .collect();
    feature_resolver.register_package_aliases(&pkg_id, aliases);
    feature_resolver.request_features(&pkg_id, &features, default_features);

    let key = (name.clone(), record.vers.clone());
    if visited.contains(&key) {
        return Ok(());
    }
    visited.insert(key);

    // Collect direct dependencies
    let mut dep_refs = Vec::new();
    for dep in &record.deps {
        // Skip dev-dependencies for transitive resolution
        if dep.kind.as_deref() == Some("dev") {
            continue;
        }

        let dep_req =
            VersionReq::from_str(&dep.req).unwrap_or_else(|_| VersionReq::from_str("*").unwrap());

        let actual_pkg_name = dep.package.clone().unwrap_or_else(|| dep.name.clone());
        let dep_ref = ResolvedDependencyRef {
            name: dep.name.clone(),
            req: dep.req.clone(),
            kind: match dep.kind.as_deref() {
                Some("build") => DependencyKind::Build,
                _ => DependencyKind::Normal,
            },
            optional: dep.optional,
            features: dep.features.clone(),
            default_features: dep.default_features,
            package: dep.package.clone(),
            target: dep.target.clone(),
            registry: dep.registry.as_deref().map(normalize_registry),
        };
        // A dependency comes from the depending package's own registry unless
        // the index says otherwise (`registry` names the other one's index).
        let dep_registry = dep_ref.registry.clone().unwrap_or_else(|| registry.clone());
        dep_refs.push(dep_ref);

        if !dep.optional {
            queue.push_back((
                actual_pkg_name,
                dep_req,
                dep.features.clone(),
                dep.default_features,
                None,
                dep_registry,
            ));
        }
    }

    resolved_packages.insert(
        pkg_id,
        ResolvedPackage {
            name,
            version: parsed_version,
            source: PackageSource::Registry(registry.clone()),
            checksum: Some(record.cksum.clone()),
            dependencies: dep_refs,
            features: Vec::new(),
            declared_features: record.all_features(),
        },
    );

    Ok(())
}

/// An index URL from a registry index's `registry` field, in the identity
/// form used everywhere else: crates.io's git index maps to crates.io.
fn normalize_registry(url: &str) -> String {
    if url.trim_end_matches('/') == "https://github.com/rust-lang/crates.io-index"
        || url.trim_end_matches('/') == "https://index.crates.io"
    {
        CRATES_IO.to_string()
    } else if url.starts_with("sparse+") || url.starts_with("registry+") {
        url.to_string()
    } else {
        format!("sparse+{}", url.trim_end_matches('/').to_string() + "/")
    }
}

/// Checks a git dependency out (at the commit the lockfile pinned, or the one
/// its branch/tag/revision points to now) and finds the named package in it.
fn git_package_dir(spec: &DependencySpec, url: &str, package: &str) -> Result<PathBuf> {
    let reference = rune_registry::git::GitRef::from_spec(
        spec.branch.as_deref(),
        spec.tag.as_deref(),
        spec.rev.as_deref(),
    );
    let checkout = rune_registry::git::checkout(url, &reference)?;
    Ok(rune_registry::git::find_package(&checkout, package)?)
}

/// Scans every currently resolved package for an optional dependency whose
/// activating feature has become active (from features requested on that
/// package at any point up to now, by any dependent) but which hasn't been
/// queued yet, queues it, and propagates the features it should itself be
/// requested with. Returns whether it queued anything, so the caller's
/// outer loop knows whether another round is needed.
///
/// Does one `DynamicFeatureResolver::resolve()` fixed-point pass plus one
/// full walk of `resolved_packages` — both O(current graph size) — so the
/// caller must call this once per BFS round (after the queue has fully
/// drained), never per dependency edge: that per-edge call was the
/// resolver's dominant cost on any graph of a few hundred packages, scaling
/// as (dependency edges) × (graph size) instead of (BFS depth) × (graph
/// size).
fn activate_newly_enabled_optional_deps(
    resolved_packages: &HashMap<String, ResolvedPackage>,
    feature_resolver: &mut DynamicFeatureResolver,
    queue: &mut VecDeque<QueueItem>,
) -> bool {
    let current_feature_results = feature_resolver.resolve();
    let mut queued_anything = false;
    let mut ordered: Vec<(&String, &ResolvedPackage)> = resolved_packages.iter().collect();
    ordered.sort_by(|a, b| a.0.cmp(b.0));
    for (curr_pkg_id, pkg) in ordered {
        let Some(feats) = current_feature_results.get(curr_pkg_id) else {
            continue;
        };
        for dep_ref in &pkg.dependencies {
            let actual_pkg_name = dep_ref
                .package
                .clone()
                .unwrap_or_else(|| dep_ref.name.clone());
            if !dep_ref.optional
                || !(feats.activated_dependencies.contains(&dep_ref.name)
                    || feats.active_features.contains(&dep_ref.name)
                    || feats.activated_dependencies.contains(&actual_pkg_name)
                    || feats.active_features.contains(&actual_pkg_name))
            {
                continue;
            }

            // Propagate requested features to matching target packages
            for target_pkg in resolved_packages.values() {
                if target_pkg.name == actual_pkg_name {
                    let target_pkg_id = format!("{} {}", target_pkg.name, target_pkg.version);
                    feature_resolver.request_features(
                        &target_pkg_id,
                        &dep_ref.features,
                        dep_ref.default_features,
                    );
                }
            }

            let dep_req = VersionReq::from_str(&dep_ref.req)
                .unwrap_or_else(|_| VersionReq::from_str("*").unwrap());
            let already_resolved = resolved_packages
                .values()
                .any(|p| p.name == actual_pkg_name && dep_req.matches(&p.version));
            if !already_resolved {
                let registry = dep_ref
                    .registry
                    .clone()
                    .unwrap_or_else(|| match &pkg.source {
                        PackageSource::Registry(r) => r.clone(),
                        _ => CRATES_IO.to_string(),
                    });
                queue.push_back((
                    actual_pkg_name,
                    dep_req,
                    dep_ref.features.clone(),
                    dep_ref.default_features,
                    None,
                    registry,
                ));
                queued_anything = true;
            }
        }
    }
    queued_anything
}

/// Evaluates a target condition string (e.g. `cfg(unix)`, `cfg(windows)`, `x86_64-unknown-linux-gnu`)
/// against `target_triple` (the host triple when building natively).
///
/// Delegates predicate evaluation to `cfg-expr`'s own target database
/// (`TargetInfo::matches`) rather than hand-checking each `cfg` kind against
/// this process's own compile-time `#[cfg(...)]`. A hand-rolled version that
/// only recognizes `target_os`/`target_arch`/`target_env`/`family` silently
/// evaluates every other predicate — `target_endian`, `target_pointer_width`,
/// `target_vendor`, `target_has_atomic`, `target_abi`, `panic` — as `false`
/// regardless of the real target, which is wrong often enough to matter: for
/// example `rustix`'s Linux dependency tables are gated in part on
/// `target_endian = "little"` and `target_pointer_width = "64"`, so with the
/// naive evaluator its `errno`-enabling dependency edge silently drops even
/// on ordinary x86_64 Linux, producing an unresolved-import build failure
/// with no obvious connection to target matching at all.
pub fn matches_target(target_str: Option<&str>, target_triple: &str) -> bool {
    let Some(target) = target_str else {
        return true;
    };

    let target = target.trim();
    if target.is_empty() {
        return true;
    }

    let Some(info) = cfg_expr::targets::get_builtin_target_by_triple(target_triple) else {
        // Unknown/custom triple: fail open, matching Cargo's own behavior of
        // including target-gated dependencies it cannot evaluate rather than
        // silently dropping them.
        return true;
    };

    // Bare triple (no `cfg(...)`) dependency spec: exact triple match.
    if !target.starts_with("cfg(") {
        return target == target_triple;
    }

    let expr_str = target
        .strip_prefix("cfg(")
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or(target);

    let Ok(expr) = cfg_expr::Expression::parse(expr_str) else {
        // Unparseable cfg expression: fail open rather than silently
        // dropping a dependency over a parser gap.
        return true;
    };

    expr.eval(|pred| match pred {
        cfg_expr::Predicate::Target(tgt) => info.matches(tgt),
        cfg_expr::Predicate::Flag("unix") => {
            info.families.contains(&cfg_expr::targets::Family::unix)
        }
        cfg_expr::Predicate::Flag("windows") => {
            info.families.contains(&cfg_expr::targets::Family::windows)
        }
        // `feature = "..."` (package features) and `target_feature = "..."`
        // are unrelated to which registry version/dependency to select and
        // aren't knowable at resolution time; `test`/`debug_assertions`/
        // `proc_macro` describe the *compiling* crate, not the dependency
        // target. None of these determine target-table applicability, so
        // treat them as inapplicable rather than guessing.
        _ => false,
    })
}
