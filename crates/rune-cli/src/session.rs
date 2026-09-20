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
    /// `--message-format json`: events go to stdout as JSON lines.
    pub json: bool,
    pub force_rebuild: bool,
    pub ignore_rust_version: bool,
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
        crate::ui::set_machine_output(json);
        crate::ui::set_quiet(common.quiet);
        let mut ws = Workspace::load(common.manifest_path.as_deref())?;

        if let Some(dir) = &common.target_dir {
            ws.config.target_dir = if dir.is_absolute() {
                dir.clone()
            } else {
                std::env::current_dir()?.join(dir)
            };
        } else if let Some(dir) = std::env::var_os("RUNE_TARGET_DIR")
            .or_else(|| std::env::var_os("CARGO_TARGET_DIR"))
            .filter(|d| !d.is_empty())
        {
            ws.config.target_dir = std::env::current_dir()?.join(dir);
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
            target: common.target.clone().or(default_target),
            features,
            selected,
            verbose: common.verbose,
            quiet: common.quiet,
            json,
            force_rebuild,
            ignore_rust_version: common.ignore_rust_version,
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
                .with_color(colored::control::SHOULD_COLORIZE.should_colorize());
        if let Some(cas) = self.cas() {
            executor = executor.with_cas(cas);
        }
        executor
    }

    /// Evicts old CAS entries if `cas.max-size` is set and exceeded.
    pub fn maybe_gc(&self) {
        let (Some(max), Some(cas)) = (self.ws.config.settings.cas_max_size(), self.cas()) else {
            return;
        };
        if cas.stats().map(|s| s.total_size_bytes).unwrap_or(0) > max {
            let _ = run_gc(
                &cas,
                &GcConfig {
                    max_size_bytes: Some(max),
                    max_age_secs: None,
                },
            );
        }
    }

    /// Prints the profile name the way summaries show it.
    pub fn profile_label(&self) -> String {
        self.profile.name.clone()
    }
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
        if let Some(target) = &self.target {
            if let Some(f) = cfg.string_list(&["target", target, "rustdocflags"]) {
                return f;
            }
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

/// Runs the executor over `graph`, calling `on_event` on this thread for every
/// pipeline event as it arrives. Draws the standard progress bar and prints
/// compiler warnings unless the session is quiet.
pub fn drive(
    sess: &Session,
    graph: &BuildGraph,
    mut on_event: impl FnMut(&PipelineEvent),
) -> Result<(BuildResult, Vec<(String, String)>)> {
    let executor = sess.executor();
    let event_rx = executor.event_receiver();
    let mut failures: Vec<(String, String)> = Vec::new();

    let result = std::thread::scope(|scope| -> Result<BuildResult> {
        let handle = scope.spawn(|| executor.execute(graph));

        let mut progress = (!sess.quiet && !sess.json)
            .then(|| BuildProgress::new(ui::multi_progress(), graph.node_count() as u64));
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
                    for line in crate::messages::lines_for(unit, &sess.profile, &event) {
                        println!("{line}");
                    }
                }
                if let PipelineEvent::AllComplete { failed, .. } = &event {
                    println!("{}", crate::messages::build_finished(*failed == 0));
                }
            }
            match &event {
                PipelineEvent::BuildFailed { node_id, error, .. } => {
                    let unit = &graph.node(*node_id).unit;
                    failures.push((
                        format!("{} v{}", unit.crate_id.name, unit.crate_id.version),
                        error.clone(),
                    ));
                }
                PipelineEvent::BuildFinished { warnings, .. }
                | PipelineEvent::CacheHit { warnings, .. }
                    if !warnings.is_empty() && !sess.quiet && !sess.json =>
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

    for (crate_desc, error) in &failures {
        ui::error(format!("failed to compile {}", crate_desc.bold()));
        if !sess.json {
            eprintln!("{error}");
        }
    }
    sess.maybe_gc();
    Ok((result, failures))
}

/// Bails with Cargo's message if a build had failures.
pub fn ensure_success(result: &BuildResult) -> Result<()> {
    if result.success {
        Ok(())
    } else {
        bail!(
            "could not compile due to {} previous error(s)",
            result.failed.len()
        )
    }
}

/// `RUSTFLAGS` (or `CARGO_ENCODED_RUSTFLAGS`) as individual arguments.
pub fn rustflags_from_env_public() -> Vec<String> {
    rune_scheduler::executor::rustflags_from_env()
}
