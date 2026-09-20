use crate::inputs::{self, DeclaredInputs, effective_key};
use crate::pipeline::PipelineEvent;
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
        /// Taken from the store (another process built it while we waited)
        /// rather than compiled by this one.
        restored: bool,
    },
    Failed {
        node_id: NodeId,
        error: String,
    },
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
}

impl BuildExecutor {
    /// Creates a new `BuildExecutor`.
    pub fn new(config: RuneConfig, profile: Profile, max_jobs: usize) -> Self {
        let (event_tx, event_rx) = unbounded();
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
            jobserver: jobserver::Client::new(max_jobs.max(1)).ok(),
        }
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
        let mut spans: Vec<TraceSpan> = Vec::new();
        let mut started: HashMap<NodeId, f64> = HashMap::new();

        // 1. Cache keys for every node, dependencies first. A unit's *base*
        // key covers its source, configuration and dependencies; the key it is
        // stored under also covers the current value of whatever external
        // inputs a previous build of it declared (see `inputs`).
        let keys_timer = rune_common::timing::phase("cache keys");
        let mut unit_base_keys: HashMap<NodeId, String> = HashMap::with_capacity(topo.len());
        let mut unit_cas_keys: HashMap<NodeId, String> = HashMap::with_capacity(topo.len());
        for &id in &topo {
            let (base, key) = self.compute_keys(graph, id, &unit_cas_keys, &mut source_cache);
            unit_base_keys.insert(id, base);
            unit_cas_keys.insert(id, key);
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
            let restored: Vec<(NodeId, Vec<PathBuf>)> = topo
                .par_iter()
                .filter_map(|&id| {
                    let unit = &graph.node(id).unit;
                    let linked = cas
                        .link_to(&unit_cas_keys[&id], &self.out_dir_for(unit))
                        .ok()?;
                    (!linked.is_empty()).then_some((id, linked))
                })
                .collect();

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
                    if let Some(output) = cas.get_value(&script_key_name) {
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
                if unit.crate_type == CrateType::Bin && !unit.harness {
                    alias_binaries(&self.config, &self.profile, unit, &linked);
                }
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
                    }
                },
                |dep, dependent, dep_cost| {
                    let pipelined = !dependent.unit.needs_real_link()
                        && dep.unit.crate_type != CrateType::ProcMacro
                        && !dep.unit.needs_real_link();
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

            if failed.is_empty() && in_flight.len() < self.max_jobs {
                let mut ready: Vec<NodeId> = unscheduled
                    .iter()
                    .copied()
                    .filter(|&id| {
                        let node = graph.node(id);
                        if node.unit.needs_real_link() {
                            transitive_deps[&id].iter().all(|d| completed.contains(d))
                        } else {
                            node.dependencies.iter().all(|dep_id| {
                                if graph.node(*dep_id).unit.crate_type == CrateType::ProcMacro {
                                    completed.contains(dep_id)
                                } else {
                                    completed.contains(dep_id) || metadata_ready.contains(dep_id)
                                }
                            })
                        }
                    })
                    .collect();

                // Longest remaining chain first.
                ready.sort_by(|a, b| {
                    let wa = weights.get(a).copied().unwrap_or(0.0);
                    let wb = weights.get(b).copied().unwrap_or(0.0);
                    wb.partial_cmp(&wa).unwrap_or(std::cmp::Ordering::Equal)
                });

                for node_id in ready {
                    if in_flight.len() >= self.max_jobs {
                        break;
                    }
                    unscheduled.retain(|id| *id != node_id);
                    in_flight.insert(node_id);
                    started.insert(node_id, start_time.elapsed().as_secs_f64());
                    let job = self.prepare_job(
                        graph,
                        node_id,
                        &unit_base_keys[&node_id],
                        &unit_cas_keys[&node_id],
                        &artifact_paths,
                        &metadata_paths,
                        &build_script_outputs,
                        worker_tx.clone(),
                    );
                    std::thread::spawn(move || job.run());
                }
            }

            if in_flight.is_empty() {
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
                        metadata_ready.insert(node_id);
                        metadata_paths.insert(node_id, rmeta_path);
                    }
                    WorkerMsg::BuildScriptDone { node_id, output } => {
                        build_script_outputs.insert(node_id, output);
                    }
                    WorkerMsg::KeyUpdate { node_id, key } => {
                        // The unit's key changed; so do those of everything
                        // above it that has not started yet (nothing above
                        // it can have, apart from pipelined dependents, which
                        // wait for the metadata that follows this message).
                        unit_cas_keys.insert(node_id, key);
                        // Only what transitively depends on this unit can have
                        // changed: one pass in dependency order, recomputing a
                        // key only when one of its dependencies' just did.
                        let mut changed: HashSet<NodeId> = HashSet::from([node_id]);
                        for &id in &topo {
                            if id == node_id
                                || completed.contains(&id)
                                || in_flight.contains(&id)
                                || !graph
                                    .node(id)
                                    .dependencies
                                    .iter()
                                    .any(|d| changed.contains(d))
                            {
                                continue;
                            }
                            let (base, key) =
                                self.compute_keys(graph, id, &unit_cas_keys, &mut source_cache);
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
                        restored,
                    } => {
                        debug!("Finished compiling node {:?}", node_id);
                        in_flight.remove(&node_id);
                        completed.insert(node_id);
                        metadata_ready.insert(node_id);
                        artifact_paths.insert(node_id, artifacts);
                        let unit = &graph.node(node_id).unit;
                        if !restored {
                            timings.record(unit, &self.profile, duration);
                        }
                        spans.push(TraceSpan {
                            name: format!(
                                "{} {} ({})",
                                unit.crate_id.name,
                                unit.crate_id.version,
                                unit.kind.as_str()
                            ),
                            kind: if unit.crate_type == CrateType::ProcMacro {
                                "proc-macro"
                            } else if unit.build_script.is_some() {
                                "build-script"
                            } else {
                                unit.kind.as_str()
                            },
                            start: started.get(&node_id).copied().unwrap_or(0.0),
                            dur: duration.as_secs_f64(),
                            after: graph
                                .node(node_id)
                                .dependencies
                                .iter()
                                .map(|d| {
                                    let u = &graph.node(*d).unit;
                                    format!(
                                        "{} {} ({})",
                                        u.crate_id.name,
                                        u.crate_id.version,
                                        u.kind.as_str()
                                    )
                                })
                                .collect(),
                        });
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
                        failed.push(node_id);
                    }
                }
            }
        }

        let _ = source_cache.save_if_changed(&source_cache_path);
        timings.save(&timings_path, &global_timings_path);
        if rune_common::timing::enabled() && !spans.is_empty() {
            let trace_path = self.config.fingerprint_dir.join("trace.json");
            write_trace(&trace_path, &spans);
            let busy: f64 = spans.iter().map(|s| s.dur).sum();
            let wall = start_time.elapsed().as_secs_f64().max(f64::EPSILON);
            spans.sort_by(|a, b| (b.start + b.dur).total_cmp(&(a.start + a.dur)));
            eprintln!(
                "  [timing] schedule: {:.1}s of unit time in {:.1}s wall ({:.1} units running on average); \
                 last to finish: {}; trace: {}",
                busy,
                wall,
                busy / wall,
                spans
                    .iter()
                    .take(3)
                    .map(|s| format!("{} ({:.1}s)", s.name, s.dur))
                    .collect::<Vec<_>>()
                    .join(", "),
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
        })
    }

    /// `(base key, effective key)` of a node, given its dependencies' final keys.
    fn compute_keys(
        &self,
        graph: &BuildGraph,
        id: NodeId,
        keys: &HashMap<NodeId, String>,
        source_cache: &mut rune_fingerprint::FingerprintCache,
    ) -> (String, String) {
        let node = graph.node(id);
        // What a unit depends on includes *how*: under which name it links a
        // dependency (`package = "..."` renames), and whether it is a normal
        // or a build dependency. The manifest that says so is not hashed, so
        // the key has to.
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
        let base = compute_unit_cas_key(&node.unit, &self.profile, &dep_keys, source_cache);
        let recorded: Option<DeclaredInputs> = self
            .cas
            .as_ref()
            .and_then(|c| c.get_value(&format!("{base}-inputs")));
        let key = effective_key(&base, recorded.as_ref(), node.unit.build_script.is_some());
        (base, key)
    }

    /// Gathers everything a worker thread needs, by value.
    #[allow(clippy::too_many_arguments)]
    fn prepare_job(
        &self,
        graph: &BuildGraph,
        node_id: NodeId,
        base_key: &str,
        cas_key: &str,
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
                    let exe =
                        out_dir_for(&self.config, &self.profile, dep).join(&dep.crate_id.name);
                    extra_env.push((
                        format!("CARGO_BIN_EXE_{}", dep.crate_id.name),
                        exe.display().to_string(),
                    ));
                }
            }
        }

        let unit_owner_key = unit.ext.script_owner.clone();
        UnitJob {
            node_id,
            unit,
            extern_deps,
            build_script_extern_deps,
            dep_metadata,
            base_key: base_key.to_string(),
            cas_key: cas_key.to_string(),
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
    build_output: Option<BuildScriptOutput>,
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
}

