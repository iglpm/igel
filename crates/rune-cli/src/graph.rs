//! Turns a workspace plus a build request into a [`BuildGraph`].
//!
//! The pipeline:
//!
//! 1. **Resolve** the whole workspace (so the lockfile is complete and stable
//!    no matter which packages a command selects), or reuse the previous
//!    resolution if no manifest or feature flag changed.
//! 2. **Prune** to the registry crates actually reachable from what was
//!    requested, and only download those.
//! 3. **Build nodes**: one per registry library, per local library, and per
//!    requested target (binary, example, test harness, ...).
//! 4. **Connect** them, then drop everything not reachable from the requested
//!    targets.

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use rune_common::timing::phase;
use rune_common::{
    CompilationUnit, CrateId, CrateSource, CrateType, Edition, PackageMeta, Profile, TargetKind,
    TargetTriple,
};
use rune_graph::{BuildGraph, NodeId};
use rune_manifest::dependency::{DependencyKind, DependencySpec};
use rune_manifest::{Manifest, TargetSpec};
use rune_registry::CrateDownloader;
use rune_resolver::lockfile::{Lockfile, LockfileManager};
use rune_resolver::resolver::{
    DependencyResolver, PackageSource, ResolvedDependencyRef, ResolvedGraph, ResolvedPackage,
    matches_target,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::ui;
use crate::workspace::{Member, Workspace, effective_spec};

// ---------------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------------

/// What kind of command is asking for a graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Build,
    Check,
    Test,
    Bench,
}

/// `--features`, `--all-features`, `--no-default-features`.
#[derive(Debug, Clone, Default)]
pub struct FeatureArgs {
    pub features: Vec<String>,
    pub all_features: bool,
    pub no_default_features: bool,
}

impl FeatureArgs {
    /// Splits `a,b c` style values (Cargo accepts commas and spaces).
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

/// `--lib`, `--bin`, `--example`, `--test`, `--bench`, `--all-targets`, ...
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
    /// Run only documentation tests (`rune test --doc`).
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

/// Everything a graph is planned for.
pub struct Request<'a> {
    pub profile: &'a Profile,
    pub target: Option<&'a str>,
    pub mode: Mode,
    pub selected: &'a [usize],
    pub features: &'a FeatureArgs,
    pub targets: &'a TargetSelection,
    pub offline: bool,
    pub locked: bool,
    /// Run doctests (only meaningful for [`Mode::Test`]).
    pub doctests: bool,
}

/// One requested output of the build.
#[derive(Debug, Clone)]
pub struct Requested {
    pub node: NodeId,
    pub member: usize,
    pub kind: TargetKind,
    pub harness: bool,
    /// Cargo target name (`my-bin`, `integration`, ...).
    pub target: String,
}

/// A library whose documentation examples should be tested.
#[derive(Debug, Clone)]
pub struct DocTest {
    pub member: usize,
    pub lib_source: PathBuf,
    pub crate_name: String,
    /// `--extern name=<artifact of node>` for the library itself and every
    /// dependency (normal, build and dev).
    pub externs: Vec<(String, NodeId)>,
    pub features: Vec<String>,
    pub edition: Edition,
}

/// The planned build.
pub struct Plan {
    pub graph: BuildGraph,
    pub requested: Vec<Requested>,
    pub doc_tests: Vec<DocTest>,
}

// ---------------------------------------------------------------------------
// Local packages (workspace members and path dependencies)
// ---------------------------------------------------------------------------

struct Locals {
    list: Vec<Member>,
    /// `list[i]` is `ws.members[member_of[i]]`, when it is a workspace member.
    member_of: Vec<Option<usize>>,
    by_dir: HashMap<PathBuf, usize>,
}

impl Locals {
    /// Loads the workspace members plus every package reachable through a
    /// `path = "..."` dependency (which may live outside the workspace).
    fn load(ws: &Workspace) -> Result<Self> {
        let mut list: Vec<Member> = ws.members.clone();
        let mut member_of: Vec<Option<usize>> = (0..list.len()).map(Some).collect();
        let mut by_dir: HashMap<PathBuf, usize> = list
            .iter()
            .enumerate()
            .map(|(i, m)| (m.dir.clone(), i))
            .collect();

        let mut i = 0;
        while i < list.len() {
            let member_ws = member_of[i].and(ws.workspace_manifest());
            let pkg_dir = list[i].dir.clone();
            reject_unsupported_sources(&list[i], ws, member_ws)?;
            let paths: Vec<PathBuf> = list[i]
                .manifest
                .dependencies
                .values()
                .filter_map(|dep| effective_spec(dep, &pkg_dir, ws.root(), member_ws).path)
                .collect();
            for path in paths {
                let dir = path.canonicalize().unwrap_or(path);
                if !by_dir.contains_key(&dir) {
                    if !dir.join("Cargo.toml").is_file() {
                        bail!(
                            "path dependency `{}` of `{}` does not contain a Cargo.toml",
                            dir.display(),
                            list[i].name
                        );
                    }
                    let member = Member::load(&dir, None)?;
                    by_dir.insert(dir, list.len());
                    list.push(member);
                    member_of.push(None);
                }
            }
            i += 1;
        }
        Ok(Self {
            list,
            member_of,
            by_dir,
        })
    }
}

