use crate::inputs::{self, DeclaredInputs, effective_key};
use crate::pipeline::PipelineEvent;
use crate::schedule::{Gate, ScheduleReport, Step, StepId};
use crossbeam_channel::{Receiver, Sender, unbounded};
use igel_cas::CasStore;
use igel_cas::store::Content;
use igel_common::toolchain::{self, TargetInfo, Toolchain};
use igel_common::{
    BuildScriptOutput, CompilationUnit, CrateSource, CrateType, IgelConfig, IgelError, IgelResult,
    LinkArgScope, Profile, TargetKind,
};
use igel_compiler::{
    BuildScriptRunner, CompilationResult, RustcCommandBuilder, RustcInvoker,
    build_script::BuildScriptEnv,
};
use igel_graph::{BuildGraph, NodeId, compute_critical_paths_pipelined};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::{debug, error};

#[derive(Debug, Clone)]
pub struct BuildResult {
    pub success: bool,
    pub compiled: usize,
    pub cached: usize,
    pub failed: Vec<NodeId>,
    pub total_duration: Duration,
    pub artifact_paths: HashMap<NodeId, Vec<PathBuf>>,
    pub cache_keys: HashMap<NodeId, String>,
    pub build_script_outputs: HashMap<NodeId, BuildScriptOutput>,
    pub schedule: Option<ScheduleReport>,
    pub memory_held: Duration,
}

#[allow(clippy::large_enum_variant)]
enum WorkerMsg {
    MetadataReady {
        node_id: NodeId,
        rmeta_path: PathBuf,
        content: Option<Content>,
        /// The store holds a different build of this unit for the same dependencies: dependents must not be published.
        diverged: bool,
    },
    BuildScriptDone {
        node_id: NodeId,
        output: BuildScriptOutput,
    },
    /// External inputs (build script declarations or `env!` reads) changed the unit's key and its dependents'.
    KeyUpdate {
        node_id: NodeId,
        key: String,
    },
    Finished {
        node_id: NodeId,
        artifacts: Vec<PathBuf>,
        duration: Duration,
        peak_memory: Option<u64>,
        restored: bool,
        content: Option<Content>,
    },
    Failed {
        node_id: NodeId,
        error: String,
    },
    ScriptPhaseDone {
        node_id: NodeId,
        duration: Duration,
        lock: Option<igel_common::fsutil::FileLock>,
        lock_waited: bool,
    },
    LockAcquired {
        node_id: NodeId,
        lock: igel_common::fsutil::FileLock,
    },
    /// An executable is compiled and waits, without a job slot, until every rlib it links exists;
    /// a message on `resume` lets it link.
    Compiled {
        node_id: NodeId,
        resume: Sender<()>,
    },
}

/// Executables and tests compile against their dependencies' metadata, like libraries, and link
/// once every rlib exists: rustc-runified's `no-link`/`link-only`. Not proc-macros (loaded by
/// their dependents) nor dynamic or static libraries.
fn links_late(unit: &CompilationUnit) -> bool {
    !unit.check_only
        && !unit.is_build_script
        && unit.crate_type != CrateType::ProcMacro
        && (unit.harness || unit.all_crate_types().all(|t| t == CrateType::Bin))
}

fn frontend_threads(
    setting: igel_common::settings::FrontendThreads,
    cpus: usize,
    busy: usize,
    waiting: usize,
) -> Option<usize> {
    use igel_common::settings::FrontendThreads;
    let n = match setting {
        FrontendThreads::Off => return None,
        FrontendThreads::Fixed(n) => n,
        FrontendThreads::Auto => (cpus.saturating_sub(busy + waiting) + 1).min(8),
    };
    (n > 1).then_some(n)
}

/// A step whose remaining chain is at least this share of the longest one is on the critical path.
const CRITICAL_SHARE: f64 = 0.9;

