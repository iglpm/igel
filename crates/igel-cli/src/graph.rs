//! Turns a workspace plus a build request into a [`BuildGraph`]: resolve versions for the whole workspace
//! (like `Cargo.lock`), resolve features for this request, download what's needed, then plan units.

use anyhow::{Context, Result, bail};
use colored::Colorize;
use igel_common::timing::phase;
use igel_common::toolchain::{self, Role, TargetInfo};
use igel_common::{
    CompilationUnit, CrateId, CrateSource, CrateType, Edition, PackageMeta, Profile, TargetKind,
    TargetTriple, UnitExtras,
};
use igel_graph::{BuildGraph, NodeId};
use igel_manifest::dependency::{DependencyKind, DependencySpec};
use igel_manifest::{Manifest, TargetSpec, UnitRole, resolve_unit_profile};
use igel_resolver::lockfile::{Lockfile, LockfileManager};
use igel_resolver::resolver::{DependencyResolver, PackageSource, ResolvedGraph, ResolvedPackage};
use igel_resolver::unit_features::{
    self, DepDecl, FeatureOptions, FeatureResolution, PackageView, RootRequest,
};
use rayon::prelude::*;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::ui;
use crate::workspace::{Member, Workspace};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Build,
    Check,
    Test,
    Bench,
}

#[derive(Debug, Clone, Default)]
pub struct FeatureArgs {
    pub features: Vec<String>,
    pub all_features: bool,
    pub no_default_features: bool,
}

impl FeatureArgs {
    pub fn parse(raw: &[String], all: bool, no_default: bool) -> Self {
        Self {
            features: raw
                .iter()
                .flat_map(|s| s.split([',', ' ']))
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
            all_features: all,
            no_default_features: no_default,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct TargetSelection {
    pub lib: bool,
    pub bins: bool,
    pub bin: Vec<String>,
    pub examples: bool,
    pub example: Vec<String>,
    pub tests: bool,
    pub test: Vec<String>,
    pub benches: bool,
    pub bench: Vec<String>,
    pub all_targets: bool,
    pub doc_only: bool,
}

impl TargetSelection {
    fn any_explicit(&self) -> bool {
        self.lib
            || self.bins
            || !self.bin.is_empty()
            || self.examples
            || !self.example.is_empty()
            || self.tests
            || !self.test.is_empty()
            || self.benches
            || !self.bench.is_empty()
            || self.all_targets
            || self.doc_only
    }
}

pub struct Request<'a> {
    pub profile: &'a Profile,
    pub target: Option<&'a str>,
    pub mode: Mode,
    pub selected: &'a [usize],
    pub features: &'a FeatureArgs,
    pub targets: &'a TargetSelection,
    pub offline: bool,
    pub locked: bool,
    pub doctests: bool,
    pub ignore_rust_version: bool,
}

#[derive(Debug, Clone)]
pub struct Requested {
    pub node: NodeId,
    pub member: usize,
    pub kind: TargetKind,
    pub harness: bool,
    pub target: String,
}

#[derive(Debug, Clone)]
pub struct DocTest {
    pub member: usize,
    pub lib_source: PathBuf,
    pub crate_name: String,
    pub externs: Vec<(String, NodeId)>,
    pub features: Vec<String>,
    pub edition: Edition,
    pub proc_macro: bool,
}

pub struct Plan {
    pub graph: BuildGraph,
    pub requested: Vec<Requested>,
    pub doc_tests: Vec<DocTest>,
}

struct Locals {
    list: Vec<Member>,
    member_of: Vec<Option<usize>>,
    by_dir: HashMap<PathBuf, usize>,
    foreign_ws: Vec<Option<(PathBuf, igel_manifest::WorkspaceManifest)>>,
    git_of: Vec<Option<igel_registry::git::GitCheckout>>,
}

impl Locals {
    fn member_ws<'a>(
        &'a self,
        ws: &'a Workspace,
        li: usize,
    ) -> Option<&'a igel_manifest::WorkspaceManifest> {
        match &self.foreign_ws[li] {
            Some((_, w)) => Some(w),
            None => self.member_of[li].and(ws.workspace_manifest()),
        }
    }

    fn ws_root<'a>(&'a self, ws: &'a Workspace, li: usize) -> &'a Path {
        match &self.foreign_ws[li] {
            Some((root, _)) => root,
            None => ws.root(),
        }
    }

    fn spec_for(
        &self,
        ws: &Workspace,
        li: usize,
        dep: &igel_manifest::Dependency,
    ) -> DependencySpec {
        igel_manifest::effective_spec(
            dep,
            &self.list[li].dir,
            self.ws_root(ws, li),
            self.member_ws(ws, li),
        )
    }

    fn load(ws: &Workspace) -> Result<Self> {
        let n = ws.members.len();
        let mut this = Locals {
            list: ws.members.clone(),
            member_of: (0..n).map(Some).collect(),
            by_dir: ws
                .members
                .iter()
                .enumerate()
                .map(|(i, m)| (m.dir.clone(), i))
                .collect(),
            foreign_ws: vec![None; n],
            git_of: vec![None; n],
        };

        let mut i = 0;
        while i < this.list.len() {
            let is_member = this.member_of[i].is_some();
            let specs: Vec<(String, DependencySpec)> = this.list[i]
                .manifest
                .dependencies
                .values()
                .map(|dep| (dep.name.clone(), this.spec_for(ws, i, dep)))
                .collect();
            for (name, spec) in specs {
                if let Some(path) = spec.path {
                    let dir = igel_common::fsutil::canonicalize(&path).unwrap_or(path);
                    if !this.by_dir.contains_key(&dir)
                        && !igel_common::manifest_file::has_manifest(&dir)
                    {
                        bail!(
                            "path dependency `{}` of `{}` does not contain a Cargo.toml or Igel.toml",
                            dir.display(),
                            this.list[i].name
                        );
                    }
                    this.add(dir, None)?;
                } else if let (Some(url), true) = (&spec.git, is_member || !spec.optional) {
                    let real = spec.package.clone().unwrap_or(name);
                    let req = spec.version.clone().unwrap_or(semver::VersionReq::STAR);
                    if let Some(dir) = ws
                        .patches
                        .git_patch_dir(url, &real, &req)
                        .map_err(|e| anyhow::anyhow!("{e}"))?
                    {
                        let dir = igel_common::fsutil::canonicalize(&dir).unwrap_or(dir);
                        let git = igel_registry::git::locate_dir(&dir);
                        this.add(dir, git)?;
                        continue;
                    }
                    let reference = igel_registry::git::GitRef::from_spec(
                        spec.branch.as_deref(),
                        spec.tag.as_deref(),
                        spec.rev.as_deref(),
                    );
                    let co = igel_registry::git::checkout(url, &reference)
                        .with_context(|| format!("git dependency `{real}` ({url})"))?;
                    let dir = igel_registry::git::find_package(&co, &real)?;
                    let dir = igel_common::fsutil::canonicalize(&dir).unwrap_or(dir);
                    this.add(dir, Some(co))?;
                } else {
                    let real = spec.package.clone().unwrap_or(name);
                    let req = spec.version.clone().unwrap_or(semver::VersionReq::STAR);
                    let source = match spec.registry.as_deref() {
                        None | Some("crates-io") => igel_resolver::PatchSource::CratesIo,
                        Some(name) => igel_resolver::PatchSource::Registry(name.to_string()),
                    };
                    for entry in ws.patches.find(&real, &source) {
                        if let Some((dir, _)) =
                            entry.locate(&req).map_err(|e| anyhow::anyhow!("{e}"))?
                        {
                            let dir = igel_common::fsutil::canonicalize(&dir).unwrap_or(dir);
                            let git = igel_registry::git::locate_dir(&dir);
                            this.add(dir, git)?;
                            break;
                        }
                    }
                }
            }
            i += 1;
        }
        Ok(this)
    }

    fn add(&mut self, dir: PathBuf, git: Option<igel_registry::git::GitCheckout>) -> Result<usize> {
        if let Some(&li) = self.by_dir.get(&dir) {
            return Ok(li);
        }
        let manifest_path = igel_common::manifest_file::manifest_in(&dir);
        let manifest = crate::workspace::read_manifest(&manifest_path)?;
        let enclosing = igel_manifest::find_enclosing_workspace(&dir, &manifest);
        let member = Member::load(&dir, enclosing.as_ref().map(|(_, w)| w))?;
        let li = self.list.len();
        self.by_dir.insert(dir, li);
        self.list.push(member);
        self.member_of.push(None);
        self.foreign_ws.push(enclosing);
        self.git_of.push(git);
        Ok(li)
    }

    fn extend_from_lock(&mut self, ws: &Workspace, lock: &Lockfile) -> Result<()> {
        for p in lock.packages.iter().filter(|p| p.source.is_none()) {
            if self
                .list
                .iter()
                .any(|m| m.name == p.name && m.version.to_string() == p.version)
            {
                continue;
            }
            let req = semver::VersionReq::parse(&format!("={}", p.version))
                .unwrap_or(semver::VersionReq::STAR);
            for entry in ws
                .patches
                .find(&p.name, &igel_resolver::PatchSource::CratesIo)
            {
                if let Some((dir, _)) = entry.locate(&req).map_err(|e| anyhow::anyhow!("{e}"))? {
                    let dir = igel_common::fsutil::canonicalize(&dir).unwrap_or(dir);
                    let git = igel_registry::git::locate_dir(&dir);
                    self.add(dir, git)?;
                    break;
                }
            }
        }
        for p in &lock.packages {
            let Some((url, query, _)) = p
                .source
                .as_deref()
                .and_then(igel_registry::git::parse_lock_source)
            else {
                continue;
            };
            if self
                .list
                .iter()
                .any(|m| m.name == p.name && m.version.to_string() == p.version)
            {
                continue;
            }
            let reference = igel_registry::git::GitRef::from_query(query.as_deref());
            let co = igel_registry::git::checkout(&url, &reference)
                .with_context(|| format!("git dependency `{}` ({url})", p.name))?;
            let dir = igel_registry::git::find_package(&co, &p.name)?;
            let dir = igel_common::fsutil::canonicalize(&dir).unwrap_or(dir);
            self.add(dir, Some(co))?;
        }
        Ok(())
    }
}

fn configure_git(ws: &Workspace, offline: bool, update: bool, lock: Option<&Lockfile>) {
    let disk;
    let lock = match lock {
        Some(l) => Some(l),
        None => {
            disk = LockfileManager::load_fast(ws.root());
            disk.as_ref()
        }
    };
    let pins = lock
        .map(|l| {
            l.packages
                .iter()
                .filter_map(|p| {
                    p.source
                        .as_deref()
                        .and_then(igel_registry::git::parse_lock_source)
                })
                .collect()
        })
        .unwrap_or_default();
    igel_registry::git::configure(igel_registry::git::GitOptions {
        offline: offline || ws.config.settings.offline(),
        update,
        pins,
    });
}

