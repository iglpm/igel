//! A resolved command invocation: workspace, profile, flags, and the machinery
//! to drive the build executor.

use anyhow::{Result, bail};
use colored::Colorize;
use rune_cas::CasStore;
use rune_cas::gc::{GcConfig, run_gc};
use rune_common::settings::{Origin, Value};
use rune_common::{Profile, RuneConfig};
use rune_graph::BuildGraph;
use rune_manifest::resolve_profile;
use rune_scheduler::executor::BuildResult;
use rune_scheduler::{BuildExecutor, BuildProgress, PipelineEvent};

use crate::args::CommonArgs;
use crate::graph::FeatureArgs;
use crate::ui;
use crate::workspace::Workspace;

/// Everything a build-like command needs, derived from flags, settings, and
/// the workspace on disk.
pub struct Session {
    pub ws: Workspace,
    pub profile: Profile,
    pub jobs: usize,
    pub offline: bool,
    pub locked: bool,
    pub target: Option<String>,
    pub features: FeatureArgs,
    pub selected: Vec<usize>,
    pub verbose: bool,
    pub quiet: bool,
    /// Print the compiler's warnings as units finish (commands that report
    /// on warnings themselves, like `rune unused`, turn this off).
    pub show_warnings: bool,
    /// `--message-format json`: events go to stdout as JSON lines.
    pub json: bool,
    pub force_rebuild: bool,
    pub ignore_rust_version: bool,
    /// Directories of native libraries that build scripts produced inside the
    /// build directory (`cargo::rustc-link-search=…/out`): a program run by
    /// `rune run`/`rune test` finds its dynamic libraries there, as with Cargo.
    pub native_dirs: std::sync::Mutex<Vec<std::path::PathBuf>>,
    /// Extra environment for every program `rune run`/`rune test` starts.
    pub run_env: Vec<(String, String)>,
    /// `build.sandbox`, checked to work here.
    pub sandbox: Option<rune_scheduler::executor::SandboxPolicy>,
    /// Seconds of work that follow a unit's build (see
    /// `BuildExecutor::with_extra_costs`).
    pub extra_costs: std::collections::HashMap<rune_graph::NodeId, f64>,
}

impl Session {
    /// `default_profile` is what the command builds with absent `--release`
    /// and `--profile` (`dev`, `test`, or `bench`).
    pub fn new(common: &CommonArgs, default_profile: &str) -> Result<Self> {
        if common.timings {
            rune_common::timing::enable();
        }
        crate::messages::validate(&common.message_format).map_err(|e| anyhow::anyhow!(e))?;
        let json = crate::messages::wants_json(&common.message_format);
        crate::ui::set_quiet(common.quiet);
        let mut ws = Workspace::load(common.manifest_path.as_deref())?;
        if let Some(why) = rune_common::toolchain::global().unusable_reason() {
            bail!(why);
        }
        if ws.config.settings.low_priority() {
            rune_common::sysinfo::lower_priority();
        }
        let sandbox = if ws.config.settings.sandbox() {
            let helper =
                anyhow::Context::context(std::env::current_exe(), "locating the rune executable")?;
            let sandbox =
                rune_common::sandbox::Sandbox::new(helper, ws.config.target_dir.join("tmp"));
            sandbox.probe().map_err(|why| {
                anyhow::anyhow!(
                    "build.sandbox is on, but build-time code cannot be confined here: {why}"
                )
            })?;
            Some(rune_scheduler::executor::SandboxPolicy {
                sandbox,
                network: ws.config.settings.sandbox_network().into_iter().collect(),
            })
        } else {
            None
        };

        if let Some(dir) = target_dir_override(common)? {
            ws.config.target_dir = dir;
        }

        let offline = common.offline || common.frozen || ws.config.settings.offline();
        let locked = common.locked || common.frozen;

        let name = if common.release {
            "release"
        } else {
            common.profile.as_deref().unwrap_or(default_profile)
        };
        let profile = profile_by_name(&ws, name)?;

        let jobs = common.jobs.unwrap_or(ws.config.jobs).max(1);

        // `build.release-rebuild-all` applies to release-like profiles; the
        // flag overrides the setting either way for a single invocation.
        let force_rebuild = match common.rebuild_all {
            Some(v) => v,
            None => profile.is_release() && ws.config.settings.release_rebuild_all(),
        };
        if common.rebuild_all.is_some() {
            ws.config.settings.override_value(
                "build.release-rebuild-all",
                Value::Bool(force_rebuild),
                Origin::Env("--rebuild-all".into()),
            );
        }

        let default_target = ws
            .cargo_config
            .string(&["build", "target"])
            .or_else(|| {
                ws.cargo_config
                    .string_list(&["build", "target"])
                    .and_then(|l| l.into_iter().next())
            })
            .filter(|t| !t.is_empty());
        let target = common.target.clone().or(default_target);
        if let Some(t) = &target {
            check_target_installed(&ws, t)?;
        }
        let selected = ws.select(&common.package, common.workspace, &common.exclude)?;
        let features = FeatureArgs::parse(
            &common.features,
            common.all_features,
            common.no_default_features,
        );

        Ok(Self {
            ws,
            profile,
            jobs,
            offline,
            locked,
            // `--target`, else `CARGO_BUILD_TARGET` / `build.target`.
            target,
            features,
            selected,
            verbose: common.verbose,
            quiet: common.quiet,
            show_warnings: true,
            json,
            force_rebuild,
            ignore_rust_version: common.ignore_rust_version,
            native_dirs: Default::default(),
            run_env: Vec::new(),
            sandbox,
            extra_costs: Default::default(),
        })
    }