/// Fails early, with an actionable message, on dependency sources Rune can't
/// resolve yet. Silently treating a `git = "..."` dependency as "whatever
/// crates.io has under that name" would build the wrong code.
fn reject_unsupported_sources(
    member: &Member,
    ws: &Workspace,
    member_ws: Option<&rune_manifest::WorkspaceManifest>,
) -> Result<()> {
    for dep in member.manifest.dependencies.values() {
        let spec = effective_spec(dep, &member.dir, ws.root(), member_ws);
        if let Some(url) = &spec.git {
            bail!(
                "dependency `{}` of `{}` uses a git source ({url}), which Rune does not support yet; \
                 use `path = \"...\"` to a checkout, or build this project with `cargo`",
                dep.name,
                member.name
            );
        }
        if let Some(registry) = &spec.registry {
            bail!(
                "dependency `{}` of `{}` uses the alternative registry `{registry}`, which Rune does \
                 not support yet (only crates.io)",
                dep.name,
                member.name
            );
        }
    }
    if !member.manifest.patch.is_empty() {
        ui::warn(format!(
            "`[patch]` in {} is ignored: Rune does not support patching dependencies yet",
            member.dir.join("Cargo.toml").display()
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// Which of a package's `features` a `--features` list turns on.
///
/// Accepts `feat`, `pkg/feat`, and `pkg?/feat`. A bare `feat` applies to every
/// selected package that declares it; it is an error if none do.
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
        // Optional dependencies are implicit features.
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
            Some(_) => {} // aimed at another package (or a dependency's feature)
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
    req: &Request,
) -> Result<Vec<(String, DependencySpec)>> {
    let mut roots = Vec::new();
    let single = req.selected.len() == 1;
    for (i, m) in ws.members.iter().enumerate() {
        let (features, default_features) =
            features_for_member(m, req.selected.contains(&i), single, req.features)?;
        roots.push((
            m.name.clone(),
            DependencySpec {
                path: Some(m.dir.clone()),
                features,
                default_features,
                ..Default::default()
            },
        ));
    }
    // Development dependencies are not followed by the resolver's package
    // walk (they don't propagate to dependents), so add them as roots. Always
    // included, even for `build`: one resolution then serves every command and
    // the lockfile doesn't flip-flop between `build` and `test`.
    for (li, m) in locals.list.iter().enumerate() {
        let member_ws = locals.member_of[li].and(ws.workspace_manifest());
        for dep in m.manifest.dependencies.values() {
            if dep.kind != DependencyKind::Dev {
                continue;
            }
            let spec = effective_spec(dep, &m.dir, ws.root(), member_ws);
            let real = spec.package.clone().unwrap_or_else(|| dep.name.clone());
            roots.push((real, spec));
        }
    }
    Ok(roots)
}

/// Hash of everything that determines a resolution's result.
fn resolution_fingerprint(ws: &Workspace, locals: &Locals, req: &Request) -> String {
    let mut hasher = rune_common::hash::HashBuilder::new();
    hasher.update(env!("CARGO_PKG_VERSION").as_bytes());
    let mut paths: Vec<PathBuf> = vec![ws.manifest_path()];
    paths.extend(locals.list.iter().map(|m| m.dir.join("Cargo.toml")));
    paths.sort();
    paths.dedup();
    for path in paths {
        hasher.update(path.to_string_lossy().as_bytes());
        if let Ok(contents) = fs::read(&path) {
            hasher.update(&contents);
        }
    }
    // Feature flags only change the resolution of the packages they apply to,
    // so the *selection* matters only when flags are in play; otherwise
    // `rune build -p a` and `rune build` share one resolution.
    let flags_in_play = !req.features.features.is_empty()
        || req.features.all_features
        || req.features.no_default_features;
    if flags_in_play {
        for &i in req.selected {
            hasher.update(ws.members[i].name.as_bytes());
        }
    }
    for f in &req.features.features {
        hasher.update(f.as_bytes());
        hasher.update(b",");
    }
    hasher.update(&[
        req.features.all_features as u8,
        req.features.no_default_features as u8,
    ]);
    hasher.finish_blake3()
}

fn resolved_from_lock(lock: &Lockfile) -> ResolvedGraph {
    let mut packages = HashMap::new();
    for pkg in &lock.packages {
        let Ok(version) = pkg.version.parse::<semver::Version>() else {
            continue;
        };
        let is_registry = pkg
            .source
            .as_deref()
            .is_some_and(|s| s.starts_with("registry+"));
        packages.insert(
            format!("{} {}", pkg.name, version),
            ResolvedPackage {
                name: pkg.name.clone(),
                version,
                source: if is_registry {
                    PackageSource::Registry
                } else {
                    PackageSource::Path(PathBuf::new())
                },
                checksum: pkg.checksum.clone(),
                // Edges are rebuilt from the extracted manifests, which know
                // about `package = "..."` renames the lockfile can't express.
                dependencies: Vec::new(),
                features: pkg.features.clone(),
            },
        );
    }
    ResolvedGraph {
        root_packages: Vec::new(),
        packages,
    }
}

/// A request with no feature flags that selects every member — what
/// commands like `update`, `fetch` and `tree` resolve against.
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
    }
}

/// Fully resolves the whole workspace from scratch (no fast path) and returns
/// the result without touching any file. `existing` seeds version choices;
/// `refresh` re-queries the registry index for newer releases.
pub fn resolve_fresh(
    ws: &Workspace,
    offline: bool,
    existing: Option<&Lockfile>,
    refresh: bool,
) -> Result<(ResolvedGraph, Lockfile)> {
    let _timer = phase("resolve (fresh)");
    let locals = Locals::load(ws)?;
    let profile = Profile::dev();
    let features = FeatureArgs::default();
    let targets = TargetSelection::default();
    let selected: Vec<usize> = (0..ws.members.len()).collect();
    let req = whole_workspace_request(&profile, offline, &features, &targets, &selected);
    let roots = resolution_roots(ws, &locals, &req)?;

    let mut resolver = if refresh {
        DependencyResolver::refreshing_resolver()
    } else {
        DependencyResolver::default_resolver()
    }
    .offline(offline);
    let resolved = resolver
        .resolve_with_workspace(ws.root(), &roots, existing, ws.workspace_manifest())
        .context("Dependency resolution failed")?;
    let lock = resolved.to_lockfile();
    Ok((resolved, lock))
}

/// Writes `lock` as the project's lockfile and records the resolution as
/// current, so the next build takes the fast path.
pub fn store_resolution(ws: &Workspace, lock: &Lockfile) -> Result<()> {
    let locals = Locals::load(ws)?;
    let profile = Profile::dev();
    let features = FeatureArgs::default();
    let targets = TargetSelection::default();
    let selected: Vec<usize> = (0..ws.members.len()).collect();
    let req = whole_workspace_request(&profile, false, &features, &targets, &selected);
    LockfileManager::sync_all(ws.root(), lock).context("Failed to write lockfiles")?;
    let fingerprint = resolution_fingerprint(ws, &locals, &req);
    let path = ws.root().join(".rune").join("resolve.fingerprint");
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, fingerprint);
    Ok(())
}