fn features_for_member(
    member: &Member,
    selected: bool,
    single_selected: bool,
    args: &FeatureArgs,
) -> Result<(Vec<String>, bool)> {
    if !selected {
        return Ok((Vec::new(), true));
    }
    let default_features = !args.no_default_features;
    if args.all_features {
        let mut all: Vec<String> = member.manifest.features.keys().cloned().collect();
        all.extend(
            member
                .manifest
                .dependencies
                .values()
                .filter(|d| d.spec.optional)
                .map(|d| d.name.clone()),
        );
        all.sort();
        all.dedup();
        return Ok((all, default_features));
    }

    let mut out = Vec::new();
    for f in &args.features {
        match f.split_once('/') {
            Some((pkg, feat)) if pkg.trim_end_matches('?') == member.name => {
                out.push(feat.to_string());
            }
            Some((pkg, _))
                if member
                    .manifest
                    .dependencies
                    .values()
                    .any(|d| d.name == pkg.trim_end_matches('?')) =>
            {
                out.push(f.clone());
            }
            Some(_) => {} // aimed at another package
            None => {
                let declared = member.manifest.features.contains_key(f)
                    || member
                        .manifest
                        .dependencies
                        .values()
                        .any(|d| d.spec.optional && d.name == *f);
                if declared {
                    out.push(f.clone());
                } else if single_selected {
                    bail!(
                        "the package `{}` does not contain the feature `{f}`",
                        member.name
                    );
                }
            }
        }
    }
    Ok((out, default_features))
}

fn resolution_roots(
    ws: &Workspace,
    locals: &Locals,
    _req: &Request,
) -> Result<Vec<(String, DependencySpec)>> {
    let mut roots = Vec::new();
    // Covers every feature of every member, like `Cargo.lock`, so it doesn't change with the command line.
    for m in &ws.members {
        let mut all: Vec<String> = m.manifest.features.keys().cloned().collect();
        all.extend(
            m.manifest
                .dependencies
                .values()
                .filter(|d| d.spec.optional)
                .map(|d| d.name.clone()),
        );
        all.sort();
        all.dedup();
        roots.push((
            m.name.clone(),
            DependencySpec {
                path: Some(m.dir.clone()),
                features: all,
                default_features: true,
                ..Default::default()
            },
        ));
    }
    // Dev-dependencies are always roots, so one resolution serves every command.
    for (li, m) in locals.list.iter().enumerate() {
        for dep in m.manifest.dependencies.values() {
            if dep.kind != DependencyKind::Dev {
                continue;
            }
            let spec = locals.spec_for(ws, li, dep);
            let real = spec.package.clone().unwrap_or_else(|| dep.name.clone());
            roots.push((real, spec));
        }
    }
    Ok(roots)
}

fn msrv_for_resolution(ws: &Workspace) -> Option<semver::Version> {
    if ws.root_manifest.resolver_version() < 3 {
        return None;
    }
    let lowest = ws
        .members
        .iter()
        .filter_map(|m| igel_common::toolchain::parse_rust_version(&m.meta.rust_version))
        .min();
    lowest.or_else(|| Some(toolchain::global().version.clone()))
}

fn resolution_fingerprint(ws: &Workspace, locals: &Locals, _req: &Request) -> String {
    with_cargo_lock(&resolution_inputs(ws, locals), ws.root())
}

/// Everything a resolution is made from except `Cargo.lock`: manifests (read before resolving), config,
/// settings and the exact `igl` binary.
fn resolution_inputs(ws: &Workspace, locals: &Locals) -> String {
    let mut hasher = igel_common::hash::HashBuilder::new();
    hasher.update(env!("CARGO_PKG_VERSION").as_bytes());
    if let Some(meta) = std::env::current_exe()
        .ok()
        .and_then(|e| fs::metadata(e).ok())
    {
        hasher.update(&meta.len().to_le_bytes());
        if let Ok(t) = meta.modified()
            && let Ok(d) = t.duration_since(std::time::UNIX_EPOCH)
        {
            hasher.update(&d.as_nanos().to_le_bytes());
        }
    }
    let mut paths: Vec<PathBuf> = vec![ws.manifest_path()];
    paths.extend(locals.list.iter().map(|m| m.manifest_path.clone()));
    paths.extend(ws.cargo_config.files.iter().cloned());
    paths.push(ws.root().join(igel_common::settings::PROJECT_FILE));
    paths.sort();
    paths.dedup();
    for path in paths {
        hasher.update(path.to_string_lossy().as_bytes());
        if let Ok(contents) = fs::read(&path) {
            hasher.update(&contents);
        }
    }
    hasher.update(format!("{:?}", ws.config.settings.min_release_age()).as_bytes());
    hasher.finish_blake3()
}

fn with_cargo_lock(inputs: &str, root: &Path) -> String {
    let mut hasher = igel_common::hash::HashBuilder::new();
    hasher.update(inputs.as_bytes());
    if let Ok(contents) = fs::read(root.join("Cargo.lock")) {
        hasher.update(b"\0Cargo.lock\0");
        hasher.update(&contents);
    }
    hasher.finish_blake3()
}

fn pigel_resolved(mut resolved: ResolvedGraph, refined: &Lockfile) -> ResolvedGraph {
    let kept: HashMap<String, &igel_resolver::LockPackage> = refined
        .packages
        .iter()
        .map(|p| (format!("{} {}", p.name, p.version), p))
        .collect();
    resolved.packages.retain(|id, _| kept.contains_key(id));
    let snapshot = resolved.clone();
    for (id, pkg) in resolved.packages.iter_mut() {
        let edges: HashSet<&String> = kept[id].dependencies.iter().collect();
        pkg.dependencies.retain(|d| {
            let real = d.package.as_deref().unwrap_or(&d.name);
            snapshot
                .find_matching(real, &d.req)
                .is_some_and(|t| edges.contains(&format!("{} {}", t.name, t.version)))
        });
        pkg.features = kept[id].features.clone();
    }
    resolved
}

/// The resolved graph as a build of every member sees it on `target` — what `igl tree` shows.
pub fn tree_view(ws: &Workspace, offline: bool, target: Option<&str>) -> Result<ResolvedGraph> {
    let existing = LockfileManager::load_fast(ws.root());
    let (resolved, lock) = resolve_fresh(ws, offline, existing.as_ref(), false)?;
    let mut locals = Locals::load(ws)?;
    locals.extend_from_lock(ws, &lock)?;
    let uni = Universe::build(ws, &locals, &lock);
    let roots: Vec<RootRequest> = ws
        .members
        .iter()
        .enumerate()
        .filter_map(|(mi, _)| locals.member_of.iter().position(|m| *m == Some(mi)))
        .map(|li| RootRequest {
            package: li,
            features: Vec::new(),
            default_features: true,
            dev: true,
        })
        .collect();
    let host = igel_common::toolchain::global().host.clone();
    let applies = |platform: &str, for_host: bool| match target {
        None => true,
        Some(t) => igel_resolver::resolver::matches_target(
            Some(platform),
            if for_host { &host } else { t },
        ),
    };
    let res = unit_features::resolve(
        &uni.views,
        &roots,
        &FeatureOptions {
            resolver: ws.root_manifest.resolver_version(),
            platform_matches: &applies,
            weak_activates: false,
        },
    );
    let mut features: HashMap<String, BTreeSet<String>> = HashMap::new();
    for ((pkg, _), feats) in &res.features {
        features
            .entry(uni.id[*pkg].clone())
            .or_default()
            .extend(feats.iter().cloned());
    }
    let mut edges: HashSet<(String, String, igel_manifest::dependency::DependencyKind)> =
        HashSet::new();
    for e in &res.edges {
        let d = &uni.views[e.from.0].deps[e.dep];
        edges.insert((uni.id[e.from.0].clone(), d.alias.clone(), d.kind));
    }
    let mut view = resolved;
    view.packages.retain(|id, _| features.contains_key(id));
    for (id, pkg) in view.packages.iter_mut() {
        pkg.dependencies
            .retain(|d| edges.contains(&(id.clone(), d.name.clone(), d.kind)));
        pkg.features = features[id].iter().cloned().collect();
    }
    Ok(view)
}

/// Tightens a fresh lockfile to exactly what Cargo would lock: the version resolver over-approximates
/// optional dependencies, so rerun the precise feature resolver and keep what it reaches.
fn refine_lock(ws: &Workspace, locals: &Locals, lock: &Lockfile) -> Lockfile {
    let uni = Universe::build(ws, locals, lock);
    let mut roots = Vec::new();
    for (mi, m) in ws.members.iter().enumerate() {
        let Some(li) = locals.member_of.iter().position(|x| *x == Some(mi)) else {
            continue;
        };
        let mut all: Vec<String> = m.manifest.features.keys().cloned().collect();
        all.extend(
            m.manifest
                .dependencies
                .values()
                .filter(|d| d.spec.optional)
                .map(|d| d.name.clone()),
        );
        roots.push(RootRequest {
            package: li,
            features: all,
            default_features: true,
            dev: true,
        });
    }
    let res = unit_features::resolve(
        &uni.views,
        &roots,
        &FeatureOptions {
            resolver: 1,
            platform_matches: &|_, _| true,
            weak_activates: true,
        },
    );

    let mut children: HashMap<usize, BTreeSet<String>> = HashMap::new();
    for e in &res.edges {
        children
            .entry(e.from.0)
            .or_default()
            .insert(uni.id[e.to.0].clone());
    }
    let mut out = Lockfile::new();
    out.version = lock.version;
    for (&(pkg, _), feats) in &res.features {
        let original = match uni.origin[pkg] {
            Origin::Registry => uni.lock_pkg[pkg].map(|i| &lock.packages[i]),
            Origin::Local(li) => {
                let m = &locals.list[li];
                lock.packages.iter().find(|p| {
                    p.name == m.name
                        && p.version == m.version.to_string()
                        && !is_registry_source(p.source.as_deref())
                })
            }
        };
        let Some(original) = original else { continue };
        let mut p = original.clone();
        p.dependencies = children
            .get(&pkg)
            .map(|c| c.iter().cloned().collect())
            .unwrap_or_default();
        p.features = feats.clone();
        out.add_or_replace(p);
    }
    out
}

/// Keeps `Cargo.lock` in step like Cargo: created if missing, rewritten only when the set of locked
/// packages changes. With `locked`, a needed change is an error.
pub fn sync_cargo_lock(ws: &Workspace, lock: &Lockfile, locked: bool) -> Result<bool> {
    if !ws.config.settings.write_cargo_lock() {
        return Ok(false);
    }
    let path = ws.root().join("Cargo.lock");
    let existing = if path.is_file() {
        match LockfileManager::read_cargo(&path) {
            Ok(l) => Some(l),
            Err(e) => {
                ui::warn(format!(
                    "leaving {} untouched: it could not be read ({e})",
                    path.display()
                ));
                return Ok(false);
            }
        }
    } else {
        None
    };
    let identity = |l: &Lockfile| -> BTreeSet<(String, String, Option<String>, Option<String>)> {
        l.packages
            .iter()
            .map(|p| {
                (
                    p.name.clone(),
                    p.version.clone(),
                    p.source.clone(),
                    p.checksum.clone(),
                )
            })
            .collect()
    };
    if existing.as_ref().map(identity) == Some(identity(lock)) {
        return Ok(false);
    }
    if locked {
        bail!(
            "the lock file {} needs to be updated but `--locked` was passed to prevent this",
            path.display()
        );
    }
    let mut out = lock.clone();
    // A new file gets format 4, or 3 if `rust-version` predates Rust 1.83.
    let newest = if toolchain::global().satisfies_msrv("1.83") {
        4
    } else {
        3
    };
    let lowest_rust = ws
        .members
        .iter()
        .filter_map(|m| igel_common::toolchain::parse_rust_version(&m.meta.rust_version))
        .min();
    let for_msrv = match lowest_rust {
        Some(v) if v < semver::Version::new(1, 83, 0) => 3,
        _ => newest,
    };
    out.version = existing
        .as_ref()
        .map_or(newest.min(for_msrv), |e| e.version.max(1));
    LockfileManager::write_cargo(&out, &path).context("Failed to write Cargo.lock")?;
    ui::action(
        if existing.is_some() {
            "updated"
        } else {
            "created"
        },
        "Cargo.lock",
    );
    Ok(true)
}