    pub fn config(&self) -> &RuneConfig {
        &self.ws.config
    }

    /// The CAS, unless disabled by `cas.enabled = false`.
    pub fn cas(&self) -> Option<CasStore> {
        if !self.ws.config.settings.cas_enabled() {
            return None;
        }
        let settings = &self.ws.config.settings;
        CasStore::new(self.ws.config.cas_dir.clone()).ok().map(|c| {
            let c = c.with_verify(settings.cas_verify());
            let Some(spec) = settings.cas_remote() else {
                return c;
            };
            let token = std::env::var("RUNE_CAS_REMOTE_TOKEN")
                .ok()
                .filter(|t| !t.is_empty())
                .or_else(|| settings.cas_remote_token());
            match rune_cas::remote::Remote::from_spec(
                &spec,
                settings.cas_remote_write(),
                token,
                settings.cas_remote_replicas(),
            ) {
                Ok(remote) => c.with_remote_store(remote),
                Err(e) => {
                    crate::ui::warn(format!("ignoring cas.remote: {e}"));
                    c
                }
            }
        })
    }

    pub fn executor(&self) -> BuildExecutor {
        let mut executor =
            BuildExecutor::new(self.ws.config.clone(), self.profile.clone(), self.jobs)
                .with_force_rebuild(self.force_rebuild)
                .with_config_env(
                    self.ws
                        .cargo_config
                        .env_entries()
                        .into_iter()
                        .map(|e| (e.name, e.value, e.force))
                        .collect(),
                )
                .with_color(colored::control::SHOULD_COLORIZE.should_colorize())
                .with_frontend_threads(self.ws.config.settings.frontend_threads());
        if let Some(cas) = self.cas() {
            executor = executor.with_cas(cas);
        }
        if let Some(policy) = &self.sandbox {
            executor = executor.with_sandbox(policy.clone());
        }
        if !self.extra_costs.is_empty() {
            executor = executor.with_extra_costs(self.extra_costs.clone());
        }
        executor
    }

    /// Says so before building when the disk the build writes to is nearly
    /// full: compilers and linkers that run out of space fail in confusing
    /// ways (a linker can die with a bus error).
    pub fn warn_if_disk_is_low(&self) {
        const LOW: u64 = 1 << 30;
        let config = self.config();
        for dir in [&config.target_dir, &config.cas_dir] {
            if let Some(free) = rune_common::sysinfo::disk_available(dir)
                && free < LOW
            {
                ui::warn(format!(
                    "only {} free on the disk holding {}: the build may fail for lack of space \
                     (`rune cas gc --max-size 5GB` shrinks the store, `rune clean` removes \
                     build output)",
                    ui::bytes(free),
                    dir.display()
                ));
                return;
            }
        }
    }

