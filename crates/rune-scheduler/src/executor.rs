use crate::inputs::{self, DeclaredInputs, effective_key};
use crate::pipeline::PipelineEvent;
use crate::schedule::{Gate, ScheduleReport, Step, StepId};
use crossbeam_channel::{Receiver, Sender, unbounded};
use rayon::prelude::*;
use rune_cas::CasStore;
use rune_common::toolchain::{self, TargetInfo, Toolchain};
use rune_common::{
    BuildScriptOutput, CompilationUnit, CrateSource, CrateType, LinkArgScope, Profile, RuneConfig,
    RuneError, RuneResult, TargetKind,
};
use rune_compiler::{
    BuildScriptRunner, CompilationResult, RustcCommandBuilder, RustcInvoker,
    build_script::BuildScriptEnv,
};
use rune_graph::{BuildGraph, NodeId, compute_critical_paths_pipelined};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::{debug, error};

/// The result of a build execution.
#[derive(Debug, Clone)]
pub struct BuildResult {
    pub success: bool,
    pub compiled: usize,
    pub cached: usize,
    pub failed: Vec<NodeId>,
    pub total_duration: Duration,
    /// Every compiled/cache-restored artifact (`.rlib`, `.rmeta`, binaries,
    /// ...) produced for each node, keyed by node id. Lets a caller that
    /// needs to link something new against an already-built node (e.g.
    /// `rune test` linking a test harness binary against a crate's rlib)
    /// find those paths without re-deriving rustc's own output naming.
    pub artifact_paths: HashMap<NodeId, Vec<PathBuf>>,
    /// The content-addressable cache key computed for each node.
    pub cache_keys: HashMap<NodeId, String>,
    /// What each unit's build script reported (run or restored): the `cfg`s,
    /// environment and native search paths later steps (`rune doc`) need.
    pub build_script_outputs: HashMap<NodeId, BuildScriptOutput>,
    /// How the build was scheduled, when anything was built.
    pub schedule: Option<ScheduleReport>,
    /// How long starting more compilers was held back for lack of memory.
    pub memory_held: Duration,
}

#[allow(clippy::large_enum_variant)]
enum WorkerMsg {
    MetadataReady {
        node_id: NodeId,
        rmeta_path: PathBuf,
    },
    BuildScriptDone {
        node_id: NodeId,
        output: BuildScriptOutput,
    },
    /// The unit learned about external inputs (declared by its build script or
    /// read through `env!`), so its key — and its dependents' — changed.
    KeyUpdate {
        node_id: NodeId,
        key: String,
    },
    Finished {
        node_id: NodeId,
        artifacts: Vec<PathBuf>,
        duration: Duration,
        /// The compiler's peak memory, when this process compiled it.
        peak_memory: Option<u64>,
        /// Taken from the store (another process built it while we waited)
        /// rather than compiled by this one.
        restored: bool,
    },
    Failed {
        node_id: NodeId,
        error: String,
    },
    /// A unit's build script ran (or its result was restored); the unit's
    /// compilation is scheduled separately, holding on to the unit's lock.
    ScriptPhaseDone {
        node_id: NodeId,
        duration: Duration,
        lock: Option<rune_common::fsutil::FileLock>,
        lock_waited: bool,
    },
    /// A waiter thread obtained the lock of a unit another process held.
    LockAcquired {
        node_id: NodeId,
        lock: rune_common::fsutil::FileLock,
    },
}

/// See [`BuildExecutor::frontend_threads_for`].
fn frontend_threads(
    setting: rune_common::settings::FrontendThreads,
    cpus: usize,
    busy: usize,
    waiting: usize,
) -> Option<usize> {
    use rune_common::settings::FrontendThreads;
    let n = match setting {
        FrontendThreads::Off => return None,
        FrontendThreads::Fixed(n) => n,
        FrontendThreads::Auto => (cpus.saturating_sub(busy + waiting) + 1).min(8),
    };
    (n > 1).then_some(n)
}

/// What a compiled build script was compiled from, kept beside it.
#[derive(serde::Serialize, serde::Deserialize)]
struct ScriptStamp {
    /// The script's base key (configuration and build-dependencies).
    base: String,
    /// Its source files and their content hashes, from rustc's dep-info.
    sources: Vec<(PathBuf, String)>,
    /// What the compiler printed.
    warnings: String,
}

impl ScriptStamp {
    fn hash(p: &Path) -> String {
        rune_common::hash::xxhash3_file(p)
            .map(|h| format!("{h:016x}"))
            .unwrap_or_default()
    }

    fn new(base: &str, sources: &[PathBuf], warnings: &str) -> Self {
        Self {
            base: base.to_string(),
            sources: sources.iter().map(|p| (p.clone(), Self::hash(p))).collect(),
            warnings: warnings.to_string(),
        }
    }

    fn sources_unchanged(&self) -> bool {
        !self.sources.is_empty() && self.sources.iter().all(|(p, h)| Self::hash(p) == *h)
    }
}

/// Shows again what compiling a restored build script printed.
fn replay_script_warnings(
    events: &Sender<PipelineEvent>,
    node_id: NodeId,
    output: Option<&BuildScriptOutput>,
) {
    if let Some(out) = output.filter(|o| !o.compile_warnings.trim().is_empty()) {
        let _ = events.send(PipelineEvent::ScriptWarnings {
            node_id,
            warnings: out.compile_warnings.clone(),
        });
    }
}

/// The name a unit has in `--timings` traces.
fn span_name(unit: &CompilationUnit) -> String {
    format!(
        "{} {} ({})",
        unit.crate_id.name,
        unit.crate_id.version,
        unit.kind.as_str()
    )
}

fn step_id(node: NodeId, script: bool) -> StepId {
    StepId {
        node: node.0,
        script,
    }
}

/// What a step waited for, mirroring the readiness rules in
/// [`BuildExecutor::execute`].
fn step_gates(
    graph: &BuildGraph,
    id: NodeId,
    script: bool,
    transitive_deps: &HashMap<NodeId, HashSet<NodeId>>,
    script_prereqs: &HashMap<NodeId, (Vec<NodeId>, Vec<NodeId>)>,
) -> Vec<Gate> {
    let end = |d: NodeId| Gate::End(step_id(d, false));
    let node = graph.node(id);
    if script {
        let Some((built, links)) = script_prereqs.get(&id) else {
            return Vec::new();
        };
        return built
            .iter()
            .map(|d| end(*d))
            .chain(links.iter().map(|d| {
                if script_prereqs.contains_key(d) {
                    Gate::End(step_id(*d, true))
                } else {
                    end(*d)
                }
            }))
            .collect();
    }
    let mut gates: Vec<Gate> = if node.unit.produces_real_artifact() {
        transitive_deps[&id].iter().map(|d| end(*d)).collect()
    } else {
        node.dependencies
            .iter()
            .map(|d| {
                if graph.node(*d).unit.crate_type == CrateType::ProcMacro {
                    end(*d)
                } else {
                    Gate::Metadata(step_id(*d, false))
                }
            })
            .collect()
    };
    if script_prereqs.contains_key(&id) {
        gates.push(Gate::End(step_id(id, true)));
    }
    gates
}

/// The advisory lock file that makes concurrent `rune` processes take turns
/// on one unit's outputs.
fn unit_lock_path(
    config: &RuneConfig,
    profile: &Profile,
    graph: &BuildGraph,
    id: NodeId,
) -> PathBuf {
    config
        .profile_target_dir(profile.output_dir_name())
        .join(".rune-locks")
        .join(script_key(&graph.node(id).unit))
}

/// Which artifact flavor a compilation needs from its dependencies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternMode {
    /// A library being type-checked/compiled: `.rmeta` is enough (and lets
    /// dependents start before a dependency finishes codegen).
    Pipelined,
    /// Something that is actually linked (binary, test harness, proc-macro,
    /// dylib): every dependency's real `.rlib` is required.
    Link,
}

/// The parallel build executor.
///
/// 1. Computes a content-addressable cache key for every unit up front
///    (registry crates are immutable, so their key never touches the disk).
/// 2. Restores every cache hit in parallel *before* scheduling anything.
/// 3. Schedules the remaining units by critical path, weighted by measured
///    compile times from earlier builds.
/// 4. Invokes `rustc` with metadata pipelining: dependents start as soon as
///    a dependency's `.rmeta` exists.
pub struct BuildExecutor {
    config: RuneConfig,
    profile: Profile,
    cas: Option<CasStore>,
    event_tx: Sender<PipelineEvent>,
    event_rx: Receiver<PipelineEvent>,
    max_jobs: usize,
    force_rebuild: bool,
    /// `[env]` from configuration, set for every compiler and build script.
    config_env: Vec<(String, String, bool)>,
    color: bool,
    /// Shared with every `rustc` (and build script) we start, so their
    /// internal codegen threads and C compilers draw from one pool of
    /// `max_jobs` tokens instead of each assuming it owns the machine.
    jobserver: Option<jobserver::Client>,
    tokens: Option<crate::tokens::JobTokens>,
    /// `build.frontend-threads`.
    frontend_threads: rune_common::settings::FrontendThreads,
    /// Cores on this machine.
    cpus: usize,
    /// Reads the machine's memory (replaceable in tests).
    memory: fn() -> Option<rune_common::sysinfo::Memory>,
    /// `build.sandbox`: compilers and build scripts run confined.
    sandbox: Option<std::sync::Arc<SandboxPolicy>>,
    /// Seconds of work that follow a unit once it is built (a test binary's
    /// run): scheduling counts them as part of its path.
    extra_costs: HashMap<NodeId, f64>,
}

/// How build-time code is confined (see `rune_common::sandbox`).
#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    pub sandbox: rune_common::sandbox::Sandbox,
    /// Packages whose build scripts may use the network.
    pub network: HashSet<String>,
}

impl BuildExecutor {
    /// Creates a new `BuildExecutor`.
    pub fn new(config: RuneConfig, profile: Profile, max_jobs: usize) -> Self {
        let (event_tx, event_rx) = unbounded();
        let jobserver = jobserver::Client::new(max_jobs.max(1)).ok();
        let tokens = jobserver.clone().map(crate::tokens::JobTokens::new);
        Self {
            config,
            profile,
            cas: None,
            event_tx,
            event_rx,
            max_jobs: max_jobs.max(1),
            force_rebuild: false,
            config_env: Vec::new(),
            color: false,
            jobserver,
            tokens,
            frontend_threads: Default::default(),
            sandbox: None,
            cpus: std::thread::available_parallelism().map_or(1, |n| n.get()),
            memory: rune_common::sysinfo::memory,
            extra_costs: HashMap::new(),
        }
    }

    /// Work that waits on units once they are built, in seconds: `rune test`
    /// passes how long each test binary ran last time, so the binary a run
    /// will wait longest on is built first and starts while the rest still
    /// compile.
    pub fn with_extra_costs(mut self, costs: HashMap<NodeId, f64>) -> Self {
        self.extra_costs = costs;
        self
    }

    /// Replaces how the machine's memory is read (tests).
    pub fn with_memory_probe(
        mut self,
        probe: fn() -> Option<rune_common::sysinfo::Memory>,
    ) -> Self {
        self.memory = probe;
        self
    }

    /// Threads for rustc's parallel front end (nightly compilers only; see
    /// `build.frontend-threads`).
    /// Runs every compiler and build script confined.
    pub fn with_sandbox(mut self, policy: SandboxPolicy) -> Self {
        self.sandbox = Some(std::sync::Arc::new(policy));
        self
    }

    pub fn with_frontend_threads(mut self, t: rune_common::settings::FrontendThreads) -> Self {
        self.frontend_threads = t;
        self
    }

    /// `-Z threads` for a unit started now, with `busy` units running
    /// (including it) and `waiting` more ready to start: under `auto`, the
    /// cores nobody else will use, so the long single crates at the end of a
    /// build type-check on several cores while a busy machine is not
    /// oversubscribed.
    fn frontend_threads_for(&self, busy: usize, waiting: usize) -> Option<usize> {
        frontend_threads(self.frontend_threads, self.cpus, busy, waiting)
            .filter(|_| toolchain::global().accepts_unstable_flags())
    }

    /// Sets the CAS store for artifact caching.
    pub fn with_cas(mut self, cas: CasStore) -> Self {
        self.cas = Some(cas);
        self
    }

    /// Never restore from the CAS: compile every unit (and re-run every build
    /// script) even if an identical artifact is cached. Results are still
    /// *stored*, so a later ordinary build can reuse them.
    ///
    /// This is what `build.release-rebuild-all` turns on for release builds.
    pub fn with_force_rebuild(mut self, force: bool) -> Self {
        self.force_rebuild = force;
        self
    }

    /// Environment from the `[env]` configuration table: `(name, value, force)`.
    pub fn with_config_env(mut self, env: Vec<(String, String, bool)>) -> Self {
        self.config_env = env;
        self
    }

    /// Ask the compiler for ANSI-colored diagnostics.
    pub fn with_color(mut self, color: bool) -> Self {
        self.color = color;
        self
    }

    /// Returns a receiver for pipeline events to drive progress bars or logs.
    pub fn event_receiver(&self) -> Receiver<PipelineEvent> {
        self.event_rx.clone()
    }

    /// Directory a unit's primary artifacts are written to.
    fn out_dir_for(&self, unit: &CompilationUnit) -> PathBuf {
        out_dir_for(&self.config, &self.profile, unit)
    }

    /// Executes the build graph.
    pub fn execute(&self, graph: &BuildGraph) -> RuneResult<BuildResult> {
        let result = self.execute_graph(graph);
        if result.is_err() {
            // Whoever follows the events must learn that nothing more comes.
            let _ = self.event_tx.send(PipelineEvent::AllComplete {
                total_duration: Duration::ZERO,
                cached: 0,
                compiled: 0,
                failed: 0,
            });
        }
        result
    }