fn is_registry_source(source: Option<&str>) -> bool {
    source.is_some_and(|s| s.starts_with("registry+") || s.starts_with("sparse+"))
}

pub fn index_of_lock_source(source: Option<&str>) -> String {
    match source {
        Some("registry+https://github.com/rust-lang/crates.io-index") | None => {
            igel_common::CRATES_IO_INDEX.to_string()
        }
        Some(s) => s.to_string(),
    }
}

fn resolved_from_lock(lock: &Lockfile) -> ResolvedGraph {
    let mut packages = HashMap::new();
    for pkg in &lock.packages {
        let Ok(version) = pkg.version.parse::<semver::Version>() else {
            continue;
        };
        let is_registry = is_registry_source(pkg.source.as_deref());
        packages.insert(
            format!("{} {}", pkg.name, version),
            ResolvedPackage {
                name: pkg.name.clone(),
                version,
                source: if is_registry {
                    PackageSource::Registry(index_of_lock_source(pkg.source.as_deref()))
                } else {
                    PackageSource::Path(PathBuf::new())
                },
                checksum: pkg.checksum.clone(),
                dependencies: Vec::new(),
                features: pkg.features.clone(),
                declared_features: pkg.feature_table.iter().cloned().collect(),
            },
        );
    }
    ResolvedGraph {
        root_packages: Vec::new(),
        packages,
    }
}

/// The resolution in `Igel.lock.bin`, when its fingerprint matches and it covers every declared dependency.
fn cached_resolution(ws: &Workspace) -> Option<(ResolvedGraph, Lockfile)> {
    let _timer = phase("resolve (cached)");
    let (lock, Some(stored)) =
        LockfileManager::read_binary_with_fingerprint(&ws.root().join("Igel.lock.bin")).ok()?
    else {
        return None;
    };
    if lock.packages.is_empty() {
        return None;
    }
    let mut locals = Locals::load(ws).ok()?;
    if stored != with_cargo_lock(&resolution_inputs(ws, &locals), ws.root()) {
        return None;
    }
    locals.extend_from_lock(ws, &lock).ok()?;
    let uni = Universe::build(ws, &locals, &lock);
    if !incomplete(&uni, &locals, &lock, &|_| true).is_empty() {
        return None;
    }
    let mut packages = HashMap::new();
    for idx in 0..uni.views.len() {
        let view = &uni.views[idx];
        let (source, checksum, features) = match uni.origin[idx] {
            Origin::Registry => {
                let lp = &lock.packages[uni.lock_pkg[idx]?];
                (
                    PackageSource::Registry(index_of_lock_source(lp.source.as_deref())),
                    lp.checksum.clone(),
                    lp.features.clone(),
                )
            }
            Origin::Local(li) => {
                let m = &locals.list[li];
                let lp = lock.packages.iter().find(|p| {
                    p.name == m.name
                        && p.version == m.version.to_string()
                        && !is_registry_source(p.source.as_deref())
                });
                let source = match &locals.git_of[li] {
                    Some(co) => {
                        let reference = lp
                            .and_then(|p| p.source.as_deref())
                            .and_then(|s| s.split_once('?'))
                            .and_then(|(_, rest)| rest.split('#').next())
                            .map(str::to_string);
                        PackageSource::Git {
                            url: co.url.clone(),
                            reference,
                            commit: co.commit.clone(),
                        }
                    }
                    None => PackageSource::Path(m.dir.clone()),
                };
                (
                    source,
                    None,
                    lp.map(|p| p.features.clone()).unwrap_or_default(),
                )
            }
        };
        let recorded: Option<HashSet<&String>> = match uni.origin[idx] {
            Origin::Registry => Some(
                lock.packages[uni.lock_pkg[idx]?]
                    .dependencies
                    .iter()
                    .collect(),
            ),
            Origin::Local(li) => {
                let m = &locals.list[li];
                lock.packages
                    .iter()
                    .find(|p| {
                        p.name == m.name
                            && p.version == m.version.to_string()
                            && !is_registry_source(p.source.as_deref())
                    })
                    .map(|p| p.dependencies.iter().collect())
            }
        };
        let dependencies = view
            .deps
            .iter()
            .filter_map(|d| {
                let t = d.resolved?;
                if recorded.as_ref().is_some_and(|r| !r.contains(&uni.id[t])) {
                    return None;
                }
                Some(igel_resolver::resolver::ResolvedDependencyRef {
                    name: d.alias.clone(),
                    req: format!("={}", uni.version[t]),
                    kind: d.kind,
                    optional: d.optional,
                    features: d.features.clone(),
                    default_features: d.default_features,
                    package: d.renamed.then(|| uni.name[t].clone()),
                    target: d.target.clone(),
                    registry: None,
                    path: None,
                })
            })
            .collect();
        packages.insert(
            uni.id[idx].clone(),
            ResolvedPackage {
                name: uni.name[idx].clone(),
                version: uni.version[idx].clone(),
                source,
                checksum,
                dependencies,
                features,
                declared_features: view
                    .features
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            },
        );
    }
    Some((
        ResolvedGraph {
            root_packages: ws.members.iter().map(|m| m.name.clone()).collect(),
            packages,
        },
        lock,
    ))
}

pub fn whole_workspace_request<'a>(
    profile: &'a Profile,
    offline: bool,
    features: &'a FeatureArgs,
    targets: &'a TargetSelection,
    selected: &'a [usize],
) -> Request<'a> {
    Request {
        profile,
        target: None,
        mode: Mode::Build,
        selected,
        features,
        targets,
        offline,
        locked: false,
        doctests: false,
        ignore_rust_version: false,
    }
}

pub fn resolve_fresh(
    ws: &Workspace,
    offline: bool,
    existing: Option<&Lockfile>,
    refresh: bool,
) -> Result<(ResolvedGraph, Lockfile)> {
    resolve_fresh_with(ws, offline, existing, refresh, &[])
}

pub fn resolve_fresh_with(
    ws: &Workspace,
    offline: bool,
    existing: Option<&Lockfile>,
    refresh: bool,
    preferred: &[(String, String, semver::Version)],
) -> Result<(ResolvedGraph, Lockfile)> {
    // The lockfile on disk was made from these manifests: it is the resolution.
    let from_disk = !refresh
        && preferred.is_empty()
        && LockfileManager::load_fast(ws.root()).as_ref() == existing;
    if from_disk && let Some(cached) = cached_resolution(ws) {
        return Ok(cached);
    }
    let (resolved, lock) = resolve_uncached(ws, offline, existing, refresh, preferred)?;
    if from_disk {
        let _ = store_resolution(ws, &lock);
    }
    Ok((resolved, lock))
}

fn resolve_uncached(
    ws: &Workspace,
    offline: bool,
    existing: Option<&Lockfile>,
    refresh: bool,
    preferred: &[(String, String, semver::Version)],
) -> Result<(ResolvedGraph, Lockfile)> {
    let _timer = phase("resolve (fresh)");
    let _activity = ui::Activity::start(
        "resolving",
        if refresh {
            "checking the index for new releases"
        } else {
            "dependency graph"
        },
    );
    let t = phase("configure git");
    configure_git(ws, offline, refresh, existing);
    drop(t);
    let t = phase("load locals");
    let locals = Locals::load(ws)?;
    drop(t);
    let profile = Profile::dev();
    let features = FeatureArgs::default();
    let targets = TargetSelection::default();
    let selected: Vec<usize> = (0..ws.members.len()).collect();
    let req = whole_workspace_request(&profile, offline, &features, &targets, &selected);
    let roots = resolution_roots(ws, &locals, &req)?;

    let on_disk = existing
        .is_none()
        .then(|| LockfileManager::load_fast(ws.root()))
        .flatten();
    let mut resolver = if refresh {
        DependencyResolver::refreshing_resolver()
    } else {
        DependencyResolver::default_resolver()
    }
    .offline(offline)
    .with_patches(ws.patches.clone())
    .with_msrv(msrv_for_resolution(ws))
    .with_min_release_age(ws.config.settings.min_release_age())
    .with_prefetch_hint(on_disk.as_ref())
    .with_preferred(preferred);
    let t = phase("resolve walk");
    let resolved = resolver
        .resolve_with_workspace(ws.root(), &roots, existing, ws.workspace_manifest())
        .context("Dependency resolution failed")?;
    drop(t);
    let t = phase("refine lock");
    let lock = refine_lock(ws, &locals, &resolved.to_lockfile());
    drop(t);
    let t = phase("prune");
    let resolved = pigel_resolved(resolved, &lock);
    drop(t);
    warn_unused_patches(ws, &resolved);
    Ok((resolved, lock))
}

fn warn_unused_patches(ws: &Workspace, resolved: &ResolvedGraph) {
    for entry in ws.patches.entries() {
        let name = entry.spec.package.as_deref().unwrap_or(&entry.name);
        let used = resolved
            .packages
            .values()
            .any(|p| p.name == name && !matches!(p.source, PackageSource::Registry(_)));
        if !used {
            let from = entry
                .spec
                .path
                .as_ref()
                .map(|p| p.display().to_string())
                .or_else(|| entry.spec.git.clone())
                .unwrap_or_default();
            ui::warn(format!(
                "patch `{name}` ({from}) was not used in the crate graph; check that its \
                 version matches what the graph requires (`igl tree -i {name}`)"
            ));
        }
    }
}

pub fn store_resolution(ws: &Workspace, lock: &Lockfile) -> Result<()> {
    let locals = Locals::load(ws)?;
    let profile = Profile::dev();
    let features = FeatureArgs::default();
    let targets = TargetSelection::default();
    let selected: Vec<usize> = (0..ws.members.len()).collect();
    let req = whole_workspace_request(&profile, false, &features, &targets, &selected);
    sync_cargo_lock(ws, lock, false)?;
    let fingerprint = resolution_fingerprint(ws, &locals, &req);
    LockfileManager::sync_all_with_fingerprint(ws.root(), lock, Some(&fingerprint))
        .context("Failed to write lockfiles")?;
    Ok(())
}

pub fn download_all(resolved: &ResolvedGraph, offline: bool) -> Result<usize> {
    let pkgs: Vec<(String, String, Option<String>, String)> = resolved
        .packages
        .values()
        .filter_map(|p| match &p.source {
            PackageSource::Registry(index) => Some((
                p.name.clone(),
                p.version.to_string(),
                p.checksum.clone(),
                index.clone(),
            )),
            _ => None,
        })
        .collect();
    igel_registry::download_and_extract_grouped(&pkgs, offline)
        .context("Failed to download one or more registry crates")?;
    Ok(pkgs.len())
}