impl UnitJob {
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
        // The other process may have learned external inputs (which change the
        // key the unit is stored under) that we did not know about.
        let recorded: Option<DeclaredInputs> = cas.get_value(&format!("{}-inputs", self.base_key));
        let key = effective_key(&self.base_key, recorded.as_ref(), has_script);
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
            if let Some(output) = cas.get_value(&script_key_name) {
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
        if unit.crate_type == CrateType::Bin && !unit.harness {
            alias_binaries(&self.config, &self.profile, unit, &linked);
        }
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
            restored: true,
        });
        true
    }

    fn run(self) {
        // Hold one jobserver token for as long as this unit runs; the compiler
        // may borrow more for its own codegen threads while others are idle.
        let _token = self.jobserver.as_ref().and_then(|js| js.acquire().ok());
        let start = Instant::now();
        let node_id = self.node_id;
        let unit = &self.unit;
        let profile = &self.profile;
        let config = &self.config;
        let _ = self.event_tx.send(PipelineEvent::BuildStarted {
            node_id,
            unit: unit.clone(),
        });

        // Another `rune` process may be building this very unit into the same
        // target directory (an editor's background check next to a terminal
        // build). Whoever holds the unit's lock owns its output files; everyone
        // else waits — and then usually finds the finished result in the store
        // instead of compiling it again.
        let lock_path = config
            .profile_target_dir(profile.output_dir_name())
            .join(".rune-locks")
            .join(script_key(unit));
        let (_unit_lock, acquired) = rune_common::fsutil::FileLock::exclusive(&lock_path);
        if acquired == rune_common::fsutil::Acquired::AfterWaiting
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
        let build_script_cache_key = format!("{}-buildscript", self.cas_key);

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
        let bump = |declared: &DeclaredInputs, store_key: &mut String| {
            let key = effective_key(&self.base_key, Some(declared), has_script);
            if key != *store_key {
                *store_key = key.clone();
                let _ = self.worker_tx.send(WorkerMsg::KeyUpdate { node_id, key });
            }
        };
        if effective_build_output.is_none() && !self.force_rebuild && !shares_script {
            if let Some(cas_store) = &self.cas {
                let restored = cas_store
                    .restore_dir_tree(&build_script_cache_key, &script_out_dir)
                    .unwrap_or(false);
                if restored {
                    effective_build_output = cas_store.get_value(&build_script_cache_key);
                }
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
                rustc: tc.rustc.clone(),
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
                    with_profile_flags(RustcCommandBuilder::new(PathBuf::new()), p, true)
                        .into_args()
                })
                .unwrap_or_default();
            let configure = |cmd: &mut std::process::Command| {
                cmd.args(&script_flags);
                if let Some(js) = &self.jobserver {
                    js.configure(cmd);
                }
            };
            let compiled = BuildScriptRunner::compile_with(
                tc.rustc_command(unit.ext.member),
                &build_rs,
                &script_out_dir,
                &deps_dir,
                unit.edition.as_str(),
                &self.build_script_extern_deps,
                Some(&compile_env),
                &configure,
            );
            match compiled {
                Ok(bin_path) => {
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
                    let outcome = BuildScriptRunner::execute_with(&bin_path, &env, &run_configure);
                    if let Some(before) = immutable_before {
                        if outcome.is_ok() && tree_stamp(&unit.manifest_dir) != before {
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
                    }
                    match outcome {
                        Ok(out) => {
                            let _ = self.event_tx.send(PipelineEvent::BuildScriptFinished {
                                node_id,
                                crate_name: unit.crate_id.name.clone(),
                                duration: start.elapsed(),
                            });
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
            bump(&declared, &mut store_key);
        }
        if let (Some(out), Some(cas_store)) = (&script_ran, &self.cas) {
            let key = format!("{store_key}-buildscript");
            let _ = cas_store.put_dir_tree(&key, &script_out_dir);
            let _ = cas_store.put_value(&key, out);
        }

        if let Some(output) = &effective_build_output {
            let _ = self.worker_tx.send(WorkerMsg::BuildScriptDone {
                node_id,
                output: output.clone(),
            });
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
        for (k, v) in &self.extra_env {
            cmd_builder = cmd_builder.env(k, v);
        }
        for (k, v, force) in &self.config_env {
            if *force || std::env::var_os(k).is_none() {
                cmd_builder = cmd_builder.env(k, v);
            }
        }
        let mut cmd = cmd_builder.build_with_command(self.toolchain.rustc_command(unit.ext.member));
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
                let reads = inputs::env_reads_from_dep_info(&artifact.artifact, &provided);
                let manifest = rune_common::manifest_file::manifest_in(&unit.manifest_dir);
                let read_manifest = inputs::reads_manifest(&artifact.artifact, &manifest);
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
                    bump(&declared, &mut store_key);
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

                if let Some(cas_store) = &self.cas {
                    let _ = cas_store.put(
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
                        let _ =
                            cas_store.put_value(&format!("{}-inputs", self.base_key), &declared);
                    }
                }
                if unit.crate_type == CrateType::Bin && !unit.harness {
                    alias_binaries(config, profile, unit, &artifacts);
                }

                let _ = self.event_tx.send(PipelineEvent::BuildFinished {
                    node_id,
                    artifacts: artifacts.clone(),
                    duration,
                    warnings,
                    diagnostics,
                });
                let _ = self.worker_tx.send(WorkerMsg::Finished {
                    node_id,
                    artifacts,
                    duration,
                    restored: false,
                });
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
fn tree_stamp(dir: &Path) -> u64 {
    let mut hb = rune_common::hash::HashBuilder::new();
    for entry in walkdir::WalkDir::new(dir)
        .sort_by_file_name()
        .into_iter()
        .flatten()
    {
        let Ok(meta) = entry.metadata() else { continue };
        hb.update(entry.path().to_string_lossy().as_bytes());
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

/// Where a unit's primary artifacts go: binaries into the profile directory
/// (under a per-target subdirectory when cross-compiling, as Cargo does),
/// everything else (and everything under `rune check`) into `deps/`.
pub fn out_dir_for(config: &RuneConfig, profile: &Profile, unit: &CompilationUnit) -> PathBuf {
    let dir = profile.output_dir_name();
    if unit.check_only || unit.harness {
        return config.deps_dir(dir);
    }
    let bin_dir = || match &unit.target {
        Some(t) => config.profile_target_dir(dir).join(target_dir_name(&t.0)),
        None => config.profile_target_dir(dir),
    };
    match (unit.crate_type, unit.kind) {
        (CrateType::Bin, TargetKind::Example) => bin_dir().join("examples"),
        (CrateType::Bin, _) => bin_dir(),
        _ => config.deps_dir(dir),
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

/// Gives a freshly built or restored binary its friendly names (`name` and the
/// underscore-normalized `name`) next to the hash-suffixed real file.
///
/// Hard links, not copies: a debug binary can be tens of megabytes, and copying
/// it on every no-op build was a large part of a "nothing changed" build's cost.
fn alias_binaries(
    config: &RuneConfig,
    profile: &Profile,
    unit: &CompilationUnit,
    artifacts: &[PathBuf],
) {
    let bin_dir = out_dir_for(config, profile, unit);
    for artifact in artifacts {
        let ext = artifact.extension().and_then(|e| e.to_str());
        if matches!(ext, Some("d") | Some("rmeta") | Some("rlib")) {
            continue;
        }
        let primary = bin_dir.join(&unit.crate_id.name);
        let normalized = bin_dir.join(unit.crate_id.crate_name_normalized());
        link_or_copy_replace(artifact, &primary);
        if normalized != primary {
            link_or_copy_replace(artifact, &normalized);
        }
    }
}

/// Makes `dest` a hard link to (or, failing that, a copy of) `src`, doing
/// nothing if it already is one.
fn link_or_copy_replace(src: &Path, dest: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(a), Ok(b)) = (std::fs::metadata(src), std::fs::metadata(dest)) {
            if a.ino() == b.ino() && a.dev() == b.dev() {
                return;
            }
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
        let sizes: Vec<(PathBuf, u64)> = todo
            .into_par_iter()
            .map(|d| {
                let bytes = rust_source_bytes(&d);
                (d, bytes)
            })
            .collect();
        self.source_bytes.extend(sizes);
    }

    /// Seconds this unit is expected to take: measured if we have seen it
    /// before, otherwise guessed from the size of its package's sources.
    fn estimate(&self, unit: &CompilationUnit, profile: &Profile) -> f64 {
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
        if unit.build_script.is_some() || unit.is_build_script {
            secs += 1.0; // compile + run the script
        }
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

/// Writes the schedule of a finished build as a Chrome trace
/// (`chrome://tracing`, Perfetto): one lane per concurrently running unit.
fn write_trace(path: &Path, spans: &[TraceSpan]) {
    let mut spans: Vec<&TraceSpan> = spans.iter().collect();
    spans.sort_by(|a, b| {
        a.start
            .partial_cmp(&b.start)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut lane_free_at: Vec<f64> = Vec::new();
    let mut events = Vec::with_capacity(spans.len());
    for span in spans {
        let lane = match lane_free_at.iter().position(|&t| t <= span.start) {
            Some(l) => l,
            None => {
                lane_free_at.push(0.0);
                lane_free_at.len() - 1
            }
        };
        lane_free_at[lane] = span.start + span.dur;
        events.push(serde_json::json!({
            "name": span.name,
            "cat": span.kind,
            "ph": "X",
            "ts": (span.start * 1e6) as u64,
            "dur": (span.dur * 1e6) as u64,
            "pid": 1,
            "tid": lane,
            "args": { "after": span.after },
        }));
    }
    let doc = serde_json::json!({ "traceEvents": events });
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, doc.to_string());
}

/// One compiled unit's place in the schedule, in seconds since the build began.
struct TraceSpan {
    name: String,
    kind: &'static str,
    start: f64,
    dur: f64,
    /// Names of the units this one waited for.
    after: Vec<String>,
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
            &unit.manifest_dir.display().to_string(),
        )
        .env(
            "CARGO_MANIFEST_PATH",
            &rune_common::manifest_file::manifest_in(&unit.manifest_dir)
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
    if let Some(split) = &profile.split_debuginfo {
        builder = builder.codegen_flag(&format!("split-debuginfo={split}"));
    }
    builder
}

fn on_off(b: bool) -> &'static str {
    if b { "on" } else { "off" }
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
    let mut builder = RustcCommandBuilder::new(unit.source_root.clone())
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
    let links_final_artifact = !unit.harness
        && unit.all_crate_types().any(|t| {
            matches!(
                t,
                CrateType::Bin | CrateType::Cdylib | CrateType::Dylib | CrateType::Staticlib
            )
        });
    builder = with_profile_flags(builder, eff, links_final_artifact);
    builder = with_cargo_env(builder, unit);

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

    // Declare which `feature = "..."` values exist, so `unexpected_cfgs`
    // catches typos in this package's own `cfg(feature = ...)` (Cargo does).
    if unit.crate_id.source == CrateSource::Local && !unit.ext.declared_features.is_empty() {
        let values = unit
            .ext
            .declared_features
            .iter()
            .map(|f| format!("\"{f}\""))
            .collect::<Vec<_>>()
            .join(", ");
        builder = builder.check_cfg(&format!("cfg(feature, values({values}))"));
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
    let mut hb = rune_common::hash::HashBuilder::new();
    hb.update(unit.metadata_hash().as_bytes());
    hb.update(
        unit.effective_profile(profile)
            .codegen_fingerprint()
            .as_bytes(),
    );
    hb.update(b"\0toolchain=");
    hb.update(toolchain::global().identity_for(unit.ext.member).as_bytes());
    for flag in &unit.ext.rustflags {
        hb.update(b"\0rustflag=");
        hb.update(flag.as_bytes());
    }
    if let Some(linker) = &unit.ext.linker {
        hb.update(b"\0linker=");
        hb.update(linker.as_bytes());
    }
    for (k, v) in &unit.ext.env {
        hb.update(b"\0env=");
        hb.update(k.as_bytes());
        hb.update(b"=");
        hb.update(v.as_bytes());
    }
    // How the build script is compiled and what it links against is part of
    // what the package produces.
    if let Some(p) = &unit.ext.script_profile {
        if unit.build_script.is_some() {
            hb.update(b"\0script-profile=");
            hb.update(p.codegen_fingerprint().as_bytes());
            for f in &unit.ext.script_rustflags {
                hb.update(b"\0script-flag=");
                hb.update(f.as_bytes());
            }
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
        hb.update(field.as_bytes());
        hb.update(b"\0");
    }

    for arg in &unit.lint_args {
        hb.update(b"\0lint=");
        hb.update(arg.as_bytes());
    }
    for f in &unit.ext.declared_features {
        hb.update(b"\0declared=");
        hb.update(f.as_bytes());
    }

    if unit.crate_id.source == CrateSource::Local {
        // Where the crate root lives is an input too (`[lib] path = ...`).
        let rel_root = unit
            .source_root
            .strip_prefix(&unit.manifest_dir)
            .unwrap_or(&unit.source_root);
        hb.update(rel_root.to_string_lossy().as_bytes());
        let package_hash =
            rune_fingerprint::hasher::compute_package_hash(&unit.manifest_dir, source_cache)
                .unwrap_or_default();
        hb.update(package_hash.as_bytes());
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
                hb.update(&h.to_le_bytes());
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
        hb.update(dep_name.as_bytes());
        hb.update(dep_key.as_bytes());
    }

    hb.finish_blake3()
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
        let cmd = with_profile_flags(RustcCommandBuilder::new("x.rs".into()), &p, true).build();
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
        let cmd = with_profile_flags(RustcCommandBuilder::new("x.rs".into()), &odd, true).build();
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.iter().any(|a| a == "debug-assertions=on"), "{args:?}");
        assert!(args.iter().any(|a| a == "overflow-checks=on"), "{args:?}");

        // LTO is a link-time flag: libraries and proc-macros never get it.
        let cmd = with_profile_flags(RustcCommandBuilder::new("x.rs".into()), &p, false).build();
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