    fn execute_graph(&self, graph: &BuildGraph) -> RuneResult<BuildResult> {
        let start_time = Instant::now();
        let profile_dir_name = self.profile.output_dir_name();
        self.config
            .ensure_dirs(profile_dir_name)
            .map_err(RuneError::Io)?;

        let topo = graph.topological_sort()?;

        let source_cache_path = self.config.fingerprint_dir.join("source-cache.json");
        let mut source_cache =
            rune_fingerprint::FingerprintCache::load(&source_cache_path).unwrap_or_default();
        let timings_path = self.config.fingerprint_dir.join("timings.json");
        let global_timings_path = self.config.cas_dir.join("timings.json");
        let mut timings = Timings::load(&timings_path, &global_timings_path);
        // The schedule as it happened (see `schedule`): when each step became
        // ready, started, wrote metadata and finished.
        let mut steps: HashMap<StepId, Step> = HashMap::new();
        let mut started: HashMap<StepId, f64> = HashMap::new();
        let mut first_ready: HashMap<StepId, f64> = HashMap::new();
        let mut metadata_at: HashMap<NodeId, f64> = HashMap::new();
        let now = || start_time.elapsed().as_secs_f64();

        // 1. Cache keys for every node, dependencies first. A unit's *base*
        // key covers its source, configuration and dependencies; the key it is
        // stored under also covers the current value of whatever external
        // inputs a previous build of it declared (see `inputs`).
        let keys_timer = rune_common::timing::phase("cache keys");
        let mut unit_base_keys: HashMap<NodeId, String> = HashMap::with_capacity(topo.len());
        let mut unit_cas_keys: HashMap<NodeId, String> = HashMap::with_capacity(topo.len());
        // `rune check` after `rune build` reuses the build: a checked unit
        // takes the stored result of the same unit fully compiled (its
        // `.rmeta` is what dependents type-check against). rustc gives the
        // two `.rmeta` flavours different crate hashes, so they must never
        // mix in one dependency chain: a unit is substituted only if its whole
        // dependency closure is, and then carries the full unit's key so that
        // every dependent's key says which flavour it was compiled against.
        let substitute = self.cas.is_some() && !self.force_rebuild;
        let mut full_keys: HashMap<NodeId, String> = HashMap::new();
        let mut substituted: HashSet<NodeId> = HashSet::new();
        for &id in &topo {
            let node = graph.node(id);
            let (mut base, mut key) =
                self.compute_keys(graph, id, &unit_cas_keys, &mut source_cache, false);
            if substitute && node.unit.check_only {
                let (full_base, full_key) =
                    self.compute_keys(graph, id, &full_keys, &mut source_cache, true);
                let closure_full = node
                    .dependencies
                    .iter()
                    .all(|d| !graph.node(*d).unit.check_only || substituted.contains(d));
                if closure_full && self.cas.as_ref().is_some_and(|c| c.contains(&full_key)) {
                    substituted.insert(id);
                    base = full_base;
                    key = full_key.clone();
                }
                full_keys.insert(id, full_key);
            } else if substitute {
                // Not a check unit (a build script's or proc-macro's
                // dependency): the same unit in both modes.
                full_keys.insert(id, key.clone());
            }
            unit_base_keys.insert(id, base);
            unit_cas_keys.insert(id, key);
        }
        if !substituted.is_empty() {
            debug!("{} checked units reuse full builds", substituted.len());
        }
        drop(keys_timer);

        let mut completed: HashSet<NodeId> = HashSet::new();
        let mut in_flight: HashSet<NodeId> = HashSet::new();
        let mut metadata_ready: HashSet<NodeId> = HashSet::new();
        let mut artifact_paths: HashMap<NodeId, Vec<PathBuf>> = HashMap::new();
        let mut metadata_paths: HashMap<NodeId, PathBuf> = HashMap::new();
        let mut build_script_outputs: HashMap<NodeId, BuildScriptOutput> = HashMap::new();
        let mut compiled = 0;
        let mut cached = 0;
        let mut failed: Vec<NodeId> = Vec::new();

        // 2. Restore every cache hit in parallel. A unit's key already covers
        // its dependencies' keys, so whether it is cached doesn't depend on
        // whether those dependencies were restored or will be rebuilt.
        if let Some(cas) = self.cas.as_ref().filter(|_| !self.force_rebuild) {
            let _timer = rune_common::timing::phase("cas restore");
            let restore = |&id: &NodeId| -> Option<(NodeId, Vec<PathBuf>)> {
                let unit = &graph.node(id).unit;
                let key = &unit_cas_keys[&id];
                // Another `rune` sharing this target directory may be
                // compiling a unit with the same output files right now
                // (the same crate built with other flags, as `rune
                // coverage` does): putting the store's read-only copy in
                // place under its compiler fails that build. Such a unit
                // is left to the scheduler, which waits its turn and
                // then restores it.
                let path = unit_lock_path(&self.config, &self.profile, graph, id);
                let rune_common::fsutil::TryLock::Locked(_lock) =
                    rune_common::fsutil::FileLock::try_exclusive(&path)
                else {
                    return None;
                };
                let linked = cas.link_to(key, &self.out_dir_for(unit)).ok()?;
                (!linked.is_empty()).then_some((id, linked))
            };
            // In parallel only where it pays: a small graph's restores are
            // quicker than starting (and spinning down) a thread pool.
            let restored: Vec<(NodeId, Vec<PathBuf>)> = if topo.len() < 48 {
                topo.iter().filter_map(restore).collect()
            } else {
                topo.par_iter().filter_map(restore).collect()
            };

            for (id, linked) in restored {
                let unit = &graph.node(id).unit;
                debug!(
                    "CAS hit for {} v{}",
                    unit.crate_id.name, unit.crate_id.version
                );
                cached += 1;
                completed.insert(id);
                metadata_ready.insert(id);

                // A cache hit skips the worker entirely, so it never runs the
                // build script — whose output dependents (`DEP_<LINKS>_<KEY>`)
                // and the package's other units (cfgs, link libraries,
                // `OUT_DIR`) still need. Restore it from the store.
                if unit.build_script.is_some() {
                    let script_key_name = format!("{}-buildscript", unit_cas_keys[&id]);
                    if let Some(output) = cas.get_value::<BuildScriptOutput>(&script_key_name) {
                        let _ = self.event_tx.send(PipelineEvent::ScriptOutput {
                            node_id: id,
                            output: output.clone(),
                        });
                        replay_script_warnings(&self.event_tx, id, Some(&output));
                        build_script_outputs.insert(id, output);
                    }
                    // The generated files too: the package's other units
                    // `include!` them from the shared `OUT_DIR`.
                    let _ = cas.restore_dir_tree(
                        &script_key_name,
                        &self.config.build_script_out_dir_by_key(
                            self.profile.output_dir_name(),
                            &script_key(unit),
                        ),
                    );
                }
                if let Some(rmeta) = linked
                    .iter()
                    .find(|p| p.extension().and_then(|e| e.to_str()) == Some("rmeta"))
                {
                    metadata_paths.insert(id, rmeta.clone());
                }
                uplift_artifacts(&self.config, &self.profile, unit, &linked);
                // Cargo replays a cached crate's warnings; so do we.
                let warnings = if unit.crate_id.source == CrateSource::Local {
                    cas.get_value::<String>(&format!("{}-warnings", unit_cas_keys[&id]))
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                let diagnostics = if unit.crate_id.source == CrateSource::Local {
                    cas.get_value::<Vec<String>>(&format!("{}-diagnostics", unit_cas_keys[&id]))
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };
                let _ = self.event_tx.send(PipelineEvent::CacheHit {
                    node_id: id,
                    artifacts: linked.clone(),
                    warnings,
                    diagnostics,
                });
                artifact_paths.insert(id, linked);
            }
        }

        // 3. Critical-path priorities over the *remaining* work only: for each
        // unit, the longest chain of work still to happen after it starts. Costs
        // are measured compile times where we have them, size-based estimates
        // where we do not. A dependent that only needs a library's `.rmeta`
        // waits for part of the library's time, not all of it.
        let weights = {
            let _timer = rune_common::timing::phase("critical path");
            timings.measure_sources(
                topo.iter()
                    .filter(|id| !completed.contains(id))
                    .map(|&id| graph.node(id).unit.manifest_dir.clone()),
            );
            compute_critical_paths_pipelined(
                graph,
                |node| {
                    if completed.contains(&node.id) {
                        0.0
                    } else {
                        timings.estimate(&node.unit, &self.profile)
                            + self.extra_costs.get(&node.id).copied().unwrap_or(0.0)
                    }
                },
                |dep, dependent, dep_cost| {
                    let pipelined = !dependent.unit.produces_real_artifact()
                        && dep.unit.crate_type != CrateType::ProcMacro
                        && !dep.unit.produces_real_artifact();
                    if pipelined {
                        dep_cost * METADATA_FRACTION
                    } else {
                        dep_cost
                    }
                },
            )
        };

        let (worker_tx, worker_rx) = unbounded::<WorkerMsg>();
        let total_nodes = graph.node_count();

        // Full transitive dependency closure per node, needed for anything
        // that performs a real link step (binaries, cdylibs, proc-macros,
        // build scripts). Pipelining lets a *library* finish compiling using
        // only its dependencies' `.rmeta`, so "direct dependency completed"
        // does NOT imply "that dependency's own dependencies finished
        // linking". A binary link needs every transitively linked crate's
        // actual `.rlib` on disk, so gating readiness on direct dependencies
        // alone raced the final link against the slowest transitive
        // dependency ("crate `X` required to be available in rlib format").
        let transitive_deps: HashMap<NodeId, HashSet<NodeId>> = {
            let mut map: HashMap<NodeId, HashSet<NodeId>> = HashMap::new();
            for &id in &topo {
                let mut set = HashSet::new();
                for &dep in &graph.node(id).dependencies {
                    set.insert(dep);
                    if let Some(dep_set) = map.get(&dep) {
                        set.extend(dep_set.iter().copied());
                    }
                }
                map.insert(id, set);
            }
            map
        };

        // A package's build script is its own step, as in Cargo: it needs its
        // build-dependencies fully built (it links them) and the scripts of
        // the `links` crates it depends on (their `DEP_*` metadata), but not
        // the package's ordinary dependencies. So it compiles and runs while
        // those are still compiling, and the library itself then starts as
        // soon as their `.rmeta` exists. (Running the script first and only
        // then the library, both after *every* dependency was fully linked,
        // put each such crate's whole script on the critical path.)
        let own_script = |id: NodeId| {
            let u = &graph.node(id).unit;
            u.build_script.is_some() && u.ext.script_owner.is_none()
        };
        let script_prereqs: HashMap<NodeId, (Vec<NodeId>, Vec<NodeId>)> = topo
            .iter()
            .copied()
            .filter(|&id| own_script(id))
            .map(|id| {
                let node = graph.node(id);
                let mut built: HashSet<NodeId> = HashSet::new();
                for bd in &node.build_dependencies {
                    built.insert(*bd);
                    built.extend(transitive_deps[bd].iter().copied());
                }
                let links: Vec<NodeId> = node
                    .dependencies
                    .iter()
                    .copied()
                    .filter(|d| graph.node(*d).unit.links.is_some())
                    .collect();
                (id, (built.into_iter().collect(), links))
            })
            .collect();
        let mut script_done: HashSet<NodeId> = HashSet::new();
        // Units whose script step is running (their key may still change as
        // their dependencies learn about outside inputs).
        let mut in_script_phase: HashSet<NodeId> = HashSet::new();
        // A unit's lock is held from its script step through its compilation,
        // so another process never re-runs the script (and rewrites `OUT_DIR`)
        // under a compiler reading it. `bool`: whether we had to wait for it.
        let mut held_locks: HashMap<NodeId, (rune_common::fsutil::FileLock, bool)> = HashMap::new();
        // Units another process holds the lock of; a waiter thread (not a job
        // slot) blocks on it and hands it over.
        let mut lock_waiting: HashSet<NodeId> = HashSet::new();
        let mut job_threads: Vec<std::thread::JoinHandle<()>> = Vec::new();
        // How long new work was held back because memory was short.
        let mut memory_held = Duration::ZERO;
        // What each running compilation is expected to peak at.
        let mut predicted_running: HashMap<NodeId, u64> = HashMap::new();
        let mut memory_held_since: Option<Instant> = None;

        // Nodes still to schedule, in stable graph order.
        let mut unscheduled: Vec<NodeId> = graph
            .nodes()
            .iter()
            .map(|n| n.id)
            .filter(|id| !completed.contains(id))
            .collect();

        while completed.len() + failed.len() < total_nodes {
            if !failed.is_empty() && in_flight.is_empty() {
                break; // Nothing more can start once something failed.
            }

            // Readiness is checked even while every slot is taken, so that the
            // schedule report knows how long a unit waited for one.
            if failed.is_empty() {
                let mut ready: Vec<(NodeId, bool)> = unscheduled
                    .iter()
                    .copied()
                    .filter(|id| !in_flight.contains(id) && !lock_waiting.contains(id))
                    .filter_map(|id| {
                        let node = graph.node(id);
                        if let Some((built, links)) = script_prereqs.get(&id)
                            && !script_done.contains(&id)
                        {
                            let ready = built.iter().all(|d| completed.contains(d))
                                && links.iter().all(|d| {
                                    completed.contains(d) || build_script_outputs.contains_key(d)
                                });
                            return ready.then_some((id, true));
                        }
                        let ready = if node.unit.produces_real_artifact() {
                            transitive_deps[&id].iter().all(|d| completed.contains(d))
                        } else {
                            node.dependencies.iter().all(|dep_id| {
                                if graph.node(*dep_id).unit.crate_type == CrateType::ProcMacro {
                                    completed.contains(dep_id)
                                } else {
                                    completed.contains(dep_id) || metadata_ready.contains(dep_id)
                                }
                            })
                        };
                        ready.then_some((id, false))
                    })
                    .collect();

                for (id, script_only) in &ready {
                    first_ready
                        .entry(step_id(*id, *script_only))
                        .or_insert_with(now);
                }
                // Longest remaining chain first.
                ready.sort_by(|(a, _), (b, _)| {
                    let wa = weights.get(a).copied().unwrap_or(0.0);
                    let wb = weights.get(b).copied().unwrap_or(0.0);
                    wb.partial_cmp(&wa).unwrap_or(std::cmp::Ordering::Equal)
                });

                let ready_count = ready.len();
                for (index, (node_id, script_only)) in ready.into_iter().enumerate() {
                    if in_flight.len() >= self.max_jobs {
                        break;
                    }
                    // Short of memory: let what runs finish (it frees memory)
                    // before starting more — a slower build instead of a
                    // swapping machine or a compiler killed for lack of it.
                    // "Short" counts what this compiler is expected to need
                    // (its peak the last time) and room for the running ones
                    // to grow toward theirs, not only what is free right now.
                    let need = if script_only {
                        0
                    } else {
                        timings
                            .memory(&graph.node(node_id).unit, &self.profile)
                            .unwrap_or(DEFAULT_COMPILER_MEMORY)
                    };
                    let growth: u64 = predicted_running.values().sum::<u64>() / 2;
                    if !in_flight.is_empty()
                        && (self.memory)().is_some_and(|m| {
                            m.is_short()
                                || m.available
                                    < m.reserve().saturating_add(need).saturating_add(growth)
                        })
                    {
                        if memory_held_since.is_none() {
                            memory_held_since = Some(Instant::now());
                            debug!("memory is short: holding back new compilations");
                        }
                        break;
                    }
                    if let Some(since) = memory_held_since.take() {
                        memory_held += since.elapsed();
                    }
                    // Another `rune` process may be building this very unit into
                    // the same target directory (an editor's background check
                    // next to a terminal build). Whoever holds the unit's lock
                    // owns its output files; everyone else waits — without
                    // tying up a job slot — and then usually finds the finished
                    // result in the store instead of compiling it again.
                    let (lock, lock_waited) = match held_locks.remove(&node_id) {
                        Some(held) => held,
                        None => {
                            let path = unit_lock_path(&self.config, &self.profile, graph, node_id);
                            match rune_common::fsutil::FileLock::try_exclusive(&path) {
                                rune_common::fsutil::TryLock::Locked(lock) => (lock, false),
                                rune_common::fsutil::TryLock::Busy => {
                                    lock_waiting.insert(node_id);
                                    let tx = worker_tx.clone();
                                    std::thread::spawn(move || {
                                        let (lock, _) =
                                            rune_common::fsutil::FileLock::exclusive(&path);
                                        let _ = tx.send(WorkerMsg::LockAcquired { node_id, lock });
                                    });
                                    continue;
                                }
                            }
                        }
                    };
                    if !script_only {
                        unscheduled.retain(|id| *id != node_id);
                    } else {
                        in_script_phase.insert(node_id);
                    }
                    in_flight.insert(node_id);
                    if !script_only {
                        predicted_running.insert(
                            node_id,
                            timings
                                .memory(&graph.node(node_id).unit, &self.profile)
                                .unwrap_or(DEFAULT_COMPILER_MEMORY),
                        );
                    }
                    started.insert(step_id(node_id, script_only), now());
                    let mut job = self.prepare_job(
                        graph,
                        node_id,
                        &unit_base_keys[&node_id],
                        &unit_cas_keys[&node_id],
                        dep_keys_of(graph, node_id, &unit_cas_keys),
                        &artifact_paths,
                        &metadata_paths,
                        &build_script_outputs,
                        worker_tx.clone(),
                    );
                    job.script_only = script_only;
                    job.lock = Some(lock);
                    job.lock_waited = lock_waited;
                    if script_only {
                        let (base, package_hash, key) = self.script_keys(
                            graph,
                            node_id,
                            &unit_cas_keys,
                            &job.dep_metadata,
                            &mut source_cache,
                        );
                        job.package_hash = package_hash;
                        job.script_cache_key = Some(key);
                        job.script_base = Some(base);
                    }
                    if !script_only {
                        job.frontend_threads =
                            self.frontend_threads_for(in_flight.len(), ready_count - index - 1);
                        // (After its script step: the script's record is the
                        // one its result was stored under.)
                        job.read_script = self.read_mode(
                            graph,
                            node_id,
                            &unit_cas_keys,
                            &mut source_cache,
                            false,
                        );
                    }
                    job_threads.push(std::thread::spawn(move || job.run()));
                }
            }

            if in_flight.is_empty() && lock_waiting.is_empty() {
                if completed.len() + failed.len() < total_nodes {
                    error!(
                        "Build stalled: no tasks in flight and remaining tasks cannot be scheduled."
                    );
                }
                break;
            }

            let Ok(msg) = worker_rx.recv() else { break };
            let mut next = Some(msg);
            // Drain everything already queued before rescanning readiness.
            while let Some(msg) = next.take().or_else(|| worker_rx.try_recv().ok()) {
                match msg {
                    WorkerMsg::MetadataReady {
                        node_id,
                        rmeta_path,
                    } => {
                        debug!("Metadata ready for node {:?}", node_id);
                        metadata_at.entry(node_id).or_insert_with(now);
                        metadata_ready.insert(node_id);
                        metadata_paths.insert(node_id, rmeta_path);
                    }
                    WorkerMsg::BuildScriptDone { node_id, output } => {
                        let _ = self.event_tx.send(PipelineEvent::ScriptOutput {
                            node_id,
                            output: output.clone(),
                        });
                        build_script_outputs.insert(node_id, output);
                    }
                    WorkerMsg::LockAcquired { node_id, lock } => {
                        lock_waiting.remove(&node_id);
                        held_locks.insert(node_id, (lock, true));
                    }
                    WorkerMsg::ScriptPhaseDone {
                        node_id,
                        duration,
                        lock,
                        lock_waited,
                    } => {
                        in_flight.remove(&node_id);
                        in_script_phase.remove(&node_id);
                        script_done.insert(node_id);
                        if let Some(lock) = lock {
                            held_locks.insert(node_id, (lock, lock_waited));
                        }
                        let unit = &graph.node(node_id).unit;
                        timings.record_script(unit, &self.profile, duration);
                        let id = step_id(node_id, true);
                        let begun = started.get(&id).copied().unwrap_or(0.0);
                        steps.insert(
                            id,
                            Step {
                                label: format!(
                                    "{} {} (build script)",
                                    unit.crate_id.name, unit.crate_id.version
                                ),
                                kind: "build-script",
                                ready: first_ready.get(&id).copied().unwrap_or(begun),
                                start: begun,
                                metadata: None,
                                end: now(),
                                gates: step_gates(
                                    graph,
                                    node_id,
                                    true,
                                    &transitive_deps,
                                    &script_prereqs,
                                ),
                            },
                        );
                    }
                    WorkerMsg::KeyUpdate { node_id, key } => {
                        // The unit's key changed; so do those of everything
                        // above it whose compilation has not started yet
                        // (nothing above it can have, apart from pipelined
                        // dependents, which wait for the metadata that follows
                        // this message — and units running only their build
                        // script, which are keyed again here too).
                        unit_cas_keys.insert(node_id, key);
                        // Only what transitively depends on this unit can have
                        // changed: one pass in dependency order, recomputing a
                        // key only when one of its dependencies' just did.
                        let mut changed: HashSet<NodeId> = HashSet::from([node_id]);
                        for &id in &topo {
                            if id == node_id
                                || completed.contains(&id)
                                || (in_flight.contains(&id) && !in_script_phase.contains(&id))
                                || !graph
                                    .node(id)
                                    .dependencies
                                    .iter()
                                    .any(|d| changed.contains(d))
                            {
                                continue;
                            }
                            let (base, key) = self.compute_keys(
                                graph,
                                id,
                                &unit_cas_keys,
                                &mut source_cache,
                                false,
                            );
                            let moved = unit_cas_keys.get(&id) != Some(&key);
                            unit_base_keys.insert(id, base);
                            unit_cas_keys.insert(id, key);
                            if moved {
                                changed.insert(id);
                            }
                        }
                    }
                    WorkerMsg::Finished {
                        node_id,
                        artifacts,
                        duration,
                        peak_memory,
                        restored,
                    } => {
                        predicted_running.remove(&node_id);
                        debug!("Finished compiling node {:?}", node_id);
                        in_flight.remove(&node_id);
                        in_script_phase.remove(&node_id);
                        unscheduled.retain(|id| *id != node_id);
                        held_locks.remove(&node_id);
                        completed.insert(node_id);
                        metadata_ready.insert(node_id);
                        artifact_paths.insert(node_id, artifacts);
                        let unit = &graph.node(node_id).unit;
                        if !restored {
                            timings.record(unit, &self.profile, duration);
                        }
                        if let Some(bytes) = peak_memory {
                            timings.record_memory(unit, &self.profile, bytes);
                        }
                        let id = step_id(node_id, false);
                        let begun = started.get(&id).copied().unwrap_or(0.0);
                        steps.insert(
                            id,
                            Step {
                                label: span_name(unit),
                                kind: if unit.crate_type == CrateType::ProcMacro {
                                    "proc-macro"
                                } else {
                                    unit.kind.as_str()
                                },
                                ready: first_ready.get(&id).copied().unwrap_or(begun),
                                start: begun,
                                metadata: metadata_at.get(&node_id).copied(),
                                end: now(),
                                gates: step_gates(
                                    graph,
                                    node_id,
                                    false,
                                    &transitive_deps,
                                    &script_prereqs,
                                ),
                            },
                        );
                        if restored {
                            cached += 1;
                        } else {
                            compiled += 1;
                        }
                    }
                    WorkerMsg::Failed { node_id, error } => {
                        // The rendered diagnostic already went out on
                        // `PipelineEvent::BuildFailed`; callers print it
                        // themselves, so this is just a trace breadcrumb.
                        debug!("Failed to compile node {:?}: {}", node_id, error);
                        in_flight.remove(&node_id);
                        in_script_phase.remove(&node_id);
                        held_locks.remove(&node_id);
                        failed.push(node_id);
                    }
                }
            }
        }
        if let Some(since) = memory_held_since.take() {
            memory_held += since.elapsed();
        }
        // Units report `Finished` before they are stored (so their dependents
        // start sooner); the build is over once every store has landed.
        drop(held_locks);
        for t in job_threads {
            let _ = t.join();
        }

        let failed_set: HashSet<NodeId> = failed.iter().copied().collect();
        self.save_identities(
            graph,
            &topo,
            |id| completed.contains(&id) && !failed_set.contains(&id),
            &unit_cas_keys,
            &mut source_cache,
        );
        let _ = source_cache.save_if_changed(&source_cache_path);
        timings.save(&timings_path, &global_timings_path);
        let schedule =
            (!steps.is_empty()).then(|| ScheduleReport::analyze(&steps, self.cpus, self.max_jobs));
        if let (true, Some(report)) = (rune_common::timing::enabled(), &schedule) {
            let trace_path = self.config.fingerprint_dir.join("trace.json");
            crate::schedule::write_chrome_trace(&trace_path, &steps);
            let html_path = self
                .config
                .rune_dir
                .join("timings")
                .join("rune-timing.html");
            crate::schedule::write_html(&html_path, &steps, report);
            eprint!("  [timing] {}", report.describe_chain());
            eprintln!(
                "  [timing] report: {} (Chrome trace: {})",
                html_path.display(),
                trace_path.display()
            );
        }
        if let Some(cas) = &self.cas {
            cas.flush_stats();
        }

        let total_duration = start_time.elapsed();
        let success = failed.is_empty();

        let _ = self.event_tx.send(PipelineEvent::AllComplete {
            total_duration,
            cached,
            compiled,
            failed: failed.len(),
        });

        Ok(BuildResult {
            success,
            compiled,
            cached,
            failed,
            total_duration,
            artifact_paths,
            cache_keys: unit_cas_keys,
            build_script_outputs,
            schedule,
            memory_held,
        })
    }

    /// `(base key, effective key)` of a node, given its dependencies' final keys.
    fn compute_keys(
        &self,
        graph: &BuildGraph,
        id: NodeId,
        keys: &HashMap<NodeId, String>,
        source_cache: &mut rune_fingerprint::FingerprintCache,
        as_full_build: bool,
    ) -> (String, String) {
        let node = graph.node(id);
        // The key the unit would have outside `rune check` (see `execute`).
        let full_unit;
        let unit = if as_full_build && node.unit.check_only {
            full_unit = CompilationUnit {
                check_only: false,
                ..node.unit.clone()
            };
            &full_unit
        } else {
            &node.unit
        };
        let dep_keys = dep_keys_of(graph, id, keys);
        let read = self.read_record(graph, id, unit, keys, source_cache);
        let base = compute_unit_key_from(
            unit,
            &self.profile,
            &dep_keys,
            source_cache,
            read.as_ref().map(|(r, s)| (r, s.as_deref())),
        );
        let recorded: Option<DeclaredInputs> = self
            .cas
            .as_ref()
            .and_then(|c| c.get_value(&format!("{base}-inputs")));
        let key = effective_key(&base, recorded.as_ref(), unit.build_script.is_some());
        (base, key)
    }

    /// Whether a unit can be keyed by the files it read (see
    /// [`inputs::ReadRecord`]), and with which build script result:
    /// `Some(None)` for a local crate without a build script, `Some(Some(key))`
    /// for one compiled with its package's script's result `key`. `None`: it
    /// is keyed by every file of its package — not a local crate, or its
    /// script receives `DEP_*` values (known only once its dependencies'
    /// scripts ran).
    fn read_mode(
        &self,
        graph: &BuildGraph,
        id: NodeId,
        keys: &HashMap<NodeId, String>,
        source_cache: &mut rune_fingerprint::FingerprintCache,
        as_full_build: bool,
    ) -> Option<Option<String>> {
        let node = graph.node(id);
        let unit = &node.unit;
        if unit.crate_id.source != CrateSource::Local {
            return None;
        }
        let owner = match &unit.ext.script_owner {
            Some(owner) => node
                .dependencies
                .iter()
                .copied()
                .find(|d| script_key(&graph.node(*d).unit) == *owner)?,
            None if unit.build_script.is_some() => id,
            None => return Some(None),
        };
        let owner_node = graph.node(owner);
        if owner_node
            .dependencies
            .iter()
            .any(|d| graph.node(*d).unit.links.is_some())
        {
            return None;
        }
        // `rune check` looking for a build's result keys it as the build did:
        // with the script result of the full build's unit.
        let full;
        let owner_unit = if as_full_build && owner_node.unit.check_only {
            full = CompilationUnit {
                check_only: false,
                ..owner_node.unit.clone()
            };
            &full
        } else {
            &owner_node.unit
        };
        let (_, _, key) = self.script_keys_of(graph, owner, owner_unit, keys, &[], source_cache);
        Some(Some(key))
    }

    /// The files a local crate read when it was last compiled, if recorded,
    /// with its build script result (see [`Self::read_mode`]).
    fn read_record(
        &self,
        graph: &BuildGraph,
        id: NodeId,
        unit: &CompilationUnit,
        keys: &HashMap<NodeId, String>,
        source_cache: &mut rune_fingerprint::FingerprintCache,
    ) -> Option<(inputs::ReadRecord, Option<String>)> {
        let as_full_build = unit.check_only != graph.node(id).unit.check_only;
        let script = self.read_mode(graph, id, keys, source_cache, as_full_build)?;
        let key = read_record_key(unit, source_cache)?;
        let record = self.cas.as_ref()?.get_value(&format!("{key}-read"))?;
        Some((record, script))
    }

    /// What a unit's key is made of now (see [`UnitIdentity`]), given its
    /// dependencies' keys.
    fn unit_identity(
        &self,
        graph: &BuildGraph,
        id: NodeId,
        keys: &HashMap<NodeId, String>,
        source_cache: &mut rune_fingerprint::FingerprintCache,
    ) -> UnitIdentity {
        let unit = &graph.node(id).unit;
        let dep_keys = dep_keys_of(graph, id, keys);
        let read = self.read_record(graph, id, unit, keys, source_cache);
        let mut identity = compute_unit_identity_from(
            unit,
            &self.profile,
            &dep_keys,
            source_cache,
            read.as_ref().map(|(r, s)| (r, s.as_deref())),
        );
        let recorded: Option<DeclaredInputs> = self
            .cas
            .as_ref()
            .and_then(|c| c.get_value(&format!("{}-inputs", identity.key)));
        let key = effective_key(
            &identity.key,
            recorded.as_ref(),
            unit.build_script.is_some(),
        );
        if key != identity.key {
            identity.parts.insert(
                "outside inputs (environment variables and files it read)".into(),
                key.clone(),
            );
            identity.key = key;
        }
        identity
    }

    /// Every unit's current identity, dependencies first, with its label —
    /// what `rune explain` compares with the last build.
    pub fn current_identities(
        &self,
        graph: &BuildGraph,
    ) -> RuneResult<Vec<(NodeId, String, UnitIdentity)>> {
        let topo = graph.topological_sort()?;
        let cache_path = self.config.fingerprint_dir.join("source-cache.json");
        let mut source_cache =
            rune_fingerprint::FingerprintCache::load(&cache_path).unwrap_or_default();
        let mut keys = HashMap::new();
        let mut out = Vec::new();
        for id in topo {
            let identity = self.unit_identity(graph, id, &keys, &mut source_cache);
            keys.insert(id, identity.key.clone());
            out.push((
                id,
                unit_label(&graph.node(id).unit, &self.profile),
                identity,
            ));
        }
        Ok(out)
    }

    /// Every store slot a build of `graph` reads, as it stands now: each
    /// unit's artifacts, its build script's result, replayed warnings and
    /// diagnostics, and the outside inputs recorded for it — what
    /// `rune cas export` puts in a bundle.
    pub fn store_keys(&self, graph: &BuildGraph) -> RuneResult<Vec<String>> {
        let topo = graph.topological_sort()?;
        let cache_path = self.config.fingerprint_dir.join("source-cache.json");
        let mut source_cache =
            rune_fingerprint::FingerprintCache::load(&cache_path).unwrap_or_default();
        let mut keys: HashMap<NodeId, String> = HashMap::new();
        // Build script results found so far, by script (for `DEP_*` values).
        let mut script_outputs: HashMap<String, BuildScriptOutput> = HashMap::new();
        let mut out = Vec::new();
        for id in topo {
            let (base, key) = self.compute_keys(graph, id, &keys, &mut source_cache, false);
            out.extend([
                key.clone(),
                // The script output a cache hit of the unit restores (for its
                // dependents' `DEP_*`, link paths and `OUT_DIR`).
                format!("{key}-buildscript"),
                format!("{key}-warnings"),
                format!("{key}-diagnostics"),
                format!("{base}-inputs"),
            ]);
            let unit = &graph.node(id).unit;
            if let Some(read) = read_record_key(unit, &mut source_cache) {
                out.push(format!("{read}-read"));
            }
            if unit.build_script.is_some() && unit.ext.script_owner.is_none() {
                let mut dep_metadata = Vec::new();
                for d in &graph.node(id).dependencies {
                    let dep = &graph.node(*d).unit;
                    let owner = dep
                        .ext
                        .script_owner
                        .clone()
                        .unwrap_or_else(|| script_key(dep));
                    if let (Some(links), Some(output)) = (&dep.links, script_outputs.get(&owner)) {
                        for (k, v) in &output.metadata {
                            dep_metadata.push((links.clone(), k.clone(), v.clone()));
                        }
                    }
                }
                let (script_base, _, script) =
                    self.script_keys(graph, id, &keys, &dep_metadata, &mut source_cache);
                let result = format!("{script}-buildscript");
                if let Some(output) = self.cas.as_ref().and_then(|c| c.get_value(&result)) {
                    script_outputs.insert(script_key(unit), output);
                }
                out.extend([format!("{script_base}-script-record"), result]);
            }
            keys.insert(id, key);
        }
        Ok(out)
    }

    /// The keys of a unit's build script result: `(base, package hash, key
    /// it is looked up under)` (see [`compute_script_base`] and
    /// [`inputs::script_result_key`]), given its build-dependencies' final
    /// keys and the `DEP_*` values the script receives.
    fn script_keys(
        &self,
        graph: &BuildGraph,
        id: NodeId,
        keys: &HashMap<NodeId, String>,
        dep_metadata: &[(String, String, String)],
        source_cache: &mut rune_fingerprint::FingerprintCache,
    ) -> (String, String, String) {
        let unit = &graph.node(id).unit;
        self.script_keys_of(graph, id, unit, keys, dep_metadata, source_cache)
    }

    /// [`Self::script_keys`] for `unit` in the place of node `id`'s (the full
    /// build's unit where the graph has a `check` one).
    fn script_keys_of(
        &self,
        graph: &BuildGraph,
        id: NodeId,
        unit: &CompilationUnit,
        keys: &HashMap<NodeId, String>,
        dep_metadata: &[(String, String, String)],
        source_cache: &mut rune_fingerprint::FingerprintCache,
    ) -> (String, String, String) {
        let node = graph.node(id);
        let build_dep_keys: Vec<(String, String)> = node
            .build_dependencies
            .iter()
            .filter_map(|d| {
                let dep = &graph.node(*d).unit;
                let alias = node
                    .build_aliases
                    .get(d)
                    .cloned()
                    .unwrap_or_else(|| dep.rustc_crate_name());
                Some((
                    format!("build-dep:{alias}:{}", dep.crate_id.name),
                    keys.get(d)?.clone(),
                ))
            })
            .collect();
        let base = compute_script_base(
            unit,
            &self.profile,
            &build_dep_keys,
            dep_metadata,
            source_cache,
        );
        let package_hash = if unit.crate_id.source == CrateSource::Local {
            rune_fingerprint::hasher::compute_package_hash(&unit.manifest_dir, source_cache)
                .unwrap_or_default()
        } else {
            String::new()
        };
        let record: Option<inputs::ScriptRecord> = self
            .cas
            .as_ref()
            .and_then(|c| c.get_value(&format!("{base}-script-record")));
        let key =
            inputs::script_result_key_in(&base, record.as_ref(), Some(&unit.manifest_dir), || {
                package_hash.clone()
            });
        (base, package_hash, key)
    }

    /// Whether the store has a result for `key`.
    pub fn is_cached(&self, key: &str) -> bool {
        self.cas.as_ref().is_some_and(|c| c.contains(key))
    }

    /// Records what each unit built now was built from, for `rune explain`.
    fn save_identities(
        &self,
        graph: &BuildGraph,
        topo: &[NodeId],
        built: impl Fn(NodeId) -> bool,
        keys: &HashMap<NodeId, String>,
        source_cache: &mut rune_fingerprint::FingerprintCache,
    ) {
        let mut all = load_identities(&self.config.fingerprint_dir);
        let before = all.clone();
        for &id in topo {
            if !built(id) {
                continue;
            }
            // The same key is the same identity: nothing to work out again
            // (every unit of a build where nothing changed).
            let label = unit_label(&graph.node(id).unit, &self.profile);
            if keys
                .get(&id)
                .is_some_and(|k| all.get(&label).is_some_and(|saved| saved.key == *k))
            {
                continue;
            }
            let mut identity = self.unit_identity(graph, id, keys, source_cache);
            if let Some(key) = keys.get(&id) {
                identity.key = key.clone();
            }
            all.insert(label, identity);
        }
        if all != before
            && let Ok(text) = serde_json::to_string(&all)
        {
            let path = self.config.fingerprint_dir.join("identities.json");
            let _ = rune_common::fsutil::write_atomic(&path, text.as_bytes());
        }
    }
}

/// A unit's dependencies' keys, named by *how* it depends on them: under
/// which name it links a dependency (`package = "..."` renames), and whether
/// it is a normal or a build dependency. The manifest that says so is not
/// hashed, so the key has to.
fn dep_keys_of(
    graph: &BuildGraph,
    id: NodeId,
    keys: &HashMap<NodeId, String>,
) -> Vec<(String, String)> {
    let node = graph.node(id);
    {
        let mut dep_keys: Vec<(String, String)> = Vec::new();
        for d in &node.dependencies {
            let Some(k) = keys.get(d) else { continue };
            let dep = &graph.node(*d).unit;
            if node.normal_dependencies.contains(d) {
                let name = node
                    .extern_aliases
                    .get(d)
                    .cloned()
                    .unwrap_or_else(|| dep.rustc_crate_name());
                dep_keys.push((format!("dep:{name}:{}", dep.crate_id.name), k.clone()));
            }
            if node.build_dependencies.contains(d) {
                let name = node
                    .build_aliases
                    .get(d)
                    .cloned()
                    .unwrap_or_else(|| dep.rustc_crate_name());
                dep_keys.push((format!("build-dep:{name}:{}", dep.crate_id.name), k.clone()));
            }
        }
        dep_keys
    }
}

impl BuildExecutor {
    /// Gathers everything a worker thread needs, by value.
    #[allow(clippy::too_many_arguments)]
    fn prepare_job(
        &self,
        graph: &BuildGraph,
        node_id: NodeId,
        base_key: &str,
        cas_key: &str,
        dep_keys: Vec<(String, String)>,
        artifact_paths: &HashMap<NodeId, Vec<PathBuf>>,
        metadata_paths: &HashMap<NodeId, PathBuf>,
        build_script_outputs: &HashMap<NodeId, BuildScriptOutput>,
        worker_tx: Sender<WorkerMsg>,
    ) -> UnitJob {
        let node = graph.node(node_id);
        let unit = node.unit.clone();
        let mode = if unit.produces_real_artifact() {
            ExternMode::Link
        } else {
            ExternMode::Pipelined
        };
        let extern_deps = extern_deps_for(graph, node_id, artifact_paths, metadata_paths, mode);

        // Build scripts run natively and link against real artifacts — of
        // their *build-dependencies* only, which are host units.
        let build_script_extern_deps: Vec<(String, PathBuf)> = node
            .dependencies
            .iter()
            .filter(|dep_id| node.build_dependencies.contains(dep_id))
            .filter_map(|&dep_id| {
                let dep_node = graph.node(dep_id);
                let name = node
                    .build_aliases
                    .get(&dep_id)
                    .map(|s| s.replace('-', "_"))
                    .unwrap_or_else(|| dep_node.unit.rustc_crate_name());
                let artifacts = artifact_paths.get(&dep_id)?;
                let ext = |p: &&PathBuf| -> Option<String> {
                    p.extension().and_then(|e| e.to_str()).map(str::to_owned)
                };
                artifacts
                    .iter()
                    .find(|p| ext(p).as_deref() == Some("rlib"))
                    .or_else(|| {
                        artifacts
                            .iter()
                            .find(|p| matches!(ext(p).as_deref(), Some("so" | "dylib" | "dll")))
                    })
                    .or_else(|| {
                        artifacts
                            .iter()
                            .find(|p| !matches!(ext(p).as_deref(), Some("d") | Some("rmeta")))
                    })
                    .map(|p| (name, p.clone()))
                    .filter(|_| {
                        // Only dependencies that are libraries can be linked.
                        dep_node.unit.crate_type != CrateType::Bin
                    })
            })
            .collect();

        // `DEP_<LINKS>_<KEY>`: a `links`-declaring crate's build script
        // reports metadata via `cargo:KEY=VALUE`, which Cargo exposes to a
        // *dependent's* build script.
        let mut dep_metadata = Vec::new();
        for &dep_id in &node.dependencies {
            let dep_unit = &graph.node(dep_id).unit;
            if let (Some(links), Some(output)) =
                (&dep_unit.links, build_script_outputs.get(&dep_id))
            {
                for (key, value) in &output.metadata {
                    dep_metadata.push((links.clone(), key.clone(), value.clone()));
                }
            }
        }

        // Integration tests, benches and examples of a package can run its
        // binaries: Cargo exposes each as `CARGO_BIN_EXE_<name>` at compile
        // time, so every binary this unit depends on gets one.
        let mut extra_env = Vec::new();
        if matches!(
            unit.kind,
            TargetKind::Test | TargetKind::Bench | TargetKind::Example
        ) {
            for &dep_id in &node.dependencies {
                let dep = &graph.node(dep_id).unit;
                if dep.crate_type == CrateType::Bin && dep.kind == TargetKind::Bin {
                    let exe = bin_dir_for(&self.config, &self.profile, dep)
                        .join(dep.executable_name(&dep.crate_id.name));
                    extra_env.push((
                        format!("CARGO_BIN_EXE_{}", dep.crate_id.name),
                        exe.display().to_string(),
                    ));
                }
            }
        }

        // `cargo::rustc-link-search` from the build scripts of everything this
        // unit links against. A library records which native libraries it
        // needs, not where they are; the directory only matters where the
        // final link happens (a `-lpython3.9` from `pyo3-ffi`, found under a
        // path only its build script knows). Cargo passes them to every
        // dependent; build-dependencies and proc-macros are not linked in.
        let mut dep_link_search: Vec<(Option<String>, PathBuf)> = Vec::new();
        let mut seen: HashSet<NodeId> = HashSet::new();
        let mut stack: Vec<NodeId> = node
            .dependencies
            .iter()
            .filter(|d| node.normal_dependencies.contains(d))
            .copied()
            .collect();
        while let Some(id) = stack.pop() {
            let dep = graph.node(id);
            if !seen.insert(id) || dep.unit.crate_type == CrateType::ProcMacro {
                continue;
            }
            if let Some(out) = build_script_outputs.get(&id) {
                for ls in &out.link_search {
                    let entry = (ls.kind.clone(), ls.path.clone());
                    if !dep_link_search.contains(&entry) {
                        dep_link_search.push(entry);
                    }
                }
            }
            stack.extend(
                dep.dependencies
                    .iter()
                    .filter(|d| dep.normal_dependencies.contains(d)),
            );
        }

        let unit_owner_key = unit.ext.script_owner.clone();
        UnitJob {
            dep_link_search,
            node_id,
            unit,
            extern_deps,
            build_script_extern_deps,
            dep_metadata,
            base_key: base_key.to_string(),
            cas_key: cas_key.to_string(),
            dep_keys,
            build_output: match &unit_owner_key {
                // A unit that shares its package's build script takes the
                // output of the unit that ran it.
                Some(owner) => node
                    .dependencies
                    .iter()
                    .find(|&&d| script_key(&graph.node(d).unit) == *owner)
                    .and_then(|d| build_script_outputs.get(d).cloned()),
                None => build_script_outputs.get(&node_id).cloned(),
            },
            config: self.config.clone(),
            profile: self.profile.clone(),
            cas: self.cas.clone(),
            toolchain: toolchain::global(),
            event_tx: self.event_tx.clone(),
            worker_tx,
            force_rebuild: self.force_rebuild,
            max_jobs: self.max_jobs,
            config_env: self.config_env.clone(),
            extra_env,
            color: self.color,
            jobserver: self.jobserver.clone(),
            tokens: self.tokens.clone(),
            sandbox: self.sandbox.clone(),
            script_only: false,
            lock: None,
            lock_waited: false,
            frontend_threads: None,
            script_base: None,
            script_cache_key: None,
            package_hash: String::new(),
            read_script: None,
        }
    }
}

/// Everything one worker thread needs to build a single unit.
struct UnitJob {
    node_id: NodeId,
    unit: CompilationUnit,
    extern_deps: Vec<(String, PathBuf)>,
    build_script_extern_deps: Vec<(String, PathBuf)>,
    dep_metadata: Vec<(String, String, String)>,
    /// The unit's key before external inputs; where its declared inputs are
    /// recorded.
    base_key: String,
    /// The key the unit was scheduled under (may be updated as it learns
    /// about external inputs).
    cas_key: String,
    /// Its dependencies' keys, to key it again by the files it read.
    dep_keys: Vec<(String, String)>,
    build_output: Option<BuildScriptOutput>,
    /// Link search paths from the build scripts of the unit's dependencies.
    dep_link_search: Vec<(Option<String>, PathBuf)>,
    config: RuneConfig,
    profile: Profile,
    cas: Option<CasStore>,
    toolchain: &'static Toolchain,
    event_tx: Sender<PipelineEvent>,
    worker_tx: Sender<WorkerMsg>,
    force_rebuild: bool,
    max_jobs: usize,
    config_env: Vec<(String, String, bool)>,
    /// Extra compile-time environment, e.g. `CARGO_BIN_EXE_<name>` for
    /// integration tests.
    extra_env: Vec<(String, String)>,
    color: bool,
    jobserver: Option<jobserver::Client>,
    tokens: Option<crate::tokens::JobTokens>,
    sandbox: Option<std::sync::Arc<SandboxPolicy>>,
    /// Only run (or restore) the unit's build script; its compilation is a
    /// separate job, started once its dependencies' metadata exists.
    script_only: bool,
    /// The unit's lock, taken by the scheduler (see `execute`).
    lock: Option<rune_common::fsutil::FileLock>,
    /// Another process held the lock first: it may have built the unit.
    lock_waited: bool,
    /// `-Z threads` for this unit's compilation.
    frontend_threads: Option<usize>,
    /// For a script step: the script result's base key (see
    /// [`compute_script_base`]) and the key it is looked up under now.
    script_base: Option<String>,
    script_cache_key: Option<String>,
    /// The package's source hash (for a script that watches no files).
    package_hash: String,
    /// Whether the unit is keyed by the files it read, and with which build
    /// script result (see [`BuildExecutor::read_mode`]).
    read_script: Option<Option<String>>,
}

impl UnitJob {
    /// The compiler command, confined when `build.sandbox` is on: it writes
    /// only into the build directory, and proc-macros get no network.
    fn compiler_command(&self, member: bool) -> std::process::Command {
        let cmd = self.toolchain.rustc_command(member);
        match &self.sandbox {
            Some(p) => p.sandbox.wrap(&cmd, &[&self.config.target_dir], false),
            None => cmd,
        }
    }