    /// A compiler that runs out of disk space rarely says so (rustc and the
    /// linker crash with a bus error): when a build failed and the disk is
    /// full, name that as the likely cause.
    fn explain_failure_by_disk(&self, failures: &[(String, String)]) {
        const NEARLY_FULL: u64 = 64 << 20;
        let dir = &self.config().target_dir;
        let said_so = failures
            .iter()
            .any(|(_, e)| e.contains("No space left on device") || e.contains("os error 28"));
        let free = rune_common::sysinfo::disk_available(dir);
        if said_so || free.is_some_and(|f| f < NEARLY_FULL) {
            ui::hint(format!(
                "the disk holding {} is full{}: that is the likely cause. \
                 `rune cas gc --max-size 5GB` shrinks the store, `rune clean` removes build \
                 output",
                dir.display(),
                free.map(|f| format!(" ({} free)", ui::bytes(f)))
                    .unwrap_or_default()
            ));
        }
    }

    /// Evicts old CAS entries if `cas.max-size` is set and exceeded, and —
    /// at most once a day — starts `rune cas gc --max-age` in the background
    /// for entries unused for `cas.max-age`.
    pub fn maybe_gc(&self) {
        let Some(cas) = self.cas() else { return };
        let settings = &self.ws.config.settings;
        let (age, compress) = (settings.cas_max_age(), settings.cas_compress_after());
        if age.is_some() || compress.is_some() {
            spawn_daily_gc(cas.root(), age, compress);
        }
        let Some(max) = self.ws.config.settings.cas_max_size_for(cas.root()) else {
            return;
        };
        if cas.stats().map(|s| s.total_size_bytes).unwrap_or(0) > max {
            let _ = run_gc(
                &cas,
                &GcConfig {
                    max_size_bytes: Some(max),
                    max_age_secs: None,
                    compress_idle_secs: None,
                },
            );
        }
    }

    /// Prints the profile name the way summaries show it.
    pub fn profile_label(&self) -> String {
        self.profile.name.clone()
    }
}