/// Downloads every registry crate in `resolved` (in parallel). Returns how
/// many were fetched or already present.
pub fn download_all(resolved: &ResolvedGraph, offline: bool) -> Result<usize> {
    let pkgs: Vec<(String, String, Option<String>)> = resolved
        .packages
        .values()
        .filter(|p| matches!(p.source, PackageSource::Registry))
        .map(|p| (p.name.clone(), p.version.to_string(), p.checksum.clone()))
        .collect();
    let downloader = CrateDownloader::default_downloader().offline(offline);
    downloader
        .download_and_extract_many(&pkgs)
        .context("Failed to download one or more registry crates")?;
    Ok(pkgs.len())
}

/// Resolves the workspace, or reuses the previous resolution when nothing
/// that feeds it changed. Returns the graph and the lockfile it corresponds to.
fn resolve(ws: &Workspace, locals: &Locals, req: &Request) -> Result<(ResolvedGraph, Lockfile)> {
    let root = ws.root();
    let fingerprint_path = root.join(".rune").join("resolve.fingerprint");
    let fingerprint = resolution_fingerprint(ws, locals, req);
    let unchanged = fs::read_to_string(&fingerprint_path)
        .map(|stored| stored.trim() == fingerprint)
        .unwrap_or(false);

    // Fast path: nothing changed since the last resolution, and we have the
    // binary lockfile that carries its feature sets.
    if unchanged {
        if let Some(lock) = LockfileManager::load_binary(root) {
            if !lock.packages.is_empty() {
                return Ok((resolved_from_lock(&lock), lock));
            }
        }
    }

    // Cargo.lock (if present) seeds version choices so that adopting Rune on
    // an existing project doesn't move any dependency.
    let existing_lock = LockfileManager::load_fast(root);
    let roots = resolution_roots(ws, locals, req)?;

    ui::action("resolving", "dependency graph");
    let _timer = phase("resolve");
    let mut resolver = DependencyResolver::default_resolver().offline(req.offline);
    let resolved = resolver
        .resolve_with_workspace(
            root,
            &roots,
            existing_lock.as_ref(),
            ws.workspace_manifest(),
        )
        .context("Dependency resolution failed")?;

    let lockfile = resolved.to_lockfile();
    if req.locked && existing_lock.as_ref().map(registry_view) != Some(registry_view(&lockfile)) {
        bail!(
            "the lock file needs to be updated but `--locked` was passed to prevent this \
             (run `rune update` first)"
        );
    }
    if existing_lock.as_ref() != Some(&lockfile) || LockfileManager::load_binary(root).is_none() {
        LockfileManager::sync_all(root, &lockfile).context("Failed to write lockfiles")?;
    }
    if let Some(parent) = fingerprint_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(&fingerprint_path, &fingerprint);

    Ok((resolved, lockfile))
}