fn resolve_with(
    ws: &Workspace,
    locals: &Locals,
    req: &Request,
    trust_cache: bool,
) -> Result<(ResolvedGraph, Lockfile)> {
    let root = ws.root();
    let inputs = resolution_inputs(ws, locals);
    let fingerprint = with_cargo_lock(&inputs, root);

    // Fast path: nothing changed since the last resolution.
    if trust_cache
        && let Ok((lock, Some(stored))) =
            LockfileManager::read_binary_with_fingerprint(&root.join("Igel.lock.bin"))
        && stored == fingerprint
        && !lock.packages.is_empty()
    {
        if !root.join("Cargo.lock").is_file() && sync_cargo_lock(ws, &lock, req.locked)? {
            let _ = LockfileManager::sync_all_with_fingerprint(
                root,
                &lock,
                Some(&with_cargo_lock(&inputs, root)),
            );
        }
        return Ok((resolved_from_lock(&lock), lock));
    }

    // `Cargo.lock` seeds version choices so adopting Igel doesn't move any dependency.
    let existing_lock = LockfileManager::load_fast(root);
    let roots = resolution_roots(ws, locals, req)?;

    let activity = ui::Activity::start("resolving", "dependency graph");
    let _timer = phase("resolve");
    let mut resolver = DependencyResolver::default_resolver()
        .offline(req.offline)
        .with_patches(ws.patches.clone())
        .with_msrv(msrv_for_resolution(ws))
        .with_min_release_age(ws.config.settings.min_release_age());
    let resolved = resolver
        .resolve_with_workspace(
            root,
            &roots,
            existing_lock.as_ref(),
            ws.workspace_manifest(),
        )
        .context("Dependency resolution failed")?;

    let lockfile = refine_lock(ws, locals, &resolved.to_lockfile());
    let resolved = pigel_resolved(resolved, &lockfile);
    let fetched = activity.fetched();
    let took = activity.elapsed();
    activity.finish(
        "resolved",
        format!(
            "{} {}",
            ui::plural(lockfile.packages.len(), "package"),
            if fetched > 0 {
                format!(
                    "({}, {})",
                    ui::plural(fetched, "index file") + " fetched",
                    ui::secs(took)
                )
            } else {
                format!("({})", ui::secs(took))
            }
            .dimmed()
        ),
    );
    warn_unused_patches(ws, &resolved);
    if req.locked
        && existing_lock.as_ref().map(locked_registry_packages)
            != Some(locked_registry_packages(&lockfile))
    {
        bail!(
            "the lock file needs to be updated but `--locked` was passed to prevent this \
             (run `igl update` first)"
        );
    }
    sync_cargo_lock(ws, &lockfile, req.locked)?;
    // Fingerprint after `Cargo.lock` was written, but from the manifests as they were before resolving.
    let fingerprint = with_cargo_lock(&inputs, root);
    LockfileManager::sync_all_with_fingerprint(root, &lockfile, Some(&fingerprint))
        .context("Failed to write lockfiles")?;

    Ok((resolved, lockfile))
}

fn locked_registry_packages(lock: &Lockfile) -> BTreeMap<(String, String), Option<String>> {
    lock.packages
        .iter()
        .filter(|p| is_registry_source(p.source.as_deref()))
        .map(|p| ((p.name.clone(), p.version.clone()), p.checksum.clone()))
        .collect()
}

struct Pick {
    kind: TargetKind,
    name: String,
    path: PathBuf,
    harness: bool,
    spec: Option<TargetSpec>,
}

fn required_features_ok(spec: &Option<TargetSpec>, active: &[String]) -> bool {
    spec.as_ref().is_none_or(|s| {
        s.required_features
            .iter()
            .all(|f| active.iter().any(|a| a == f))
    })
}

fn pick_targets(member: &Member, req: &Request, active: &[String]) -> Result<Vec<Pick>> {
    let sel = req.targets;
    let m = &member.manifest;
    let explicit = sel.any_explicit();
    let test_mode = req.mode == Mode::Test;
    let bench_mode = req.mode == Mode::Bench;
    let all = sel.all_targets;
    let mut out = Vec::new();

    let named = |wanted: &[String], have: &str| wanted.iter().any(|w| w == have);

    if let Some(lib) = &m.lib {
        let want_lib = if explicit {
            sel.lib || all
        } else {
            !bench_mode
        };
        let path = lib
            .path
            .clone()
            .unwrap_or_else(|| PathBuf::from("src/lib.rs"));
        let name = lib
            .name
            .clone()
            .unwrap_or_else(|| member.name.replace('-', "_"));
        if want_lib && !sel.doc_only && (!test_mode || all) {
            out.push(Pick {
                kind: TargetKind::Lib,
                name: name.clone(),
                path: path.clone(),
                harness: false,
                spec: Some(lib.clone()),
            });
        }
        let want_lib_tests = (test_mode && (!explicit || sel.lib || all) && !sel.doc_only)
            || (all && !test_mode && !bench_mode);
        if want_lib_tests && lib.tested(true) && !lib.proc_macro_or_no_harness() {
            out.push(Pick {
                kind: TargetKind::Lib,
                name,
                path,
                harness: true,
                spec: Some(lib.clone()),
            });
        }
    } else if sel.lib && !explicit_other_than_lib(sel) {
        bail!("no library targets found in package `{}`", member.name);
    }

    let want_all_bins = if explicit {
        sel.bins || all
    } else {
        !bench_mode
    };
    for bin in &m.bin {
        let name = bin.name.clone().unwrap_or_else(|| member.name.clone());
        let chosen = want_all_bins || named(&sel.bin, &name);
        if !chosen || sel.doc_only {
            continue;
        }
        let path = bin
            .path
            .clone()
            .unwrap_or_else(|| PathBuf::from("src/main.rs"));
        if !required_features_ok(&Some(bin.clone()), active) {
            if named(&sel.bin, &name) {
                bail!(
                    "target `{name}` in package `{}` requires the features: {}",
                    member.name,
                    bin.required_features.join(", ")
                );
            }
            continue;
        }
        if !test_mode || all || named(&sel.bin, &name) && !test_mode {
            out.push(Pick {
                kind: TargetKind::Bin,
                name: name.clone(),
                path: path.clone(),
                harness: false,
                spec: Some(bin.clone()),
            });
        }
        if test_mode && bin.tested(true) && bin.harness {
            out.push(Pick {
                kind: TargetKind::Bin,
                name,
                path,
                harness: true,
                spec: Some(bin.clone()),
            });
        }
    }
    for wanted in &sel.bin {
        if !m
            .bin
            .iter()
            .any(|b| b.name.as_deref() == Some(wanted.as_str()))
        {}
    }

    // `test` without a selection builds every example, as Cargo does, and runs those with `test = true`.
    let want_all_examples = if explicit {
        sel.examples || all
    } else {
        test_mode
    };
    for ex in &m.example {
        let name = ex.name.clone().unwrap_or_default();
        if !(want_all_examples || named(&sel.example, &name)) || sel.doc_only {
            continue;
        }
        if !required_features_ok(&Some(ex.clone()), active) {
            if named(&sel.example, &name) {
                bail!(
                    "target `{name}` in package `{}` requires the features: {}",
                    member.name,
                    ex.required_features.join(", ")
                );
            }
            continue;
        }
        let path = ex.path.clone().unwrap_or_default();
        out.push(Pick {
            kind: TargetKind::Example,
            name: name.clone(),
            path: path.clone(),
            harness: false,
            spec: Some(ex.clone()),
        });
        if test_mode && ex.harness && ex.tested(false) {
            out.push(Pick {
                kind: TargetKind::Example,
                name,
                path,
                harness: true,
                spec: Some(ex.clone()),
            });
        }
    }

    let want_all_tests = if explicit {
        sel.tests || all
    } else {
        test_mode
    };
    for t in &m.test {
        let name = t.name.clone().unwrap_or_default();
        if !(want_all_tests || named(&sel.test, &name)) || sel.doc_only {
            continue;
        }
        if !required_features_ok(&Some(t.clone()), active) {
            if named(&sel.test, &name) {
                bail!(
                    "target `{name}` in package `{}` requires the features: {}",
                    member.name,
                    t.required_features.join(", ")
                );
            }
            continue;
        }
        if !t.tested(true) && !named(&sel.test, &name) && !all {
            continue;
        }
        out.push(Pick {
            kind: TargetKind::Test,
            name,
            path: t.path.clone().unwrap_or_default(),
            harness: t.harness,
            spec: Some(t.clone()),
        });
    }

    let want_all_benches = if explicit {
        sel.benches || all
    } else {
        bench_mode
    };
    for b in &m.bench {
        let name = b.name.clone().unwrap_or_default();
        if !(want_all_benches || named(&sel.bench, &name)) || sel.doc_only {
            continue;
        }
        if !required_features_ok(&Some(b.clone()), active) {
            continue;
        }
        out.push(Pick {
            kind: TargetKind::Bench,
            name,
            path: b.path.clone().unwrap_or_default(),
            harness: b.harness,
            spec: Some(b.clone()),
        });
    }

    ensure_target_exists(member, &out)?;
    Ok(out)
}

fn ensure_target_exists(member: &Member, picks: &[Pick]) -> Result<()> {
    for p in picks {
        let path = member.dir.join(&p.path);
        if !p.path.as_os_str().is_empty() && !path.exists() {
            let kind = match p.kind {
                TargetKind::Lib => "library",
                TargetKind::Bin => "bin",
                TargetKind::Example => "example",
                TargetKind::Test => "test",
                TargetKind::Bench => "bench",
            };
            bail!(
                "can't find {kind} `{}` at path `{}` (declared in {})",
                p.name,
                path.display(),
                member.manifest_path.display()
            );
        }
    }
    Ok(())
}

fn explicit_other_than_lib(sel: &TargetSelection) -> bool {
    sel.bins
        || !sel.bin.is_empty()
        || sel.examples
        || !sel.example.is_empty()
        || sel.tests
        || !sel.test.is_empty()
        || sel.benches
        || !sel.bench.is_empty()
        || sel.all_targets
}

trait LibHarness {
    fn proc_macro_or_no_harness(&self) -> bool;
}
impl LibHarness for TargetSpec {
    fn proc_macro_or_no_harness(&self) -> bool {
        !self.harness
    }
}

#[derive(Clone)]
struct RoleCfg {
    info: TargetInfo,
    flags: Vec<String>,
    linker: Option<String>,
}

/// Build scripts, proc-macros and what only they use run on the host; the rest is built for the target.
struct Roles {
    target: RoleCfg,
    host: RoleCfg,
    cross: bool,
}

impl Roles {
    fn new(ws: &Workspace, req: &Request) -> Result<Roles> {
        let tc = toolchain::global();
        let cfg = &ws.cargo_config;
        let cross = req.target.is_some();
        let hyprlnk = match ws.config.settings.linker().as_str() {
            "hyprlnk" => true,
            "default" => false,
            other => bail!("unknown `build.linker` `{other}`: expected `hyprlnk` or `default`"),
        };
        let build = |arg: Option<&str>, role: Role| {
            // Flags can depend on cfgs and cfgs on flags (`-C target-feature`): ask bare, pick flags, ask again.
            let probe = TargetInfo::query(tc, arg, &[]);
            let flags = toolchain::rustflags(cfg, &probe, role, cross);
            let info = if flags.is_empty() {
                probe
            } else {
                TargetInfo::query(tc, arg, &flags)
            };
            let linker = if hyprlnk && hyprlnk::supports_target(&info.name) {
                // On Linux hyprlnk links through a C compiler: the target's, if it names one.
                let driver = toolchain::linker(cfg, &info)
                    .filter(|l| {
                        info.name.contains("-linux-") && igel_common::linker::is_c_compiler(l)
                    })
                    .map(|l| l.to_string_lossy().into_owned());
                Some(igel_common::linker::embedded(
                    hyprlnk::VERSION,
                    driver.as_deref(),
                ))
            } else {
                toolchain::linker(cfg, &info).map(|p| p.to_string_lossy().into_owned())
            };
            RoleCfg {
                info,
                flags,
                linker,
            }
        };
        let target = build(req.target, Role::Target);
        let host = if cross {
            build(None, Role::Host)
        } else {
            target.clone()
        };
        Ok(Roles {
            target,
            host,
            cross,
        })
    }

