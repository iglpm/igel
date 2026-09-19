use cfg_expr::expr::TargetMatcher;
use rune_manifest::dependency::{DependencyKind, DependencySpec};
use rune_registry::{CrateIndex, IndexRecord, SparseIndexClient};
use semver::{Version, VersionReq};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use thiserror::Error;

use crate::features::DynamicFeatureResolver;
use crate::lockfile::{LockPackage, Lockfile};

#[derive(Debug, Error)]
pub enum ResolverError {
    #[error("Failed to find compatible version for `{name}` matching `{req}`")]
    NoMatchingVersion { name: String, req: String },
    #[error("Registry error: {0}")]
    Registry(#[from] rune_registry::index::IndexError),
    #[error("Manifest parse error: {0}")]
    Manifest(String),
}

pub type Result<T> = std::result::Result<T, ResolverError>;

/// A fully resolved package in the dependency graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPackage {
    pub name: String,
    pub version: Version,
    pub source: PackageSource,
    pub checksum: Option<String>,
    pub dependencies: Vec<ResolvedDependencyRef>,
    pub features: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageSource {
    Registry,
    Path(PathBuf),
    Git { url: String, rev: Option<String> },
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
            let source_str = match &pkg.source {
                PackageSource::Registry => {
                    Some("registry+https://github.com/rust-lang/crates.io-index".to_string())
                }
                PackageSource::Path(_) => None,
                PackageSource::Git { url, .. } => Some(format!("git+{url}")),
            };

            let mut deps = Vec::new();
            for dep in &pkg.dependencies {
                // `dep.name` is the local extern alias (e.g. `webpki-roots`
                // depends on `rustls-pki-types` under the name `pki-types`);
                // matching against it directly only works for dependencies
                // that aren't renamed. Search by the real package name when
                // `package = "..."` is set, or the lockfile silently loses
                // the edge for every renamed dependency — which is exactly
                // what let a stale, edge-less lockfile through as "resolved
                // successfully" despite compilation needing that edge.
                let actual_name = dep.package.as_deref().unwrap_or(&dep.name);
                if let Some(target) = self.find_matching(actual_name, &dep.req) {
                    deps.push(format!("{} {}", target.name, target.version));
                }
            }
            deps.sort();

            lockfile.add_or_replace(LockPackage {
                name: pkg.name.clone(),
                version: pkg.version.to_string(),
                source: source_str,
                checksum: pkg.checksum.clone(),
                dependencies: deps,
                features: pkg.features.clone(),
            });
        }

        lockfile
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
    index_client: SparseIndexClient,
    cached_indices: HashMap<String, CrateIndex>,
    /// When true, always re-query the registry index over the network
    /// instead of trusting the local cache. Used by `rune update`/`rune add`,
    /// which need to discover newly published versions; left off for
    /// `rune build`/`rune check`, where the local cache is always sufficient
    /// once a package has been resolved once.
    refresh: bool,
    /// Never touch the network.
    offline: bool,
}

impl DependencyResolver {
    /// Creates a new `DependencyResolver` that trusts the local index cache.
    pub fn new(index_client: SparseIndexClient) -> Self {
        Self {
            index_client,
            cached_indices: HashMap::new(),
            refresh: false,
            offline: false,
        }
    }

    /// Creates a default resolver that trusts the local index cache
    /// (no network access unless a package has never been seen before).
    pub fn default_resolver() -> Self {
        Self::new(SparseIndexClient::default_client())
    }

    /// Creates a resolver that always re-queries the registry index over
    /// the network, used for `rune update`/`rune add` where discovering
    /// newly published versions is the point of the operation.
    pub fn refreshing_resolver() -> Self {
        let mut resolver = Self::new(SparseIndexClient::default_client());
        resolver.refresh = true;
        resolver
    }

    /// Makes the resolver use only the local index cache.
    pub fn offline(mut self, offline: bool) -> Self {
        self.index_client =
            std::mem::replace(&mut self.index_client, SparseIndexClient::default_client())
                .offline(offline);
        self.offline = offline;
        self
    }