/// Starts `rune cas gc --max-age --compress-idle` for the store at `root` in the background
/// when the last run was more than a day ago; the build does not wait for it.
/// A store seen for the first time only gets its stamp: nothing in it can
/// have gone unused for long, and short-lived stores (tests, CI) never pay.
fn spawn_daily_gc(root: &std::path::Path, max_age_secs: Option<u64>, compress: Option<u64>) {
    let stamp = root.join("last-gc");
    let due = match std::fs::metadata(&stamp).and_then(|m| m.modified()) {
        Ok(t) => t
            .elapsed()
            .is_ok_and(|e| e > std::time::Duration::from_secs(86_400)),
        Err(_) => {
            let _ = std::fs::write(&stamp, b"");
            false
        }
    };
    if !due {
        return;
    }
    // Claim the day first, so concurrent builds do not all start one.
    let _ = std::fs::write(&stamp, b"");
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["cas", "gc"]);
    if let Some(age) = max_age_secs {
        cmd.arg("--max-age").arg(age.to_string());
    }
    if let Some(idle) = compress {
        cmd.arg("--compress-idle").arg(idle.to_string());
    }
    // Compressing is background work: never at the expense of the user's.
    let _ = cmd
        .env("RUNE_CAS_DIR", root)
        .env("RUNE_BACKGROUND", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

impl Session {
    /// A command that runs `exe`: directly, or — when cross-compiling and
    /// `[target.<triple>] runner` is configured — through that runner (`qemu-aarch64`,
    /// `wine`, a device shim), exactly like `cargo run`/`cargo test` do.
    pub fn command_for(&self, exe: &std::path::Path) -> std::process::Command {
        if let Some(target) = &self.target {
            let tc = rune_common::toolchain::global();
            let info = rune_common::toolchain::TargetInfo::query(tc, Some(target), &[]);
            if let Some(runner) = rune_common::toolchain::runner(&self.ws.cargo_config, &info) {
                let mut cmd = std::process::Command::new(&runner[0]);
                cmd.args(&runner[1..]).arg(exe);
                return cmd;
            }
        }
        std::process::Command::new(exe)
    }

    /// `RUSTDOCFLAGS` / `build.rustdocflags` / `target.<triple>.rustdocflags`.
    pub fn rustdoc_flags(&self) -> Vec<String> {
        if let Ok(v) = std::env::var("RUSTDOCFLAGS") {
            return v.split_whitespace().map(str::to_string).collect();
        }
        let cfg = &self.ws.cargo_config;
        if let Some(target) = &self.target
            && let Some(f) = cfg.string_list(&["target", target, "rustdocflags"])
        {
            return f;
        }
        cfg.string_list(&["build", "rustdocflags"])
            .unwrap_or_default()
    }
}

/// Resolves a profile name. `test` and `bench` fall back to `dev`/`release`
/// unless the manifest customizes them, so plain `rune build` and `rune test`
/// share compiled dependencies through the CAS.
fn profile_by_name(ws: &Workspace, name: &str) -> Result<Profile> {
    let tables = &ws.root_manifest.profile;
    let effective = match name {
        "test" if !tables.contains_key("test") => "dev",
        "bench" if !tables.contains_key("bench") => "release",
        other => other,
    };
    resolve_profile(effective, tables).map_err(|e| anyhow::anyhow!("{e}"))
}

/// The target directory `--target-dir`, `RUNE_TARGET_DIR` or
/// `CARGO_TARGET_DIR` ask for, if any (else `build.target-dir` or
/// `.rune/target`).
pub fn target_dir_override(common: &CommonArgs) -> Result<Option<std::path::PathBuf>> {
    if let Some(dir) = &common.target_dir {
        return Ok(Some(if dir.is_absolute() {
            dir.clone()
        } else {
            std::env::current_dir()?.join(dir)
        }));
    }
    match std::env::var_os("RUNE_TARGET_DIR")
        .or_else(|| std::env::var_os("CARGO_TARGET_DIR"))
        .filter(|d| !d.is_empty())
    {
        Some(dir) => Ok(Some(std::env::current_dir()?.join(dir))),
        None => Ok(None),
    }
}

/// Runs the executor over `graph`, calling `on_event` on this thread for every
/// pipeline event as it arrives. Draws the standard progress bar and prints
/// compiler warnings unless the session is quiet.
pub fn drive(
    sess: &Session,
    graph: &BuildGraph,
    mut on_event: impl FnMut(&PipelineEvent),
) -> Result<(BuildResult, Vec<(String, String)>)> {
    sess.warn_if_disk_is_low();
    let executor = sess.executor();
    let event_rx = executor.event_receiver();
    let mut failures: Vec<(String, String)> = Vec::new();

    let result = std::thread::scope(|scope| -> Result<BuildResult> {
        let handle = scope.spawn(|| executor.execute(graph));

        let checking = graph
            .nodes()
            .iter()
            .all(|n| n.unit.check_only || n.unit.is_build_script || !n.unit.ext.member);
        let verb = if checking { "checking" } else { "building" };
        let mut progress = (!sess.quiet && !sess.json).then(|| {
            BuildProgress::with_verb(ui::multi_progress(), graph.node_count() as u64, verb)
        });
        let mut scripts_reported: std::collections::HashSet<String> = Default::default();
        while let Ok(event) = event_rx.recv() {
            let is_final = matches!(event, PipelineEvent::AllComplete { .. });
            if sess.json {
                let node = match &event {
                    PipelineEvent::BuildFinished { node_id, .. }
                    | PipelineEvent::CacheHit { node_id, .. }
                    | PipelineEvent::BuildFailed { node_id, .. } => Some(*node_id),
                    _ => None,
                };
                if let Some(node) = node {
                    let unit = &graph.node(node).unit;
                    for line in
                        crate::messages::lines_for(unit, sess.config(), &sess.profile, &event)
                    {
                        ui::out(line);
                    }
                }
                if let PipelineEvent::ScriptOutput { node_id, output } = &event {
                    let unit = &graph.node(*node_id).unit;
                    let owner = unit
                        .ext
                        .script_owner
                        .clone()
                        .unwrap_or_else(|| rune_scheduler::executor::script_key(unit));
                    // Once per script, however many of its package's units
                    // report it.
                    if scripts_reported.insert(owner.clone()) {
                        let out_dir = sess
                            .config()
                            .build_script_out_dir_by_key(sess.profile.output_dir_name(), &owner);
                        ui::out(crate::messages::build_script_executed(
                            unit, output, &out_dir,
                        ));
                    }
                }
                if let PipelineEvent::AllComplete { failed, .. } = &event {
                    ui::out(crate::messages::build_finished(*failed == 0));
                }
            }
            match &event {
                PipelineEvent::ScriptOutput { output, .. } => {
                    let root = &sess.config().target_dir;
                    let mut dirs = sess.native_dirs.lock().unwrap();
                    for ls in &output.link_search {
                        if ls.path.starts_with(root) && !dirs.contains(&ls.path) {
                            dirs.push(ls.path.clone());
                        }
                    }
                }
                PipelineEvent::BuildFailed { node_id, error, .. } => {
                    let unit = &graph.node(*node_id).unit;
                    failures.push((
                        format!("{} v{}", unit.crate_id.name, unit.crate_id.version),
                        error.clone(),
                    ));
                }
                PipelineEvent::BuildFinished { warnings, .. }
                | PipelineEvent::CacheHit { warnings, .. }
                | PipelineEvent::ScriptWarnings { warnings, .. }
                    if !warnings.is_empty() && sess.show_warnings && !sess.quiet && !sess.json =>
                {
                    ui::print_above_progress(warnings);
                }
                _ => {}
            }
            if let Some(p) = progress.as_mut() {
                p.on_event(&event);
            }
            on_event(&event);
            if is_final {
                break;
            }
        }

        handle
            .join()
            .map_err(|_| anyhow::anyhow!("build worker thread panicked"))?
            .map_err(|e| anyhow::anyhow!(e.to_string()))
    })?;

    // The compiler's own messages; the one-line verdict naming the crates
    // comes last, from `ensure_success`, like Cargo's.
    for (_, error) in &failures {
        if !sess.json {
            ui::err(error.trim_end());
            ui::err("");
        }
    }
    if !failures.is_empty() {
        sess.explain_failure_by_disk(&failures);
    }
    *FAILED.lock().unwrap_or_else(|e| e.into_inner()) =
        failures.iter().map(|(c, _)| c.clone()).collect();
    sess.maybe_gc();
    Ok((result, failures))
}

/// A build for a target whose standard library is not installed fails in
/// every crate with rustc's "can't find crate for `core`"; say what to do
/// instead, before anything is compiled. Custom targets (a `.json` spec) and
/// `-Zbuild-std`, which builds the standard library itself, are left alone.
fn check_target_installed(ws: &Workspace, target: &str) -> Result<()> {
    if target.ends_with(".json")
        || ws
            .cargo_config
            .string_list(&["unstable", "build-std"])
            .is_some()
        || ws.cargo_config.string(&["unstable", "build-std"]).is_some()
    {
        return Ok(());
    }
    let tc = rune_common::toolchain::global();
    let Some(sysroot) = &tc.sysroot else {
        return Ok(());
    };
    if sysroot
        .join("lib/rustlib")
        .join(target)
        .join("lib")
        .is_dir()
    {
        return Ok(());
    }
    let known = std::process::Command::new(&tc.rustc)
        .args(["--print", "target-list"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .any(|l| l.trim() == target)
        });
    match known {
        Some(false) => bail!(
            "unknown target `{target}` (`rustc --print target-list` lists the ones this compiler knows)"
        ),
        _ => {
            let rustup = sysroot.components().any(|c| c.as_os_str() == ".rustup")
                || std::env::var_os("RUSTUP_HOME").is_some();
            if rustup {
                bail!(
                    "the standard library for `{target}` is not installed; add it with:\n\n    rustup target add {target}\n"
                )
            }
            bail!(
                "the standard library for `{target}` is not installed in {}",
                sysroot.display()
            )
        }
    }
}

/// The crates the last build could not compile (`name vX.Y.Z`).
static FAILED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Bails, naming the crates that did not compile, if a build had failures.
pub fn ensure_success(result: &BuildResult) -> Result<()> {
    if result.success {
        return Ok(());
    }
    let failed = FAILED.lock().unwrap_or_else(|e| e.into_inner()).clone();
    match failed.as_slice() {
        [] => bail!(
            "could not compile {}",
            ui::plural(result.failed.len(), "crate")
        ),
        [one] => bail!("could not compile {}", one.bold()),
        many => {
            const SHOWN: usize = 3;
            let mut names: Vec<String> = many
                .iter()
                .take(SHOWN)
                .map(|c| c.bold().to_string())
                .collect();
            if many.len() > SHOWN {
                names.push(format!("{} more", many.len() - SHOWN));
            }
            bail!(
                "could not compile {}: {}",
                ui::plural(many.len(), "crate"),
                names.join(", ")
            )
        }
    }
}

/// `RUSTFLAGS` (or `CARGO_ENCODED_RUSTFLAGS`) as individual arguments.
pub fn rustflags_from_env_public() -> Vec<String> {
    rune_scheduler::executor::rustflags_from_env()
}