/// The registry part of a lockfile: what `--locked` compares.
fn registry_view(lock: &Lockfile) -> BTreeMap<(String, String), Option<String>> {
    lock.packages
        .iter()
        .filter(|p| {
            p.source
                .as_deref()
                .is_some_and(|s| s.starts_with("registry+"))
        })
        .map(|p| ((p.name.clone(), p.version.clone()), p.checksum.clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// Target picking
// ---------------------------------------------------------------------------

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

/// Which targets of `member` the request asks for.
fn pick_targets(member: &Member, req: &Request, active: &[String]) -> Result<Vec<Pick>> {
    let sel = req.targets;
    let m = &member.manifest;
    let explicit = sel.any_explicit();
    let test_mode = req.mode == Mode::Test;
    let bench_mode = req.mode == Mode::Bench;
    let all = sel.all_targets;
    let mut out = Vec::new();

    let named = |wanted: &[String], have: &str| wanted.iter().any(|w| w == have);

    // Library.
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
        if want_lib && !sel.doc_only {
            // Plain build of the library (a dependency of everything else in
            // build/check; in test mode it is a dependency only, added lazily).
            if !test_mode || all {
                out.push(Pick {
                    kind: TargetKind::Lib,
                    name: name.clone(),
                    path: path.clone(),
                    harness: false,
                    spec: Some(lib.clone()),
                });
            }
        }
        let want_lib_tests = (test_mode && (!explicit || sel.lib || all) && !sel.doc_only)
            || (all && !test_mode && !bench_mode);
        if want_lib_tests && lib.test && !lib.proc_macro_or_no_harness() {
            out.push(Pick {
                kind: TargetKind::Lib,
                name,
                path,
                harness: true,
                spec: Some(lib.clone()),
            });
        }
    } else if !sel.bin.is_empty() || sel.lib {
        // Asked for a lib that doesn't exist: only an error when explicit and
        // nothing else was asked for.
        if sel.lib && !explicit_other_than_lib(sel) {
            bail!("no library targets found in package `{}`", member.name);
        }
    }

    // Binaries.
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
        if test_mode && bin.test && bin.harness {
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
        {
            // Only an error if no selected package has it; the caller checks
            // that across all packages.
        }
    }

    // Examples.
    let want_all_examples = explicit && (sel.examples || all);
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
        out.push(Pick {
            kind: TargetKind::Example,
            name,
            path: ex.path.clone().unwrap_or_default(),
            harness: false,
            spec: Some(ex.clone()),
        });
    }

    // Integration tests.
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
        if !t.test && !named(&sel.test, &name) && !all {
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

    // Benchmarks.
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

    Ok(out)
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
    /// A `harness = false` library has no `#[test]` harness to run.
    fn proc_macro_or_no_harness(&self) -> bool {
        !self.harness
    }
}

// ---------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------

/// Which kinds of dependency a unit links against.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Deps {
    NormalAndBuild,
    WithDev,
}

fn dep_allowed(kind: DependencyKind, deps: Deps) -> bool {
    match deps {
        Deps::NormalAndBuild => kind != DependencyKind::Dev,
        Deps::WithDev => true,
    }
}

struct Builder<'a> {
    ws: &'a Workspace,
    locals: &'a Locals,
    req: &'a Request<'a>,
    resolved: &'a ResolvedGraph,
    /// `name version` → node, for registry crates.
    registry_nodes: HashMap<String, NodeId>,
    /// local index → plain library node.
    lib_nodes: HashMap<usize, NodeId>,
    graph: BuildGraph,
    target_triple: String,
}

impl Builder<'_> {
    fn local_features(&self, li: usize) -> Vec<String> {
        let m = &self.locals.list[li];
        self.resolved
            .packages
            .get(&m.id())
            .map(|p| p.features.clone())
            .unwrap_or_default()
    }

    #[allow(clippy::too_many_arguments)]
    fn unit(
        &self,
        li: usize,
        kind: TargetKind,
        name: &str,
        crate_type: CrateType,
        source: &Path,
        harness: bool,
        edition_override: Option<&str>,
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
        let crate_name = name.to_string();
        CompilationUnit {
            crate_id: CrateId::local(
                if kind == TargetKind::Lib {
                    m.name.clone()
                } else {
                    crate_name.clone()
                },
                m.version.clone(),
            ),
            crate_type,
            source_root: m.dir.join(source),
            manifest_dir: m.dir.clone(),
            features: self.local_features(li),
            profile_name: self.req.profile.name.clone(),
            target: self.req.target.map(|t| TargetTriple(t.to_string())),
            edition,
            is_build_script: false,
            check_only: self.req.mode == Mode::Check,
            links: m.manifest.package.as_ref().and_then(|p| p.links.clone()),
            build_script,
            meta: m.meta.clone(),
            lint_args: m
                .manifest
                .lint_args(self.locals.member_of[li].and(self.ws.workspace_manifest())),
            target_name: crate_name,
            kind,
            harness,
        }
    }

    /// Resolves one declared dependency to a graph node and its `--extern`
    /// alias (only when the dependency is renamed with `package = "..."`).
    fn dep_target(
        &self,
        li: usize,
        dep: &rune_manifest::Dependency,
    ) -> Option<(NodeId, Option<String>)> {
        let m = &self.locals.list[li];
        let member_ws = self.locals.member_of[li].and(self.ws.workspace_manifest());
        let spec = effective_spec(dep, &m.dir, self.ws.root(), member_ws);
        let alias = spec.package.as_ref().map(|_| dep.name.clone());

        if let Some(path) = &spec.path {
            let dir = path.canonicalize().unwrap_or_else(|_| path.clone());
            let target_li = *self.locals.by_dir.get(&dir)?;
            return self.lib_nodes.get(&target_li).map(|&n| (n, alias));
        }
        let real = spec.package.as_deref().unwrap_or(&dep.name);
        let req = spec
            .version
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_else(|| "*".to_string());
        let pkg = self.resolved.find_matching(real, &req)?;
        self.registry_nodes
            .get(&format!("{} {}", pkg.name, pkg.version))
            .map(|&n| (n, alias))
    }

    /// Adds edges from `node` to everything package `li` depends on.
    fn connect(&mut self, node: NodeId, li: usize, deps: Deps) -> Vec<(String, NodeId)> {
        let m = &self.locals.list[li];
        let mut externs = Vec::new();
        let deps_list: Vec<&rune_manifest::Dependency> = {
            let mut v: Vec<_> = m.manifest.dependencies.values().collect();
            v.sort_by(|a, b| (&a.name, a.kind as u8).cmp(&(&b.name, b.kind as u8)));
            v
        };
        for dep in deps_list {
            if !dep_allowed(dep.kind, deps) {
                continue;
            }
            if !matches_target(dep.target.as_deref(), &self.target_triple) {
                continue;
            }
            let Some((target_node, alias)) = self.dep_target(li, dep) else {
                continue; // an inactive optional dependency has no node
            };
            if target_node == node {
                continue;
            }
            self.graph
                .add_edge_with_extern(node, target_node, alias.as_deref());
            let extern_name =
                alias.unwrap_or_else(|| self.graph.node(target_node).unit.rustc_crate_name());
            externs.push((extern_name, target_node));
        }
        externs
    }
}

