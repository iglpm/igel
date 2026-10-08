use cfg_expr::expr::TargetMatcher;
use rune_manifest::dependency::{DependencyKind, DependencySpec};
use rune_registry::source::{CRATES_IO, IndexClient};
use rune_registry::{CrateIndex, IndexRecord};
use semver::{Version, VersionReq};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use thiserror::Error;

use crate::features::DynamicFeatureResolver;
use crate::lockfile::{LockDecl, LockPackage, Lockfile};

#[derive(Debug, Error)]
pub enum ResolverError {
    #[error("Failed to find compatible version for `{name}` matching `{req}`")]
    NoMatchingVersion { name: String, req: String },
    #[error(
        "the lockfile pins `{name}` {version}, which the registry index cached on this \
         machine does not list yet (run once without `--offline` to update it)"
    )]
    LockedVersionNotCached { name: String, version: String },
    #[error(transparent)]
    Registry(#[from] rune_registry::index::IndexError),
    #[error("Manifest parse error: {0}")]
    Manifest(String),
    #[error("Git dependency error: {0}")]
    Git(#[from] rune_registry::git::GitError),
}

pub type Result<T> = std::result::Result<T, ResolverError>;

type QueueItem = (
    String,
    VersionReq,
    Vec<String>,
    bool,
    Option<PathBuf>,
    String,
);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPackage {
    pub name: String,
    pub version: Version,
    pub source: PackageSource,
    pub checksum: Option<String>,
    pub dependencies: Vec<ResolvedDependencyRef>,
    pub features: Vec<String>,
    pub declared_features: HashMap<String, Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageSource {
    Registry(String),
    Path(PathBuf),
    Git {
        url: String,
        reference: Option<String>,
        commit: String,
    },
}

impl PackageSource {
    pub fn crates_io() -> Self {
        Self::Registry(rune_common::CRATES_IO_INDEX.to_string())
    }

    pub fn is_registry(&self) -> bool {
        matches!(self, Self::Registry(_))
    }

    pub fn lock_source(&self) -> Option<String> {
        match self {
            Self::Registry(index) => Some(if index == rune_common::CRATES_IO_INDEX {
                // Cargo's canonical spelling of crates.io.
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
    pub target: Option<String>,
    pub registry: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ResolvedGraph {
    pub root_packages: Vec<String>,
    pub packages: HashMap<String, ResolvedPackage>,
}

impl ResolvedGraph {
    pub fn to_lockfile(&self) -> Lockfile {
        let mut lockfile = Lockfile::new();

        for pkg in self.packages.values() {
            let source_str = pkg.source.lock_source();

            let mut deps = Vec::new();
            let mut decls = Vec::new();
            for dep in &pkg.dependencies {
                // Match by the real package name: `dep.name` is the local alias for renamed dependencies.
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

    pub fn find_matching_in(
        &self,
        name: &str,
        req_str: &str,
        registry: &str,
    ) -> Option<&ResolvedPackage> {
        let req =
            VersionReq::from_str(req_str).unwrap_or_else(|_| VersionReq::from_str("*").unwrap());
        let candidates = || {
            self.packages
                .values()
                .filter(|p| p.name == name && req.matches(&p.version))
        };
        candidates()
            .filter(|p| matches!(&p.source, PackageSource::Registry(r) if r == registry))
            .max_by(|a, b| a.version.cmp(&b.version))
            .or_else(|| {
                candidates()
                    .filter(|p| !matches!(p.source, PackageSource::Registry(_)))
                    .max_by(|a, b| a.version.cmp(&b.version))
            })
    }

    pub fn find_matching(&self, name: &str, req_str: &str) -> Option<&ResolvedPackage> {
        let req =
            VersionReq::from_str(req_str).unwrap_or_else(|_| VersionReq::from_str("*").unwrap());
        self.packages
            .values()
            .filter(|p| p.name == name && req.matches(&p.version))
            .max_by(|a, b| a.version.cmp(&b.version))
    }
}

pub struct DependencyResolver {
    clients: HashMap<String, IndexClient>,
    cached_indices: HashMap<(String, String), CrateIndex>,
    msrv: Option<Version>,
    refresh: bool,
    offline: bool,
    patches: crate::patch::Patches,
    hints: Vec<(String, String)>,
    prefer: HashMap<(String, String), Vec<Version>>,
    exclude: HashSet<(String, String, Version)>,
    min_release_age: Option<std::time::Duration>,
    partial: Vec<(String, String, Version)>,
}

impl Default for DependencyResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl DependencyResolver {
    pub fn new() -> Self {
        Self {
            clients: HashMap::new(),
            cached_indices: HashMap::new(),
            msrv: None,
            refresh: false,
            offline: false,
            patches: Default::default(),
            hints: Vec::new(),
            prefer: HashMap::new(),
            exclude: HashSet::new(),
            min_release_age: None,
            partial: Vec::new(),
        }
    }

    /// Only choose releases at least `age` old unless locked; falls back to the newest fitting one.
    pub fn with_min_release_age(mut self, age: Option<std::time::Duration>) -> Self {
        self.min_release_age = age.filter(|a| !a.is_zero());
        self
    }

    pub fn with_preferred(mut self, preferred: &[(String, String, Version)]) -> Self {
        for (registry, name, version) in preferred {
            self.prefer
                .entry((registry.clone(), name.clone()))
                .or_default()
                .push(version.clone());
        }
        self
    }

    /// Prefetch the crates of an earlier lockfile in one concurrent wave. Only warms the cache.
    pub fn with_prefetch_hint(mut self, lock: Option<&Lockfile>) -> Self {
        if let Some(lock) = lock {
            self.hints.extend(lock.packages.iter().filter_map(|p| {
                let registry = registry_of_lock_source(p.source.as_deref()?)?;
                Some((registry, p.name.clone()))
            }));
        }
        self
    }

    pub fn with_patches(mut self, patches: crate::patch::Patches) -> Self {
        self.patches = patches;
        self
    }

    pub fn with_msrv(mut self, msrv: Option<Version>) -> Self {
        self.msrv = msrv;
        self
    }

    pub fn default_resolver() -> Self {
        Self::new()
    }

    pub fn refreshing_resolver() -> Self {
        let mut resolver = Self::new();
        resolver.refresh = true;
        resolver
    }

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
                if let Ok(index) = result {
                    self.cached_indices.insert((registry.clone(), name), index);
                }
            }
        }
    }

    /// Pipelined prefetch from `seeds`: each arriving index file requests the likely choice's dependencies
    /// at once instead of waiting per BFS level. A wrong guess costs a request, never correctness.
    fn prefetch_closure(
        &mut self,
        seeds: Vec<(String, String, VersionReq)>,
        existing_lock: Option<&Lockfile>,
    ) {
        use std::sync::{Condvar, Mutex};
        if self.offline || seeds.is_empty() {
            return;
        }
        let _timer = rune_common::timing::phase("index prefetch");
        let mut registries: Vec<String> = seeds.iter().map(|(r, ..)| r.clone()).collect();
        registries.sort();
        registries.dedup();
        for r in &registries {
            let _ = self.client(r);
        }
        let locked: HashMap<&str, Vec<Version>> = existing_lock.map_or_else(HashMap::new, |lock| {
            let mut m: HashMap<&str, Vec<Version>> = HashMap::new();
            for p in lock.packages.iter().filter(|p| p.source.is_some()) {
                if let Ok(v) = Version::parse(&p.version) {
                    m.entry(p.name.as_str()).or_default().push(v);
                }
            }
            m
        });

        struct State {
            queue: VecDeque<(String, String, VersionReq)>,
            seen: HashSet<(String, String)>,
            in_flight: usize,
            fetched: Vec<((String, String), CrateIndex)>,
        }
        let mut seen = HashSet::new();
        let mut queue = VecDeque::new();
        for (r, n, req) in seeds {
            if seen.insert((r.clone(), n.clone())) {
                queue.push_back((r, n, req));
            }
        }
        let state = Mutex::new(State {
            queue,
            seen,
            in_flight: 0,
            fetched: Vec::new(),
        });
        let wake = Condvar::new();
        let clients = &self.clients;
        let cached = &self.cached_indices;
        let refresh = self.refresh;
        std::thread::scope(|scope| {
            for _ in 0..PREFETCH_WORKERS {
                scope.spawn(|| {
                    loop {
                        let (registry, name, req) = {
                            let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                            loop {
                                if let Some(item) = st.queue.pop_front() {
                                    st.in_flight += 1;
                                    break item;
                                }
                                if st.in_flight == 0 {
                                    wake.notify_all();
                                    return;
                                }
                                st = wake.wait(st).unwrap_or_else(|e| e.into_inner());
                            }
                        };
                        let key = (registry.clone(), name.clone());
                        let (index, fresh) = match cached.get(&key) {
                            Some(i) => (Some(i.clone()), false),
                            None => (
                                clients.get(&registry).and_then(|c| {
                                    if refresh {
                                        c.refresh_crate(&name).ok()
                                    } else {
                                        c.fetch_crate(&name).ok()
                                    }
                                }),
                                true,
                            ),
                        };
                        let children = index
                            .as_ref()
                            .map(|i| {
                                likely_dependencies(
                                    i,
                                    &req,
                                    locked.get(name.as_str()).map(Vec::as_slice),
                                )
                            })
                            .unwrap_or_default();
                        let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                        for (dep_registry, dep_name, dep_req) in children {
                            let dep_registry = dep_registry.unwrap_or_else(|| registry.clone());
                            if !clients.contains_key(&dep_registry) {
                                continue;
                            }
                            let dep_key = (dep_registry.clone(), dep_name.clone());
                            if cached.contains_key(&dep_key) {
                                continue;
                            }
                            if st.seen.insert(dep_key) {
                                st.queue.push_back((dep_registry, dep_name, dep_req));
                            }
                        }
                        if fresh && let Some(index) = index {
                            st.fetched.push((key, index));
                        }
                        st.in_flight -= 1;
                        wake.notify_all();
                    }
                });
            }
        });
        let fetched = std::mem::take(
            &mut state
                .into_inner()
                .unwrap_or_else(|e| e.into_inner())
                .fetched,
        );
        for (key, index) in fetched {
            self.cached_indices.insert(key, index);
        }
    }

    pub fn resolve(
        &mut self,
        project_root: &Path,
        root_dependencies: &[(String, DependencySpec)],
        existing_lock: Option<&Lockfile>,
    ) -> Result<ResolvedGraph> {
        self.resolve_with_workspace(project_root, root_dependencies, existing_lock, None)
    }

    pub fn resolve_with_workspace(
        &mut self,
        project_root: &Path,
        root_dependencies: &[(String, DependencySpec)],
        existing_lock: Option<&Lockfile>,
        workspace: Option<&rune_manifest::workspace::WorkspaceManifest>,
    ) -> Result<ResolvedGraph> {
        // Cargo allows one version per compatible range. Conflicts are settled by preference or backtracking
        // and the walk re-run; almost every graph needs one round.
        let mut last: Option<(ResolvedGraph, Vec<Edge>)> = None;
        let mut solved = false;
        for _ in 0..64 {
            let round =
                self.resolve_round(project_root, root_dependencies, existing_lock, workspace);
            let (graph, edges) = match round {
                Ok(r) => r,
                Err(ResolverError::NoMatchingVersion { name, req })
                    if self.backtrack_from_missing(&name, &req) =>
                {
                    continue;
                }
                Err(e) => {
                    let Some((graph, edges)) = last else {
                        return Err(e);
                    };
                    if !solved {
                        solved = true;
                        if self.solve_whole(&graph, &edges) {
                            last = Some((graph, edges));
                            continue;
                        }
                    }
                    warn_side_by_side(&graph);
                    return Ok(graph);
                }
            };
            let conflicts = compatible_duplicates(&graph);
            if conflicts.is_empty() {
                return Ok(graph);
            }
            let _timer = rune_common::timing::phase("version unification");
            let mut progressed = false;
            for (registry, name, versions) in conflicts {
                progressed |= self.settle(&registry, &name, &versions, &edges, &graph);
            }
            if !progressed {
                if !solved {
                    solved = true;
                    if self.solve_whole(&graph, &edges) {
                        last = Some((graph, edges));
                        continue;
                    }
                }
                warn_side_by_side(&graph);
                return Ok(graph);
            }
            last = Some((graph, edges));
        }
        last.map(|(g, _)| g)
            .ok_or_else(|| ResolverError::Manifest("version resolution did not settle".into()))
    }

    fn solve_whole(&mut self, graph: &ResolvedGraph, edges: &[Edge]) -> bool {
        let _timer = rune_common::timing::phase("version solving");
        let requirements: Vec<crate::solver::Requirement<'_>> = edges
            .iter()
            .map(|e| crate::solver::Requirement {
                from: e.from.as_deref(),
                registry: &e.registry,
                name: &e.name,
                req: &e.req,
            })
            .collect();
        match crate::solver::solve(self, graph, &requirements) {
            crate::solver::Solution::Found(chosen) => {
                for (registry, name, v) in chosen {
                    let list = self.prefer.entry((registry, name)).or_default();
                    list.retain(|p| compat_class(p) != compat_class(&v));
                    list.push(v);
                }
                self.exclude.clear();
                true
            }
            crate::solver::Solution::Impossible(why) => {
                tracing::warn!("no set of versions satisfies every requirement:\n{why}");
                false
            }
        }
    }

    fn backtrack_from_missing(&mut self, name: &str, req: &str) -> bool {
        let Ok(wanted) = VersionReq::from_str(req) else {
            return false;
        };
        let mut progressed = false;
        for (registry, pkg, version) in std::mem::take(&mut self.partial) {
            let requires = self
                .get_or_fetch_index(&registry, &pkg)
                .ok()
                .and_then(|i| i.get_version(&version.to_string()))
                .is_some_and(|rec| {
                    rec.deps.iter().any(|d| {
                        d.kind.as_deref() != Some("dev")
                            && d.package.as_deref().unwrap_or(&d.name) == name
                            && VersionReq::from_str(&d.req).is_ok_and(|r| r == wanted)
                    })
                });
            if requires {
                tracing::debug!("{pkg} {version} needs {name} {req}, which does not exist");
                progressed |= self.exclude.insert((registry, pkg, version));
            }
        }
        progressed
    }

    pub(crate) fn index_for(&mut self, registry: &str, name: &str) -> Result<&CrateIndex> {
        self.get_or_fetch_index(registry, name)
    }

    fn settle(
        &mut self,
        registry: &str,
        name: &str,
        versions: &[Version],
        edges: &[Edge],
        graph: &ResolvedGraph,
    ) -> bool {
        let reqs: Vec<&Edge> = edges
            .iter()
            .filter(|e| {
                e.name == name
                    && e.registry == registry
                    && versions.iter().any(|v| e.req.matches(v))
            })
            .collect();
        let class = compat_class(&versions[0]);
        let Ok(index) = self.get_or_fetch_index(registry, name) else {
            return false;
        };
        let mut candidates: Vec<Version> = index
            .available_versions()
            .into_iter()
            .filter_map(|r| Version::parse(&r.vers).ok())
            .filter(|v| compat_class(v) == class)
            .collect();
        candidates.sort_by(|a, b| b.cmp(a));
        candidates.retain(|v| {
            !self
                .exclude
                .contains(&(registry.to_string(), name.to_string(), v.clone()))
        });
        let fits = |subset: &[&&Edge], v: &Version| subset.iter().all(|e| e.req.matches(v));
        let all: Vec<&&Edge> = reqs.iter().collect();
        if let Some(v) = candidates.iter().find(|v| fits(&all, v)) {
            let key = (registry.to_string(), name.to_string());
            let list = self.prefer.entry(key).or_default();
            if list.contains(v) {
                return false;
            }
            list.retain(|p| compat_class(p) != class);
            list.push(v.clone());
            return true;
        }
        for (i, culprit) in reqs.iter().enumerate() {
            let others: Vec<&&Edge> = reqs
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, e)| e)
                .collect();
            if !candidates.iter().any(|v| fits(&others, v)) {
                continue;
            }
            let Some(from) = &culprit.from else { continue };
            let Some(pkg) = graph.packages.get(from) else {
                continue;
            };
            if let PackageSource::Registry(r) = &pkg.source
                && self
                    .exclude
                    .insert((r.clone(), pkg.name.clone(), pkg.version.clone()))
            {
                return true;
            }
        }

        let registry_release = |e: &Edge| -> Option<(String, String, Version)> {
            let pkg = graph.packages.get(e.from.as_ref()?)?;
            match &pkg.source {
                PackageSource::Registry(r) => {
                    Some((r.clone(), pkg.name.clone(), pkg.version.clone()))
                }
                _ => None,
            }
        };
        let fixed: Vec<&&Edge> = reqs
            .iter()
            .filter(|e| registry_release(e).is_none())
            .collect();
        let mut progressed = false;
        if !fixed.is_empty() && candidates.iter().any(|v| fits(&fixed, v)) {
            for e in &reqs {
                let compatible = candidates
                    .iter()
                    .any(|v| e.req.matches(v) && fixed.iter().all(|f| f.req.matches(v)));
                if !compatible && let Some(release) = registry_release(e) {
                    progressed |= self.exclude.insert(release);
                }
            }
            if progressed {
                return true;
            }
        }
        let mut movable: Vec<(usize, (String, String, Version))> = reqs
            .iter()
            .filter_map(|e| {
                let release = registry_release(e)?;
                let allowed = candidates.iter().filter(|v| e.req.matches(v)).count();
                Some((allowed, release))
            })
            .collect();
        movable.sort();
        movable
            .into_iter()
            .any(|(_, release)| self.exclude.insert(release))
    }

    fn resolve_round(
        &mut self,
        project_root: &Path,
        root_dependencies: &[(String, DependencySpec)],
        existing_lock: Option<&Lockfile>,
        workspace: Option<&rune_manifest::workspace::WorkspaceManifest>,
    ) -> Result<(ResolvedGraph, Vec<Edge>)> {
        let mut hints = std::mem::take(&mut self.hints);
        if let Some(lock) = existing_lock {
            hints.extend(lock.packages.iter().filter_map(|p| {
                let registry = registry_of_lock_source(p.source.as_deref()?)?;
                Some((registry, p.name.clone()))
            }));
        }
        if !self.offline {
            let mut seeds: Vec<(String, String, VersionReq)> = hints
                .iter()
                .map(|(r, n)| (r.clone(), n.clone(), VersionReq::STAR))
                .collect();
            for (name, spec) in root_dependencies {
                if spec.path.is_some() || spec.git.is_some() {
                    continue;
                }
                if let Ok(registry) =
                    rune_registry::source::defs().index_for_name(spec.registry.as_deref())
                {
                    seeds.push((
                        registry,
                        spec.package.clone().unwrap_or_else(|| name.clone()),
                        spec.version.clone().unwrap_or(VersionReq::STAR),
                    ));
                }
            }
            self.prefetch_closure(seeds, existing_lock);
        }

        let mut resolved_packages: HashMap<String, ResolvedPackage> = HashMap::new();
        let mut feature_resolver = DynamicFeatureResolver::new();
        let mut queue: VecDeque<QueueItem> = VecDeque::new();
        let mut visited: HashSet<(String, String)> = HashSet::new();

        let mut root_names = Vec::new();

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

        // Feature activation runs once per BFS round, not per edge: it is O(graph size).
        loop {
            while !queue.is_empty() {
                let generation: Vec<_> = queue.drain(..).collect();
                let missing: Vec<(String, String, VersionReq)> = generation
                    .iter()
                    .filter(|(name, _, _, _, local_dir, registry)| {
                        local_dir.is_none()
                            && !self
                                .cached_indices
                                .contains_key(&(registry.clone(), name.clone()))
                    })
                    .map(|(name, req, _, _, _, registry)| {
                        (registry.clone(), name.clone(), req.clone())
                    })
                    .collect();
                if self.offline {
                    self.prefetch(missing.iter().map(|(r, n, _)| (r.as_str(), n.as_str())));
                } else {
                    self.prefetch_closure(missing, existing_lock);
                }
                for (name, req, features, default_features, local_dir, registry) in generation {
                    let step = resolve_one_queue_item(
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
                    );
                    if let Err(e) = step {
                        self.partial = resolved_packages
                            .values()
                            .filter_map(|p| match &p.source {
                                PackageSource::Registry(r) => {
                                    Some((r.clone(), p.name.clone(), p.version.clone()))
                                }
                                _ => None,
                            })
                            .collect();
                        return Err(e);
                    }
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

        let _timer = rune_common::timing::phase("feature unification");
        let feature_results = feature_resolver.resolve();

        let mut edges = Vec::new();
        for (name, spec) in root_dependencies {
            if spec.path.is_some() || spec.git.is_some() {
                continue;
            }
            let Ok(registry) =
                rune_registry::source::defs().index_for_name(spec.registry.as_deref())
            else {
                continue;
            };
            edges.push(Edge {
                from: None,
                name: spec.package.clone().unwrap_or_else(|| name.clone()),
                registry,
                req: spec.version.clone().unwrap_or(VersionReq::STAR),
            });
        }
        for (pkg_id, pkg) in &resolved_packages {
            let feats = feature_results.get(pkg_id);
            for dep in &pkg.dependencies {
                if dep.kind == DependencyKind::Dev {
                    continue;
                }
                let real = dep.package.clone().unwrap_or_else(|| dep.name.clone());
                let active = !dep.optional || feats.is_some_and(|f| switched_on(f, &dep.name));
                let Ok(req) = VersionReq::from_str(&dep.req) else {
                    continue;
                };
                if active {
                    edges.push(Edge {
                        from: Some(pkg_id.clone()),
                        name: real,
                        registry: dep.registry.clone().unwrap_or_else(|| match &pkg.source {
                            PackageSource::Registry(r) => r.clone(),
                            _ => CRATES_IO.to_string(),
                        }),
                        req,
                    });
                }
            }
        }

        for (pkg_id, resolved_pkg) in &mut resolved_packages {
            if let Some(feats) = feature_results.get(pkg_id) {
                resolved_pkg.features = feats.active_features.clone();
            }
        }

        Ok((
            ResolvedGraph {
                root_packages: root_names,
                packages: resolved_packages,
            },
            edges,
        ))
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

const PREFETCH_WORKERS: usize = 96;

fn likely_dependencies(
    index: &CrateIndex,
    req: &VersionReq,
    locked: Option<&[Version]>,
) -> Vec<(Option<String>, String, VersionReq)> {
    let versions: Vec<(&IndexRecord, Version)> = index
        .versions
        .iter()
        .filter_map(|r| Some((r, Version::parse(&r.vers).ok()?)))
        .filter(|(_, v)| req.matches(v))
        .collect();
    let chosen = locked
        .and_then(|l| {
            versions
                .iter()
                .filter(|(_, v)| l.contains(v))
                .max_by(|a, b| a.1.cmp(&b.1))
        })
        .or_else(|| {
            versions
                .iter()
                .filter(|(r, _)| !r.yanked)
                .max_by(|a, b| a.1.cmp(&b.1))
        });
    let Some((record, _)) = chosen else {
        return Vec::new();
    };
    let on = default_enabled_dependencies(record);
    record
        .deps
        .iter()
        .filter(|d| d.kind.as_deref() != Some("dev"))
        .filter(|d| !d.optional || on.contains(d.name.as_str()))
        .map(|d| {
            (
                d.registry.as_deref().map(normalize_registry),
                d.package.clone().unwrap_or_else(|| d.name.clone()),
                VersionReq::from_str(&d.req).unwrap_or(VersionReq::STAR),
            )
        })
        .collect()
}

fn default_enabled_dependencies(record: &IndexRecord) -> HashSet<String> {
    let features = record.all_features();
    let optional: HashSet<&str> = record
        .deps
        .iter()
        .filter(|d| d.optional)
        .map(|d| d.name.as_str())
        .collect();
    let mut on = HashSet::new();
    let mut seen = HashSet::new();
    let mut stack = vec!["default".to_string()];
    while let Some(f) = stack.pop() {
        if !seen.insert(f.clone()) {
            continue;
        }
        for entry in features.get(&f).into_iter().flatten() {
            if let Some(dep) = entry.strip_prefix("dep:") {
                on.insert(dep.to_string());
            } else if let Some((dep, _)) = entry.split_once('/') {
                if !dep.ends_with('?') && optional.contains(dep) {
                    on.insert(dep.to_string());
                }
            } else if features.contains_key(entry) {
                stack.push(entry.clone());
            } else if optional.contains(entry.as_str()) {
                on.insert(entry.clone());
            }
        }
    }
    on
}

struct Edge {
    from: Option<String>,
    name: String,
    registry: String,
    req: VersionReq,
}

use crate::solver::compat_class;

fn warn_side_by_side(graph: &ResolvedGraph) {
    for (_, name, versions) in compatible_duplicates(graph) {
        tracing::warn!(
            "no single version of `{name}` satisfies every requirement in the \
             graph; using {} side by side (Cargo would refuse)",
            versions
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(" and ")
        );
    }
}

pub(crate) fn normalize_registry_pub(url: &str) -> String {
    normalize_registry(url)
}

fn compatible_duplicates(graph: &ResolvedGraph) -> Vec<(String, String, Vec<Version>)> {
    type Class = (String, String, (u64, u64, u64));
    let mut classes: BTreeMap<Class, Vec<Version>> = BTreeMap::new();
    for p in graph.packages.values() {
        if let PackageSource::Registry(r) = &p.source {
            classes
                .entry((r.clone(), p.name.clone(), compat_class(&p.version)))
                .or_default()
                .push(p.version.clone());
        }
    }
    classes
        .into_iter()
        .filter(|(_, v)| v.len() > 1)
        .map(|((r, n, _), mut v)| {
            v.sort();
            (r, n, v)
        })
        .collect()
}

fn registry_of_lock_source(source: &str) -> Option<String> {
    if source == "registry+https://github.com/rust-lang/crates.io-index" {
        Some(rune_common::CRATES_IO_INDEX.to_string())
    } else if source.starts_with("sparse+") || source.starts_with("registry+") {
        Some(source.to_string())
    } else {
        None
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
    if let Some(dir) = local_dir {
        let manifest_path = rune_common::manifest_file::manifest_in(&dir);
        if let Ok(manifest) = rune_manifest::Manifest::from_path(&manifest_path) {
            let canon = rune_common::fsutil::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
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
                // Sorted: `HashMap` order would make the lockfile vary between runs.
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
                    let child_registry = rune_registry::source::defs()
                        .index_for_name(spec.registry.as_deref())
                        .map_err(ResolverError::Registry)?;
                    let child_local_dir = if let Some(p) = &spec.path {
                        Some(p.clone())
                    } else if let (Some(url), false) = (&spec.git, spec.optional) {
                        match resolver
                            .patches
                            .git_patch_dir(url, &actual_pkg_name, &dep_req)
                            .map_err(ResolverError::Manifest)?
                        {
                            Some(dir) => Some(dir),
                            None => Some(git_package_dir(&spec, url, &actual_pkg_name)?),
                        }
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

    // Reuse a resolved version from the same registry; a local package of the same name doesn't count.
    if let Some((pkg_id, _existing)) = resolved_packages.iter().find(|(_, p)| {
        p.name == name
            && req.matches(&p.version)
            && matches!(&p.source, PackageSource::Registry(r) if *r == registry)
    }) {
        feature_resolver.request_features(pkg_id, &features, default_features);
        return Ok(());
    }

    // Releases an earlier round backtracked away from.
    let excluded_versions: HashSet<Version> = resolver
        .exclude
        .iter()
        .filter(|(r, n, _)| *r == registry && *n == name)
        .map(|(_, _, v)| v.clone())
        .collect();
    let excluded = |v: &Version| excluded_versions.contains(v);
    let mut chosen_record = None;
    let preferred = resolver
        .prefer
        .get(&(registry.clone(), name.clone()))
        .and_then(|list| list.iter().filter(|v| req.matches(v) && !excluded(v)).max())
        .cloned();
    if let Some(ver) = preferred
        && let Ok(index) = resolver.get_or_fetch_index(&registry, &name)
        && let Some(rec) = index.get_version(&ver.to_string())
    {
        chosen_record = Some(rec.clone());
    }
    if let (Some(lock), None) = (existing_lock, &chosen_record) {
        let wanted_source = PackageSource::Registry(registry.clone()).lock_source();
        let pinned = lock
            .packages
            .iter()
            .filter(|p| p.name == name && p.source == wanted_source)
            .filter_map(|p| Version::parse(&p.version).ok())
            .filter(|v| req.matches(v) && !excluded(v))
            .max();
        if let Some(ver) = pinned {
            let key = (registry.clone(), name.clone());
            let lookup = |resolver: &mut DependencyResolver| {
                resolver
                    .get_or_fetch_index(&registry, &name)
                    .ok()
                    .and_then(|index| index.get_version(&ver.to_string()).cloned())
            };
            let mut found = lookup(resolver);
            if found.is_none() && !resolver.refresh && !resolver.offline {
                // The cached index predates the locked release: refresh, as Cargo would.
                resolver.cached_indices.remove(&key);
                resolver.refresh = true;
                found = lookup(resolver);
                resolver.refresh = false;
            }
            if found.is_none() && resolver.offline {
                return Err(ResolverError::LockedVersionNotCached {
                    name,
                    version: ver.to_string(),
                });
            }
            chosen_record = found;
        }
    }

    if chosen_record.is_none() {
        let msrv = resolver.msrv.clone();
        let min_release_age = resolver.min_release_age;
        let index = resolver.get_or_fetch_index(&registry, &name)?;
        let mut matches: Vec<&IndexRecord> = index
            .available_versions()
            .into_iter()
            .filter(|rec| {
                if let Ok(ver) = Version::parse(&rec.vers) {
                    req.matches(&ver) && !excluded(&ver)
                } else {
                    false
                }
            })
            .collect();

        matches.sort_by(|a, b| {
            let va = Version::parse(&a.vers).unwrap_or_else(|_| Version::new(0, 0, 0));
            let vb = Version::parse(&b.vers).unwrap_or_else(|_| Version::new(0, 0, 0));
            vb.cmp(&va)
        });

        if let Some(age) = min_release_age {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let cutoff = now.saturating_sub(age.as_secs());
            let mature = |rec: &&IndexRecord| {
                rec.pubtime
                    .as_deref()
                    .and_then(rune_common::time::parse_rfc3339)
                    .is_none_or(|t| t <= cutoff)
            };
            if matches.iter().any(mature) {
                matches.retain(mature);
            } else if let Some(newest) = matches.first() {
                tracing::warn!(
                    "{name} {}: every release matching `{req}` is younger than \
                     resolve.min-release-age; using it anyway",
                    newest.vers
                );
            }
        }

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
            // The cached index may predate the requested release; query once before giving up.
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
    // A local package and the registry release of the same version can't coexist: the local one serves.
    if resolved_packages
        .get(&pkg_id)
        .is_some_and(|p| !matches!(p.source, PackageSource::Registry(_)))
    {
        feature_resolver.request_features(&pkg_id, &features, default_features);
        return Ok(());
    }
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

    let mut dep_refs = Vec::new();
    for dep in &record.deps {
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

fn normalize_registry(url: &str) -> String {
    if url.trim_end_matches('/') == "https://github.com/rust-lang/crates.io-index"
        || url.trim_end_matches('/') == "https://index.crates.io"
    {
        CRATES_IO.to_string()
    } else if url.starts_with("sparse+") || url.starts_with("registry+") {
        url.to_string()
    } else {
        format!("registry+{}", url.trim_end_matches('/'))
    }
}

fn git_package_dir(spec: &DependencySpec, url: &str, package: &str) -> Result<PathBuf> {
    let reference = rune_registry::git::GitRef::from_spec(
        spec.branch.as_deref(),
        spec.tag.as_deref(),
        spec.rev.as_deref(),
    );
    let checkout = rune_registry::git::checkout(url, &reference)?;
    Ok(rune_registry::git::find_package(&checkout, package)?)
}

/// Only the declared alias counts (`bytes_05 = { package = "bytes" }` is not `bytes`).
fn switched_on(features: &crate::features::ResolvedPackageFeatures, alias: &str) -> bool {
    features.activated_dependencies.iter().any(|d| d == alias)
        || features.active_features.iter().any(|f| f == alias)
}

/// Queues optional dependencies whose activating feature became active. O(graph size): once per BFS round.
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
            if !dep_ref.optional || !switched_on(feats, &dep_ref.name) {
                continue;
            }

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

/// Uses `cfg-expr`'s target database so every predicate (`target_endian`, ...) is evaluated.
pub fn matches_target(target_str: Option<&str>, target_triple: &str) -> bool {
    let Some(target) = target_str else {
        return true;
    };

    let target = target.trim();
    if target.is_empty() {
        return true;
    }

    let Some(info) = cfg_expr::targets::get_builtin_target_by_triple(target_triple) else {
        // Unknown triple: fail open, like Cargo.
        return true;
    };

    if !target.starts_with("cfg(") {
        return target == target_triple;
    }

    let expr_str = target
        .strip_prefix("cfg(")
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or(target);

    let Ok(expr) = cfg_expr::Expression::parse(expr_str) else {
        // Unparseable cfg: fail open.
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
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune_registry::IndexDependency;

    fn dep(name: &str, req: &str, optional: bool, kind: Option<&str>) -> IndexDependency {
        IndexDependency {
            name: name.into(),
            req: req.into(),
            features: vec![],
            optional,
            default_features: true,
            target: None,
            kind: kind.map(str::to_string),
            package: None,
            registry: None,
        }
    }

    fn record(vers: &str, deps: Vec<IndexDependency>, features: &[(&str, &[&str])]) -> IndexRecord {
        IndexRecord {
            name: "pkg".into(),
            vers: vers.into(),
            deps,
            cksum: String::new(),
            features: features
                .iter()
                .map(|(k, v)| (k.to_string(), v.iter().map(|s| s.to_string()).collect()))
                .collect(),
            features2: None,
            yanked: false,
            rust_version: None,
            pubtime: None,
        }
    }

    #[test]
    fn the_solver_moves_several_crates_at_once_or_explains_why_it_cannot() {
        let mut resolver = DependencyResolver::new().offline(true);
        let reg = CRATES_IO.to_string();
        let mut add = |name: &str, versions: Vec<IndexRecord>| {
            let versions = versions
                .into_iter()
                .map(|mut r| {
                    r.name = name.into();
                    r
                })
                .collect();
            resolver.cached_indices.insert(
                (reg.clone(), name.into()),
                CrateIndex {
                    name: name.into(),
                    versions,
                },
            );
        };
        add(
            "x",
            vec![
                record("1.0.0", vec![dep("z", "^1", false, None)], &[]),
                record("2.0.0", vec![dep("z", "=1.1.0", false, None)], &[]),
            ],
        );
        add(
            "y",
            vec![
                record("1.0.0", vec![dep("z", "^1", false, None)], &[]),
                record("2.0.0", vec![dep("z", "=1.0.0", false, None)], &[]),
            ],
        );
        add(
            "z",
            vec![record("1.0.0", vec![], &[]), record("1.1.0", vec![], &[])],
        );
        let star = VersionReq::STAR;
        let pinned = VersionReq::from_str("=2.0.0").unwrap();
        let reqs = |x: &'static VersionReq, y: &'static VersionReq| {
            vec![
                crate::solver::Requirement {
                    from: None,
                    registry: CRATES_IO,
                    name: "x",
                    req: x,
                },
                crate::solver::Requirement {
                    from: None,
                    registry: CRATES_IO,
                    name: "y",
                    req: y,
                },
            ]
        };
        let star: &'static VersionReq = Box::leak(Box::new(star));
        let pinned: &'static VersionReq = Box::leak(Box::new(pinned));
        let graph = ResolvedGraph::default();
        let v = |s: &str| Version::parse(s).unwrap();
        match crate::solver::solve(&mut resolver, &graph, &reqs(star, star)) {
            crate::solver::Solution::Found(mut chosen) => {
                chosen.sort();
                let z = chosen.iter().find(|(_, n, _)| n == "z").unwrap().2.clone();
                let x = chosen.iter().find(|(_, n, _)| n == "x").unwrap().2.clone();
                let y = chosen.iter().find(|(_, n, _)| n == "y").unwrap().2.clone();
                assert!(x != v("2.0.0") || z == v("1.1.0"));
                assert!(y != v("2.0.0") || z == v("1.0.0"));
                assert!(x == v("2.0.0") || y == v("2.0.0"), "keeps one newest");
            }
            crate::solver::Solution::Impossible(why) => panic!("{why}"),
        }
        match crate::solver::solve(&mut resolver, &graph, &reqs(pinned, pinned)) {
            crate::solver::Solution::Impossible(why) => {
                assert!(why.contains("x 2.0.0 depends on z 1.1.0"), "{why}");
                assert!(why.contains("y 2.0.0 depends on z 1.0.0"), "{why}");
            }
            crate::solver::Solution::Found(c) => panic!("{c:?}"),
        }
    }

    #[test]
    fn an_optional_dependency_is_switched_on_by_its_own_name_only() {
        let features = crate::features::ResolvedPackageFeatures {
            package_name: "combine".into(),
            active_features: vec!["default".into(), "std".into(), "bytes".into()],
            activated_dependencies: vec!["bytes".into(), "memchr".into()],
            cfg_flags: vec![],
        };
        assert!(switched_on(&features, "bytes"));
        assert!(switched_on(&features, "memchr"));
        assert!(!switched_on(&features, "bytes_05"));
        let features = crate::features::ResolvedPackageFeatures {
            activated_dependencies: vec!["bytes_05".into()],
            ..features
        };
        assert!(switched_on(&features, "bytes_05"));
    }

    #[test]
    fn the_prefetch_follows_the_version_resolution_would_pick() {
        let index = CrateIndex {
            name: "pkg".into(),
            versions: vec![
                record("1.0.0", vec![dep("old", "1", false, None)], &[]),
                record(
                    "1.2.0",
                    vec![
                        dep("always", "^2", false, None),
                        dep("builder", "1", false, Some("build")),
                        dep("tests_only", "1", false, Some("dev")),
                        dep("by_default", "1", true, None),
                        dep("via_feature", "1", true, None),
                        dep("weak", "1", true, None),
                        dep("never", "1", true, None),
                    ],
                    &[
                        ("default", &["std", "dep:by_default"]),
                        ("std", &["via_feature/std", "weak?/std"]),
                        ("extra", &["never"]),
                    ],
                ),
            ],
        };
        let names = |deps: Vec<(Option<String>, String, VersionReq)>| {
            let mut n: Vec<String> = deps.into_iter().map(|(_, n, _)| n).collect();
            n.sort();
            n
        };
        assert_eq!(
            names(likely_dependencies(&index, &VersionReq::STAR, None)),
            ["always", "builder", "by_default", "via_feature"]
        );
        let locked = [Version::parse("1.0.0").unwrap()];
        assert_eq!(
            names(likely_dependencies(
                &index,
                &VersionReq::STAR,
                Some(&locked)
            )),
            ["old"]
        );
        let req = VersionReq::parse("^3").unwrap();
        assert!(likely_dependencies(&index, &req, None).is_empty());
    }
}
