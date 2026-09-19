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
    pub force_rebuild: bool,
}

impl Session {
    /// `default_profile` is what the command builds with absent `--release`
    /// and `--profile` (`dev`, `test`, or `bench`).
    pub fn new(common: &CommonArgs, default_profile: &str) -> Result<Self> {
        if common.timings {
            rune_common::timing::enable();
        }
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
            target: common.target.clone(),
            features,
            selected,
            verbose: common.verbose,
            quiet: common.quiet,
            force_rebuild,
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
        CasStore::new(self.ws.config.cas_dir.clone()).ok()
    }

    pub fn executor(&self) -> BuildExecutor {
        let mut executor =
            BuildExecutor::new(self.ws.config.clone(), self.profile.clone(), self.jobs)
                .with_force_rebuild(self.force_rebuild)
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

        let mut progress = (!sess.quiet)
            .then(|| BuildProgress::new(ui::multi_progress(), graph.node_count() as u64));
        while let Ok(event) = event_rx.recv() {
            let is_final = matches!(event, PipelineEvent::AllComplete { .. });
            match &event {
                PipelineEvent::BuildFailed { node_id, error } => {
                    let unit = &graph.node(*node_id).unit;
                    failures.push((
                        format!("{} v{}", unit.crate_id.name, unit.crate_id.version),
                        error.clone(),
                    ));
                }
                PipelineEvent::BuildFinished { warnings, .. }
                | PipelineEvent::CacheHit { warnings, .. }
                    if !warnings.is_empty() && !sess.quiet =>
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
        eprintln!("{error}");
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