    fn of(&self, host: bool) -> &RoleCfg {
        if host { &self.host } else { &self.target }
    }
}

#[derive(Clone, Copy)]
enum Origin {
    Local(usize),
    Registry,
}

struct Universe {
    views: Vec<PackageView>,
    origin: Vec<Origin>,
    id: Vec<String>,
    name: Vec<String>,
    version: Vec<semver::Version>,
    lock_pkg: Vec<Option<usize>>,
}

impl Universe {
    fn build(ws: &Workspace, locals: &Locals, lock: &Lockfile) -> Universe {
        let mut uni = Universe {
            views: Vec::new(),
            origin: Vec::new(),
            id: Vec::new(),
            name: Vec::new(),
            version: Vec::new(),
            lock_pkg: Vec::new(),
        };
        let mut by_id: HashMap<String, usize> = HashMap::new();

        let mut registry_by_name: HashMap<String, Vec<usize>> = HashMap::new();
        for (li, m) in locals.list.iter().enumerate() {
            by_id.insert(m.id(), li);
            if locals.member_of[li].is_none() {
                registry_by_name.entry(m.name.clone()).or_default().push(li);
            }
            uni.views.push(PackageView::default());
            uni.origin.push(Origin::Local(li));
            uni.id.push(m.id());
            uni.name.push(m.name.clone());
            uni.version.push(m.version.clone());
            uni.lock_pkg.push(None);
        }
        for (i, lp) in lock.packages.iter().enumerate() {
            if !is_registry_source(lp.source.as_deref()) {
                continue;
            }
            let id = format!("{} {}", lp.name, lp.version);
            if by_id.contains_key(&id) {
                continue;
            }
            let Ok(version) = lp.version.parse::<semver::Version>() else {
                continue;
            };
            let idx = uni.views.len();
            by_id.insert(id.clone(), idx);
            registry_by_name
                .entry(lp.name.clone())
                .or_default()
                .push(idx);
            uni.views.push(PackageView::default());
            uni.origin.push(Origin::Registry);
            uni.id.push(id);
            uni.name.push(lp.name.clone());
            uni.version.push(version);
            uni.lock_pkg.push(Some(i));
        }

        for idx in 0..uni.views.len() {
            uni.views[idx] = match uni.origin[idx] {
                Origin::Local(li) => local_view(ws, locals, li, &uni, &registry_by_name, lock),
                Origin::Registry => {
                    let lp = &lock.packages[uni.lock_pkg[idx].expect("registry has a lock entry")];
                    registry_view(lp, &by_id)
                }
            };
        }
        uni
    }
}