/// Registry crates (and local packages) that `roots` need, following the
/// lockfile's dependency edges.
fn reachable_registry(lock: &Lockfile, seeds: impl IntoIterator<Item = String>) -> HashSet<String> {
    let index: HashMap<String, &rune_resolver::LockPackage> = lock
        .packages
        .iter()
        .map(|p| (format!("{} {}", p.name, p.version), p))
        .collect();
    let mut seen: HashSet<String> = HashSet::new();
    let mut stack: Vec<String> = seeds.into_iter().collect();
    while let Some(id) = stack.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        if let Some(pkg) = index.get(&id) {
            for dep in &pkg.dependencies {
                // Entries are "name version"; tolerate the bare-name form
                // Cargo.lock uses when unambiguous.
                let id = match dep.split_once(' ') {
                    Some(_) => dep.clone(),
                    None => match lock.find_package(dep) {
                        Some(p) => format!("{} {}", p.name, p.version),
                        None => continue,
                    },
                };
                stack.push(id);
            }
        }
    }
    seen
}

/// Plans the build for `req`.
pub fn plan(ws: &Workspace, req: &Request) -> Result<Plan> {
    let total = phase("plan");
    let locals = Locals::load(ws)?;
    let (mut resolved, lock) = resolve(ws, &locals, req)?;

    let target_triple = req
        .target
        .map(str::to_string)
        .or_else(|| TargetTriple::host().map(|t| t.0))
        .unwrap_or_else(|| "x86_64-unknown-linux-gnu".to_string());

    // -- Which local packages are needed, and which registry crates that implies.
    let include_dev_for_selected = matches!(req.mode, Mode::Test | Mode::Bench)
        || req.targets.all_targets
        || req.targets.examples
        || req.targets.tests
        || req.targets.benches
        || !req.targets.example.is_empty()
        || !req.targets.test.is_empty()
        || !req.targets.bench.is_empty();

    let selected_locals: HashSet<usize> = req
        .selected
        .iter()
        .filter_map(|&mi| locals.member_of.iter().position(|m| *m == Some(mi)))
        .collect();

    let mut needed_locals: HashSet<usize> = selected_locals.clone();
    let mut registry_seeds: Vec<String> = Vec::new();
    let mut stack: Vec<usize> = selected_locals.iter().copied().collect();
    while let Some(li) = stack.pop() {
        let m = &locals.list[li];
        let member_ws = locals.member_of[li].and(ws.workspace_manifest());
        let with_dev = include_dev_for_selected && selected_locals.contains(&li);
        for dep in m.manifest.dependencies.values() {
            if dep.kind == DependencyKind::Dev && !with_dev {
                continue;
            }
            if !matches_target(dep.target.as_deref(), &target_triple) {
                continue;
            }
            let spec = effective_spec(dep, &m.dir, ws.root(), member_ws);
            if let Some(path) = &spec.path {
                let dir = path.canonicalize().unwrap_or_else(|_| path.clone());
                if let Some(&target_li) = locals.by_dir.get(&dir) {
                    if needed_locals.insert(target_li) {
                        stack.push(target_li);
                    }
                }
            } else {
                let real = spec.package.as_deref().unwrap_or(&dep.name);
                let req_s = spec
                    .version
                    .as_ref()
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "*".to_string());
                if let Some(pkg) = resolved.find_matching(real, &req_s) {
                    registry_seeds.push(format!("{} {}", pkg.name, pkg.version));
                }
            }
        }
    }
    let reachable = reachable_registry(&lock, registry_seeds);

    // -- Download and read the manifests of the registry crates we need.
    let registry_pkgs: Vec<(String, String, Option<String>)> = {
        let mut v: Vec<_> = resolved
            .packages
            .values()
            .filter(|p| {
                matches!(p.source, PackageSource::Registry)
                    && reachable.contains(&format!("{} {}", p.name, p.version))
            })
            .map(|p| (p.name.clone(), p.version.to_string(), p.checksum.clone()))
            .collect();
        v.sort();
        v
    };
    let dl_timer = phase("download");
    let downloader = CrateDownloader::default_downloader().offline(req.offline);
    let extracted_paths = downloader
        .download_and_extract_many(&registry_pkgs)
        .context("Failed to download one or more registry crates")?;
    drop(dl_timer);

    let manifest_timer = phase("read manifests");
    let manifests: Vec<Option<Manifest>> = extracted_paths
        .par_iter()
        .map(|dir| Manifest::from_path(&dir.join("Cargo.toml")).ok())
        .collect();
    drop(manifest_timer);

    // -- Registry edge information, from the extracted manifests + lockfile.
    {
        let mut extracted_dirs: HashMap<(String, String), PathBuf> = HashMap::new();
        for ((name, version, _), dir) in registry_pkgs.iter().zip(&extracted_paths) {
            extracted_dirs.insert((name.clone(), version.clone()), dir.clone());
        }
        // Rebuild edges from manifests when they were not produced by a full
        // resolution (fast path): this is what recovers `package = ...` aliases.
        let needs_repair = resolved
            .packages
            .values()
            .filter(|p| matches!(p.source, PackageSource::Registry))
            .all(|p| p.dependencies.is_empty());
        if needs_repair {
            repair_registry_dependency_aliases(&mut resolved, &lock, &extracted_dirs);
        }
    }
    // -- Registry nodes.
    let mut builder = Builder {
        ws,
        locals: &locals,
        req,
        resolved: &resolved,
        registry_nodes: HashMap::new(),
        lib_nodes: HashMap::new(),
        graph: BuildGraph::new(),
        target_triple: target_triple.clone(),
    };

    let target = req.target.map(|t| TargetTriple(t.to_string()));
    for (((name, version, _), dir), manifest) in
        registry_pkgs.iter().zip(&extracted_paths).zip(&manifests)
    {
        let pkg_meta = manifest.as_ref().and_then(|m| m.package.as_ref());
        let edition = pkg_meta
            .map(|p| p.edition_in(None))
            .and_then(|e| e.parse().ok())
            .unwrap_or(Edition::E2015);
        let lib = manifest.as_ref().and_then(|m| m.lib.as_ref());
        let is_proc_macro = lib.is_some_and(|l| l.proc_macro);
        let lib_src = lib
            .and_then(|l| l.path.clone())
            .unwrap_or_else(|| PathBuf::from("src/lib.rs"));
        let source_root = if dir.join(&lib_src).exists() {
            dir.join(&lib_src)
        } else {
            dir.join("src/lib.rs")
        };
        let features = resolved
            .packages
            .get(&format!("{name} {version}"))
            .map(|p| p.features.clone())
            .unwrap_or_default();
        let parsed_version: semver::Version =
            version.parse().unwrap_or(semver::Version::new(0, 0, 0));

        let unit = CompilationUnit {
            crate_id: CrateId::new(
                name.clone(),
                parsed_version,
                CrateSource::Registry("https://index.crates.io".to_string()),
            ),
            crate_type: if is_proc_macro {
                CrateType::ProcMacro
            } else {
                CrateType::Lib
            },
            source_root,
            manifest_dir: dir.clone(),
            features,
            profile_name: req.profile.name.clone(),
            target: target.clone(),
            edition,
            is_build_script: false,
            check_only: req.mode == Mode::Check,
            links: pkg_meta.and_then(|p| p.links.clone()),
            build_script: pkg_meta.and_then(|p| p.resolve_build_script(dir)),
            meta: pkg_meta
                .map(|p| p.meta_in(None))
                .unwrap_or_else(PackageMeta::default),
            lint_args: Vec::new(),
            target_name: lib.and_then(|l| l.name.clone()).unwrap_or_default(),
            kind: TargetKind::Lib,
            harness: false,
        };
        let id = builder.graph.add_node(unit);
        builder
            .registry_nodes
            .insert(format!("{name} {version}"), id);
    }

    // -- Local library nodes (for every needed local package).
    let mut needed_sorted: Vec<usize> = needed_locals.iter().copied().collect();
    needed_sorted.sort_unstable();
    for &li in &needed_sorted {
        let m = &locals.list[li];
        let Some(lib) = &m.manifest.lib else { continue };
        let is_proc_macro = lib.proc_macro;
        let path = lib
            .path
            .clone()
            .unwrap_or_else(|| PathBuf::from("src/lib.rs"));
        let name = lib.name.clone().unwrap_or_else(|| m.name.replace('-', "_"));
        let unit = builder.unit(
            li,
            TargetKind::Lib,
            &name,
            if is_proc_macro {
                CrateType::ProcMacro
            } else {
                CrateType::Lib
            },
            &path,
            false,
            lib.edition.as_deref(),
        );
        let id = builder.graph.add_node(unit);
        builder.lib_nodes.insert(li, id);
    }
    // Wire library dependencies.
    for &li in &needed_sorted {
        if let Some(&node) = builder.lib_nodes.get(&li) {
            let with_dev = false;
            let _ = with_dev;
            builder.connect(node, li, Deps::NormalAndBuild);
        }
    }

    let registry_ids: Vec<(String, NodeId)> = builder
        .registry_nodes
        .iter()
        .map(|(k, &v)| (k.clone(), v))
        .collect();
    for (pkg_id, node_id) in registry_ids {
        let Some(pkg) = resolved.packages.get(&pkg_id) else {
            continue;
        };
        for dep in &pkg.dependencies {
            if dep.kind == DependencyKind::Dev {
                continue;
            }
            let actual_pkg_name = dep.package.as_deref().unwrap_or(&dep.name);
            let Some(target_pkg) = resolved.find_matching(actual_pkg_name, &dep.req) else {
                continue;
            };
            let Some(&dep_id) = builder
                .registry_nodes
                .get(&format!("{} {}", target_pkg.name, target_pkg.version))
            else {
                continue;
            };
            if dep_id == node_id {
                continue;
            }
            // Only a renamed dependency needs an explicit `--extern` name;
            // otherwise the dependency's own crate name is right (and can
            // differ from its package name).
            let alias = dep.package.as_ref().map(|_| dep.name.as_str());
            builder.graph.add_edge_with_extern(node_id, dep_id, alias);
        }
    }

    // -- Requested targets.
    let mut requested: Vec<Requested> = Vec::new();
    let mut doc_tests: Vec<DocTest> = Vec::new();
    let mut extra_roots: Vec<NodeId> = Vec::new();

    for &mi in req.selected {
        let Some(li) = locals.member_of.iter().position(|m| *m == Some(mi)) else {
            continue;
        };
        let member = &locals.list[li];
        let active = builder.local_features(li);
        let picks = pick_targets(member, req, &active)?;

        // Package binaries are build-order dependencies of tests and benches.
        let mut bin_nodes: HashMap<String, NodeId> = HashMap::new();

        let mut nodes_for_picks: Vec<(usize, NodeId, Vec<(String, NodeId)>)> = Vec::new();
        // First pass: binaries (plain) so tests can depend on them.
        for (pi, pick) in picks.iter().enumerate() {
            let crate_type = match pick.kind {
                TargetKind::Lib => CrateType::Lib,
                _ => CrateType::Bin,
            };
            let proc_macro = pick.kind == TargetKind::Lib
                && member.manifest.lib.as_ref().is_some_and(|l| l.proc_macro);
            let crate_type = if proc_macro {
                CrateType::ProcMacro
            } else {
                crate_type
            };
            let is_plain_lib = pick.kind == TargetKind::Lib && !pick.harness;
            if is_plain_lib {
                // Already created above; just request it.
                if let Some(&n) = builder.lib_nodes.get(&li) {
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
            let unit = builder.unit(
                li,
                pick.kind,
                &pick.name,
                crate_type,
                &pick.path,
                pick.harness,
                pick.spec.as_ref().and_then(|s| s.edition.as_deref()),
            );
            let node = builder.graph.add_node(unit);
            if pick.kind == TargetKind::Bin && !pick.harness {
                bin_nodes.insert(pick.name.clone(), node);
            }
            nodes_for_picks.push((pi, node, Vec::new()));
        }
        // Test/bench units need every package binary built first.
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
                let unit = builder.unit(
                    li,
                    TargetKind::Bin,
                    &name,
                    CrateType::Bin,
                    &path,
                    false,
                    bin.edition.as_deref(),
                );
                let node = builder.graph.add_node(unit);
                bin_nodes.insert(name, node);
                // Wire the binary itself.
                builder.connect(node, li, Deps::NormalAndBuild);
                if let Some(&lib) = builder.lib_nodes.get(&li) {
                    builder.graph.add_edge(node, lib);
                }
            }
        }

        let mut harness_lib_externs: Option<Vec<(String, NodeId)>> = None;
        for (pi, node, _) in nodes_for_picks {
            let pick = &picks[pi];
            let deps_kind = if pick.harness
                || matches!(
                    pick.kind,
                    TargetKind::Example | TargetKind::Test | TargetKind::Bench
                ) {
                Deps::WithDev
            } else {
                Deps::NormalAndBuild
            };
            let mut externs = builder.connect(node, li, deps_kind);
            let own_lib = builder.lib_nodes.get(&li).copied();
            let depends_on_own_lib = !(pick.kind == TargetKind::Lib);
            if depends_on_own_lib {
                if let Some(lib) = own_lib {
                    builder.graph.add_edge(node, lib);
                    externs.push((builder.graph.node(lib).unit.rustc_crate_name(), lib));
                }
            }
            if matches!(pick.kind, TargetKind::Test | TargetKind::Bench) {
                for &bin in bin_nodes.values() {
                    builder.graph.add_edge(node, bin);
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

        // Doctests: need the library and every dependency, dev-dependencies
        // included, as real artifacts.
        if req.mode == Mode::Test
            && req.doctests
            && (req.targets.doc_only || !req.targets.any_explicit() || req.targets.all_targets)
        {
            if let (Some(lib), Some(&lib_node)) = (&member.manifest.lib, builder.lib_nodes.get(&li))
            {
                if lib.doctest && !lib.proc_macro {
                    let mut externs = match harness_lib_externs.take() {
                        Some(e) => e,
                        None => {
                            // No lib harness node (e.g. `--doc`): compute the
                            // dependency list without adding a compile unit.
                            let placeholder = builder.graph.add_node(builder.unit(
                                li,
                                TargetKind::Lib,
                                &lib.name.clone().unwrap_or_default(),
                                CrateType::Lib,
                                Path::new("src/lib.rs"),
                                true,
                                None,
                            ));
                            let e = builder.connect(placeholder, li, Deps::WithDev);
                            // The placeholder exists only to gather edges; its
                            // dependencies become roots so they get built.
                            extra_roots
                                .extend(builder.graph.node(placeholder).dependencies.clone());
                            e
                        }
                    };
                    externs.push((
                        builder.graph.node(lib_node).unit.rustc_crate_name(),
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
                        crate_name: builder.graph.node(lib_node).unit.rustc_crate_name(),
                        externs,
                        features: active.clone(),
                        edition: member.edition,
                    });
                }
            }
        }
    }

    // Explicitly requested names must exist in at least one selected package.
    check_named_targets(req, &locals, ws)?;

    // -- Prune to what was asked for.
    let mut roots: Vec<NodeId> = requested.iter().map(|r| r.node).collect();
    roots.extend(extra_roots.iter().copied());
    let (mut graph, map) = builder.graph.subgraph(&roots);
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
    drop(total);
    Ok(Plan {
        graph,
        requested,
        doc_tests,
    })
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

/// `rune check` marks nearly every unit `check_only` so `build_rustc_command`
/// emits metadata only, skipping codegen and linking. That's unsound for two
/// kinds of node regardless of check mode: a build script is a real executable
/// compiled and run before its owning crate compiles, and a proc-macro is
/// loaded into the compiler process — both need actual linked `.rlib`s (or a
/// dylib) from their dependencies, not just `.rmeta`.
///
/// This walks the graph once, seeded from every build-script-having or
/// proc-macro node, and lifts `check_only` back to `false` across each one's
/// *dependencies* (never the seed itself: a crate that merely *has* a build
/// script still only needs its own code type-checked).
fn exempt_real_link_requirements_from_check(graph: &mut BuildGraph) {
    let mut must_be_real: HashSet<NodeId> = HashSet::new();
    let mut stack: Vec<NodeId> = graph
        .nodes()
        .iter()
        .filter(|n| n.unit.build_script.is_some() || n.unit.crate_type == CrateType::ProcMacro)
        .map(|n| n.id)
        .collect();
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

/// Rebuilds dependency edges (with correct `package = "..."` alias info) for
/// every registry package restored from a lockfile, by reading each package's
/// own manifest from its extracted directory and cross-referencing against the
/// lockfile's already-correct (but alias-blind) `name version` edge list to
/// confirm which of its declared dependencies were actually activated.
fn repair_registry_dependency_aliases(
    resolved_graph: &mut ResolvedGraph,
    lock: &Lockfile,
    extracted_dirs: &HashMap<(String, String), PathBuf>,
) {
    let locked_by_id: HashMap<(String, String), &rune_resolver::LockPackage> = lock
        .packages
        .iter()
        .map(|p| ((p.name.clone(), p.version.clone()), p))
        .collect();

    for pkg in resolved_graph.packages.values_mut() {
        if !matches!(pkg.source, PackageSource::Registry) {
            continue;
        }
        let key = (pkg.name.clone(), pkg.version.to_string());
        let Some(locked) = locked_by_id.get(&key) else {
            continue;
        };
        let Some(src_dir) = extracted_dirs.get(&key) else {
            continue;
        };
        let Ok(manifest) = Manifest::from_path(&src_dir.join("Cargo.toml")) else {
            continue;
        };

        let mut deps = Vec::new();
        let mut seen = HashSet::new();
        // The manifest parser keys `dependencies` by `"kind:target:name"` (so
        // e.g. the unix and wasi variants of a `libc` dependency don't
        // collide); that composite string is a map key, never a name.
        let mut manifest_deps: Vec<&rune_manifest::Dependency> =
            manifest.dependencies.values().collect();
        manifest_deps.sort_by(|a, b| (&a.name, a.kind as u8).cmp(&(&b.name, b.kind as u8)));
        for dep in manifest_deps {
            if dep.kind == DependencyKind::Dev {
                continue;
            }
            if !seen.insert((dep.name.clone(), dep.spec.package.clone(), dep.kind as u8)) {
                continue;
            }
            let actual_name = dep.spec.package.as_deref().unwrap_or(&dep.name);
            let Some(locked_version) =
                locked
                    .dependencies
                    .iter()
                    .find_map(|dep_id| match dep_id.rsplit_once(' ') {
                        Some((n, v)) => (n == actual_name).then(|| v.to_string()),
                        None => (dep_id == actual_name)
                            .then(|| lock.find_package(actual_name).map(|p| p.version.clone()))
                            .flatten(),
                    })
            else {
                continue;
            };
            // A crate can rename the same real package under several
            // mutually-exclusive optional aliases pinned to disjoint version
            // ranges (e.g. signal-hook-mio's `mio-0_6`/`mio-0_7`/`mio-0_8`/
            // `mio-1_0`). Only one alias's range can match the single version
            // present in the lockfile; the others must be dropped or they all
            // become edges to that one locked version.
            if let Some(req) = &dep.spec.version {
                let Ok(version) = locked_version.parse::<semver::Version>() else {
                    continue;
                };
                if !req.matches(&version) {
                    continue;
                }
            }

            deps.push(ResolvedDependencyRef {
                name: dep.name.clone(),
                req: format!("={locked_version}"),
                kind: dep.kind,
                optional: dep.spec.optional,
                features: dep.spec.features.clone(),
                default_features: dep.spec.default_features,
                package: dep.spec.package.clone(),
            });
        }
        pkg.dependencies = deps;
    }
}