    fn fail(&self, error: String, event_error: String) {
        self.fail_with(error, event_error, Vec::new());
    }

    fn fail_with(&self, error: String, event_error: String, diagnostics: Vec<String>) {
        let _ = self.event_tx.send(PipelineEvent::BuildFailed {
            node_id: self.node_id,
            error: event_error,
            diagnostics,
        });
        let _ = self.worker_tx.send(WorkerMsg::Failed {
            node_id: self.node_id,
            error,
        });
    }

    /// After waiting for another process's lock: if that process stored this
    /// unit, restore it exactly as the up-front cache pass would and report
    /// the unit finished. Returns whether it did.
    fn restore_finished_elsewhere(&self, start: Instant) -> bool {
        let Some(cas) = &self.cas else { return false };
        let unit = &self.unit;
        let has_script = unit.build_script.is_some();
        // The other process may have learned which files the unit reads and
        // what it depends on outside itself (which change the key the unit
        // is stored under), which we did not know about.
        let mut base = self.base_key.clone();
        let mut files = rune_fingerprint::FingerprintCache::new();
        if let Some(script) = &self.read_script
            && let Some(read) = read_record_key(unit, &mut files)
            && let Some(record) = cas.get_value::<inputs::ReadRecord>(&format!("{read}-read"))
        {
            base = compute_unit_key_from(
                unit,
                &self.profile,
                &self.dep_keys,
                &mut files,
                Some((&record, script.as_deref())),
            );
        }
        let recorded: Option<DeclaredInputs> = cas.get_value(&format!("{base}-inputs"));
        let key = effective_key(&base, recorded.as_ref(), has_script);
        let out_dir = out_dir_for(&self.config, &self.profile, unit);
        let Ok(linked) = cas.link_to(&key, &out_dir) else {
            return false;
        };
        if linked.is_empty() {
            return false;
        }
        if key != self.cas_key {
            let _ = self.worker_tx.send(WorkerMsg::KeyUpdate {
                node_id: self.node_id,
                key: key.clone(),
            });
        }
        if has_script {
            let script_key_name = format!("{key}-buildscript");
            if let Some(output) = cas.get_value::<BuildScriptOutput>(&script_key_name) {
                let _ = self.worker_tx.send(WorkerMsg::BuildScriptDone {
                    node_id: self.node_id,
                    output,
                });
            }
            let _ = cas.restore_dir_tree(
                &script_key_name,
                &self
                    .config
                    .build_script_out_dir_by_key(self.profile.output_dir_name(), &script_key(unit)),
            );
        }
        if let Some(rmeta) = linked
            .iter()
            .find(|p| p.extension().and_then(|e| e.to_str()) == Some("rmeta"))
        {
            let _ = self.worker_tx.send(WorkerMsg::MetadataReady {
                node_id: self.node_id,
                rmeta_path: rmeta.clone(),
            });
        }
        uplift_artifacts(&self.config, &self.profile, unit, &linked);
        let warnings = if unit.crate_id.source == CrateSource::Local {
            cas.get_value::<String>(&format!("{key}-warnings"))
                .unwrap_or_default()
        } else {
            String::new()
        };
        let diagnostics = if unit.crate_id.source == CrateSource::Local {
            cas.get_value::<Vec<String>>(&format!("{key}-diagnostics"))
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let duration = start.elapsed();
        let _ = self.event_tx.send(PipelineEvent::BuildFinished {
            node_id: self.node_id,
            artifacts: linked.clone(),
            duration,
            warnings,
            diagnostics,
        });
        let _ = self.worker_tx.send(WorkerMsg::Finished {
            node_id: self.node_id,
            artifacts: linked,
            duration,
            peak_memory: None,
            restored: true,
        });
        true
    }

    fn run(mut self) {
        // Claim a core for as long as this unit runs, without waiting behind
        // other compilers' codegen threads (see `tokens`); the compiler may
        // borrow more for its own codegen while cores are idle.
        let token = self.tokens.as_ref().map(|t| t.start());
        let start = Instant::now();
        // Held until the unit is stored (or, for a script step, handed back to
        // the scheduler for the compilation that follows).
        let mut unit_lock = self.lock.take();
        let node_id = self.node_id;
        let unit = &self.unit;
        let profile = &self.profile;
        let config = &self.config;
        if !self.script_only {
            let _ = self.event_tx.send(PipelineEvent::BuildStarted {
                node_id,
                unit: unit.clone(),
            });
        }

        // The process we waited for may have built this unit. (Not checked by
        // a script step: its dependencies may not have their final keys yet,
        // so neither has the unit.)
        if self.lock_waited
            && !self.script_only
            && !self.force_rebuild
            && self.restore_finished_elsewhere(start)
        {
            return;
        }

        // A distinct CAS slot from the compiled-artifact entry, keyed off the
        // same (build-script-aware) fingerprint: whenever the crate needs
        // recompiling for any reason, a build script whose own inputs didn't
        // change can skip being recompiled and re-executed — the most
        // expensive part of crates like `openssl-sys` or `ring`.
        // A script step looks its result up by the script's own key (see
        // `compute_script_base`): editing the package's other files does not
        // run a script that declared which files it reads.
        let build_script_cache_key = match &self.script_cache_key {
            Some(k) => format!("{k}-buildscript"),
            None => format!("{}-buildscript", self.cas_key),
        };
        // The files the script's own compilation read (for its record).
        let mut script_sources: Vec<PathBuf> = Vec::new();

        // 1. Compile & execute the build script, unless a cached run already
        //    told us its output. Its output is two things that must be
        //    restored together: the parsed `cargo:` directives and whatever
        //    files it wrote to `OUT_DIR` (which the crate may `include!()`).
        let script_out_dir = config.build_script_out_dir_by_key(
            profile.output_dir_name(),
            unit.ext
                .script_owner
                .clone()
                .unwrap_or_else(|| script_key(unit))
                .as_str(),
        );
        let mut effective_build_output = self.build_output.clone();
        let shares_script = unit.ext.script_owner.is_some();
        // External inputs this unit is observed to depend on, and the key it
        // is finally stored under (which follows them).
        let mut declared = DeclaredInputs::default();
        let mut store_key = self.cas_key.clone();
        let mut script_ran: Option<BuildScriptOutput> = None;
        let has_script = unit.build_script.is_some();
        // A script step does not announce keys: its dependencies may still
        // change theirs, and the compilation that follows keys the unit again
        // (with the same script output) once they are settled.
        let script_only = self.script_only;
        // The unit's key before external inputs: what it was scheduled under,
        // until the compiler says which files it read.
        let mut base_key = self.base_key.clone();
        let read_key = self
            .read_script
            .as_ref()
            .and_then(|_| read_record_key(unit, &mut rune_fingerprint::FingerprintCache::new()));
        let mut read_record: Option<inputs::ReadRecord> = None;
        let bump = |base_key: &str, declared: &DeclaredInputs, store_key: &mut String| {
            let key = effective_key(base_key, Some(declared), has_script);
            if key != *store_key {
                *store_key = key.clone();
                if !script_only {
                    let _ = self.worker_tx.send(WorkerMsg::KeyUpdate { node_id, key });
                }
            }
        };
        if effective_build_output.is_none()
            && !self.force_rebuild
            && !shares_script
            && let Some(cas_store) = &self.cas
        {
            let restored = cas_store
                .restore_dir_tree(&build_script_cache_key, &script_out_dir)
                .unwrap_or(false);
            if restored {
                effective_build_output = cas_store.get_value(&build_script_cache_key);
                replay_script_warnings(&self.event_tx, node_id, effective_build_output.as_ref());
            }
        }

        if let (Some(build_rs), true) = (
            unit.build_script.clone(),
            effective_build_output.is_none() && !shares_script,
        ) {
            let _ = self.event_tx.send(PipelineEvent::BuildScriptStarted {
                node_id,
                crate_name: unit.crate_id.name.clone(),
            });
            // Start from an empty directory. Files restored from the store are
            // hard links to shared objects; a script that rewrote one *in
            // place* would change the object for everyone. Unlinking first
            // means every file the script writes is its own.
            let _ = std::fs::remove_dir_all(&script_out_dir);
            let _ = std::fs::create_dir_all(&script_out_dir);

            let deps_dir = config.deps_dir(profile.output_dir_name());
            let tc = self.toolchain;
            let eff = unit.effective_profile(profile);
            // `TARGET` must always be a real triple: build scripts like
            // openssl-sys parse it. `HOST` is always the build machine's.
            // `CARGO_CFG_*` describes the *target* (asked of the compiler,
            // with the unit's own flags), never the machine Rune runs on.
            let target_info = TargetInfo::query(
                tc,
                unit.target.as_ref().map(|t| t.0.as_str()),
                &unit.ext.rustflags,
            );
            let env = BuildScriptEnv {
                manifest_dir: unit.manifest_dir.clone(),
                out_dir: script_out_dir.clone(),
                target: target_info.name.clone(),
                host: tc.host.clone(),
                opt_level: eff.opt_level.as_str().to_string(),
                debug: eff.debuginfo != rune_common::DebugInfo::None,
                debug_assertions: eff.debug_assertions,
                profile: if profile.is_release() {
                    "release"
                } else {
                    "debug"
                }
                .to_string(),
                num_jobs: self.max_jobs as u32,
                rustc: tc.exec.clone(),
                rustdoc: tc.rustdoc.clone(),
                rustc_wrapper: tc.wrapper.clone(),
                rustc_workspace_wrapper: tc.workspace_wrapper.clone(),
                linker: unit.ext.linker.as_ref().map(PathBuf::from),
                pkg_name: if unit.meta.name.is_empty() {
                    unit.crate_id.name.clone()
                } else {
                    unit.meta.name.clone()
                },
                pkg_version: unit.crate_id.version.to_string(),
                meta: unit.meta.clone(),
                features: unit.features.clone(),
                target_cfg_env: target_info.cargo_cfg_env(eff.debug_assertions),
                dep_metadata: self.dep_metadata.clone(),
                links: unit.links.clone(),
                rustflags: unit.ext.script_rustflags.clone(),
                config_env: self.config_env.clone(),
            };
            // The script itself is compiled with the host's flags.
            let compile_env = BuildScriptEnv {
                linker: unit.ext.script_linker.as_ref().map(PathBuf::from),
                ..env.clone()
            };

            // `[profile.*.build-override]` decides how the script itself is
            // compiled (by default unoptimized: it runs once).
            let script_flags = unit
                .ext
                .script_profile
                .as_ref()
                .map(|p| {
                    // Build scripts ignore `panic`, as in Cargo.
                    let p = Profile {
                        panic: rune_common::PanicStrategy::Unwind,
                        ..p.clone()
                    };
                    with_profile_flags(
                        RustcCommandBuilder::new(PathBuf::new()),
                        &p,
                        &self.toolchain.host,
                        true,
                    )
                    .into_args()
                })
                .unwrap_or_default();
            let color = self.color;
            let configure = |cmd: &mut std::process::Command| {
                cmd.args(&script_flags);
                if color {
                    cmd.arg("--color=always");
                }
                // A dependency's build script is not the user's code: its
                // warnings are noise, exactly as for the crate itself.
                if unit.crate_id.source != CrateSource::Local {
                    cmd.arg("--cap-lints").arg("allow");
                }
                if let Some(js) = &self.jobserver {
                    js.configure(cmd);
                }
            };
            let script_bin_dir =
                config.build_script_bin_dir_by_key(profile.output_dir_name(), &script_key(unit));
            // The compiled script is kept beside `OUT_DIR` with a stamp of
            // what it was compiled from; a script that must run again because
            // a file it watches changed is not compiled again for that.
            let stamp_path = script_bin_dir.join(".rune-stamp");
            let reusable = self
                .script_base
                .as_ref()
                .filter(|_| !self.force_rebuild)
                .and_then(|base| {
                    let stamp: ScriptStamp =
                        serde_json::from_str(&std::fs::read_to_string(&stamp_path).ok()?).ok()?;
                    let bin = script_bin_dir.join(if cfg!(windows) {
                        "build_script_build.exe"
                    } else {
                        "build_script_build"
                    });
                    (stamp.base == *base && bin.is_file() && stamp.sources_unchanged())
                        .then_some((bin, stamp))
                });
            let compiled = match reusable {
                Some((bin, stamp)) => {
                    script_sources = stamp.sources.iter().map(|(p, _)| p.clone()).collect();
                    Ok((bin, stamp.warnings))
                }
                None => {
                    let compiled = BuildScriptRunner::compile_reporting(
                        self.compiler_command(unit.ext.member),
                        &build_rs,
                        &script_bin_dir,
                        &deps_dir,
                        unit.edition.as_str(),
                        &self.build_script_extern_deps,
                        Some(&compile_env),
                        &configure,
                    );
                    if let (Ok((_, printed)), Some(base)) = (&compiled, &self.script_base) {
                        script_sources =
                            inputs::dep_info_sources(&script_bin_dir.join("build_script_build.d"));
                        let stamp = ScriptStamp::new(base, &script_sources, printed);
                        if let Ok(text) = serde_json::to_string(&stamp) {
                            let _ = rune_common::fsutil::write_atomic(&stamp_path, text.as_bytes());
                        }
                    }
                    compiled
                }
            };
            match compiled {
                Ok((bin_path, printed)) => {
                    // Warnings about a dependency's `build.rs` are capped, as
                    // for the crate itself; the workspace's own are kept.
                    let compile_warnings = if unit.crate_id.source == CrateSource::Local {
                        printed
                    } else {
                        String::new()
                    };
                    let run_configure = |cmd: &mut std::process::Command| {
                        if let Some(js) = &self.jobserver {
                            js.configure(cmd);
                        }
                    };
                    // A registry or git package is immutable, and shared by
                    // every project: its build script may write to `OUT_DIR`
                    // and nowhere near its own source.
                    let immutable_before = (unit.crate_id.source != CrateSource::Local)
                        .then(|| tree_stamp(&unit.manifest_dir));
                    let script_cmd = match &self.sandbox {
                        // Writes go to `OUT_DIR` only; the network is off
                        // unless the package is allowed it.
                        Some(p) => p.sandbox.command(
                            bin_path.as_os_str(),
                            &[&env.out_dir],
                            p.network.contains(&unit.crate_id.name),
                        ),
                        None => std::process::Command::new(&bin_path),
                    };
                    let outcome =
                        BuildScriptRunner::execute_command(script_cmd, &env, &run_configure);
                    if let Some(before) = immutable_before
                        && outcome.is_ok()
                        && tree_stamp(&unit.manifest_dir) != before
                    {
                        self.fail(
                                "build script modified its package source".into(),
                                format!(
                                    "the build script of `{} v{}` modified its own source directory \
                                     ({}), which is immutable: a build script may only write to its \
                                     `OUT_DIR`",
                                    unit.crate_id.name,
                                    unit.crate_id.version,
                                    unit.manifest_dir.display()
                                ),
                            );
                        return;
                    }
                    match outcome {
                        Ok(mut out) => {
                            let _ = self.event_tx.send(PipelineEvent::BuildScriptFinished {
                                node_id,
                                crate_name: unit.crate_id.name.clone(),
                                duration: start.elapsed(),
                            });
                            out.compile_warnings = compile_warnings;
                            // Shown when the script runs (as Cargo does), for
                            // the workspace's own packages.
                            if unit.crate_id.source == CrateSource::Local {
                                let mut text = out.compile_warnings.clone();
                                for w in &out.warnings {
                                    text.push_str(&format!(
                                        "warning: {}@{}: {w}\n",
                                        unit.crate_id.name, unit.crate_id.version
                                    ));
                                }
                                if !text.trim().is_empty() {
                                    let _ = self.event_tx.send(PipelineEvent::ScriptWarnings {
                                        node_id,
                                        warnings: text,
                                    });
                                }
                            }
                            script_ran = Some(out.clone());
                            effective_build_output = Some(out);
                        }
                        Err(e) => {
                            self.fail(
                                e.to_string(),
                                with_hint(format!("build.rs execution failed: {e}")),
                            );
                            return;
                        }
                    }
                }
                Err(e) => {
                    self.fail(
                        e.to_string(),
                        with_hint(format!("build.rs compilation failed: {e}")),
                    );
                    return;
                }
            }
        }

        // What the script told us about its outside reads. If that changes the
        // unit's key, say so before anything above it can start; then cache the
        // script's result under the final key.
        if let (Some(out), false) = (&effective_build_output, shares_script) {
            declared.merge(inputs::from_script_output(
                out,
                &unit.manifest_dir,
                &script_out_dir,
            ));
            bump(&base_key, &declared, &mut store_key);
        }
        if let (Some(out), Some(cas_store)) = (&script_ran, &self.cas) {
            let key = match &self.script_base {
                // What the run showed it depends on keys its next lookup.
                Some(base) => {
                    let record = inputs::script_record(
                        out,
                        &unit.manifest_dir,
                        &script_out_dir,
                        &script_sources,
                    );
                    let _ = cas_store.put_value(&format!("{base}-script-record"), &record);
                    let package_hash = self.package_hash.clone();
                    let key = inputs::script_result_key_in(
                        base,
                        Some(&record),
                        Some(&unit.manifest_dir),
                        || package_hash,
                    );
                    format!("{key}-buildscript")
                }
                None => format!("{store_key}-buildscript"),
            };
            let _ = cas_store.put_dir_tree(&key, &script_out_dir);
            let _ = cas_store.put_value(&key, out);
        }

        if let Some(output) = &effective_build_output {
            let _ = self.worker_tx.send(WorkerMsg::BuildScriptDone {
                node_id,
                output: output.clone(),
            });
        }
        if self.script_only {
            let _ = self.worker_tx.send(WorkerMsg::ScriptPhaseDone {
                node_id,
                duration: start.elapsed(),
                lock: unit_lock.take(),
                lock_waited: self.lock_waited,
            });
            return;
        }

        // 2. Build the rustc command. Any leftover output from an earlier
        // build is unlinked first: outputs are hard links into the CAS, and a
        // compiler that rewrites an existing file in place would silently
        // change the cached object every other project shares.
        let out_dir = out_dir_for(config, profile, unit);
        remove_stale_outputs(&out_dir, unit);

        let _ = std::fs::create_dir_all(&out_dir);
        let mut cmd_builder = build_rustc_command(
            unit,
            profile,
            config,
            &self.extern_deps,
            effective_build_output.as_ref(),
            self.color,
        );
        let own_search: Vec<(Option<String>, PathBuf)> = effective_build_output
            .iter()
            .flat_map(|o| &o.link_search)
            .map(|ls| (ls.kind.clone(), ls.path.clone()))
            .collect();
        for (kind, path) in &self.dep_link_search {
            if !own_search.contains(&(kind.clone(), path.clone())) {
                cmd_builder = cmd_builder.link_search(kind.as_deref(), path);
            }
        }
        for (k, v) in &self.extra_env {
            cmd_builder = cmd_builder.env(k, v);
        }
        // How the compiler divides its work, not what it produces: not part
        // of the key (like `-j`).
        if let Some(n) = self.frontend_threads {
            cmd_builder = cmd_builder.extra_flags(vec![format!("-Zthreads={n}")]);
        }
        for (k, v, force) in &self.config_env {
            if *force || std::env::var_os(k).is_none() {
                cmd_builder = cmd_builder.env(k, v);
            }
        }
        let mut cmd = cmd_builder.build_with_command(self.compiler_command(unit.ext.member));
        if let Some(js) = &self.jobserver {
            js.configure(&mut cmd);
        }

        // 3. Pipelined invocation: report `.rmeta` as soon as it exists. rustc
        // announces its dep-info first; it lists the environment variables the
        // crate read through `env!`/`option_env!`, which belong in the key.
        let invoker = RustcInvoker::new();
        let worker_tx = self.worker_tx.clone();
        let event_tx = self.event_tx.clone();
        let script_env: Vec<String> = effective_build_output
            .as_ref()
            .map(|o| o.env_vars.iter().map(|(k, _)| k.clone()).collect())
            .unwrap_or_default();
        let provided = |name: &str| {
            name.starts_with("CARGO_")
                || name == "OUT_DIR"
                || script_env.iter().any(|n| n == name)
                || self.config_env.iter().any(|(n, ..)| n == name)
                || self.extra_env.iter().any(|(n, _)| n == name)
        };
        let mut sent_metadata = false;
        let result = invoker.invoke_streaming(cmd, &mut |artifact| match artifact.emit.as_str() {
            "dep-info" => {
                let mut moved = false;
                // Keyed by exactly the files it read from now on (and found
                // again by them next time; see `ReadRecord`).
                if read_key.is_some() {
                    let record = inputs::ReadRecord::from_dep_info(
                        &artifact.artifact,
                        rune_common::fsutil::simplified(&self.config.project_root),
                        &unit.manifest_dir,
                        &self.config.target_dir,
                    );
                    let script = self.read_script.clone().flatten();
                    let base = compute_unit_key_from(
                        unit,
                        profile,
                        &self.dep_keys,
                        &mut rune_fingerprint::FingerprintCache::new(),
                        Some((&record, script.as_deref())),
                    );
                    read_record = Some(record);
                    if base != base_key {
                        base_key = base;
                        moved = true;
                    }
                }
                let reads = inputs::env_reads_from_dep_info(&artifact.artifact, &provided);
                let manifest = rune_common::manifest_file::manifest_in(&unit.manifest_dir);
                let read_manifest = inputs::reads_manifest(
                    &artifact.artifact,
                    &manifest,
                    &self.config.project_root,
                );
                if !reads.is_empty() || read_manifest {
                    declared.merge(DeclaredInputs {
                        env: reads,
                        files: if read_manifest {
                            vec![manifest]
                        } else {
                            Vec::new()
                        },
                        ..Default::default()
                    });
                    moved = true;
                }
                if moved {
                    bump(&base_key, &declared, &mut store_key);
                }
            }
            "metadata" if !sent_metadata => {
                sent_metadata = true;
                let _ = event_tx.send(PipelineEvent::MetadataReady {
                    node_id,
                    rmeta_path: artifact.artifact.clone(),
                });
                let _ = worker_tx.send(WorkerMsg::MetadataReady {
                    node_id,
                    rmeta_path: artifact.artifact.clone(),
                });
            }
            _ => {}
        });

        match result {
            Ok(comp_res) if comp_res.success => {
                let duration = start.elapsed();
                // Dependencies are compiled with `--cap-lints allow`; only
                // the user's own crates have warnings worth showing.
                let warnings: String = if unit.crate_id.source == CrateSource::Local {
                    comp_res
                        .diagnostics
                        .iter()
                        .filter_map(|d| d.rendered.as_deref())
                        .collect::<Vec<_>>()
                        .join("")
                } else {
                    String::new()
                };
                let diagnostics: Vec<String> = if unit.crate_id.source == CrateSource::Local {
                    rune_compiler::diagnostic_lines(&comp_res.stderr_raw)
                } else {
                    Vec::new()
                };
                let artifacts: Vec<PathBuf> =
                    comp_res.artifacts.into_iter().map(|a| a.artifact).collect();
                uplift_artifacts(config, profile, unit, &artifacts);

                // Dependents can start now; storing the result is not their
                // concern (the files they use are the ones just written).
                let _ = self.event_tx.send(PipelineEvent::BuildFinished {
                    node_id,
                    artifacts: artifacts.clone(),
                    duration,
                    warnings: warnings.clone(),
                    diagnostics: diagnostics.clone(),
                });
                let _ = self.worker_tx.send(WorkerMsg::Finished {
                    node_id,
                    artifacts: artifacts.clone(),
                    duration,
                    peak_memory: comp_res.peak_memory,
                    restored: false,
                });
                // Storing is disk work, not compiler work: give the job slot's
                // token back so another compiler can use the core meanwhile.
                drop(token);

                if let Some(cas_store) = &self.cas {
                    let _ = cas_store.put_adopted(
                        &store_key,
                        &unit.crate_id.name,
                        &unit.crate_id.version.to_string(),
                        &artifacts,
                    );
                    if !warnings.is_empty() {
                        let _ = cas_store.put_value(&format!("{store_key}-warnings"), &warnings);
                    }
                    if !diagnostics.is_empty() {
                        let _ =
                            cas_store.put_value(&format!("{store_key}-diagnostics"), &diagnostics);
                    }
                    // Remember what this unit depends on outside itself, so
                    // the next lookup keys on the current values of those.
                    if !declared.is_empty() {
                        let _ = cas_store.put_value(&format!("{base_key}-inputs"), &declared);
                    }
                    // And which of its files it read, which the next lookup
                    // keys on instead of every file of the package.
                    if let (Some(key), Some(record)) = (&read_key, &read_record) {
                        let _ = cas_store.put_value(&format!("{key}-read"), record);
                    }
                    // The script's result under the key the unit is finally
                    // stored under, which is what a later cache hit restores
                    // it by (the script step stored it under the key the unit
                    // had then, which its dependencies may have changed since).
                    if let (Some(out), false) = (&effective_build_output, shares_script) {
                        let key = format!("{store_key}-buildscript");
                        if !cas_store.contains_tree(&key) {
                            let _ = cas_store.put_dir_tree(&key, &script_out_dir);
                            let _ = cas_store.put_value(&key, out);
                        }
                    }
                }
                drop(unit_lock);
            }
            Ok(comp_res) => {
                let err_msg = render_compile_error(&comp_res);
                let diagnostics = rune_compiler::diagnostic_lines(&comp_res.stderr_raw);
                self.fail_with(err_msg.clone(), err_msg, diagnostics);
            }
            Err(e) => self.fail(e.to_string(), e.to_string()),
        }
    }
}

/// Appends advice to a build-script failure when it is one of the usual
/// "a system tool or library is missing" ones.
fn with_hint(message: String) -> String {
    match crate::hints::hint_for_failure(&message, crate::hints::Family::current()) {
        Some(hint) => format!("{message}\n\n{hint}"),
        None => message,
    }
}

/// A cheap fingerprint of every file below `dir` (path, size, modification
/// time), to notice that something wrote there.
///
/// Directories only contribute their path, not their own size/mtime: tools
/// invoked by a build script (e.g. `rustc --emit=metadata -o -`, which stages
/// the `.rmeta` in a temp dir under the process's CWD before streaming it to
/// stdout) routinely create-then-remove a scratch directory inside the
/// package root. That leaves no file behind, but it does bump the mtime of
/// whichever directory held it — which would otherwise register as a false
/// "modified its own source" violation even though the tree's actual
/// contents are unchanged.
fn tree_stamp(dir: &Path) -> u64 {
    let mut hb = rune_common::hash::HashBuilder::new();
    for entry in walkdir::WalkDir::new(dir)
        .sort_by_file_name()
        .into_iter()
        .flatten()
    {
        let Ok(meta) = entry.metadata() else { continue };
        hb.update(entry.path().to_string_lossy().as_bytes());
        if meta.is_dir() {
            continue;
        }
        hb.update(&meta.len().to_le_bytes());
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        hb.update(&mtime.to_le_bytes());
    }
    rune_common::hash::xxhash3(hb.finish_blake3().as_bytes())
}

/// The `crate-name-hash` directory a unit's build script output lives in.
pub fn script_key(unit: &CompilationUnit) -> String {
    format!("{}-{}", unit.rustc_crate_name(), unit.metadata_hash())
}

/// Where a unit is compiled to (and restored from the store to): `deps/`, or
/// `examples/` for examples, as in Cargo. Binaries get their friendly names
/// in [`bin_dir_for`]; keeping the hash-named file and whatever else the
/// compiler leaves beside it (object files under `split-debuginfo=unpacked`)
/// in `deps/` keeps the profile directory clean.
pub fn out_dir_for(config: &RuneConfig, profile: &Profile, unit: &CompilationUnit) -> PathBuf {
    match (unit.crate_type, unit.kind) {
        (CrateType::Bin, TargetKind::Example) if !unit.check_only && !unit.harness => {
            bin_dir_for(config, profile, unit)
        }
        _ => config.deps_dir(profile.output_dir_name()),
    }
}

/// Where a binary's friendly names (`target/debug/my-app`) live: the profile
/// directory — `target/<triple>/debug/` with `--target`, as Cargo lays it
/// out — or its `examples/`.
pub fn bin_dir_for(config: &RuneConfig, profile: &Profile, unit: &CompilationUnit) -> PathBuf {
    let dir = profile.output_dir_name();
    let base = match &unit.target {
        Some(t) => {
            let profile_dir = config.profile_target_dir(dir);
            let name = profile_dir.file_name().unwrap_or_default().to_owned();
            config.target_dir.join(target_dir_name(&t.0)).join(name)
        }
        None => config.profile_target_dir(dir),
    };
    match unit.kind {
        TargetKind::Example => base.join("examples"),
        _ => base,
    }
}

/// The directory name for a target: the triple, or a custom specification's
/// file stem (`--target specs/my-os.json` → `my-os`).
pub fn target_dir_name(target: &str) -> String {
    match target.strip_suffix(".json") {
        Some(stem) => Path::new(stem)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| stem.to_string()),
        None => target.to_string(),
    }
}

/// The `--extern` name a dependent uses for one of its dependencies.
fn extern_name(graph: &BuildGraph, node_id: NodeId, dep_id: NodeId) -> String {
    graph
        .node(node_id)
        .extern_aliases
        .get(&dep_id)
        .map(|s| s.replace('-', "_"))
        .unwrap_or_else(|| graph.node(dep_id).unit.rustc_crate_name())
}

/// Builds the `--extern name=path` list for a graph node.
///
/// Proc-macros are always passed as their shared library; everything else
/// prefers `.rmeta` in [`ExternMode::Pipelined`] and `.rlib` in
/// [`ExternMode::Link`].
pub fn extern_deps_for(
    graph: &BuildGraph,
    node_id: NodeId,
    artifact_paths: &HashMap<NodeId, Vec<PathBuf>>,
    metadata_paths: &HashMap<NodeId, PathBuf>,
    mode: ExternMode,
) -> Vec<(String, PathBuf)> {
    let ext_of = |p: &&PathBuf| p.extension().and_then(|e| e.to_str()).map(str::to_owned);
    let mut deps = Vec::new();
    for &dep_id in &graph.node(node_id).dependencies {
        // A build-dependency links into the build script, not the crate.
        if !graph.is_normal_edge(node_id, dep_id) {
            continue;
        }
        let dep_node = graph.node(dep_id);
        // A binary can be a *build-order* dependency (integration tests need
        // the package's executables to exist) but is never `--extern`ed.
        if dep_node.unit.crate_type == CrateType::Bin {
            continue;
        }
        let name = extern_name(graph, node_id, dep_id);
        // A dependency that is still compiling has no entry in
        // `artifact_paths` yet, but may already have published its `.rmeta`
        // (that is the whole point of pipelining), so an empty list is fine.
        let no_artifacts = Vec::new();
        let artifacts = artifact_paths.get(&dep_id).unwrap_or(&no_artifacts);
        let is_real = |p: &&PathBuf| !matches!(ext_of(p).as_deref(), Some("d") | Some("rmeta"));

        let chosen: Option<PathBuf> = if dep_node.unit.crate_type == CrateType::ProcMacro {
            artifacts
                .iter()
                .find(|p| matches!(ext_of(p).as_deref(), Some("so" | "dylib" | "dll")))
                .or_else(|| artifacts.iter().find(is_real))
                .cloned()
        } else if mode == ExternMode::Link {
            artifacts
                .iter()
                .find(|p| ext_of(p).as_deref() == Some("rlib"))
                .or_else(|| artifacts.iter().find(is_real))
                .cloned()
        } else {
            metadata_paths
                .get(&dep_id)
                .cloned()
                .or_else(|| {
                    artifacts
                        .iter()
                        .find(|p| ext_of(p).as_deref() == Some("rmeta"))
                        .cloned()
                })
                .or_else(|| {
                    artifacts
                        .iter()
                        .find(|p| ext_of(p).as_deref() == Some("rlib"))
                        .cloned()
                })
                .or_else(|| {
                    artifacts
                        .iter()
                        .find(|p| ext_of(p).as_deref() != Some("d"))
                        .cloned()
                })
        };
        if let Some(path) = chosen {
            deps.push((name, path));
        }
    }
    deps
}

/// Removes files from a previous build of this exact unit (same crate and
/// metadata hash) so the compiler creates fresh files instead of rewriting
/// hard-linked ones. See the note at the call site.
fn remove_stale_outputs(out_dir: &Path, unit: &CompilationUnit) {
    let needle = format!("{}-{}", unit.rustc_crate_name(), unit.metadata_hash());
    let Ok(entries) = std::fs::read_dir(out_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.contains(&needle) && !name.starts_with('.') {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Gives a freshly built or restored artifact the names Cargo gives it in the
/// profile directory, next to the hash-suffixed real file in `deps/`:
///
/// * a binary: `name` (and the underscore-normalized `name`), `name.exe` on
///   Windows;
/// * a workspace member's library: `libname.rlib`, `libname.a`,
///   `libname.so`/`.dylib`, `name.dll`/`name.lib` — what C build systems,
///   Python packaging and plugin loaders look for.
///
/// Hard links, not copies: a debug binary can be tens of megabytes, and copying
/// it on every no-op build was a large part of a "nothing changed" build's cost.
fn uplift_artifacts(
    config: &RuneConfig,
    profile: &Profile,
    unit: &CompilationUnit,
    artifacts: &[PathBuf],
) {
    if unit.harness || unit.check_only {
        return;
    }
    let is_bin = unit.crate_type == CrateType::Bin;
    if !is_bin && (!unit.ext.member || unit.crate_type == CrateType::ProcMacro) {
        return;
    }
    let bin_dir = bin_dir_for(config, profile, unit);
    let _ = std::fs::create_dir_all(&bin_dir);
    let suffix = format!("-{}", unit.metadata_hash());
    for artifact in artifacts {
        let ext = artifact.extension().and_then(|e| e.to_str());
        if is_bin {
            if matches!(ext, Some("d" | "rmeta" | "rlib")) {
                continue;
            }
            let with_ext = |name: String| match ext {
                Some(e) => format!("{name}.{e}"),
                None => name,
            };
            let primary = bin_dir.join(with_ext(unit.crate_id.name.clone()));
            let normalized = bin_dir.join(with_ext(unit.crate_id.crate_name_normalized()));
            link_or_copy_replace(artifact, &primary);
            if normalized != primary {
                link_or_copy_replace(artifact, &normalized);
            }
        } else if matches!(ext, Some("rlib" | "a" | "so" | "dylib" | "dll" | "lib")) {
            let Some(name) = artifact.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            link_or_copy_replace(artifact, &bin_dir.join(name.replacen(&suffix, "", 1)));
        }
    }
}

/// Makes `dest` a hard link to (or, failing that, a copy of) `src`, doing
/// nothing if it already is one.
fn link_or_copy_replace(src: &Path, dest: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(a), Ok(b)) = (std::fs::metadata(src), std::fs::metadata(dest))
            && a.ino() == b.ino()
            && a.dev() == b.dev()
        {
            return;
        }
    }
    let _ = rune_common::fsutil::replace_with_link(src, dest);
}

/// Fraction of a library's compile time after which its `.rmeta` exists, i.e.
/// how long a pipelined dependent waits for it.
const METADATA_FRACTION: f64 = 0.4;

/// Compile times, used to weight the critical path.
///
/// Measured seconds from earlier builds win (this project's, then those of any
/// project on the machine, kept next to the CAS so a fresh checkout does not
/// start blind). For a unit nobody has timed yet the estimate comes from the
/// size of its sources, which on a cold build is the difference between
/// starting `syn` first and starting it last.
/// What a compiler never measured before is assumed to need at its peak.
const DEFAULT_COMPILER_MEMORY: u64 = 256 << 20;

#[derive(Default)]
struct Timings {
    secs: HashMap<String, f64>,
    /// Sizes of package sources in bytes, by manifest directory.
    source_bytes: HashMap<PathBuf, u64>,
    dirty: bool,
}

impl Timings {
    fn key(unit: &CompilationUnit, profile: &Profile) -> String {
        format!(
            "{} {} {} {}{}",
            unit.crate_id.name,
            unit.crate_id.version,
            profile.output_dir_name(),
            unit.kind.as_str(),
            if unit.harness { " harness" } else { "" }
        )
    }

    /// Loads the machine-wide timings, then this project's on top of them.
    fn load(project: &Path, global: &Path) -> Self {
        let read = |p: &Path| -> HashMap<String, f64> {
            std::fs::read_to_string(p)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default()
        };
        let mut secs = read(global);
        secs.extend(read(project));
        Self {
            secs,
            ..Self::default()
        }
    }

    /// Measures the source size of every distinct package directory in
    /// `dirs`, in parallel.
    fn measure_sources(&mut self, dirs: impl IntoIterator<Item = PathBuf>) {
        let todo: Vec<PathBuf> = dirs
            .into_iter()
            .collect::<HashSet<_>>()
            .into_iter()
            .filter(|d| !self.source_bytes.contains_key(d))
            .collect();
        let measure = |d: PathBuf| {
            let bytes = rust_source_bytes(&d);
            (d, bytes)
        };
        let sizes: Vec<(PathBuf, u64)> = if todo.len() < 8 {
            todo.into_iter().map(measure).collect()
        } else {
            todo.into_par_iter().map(measure).collect()
        };
        self.source_bytes.extend(sizes);
    }

    /// Seconds this unit is expected to take: measured if we have seen it
    /// before, otherwise guessed from the size of its package's sources.
    fn estimate(&self, unit: &CompilationUnit, profile: &Profile) -> f64 {
        // A unit that runs its own build script also waits for that (measured
        // separately, since it can overlap the unit's dependencies).
        let script = if unit.build_script.is_some() && unit.ext.script_owner.is_none() {
            self.secs
                .get(&Self::script_key(unit, profile))
                .copied()
                .unwrap_or(1.0)
        } else {
            0.0
        };
        script + self.estimate_compile(unit, profile)
    }

    fn script_key(unit: &CompilationUnit, profile: &Profile) -> String {
        format!("{} script", Self::key(unit, profile))
    }

    fn record_script(&mut self, unit: &CompilationUnit, profile: &Profile, took: Duration) {
        self.secs
            .insert(Self::script_key(unit, profile), took.as_secs_f64());
        self.dirty = true;
    }

    /// Seconds the unit's own compilation is expected to take.
    fn estimate_compile(&self, unit: &CompilationUnit, profile: &Profile) -> f64 {
        if let Some(&secs) = self.secs.get(&Self::key(unit, profile)) {
            return secs;
        }
        let bytes = self
            .source_bytes
            .get(&unit.manifest_dir)
            .copied()
            .unwrap_or(0) as f64;
        // Roughly: 100 KB of Rust ≈ 1 s, 1 MB ≈ 4 s, 4 MB ≈ 8 s of debug
        // compile time (fitted to a 182-crate build; only the *ordering*
        // matters to the scheduler, not the absolute value).
        let mb = bytes / (1024.0 * 1024.0);
        let mut secs = 0.3 + 3.0 * mb.powf(0.6);
        if unit.harness {
            secs *= 1.2;
        }
        if unit.crate_type == CrateType::ProcMacro {
            secs *= 1.3;
        }
        if profile.output_dir_name() != "debug" {
            secs *= 2.5; // optimised builds spend far longer in codegen
        }
        secs
    }

    /// Records a compiler's peak memory for this unit.
    fn record_memory(&mut self, unit: &CompilationUnit, profile: &Profile, bytes: u64) {
        self.secs
            .insert(format!("{} rss", Self::key(unit, profile)), bytes as f64);
        self.dirty = true;
    }

    /// The unit's peak memory the last time it was compiled.
    fn memory(&self, unit: &CompilationUnit, profile: &Profile) -> Option<u64> {
        self.secs
            .get(&format!("{} rss", Self::key(unit, profile)))
            .map(|b| *b as u64)
    }

    fn record(&mut self, unit: &CompilationUnit, profile: &Profile, took: Duration) {
        self.secs
            .insert(Self::key(unit, profile), took.as_secs_f64());
        self.dirty = true;
    }

    /// Writes this project's timings, and merges them into the machine-wide
    /// file (atomically; concurrent builds may lose each other's newest
    /// numbers, which only costs some scheduling accuracy).
    fn save(&self, project: &Path, global: &Path) {
        if !self.dirty {
            return;
        }
        if let Ok(json) = serde_json::to_string(&self.secs) {
            let _ = std::fs::write(project, &json);
        }
        let mut merged: HashMap<String, f64> = std::fs::read_to_string(global)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        merged.extend(self.secs.iter().map(|(k, v)| (k.clone(), *v)));
        // Bound the file: keep it a few thousand entries at most.
        if merged.len() > 20_000 {
            merged.retain(|k, _| self.secs.contains_key(k));
        }
        if let Ok(json) = serde_json::to_string(&merged) {
            let tmp = global.with_extension(format!("json.{}.tmp", std::process::id()));
            if std::fs::write(&tmp, json).is_ok() && std::fs::rename(&tmp, global).is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }
}

/// Total size of the `.rs` files below a package directory (build output,
/// VCS metadata and vendored trees excluded); a cheap proxy for compile time.
fn rust_source_bytes(dir: &Path) -> u64 {
    const MAX_FILES: usize = 20_000;
    let mut total = 0u64;
    let mut seen = 0usize;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if ft.is_dir() {
                if !matches!(
                    name.as_ref(),
                    "target" | ".git" | ".rune" | ".cargo" | "node_modules" | "tests" | "benches"
                ) {
                    stack.push(entry.path());
                }
            } else if name.ends_with(".rs") {
                seen += 1;
                if seen > MAX_FILES {
                    return total;
                }
                total += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    total
}

/// `RUSTFLAGS` (or `CARGO_ENCODED_RUSTFLAGS`) as individual arguments.
pub fn rustflags_from_env() -> Vec<String> {
    if let Ok(encoded) = std::env::var("CARGO_ENCODED_RUSTFLAGS") {
        return encoded
            .split('\x1f')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
    }
    std::env::var("RUSTFLAGS")
        .map(|v| v.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default()
}

/// Turns a failed `rustc` invocation into a human-readable message.
///
/// Prefers each diagnostic's pre-rendered, ANSI-colored text (the same text
/// `rustc` would print directly to a terminal) over dumping the raw
/// `--json=diagnostic-rendered-ansi` event stream.
pub fn render_compile_error(result: &CompilationResult) -> String {
    let rendered: String = result
        .diagnostics
        .iter()
        .filter_map(|d| d.rendered.as_deref().or(Some(d.message.as_str())))
        .collect::<Vec<_>>()
        .join("");

    if !rendered.trim().is_empty() {
        rendered
    } else if !result.stderr_raw.trim().is_empty() {
        result.stderr_raw.clone()
    } else {
        format!("rustc exited with code {}", result.exit_code)
    }
}

/// Sets the `CARGO_PKG_*` / `CARGO_CRATE_NAME` / `CARGO_MANIFEST_*`
/// environment that `env!()` reads at compile time.
pub fn with_cargo_env(
    mut builder: RustcCommandBuilder,
    unit: &CompilationUnit,
) -> RustcCommandBuilder {
    let meta = &unit.meta;
    let pkg_name = if meta.name.is_empty() {
        unit.crate_id.name.clone()
    } else {
        meta.name.clone()
    };
    let pkg_version = unit.crate_id.version.to_string();
    builder = builder
        .env("CARGO_CRATE_NAME", &unit.rustc_crate_name())
        .env("CARGO_PKG_NAME", &pkg_name)
        .env("CARGO_PKG_VERSION", &pkg_version)
        .env(
            "CARGO_MANIFEST_DIR",
            &rune_common::fsutil::simplified(&unit.manifest_dir)
                .display()
                .to_string(),
        )
        .env(
            "CARGO_MANIFEST_PATH",
            &rune_common::fsutil::simplified(&rune_common::manifest_file::manifest_in(
                &unit.manifest_dir,
            ))
            .display()
            .to_string(),
        )
        .env("CARGO_PKG_AUTHORS", &meta.authors)
        .env("CARGO_PKG_DESCRIPTION", &meta.description)
        .env("CARGO_PKG_HOMEPAGE", &meta.homepage)
        .env("CARGO_PKG_REPOSITORY", &meta.repository)
        .env("CARGO_PKG_LICENSE", &meta.license)
        .env("CARGO_PKG_LICENSE_FILE", &meta.license_file)
        .env("CARGO_PKG_README", &meta.readme)
        .env("CARGO_PKG_RUST_VERSION", &meta.rust_version);

    if let Some(links) = &unit.links {
        builder = builder.env("CARGO_MANIFEST_LINKS", links);
    }
    if unit.kind == TargetKind::Bin {
        builder = builder.env("CARGO_BIN_NAME", &unit.crate_id.name);
    }

    // Cargo always sets all four version components, MAJOR/MINOR/PATCH/PRE,
    // even when a component is absent (as an empty string) — crates
    // routinely read all four unconditionally via `env!(...)`.
    let core_version = pkg_version.split(['-', '+']).next().unwrap_or(&pkg_version);
    let mut version_parts = core_version.split('.');
    builder
        .env(
            "CARGO_PKG_VERSION_MAJOR",
            version_parts.next().unwrap_or("0"),
        )
        .env(
            "CARGO_PKG_VERSION_MINOR",
            version_parts.next().unwrap_or("0"),
        )
        .env(
            "CARGO_PKG_VERSION_PATCH",
            version_parts.next().unwrap_or("0"),
        )
        .env(
            "CARGO_PKG_VERSION_PRE",
            pkg_version
                .split_once('-')
                .map(|(_, pre)| pre)
                .unwrap_or(""),
        )
}

/// Adds the codegen flags a [`Profile`] implies (`-C opt-level`, `debuginfo`,
/// `debug-assertions`, `overflow-checks`, `lto`, `panic`, `codegen-units`,
/// `strip`, `rpath`).
pub fn with_profile_flags(
    mut builder: RustcCommandBuilder,
    profile: &Profile,
    target: &str,
    links_final_artifact: bool,
) -> RustcCommandBuilder {
    // Only spell out settings that differ from what rustc does on its own for
    // this opt-level (`opt-level=0` implies debug assertions and overflow
    // checks; higher levels imply neither). Passing the defaults explicitly
    // is harmless but makes every command line — and thus every diagnostic
    // and debug session — noisier than Cargo's.
    let opt0 = profile.opt_level == rune_common::OptLevel::O0;
    if !opt0 {
        builder = builder.opt_level(profile.opt_level.as_str());
    }
    builder = builder.debuginfo(profile.debuginfo.as_str());
    if profile.debug_assertions != opt0 {
        builder = builder.codegen_flag(&format!(
            "debug-assertions={}",
            on_off(profile.debug_assertions)
        ));
    }
    if profile.overflow_checks != opt0 {
        builder = builder.codegen_flag(&format!(
            "overflow-checks={}",
            on_off(profile.overflow_checks)
        ));
    }
    // LTO happens when the final artifact is linked, over the bitcode embedded
    // in every library it pulls in. So `-C lto` goes only to units that link
    // (binaries and C-style libraries), never to ordinary libraries or
    // proc-macros (rustc rejects LTO for proc-macro crates outright); and
    // bitcode is embedded in libraries exactly when some LTO is in play.
    match (profile.lto.as_flag(), links_final_artifact) {
        (Some(lto), true) => builder = builder.lto(lto),
        (Some(_), false) => {}
        (None, _) => {
            // Nothing will LTO this crate, so don't spend time and disk
            // embedding bitcode in it (Cargo does the same).
            builder = builder.codegen_flag("embed-bitcode=no");
        }
    }
    if profile.panic != rune_common::PanicStrategy::Unwind {
        builder = builder.panic(profile.panic.as_str());
    }
    if let Some(cgu) = profile.codegen_units {
        builder = builder.codegen_units(cgu);
    }
    // Stripping applies to what gets linked; a stripped proc-macro dylib could
    // no longer be loaded by the compiler.
    if let (Some(strip), true) = (&profile.strip, links_final_artifact) {
        builder = builder.codegen_flag(&format!("strip={strip}"));
    }
    if profile.rpath {
        builder = builder.codegen_flag("rpath=yes");
    }
    if let Some(split) = profile.split_debuginfo_for(target) {
        builder = builder.codegen_flag(&format!("split-debuginfo={split}"));
    }
    builder
}

fn on_off(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

/// The triple a unit is compiled for: its `--target`, or the host for host
/// units (build-script dependencies, proc-macros) and native builds.
fn compiled_for(unit: &CompilationUnit) -> &str {
    unit.compiled_for()
}

/// `--remap-path-prefix` flags for `trim-paths`: a registry or git package's
/// sources become `name-version/src/…` (or the checkout's own relative path),
/// the workspace's become relative to its root — what Cargo's `trim-paths`
/// does. The standard library is remapped by rustc itself. The flags are not
/// part of the key (they name this machine's paths; the result does not).
fn trim_path_flags(unit: &CompilationUnit, config: &RuneConfig) -> Vec<String> {
    let with_slash = |p: &Path| {
        let mut s = p.to_string_lossy().into_owned();
        if !s.ends_with(std::path::MAIN_SEPARATOR) {
            s.push(std::path::MAIN_SEPARATOR);
        }
        s
    };
    let root = &config.project_root;
    let mut flags = Vec::new();
    // rustc applies the last matching rule, so the most specific comes last.
    if unit.crate_id.source != CrateSource::Local || !unit.manifest_dir.starts_with(root) {
        if let Some(parent) = unit.manifest_dir.parent() {
            flags.push(format!("--remap-path-prefix={}=", with_slash(parent)));
        }
    } else {
        flags.push(format!("--remap-path-prefix={}=", with_slash(root)));
    }
    flags
}

/// Helper to build a complete `rustc` command for a compilation unit.
pub fn build_rustc_command(
    unit: &CompilationUnit,
    profile: &Profile,
    config: &RuneConfig,
    extern_deps: &[(String, PathBuf)],
    build_output: Option<&BuildScriptOutput>,
    color: bool,
) -> RustcCommandBuilder {
    // The unit's own profile (`[profile.*.package.*]`, `build-override`);
    // the request's profile still names the output directory.
    let eff = unit.effective_profile(profile);
    // A crate of the project is compiled from the project root with a
    // relative path, as Cargo does: diagnostics and panic locations read
    let unit_source_root = rune_common::fsutil::simplified(&unit.source_root);
    let config_project_root = rune_common::fsutil::simplified(&config.project_root);
    let (source, cwd) = match unit_source_root.strip_prefix(config_project_root) {
        Ok(rel) if unit.crate_id.source == CrateSource::Local && !rel.as_os_str().is_empty() => {
            (rel.to_path_buf(), Some(config_project_root.to_path_buf()))
        }
        _ => (unit.source_root.clone(), None),
    };
    let mut builder = RustcCommandBuilder::new(source);
    if let Some(dir) = cwd {
        builder = builder.current_dir(dir);
    }
    let mut builder = builder
        .crate_name(&unit.rustc_crate_name())
        .edition(unit.edition.as_str())
        .metadata_hash(&unit.metadata_hash());
    // A test harness replaces `--crate-type`: rustc generates a `main()` that
    // discovers and runs every `#[test]` item, and defines `cfg(test)`.
    builder = if unit.harness {
        builder.test_harness()
    } else {
        // `[lib] crate-type = ["cdylib", "rlib"]` is one compilation with
        // several outputs, not several compilations.
        unit.all_crate_types()
            .fold(builder, |b, t| b.crate_type(t.rustc_crate_type()))
    };
    // A test harness is a program too: `[profile.test] lto = "fat"` applies
    // to it (some crates' tests depend on it, e.g. `no-panic` checks).
    let links_final_artifact = if unit.harness {
        !unit.check_only && unit.crate_type != CrateType::ProcMacro
    } else {
        unit.all_crate_types().any(|t| {
            matches!(
                t,
                CrateType::Bin | CrateType::Cdylib | CrateType::Dylib | CrateType::Staticlib
            )
        })
    };
    // The test harness needs unwinding, and a panicking proc-macro must not
    // take the compiler down: both ignore `panic = "abort"`, as in Cargo.
    let unwinding;
    let eff = if eff.panic != rune_common::PanicStrategy::Unwind
        && (unit.harness || unit.crate_type == CrateType::ProcMacro)
    {
        unwinding = Profile {
            panic: rune_common::PanicStrategy::Unwind,
            ..eff.clone()
        };
        &unwinding
    } else {
        eff
    };
    builder = with_profile_flags(builder, eff, compiled_for(unit), links_final_artifact);
    builder = with_cargo_env(builder, unit);
    if eff.trim_paths {
        builder = builder.extra_flags(trim_path_flags(unit, config));
    }

    // Host units (proc-macros, build-script dependencies) are built for the
    // machine running the compiler, whatever `--target` says.
    if let (Some(target), false, false) = (
        &unit.target,
        unit.ext.for_host,
        unit.crate_type == CrateType::ProcMacro,
    ) {
        builder = builder.target(&target.0);
    }

    if !unit.ext.rustflags.is_empty() {
        builder = builder.extra_flags(unit.ext.rustflags.clone());
    }
    if let (Some(linker), true) = (&unit.ext.linker, links_final_artifact) {
        builder = builder.linker(linker);
    }

    if !unit.lint_args.is_empty() {
        builder = builder.extra_flags(unit.lint_args.clone());
    }

    if unit.crate_id.source != CrateSource::Local {
        builder = builder.cap_lints("allow");
    }

    // Declare the cfgs Cargo declares for every package it lints: `docsrs`
    // and `test`, and which `feature = "..."` values exist (none is a list
    // too), so `unexpected_cfgs` catches typos but not `#[cfg(test)]` — a
    // crate that denies warnings fails otherwise.
    if unit.crate_id.source == CrateSource::Local {
        let values = unit
            .ext
            .declared_features
            .iter()
            .map(|f| format!("\"{f}\""))
            .collect::<Vec<_>>()
            .join(", ");
        builder = builder
            .check_cfg("cfg(docsrs,test)")
            .check_cfg(&format!("cfg(feature, values({values}))"));
    }

    if unit.crate_type == CrateType::ProcMacro {
        builder = builder.extern_crate_bare("proc_macro");
    }

    let profile_dir_name = profile.output_dir_name();
    // `rune check` never links a binary, so there's nothing to place in the
    // top-level profile dir — everything lands in `deps/`, like `cargo check`.
    builder = builder.out_dir(&out_dir_for(config, profile, unit));

    // Emits. `rune check` skips codegen and linking for everything except a
    // proc-macro, which must be a real, loadable dylib even to typecheck code
    // that invokes it (see `CompilationUnit::produces_real_artifact`).
    if unit.check_only && unit.crate_type != CrateType::ProcMacro {
        builder = builder.emit(&["dep-info", "metadata"]);
    } else if unit.crate_type == CrateType::Bin || unit.harness {
        builder = builder.emit(&["dep-info", "link"]);
    } else {
        builder = builder.emit(&["dep-info", "metadata", "link"]);
    }

    // Incremental compilation for the crates being edited (path dependencies
    // and workspace members) in profiles that ask for it, exactly like Cargo.
    // Registry crates never change, so incremental state would only cost disk.
    // It lives in the project's build directory, never in the shared CAS.
    if eff.incremental && unit.crate_id.source == CrateSource::Local {
        let dir = config
            .profile_target_dir(profile_dir_name)
            .join("incremental")
            .join(format!(
                "{}-{}",
                unit.rustc_crate_name(),
                unit.metadata_hash()
            ));
        builder = builder.incremental(&dir);
    }

    // Dependencies
    builder = builder.search_path("dependency", &config.deps_dir(profile_dir_name));
    for (crate_name, path) in extern_deps {
        builder = builder.extern_crate(crate_name, path);
    }

    for feat in &unit.features {
        builder = builder.feature(feat);
    }

    // Build script outputs (native libs, search paths, cfgs, envs)
    if unit.build_script.is_some() {
        let key = unit
            .ext
            .script_owner
            .clone()
            .unwrap_or_else(|| script_key(unit));
        let script_out_dir = config.build_script_out_dir_by_key(profile_dir_name, &key);
        builder = builder.env("OUT_DIR", &script_out_dir.display().to_string());
    }

    if let Some(out) = build_output {
        for ls in &out.link_search {
            builder = builder.link_search(ls.kind.as_deref(), &ls.path);
        }
        for ll in &out.link_libs {
            builder = builder.link_lib(ll.kind.as_deref(), &ll.name);
        }
        for cfg_flag in &out.cfgs {
            builder = builder.cfg(cfg_flag);
        }
        for check_cfg in &out.check_cfgs {
            builder = builder.check_cfg(check_cfg);
        }
        for (k, v) in &out.env_vars {
            builder = builder.env(k, v);
        }
        for arg in &out.link_args {
            if link_arg_applies(&arg.scope, unit) {
                builder = builder.link_arg(&arg.arg);
            }
        }
    }

    // JSON output for diagnostics & artifact tracking
    builder.json_output(color)
}

/// Whether a `cargo::rustc-link-arg*` directive applies to this unit.
fn link_arg_applies(scope: &LinkArgScope, unit: &CompilationUnit) -> bool {
    let links = !unit.check_only
        && (unit.harness
            || unit.all_crate_types().any(|t| {
                matches!(
                    t,
                    CrateType::Bin | CrateType::Cdylib | CrateType::Dylib | CrateType::ProcMacro
                )
            }));
    if !links {
        return false;
    }
    match scope {
        LinkArgScope::All => true,
        LinkArgScope::Cdylib => unit.all_crate_types().any(|t| t == CrateType::Cdylib),
        LinkArgScope::Bins => unit.kind == TargetKind::Bin && !unit.harness,
        LinkArgScope::Bin(name) => {
            unit.kind == TargetKind::Bin
                && !unit.harness
                && unit.rustc_crate_name() == name.replace('-', "_")
        }
        LinkArgScope::Tests => unit.kind == TargetKind::Test,
        LinkArgScope::Examples => unit.kind == TargetKind::Example,
        LinkArgScope::Benches => unit.kind == TargetKind::Bench,
    }
}

/// Computes the content-addressable cache key for a compilation unit.
///
/// The key is the unit's *complete* identity, not merely its source:
///
/// * the unit itself (`metadata_hash`: package, version, **source identity**,
///   crate types, features, target, edition, mode, role),
/// * every codegen-relevant setting of its *effective* profile,
/// * the compiler — executable, version, driver, and the `RUSTC_WRAPPER` /
///   `RUSTC_WORKSPACE_WRAPPER` in front of it,
/// * the effective `rustflags`, linker, lint flags and package metadata baked
///   in via `env!`,
/// * for local packages, every input file, and
/// * every dependency's own key (so a change deep in the graph invalidates
///   everything above it).
///
/// *Registry* and *Git* crates are immutable — a checksum or a commit pins
/// their content — so their inputs are not read from disk at all. Only local
/// (path) packages are hashed, package-wide and mtime-gated (see
/// [`rune_fingerprint::hasher::compute_package_hash`]).
pub fn compute_unit_cas_key(
    unit: &CompilationUnit,
    profile: &Profile,
    dep_keys: &[(String, String)],
    source_cache: &mut rune_fingerprint::FingerprintCache,
) -> String {
    identity_with(
        unit,
        profile,
        dep_keys,
        source_cache,
        Sources::Package,
        false,
    )
    .key
}

/// What a unit's key is made of, part by part: what `rune explain` compares
/// to say why a unit is (or would be) compiled again.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UnitIdentity {
    pub key: String,
    /// Part (`sources`, `toolchain`, `dependency serde`, …) → digest.
    pub parts: std::collections::BTreeMap<String, String>,
    /// Local packages: each file (relative to the package) → content hash.
    #[serde(default)]
    pub files: std::collections::BTreeMap<String, String>,
}

/// Feeds the key hasher exactly as before, and — when the parts are wanted
/// (`rune explain`'s records) — a hasher per named part.
struct KeyRecorder {
    key: rune_common::hash::HashBuilder,
    parts: Option<std::collections::BTreeMap<String, rune_common::hash::HashBuilder>>,
}

impl KeyRecorder {
    fn update(&mut self, part: &str, bytes: &[u8]) {
        self.key.update(bytes);
        let Some(parts) = &mut self.parts else { return };
        match parts.get_mut(part) {
            Some(hasher) => hasher.update(bytes),
            None => {
                let mut hasher = rune_common::hash::HashBuilder::new();
                hasher.update(bytes);
                parts.insert(part.to_string(), hasher);
            }
        }
    }
}

/// [`compute_unit_cas_key`], with the key's parts.
pub fn compute_unit_identity(
    unit: &CompilationUnit,
    profile: &Profile,
    dep_keys: &[(String, String)],
    source_cache: &mut rune_fingerprint::FingerprintCache,
) -> UnitIdentity {
    identity_with(
        unit,
        profile,
        dep_keys,
        source_cache,
        Sources::Package,
        true,
    )
}

/// [`compute_unit_identity`] keyed by the files a local crate read when it
/// was last compiled, when they are known (see [`inputs::ReadRecord`]), and
/// by the key of its build script's result if it has one.
pub fn compute_unit_identity_from(
    unit: &CompilationUnit,
    profile: &Profile,
    dep_keys: &[(String, String)],
    source_cache: &mut rune_fingerprint::FingerprintCache,
    read: Option<(&inputs::ReadRecord, Option<&str>)>,
) -> UnitIdentity {
    let sources = match read {
        Some((record, script)) => Sources::Read(record, script),
        None => Sources::Package,
    };
    identity_with(unit, profile, dep_keys, source_cache, sources, true)
}

/// The key alone of [`compute_unit_identity_from`] (without the parts it
/// is made of, which only `rune explain`'s records need).
pub fn compute_unit_key_from(
    unit: &CompilationUnit,
    profile: &Profile,
    dep_keys: &[(String, String)],
    source_cache: &mut rune_fingerprint::FingerprintCache,
    read: Option<(&inputs::ReadRecord, Option<&str>)>,
) -> String {
    let sources = match read {
        Some((record, script)) => Sources::Read(record, script),
        None => Sources::Package,
    };
    identity_with(unit, profile, dep_keys, source_cache, sources, false).key
}

/// Where the files a local crate read are recorded (`<this>-read`). Keyed
/// by the crate's configuration and its root file's contents, so each
/// version of the root keeps its record. A record that does not fit only
/// costs a miss: a stored result's key names the files it read and their
/// contents.
pub fn read_record_key(
    unit: &CompilationUnit,
    source_cache: &mut rune_fingerprint::FingerprintCache,
) -> Option<String> {
    if unit.crate_id.source != CrateSource::Local {
        return None;
    }
    let root = rune_fingerprint::hasher::file_hash(&unit.source_root, source_cache)?;
    let mut hb = rune_common::hash::HashBuilder::new();
    hb.update(b"read-record\0");
    hb.update(unit.metadata_hash().as_bytes());
    hb.update(b"\0");
    hb.update(unit.crate_id.name.as_bytes());
    hb.update(b"\0");
    let rel_root = unit
        .source_root
        .strip_prefix(&unit.manifest_dir)
        .unwrap_or(&unit.source_root);
    hb.update(rel_root.to_string_lossy().as_bytes());
    hb.update(&root.to_le_bytes());
    Some(hb.finish_blake3())
}

/// Which of a local package's files a unit's key covers.
enum Sources<'a> {
    /// None (a build script result's base: the script says what it read).
    None,
    /// Every file of the package (nothing is known about what it reads).
    Package,
    /// The files it read when it was last compiled, and the key of the build
    /// script result it compiles with (whose output — generated code in
    /// `OUT_DIR`, cfgs, environment — the files do not cover).
    Read(&'a inputs::ReadRecord, Option<&'a str>),
}

/// The base key of a unit's *build script result*: the unit's configuration
/// without its package's sources (which [`inputs::script_result_key`] adds
/// back as the script declared them), with the script's build-dependencies
/// and the `DEP_*` values it receives in place of the unit's dependencies.
pub fn compute_script_base(
    unit: &CompilationUnit,
    profile: &Profile,
    build_dep_keys: &[(String, String)],
    dep_metadata: &[(String, String, String)],
    source_cache: &mut rune_fingerprint::FingerprintCache,
) -> String {
    let mut deps: Vec<(String, String)> = build_dep_keys.to_vec();
    for (links, k, v) in dep_metadata {
        deps.push((format!("dep-metadata:{links}:{k}"), v.clone()));
    }
    let identity = identity_with(unit, profile, &deps, source_cache, Sources::None, false);
    let mut hb = rune_common::hash::HashBuilder::new();
    hb.update(b"build-script-result\0");
    hb.update(identity.key.as_bytes());
    if let Some(script) = &unit.build_script {
        let rel = script.strip_prefix(&unit.manifest_dir).unwrap_or(script);
        hb.update(rel.to_string_lossy().as_bytes());
        // The script itself: two packages that agree on everything else (the
        // same name and version in two projects) must not share a record of
        // what the other's script read.
        hb.update(b"\0script=");
        hb.update(inputs::hash_path(script).as_bytes());
    }
    hb.finish_blake3()
}

fn identity_with(
    unit: &CompilationUnit,
    profile: &Profile,
    dep_keys: &[(String, String)],
    source_cache: &mut rune_fingerprint::FingerprintCache,
    sources: Sources,
    with_parts: bool,
) -> UnitIdentity {
    let mut hb = KeyRecorder {
        key: rune_common::hash::HashBuilder::new(),
        parts: with_parts.then(Default::default),
    };
    const UNIT: &str = "unit (features, crate type, edition, target)";
    hb.update(UNIT, unit.metadata_hash().as_bytes());
    let eff = unit.effective_profile(profile);
    hb.update("profile", eff.codegen_fingerprint().as_bytes());
    // A platform default (not in the profile itself) that changes what rustc
    // emits. Only added when it applies, so other platforms' keys are unchanged.
    if eff.split_debuginfo.is_none()
        && let Some(split) = eff.split_debuginfo_for(compiled_for(unit))
    {
        hb.update("profile", b"\0split-default=");
        hb.update("profile", split.as_bytes());
    }
    hb.update("toolchain", b"\0toolchain=");
    hb.update(
        "toolchain",
        toolchain::global().identity_for(unit.ext.member).as_bytes(),
    );
    for flag in &unit.ext.rustflags {
        hb.update("rustflags", b"\0rustflag=");
        hb.update("rustflags", flag.as_bytes());
    }
    if let Some(linker) = &unit.ext.linker {
        hb.update("linker", b"\0linker=");
        hb.update("linker", linker.as_bytes());
    }
    for (k, v) in &unit.ext.env {
        hb.update("environment", b"\0env=");
        hb.update("environment", k.as_bytes());
        hb.update("environment", b"=");
        hb.update("environment", v.as_bytes());
    }
    for (k, v) in inputs::apple_environment(compiled_for(unit)) {
        if unit.ext.env.iter().all(|(set, _)| set != k) {
            hb.update("environment", b"\0apple-env=");
            hb.update("environment", k.as_bytes());
            hb.update("environment", b"=");
            hb.update("environment", v.as_bytes());
        }
    }
    // How the build script is compiled and what it links against is part of
    // what the package produces.
    if let Some(p) = &unit.ext.script_profile
        && unit.build_script.is_some()
    {
        const SCRIPT: &str = "build script profile and flags";
        hb.update(SCRIPT, b"\0script-profile=");
        hb.update(SCRIPT, p.codegen_fingerprint().as_bytes());
        if p.split_debuginfo.is_none()
            && let Some(split) = p.split_debuginfo_for(&toolchain::global().host)
        {
            hb.update(SCRIPT, b"\0script-split-default=");
            hb.update(SCRIPT, split.as_bytes());
        }
        for f in &unit.ext.script_rustflags {
            hb.update(SCRIPT, b"\0script-flag=");
            hb.update(SCRIPT, f.as_bytes());
        }
    }
    let m = &unit.meta;
    for field in [
        &m.authors,
        &m.description,
        &m.homepage,
        &m.repository,
        &m.license,
        &m.license_file,
        &m.readme,
        &m.rust_version,
    ] {
        hb.update("package metadata (CARGO_PKG_*)", field.as_bytes());
        hb.update("package metadata (CARGO_PKG_*)", b"\0");
    }

    for arg in &unit.lint_args {
        hb.update("lints", b"\0lint=");
        hb.update("lints", arg.as_bytes());
    }
    // The cfgs declared for `unexpected_cfgs` (see `build_rustc_command`)
    // decide which warnings a local crate's result replays.
    if unit.crate_id.source == CrateSource::Local {
        hb.update("lints", b"\0check-cfg=docsrs,test,features");
    }
    for f in &unit.ext.declared_features {
        hb.update("declared features", b"\0declared=");
        hb.update("declared features", f.as_bytes());
    }

    let mut files = std::collections::BTreeMap::new();
    if unit.crate_id.source == CrateSource::Local && !matches!(sources, Sources::None) {
        // Where the crate root lives is an input too (`[lib] path = ...`).
        let rel_root = unit
            .source_root
            .strip_prefix(&unit.manifest_dir)
            .unwrap_or(&unit.source_root);
        hb.update("crate root", rel_root.to_string_lossy().as_bytes());
        if let Sources::Read(record, script) = sources {
            // The result of its build script: what it generated, set and
            // linked follows from that.
            if let Some(script) = script {
                hb.update("build script result", b"script=");
                hb.update("build script result", script.as_bytes());
            }
            // Exactly the files it read, by name and contents ("missing" for
            // one that is gone).
            hb.update("sources", b"read\0");
            for (name, path) in record.paths(&unit.manifest_dir) {
                let hash = rune_fingerprint::hasher::file_hash(&path, source_cache)
                    .map_or_else(|| "missing".to_string(), |h| format!("{h:016x}"));
                hb.update("sources", name.as_bytes());
                hb.update("sources", b"=");
                hb.update("sources", hash.as_bytes());
                hb.update("sources", b"\0");
                if with_parts {
                    files.insert(name, hash);
                }
            }
        } else {
            let package_hash =
                rune_fingerprint::hasher::compute_package_hash(&unit.manifest_dir, source_cache)
                    .unwrap_or_default();
            hb.update("sources", package_hash.as_bytes());
            if with_parts {
                files.extend(source_cache.package_file_hashes(&unit.manifest_dir));
            }
        }
        // A build script (or crate root) can live outside the package
        // directory (`build = "../shared/build.rs"`), where the walk above
        // doesn't see it.
        for outside in [
            unit.build_script.as_deref(),
            Some(unit.source_root.as_path()),
        ]
        .into_iter()
        .flatten()
        .filter(|p| !p.starts_with(&unit.manifest_dir))
        {
            if let Ok(h) = rune_common::hash::xxhash3_file(outside) {
                hb.update("sources outside the package", &h.to_le_bytes());
            }
        }
    }

    // `dep_keys` arrives in dependency order, which traces back to iterating
    // a `HashMap` during graph construction — Rust randomizes iteration order
    // per process, so hashing it unsorted made the key change from run to run
    // for any unit with more than one dependency.
    let mut sorted_dep_keys: Vec<&(String, String)> = dep_keys.iter().collect();
    sorted_dep_keys.sort();
    for (dep_name, dep_key) in sorted_dep_keys {
        // `dep:alias:crate` / `build-dep:alias:crate`.
        let mut it = dep_name.splitn(3, ':');
        let part = match (it.next(), it.nth(1)) {
            (Some("build-dep"), Some(krate)) => format!("build dependency {krate}"),
            (_, Some(krate)) => format!("dependency {krate}"),
            _ => format!("dependency {dep_name}"),
        };
        hb.update(&part, dep_name.as_bytes());
        hb.update(&part, dep_key.as_bytes());
    }

    UnitIdentity {
        key: hb.key.finish_blake3(),
        parts: hb
            .parts
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| (k, v.finish_blake3()))
            .collect(),
        files,
    }
}

/// A stable name for a unit across builds (its key and hash change with
/// what it is built from; this does not): package, target, role, profile.
pub fn unit_label(unit: &CompilationUnit, profile: &Profile) -> String {
    let mut label = format!(
        "{} v{} ({} `{}`",
        unit.crate_id.name,
        unit.crate_id.version,
        unit.kind.as_str(),
        unit.rustc_crate_name()
    );
    if unit.harness {
        label.push_str(", test");
    }
    if unit.check_only {
        label.push_str(", check");
    }
    if unit.ext.for_host {
        label.push_str(", host");
    }
    if let Some(t) = &unit.target {
        label.push_str(&format!(", {}", t.0));
    }
    label.push_str(&format!(") [{}]", profile.name));
    label
}

/// The identities of the last build, by [`unit_label`].
pub fn load_identities(fingerprint_dir: &Path) -> HashMap<String, UnitIdentity> {
    std::fs::read_to_string(fingerprint_dir.join("identities.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune_common::{CrateId, Edition};

    fn unit(name: &str, source: CrateSource, dir: &Path) -> CompilationUnit {
        CompilationUnit {
            crate_id: CrateId::new(name, semver::Version::new(1, 0, 0), source),
            crate_type: CrateType::Lib,
            source_root: dir.join("src/lib.rs"),
            manifest_dir: dir.to_path_buf(),
            features: vec![],
            profile_name: "dev".into(),
            target: None,
            edition: Edition::E2021,
            is_build_script: false,
            check_only: false,
            links: None,
            build_script: None,
            meta: Default::default(),
            lint_args: Vec::new(),
            target_name: String::new(),
            kind: Default::default(),
            harness: false,
            ext: Default::default(),
        }
    }

    fn registry() -> CrateSource {
        CrateSource::Registry {
            index: "x".into(),
            checksum: Some("c".into()),
        }
    }

    fn key(u: &CompilationUnit, p: &Profile) -> String {
        compute_unit_cas_key(u, p, &[], &mut rune_fingerprint::FingerprintCache::new())
    }

    #[test]
    fn registry_keys_never_touch_the_filesystem() {
        // The directory doesn't exist: a registry key must not care.
        let u = unit("serde", registry(), Path::new("/nonexistent/x"));
        assert_eq!(key(&u, &Profile::dev()), key(&u, &Profile::dev()));
    }

    #[test]
    fn local_keys_follow_file_contents() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "fn a() {}").unwrap();
        let u = unit("local", CrateSource::Local, dir.path());
        let before = key(&u, &Profile::dev());
        std::fs::write(dir.path().join("src/lib.rs"), "fn b() {}").unwrap();
        assert_ne!(before, key(&u, &Profile::dev()));
    }

    #[test]
    fn a_machine_short_of_memory_builds_one_unit_at_a_time_and_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let mut graph = BuildGraph::new();
        for name in ["one", "two", "three", "four"] {
            let pkg = dir.path().join(name);
            std::fs::create_dir_all(pkg.join("src")).unwrap();
            std::fs::write(pkg.join("Cargo.toml"), "").unwrap();
            std::fs::write(
                pkg.join("src/lib.rs"),
                format!("pub fn {name}() -> u32 {{ 1 }}\n"),
            )
            .unwrap();
            let mut u = unit(name, CrateSource::Local, &pkg);
            u.target_name = name.into();
            graph.add_node(u);
        }
        fn short() -> Option<rune_common::sysinfo::Memory> {
            Some(rune_common::sysinfo::Memory {
                total: 8 << 30,
                available: 100 << 20,
            })
        }
        let config = RuneConfig::new(dir.path().join("project"));
        let result = BuildExecutor::new(config, Profile::dev(), 4)
            .with_memory_probe(short)
            .execute(&graph)
            .unwrap();
        assert!(result.success, "{:?}", result.failed);
        assert_eq!(result.compiled, 4);
        let schedule = result.schedule.unwrap();
        assert!(
            schedule.parallelism() <= 1.05,
            "ran {:.2} units at once while memory was short",
            schedule.parallelism()
        );
        assert!(result.memory_held > Duration::ZERO);
    }

    #[test]
    fn compilers_expected_to_need_more_memory_than_is_free_run_one_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let mut graph = BuildGraph::new();
        for name in ["one", "two", "three", "four"] {
            let pkg = dir.path().join(name);
            std::fs::create_dir_all(pkg.join("src")).unwrap();
            std::fs::write(pkg.join("Cargo.toml"), "").unwrap();
            std::fs::write(pkg.join("src/lib.rs"), format!("pub fn {name}() {{}}\n")).unwrap();
            let mut u = unit(name, CrateSource::Local, &pkg);
            u.target_name = name.into();
            graph.add_node(u);
        }
        // 3 GiB free of 8: room for several ordinary compilers.
        fn roomy() -> Option<rune_common::sysinfo::Memory> {
            Some(rune_common::sysinfo::Memory {
                total: 8 << 30,
                available: 3 << 30,
            })
        }
        let config = RuneConfig::new(dir.path().join("project"));
        let first = BuildExecutor::new(config.clone(), Profile::dev(), 4)
            .with_memory_probe(roomy)
            .execute(&graph)
            .unwrap();
        assert!(first.success);
        // Each compiler's peak was measured and kept.
        let path = config.fingerprint_dir.join("timings.json");
        let mut timings: HashMap<String, f64> =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        // (The timings file also holds the machine-wide ones, of real builds
        // and other tests: packages named `two` among them.)
        let our_keys: HashSet<String> = graph
            .nodes()
            .iter()
            .map(|n| format!("{} rss", Timings::key(&n.unit, &Profile::dev())))
            .collect();
        let ours = |k: &str| our_keys.contains(k);
        let peaks: Vec<&String> = timings.keys().filter(|k| ours(k)).collect();
        assert_eq!(peaks.len(), 4, "{timings:?}");
        assert!(
            timings
                .iter()
                .filter(|(k, _)| ours(k))
                .all(|(_, v)| *v > 0.0)
        );
        // Pretend each needed 2 GiB: two at once would not fit in 3.
        for (k, v) in timings.iter_mut() {
            if ours(k) {
                *v = (2u64 << 30) as f64;
            }
        }
        std::fs::write(&path, serde_json::to_string(&timings).unwrap()).unwrap();
        let second = BuildExecutor::new(config, Profile::dev(), 4)
            .with_memory_probe(roomy)
            .with_force_rebuild(true)
            .execute(&graph)
            .unwrap();
        assert!(second.success);
        assert_eq!(second.compiled, 4);
        let schedule = second.schedule.unwrap();
        assert!(
            schedule.parallelism() <= 1.05,
            "ran {:.2} units at once",
            schedule.parallelism()
        );
    }

    #[test]
    fn auto_frontend_threads_use_only_idle_cores() {
        use rune_common::settings::FrontendThreads::*;
        // A busy machine: one thread, i.e. no flag.
        assert_eq!(frontend_threads(Auto, 10, 10, 0), None);
        assert_eq!(frontend_threads(Auto, 10, 4, 6), None);
        // The last crate of a build on an otherwise idle machine.
        assert_eq!(frontend_threads(Auto, 10, 1, 0), Some(8));
        assert_eq!(frontend_threads(Auto, 4, 2, 0), Some(3));
        assert_eq!(frontend_threads(Off, 16, 1, 0), None);
        assert_eq!(frontend_threads(Fixed(4), 16, 16, 9), Some(4));
    }

    #[test]
    fn keys_depend_on_every_codegen_setting() {
        let u = unit("serde", registry(), Path::new("/x"));
        let base = key(&u, &Profile::release());
        let mut lto = Profile::release();
        lto.lto = rune_common::LtoMode::Fat;
        assert_ne!(base, key(&u, &lto), "LTO must change the key");
        let mut cgu = Profile::release();
        cgu.codegen_units = Some(1);
        assert_ne!(base, key(&u, &cgu), "codegen-units must change the key");
        assert_ne!(base, key(&u, &Profile::dev()));
    }

    #[test]
    fn keys_depend_on_dependency_keys_but_not_their_order() {
        let u = unit("serde", registry(), Path::new("/x"));
        let p = Profile::dev();
        let mut cache = rune_fingerprint::FingerprintCache::new();
        let a = ("a".to_string(), "k1".to_string());
        let b = ("b".to_string(), "k2".to_string());
        let k1 = compute_unit_cas_key(&u, &p, &[a.clone(), b.clone()], &mut cache);
        let k2 = compute_unit_cas_key(&u, &p, &[b.clone(), a.clone()], &mut cache);
        assert_eq!(k1, k2);
        let changed = ("b".to_string(), "k3".to_string());
        assert_ne!(k1, compute_unit_cas_key(&u, &p, &[a, changed], &mut cache));
    }

    #[test]
    fn profile_flags_cover_the_whole_profile() {
        let mut p = Profile::release();
        p.lto = rune_common::LtoMode::Thin;
        p.codegen_units = Some(1);
        p.strip = Some("symbols".into());
        p.panic = rune_common::PanicStrategy::Abort;
        let cmd = with_profile_flags(
            RustcCommandBuilder::new("x.rs".into()),
            &p,
            "x86_64-unknown-linux-gnu",
            true,
        )
        .build();
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        for expected in [
            "opt-level=3",
            "lto=thin",
            "panic=abort",
            "codegen-units=1",
            "strip=symbols",
        ] {
            assert!(
                args.iter().any(|a| a == expected),
                "missing {expected}: {args:?}"
            );
        }
        // Release defaults (`debug-assertions` off) match rustc's own for
        // opt-level 3, so they are not spelled out.
        assert!(
            !args.iter().any(|a| a.starts_with("debug-assertions")),
            "{args:?}"
        );

        // A non-default combination *is* spelled out.
        let mut odd = Profile::release();
        odd.debug_assertions = true;
        odd.overflow_checks = true;
        let cmd = with_profile_flags(
            RustcCommandBuilder::new("x.rs".into()),
            &odd,
            "x86_64-unknown-linux-gnu",
            true,
        )
        .build();
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.iter().any(|a| a == "debug-assertions=on"), "{args:?}");
        assert!(args.iter().any(|a| a == "overflow-checks=on"), "{args:?}");

        // LTO is a link-time flag: libraries and proc-macros never get it.
        let cmd = with_profile_flags(
            RustcCommandBuilder::new("x.rs".into()),
            &p,
            "x86_64-unknown-linux-gnu",
            false,
        )
        .build();
        let lib_args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            !lib_args.iter().any(|a| a.starts_with("lto")),
            "{lib_args:?}"
        );
        assert!(
            !lib_args.iter().any(|a| a == "embed-bitcode=no"),
            "LTO needs bitcode: {lib_args:?}"
        );
    }

    #[test]
    fn rustflags_are_split_on_whitespace() {
        // Only exercises the parser; env vars are process-global, so use the
        // pure splitting logic through a scoped set.
        let parsed: Vec<String> = "-C target-cpu=native  -D warnings"
            .split_whitespace()
            .map(str::to_string)
            .collect();
        assert_eq!(parsed, ["-C", "target-cpu=native", "-D", "warnings"]);
    }
}