fn incomplete(
    uni: &Universe,
    locals: &Locals,
    lock: &Lockfile,
    applies: &dyn Fn(&Option<String>) -> bool,
) -> Vec<String> {
    let mut out = Vec::new();
    for (idx, view) in uni.views.iter().enumerate() {
        let Origin::Local(li) = uni.origin[idx] else {
            continue;
        };
        let member = locals.member_of[li].is_some();
        for d in &view.deps {
            if d.resolved.is_none()
                && !d.optional
                && (member || d.kind != DependencyKind::Dev)
                && applies(&d.target)
            {
                out.push(format!("`{}` → `{}`", view.name, d.alias));
            }
        }
    }
    let ids: HashSet<String> = lock
        .packages
        .iter()
        .map(|p| format!("{} {}", p.name, p.version))
        .collect();
    for p in &lock.packages {
        for d in &p.decls {
            // Optional dependencies nothing switches on are left out of the lockfile on purpose, as Cargo does.
            if let Some(id) = &d.resolved
                && d.kind != 1
                && !d.optional
                && applies(&d.target)
                && !ids.contains(id)
            {
                out.push(format!("`{}` → `{id}`", p.name));
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn unresolved_deps(uni: &Universe, res: &FeatureResolution) -> Vec<String> {
    let mut out: Vec<String> = res
        .unresolved
        .iter()
        .map(|&((pkg, _), di)| format!("`{}` → `{}`", uni.name[pkg], uni.views[pkg].deps[di].alias))
        .collect();
    out.sort();
    out.dedup();
    out
}

fn kind_of(k: u8) -> DependencyKind {
    match k {
        1 => DependencyKind::Dev,
        2 => DependencyKind::Build,
        _ => DependencyKind::Normal,
    }
}

fn registry_view(lp: &igel_resolver::LockPackage, by_id: &HashMap<String, usize>) -> PackageView {
    PackageView {
        name: lp.name.clone(),
        features: lp.feature_table.iter().cloned().collect(),
        deps: lp
            .decls
            .iter()
            .filter(|d| d.kind != 1)
            .map(|d| DepDecl {
                alias: d.alias.clone(),
                renamed: d.package.is_some(),
                kind: kind_of(d.kind),
                target: d.target.clone(),
                optional: d.optional,
                features: d.features.clone(),
                default_features: d.default_features,
                resolved: d.resolved.as_ref().and_then(|id| by_id.get(id).copied()),
            })
            .collect(),
        proc_macro: false,
    }
}

fn pick_registry(
    candidates: &[usize],
    uni: &Universe,
    req: Option<&semver::VersionReq>,
    locked_edges: &[String],
) -> Option<usize> {
    let fits = |i: usize| req.is_none_or(|r| r.matches(&uni.version[i]));
    for id in locked_edges {
        if let Some(&i) = candidates.iter().find(|&&i| uni.id[i] == *id)
            && fits(i)
        {
            return Some(i);
        }
    }
    candidates
        .iter()
        .copied()
        .filter(|&i| fits(i))
        .max_by(|&a, &b| uni.version[a].cmp(&uni.version[b]))
}

fn local_view(
    ws: &Workspace,
    locals: &Locals,
    li: usize,
    uni: &Universe,
    registry_by_name: &HashMap<String, Vec<usize>>,
    lock: &Lockfile,
) -> PackageView {
    let m = &locals.list[li];
    let locked_edges: Vec<String> = lock
        .packages
        .iter()
        .find(|p| {
            p.name == m.name
                && p.version == m.version.to_string()
                && !is_registry_source(p.source.as_deref())
        })
        .map(|p| p.dependencies.clone())
        .unwrap_or_default();

    let mut deps: Vec<&igel_manifest::Dependency> = m.manifest.dependencies.values().collect();
    deps.sort_by(|a, b| {
        (&a.name, a.kind as u8, &a.target).cmp(&(&b.name, b.kind as u8, &b.target))
    });
    let deps = deps
        .into_iter()
        .map(|dep| {
            let spec = locals.spec_for(ws, li, dep);
            let resolved = if let Some(path) = &spec.path {
                let dir = igel_common::fsutil::canonicalize(path).unwrap_or_else(|_| path.clone());
                locals.by_dir.get(&dir).copied()
            } else if let Some(url) = &spec.git
                && let Ok(Some(dir)) = ws.patches.git_patch_dir(
                    url,
                    spec.package.as_deref().unwrap_or(&dep.name),
                    spec.version.as_ref().unwrap_or(&semver::VersionReq::STAR),
                )
            {
                let dir = igel_common::fsutil::canonicalize(&dir).unwrap_or(dir);
                locals.by_dir.get(&dir).copied()
            } else if let Some(url) = &spec.git {
                let real = spec.package.as_deref().unwrap_or(&dep.name);
                let reference = igel_registry::git::GitRef::from_spec(
                    spec.branch.as_deref(),
                    spec.tag.as_deref(),
                    spec.rev.as_deref(),
                );
                igel_registry::git::checkout(url, &reference)
                    .ok()
                    .and_then(|co| igel_registry::git::find_package(&co, real).ok())
                    .map(|d| igel_common::fsutil::canonicalize(&d).unwrap_or(d))
                    .and_then(|d| locals.by_dir.get(&d).copied())
            } else {
                let real = spec.package.as_deref().unwrap_or(&dep.name);
                registry_by_name
                    .get(real)
                    .and_then(|c| pick_registry(c, uni, spec.version.as_ref(), &locked_edges))
            };
            DepDecl {
                alias: dep.name.clone(),
                renamed: spec.package.is_some(),
                kind: dep.kind,
                target: dep.target.clone(),
                optional: spec.optional,
                features: spec.features.clone(),
                default_features: spec.default_features,
                resolved,
            }
        })
        .collect();
    PackageView {
        name: m.name.clone(),
        features: m.manifest.features.clone(),
        deps,
        proc_macro: m.manifest.lib.as_ref().is_some_and(|l| l.proc_macro),
    }
}

type UnitRef = unit_features::UnitRef;

struct Planner<'a> {
    ws: &'a Workspace,
    locals: &'a Locals,
    req: &'a Request<'a>,
    uni: Universe,
    lock: &'a Lockfile,
    roles: Roles,
    reg: HashMap<usize, (PathBuf, Manifest)>,
    res: FeatureResolution,
    distinct_host: HashSet<usize>,
    edges_from: HashMap<UnitRef, Vec<usize>>,
    graph: BuildGraph,
    lib_node: HashMap<UnitRef, NodeId>,
}

impl Planner<'_> {
    fn canon(&self, u: UnitRef) -> UnitRef {
        if u.1 && !self.distinct_host.contains(&u.0) {
            (u.0, false)
        } else {
            u
        }
    }

    fn features_of(&self, u: UnitRef) -> Vec<String> {
        self.res
            .features
            .get(&u)
            .or_else(|| self.res.features.get(&(u.0, !u.1)))
            .cloned()
            .unwrap_or_default()
    }

    fn target_of(&self, host: bool) -> Option<TargetTriple> {
        if host {
            None
        } else {
            self.req.target.map(|t| TargetTriple(t.to_string()))
        }
    }

    fn is_member(&self, pkg: usize) -> bool {
        match self.uni.origin[pkg] {
            Origin::Local(li) => self.locals.member_of[li].is_some(),
            Origin::Registry => false,
        }
    }

    fn profile_for(&self, pkg: usize, host: bool) -> Option<Profile> {
        let tables = &self.ws.root_manifest.profile;
        let role = UnitRole {
            package: &self.uni.name[pkg],
            version: &self.uni.version[pkg],
            workspace_member: self.is_member(pkg),
            host,
            lean_debuginfo: matches!(self.uni.origin[pkg], Origin::Registry)
                && !self.ws.config.settings.dependency_debuginfo(),
        };
        let mut p = resolve_unit_profile(&self.req.profile.name, tables, role).ok()?;
        // Everything a test or benchmark links unwinds, as in Cargo.
        if matches!(self.req.mode, Mode::Test | Mode::Bench) {
            p.panic = igel_common::PanicStrategy::Unwind;
        }
        (p != *self.req.profile).then_some(p)
    }

    fn extras(&self, pkg: usize, host: bool) -> UnitExtras {
        let canon_host = host && self.distinct_host.contains(&pkg);
        let cfg = self.roles.of(host);
        let script_profile = {
            let tables = &self.ws.root_manifest.profile;
            resolve_unit_profile(
                &self.req.profile.name,
                tables,
                UnitRole {
                    package: &self.uni.name[pkg],
                    version: &self.uni.version[pkg],
                    workspace_member: self.is_member(pkg),
                    host: true,
                    lean_debuginfo: false,
                },
            )
            .ok()
        };
        let declared_features = match self.uni.origin[pkg] {
            Origin::Local(li) => {
                let m = &self.locals.list[li].manifest;
                let mut f: Vec<String> = m.features.keys().cloned().collect();
                f.extend(
                    m.dependencies
                        .values()
                        .filter(|d| d.spec.optional)
                        .map(|d| d.name.clone()),
                );
                f.sort();
                f.dedup();
                f
            }
            Origin::Registry => Vec::new(),
        };
        UnitExtras {
            for_host: canon_host,
            profile: self.profile_for(pkg, host),
            extra_crate_types: Vec::new(),
            member: self.is_member(pkg),
            rustflags: cfg.flags.clone(),
            linker: cfg.linker.clone(),
            script_rustflags: self.roles.host.flags.clone(),
            script_linker: self.roles.host.linker.clone(),
            script_profile,
            script_owner: None,
            target_spec: cfg.info.spec_hash.clone(),
            declared_features,
            env: self
                .ws
                .cargo_config
                .env_entries()
                .into_iter()
                .map(|e| (e.name, e.value))
                .collect(),
            cross: self.roles.cross,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn local_unit(
        &self,
        li: usize,
        kind: TargetKind,
        name: &str,
        crate_type: CrateType,
        extra_crate_types: Vec<CrateType>,
        source: &Path,
        harness: bool,
        edition_override: Option<&str>,
        host: bool,
    ) -> CompilationUnit {
        let m = &self.locals.list[li];
        let edition = edition_override
            .and_then(|e| e.parse().ok())
            .unwrap_or(m.edition);
        let build_script = m
            .manifest
            .package
            .as_ref()
            .and_then(|p| p.resolve_build_script(&m.dir));
        let mut ext = self.extras(li, host);
        ext.extra_crate_types = extra_crate_types;
        let crate_source = match &self.locals.git_of[li] {
            Some(co) => CrateSource::Git {
                url: co.url.clone(),
                commit: co.commit.clone(),
                subdir: m
                    .dir
                    .strip_prefix(&co.dir)
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
                    .filter(|p| !p.is_empty()),
                submodules: co.submodules.clone(),
            },
            None => CrateSource::Local,
        };
        CompilationUnit {
            crate_id: CrateId::new(
                if kind == TargetKind::Lib {
                    m.name.clone()
                } else {
                    name.to_string()
                },
                m.version.clone(),
                crate_source,
            ),
            crate_type,
            source_root: m.dir.join(source),
            manifest_dir: m.dir.clone(),
            features: self.features_of((li, host)),
            profile_name: self.req.profile.name.clone(),
            target: self.target_of(host),
            edition,
            is_build_script: false,
            check_only: self.req.mode == Mode::Check,
            links: m.manifest.package.as_ref().and_then(|p| p.links.clone()),
            build_script,
            meta: m.meta.clone(),
            lint_args: m.manifest.lint_args(self.locals.member_ws(self.ws, li)),
            target_name: name.to_string(),
            kind,
            harness,
            ext,
        }
    }

    fn registry_unit(&self, idx: usize, host: bool) -> Option<CompilationUnit> {
        let (dir, manifest) = self.reg.get(&idx)?;
        let lp = &self.lock.packages[self.uni.lock_pkg[idx]?];
        let pkg_meta = manifest.package.as_ref();
        let edition = pkg_meta
            .map(|p| p.edition_in(None))
            .and_then(|e| e.parse().ok())
            .unwrap_or(Edition::E2015);
        let lib = manifest.lib.as_ref();
        let is_proc_macro = lib.is_some_and(|l| l.proc_macro);
        let lib_src = lib
            .and_then(|l| l.path.clone())
            .unwrap_or_else(|| PathBuf::from("src/lib.rs"));
        let source_root = if dir.join(&lib_src).exists() {
            dir.join(&lib_src)
        } else {
            dir.join("src/lib.rs")
        };
        lib?;
        Some(CompilationUnit {
            crate_id: CrateId::new(
                self.uni.name[idx].clone(),
                self.uni.version[idx].clone(),
                CrateSource::Registry {
                    index: index_of_lock_source(lp.source.as_deref()),
                    checksum: lp.checksum.clone(),
                },
            ),
            crate_type: if is_proc_macro {
                CrateType::ProcMacro
            } else {
                CrateType::Lib
            },
            source_root,
            manifest_dir: dir.clone(),
            features: self.features_of((idx, host)),
            profile_name: self.req.profile.name.clone(),
            target: self.target_of(host),
            edition,
            is_build_script: false,
            check_only: self.req.mode == Mode::Check,
            links: pkg_meta.and_then(|p| p.links.clone()),
            build_script: pkg_meta.and_then(|p| p.resolve_build_script(dir)),
            meta: pkg_meta
                .map(|p| p.meta_in(None))
                .unwrap_or_else(PackageMeta::default),
            lint_args: Vec::new(),
            target_name: lib.and_then(|l| l.name.clone()).unwrap_or_default(),
            kind: TargetKind::Lib,
            harness: false,
            ext: self.extras(idx, host),
        })
    }

    fn wire(
        &mut self,
        node: Option<NodeId>,
        role: UnitRef,
        with_dev: bool,
        add: bool,
    ) -> Vec<(String, NodeId)> {
        let role = self.canon(role);
        let mut externs = Vec::new();
        let edge_ids = self.edges_from.get(&role).cloned().unwrap_or_default();
        for ei in edge_ids {
            let e = self.res.edges[ei].clone();
            let decl = self.uni.views[e.from.0].deps[e.dep].clone();
            if decl.kind == DependencyKind::Dev && !with_dev {
                continue;
            }
            let Some(&child) = self.lib_node.get(&self.canon(e.to)) else {
                continue; // a package with no library has nothing to link
            };
            if Some(child) == node {
                continue;
            }
            let alias = decl.renamed.then(|| decl.alias.clone());
            if add && let Some(node) = node {
                if decl.kind == DependencyKind::Build {
                    self.graph.add_build_edge(node, child, alias.as_deref());
                } else {
                    self.graph
                        .add_edge_with_extern(node, child, alias.as_deref());
                }
            }
            if decl.kind != DependencyKind::Build {
                let extern_name =
                    alias.unwrap_or_else(|| self.graph.node(child).unit.rustc_crate_name());
                externs.push((extern_name, child));
            }
        }
        externs
    }
}

fn check_links_conflicts(graph: &BuildGraph) -> Result<()> {
    let mut seen: HashMap<String, (PathBuf, String, semver::Version)> = HashMap::new();
    for node in graph.nodes() {
        let Some(links) = &node.unit.links else {
            continue;
        };
        let unit = &node.unit;
        match seen.get(links) {
            Some((dir, name, version)) if *dir != unit.manifest_dir => {
                bail!(
                    "multiple packages link to the native library `{links}`, but a native library can be linked only once\n  \
                     package `{name} v{version}` links to `{links}`\n  package `{} v{}` also links to `{links}`",
                    unit.crate_id.name,
                    unit.crate_id.version
                );
            }
            Some(_) => {}
            None => {
                seen.insert(
                    links.clone(),
                    (
                        unit.manifest_dir.clone(),
                        unit.crate_id.name.clone(),
                        unit.crate_id.version.clone(),
                    ),
                );
            }
        }
    }
    Ok(())
}

fn check_rust_versions(
    uni: &Universe,
    locals: &Locals,
    reg: &HashMap<usize, (PathBuf, Manifest)>,
    res: &FeatureResolution,
    req: &Request,
) -> Result<()> {
    if req.ignore_rust_version {
        return Ok(());
    }
    let tc = toolchain::global();
    let mut seen = HashSet::new();
    for &(pkg, _) in res.features.keys() {
        if !seen.insert(pkg) {
            continue;
        }
        let msrv = match uni.origin[pkg] {
            Origin::Local(li) => locals.list[li].meta.rust_version.clone(),
            Origin::Registry => reg
                .get(&pkg)
                .and_then(|(_, m)| m.package.as_ref())
                .map(|p| p.meta_in(None).rust_version)
                .unwrap_or_default(),
        };
        if !msrv.is_empty() && !tc.satisfies_msrv(&msrv) {
            bail!(
                "package `{} v{}` cannot be built because it requires rustc {msrv} or newer, \
                 while the currently active rustc version is {} (pass `--ignore-rust-version` to try anyway)",
                uni.name[pkg],
                uni.version[pkg],
                tc.version
            );
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unification {
    Selected,
    Workspace,
}

fn feature_unification(ws: &Workspace) -> Result<Unification> {
    let value = ws
        .config
        .settings
        .feature_unification()
        .or_else(|| ws.cargo_config.string(&["resolver", "feature-unification"]));
    match value.as_deref() {
        None | Some("selected") | Some("package") => Ok(Unification::Selected),
        Some("workspace") => Ok(Unification::Workspace),
        Some(other) => {
            bail!("unknown feature unification `{other}`: expected `selected` or `workspace`")
        }
    }
}

fn reachable(mut res: FeatureResolution, roots: &[UnitRef]) -> FeatureResolution {
    let mut from: HashMap<UnitRef, Vec<UnitRef>> = HashMap::new();
    for e in &res.edges {
        from.entry(e.from).or_default().push(e.to);
    }
    let mut seen: HashSet<UnitRef> = roots.iter().copied().collect();
    let mut stack: Vec<UnitRef> = roots.to_vec();
    while let Some(u) = stack.pop() {
        for &to in from.get(&u).into_iter().flatten() {
            if seen.insert(to) {
                stack.push(to);
            }
        }
    }
    if seen.len() == res.features.len() {
        return res;
    }
    res.features.retain(|u, _| seen.contains(u));
    res.edges.retain(|e| seen.contains(&e.from));
    res.unresolved.retain(|(u, _)| seen.contains(u));
    res
}

fn feature_roots(
    ws: &Workspace,
    locals: &Locals,
    req: &Request,
) -> Result<(Vec<RootRequest>, Vec<RootRequest>, Vec<UnitRef>)> {
    let include_dev = matches!(req.mode, Mode::Test | Mode::Bench)
        || req.targets.all_targets
        || req.targets.examples
        || req.targets.tests
        || req.targets.benches
        || !req.targets.example.is_empty()
        || !req.targets.test.is_empty()
        || !req.targets.bench.is_empty();

    let mut roots: Vec<RootRequest> = Vec::new();
    let single = req.selected.len() == 1;
    for &mi in req.selected {
        let Some(li) = locals.member_of.iter().position(|m| *m == Some(mi)) else {
            continue;
        };
        let (features, default_features) =
            features_for_member(&locals.list[li], true, single, req.features)?;
        roots.push(RootRequest {
            package: li,
            features,
            default_features,
            dev: include_dev,
        });
    }

    let unify = feature_unification(ws)?;
    let mut all_roots = roots.clone();
    if unify == Unification::Workspace {
        for (li, m) in locals.member_of.iter().enumerate() {
            if m.is_some() && !roots.iter().any(|r| r.package == li) {
                all_roots.push(RootRequest {
                    package: li,
                    features: Vec::new(),
                    default_features: true,
                    dev: include_dev,
                });
            }
        }
    }
    let selected_units: Vec<UnitRef> = roots.iter().map(|r| (r.package, false)).collect();
    Ok((roots, all_roots, selected_units))
}

pub fn plan(ws: &Workspace, req: &Request) -> Result<Plan> {
    let total = phase("plan");
    configure_git(ws, req.offline, false, None);
    let roles = Roles::new(ws, req)?;
    let applies = |t: &Option<String>| {
        t.as_deref()
            .is_none_or(|t| roles.target.info.matches(t) || roles.host.info.matches(t))
    };
    let resolver_version = ws.root_manifest.resolver_version();
    let either_role =
        |t: &str, _host: bool| roles.target.info.matches(t) || roles.host.info.matches(t);

    // Never build from a lockfile missing a needed dependency: resolve again from scratch.
    let mut fresh = false;
    let (locals, lock, uni, all_roots, selected_units, pass1) = loop {
        let mut locals = Locals::load(ws)?;
        let (_resolved, lock) = resolve_with(ws, &locals, req, !fresh)?;
        locals.extend_from_lock(ws, &lock)?;
        let uni = Universe::build(ws, &locals, &lock);
        let mut missing = incomplete(&uni, &locals, &lock, &applies);
        let (_roots, all_roots, selected_units) = feature_roots(ws, &locals, req)?;

        // Feature resolution pass 1: which packages are reached in either role (may over-approximate roles).
        let pass1 = reachable(
            unit_features::resolve(
                &uni.views,
                &all_roots,
                &FeatureOptions {
                    resolver: resolver_version,
                    platform_matches: &either_role,
                    weak_activates: false,
                },
            ),
            &selected_units,
        );
        missing.extend(unresolved_deps(&uni, &pass1));
        missing.sort();
        missing.dedup();
        if missing.is_empty() {
            break (locals, lock, uni, all_roots, selected_units, pass1);
        }
        if fresh {
            bail!(
                "the dependency resolution does not contain {} (declared in the manifests); \
                 `igl update` resolves everything again",
                missing.join(", ")
            );
        }
        tracing::debug!("the lockfile does not cover {missing:?}; resolving again");
        fresh = true;
    };
    let mut uni = uni;

    // Download and read the manifests of the needed registry crates.
    let mut needed: Vec<usize> = pass1
        .features
        .keys()
        .map(|(p, _)| *p)
        .filter(|&p| matches!(uni.origin[p], Origin::Registry))
        .collect();
    needed.sort_unstable();
    needed.dedup();
    let registry_pkgs: Vec<(String, String, Option<String>, String)> = needed
        .iter()
        .map(|&p| {
            let lp = &lock.packages[uni.lock_pkg[p].expect("registry has a lock entry")];
            (
                lp.name.clone(),
                lp.version.clone(),
                lp.checksum.clone(),
                index_of_lock_source(lp.source.as_deref()),
            )
        })
        .collect();
    let dl_timer = phase("download");
    let extracted_paths = igel_registry::download_and_extract_grouped(&registry_pkgs, req.offline)
        .context("Failed to download one or more registry crates")?;
    drop(dl_timer);

    let manifest_timer = phase("read manifests");
    let read = |dir: &PathBuf| Manifest::from_path(&dir.join("Cargo.toml")).ok();
    let manifests: Vec<Option<Manifest>> = if extracted_paths.len() < 16 {
        extracted_paths.iter().map(read).collect()
    } else {
        extracted_paths.par_iter().map(read).collect()
    };
    drop(manifest_timer);
    let mut reg: HashMap<usize, (PathBuf, Manifest)> = HashMap::new();
    for ((&p, dir), manifest) in needed.iter().zip(&extracted_paths).zip(manifests) {
        if let Some(m) = manifest {
            uni.views[p].proc_macro = m.lib.as_ref().is_some_and(|l| l.proc_macro);
            reg.insert(p, (dir.clone(), m));
        }
    }

    // Feature resolution pass 2: exact roles and platforms.
    let exact = |t: &str, host: bool| roles.of(host).info.matches(t);
    let res = reachable(
        unit_features::resolve(
            &uni.views,
            &all_roots,
            &FeatureOptions {
                resolver: resolver_version,
                platform_matches: &exact,
                weak_activates: false,
            },
        ),
        &selected_units,
    );
    check_rust_versions(&uni, &locals, &reg, &res, req)?;
    // Check feature requests between local packages on any platform, as Cargo does.
    let is_local = |p: usize| matches!(uni.origin[p], Origin::Local(_));
    let gated_way_to_a_local = uni.views.iter().enumerate().any(|(from, view)| {
        view.deps.iter().any(|d| {
            d.resolved
                .is_some_and(|to| is_local(to) && (d.target.is_some() || !is_local(from)))
        })
    });
    let everywhere = gated_way_to_a_local.then(|| {
        reachable(
            unit_features::resolve(
                &uni.views,
                &all_roots,
                &FeatureOptions {
                    resolver: resolver_version,
                    platform_matches: &|_, _| true,
                    weak_activates: false,
                },
            ),
            &selected_units,
        )
    });
    let unknown = everywhere.as_ref().unwrap_or(&res).unknown_features.iter();
    if let Some((from, to, f)) = unknown
        .clone()
        .find(|(from, to, _)| is_local(*from) && is_local(*to))
    {
        bail!(
            "package `{} v{}` depends on `{}` with feature `{f}`, but `{}` does not have that feature",
            uni.name[*from],
            uni.version[*from],
            uni.name[*to],
            uni.name[*to]
        );
    }

    let mut planner = Planner {
        ws,
        locals: &locals,
        req,
        uni,
        lock: &lock,
        roles,
        reg,
        res,
        distinct_host: HashSet::new(),
        edges_from: HashMap::new(),
        graph: BuildGraph::new(),
        lib_node: HashMap::new(),
    };

    {
        let pkgs: HashSet<usize> = planner.res.features.keys().map(|(p, _)| *p).collect();
        for pkg in pkgs {
            let host = planner.res.features.get(&(pkg, true));
            let target = planner.res.features.get(&(pkg, false));
            let distinct = match (host, target) {
                (Some(_), _) if planner.roles.cross => true,
                (Some(h), Some(t)) => {
                    h != t || planner.profile_for(pkg, true) != planner.profile_for(pkg, false)
                }
                _ => false,
            };
            if distinct {
                planner.distinct_host.insert(pkg);
            }
        }
        let edges = planner.res.edges.clone();
        for (i, e) in edges.iter().enumerate() {
            let from = planner.canon(e.from);
            planner.edges_from.entry(from).or_default().push(i);
        }
    }

    let unit_keys: Vec<UnitRef> = planner.res.features.keys().copied().collect();
    for (pkg, host) in unit_keys {
        let key = planner.canon((pkg, host));
        if planner.lib_node.contains_key(&key) {
            continue;
        }
        let unit = match planner.uni.origin[pkg] {
            Origin::Registry => planner.registry_unit(pkg, key.1),
            Origin::Local(li) => {
                let m = &locals.list[li];
                m.manifest.lib.as_ref().map(|lib| {
                    let (crate_type, extras) = crate_types_of(lib);
                    let path = lib
                        .path
                        .clone()
                        .unwrap_or_else(|| PathBuf::from("src/lib.rs"));
                    let name = lib.name.clone().unwrap_or_else(|| m.name.replace('-', "_"));
                    planner.local_unit(
                        li,
                        TargetKind::Lib,
                        &name,
                        crate_type,
                        extras,
                        &path,
                        false,
                        lib.edition.as_deref(),
                        key.1,
                    )
                })
            }
        };
        if let Some(unit) = unit {
            let id = planner.graph.add_node(unit);
            planner.lib_node.insert(key, id);
        }
    }
    let lib_nodes: Vec<(UnitRef, NodeId)> = {
        let mut v: Vec<_> = planner.lib_node.iter().map(|(k, v)| (*k, *v)).collect();
        v.sort_by_key(|(_, n)| n.0);
        v
    };
    for (key, node) in lib_nodes {
        planner.wire(Some(node), key, false, true);
    }

    let mut requested: Vec<Requested> = Vec::new();
    let mut doc_tests: Vec<DocTest> = Vec::new();
    let mut extra_roots: Vec<NodeId> = Vec::new();

    for &mi in req.selected {
        let Some(li) = locals.member_of.iter().position(|m| *m == Some(mi)) else {
            continue;
        };
        let member = &locals.list[li];
        let active = planner.features_of((li, false));
        let picks = pick_targets(member, req, &active)?;

        let mut bin_nodes: HashMap<String, NodeId> = HashMap::new();
        let mut nodes_for_picks: Vec<(usize, NodeId)> = Vec::new();

        for (pi, pick) in picks.iter().enumerate() {
            let proc_macro = pick.kind == TargetKind::Lib
                && member.manifest.lib.as_ref().is_some_and(|l| l.proc_macro);
            let (crate_type, extras) = match pick.kind {
                TargetKind::Lib if proc_macro => (CrateType::ProcMacro, Vec::new()),
                TargetKind::Lib => pick
                    .spec
                    .as_ref()
                    .map(crate_types_of)
                    .unwrap_or((CrateType::Lib, Vec::new())),
                _ => (CrateType::Bin, Vec::new()),
            };
            let is_plain_lib = pick.kind == TargetKind::Lib && !pick.harness;
            if is_plain_lib {
                if let Some(&n) = planner.lib_node.get(&(li, false)) {
                    requested.push(Requested {
                        node: n,
                        member: mi,
                        kind: TargetKind::Lib,
                        harness: false,
                        target: pick.name.clone(),
                    });
                }
                continue;
            }
            let extras = if pick.harness { Vec::new() } else { extras };
            let unit = planner.local_unit(
                li,
                pick.kind,
                &pick.name,
                crate_type,
                extras,
                &pick.path,
                pick.harness,
                pick.spec.as_ref().and_then(|s| s.edition.as_deref()),
                false,
            );
            let node = planner.graph.add_node(unit);
            if pick.kind == TargetKind::Bin && !pick.harness {
                bin_nodes.insert(pick.name.clone(), node);
            }
            nodes_for_picks.push((pi, node));
        }
        let needs_bins = picks
            .iter()
            .any(|p| matches!(p.kind, TargetKind::Test | TargetKind::Bench));
        if needs_bins {
            for bin in &member.manifest.bin {
                let name = bin.name.clone().unwrap_or_else(|| member.name.clone());
                if bin_nodes.contains_key(&name)
                    || !required_features_ok(&Some(bin.clone()), &active)
                {
                    continue;
                }
                let path = bin
                    .path
                    .clone()
                    .unwrap_or_else(|| PathBuf::from("src/main.rs"));
                let unit = planner.local_unit(
                    li,
                    TargetKind::Bin,
                    &name,
                    CrateType::Bin,
                    Vec::new(),
                    &path,
                    false,
                    bin.edition.as_deref(),
                    false,
                );
                let node = planner.graph.add_node(unit);
                bin_nodes.insert(name, node);
                planner.wire(Some(node), (li, false), false, true);
                if let Some(&lib) = planner.lib_node.get(&(li, false)) {
                    planner.graph.add_edge(node, lib);
                }
            }
        }

        let mut harness_lib_externs: Option<Vec<(String, NodeId)>> = None;
        for (pi, node) in nodes_for_picks {
            let pick = &picks[pi];
            let with_dev = pick.harness
                || matches!(
                    pick.kind,
                    TargetKind::Example | TargetKind::Test | TargetKind::Bench
                );
            let mut externs = planner.wire(Some(node), (li, false), with_dev, true);
            let own_lib = planner.lib_node.get(&(li, false)).copied();
            if pick.kind != TargetKind::Lib
                && let Some(lib) = own_lib
            {
                planner.graph.add_edge(node, lib);
                externs.push((planner.graph.node(lib).unit.rustc_crate_name(), lib));
            }
            if matches!(pick.kind, TargetKind::Test | TargetKind::Bench) {
                for &bin in bin_nodes.values() {
                    planner.graph.add_edge(node, bin);
                }
            }
            if pick.kind == TargetKind::Lib && pick.harness {
                harness_lib_externs = Some(externs.clone());
            }
            requested.push(Requested {
                node,
                member: mi,
                kind: pick.kind,
                harness: pick.harness,
                target: pick.name.clone(),
            });
        }

        if req.mode == Mode::Test
            && req.doctests
            && (req.targets.doc_only || !req.targets.any_explicit() || req.targets.all_targets)
            && let (Some(lib), Some(&lib_node)) =
                (&member.manifest.lib, planner.lib_node.get(&(li, false)))
            && lib.doctest
        {
            let mut externs = match harness_lib_externs.take() {
                Some(e) => e,
                None => planner.wire(None, (li, false), true, false),
            };
            externs.push((
                planner.graph.node(lib_node).unit.rustc_crate_name(),
                lib_node,
            ));
            extra_roots.push(lib_node);
            extra_roots.extend(externs.iter().map(|(_, n)| *n));
            doc_tests.push(DocTest {
                member: mi,
                lib_source: member.dir.join(
                    lib.path
                        .clone()
                        .unwrap_or_else(|| PathBuf::from("src/lib.rs")),
                ),
                crate_name: planner.graph.node(lib_node).unit.rustc_crate_name(),
                externs,
                features: active.clone(),
                edition: member.edition,
                proc_macro: lib.proc_macro,
            });
        }
    }

    check_named_targets(req, &locals, ws)?;

    let mut roots: Vec<NodeId> = requested.iter().map(|r| r.node).collect();
    roots.extend(extra_roots.iter().copied());
    let (mut graph, map) = planner.graph.subgraph(&roots);
    for r in &mut requested {
        r.node = map[&r.node];
    }
    for d in &mut doc_tests {
        for (_, n) in &mut d.externs {
            if let Some(&new) = map.get(n) {
                *n = new;
            }
        }
    }
    if req.mode == Mode::Check {
        exempt_real_link_requirements_from_check(&mut graph);
    }
    assign_script_owners(&mut graph);
    check_links_conflicts(&graph)?;
    drop(total);
    Ok(Plan {
        graph,
        requested,
        doc_tests,
    })
}

fn crate_types_of(lib: &TargetSpec) -> (CrateType, Vec<CrateType>) {
    let parse = |s: &str| match s {
        "lib" | "rlib" => Some(CrateType::Lib),
        "dylib" => Some(CrateType::Dylib),
        "cdylib" => Some(CrateType::Cdylib),
        "staticlib" => Some(CrateType::Staticlib),
        "proc-macro" => Some(CrateType::ProcMacro),
        _ => None,
    };
    let mut types: Vec<CrateType> = lib
        .crate_types
        .iter()
        .filter_map(|s| parse(s))
        .collect::<Vec<_>>();
    if lib.proc_macro && !types.contains(&CrateType::ProcMacro) {
        types.push(CrateType::ProcMacro);
    }
    types.dedup();
    if types.is_empty() {
        return (CrateType::Lib, Vec::new());
    }
    if types.contains(&CrateType::ProcMacro) {
        return (CrateType::ProcMacro, Vec::new());
    }
    let primary = if types.contains(&CrateType::Lib) {
        CrateType::Lib
    } else {
        types[0]
    };
    let extras = types.into_iter().filter(|t| *t != primary).collect();
    (primary, extras)
}

/// A package's build script runs once; all its units share the library's run and `OUT_DIR`.
fn assign_script_owners(graph: &mut BuildGraph) {
    let mut assignments: Vec<(NodeId, String)> = Vec::new();
    for node in graph.nodes() {
        let u = &node.unit;
        if u.build_script.is_none() || u.kind == TargetKind::Lib {
            continue;
        }
        let owner = node.dependencies.iter().find_map(|&d| {
            let o = &graph.node(d).unit;
            (o.kind == TargetKind::Lib
                && !o.harness
                && o.build_script.is_some()
                && o.manifest_dir == u.manifest_dir
                && o.ext.for_host == u.ext.for_host)
                .then(|| igel_scheduler::executor::script_key(o))
        });
        if let Some(owner) = owner {
            assignments.push((node.id, owner));
        }
    }
    for (id, owner) in assignments {
        graph.node_mut(id).unit.ext.script_owner = Some(owner);
    }
}

fn check_named_targets(req: &Request, locals: &Locals, _ws: &Workspace) -> Result<()> {
    let sel = req.targets;
    let mut have_bin = HashSet::new();
    let mut have_example = HashSet::new();
    let mut have_test = HashSet::new();
    let mut have_bench = HashSet::new();
    for &mi in req.selected {
        let Some(li) = locals.member_of.iter().position(|m| *m == Some(mi)) else {
            continue;
        };
        let m = &locals.list[li].manifest;
        have_bin.extend(m.bin.iter().filter_map(|t| t.name.clone()));
        have_example.extend(m.example.iter().filter_map(|t| t.name.clone()));
        have_test.extend(m.test.iter().filter_map(|t| t.name.clone()));
        have_bench.extend(m.bench.iter().filter_map(|t| t.name.clone()));
    }
    let check = |what: &str, wanted: &[String], have: &HashSet<String>| -> Result<()> {
        for w in wanted {
            if !have.contains(w) {
                let mut known: Vec<_> = have.iter().cloned().collect();
                known.sort();
                bail!(
                    "no {what} target named `{w}` in the selected packages{}",
                    if known.is_empty() {
                        String::new()
                    } else {
                        format!(" (available: {})", known.join(", "))
                    }
                );
            }
        }
        Ok(())
    };
    check("bin", &sel.bin, &have_bin)?;
    check("example", &sel.example, &have_example)?;
    check("test", &sel.test, &have_test)?;
    check("bench", &sel.bench, &have_bench)
}

/// Build scripts and proc-macros need real linked dependencies, so lift `check_only` across their
/// dependency closure (proc-macro deps and build-dependencies).
fn exempt_real_link_requirements_from_check(graph: &mut BuildGraph) {
    let mut must_be_real: HashSet<NodeId> = HashSet::new();
    let mut stack: Vec<NodeId> = Vec::new();
    for n in graph.nodes() {
        if n.unit.crate_type == CrateType::ProcMacro {
            stack.extend(n.dependencies.iter().copied());
        } else if n.unit.build_script.is_some() {
            stack.extend(
                n.dependencies
                    .iter()
                    .copied()
                    .filter(|d| n.build_dependencies.contains(d)),
            );
        }
    }
    for &id in &stack {
        must_be_real.insert(id);
    }
    while let Some(id) = stack.pop() {
        for &dep_id in &graph.node(id).dependencies {
            if must_be_real.insert(dep_id) {
                stack.push(dep_id);
            }
        }
    }
    for id in must_be_real {
        graph.node_mut(id).unit.check_only = false;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EdgeKind {
    Normal,
    Build,
    Dev,
}

#[derive(Debug, Clone)]
pub struct InventoryPackage {
    pub name: String,
    pub version: semver::Version,
    pub source: Option<String>,
    pub checksum: Option<String>,
    pub meta: igel_common::PackageMeta,
    pub member: bool,
    pub git: Option<(String, String)>,
    pub registry: Option<String>,
    pub deps: Vec<(usize, EdgeKind)>,
}

pub fn inventory(ws: &Workspace, offline: bool) -> Result<Vec<InventoryPackage>> {
    let existing = LockfileManager::load_fast(ws.root());
    let (resolved, lock) = resolve_fresh(ws, offline, existing.as_ref(), false)?;
    store_resolution(ws, &lock)?;
    download_all(&resolved, offline)?;
    let mut locals = Locals::load(ws)?;
    locals.extend_from_lock(ws, &lock)?;

    let mut pkgs: Vec<&ResolvedPackage> = resolved.packages.values().collect();
    pkgs.sort_by(|a, b| (&a.name, &a.version).cmp(&(&b.name, &b.version)));
    let manifests: Vec<Option<igel_common::PackageMeta>> = pkgs
        .par_iter()
        .map(|p| match &p.source {
            PackageSource::Registry(index) => {
                let dl = igel_registry::CrateDownloader::for_registry(index)
                    .ok()?
                    .offline(true);
                let dir = dl
                    .download_and_extract(&p.name, &p.version.to_string(), p.checksum.as_deref())
                    .ok()?;
                let m = Manifest::from_path(&igel_common::manifest_file::manifest_in(&dir)).ok()?;
                Some(m.package.as_ref()?.meta_in(None))
            }
            _ => None,
        })
        .collect();

    let mut out: Vec<InventoryPackage> = Vec::with_capacity(pkgs.len());
    for (p, registry_meta) in pkgs.iter().zip(manifests) {
        let local = locals
            .list
            .iter()
            .position(|m| m.name == p.name && m.version == p.version);
        let meta = registry_meta
            .or_else(|| local.map(|li| locals.list[li].meta.clone()))
            .unwrap_or_default();
        out.push(InventoryPackage {
            name: p.name.clone(),
            version: p.version.clone(),
            source: p.source.lock_source(),
            checksum: p.checksum.clone(),
            meta,
            member: local.is_some_and(|li| locals.member_of[li].is_some()),
            git: match &p.source {
                PackageSource::Git { url, commit, .. } => Some((url.clone(), commit.clone())),
                _ => None,
            },
            registry: match &p.source {
                PackageSource::Registry(index) => Some(index.clone()),
                _ => None,
            },
            deps: Vec::new(),
        });
    }

    let find = |spec: &str| -> Option<usize> {
        let mut parts = spec.split_whitespace();
        let name = parts.next()?;
        let version = parts.next();
        let source = spec.split_once(" (").map(|(_, s)| s.trim_end_matches(')'));
        out.iter().position(|q| {
            q.name == name
                && version.is_none_or(|v| q.version.to_string() == v)
                && source.is_none_or(|s| q.source.as_deref() == Some(s))
        })
    };
    let mut edges: Vec<Vec<(usize, EdgeKind)>> = vec![Vec::new(); out.len()];
    for (i, p) in pkgs.iter().enumerate() {
        let Some(locked) = lock.packages.iter().find(|l| {
            l.name == p.name
                && l.version == p.version.to_string()
                && l.source == p.source.lock_source()
        }) else {
            continue;
        };
        for spec in &locked.dependencies {
            let Some(j) = find(spec) else { continue };
            let kinds: Vec<DependencyKind> = p
                .dependencies
                .iter()
                .filter(|d| d.package.as_deref().unwrap_or(&d.name) == out[j].name)
                .map(|d| d.kind)
                .collect();
            let kind = if kinds.is_empty() || kinds.contains(&DependencyKind::Normal) {
                EdgeKind::Normal
            } else if kinds.contains(&DependencyKind::Build) {
                EdgeKind::Build
            } else {
                EdgeKind::Dev
            };
            if !edges[i].iter().any(|(k, _)| *k == j) {
                edges[i].push((j, kind));
            }
        }
    }
    for (p, e) in out.iter_mut().zip(edges) {
        p.deps = e;
    }
    Ok(out)
}