    /// Fetches the index of every not-yet-loaded registry crate in `names`
    /// concurrently, so the sequential resolution walk below hits memory.
    fn prefetch<'a>(&mut self, names: impl Iterator<Item = &'a str>) {
        let mut wanted: Vec<String> = names
            .filter(|n| !self.cached_indices.contains_key(*n))
            .map(str::to_string)
            .collect();
        wanted.sort();
        wanted.dedup();
        if wanted.is_empty() {
            return;
        }
        let _timer = rune_common::timing::phase("index prefetch");
        let results = self.index_client.fetch_many_with(&wanted, self.refresh);
        for (name, result) in wanted.into_iter().zip(results) {
            // Errors are deliberately not recorded: the item that actually
            // needs this crate re-fetches it and reports the real error.
            if let Ok(index) = result {
                self.cached_indices.insert(name, index);
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
        let host_triple = rune_common::TargetTriple::host()
            .map(|t| t.0)
            .unwrap_or_else(|| "x86_64-unknown-linux-gnu".to_string());

        let mut resolved_packages: HashMap<String, ResolvedPackage> = HashMap::new();
        let mut feature_resolver = DynamicFeatureResolver::new();
        let mut queue: VecDeque<(String, VersionReq, Vec<String>, bool, Option<PathBuf>)> =
            VecDeque::new();
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
            queue.push_back((
                name.clone(),
                req,
                spec.features.clone(),
                spec.default_features,
                local_dir,
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
                        .filter(|(_, _, _, _, local_dir)| local_dir.is_none())
                        .map(|(name, ..)| name.as_str()),
                );
                for (name, req, features, default_features, local_dir) in generation {
                    resolve_one_queue_item(
                        name,
                        req,
                        features,
                        default_features,
                        local_dir,
                        &mut resolved_packages,
                        &mut feature_resolver,
                        &mut queue,
                        &mut visited,
                        &host_triple,
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

    fn get_or_fetch_index(&mut self, name: &str) -> Result<&CrateIndex> {
        if !self.cached_indices.contains_key(name) {
            let index = if self.refresh {
                self.index_client.refresh_crate(name)?
            } else {
                self.index_client.fetch_crate(name)?
            };
            self.cached_indices.insert(name.to_string(), index);
        }
        Ok(self.cached_indices.get(name).unwrap())
    }
}

#[allow(clippy::too_many_arguments)]
fn resolve_one_queue_item(
    name: String,
    req: VersionReq,
    features: Vec<String>,
    default_features: bool,
    local_dir: Option<PathBuf>,
    resolved_packages: &mut HashMap<String, ResolvedPackage>,
    feature_resolver: &mut DynamicFeatureResolver,
    queue: &mut VecDeque<(String, VersionReq, Vec<String>, bool, Option<PathBuf>)>,
    visited: &mut HashSet<(String, String)>,
    host_triple: &str,
    workspace: Option<&rune_manifest::workspace::WorkspaceManifest>,
    existing_lock: Option<&Lockfile>,
    resolver: &mut DependencyResolver,
) -> Result<()> {
    // If local path dependency
    if let Some(dir) = local_dir {
        let manifest_path = dir.join("Cargo.toml");
        if let Ok(manifest) = rune_manifest::Manifest::from_path(&manifest_path) {
            if let Some(pkg) = manifest.package {
                let version: Version = pkg
                    .version_in(workspace)
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
                    if dep_spec.kind == DependencyKind::Dev {
                        continue;
                    }
                    let mut spec = dep_spec.spec.clone();
                    if let Some(ws_dep) =
                        workspace.and_then(|ws| ws.dependencies.get(&dep_spec.name))
                    {
                        spec.merge_workspace(ws_dep);
                    }
                    let dep_req = spec
                        .version
                        .clone()
                        .unwrap_or_else(|| VersionReq::from_str("*").unwrap());
                    let child_local_dir = spec.path.as_ref().map(|p| dir.join(p));

                    let actual_pkg_name = spec
                        .package
                        .clone()
                        .unwrap_or_else(|| dep_spec.name.clone());
                    dep_refs.push(ResolvedDependencyRef {
                        name: dep_spec.name.clone(),
                        req: dep_req.to_string(),
                        kind: dep_spec.kind,
                        optional: spec.optional,
                        features: spec.features.clone(),
                        default_features: spec.default_features,
                        package: spec.package.clone(),
                    });

                    if !spec.optional {
                        queue.push_back((
                            actual_pkg_name,
                            dep_req,
                            spec.features.clone(),
                            spec.default_features,
                            child_local_dir,
                        ));
                    }
                }

                resolved_packages.insert(
                    pkg_id,
                    ResolvedPackage {
                        name,
                        version,
                        source: PackageSource::Path(dir),
                        checksum: None,
                        dependencies: dep_refs,
                        features: Vec::new(),
                    },
                );
                return Ok(());
            }
        }
    }

    // If already resolved with a version that matches req, reuse and request features
    if let Some((pkg_id, _existing)) = resolved_packages
        .iter()
        .find(|(_, p)| p.name == name && req.matches(&p.version))
    {
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
        let pinned = lock
            .packages
            .iter()
            .filter(|p| p.name == name && p.source.is_some())
            .filter_map(|p| Version::parse(&p.version).ok())
            .filter(|v| req.matches(v))
            .max();
        if let Some(ver) = pinned {
            if let Ok(index) = resolver.get_or_fetch_index(&name) {
                if let Some(rec) = index.get_version(&ver.to_string()) {
                    chosen_record = Some(rec.clone());
                }
            }
        }
    }

    // If not found in lockfile, query index for latest matching version
    if chosen_record.is_none() {
        let index = resolver.get_or_fetch_index(&name)?;
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

        if let Some(&best) = matches.first() {
            chosen_record = Some(best.clone());
        } else if !resolver.refresh && !resolver.offline {
            // The cached index may simply predate the release this manifest
            // asks for (e.g. `serde = "1.0.999"` typed by hand). Cargo would
            // have queried the registry; do the same once before giving up.
            resolver.cached_indices.remove(&name);
            resolver.refresh = true;
            let retry = resolver.get_or_fetch_index(&name).map(|i| {
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

        if !matches_target(dep.target.as_deref(), host_triple) {
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
        };
        dep_refs.push(dep_ref);

        if !dep.optional {
            queue.push_back((
                actual_pkg_name,
                dep_req,
                dep.features.clone(),
                dep.default_features,
                None,
            ));
        }
    }

    resolved_packages.insert(
        pkg_id,
        ResolvedPackage {
            name,
            version: parsed_version,
            source: PackageSource::Registry,
            checksum: Some(record.cksum),
            dependencies: dep_refs,
            features: Vec::new(),
        },
    );

    Ok(())
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
    queue: &mut VecDeque<(String, VersionReq, Vec<String>, bool, Option<PathBuf>)>,
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
                queue.push_back((
                    actual_pkg_name,
                    dep_req,
                    dep_ref.features.clone(),
                    dep_ref.default_features,
                    None,
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