/// Whether a step heads the longest chain of work left: the build ends no sooner than it does.
fn on_critical_path(
    weights: &HashMap<NodeId, f64>,
    completed: &HashSet<NodeId>,
    id: NodeId,
) -> bool {
    let longest = weights
        .iter()
        .filter(|(n, _)| !completed.contains(*n))
        .fold(0.0_f64, |m, (_, w)| m.max(*w));
    weights
        .get(&id)
        .is_some_and(|w| *w > 0.0 && *w >= CRITICAL_SHARE * longest)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ScriptStamp {
    base: String,
    sources: Vec<(PathBuf, String)>,
    warnings: String,
}

impl ScriptStamp {
    fn hash(p: &Path) -> String {
        igel_common::hash::xxhash3_file(p)
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
        link: false,
    }
}

fn link_step_id(node: NodeId) -> StepId {
    StepId {
        node: node.0,
        script: false,
        link: true,
    }
}

fn step_gates(
    graph: &BuildGraph,
    id: NodeId,
    script: bool,
    transitive_deps: &HashMap<NodeId, HashSet<NodeId>>,
    script_prereqs: &HashMap<NodeId, (Vec<NodeId>, Vec<NodeId>)>,
    late: &HashSet<NodeId>,
) -> Vec<Gate> {
    let end = |d: NodeId| Gate::End(end_step(d, late));
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
    let mut gates: Vec<Gate> = if node.unit.produces_real_artifact() && !late.contains(&id) {
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

/// The step after which a unit's outputs all exist: its link, for an executable that linked late.
fn end_step(id: NodeId, late: &HashSet<NodeId>) -> StepId {
    if late.contains(&id) {
        link_step_id(id)
    } else {
        step_id(id, false)
    }
}

/// A late link waits for its own compilation and for every rlib it links.
fn link_gates(
    id: NodeId,
    transitive_deps: &HashMap<NodeId, HashSet<NodeId>>,
    late: &HashSet<NodeId>,
) -> Vec<Gate> {
    std::iter::once(Gate::End(step_id(id, false)))
        .chain(
            transitive_deps[&id]
                .iter()
                .map(|d| Gate::End(end_step(*d, late))),
        )
        .collect()
}

fn unit_lock_path(
    config: &IgelConfig,
    profile: &Profile,
    graph: &BuildGraph,
    id: NodeId,
) -> PathBuf {
    config
        .profile_target_dir(profile.output_dir_name())
        .join(".igel-locks")
        .join(script_key(&graph.node(id).unit))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternMode {
    Pipelined,
    Link,
}

/// Parallel build executor: keys every unit up front, restores cache hits in parallel, then schedules the
/// rest by critical path with `.rmeta` pipelining.
pub struct BuildExecutor {
    config: IgelConfig,
    profile: Profile,
    cas: Option<CasStore>,
    event_tx: Sender<PipelineEvent>,
    event_rx: Receiver<PipelineEvent>,
    max_jobs: usize,
    force_rebuild: bool,
    config_env: Vec<(String, String, bool)>,
    color: bool,
    /// Shared with every `rustc` and build script so all codegen threads draw from one token pool.
    jobserver: Option<jobserver::Client>,
    tokens: Option<crate::tokens::JobTokens>,
    frontend_threads: igel_common::settings::FrontendThreads,
    /// rustc-runified can compile executables before their dependencies' rlibs exist.
    link_late: bool,
    cpus: usize,
    memory: fn() -> Option<igel_common::sysinfo::Memory>,
    sandbox: Option<std::sync::Arc<SandboxPolicy>>,
    extra_costs: HashMap<NodeId, f64>,
}

#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    pub sandbox: igel_common::sandbox::Sandbox,
    pub network: HashSet<String>,
}

impl BuildExecutor {
    pub fn new(config: IgelConfig, profile: Profile, max_jobs: usize) -> Self {
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
            link_late: toolchain::global()
                .runified
                .as_ref()
                .is_some_and(|d| d.supports("no-link") && d.supports("link-only")),
            sandbox: None,
            cpus: std::thread::available_parallelism().map_or(1, |n| n.get()),
            memory: igel_common::sysinfo::memory,
            extra_costs: HashMap::new(),
        }
    }

    pub fn with_extra_costs(mut self, costs: HashMap<NodeId, f64>) -> Self {
        self.extra_costs = costs;
        self
    }

    pub fn with_memory_probe(
        mut self,
        probe: fn() -> Option<igel_common::sysinfo::Memory>,
    ) -> Self {
        self.memory = probe;
        self
    }

    pub fn with_sandbox(mut self, policy: SandboxPolicy) -> Self {
        self.sandbox = Some(std::sync::Arc::new(policy));
        self
    }

    pub fn with_frontend_threads(mut self, t: igel_common::settings::FrontendThreads) -> Self {
        self.frontend_threads = t;
        self
    }

    /// Under `auto`, the compiler uses the cores nobody else will. Parallel compilers always get
    /// a count, 1 when the setting is `off`, so independent hyprc processes do not each create a
    /// machine-sized Rayon pool and oversubscribe the build.
    fn frontend_threads_for(&self, busy: usize, waiting: usize) -> Option<usize> {
        let tc = toolchain::global();
        let n = frontend_threads(self.frontend_threads, self.cpus, busy, waiting);
        if tc.hyprc || tc.runified.as_ref().is_some_and(|d| d.supports("threads")) {
            return Some(n.unwrap_or(1));
        }
        n.filter(|_| tc.accepts_unstable_flags())
    }

    pub fn with_cas(mut self, cas: CasStore) -> Self {
        self.cas = Some(cas);
        self
    }

    /// Never restore from the CAS (results are still stored). `build.release-rebuild-all`.
    pub fn with_force_rebuild(mut self, force: bool) -> Self {
        self.force_rebuild = force;
        self
    }

    pub fn with_config_env(mut self, env: Vec<(String, String, bool)>) -> Self {
        self.config_env = env;
        self
    }

    pub fn with_color(mut self, color: bool) -> Self {
        self.color = color;
        self
    }

    pub fn event_receiver(&self) -> Receiver<PipelineEvent> {
        self.event_rx.clone()
    }

    fn out_dir_for(&self, unit: &CompilationUnit) -> PathBuf {
        out_dir_for(&self.config, &self.profile, unit)
    }

    pub fn execute(&self, graph: &BuildGraph) -> IgelResult<BuildResult> {
        let result = self.execute_graph(graph);
        if result.is_err() {
            let _ = self.event_tx.send(PipelineEvent::AllComplete {
                total_duration: Duration::ZERO,
                cached: 0,
                compiled: 0,
                failed: 0,
            });
        }
        result
    }

    fn execute_graph(&self, graph: &BuildGraph) -> IgelResult<BuildResult> {
        let start_time = Instant::now();
        let profile_dir_name = self.profile.output_dir_name();
        self.config
            .ensure_dirs(profile_dir_name)
            .map_err(IgelError::Io)?;

        let topo = graph.topological_sort()?;

        let source_cache_path = self.config.fingerprint_dir.join("source-cache.json");
        let mut source_cache =
            igel_fingerprint::FingerprintCache::load(&source_cache_path).unwrap_or_default();
        let timings_path = self.config.fingerprint_dir.join("timings.json");
        let global_timings_path = self.config.cas_dir.join("timings.json");
        let mut timings = Timings::load(&timings_path, &global_timings_path);
        let mut steps: HashMap<StepId, Step> = HashMap::new();
        let mut started: HashMap<StepId, f64> = HashMap::new();
        let mut first_ready: HashMap<StepId, f64> = HashMap::new();
        let mut metadata_at: HashMap<NodeId, f64> = HashMap::new();
        let now = || start_time.elapsed().as_secs_f64();

        // 1. Cache keys, dependencies first. The stored key also covers the current values of declared inputs.
        let keys_timer = igel_common::timing::phase("cache keys");
        let mut unit_base_keys: HashMap<NodeId, String> = HashMap::with_capacity(topo.len());
        let mut unit_cas_keys: HashMap<NodeId, String> = HashMap::with_capacity(topo.len());
        // `igl check` reuses a build's stored result only if the unit's whole dependency closure is substituted:
        // rustc gives the two `.rmeta` flavours different crate hashes, so they must never mix in one chain.
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
        let mut contents: HashMap<NodeId, Content> = HashMap::new();
        // Compiled against a dependency build the store doesn't hold under its key: never published.
        let mut tainted: HashSet<NodeId> = HashSet::new();
        let mut compiled = 0;
        let mut cached = 0;
        let mut failed: Vec<NodeId> = Vec::new();

        // 2. Restore every cache hit in parallel.
        if let Some(cas) = self.cas.as_ref().filter(|_| !self.force_rebuild) {
            let _timer = igel_common::timing::phase("cas restore");
            let restore = |&id: &NodeId| -> Option<(NodeId, igel_cas::store::Linked)> {
                let unit = &graph.node(id).unit;
                let key = &unit_cas_keys[&id];
                if unit.build_script.is_some()
                    && unit.ext.script_owner.is_none()
                    && stored_script_output(
                        cas,
                        &format!("{key}-script-output"),
                        &self.config.build_script_out_dir_by_key(
                            self.profile.output_dir_name(),
                            &script_key(unit),
                        ),
                        &unit.manifest_dir,
                    )
                    .is_none()
                {
                    return None;
                }
                // Another `igl` may be compiling a unit with the same outputs right now; leave it to the scheduler.
                let path = unit_lock_path(&self.config, &self.profile, graph, id);
                let igel_common::fsutil::TryLock::Locked(_lock) =
                    igel_common::fsutil::FileLock::try_exclusive(&path)
                else {
                    return None;
                };
                let linked = cas
                    .link_entry(key, &self.out_dir_for(unit))
                    .inspect_err(|e| {
                        if cas.contains(key) {
                            debug!(
                                "could not restore {} v{}: {e}",
                                unit.crate_id.name, unit.crate_id.version
                            );
                        }
                    })
                    .ok()?;
                (!linked.files.is_empty()).then_some((id, linked))
            };
            let mut restored: HashMap<NodeId, igel_cas::store::Linked> = if topo.len() < 48 {
                topo.iter().filter_map(restore).collect()
            } else {
                topo.par_iter().filter_map(restore).collect()
            };

            // A restored unit is only usable next to the exact dependency builds it was compiled against.
            for &id in &topo {
                let Some(linked) = restored.get(&id) else {
                    continue;
                };
                let fits = compiled_against(graph, id, &contents)
                    .is_some_and(|deps| linked.manifest.matches_dependencies(&deps));
                match linked.manifest.content() {
                    Some(content) if fits => {
                        contents.insert(id, content);
                    }
                    None if fits => {}
                    _ => {
                        let unit = &graph.node(id).unit;
                        debug!(
                            "stored {} v{} was compiled against other builds of its dependencies: compiling it again",
                            unit.crate_id.name, unit.crate_id.version
                        );
                        restored.remove(&id);
                    }
                }
            }

            for &id in &topo {
                let Some(linked) = restored.remove(&id) else {
                    continue;
                };
                let linked = linked.files;
                let unit = &graph.node(id).unit;
                debug!(
                    "CAS hit for {} v{}",
                    unit.crate_id.name, unit.crate_id.version
                );
                cached += 1;
                completed.insert(id);
                metadata_ready.insert(id);

                // A cache hit never runs the build script, but dependents and sibling units need its output.
                if unit.build_script.is_some() {
                    let script_key_name = format!("{}-script-output", unit_cas_keys[&id]);
                    let script_out_dir = self.config.build_script_out_dir_by_key(
                        self.profile.output_dir_name(),
                        &script_key(unit),
                    );
                    if let Some(output) = restore_script_output(
                        cas,
                        &script_key_name,
                        &script_out_dir,
                        &unit.manifest_dir,
                    ) {
                        let _ = self.event_tx.send(PipelineEvent::ScriptOutput {
                            node_id: id,
                            output: output.clone(),
                        });
                        replay_script_warnings(&self.event_tx, id, Some(&output));
                        build_script_outputs.insert(id, output);
                    }
                }
                if let Some(rmeta) = linked
                    .iter()
                    .find(|p| p.extension().and_then(|e| e.to_str()) == Some("rmeta"))
                {
                    metadata_paths.insert(id, rmeta.clone());
                }
                uplift_artifacts(&self.config, &self.profile, unit, &linked);
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

        // 3. Critical-path priorities over the remaining work (measured times, else size estimates).
        let weights = {
            let _timer = igel_common::timing::phase("critical path");
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

        // Transitive closure: anything that links needs every transitive `.rlib`, not just direct deps' `.rmeta`.
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

        // A build script is its own step: it needs its build-dependencies and `links` scripts, not the package's
        // ordinary dependencies, so it runs while those still compile.
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
        let mut in_script_phase: HashSet<NodeId> = HashSet::new();
        // Executables compiled before their dependencies, waiting to link (`links_late`).
        // Not when a dependency is a Rust dylib: there is no rlib to link it from.
        let late_ok = |id: NodeId| {
            self.link_late
                && links_late(&graph.node(id).unit)
                && !transitive_deps[&id].iter().any(|d| {
                    graph
                        .node(*d)
                        .unit
                        .all_crate_types()
                        .any(|t| t == CrateType::Dylib)
                })
        };
        let mut late: HashSet<NodeId> = HashSet::new();
        let mut awaiting_link: HashMap<NodeId, Sender<()>> = HashMap::new();
        let mut compiled_at: HashMap<NodeId, f64> = HashMap::new();
        // Held from the script step through compilation so no other process reruns the script under a compiler.
        let mut held_locks: HashMap<NodeId, (igel_common::fsutil::FileLock, bool)> = HashMap::new();
        let mut lock_waiting: HashSet<NodeId> = HashSet::new();
        let mut job_threads: Vec<std::thread::JoinHandle<()>> = Vec::new();
        let mut memory_held = Duration::ZERO;
        let mut predicted_running: HashMap<NodeId, u64> = HashMap::new();
        let mut memory_held_since: Option<Instant> = None;

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

            if failed.is_empty() {
                // Every rlib an executable links exists: it links now (it needs a job slot again).
                let linkable: Vec<NodeId> = awaiting_link
                    .keys()
                    .copied()
                    .filter(|id| transitive_deps[id].iter().all(|d| completed.contains(d)))
                    .collect();
                for id in linkable {
                    let step = link_step_id(id);
                    first_ready.entry(step).or_insert_with(now);
                    if in_flight.len() >= self.max_jobs {
                        break;
                    }
                    if let Some(resume) = awaiting_link.remove(&id)
                        && resume.send(()).is_ok()
                    {
                        in_flight.insert(id);
                        started.insert(step, now());
                    }
                }

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
                        let ready = if node.unit.produces_real_artifact() && !late_ok(id) {
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
                    // Short of memory: let running compilers finish before starting more.
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
                    // Another process holding this unit's lock owns its outputs; wait without a job slot, then likely restore.
                    let (lock, lock_waited) = match held_locks.remove(&node_id) {
                        Some(held) => held,
                        None => {
                            let path = unit_lock_path(&self.config, &self.profile, graph, node_id);
                            match igel_common::fsutil::FileLock::try_exclusive(&path) {
                                igel_common::fsutil::TryLock::Locked(lock) => (lock, false),
                                igel_common::fsutil::TryLock::Busy => {
                                    lock_waiting.insert(node_id);
                                    let tx = worker_tx.clone();
                                    std::thread::spawn(move || {
                                        let (lock, _) =
                                            igel_common::fsutil::FileLock::exclusive(&path);
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
                    let link_late = !script_only && late_ok(node_id);
                    let mut job = self.prepare_job(
                        graph,
                        node_id,
                        link_late,
                        &unit_base_keys[&node_id],
                        &unit_cas_keys[&node_id],
                        dep_keys_of(graph, node_id, &unit_cas_keys),
                        &artifact_paths,
                        &metadata_paths,
                        &build_script_outputs,
                        worker_tx.clone(),
                    );
                    job.script_only = script_only;
                    if !script_only {
                        job.compiled_against = compiled_against(graph, node_id, &contents);
                        let fits = job.compiled_against.is_some()
                            && extern_dependencies(graph, node_id).all(|d| !tainted.contains(&d));
                        if !fits {
                            tainted.insert(node_id);
                        }
                        job.publish = fits;
                    }
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
                    // Critical path prioritization: allocates dedicated job tokens to scripts
                    // and boosts frontend threads on critical compiles when parallel frontend is enabled.
                    let experiment = std::env::var("IGEL_EXPERIMENT_CRITICAL")
                        .unwrap_or_else(|_| "tokens".into());
                    let has =
                        |f: &str| experiment == "all" || experiment.split(',').any(|x| x == f);
                    let critical = on_critical_path(&weights, &completed, node_id);
                    let boost = critical
                        && if script_only {
                            has("tokens")
                        } else {
                            has("threads")
                        };
                    if boost {
                        job.own_tokens = Some(self.cpus);
                        debug!(
                            "critical path: {} gets {} tokens of its own",
                            graph.node(node_id).unit.crate_id.name,
                            self.cpus
                        );
                    }
                    if !critical && has("nice") {
                        job.nice = Some(5);
                    }
                    if !script_only {
                        job.frontend_threads =
                            self.frontend_threads_for(in_flight.len(), ready_count - index - 1);
                        if boost
                            && (job.frontend_threads.is_some()
                                || self.frontend_threads
                                    == igel_common::settings::FrontendThreads::Auto)
                        {
                            job.frontend_threads = Some(self.cpus.min(8));
                        }
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
            while let Some(msg) = next.take().or_else(|| worker_rx.try_recv().ok()) {
                match msg {
                    WorkerMsg::MetadataReady {
                        node_id,
                        rmeta_path,
                        content,
                        diverged,
                    } => {
                        debug!("Metadata ready for node {:?}", node_id);
                        metadata_at.entry(node_id).or_insert_with(now);
                        metadata_ready.insert(node_id);
                        metadata_paths.insert(node_id, rmeta_path);
                        if let Some(content) = content {
                            contents.insert(node_id, content);
                        }
                        if diverged {
                            let unit = &graph.node(node_id).unit;
                            debug!(
                                "{} v{} compiled to other bytes than the stored build of it: \
                                 what is compiled against it is not stored",
                                unit.crate_id.name, unit.crate_id.version
                            );
                            tainted.insert(node_id);
                        }
                    }
                    WorkerMsg::BuildScriptDone { node_id, output } => {
                        let _ = self.event_tx.send(PipelineEvent::ScriptOutput {
                            node_id,
                            output: output.clone(),
                        });
                        build_script_outputs.insert(node_id, output);
                    }
                    WorkerMsg::Compiled { node_id, resume } => {
                        in_flight.remove(&node_id);
                        predicted_running.remove(&node_id);
                        late.insert(node_id);
                        compiled_at.insert(node_id, now());
                        awaiting_link.insert(node_id, resume);
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
                                    &late,
                                ),
                            },
                        );
                    }
                    WorkerMsg::KeyUpdate { node_id, key } => {
                        // The key changed; recompute the keys of everything above it that hasn't started.
                        unit_cas_keys.insert(node_id, key);
                        let mut changed: HashSet<NodeId> = HashSet::from([node_id]);
                        for &id in &topo {
                            if id == node_id
                                || completed.contains(&id)
                                || (in_flight.contains(&id) && !in_script_phase.contains(&id))
                                || late.contains(&id)
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
                        content,
                    } => {
                        if let Some(content) = content {
                            contents.entry(node_id).or_insert(content);
                        }
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
                                end: compiled_at.get(&node_id).copied().unwrap_or_else(now),
                                gates: step_gates(
                                    graph,
                                    node_id,
                                    false,
                                    &transitive_deps,
                                    &script_prereqs,
                                    &late,
                                ),
                            },
                        );
                        if late.contains(&node_id) {
                            let id = link_step_id(node_id);
                            let begun = started.get(&id).copied().unwrap_or(0.0);
                            steps.insert(
                                id,
                                Step {
                                    label: format!("{} (link)", span_name(unit)),
                                    kind: "link",
                                    ready: first_ready.get(&id).copied().unwrap_or(begun),
                                    start: begun,
                                    metadata: None,
                                    end: now(),
                                    gates: link_gates(node_id, &transitive_deps, &late),
                                },
                            );
                        }
                        if restored {
                            cached += 1;
                        } else {
                            compiled += 1;
                        }
                    }
                    WorkerMsg::Failed { node_id, error } => {
                        debug!("Failed to compile node {:?}: {}", node_id, error);
                        in_flight.remove(&node_id);
                        in_script_phase.remove(&node_id);
                        held_locks.remove(&node_id);
                        failed.push(node_id);
                        // Dependents of an early `.rmeta` wait for the rest of it: it won't come.
                        if let Some(rmeta) = metadata_paths.get(&node_id)
                            && toolchain::global()
                                .runified
                                .as_ref()
                                .is_some_and(|d| d.supports("early-metadata"))
                        {
                            igel_common::runified::metadata_never_completes(rmeta);
                        }
                    }
                }
            }
        }
        if let Some(since) = memory_held_since.take() {
            memory_held += since.elapsed();
        }
        // Units report `Finished` before they are stored; the build ends once every store has landed.
        drop(held_locks);
        // Executables still waiting to link after a failure: let them go.
        drop(awaiting_link);
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
        if let (true, Some(report)) = (igel_common::timing::enabled(), &schedule) {
            let trace_path = self.config.fingerprint_dir.join("trace.json");
            crate::schedule::write_chrome_trace(&trace_path, &steps);
            let html_path = self
                .config
                .igel_dir
                .join("timings")
                .join("igel-timing.html");
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

    fn compute_keys(
        &self,
        graph: &BuildGraph,
        id: NodeId,
        keys: &HashMap<NodeId, String>,
        source_cache: &mut igel_fingerprint::FingerprintCache,
        as_full_build: bool,
    ) -> (String, String) {
        let node = graph.node(id);
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

    /// Whether a unit can be keyed by the files it read: `Some(None)` local crate without a build script,
    /// `Some(Some(key))` with script result `key`, `None` keyed by every file of its package.
    fn read_mode(
        &self,
        graph: &BuildGraph,
        id: NodeId,
        keys: &HashMap<NodeId, String>,
        source_cache: &mut igel_fingerprint::FingerprintCache,
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

    fn read_record(
        &self,
        graph: &BuildGraph,
        id: NodeId,
        unit: &CompilationUnit,
        keys: &HashMap<NodeId, String>,
        source_cache: &mut igel_fingerprint::FingerprintCache,
    ) -> Option<(inputs::ReadRecord, Option<String>)> {
        let as_full_build = unit.check_only != graph.node(id).unit.check_only;
        let script = self.read_mode(graph, id, keys, source_cache, as_full_build)?;
        let key = read_record_key(unit, source_cache)?;
        let record = self.cas.as_ref()?.get_value(&format!("{key}-read"))?;
        Some((record, script))
    }

    fn unit_identity(
        &self,
        graph: &BuildGraph,
        id: NodeId,
        keys: &HashMap<NodeId, String>,
        source_cache: &mut igel_fingerprint::FingerprintCache,
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

    pub fn current_identities(
        &self,
        graph: &BuildGraph,
    ) -> IgelResult<Vec<(NodeId, String, UnitIdentity)>> {
        let topo = graph.topological_sort()?;
        let cache_path = self.config.fingerprint_dir.join("source-cache.json");
        let mut source_cache =
            igel_fingerprint::FingerprintCache::load(&cache_path).unwrap_or_default();
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

    pub fn store_keys(&self, graph: &BuildGraph) -> IgelResult<Vec<String>> {
        let topo = graph.topological_sort()?;
        let cache_path = self.config.fingerprint_dir.join("source-cache.json");
        let mut source_cache =
            igel_fingerprint::FingerprintCache::load(&cache_path).unwrap_or_default();
        let mut keys: HashMap<NodeId, String> = HashMap::new();
        let mut script_outputs: HashMap<String, BuildScriptOutput> = HashMap::new();
        let mut out = Vec::new();
        for id in topo {
            let (base, key) = self.compute_keys(graph, id, &keys, &mut source_cache, false);
            out.extend([
                key.clone(),
                format!("{key}-script-output"),
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
                let result = format!("{script}-script-output");
                let script_out_dir = self
                    .config
                    .build_script_out_dir_by_key(self.profile.output_dir_name(), &script_key(unit));
                if let Some((_, output)) = self.cas.as_ref().and_then(|c| {
                    stored_script_output(c, &result, &script_out_dir, &unit.manifest_dir)
                }) {
                    script_outputs.insert(script_key(unit), output);
                }
                out.extend([format!("{script_base}-script-record"), result]);
            }
            keys.insert(id, key);
        }
        Ok(out)
    }

    fn script_keys(
        &self,
        graph: &BuildGraph,
        id: NodeId,
        keys: &HashMap<NodeId, String>,
        dep_metadata: &[(String, String, String)],
        source_cache: &mut igel_fingerprint::FingerprintCache,
    ) -> (String, String, String) {
        let unit = &graph.node(id).unit;
        self.script_keys_of(graph, id, unit, keys, dep_metadata, source_cache)
    }

    fn script_keys_of(
        &self,
        graph: &BuildGraph,
        id: NodeId,
        unit: &CompilationUnit,
        keys: &HashMap<NodeId, String>,
        dep_metadata: &[(String, String, String)],
        source_cache: &mut igel_fingerprint::FingerprintCache,
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
            igel_fingerprint::hasher::compute_package_hash(&unit.manifest_dir, source_cache)
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

    pub fn is_cached(&self, key: &str) -> bool {
        self.cas.as_ref().is_some_and(|c| c.contains(key))
    }

    fn save_identities(
        &self,
        graph: &BuildGraph,
        topo: &[NodeId],
        built: impl Fn(NodeId) -> bool,
        keys: &HashMap<NodeId, String>,
        source_cache: &mut igel_fingerprint::FingerprintCache,
    ) {
        let mut all = load_identities(&self.config.fingerprint_dir);
        let before = all.clone();
        for &id in topo {
            if !built(id) {
                continue;
            }
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
            let _ = igel_common::fsutil::write_atomic(&path, text.as_bytes());
        }
    }
}

/// Dependency keys named by how they are depended on (rename, normal/build): the manifest isn't hashed.
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
    #[allow(clippy::too_many_arguments)]
    fn prepare_job(
        &self,
        graph: &BuildGraph,
        node_id: NodeId,
        link_late: bool,
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
        let mode = if unit.produces_real_artifact() && !link_late {
            ExternMode::Link
        } else {
            ExternMode::Pipelined
        };
        let extern_deps = extern_deps_for(graph, node_id, artifact_paths, metadata_paths, mode);

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
                    .filter(|_| dep_node.unit.crate_type != CrateType::Bin)
            })
            .collect();

        // `DEP_<LINKS>_<KEY>` from `links` crates' build scripts.
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

        // `CARGO_BIN_EXE_<name>` for every binary this unit depends on.
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

        // Link search paths from the build scripts of everything this unit links against, as Cargo passes them.
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
            own_tokens: None,
            nice: None,
            link_late,
            script_base: None,
            script_cache_key: None,
            package_hash: String::new(),
            read_script: None,
            compiled_against: None,
            publish: false,
        }
    }
}

struct UnitJob {
    node_id: NodeId,
    unit: CompilationUnit,
    extern_deps: Vec<(String, PathBuf)>,
    build_script_extern_deps: Vec<(String, PathBuf)>,
    dep_metadata: Vec<(String, String, String)>,
    base_key: String,
    cas_key: String,
    dep_keys: Vec<(String, String)>,
    build_output: Option<BuildScriptOutput>,
    dep_link_search: Vec<(Option<String>, PathBuf)>,
    config: IgelConfig,
    profile: Profile,
    cas: Option<CasStore>,
    toolchain: &'static Toolchain,
    event_tx: Sender<PipelineEvent>,
    worker_tx: Sender<WorkerMsg>,
    force_rebuild: bool,
    max_jobs: usize,
    config_env: Vec<(String, String, bool)>,
    extra_env: Vec<(String, String)>,
    color: bool,
    jobserver: Option<jobserver::Client>,
    tokens: Option<crate::tokens::JobTokens>,
    sandbox: Option<std::sync::Arc<SandboxPolicy>>,
    script_only: bool,
    lock: Option<igel_common::fsutil::FileLock>,
    lock_waited: bool,
    frontend_threads: Option<usize>,
    /// On the critical path: a jobserver of its own with this many tokens, so what the step runs
    /// in parallel (a build script's C compiles) isn't held to what other jobs leave of the shared one.
    own_tokens: Option<usize>,
    /// Off the critical path: this much lower CPU priority, so the critical path runs first.
    nice: Option<i32>,
    /// Compile against the dependencies' metadata now, link once their rlibs exist.
    link_late: bool,
    script_base: Option<String>,
    script_cache_key: Option<String>,
    package_hash: String,
    read_script: Option<Option<String>>,
    compiled_against: Option<std::collections::BTreeMap<String, String>>,
    publish: bool,
}

impl UnitJob {
    fn compiler_command(&self, member: bool) -> std::process::Command {
        let cmd = self.toolchain.rustc_command(member);
        match &self.sandbox {
            Some(p) => p.sandbox.wrap(&cmd, &[&self.config.target_dir], false),
            None => cmd,
        }
    }

    fn host_compiler_command(&self, member: bool) -> std::process::Command {
        let cmd = if self.toolchain.hyprc {
            self.toolchain.host_rustc_command(member)
        } else {
            self.toolchain.rustc_command(member)
        };
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

    fn restore_finished_elsewhere(&self, start: Instant) -> bool {
        let Some(cas) = &self.cas else { return false };
        let unit = &self.unit;
        let has_script = unit.build_script.is_some();
        let mut base = self.base_key.clone();
        let mut files = igel_fingerprint::FingerprintCache::new();
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
        let fits = |m: &igel_cas::store::ArtifactManifest| {
            self.compiled_against
                .as_ref()
                .is_some_and(|deps| m.matches_dependencies(deps))
        };
        let script_out_dir = self
            .config
            .build_script_out_dir_by_key(self.profile.output_dir_name(), &script_key(unit));
        let script_key_name = format!("{key}-script-output");
        if !cas.get(&key).is_some_and(|e| fits(&e.manifest))
            || (has_script
                && unit.ext.script_owner.is_none()
                && stored_script_output(cas, &script_key_name, &script_out_dir, &unit.manifest_dir)
                    .is_none())
        {
            return false;
        }
        let out_dir = out_dir_for(&self.config, &self.profile, unit);
        let Ok(igel_cas::store::Linked {
            files: linked,
            manifest,
        }) = cas.link_entry(&key, &out_dir)
        else {
            return false;
        };
        if linked.is_empty() || !fits(&manifest) {
            return false;
        }
        let content = manifest.content();
        if key != self.cas_key {
            let _ = self.worker_tx.send(WorkerMsg::KeyUpdate {
                node_id: self.node_id,
                key: key.clone(),
            });
        }
        if has_script
            && let Some(output) =
                restore_script_output(cas, &script_key_name, &script_out_dir, &unit.manifest_dir)
        {
            let _ = self.worker_tx.send(WorkerMsg::BuildScriptDone {
                node_id: self.node_id,
                output,
            });
        }
        if let Some(rmeta) = linked
            .iter()
            .find(|p| p.extension().and_then(|e| e.to_str()) == Some("rmeta"))
        {
            let _ = self.worker_tx.send(WorkerMsg::MetadataReady {
                node_id: self.node_id,
                rmeta_path: rmeta.clone(),
                content: content.clone(),
                diverged: false,
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
            content,
        });
        true
    }

    fn run(mut self) {
        // Claim a core without waiting behind other compilers' codegen threads (see `tokens`).
        let mut token = self.tokens.as_ref().map(|t| t.start());
        let start = Instant::now();
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

        if self.lock_waited
            && !self.script_only
            && !self.force_rebuild
            && self.restore_finished_elsewhere(start)
        {
            return;
        }

        // The build script result has its own slot, so a recompiled crate needn't rerun an unchanged script.
        let build_script_cache_key = match &self.script_cache_key {
            Some(k) => format!("{k}-script-output"),
            None => format!("{}-script-output", self.cas_key),
        };
        let mut script_sources: Vec<PathBuf> = Vec::new();

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
        let mut declared = DeclaredInputs::default();
        let mut store_key = self.cas_key.clone();
        let mut script_ran: Option<BuildScriptOutput> = None;
        let has_script = unit.build_script.is_some();
        let script_only = self.script_only;
        let mut base_key = self.base_key.clone();
        let read_key = self
            .read_script
            .as_ref()
            .and_then(|_| read_record_key(unit, &mut igel_fingerprint::FingerprintCache::new()));
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
            effective_build_output = restore_script_output(
                cas_store,
                &build_script_cache_key,
                &script_out_dir,
                &unit.manifest_dir,
            );
            replay_script_warnings(&self.event_tx, node_id, effective_build_output.as_ref());
        }

        if let (Some(build_rs), true) = (
            unit.build_script.clone(),
            effective_build_output.is_none() && !shares_script,
        ) {
            let _ = self.event_tx.send(PipelineEvent::BuildScriptStarted {
                node_id,
                crate_name: unit.crate_id.name.clone(),
            });
            // Start from an empty directory: restored files are hard links to shared objects.
            let _ = std::fs::remove_dir_all(&script_out_dir);
            let _ = std::fs::create_dir_all(&script_out_dir);

            let deps_dir = config.deps_dir(profile.output_dir_name());
            let tc = self.toolchain;
            let eff = unit.effective_profile(profile);
            // `TARGET` must be a real triple; `CARGO_CFG_*` describes the target, never this machine.
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
                debug: eff.debuginfo != igel_common::DebugInfo::None,
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
                hyprc: tc.hyprc,
            };
            let compile_env = BuildScriptEnv {
                linker: unit.ext.script_linker.as_ref().map(PathBuf::from),
                ..env.clone()
            };

            let script_flags = unit
                .ext
                .script_profile
                .as_ref()
                .map(|p| {
                    let p = Profile {
                        panic: igel_common::PanicStrategy::Unwind,
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
                if unit.crate_id.source != CrateSource::Local {
                    cmd.arg("--cap-lints").arg("allow");
                }
                if let Some(js) = &self.jobserver {
                    js.configure(cmd);
                }
                if let Some(n) = self.nice {
                    igel_common::sysinfo::lower_priority_of(cmd, n);
                }
            };
            let script_bin_dir =
                config.build_script_bin_dir_by_key(profile.output_dir_name(), &script_key(unit));
            // A stamp beside the compiled script: rerunning a script doesn't recompile it.
            let stamp_path = script_bin_dir.join(".igel-stamp");
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
                        self.host_compiler_command(unit.ext.member),
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
                            let _ = igel_common::fsutil::write_atomic(&stamp_path, text.as_bytes());
                        }
                    }
                    compiled
                }
            };
            match compiled {
                Ok((bin_path, printed)) => {
                    let compile_warnings = if unit.crate_id.source == CrateSource::Local {
                        printed
                    } else {
                        String::new()
                    };
                    let own = self.own_tokens.and_then(|n| jobserver::Client::new(n).ok());
                    let run_configure = |cmd: &mut std::process::Command| {
                        if let Some(js) = own.as_ref().or(self.jobserver.as_ref()) {
                            js.configure(cmd);
                        }
                        if let Some(n) = self.nice {
                            igel_common::sysinfo::lower_priority_of(cmd, n);
                        }
                    };
                    // Registry/git packages are immutable and shared: their scripts may only write to `OUT_DIR`.
                    let immutable_before = (unit.crate_id.source != CrateSource::Local)
                        .then(|| tree_stamp(&unit.manifest_dir));
                    let script_cmd = match &self.sandbox {
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
                    format!("{key}-script-output")
                }
                None => format!("{store_key}-script-output"),
            };
            store_script_output(cas_store, &key, out, &script_out_dir, &unit.manifest_dir);
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

        // Unlink leftover outputs first: they are hard links into the CAS.
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
        for (k, v, force) in &self.config_env {
            if *force || std::env::var_os(k).is_none() {
                cmd_builder = cmd_builder.env(k, v);
            }
        }
        // What links the executable later: this command, minus what only compiling needs.
        let link_builder = self.link_late.then(|| cmd_builder.clone());
        if let Some(n) = self.frontend_threads {
            if self.toolchain.hyprc {
                cmd_builder = cmd_builder.env("RAYON_NUM_THREADS", &n.to_string());
            } else {
                cmd_builder = cmd_builder.extra_flags(vec![if self.toolchain.runified.is_some() {
                    format!("--runified=threads={n}")
                } else {
                    format!("-Zthreads={n}")
                }]);
            }
        }
        if self.link_late {
            cmd_builder = cmd_builder.extra_flags(vec!["--runified=no-link".into()]);
        }
        let compiler_base = if unit.crate_type == CrateType::ProcMacro {
            self.host_compiler_command(unit.ext.member)
        } else {
            self.compiler_command(unit.ext.member)
        };
        let mut cmd = cmd_builder.build_with_command(compiler_base);
        // Alive until the compiler exits.
        let own = self.own_tokens.and_then(|n| jobserver::Client::new(n).ok());
        if let Some(js) = own.as_ref().or(self.jobserver.as_ref()) {
            js.configure(&mut cmd);
        }
        if let Some(n) = self.nice {
            igel_common::sysinfo::lower_priority_of(&mut cmd, n);
        }
        debug!("Running {:?}", cmd);

        // Pipelined: report `.rmeta` as soon as it exists. Dep-info comes first and lists `env!` reads.
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
        let mut own_content: Option<Content> = None;
        let result = invoker.invoke_streaming(cmd, &mut |artifact| match artifact.emit.as_str() {
            "dep-info" => {
                let mut moved = false;
                if read_key.is_some() {
                    let record = inputs::ReadRecord::from_dep_info(
                        &artifact.artifact,
                        igel_common::fsutil::simplified(&self.config.project_root),
                        &unit.manifest_dir,
                        &self.config.target_dir,
                    );
                    let script = self.read_script.clone().flatten();
                    let base = compute_unit_key_from(
                        unit,
                        profile,
                        &self.dep_keys,
                        &mut igel_fingerprint::FingerprintCache::new(),
                        Some((&record, script.as_deref())),
                    );
                    read_record = Some(record);
                    if base != base_key {
                        base_key = base;
                        moved = true;
                    }
                }
                let reads = inputs::env_reads_from_dep_info(&artifact.artifact, &provided);
                let manifest = igel_common::manifest_file::manifest_in(&unit.manifest_dir);
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
                // A non-reproducible compiler output: nothing compiled against it may be stored.
                own_content =
                    igel_cas::store::content_of_files(std::slice::from_ref(&artifact.artifact));
                let diverged = match (&self.cas, &own_content, &self.compiled_against) {
                    (Some(cas), Some(own), Some(deps)) if self.publish => {
                        cas.manifest(&store_key).is_some_and(|m| {
                            m.matches_dependencies(deps) && m.content().is_some_and(|c| c != *own)
                        })
                    }
                    _ => false,
                };
                let _ = event_tx.send(PipelineEvent::MetadataReady {
                    node_id,
                    rmeta_path: artifact.artifact.clone(),
                });
                let _ = worker_tx.send(WorkerMsg::MetadataReady {
                    node_id,
                    rmeta_path: artifact.artifact.clone(),
                    content: own_content.clone(),
                    diverged,
                });
            }
            _ => {}
        });

        match result {
            Ok(mut comp_res) if comp_res.success => {
                let mut waited = Duration::ZERO;
                if let Some(link) = link_builder {
                    // Compiled: wait for every rlib without a job slot or token, then link.
                    drop(token.take());
                    let (resume, resumed) = crossbeam_channel::bounded(1);
                    let paused = Instant::now();
                    let _ = self.worker_tx.send(WorkerMsg::Compiled { node_id, resume });
                    if resumed.recv().is_err() {
                        return; // the build stopped
                    }
                    waited = paused.elapsed();
                    token = self.tokens.as_ref().map(|t| t.start());
                    let rlink = out_dir.join(format!(
                        "{}-{}.rlink",
                        unit.rustc_crate_name(),
                        unit.metadata_hash()
                    ));
                    let mut cmd = link
                        .for_link_only(&rlink)
                        .extra_flags(vec!["--runified=link-only".into()])
                        .build_with_command(self.compiler_command(unit.ext.member));
                    if let Some(js) = &self.jobserver {
                        js.configure(&mut cmd);
                    }
                    debug!("Linking {:?}", cmd);
                    let linked = RustcInvoker::new().invoke_streaming(cmd, &mut |_| {});
                    let _ = std::fs::remove_file(&rlink);
                    match linked {
                        Ok(l) if l.success => {
                            comp_res.artifacts.extend(l.artifacts);
                            comp_res.diagnostics.extend(l.diagnostics);
                            comp_res.stderr_raw.push_str(&l.stderr_raw);
                            comp_res.peak_memory = comp_res.peak_memory.max(l.peak_memory);
                        }
                        Ok(l) => {
                            let err_msg = render_compile_error(&l);
                            let diagnostics = igel_compiler::diagnostic_lines(&l.stderr_raw);
                            self.fail_with(err_msg.clone(), err_msg, diagnostics);
                            return;
                        }
                        Err(e) => {
                            self.fail(e.to_string(), e.to_string());
                            return;
                        }
                    }
                }
                let duration = start.elapsed().saturating_sub(waited);
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
                    igel_compiler::diagnostic_lines(&comp_res.stderr_raw)
                } else {
                    Vec::new()
                };
                let artifacts: Vec<PathBuf> =
                    comp_res.artifacts.into_iter().map(|a| a.artifact).collect();
                uplift_artifacts(config, profile, unit, &artifacts);
                if own_content.is_none() {
                    own_content = igel_cas::store::content_of_files(&artifacts);
                }

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
                    content: own_content,
                });
                // Storing is disk work: give the job token back meanwhile.
                drop(token);

                if let Some(cas_store) = &self.cas {
                    if let (true, Some(deps)) = (self.publish, &self.compiled_against) {
                        let _ = cas_store.put_unit(
                            &store_key,
                            &unit.crate_id.name,
                            &unit.crate_id.version.to_string(),
                            &artifacts,
                            &igel_cas::store::PutOptions {
                                adopt: true,
                                compiled_against: deps.clone(),
                                replace_stale: true,
                            },
                        );
                    }
                    if !warnings.is_empty() {
                        let _ = cas_store.put_value(&format!("{store_key}-warnings"), &warnings);
                    }
                    if !diagnostics.is_empty() {
                        let _ =
                            cas_store.put_value(&format!("{store_key}-diagnostics"), &diagnostics);
                    }
                    if !declared.is_empty() {
                        let _ = cas_store.put_value(&format!("{base_key}-inputs"), &declared);
                    }
                    if let (Some(key), Some(record)) = (&read_key, &read_record) {
                        let _ = cas_store.put_value(&format!("{key}-read"), record);
                    }
                    if let (Some(out), false) = (&effective_build_output, shares_script) {
                        let key = format!("{store_key}-script-output");
                        if stored_script_output(
                            cas_store,
                            &key,
                            &script_out_dir,
                            &unit.manifest_dir,
                        )
                        .is_none()
                        {
                            store_script_output(
                                cas_store,
                                &key,
                                out,
                                &script_out_dir,
                                &unit.manifest_dir,
                            );
                        }
                    }
                }
                drop(unit_lock);
            }
            Ok(comp_res) => {
                let err_msg = render_compile_error(&comp_res);
                let diagnostics = igel_compiler::diagnostic_lines(&comp_res.stderr_raw);
                self.fail_with(err_msg.clone(), err_msg, diagnostics);
            }
            Err(e) => self.fail(e.to_string(), e.to_string()),
        }
    }
}

fn with_hint(message: String) -> String {
    match crate::hints::hint_for_failure(&message, crate::hints::Family::current()) {
        Some(hint) => format!("{message}\n\n{hint}"),
        None => message,
    }
}

/// Fingerprint of files below `dir` (path, size, mtime). Directory mtimes are ignored: tools create and
/// remove scratch directories in the package root.
fn tree_stamp(dir: &Path) -> u64 {
    let mut hb = igel_common::hash::HashBuilder::new();
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
    igel_common::hash::xxhash3(hb.finish_blake3().as_bytes())
}

pub fn script_key(unit: &CompilationUnit) -> String {
    format!("{}-{}", unit.rustc_crate_name(), unit.metadata_hash())
}

fn pinned_script_key(key: &str, out_dir: &Path, manifest_dir: &Path) -> String {
    let at = format!(
        "{}\0{}",
        igel_common::fsutil::simplified(out_dir).display(),
        igel_common::fsutil::simplified(manifest_dir).display()
    );
    format!("{key}-at-{}", igel_common::hash::xxhash3_hex(at.as_bytes()))
}

/// Whether a script's result names this project's directories in a way that cannot be relocated.
fn names_project(output: &BuildScriptOutput, out_dir: &Path, manifest_dir: &Path) -> bool {
    let build_dir = out_dir.parent().and_then(Path::parent).unwrap_or(out_dir);
    let dirs = [
        igel_common::fsutil::simplified(build_dir),
        igel_common::fsutil::simplified(manifest_dir),
    ];
    if output.names_any(&dirs) {
        return true;
    }
    fn sources(dir: &Path, found: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(t) if t.is_dir() => sources(&path, found),
                Ok(t) if t.is_file() && path.extension().is_some_and(|e| e == "rs") => {
                    found.push(path)
                }
                _ => {}
            }
        }
    }
    let mut files = Vec::new();
    sources(out_dir, &mut files);
    files.iter().any(|f| {
        std::fs::read(f).is_ok_and(|bytes| {
            let text = String::from_utf8_lossy(&bytes);
            dirs.iter().any(|d| igel_common::text_names_dir(&text, d))
        })
    })
}

/// Stores a build script's result with portable placeholders; one that still names the project is
/// stored for this project alone.
fn store_script_output(
    cas: &CasStore,
    key: &str,
    output: &BuildScriptOutput,
    out_dir: &Path,
    manifest_dir: &Path,
) {
    let mut portable = output.clone();
    portable.normalize(
        igel_common::fsutil::simplified(out_dir),
        igel_common::fsutil::simplified(manifest_dir),
    );
    let key = if names_project(&portable, out_dir, manifest_dir) {
        pinned_script_key(key, out_dir, manifest_dir)
    } else {
        key.to_string()
    };
    let _ = cas.put_dir_tree(&key, out_dir);
    let _ = cas.put_value(&key, &portable);
}

fn stored_script_output(
    cas: &CasStore,
    key: &str,
    out_dir: &Path,
    manifest_dir: &Path,
) -> Option<(String, BuildScriptOutput)> {
    [
        key.to_string(),
        pinned_script_key(key, out_dir, manifest_dir),
    ]
    .into_iter()
    .find_map(|key| {
        let mut output: BuildScriptOutput = cas.get_value(&key)?;
        if !output.is_portable() {
            return None;
        }
        output.relocate(
            igel_common::fsutil::simplified(out_dir),
            igel_common::fsutil::simplified(manifest_dir),
        );
        Some((key, output))
    })
}

fn restore_script_output(
    cas: &CasStore,
    key: &str,
    out_dir: &Path,
    manifest_dir: &Path,
) -> Option<BuildScriptOutput> {
    let (key, output) = stored_script_output(cas, key, out_dir, manifest_dir)?;
    cas.restore_dir_tree(&key, out_dir)
        .unwrap_or(false)
        .then_some(output)
}

/// `deps/`, or `examples/` for examples, as in Cargo.
pub fn out_dir_for(config: &IgelConfig, profile: &Profile, unit: &CompilationUnit) -> PathBuf {
    match (unit.crate_type, unit.kind) {
        (CrateType::Bin, TargetKind::Example) if !unit.check_only && !unit.harness => {
            bin_dir_for(config, profile, unit)
        }
        _ => config.deps_dir(profile.output_dir_name()),
    }
}

pub fn bin_dir_for(config: &IgelConfig, profile: &Profile, unit: &CompilationUnit) -> PathBuf {
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

pub fn target_dir_name(target: &str) -> String {
    match target.strip_suffix(".json") {
        Some(stem) => Path::new(stem)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| stem.to_string()),
        None => target.to_string(),
    }
}

fn extern_name(graph: &BuildGraph, node_id: NodeId, dep_id: NodeId) -> String {
    graph
        .node(node_id)
        .extern_aliases
        .get(&dep_id)
        .map(|s| s.replace('-', "_"))
        .unwrap_or_else(|| graph.node(dep_id).unit.rustc_crate_name())
}

fn extern_dependencies(graph: &BuildGraph, node_id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
    graph
        .node(node_id)
        .dependencies
        .iter()
        .copied()
        .filter(move |&dep| {
            graph.is_normal_edge(node_id, dep) && graph.node(dep).unit.crate_type != CrateType::Bin
        })
}

/// By file name: `igl check` puts a full build's `.rmeta` where its own would go.
fn compiled_against(
    graph: &BuildGraph,
    node_id: NodeId,
    contents: &HashMap<NodeId, Content>,
) -> Option<std::collections::BTreeMap<String, String>> {
    extern_dependencies(graph, node_id)
        .map(|dep| {
            let content = contents.get(&dep)?;
            Some((content.file.clone(), content.id.clone()))
        })
        .collect()
}

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
        if !graph.is_normal_edge(node_id, dep_id) {
            continue;
        }
        let dep_node = graph.node(dep_id);
        if dep_node.unit.crate_type == CrateType::Bin {
            continue;
        }
        let name = extern_name(graph, node_id, dep_id);
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

/// Cargo's friendly names in the profile directory, as hard links to the real files in `deps/`.
fn uplift_artifacts(
    config: &IgelConfig,
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
    let _ = igel_common::fsutil::replace_with_link(src, dest);
}

const METADATA_FRACTION: f64 = 0.4;

const DEFAULT_COMPILER_MEMORY: u64 = 256 << 20;

/// Part of every unit's key: how Igel runs the compiler (2: non-project crates compile in their package root).
const INVOCATION: &str = "\0invocation=2";

/// Compile times for critical-path weighting: measured (this project, then machine-wide), else estimated
/// from source size.
#[derive(Default)]
struct Timings {
    secs: HashMap<String, f64>,
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

    fn estimate(&self, unit: &CompilationUnit, profile: &Profile) -> f64 {
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

    fn estimate_compile(&self, unit: &CompilationUnit, profile: &Profile) -> f64 {
        if let Some(&secs) = self.secs.get(&Self::key(unit, profile)) {
            return secs;
        }
        let bytes = self
            .source_bytes
            .get(&unit.manifest_dir)
            .copied()
            .unwrap_or(0) as f64;
        // Fitted to a 182-crate build; only the ordering matters.
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

    fn record_memory(&mut self, unit: &CompilationUnit, profile: &Profile, bytes: u64) {
        self.secs
            .insert(format!("{} rss", Self::key(unit, profile)), bytes as f64);
        self.dirty = true;
    }

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
                    "target" | ".git" | ".igel" | ".cargo" | "node_modules" | "tests" | "benches"
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
            &igel_common::fsutil::simplified(&unit.manifest_dir)
                .display()
                .to_string(),
        )
        .env(
            "CARGO_MANIFEST_PATH",
            &igel_common::fsutil::simplified(&igel_common::manifest_file::manifest_in(
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

    // Cargo always sets all four version components, even empty.
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

pub fn with_profile_flags(
    mut builder: RustcCommandBuilder,
    profile: &Profile,
    target: &str,
    links_final_artifact: bool,
) -> RustcCommandBuilder {
    // Only spell out settings that differ from rustc's defaults for this opt-level.
    let opt0 = profile.opt_level == igel_common::OptLevel::O0;
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
    // `-C lto` only for units that link; bitcode is embedded exactly when some LTO is in play.
    match (profile.lto.as_flag(), links_final_artifact) {
        (Some(lto), true) => builder = builder.lto(lto),
        (Some(_), false) => {}
        (None, _) => {
            builder = builder.codegen_flag("embed-bitcode=no");
        }
    }
    if profile.panic != igel_common::PanicStrategy::Unwind {
        builder = builder.panic(profile.panic.as_str());
    }
    if let Some(cgu) = profile.codegen_units {
        builder = builder.codegen_units(cgu);
    }
    // A stripped proc-macro could no longer be loaded.
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

fn compiled_for(unit: &CompilationUnit) -> &str {
    unit.compiled_for()
}

/// `--remap-path-prefix` for `trim-paths`. Not part of the key.
fn trim_path_flags(unit: &CompilationUnit, config: &IgelConfig) -> Vec<String> {
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

pub fn build_rustc_command(
    unit: &CompilationUnit,
    profile: &Profile,
    config: &IgelConfig,
    extern_deps: &[(String, PathBuf)],
    build_output: Option<&BuildScriptOutput>,
    color: bool,
) -> RustcCommandBuilder {
    let eff = unit.effective_profile(profile);
    // Project crates compile from the project root with relative paths; others from their own package root.
    // rustc hashes its working directory into the crate's identity, so this must be stable.
    let unit_source_root = igel_common::fsutil::simplified(&unit.source_root);
    let config_project_root = igel_common::fsutil::simplified(&config.project_root);
    let (source, cwd) = match unit_source_root.strip_prefix(config_project_root) {
        Ok(rel) if unit.crate_id.source == CrateSource::Local && !rel.as_os_str().is_empty() => {
            (rel.to_path_buf(), config_project_root.to_path_buf())
        }
        _ => (
            unit.source_root.clone(),
            igel_common::fsutil::simplified(&unit.manifest_dir).to_path_buf(),
        ),
    };
    // Platform separators, as Cargo passes it.
    let source: PathBuf = source.components().collect();
    let mut builder = RustcCommandBuilder::new(source)
        .current_dir(cwd)
        .crate_name(&unit.rustc_crate_name())
        .edition(unit.edition.as_str())
        .metadata_hash(&unit.metadata_hash());
    builder = if unit.harness {
        builder.test_harness()
    } else {
        unit.all_crate_types()
            .fold(builder, |b, t| b.crate_type(t.rustc_crate_type()))
    };
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
    // Test harnesses and proc-macros need unwinding: they ignore `panic = "abort"`, as in Cargo.
    let unwinding;
    let eff = if eff.panic != igel_common::PanicStrategy::Unwind
        && (unit.harness || unit.crate_type == CrateType::ProcMacro)
    {
        unwinding = Profile {
            panic: igel_common::PanicStrategy::Unwind,
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
    // Proc-macros are linked too: shared libraries rustc loads, linked by the host's linker.
    let links = links_final_artifact || unit.crate_type == CrateType::ProcMacro;
    if let (Some(linker), true) = (&unit.ext.linker, links) {
        let (exe, env) = igel_common::linker::for_rustc(linker);
        builder = builder.linker(&exe.to_string_lossy());
        if let Some(flavor) = igel_common::linker::rustc_flavor(
            linker,
            compiled_for(unit),
            &igel_common::toolchain::global().host,
        ) {
            builder = builder.extra_flags(vec!["-C".into(), format!("linker-flavor={flavor}")]);
        }
        for (k, v) in env {
            builder = builder.env(k, &v);
        }
    }

    if !unit.lint_args.is_empty() {
        builder = builder.extra_flags(unit.lint_args.clone());
    }

    if unit.crate_id.source != CrateSource::Local {
        builder = builder.cap_lints("allow");
    }

    // Declare the cfgs Cargo declares (`docsrs`, `test`, feature values) for `unexpected_cfgs`.
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
    builder = builder.out_dir(&out_dir_for(config, profile, unit));

    // `igl check` skips codegen except for proc-macros, which must be loadable.
    if unit.check_only && unit.crate_type != CrateType::ProcMacro {
        builder = builder.emit(&["dep-info", "metadata"]);
    } else if unit.crate_type == CrateType::Bin || unit.harness {
        builder = builder.emit(&["dep-info", "link"]);
    } else {
        builder = builder.emit(&["dep-info", "metadata", "link"]);
        // rustc-runified writes the `.rmeta` before monomorphization, the rest when it's known.
        if toolchain::global()
            .runified
            .as_ref()
            .is_some_and(|d| d.supports("early-metadata"))
        {
            builder = builder.extra_flags(vec!["--runified=early-metadata".into()]);
        }
    }

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
        // rustc-runified keeps the output of derive macros known to be pure in that cache too.
        if toolchain::global()
            .runified
            .as_ref()
            .is_some_and(|d| d.supports("cache-derives"))
        {
            builder = builder.extra_flags(vec!["--runified=cache-derives".into()]);
        }
    }

    builder = builder.search_path("dependency", &config.deps_dir(profile_dir_name));
    for (crate_name, path) in extern_deps {
        builder = builder.extern_crate(crate_name, path);
    }

    for feat in &unit.features {
        builder = builder.feature(feat);
    }

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

    builder.json_output(color)
}

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

/// A unit's complete identity: the unit, its effective profile, the compiler and wrappers, flags, linker,
/// metadata, local input files and every dependency's key. Registry/git crates are never read from disk.
pub fn compute_unit_cas_key(
    unit: &CompilationUnit,
    profile: &Profile,
    dep_keys: &[(String, String)],
    source_cache: &mut igel_fingerprint::FingerprintCache,
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

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UnitIdentity {
    pub key: String,
    pub parts: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub files: std::collections::BTreeMap<String, String>,
}

struct KeyRecorder {
    key: igel_common::hash::HashBuilder,
    parts: Option<std::collections::BTreeMap<String, igel_common::hash::HashBuilder>>,
}

impl KeyRecorder {
    fn update(&mut self, part: &str, bytes: &[u8]) {
        self.key.update(bytes);
        let Some(parts) = &mut self.parts else { return };
        match parts.get_mut(part) {
            Some(hasher) => hasher.update(bytes),
            None => {
                let mut hasher = igel_common::hash::HashBuilder::new();
                hasher.update(bytes);
                parts.insert(part.to_string(), hasher);
            }
        }
    }
}

pub fn compute_unit_identity(
    unit: &CompilationUnit,
    profile: &Profile,
    dep_keys: &[(String, String)],
    source_cache: &mut igel_fingerprint::FingerprintCache,
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

pub fn compute_unit_identity_from(
    unit: &CompilationUnit,
    profile: &Profile,
    dep_keys: &[(String, String)],
    source_cache: &mut igel_fingerprint::FingerprintCache,
    read: Option<(&inputs::ReadRecord, Option<&str>)>,
) -> UnitIdentity {
    let sources = match read {
        Some((record, script)) => Sources::Read(record, script),
        None => Sources::Package,
    };
    identity_with(unit, profile, dep_keys, source_cache, sources, true)
}

pub fn compute_unit_key_from(
    unit: &CompilationUnit,
    profile: &Profile,
    dep_keys: &[(String, String)],
    source_cache: &mut igel_fingerprint::FingerprintCache,
    read: Option<(&inputs::ReadRecord, Option<&str>)>,
) -> String {
    let sources = match read {
        Some((record, script)) => Sources::Read(record, script),
        None => Sources::Package,
    };
    identity_with(unit, profile, dep_keys, source_cache, sources, false).key
}

/// Keyed by configuration and root file contents; a stale record only costs a miss.
pub fn read_record_key(
    unit: &CompilationUnit,
    source_cache: &mut igel_fingerprint::FingerprintCache,
) -> Option<String> {
    if unit.crate_id.source != CrateSource::Local {
        return None;
    }
    let root = igel_fingerprint::hasher::file_hash(&unit.source_root, source_cache)?;
    let mut hb = igel_common::hash::HashBuilder::new();
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

enum Sources<'a> {
    None,
    Package,
    Read(&'a inputs::ReadRecord, Option<&'a str>),
}

pub fn compute_script_base(
    unit: &CompilationUnit,
    profile: &Profile,
    build_dep_keys: &[(String, String)],
    dep_metadata: &[(String, String, String)],
    source_cache: &mut igel_fingerprint::FingerprintCache,
) -> String {
    let mut deps: Vec<(String, String)> = build_dep_keys.to_vec();
    for (links, k, v) in dep_metadata {
        deps.push((format!("dep-metadata:{links}:{k}"), v.clone()));
    }
    let identity = identity_with(unit, profile, &deps, source_cache, Sources::None, false);
    let mut hb = igel_common::hash::HashBuilder::new();
    hb.update(b"build-script-result\0");
    hb.update(identity.key.as_bytes());
    if let Some(script) = &unit.build_script {
        let rel = script.strip_prefix(&unit.manifest_dir).unwrap_or(script);
        hb.update(rel.to_string_lossy().as_bytes());
        // Two packages agreeing on everything else must not share a record of what the other's script read.
        hb.update(b"\0script=");
        hb.update(inputs::hash_path(script).as_bytes());
    }
    hb.finish_blake3()
}

fn identity_with(
    unit: &CompilationUnit,
    profile: &Profile,
    dep_keys: &[(String, String)],
    source_cache: &mut igel_fingerprint::FingerprintCache,
    sources: Sources,
    with_parts: bool,
) -> UnitIdentity {
    let mut hb = KeyRecorder {
        key: igel_common::hash::HashBuilder::new(),
        parts: with_parts.then(Default::default),
    };
    const UNIT: &str = "unit (features, crate type, edition, target)";
    hb.update(UNIT, unit.metadata_hash().as_bytes());
    let eff = unit.effective_profile(profile);
    hb.update("profile", eff.codegen_fingerprint().as_bytes());
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
    hb.update("toolchain", INVOCATION.as_bytes());
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
    if unit.crate_id.source == CrateSource::Local {
        hb.update("lints", b"\0check-cfg=docsrs,test,features");
    }
    for f in &unit.ext.declared_features {
        hb.update("declared features", b"\0declared=");
        hb.update("declared features", f.as_bytes());
    }

    let mut files = std::collections::BTreeMap::new();
    if unit.crate_id.source == CrateSource::Local && !matches!(sources, Sources::None) {
        let rel_root = unit
            .source_root
            .strip_prefix(&unit.manifest_dir)
            .unwrap_or(&unit.source_root);
        hb.update("crate root", rel_root.to_string_lossy().as_bytes());
        if let Sources::Read(record, script) = sources {
            if let Some(script) = script {
                hb.update("build script result", b"script=");
                hb.update("build script result", script.as_bytes());
            }
            hb.update("sources", b"read\0");
            for (name, path) in record.paths(&unit.manifest_dir) {
                let hash = igel_fingerprint::hasher::file_hash(&path, source_cache)
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
                igel_fingerprint::hasher::compute_package_hash(&unit.manifest_dir, source_cache)
                    .unwrap_or_default();
            hb.update("sources", package_hash.as_bytes());
            if with_parts {
                files.extend(source_cache.package_file_hashes(&unit.manifest_dir));
            }
        }
        for outside in [
            unit.build_script.as_deref(),
            Some(unit.source_root.as_path()),
        ]
        .into_iter()
        .flatten()
        .filter(|p| !p.starts_with(&unit.manifest_dir))
        {
            if let Ok(h) = igel_common::hash::xxhash3_file(outside) {
                hb.update("sources outside the package", &h.to_le_bytes());
            }
        }
    }

    // Sorted: `dep_keys` order comes from a `HashMap`.
    let mut sorted_dep_keys: Vec<&(String, String)> = dep_keys.iter().collect();
    sorted_dep_keys.sort();
    for (dep_name, dep_key) in sorted_dep_keys {
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

pub fn load_identities(fingerprint_dir: &Path) -> HashMap<String, UnitIdentity> {
    std::fs::read_to_string(fingerprint_dir.join("identities.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use igel_common::{CrateId, Edition};

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

    #[test]
    fn executables_and_tests_link_late_libraries_and_macros_never() {
        let dir = tempfile::tempdir().unwrap();
        let of = |crate_type, harness, check_only| CompilationUnit {
            crate_type,
            harness,
            check_only,
            ..unit("u", registry(), dir.path())
        };
        assert!(links_late(&of(CrateType::Bin, false, false)));
        assert!(
            links_late(&of(CrateType::Lib, true, false)),
            "a test harness"
        );
        assert!(
            !links_late(&of(CrateType::Bin, false, true)),
            "checking links nothing"
        );
        assert!(!links_late(&of(CrateType::Lib, false, false)));
        assert!(!links_late(&of(CrateType::ProcMacro, false, false)));
        assert!(!links_late(&of(CrateType::Cdylib, false, false)));
        assert!(!links_late(&CompilationUnit {
            is_build_script: true,
            ..of(CrateType::Bin, false, false)
        }));
    }

    fn registry() -> CrateSource {
        CrateSource::Registry {
            index: "x".into(),
            checksum: Some("c".into()),
        }
    }

    fn key(u: &CompilationUnit, p: &Profile) -> String {
        compute_unit_cas_key(u, p, &[], &mut igel_fingerprint::FingerprintCache::new())
    }

    #[test]
    fn registry_keys_never_touch_the_filesystem() {
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
        fn short() -> Option<igel_common::sysinfo::Memory> {
            Some(igel_common::sysinfo::Memory {
                total: 8 << 30,
                available: 100 << 20,
            })
        }
        let config = IgelConfig::new(dir.path().join("project"));
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
        fn roomy() -> Option<igel_common::sysinfo::Memory> {
            Some(igel_common::sysinfo::Memory {
                total: 8 << 30,
                available: 3 << 30,
            })
        }
        let config = IgelConfig::new(dir.path().join("project"));
        let first = BuildExecutor::new(config.clone(), Profile::dev(), 4)
            .with_memory_probe(roomy)
            .execute(&graph)
            .unwrap();
        assert!(first.success);
        let path = config.fingerprint_dir.join("timings.json");
        let mut timings: HashMap<String, f64> =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
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
        use igel_common::settings::FrontendThreads::*;
        assert_eq!(frontend_threads(Auto, 10, 10, 0), None);
        assert_eq!(frontend_threads(Auto, 10, 4, 6), None);
        assert_eq!(frontend_threads(Auto, 10, 1, 0), Some(8));
        assert_eq!(frontend_threads(Auto, 4, 2, 0), Some(3));
        assert_eq!(frontend_threads(Off, 16, 1, 0), None);
        assert_eq!(frontend_threads(Fixed(4), 16, 16, 9), Some(4));
    }

    #[test]
    fn only_the_head_of_the_longest_remaining_chain_is_critical() {
        // 0 (script, 12 s) → 1 (big library, 25 s) → 2; 3 is a shorter chain beside them.
        let weights: HashMap<NodeId, f64> = [(0, 40.0), (1, 28.0), (2, 3.0), (3, 20.0)]
            .into_iter()
            .map(|(id, w)| (NodeId(id), w))
            .collect();
        let mut completed = HashSet::new();
        assert!(on_critical_path(&weights, &completed, NodeId(0)));
        assert!(!on_critical_path(&weights, &completed, NodeId(3)));
        completed.insert(NodeId(0));
        assert!(on_critical_path(&weights, &completed, NodeId(1)));
        assert!(!on_critical_path(&weights, &completed, NodeId(3)));
        completed.extend([NodeId(1), NodeId(2)]);
        assert!(on_critical_path(&weights, &completed, NodeId(3)));
        assert!(
            !on_critical_path(&weights, &completed, NodeId(9)),
            "unknown"
        );
    }

    #[test]
    fn keys_depend_on_every_codegen_setting() {
        let u = unit("serde", registry(), Path::new("/x"));
        let base = key(&u, &Profile::release());
        let mut lto = Profile::release();
        lto.lto = igel_common::LtoMode::Fat;
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
        let mut cache = igel_fingerprint::FingerprintCache::new();
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
        p.lto = igel_common::LtoMode::Thin;
        p.codegen_units = Some(1);
        p.strip = Some("symbols".into());
        p.panic = igel_common::PanicStrategy::Abort;
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
        assert!(
            !args.iter().any(|a| a.starts_with("debug-assertions")),
            "{args:?}"
        );

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
        let parsed: Vec<String> = "-C target-cpu=native  -D warnings"
            .split_whitespace()
            .map(str::to_string)
            .collect();
        assert_eq!(parsed, ["-C", "target-cpu=native", "-D", "warnings"]);
    }
}
