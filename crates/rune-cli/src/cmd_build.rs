//! `build`, `check`, `clippy`, `run`, `install`, `doc`.

use anyhow::{Context, Result, bail};
use colored::Colorize;
use rune_common::TargetKind;
use rune_scheduler::executor::out_dir_for;
use std::path::{Path, PathBuf};

use crate::args::{BuildArgs, DocArgs, InstallArgs, RunArgs, TargetArgs};
use crate::graph::{self, Mode, Plan, Request, TargetSelection};
use crate::session::{Session, drive, ensure_success};
use crate::ui;

pub fn selection(t: &TargetArgs) -> TargetSelection {
    TargetSelection {
        lib: t.lib,
        bins: t.bins,
        bin: t.bin.clone(),
        examples: t.examples,
        example: t.example.clone(),
        tests: t.tests,
        test: t.test.clone(),
        benches: t.benches,
        bench: t.bench.clone(),
        all_targets: t.all_targets,
        doc_only: false,
    }
}

/// Plans a build for a session.
pub fn plan_for(sess: &Session, mode: Mode, targets: &TargetSelection) -> Result<Plan> {
    warn_on_cross_compile(sess);
    graph::plan(
        &sess.ws,
        &Request {
            profile: &sess.profile,
            target: sess.target.as_deref(),
            mode,
            selected: &sess.selected,
            features: &sess.features,
            targets,
            offline: sess.offline,
            locked: sess.locked,
            doctests: false,
        },
    )
}

fn warn_on_cross_compile(sess: &Session) {
    let host = rune_common::TargetTriple::host().map(|t| t.0);
    if let (Some(target), Some(host)) = (&sess.target, host) {
        if *target != host {
            ui::warn(format!(
                "cross-compiling for `{target}`: build scripts and proc-macros are still built \
                 for the host, and their dependencies are not yet built separately for the host"
            ));
        }
    }
}

pub fn build(args: BuildArgs, mode: Mode) -> Result<()> {
    let sess = Session::new(&args.common, "dev")?;
    let plan = plan_for(&sess, mode, &selection(&args.targets))?;
    sess.ws.config.ensure_dirs(sess.profile.output_dir_name())?;

    let (result, _) = drive(&sess, &plan.graph, |_| {})?;
    ensure_success(&result)?;
    summarize(&sess, &result);
    Ok(())
}

pub fn summarize(sess: &Session, result: &rune_scheduler::executor::BuildResult) {
    if sess.quiet {
        return;
    }
    let mut parts = vec![
        sess.profile_label().bold().to_string(),
        format!("{} compiled", result.compiled),
        format!("{} cached", result.cached),
    ];
    if sess.force_rebuild {
        parts.push("rebuilt all".to_string());
    }
    parts.push(format!("{:.2}s", result.total_duration.as_secs_f64()));
    ui::summary(parts.join(" · "));
}

/// `rune clippy`: lints every local crate with `clippy-driver`.
///
/// Reuses the exact same resolve/graph/executor pipeline as `rune check`
/// (`clippy-driver` accepts every flag `rustc` does, plus its own lint
/// passes) by pointing the `RUSTC` environment variable at it before building.
/// Dependencies are compiled with `--cap-lints allow`, so only local crates
/// actually get linted, as with `cargo clippy`.
pub fn clippy(args: BuildArgs) -> Result<()> {
    let clippy_driver = std::env::var_os("CLIPPY_DRIVER")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("clippy-driver"));

    if std::process::Command::new(&clippy_driver)
        .arg("--version")
        .output()
        .is_err()
    {
        bail!(
            "`{}` not found on PATH — install it with `rustup component add clippy`",
            clippy_driver.display()
        );
    }
    // SAFETY: called on the main thread before any worker threads exist.
    unsafe {
        std::env::set_var("RUSTC", &clippy_driver);
    }
    build(args, Mode::Check)
}

// ---------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------

pub fn run(args: RunArgs) -> Result<()> {
    let sess = Session::new(&args.common, "dev")?;

    // Decide what to run: `--bin`, `--example`, `default-run`, or the only binary.
    let mut targets = TargetSelection::default();
    let (kind, name) = if let Some(ex) = &args.example {
        targets.example = vec![ex.clone()];
        (TargetKind::Example, ex.clone())
    } else {
        let name = match &args.bin {
            Some(b) => b.clone(),
            None => default_bin(&sess)?,
        };
        targets.bin = vec![name.clone()];
        (TargetKind::Bin, name)
    };

    let plan = plan_for(&sess, Mode::Build, &targets)?;
    sess.ws.config.ensure_dirs(sess.profile.output_dir_name())?;
    let (result, _) = drive(&sess, &plan.graph, |_| {})?;
    ensure_success(&result)?;
    summarize(&sess, &result);

    let node = plan
        .requested
        .iter()
        .find(|r| r.kind == kind && r.target == name && !r.harness)
        .with_context(|| format!("no {} target named `{name}`", kind.as_str()))?;
    let unit = &plan.graph.node(node.node).unit;
    let exe = out_dir_for(sess.config(), &sess.profile, unit).join(&name);
    let member = &sess.ws.members[node.member];

    ui::action("running", exe.display());
    let mut cmd = std::process::Command::new(&exe);
    cmd.args(&args.args);
    apply_run_env(&mut cmd, member, &sess);
    exec(cmd)
}

/// The binary `rune run` picks when `--bin` isn't given.
fn default_bin(sess: &Session) -> Result<String> {
    let mut bins: Vec<(String, Option<String>)> = Vec::new();
    for &mi in &sess.selected {
        let m = &sess.ws.members[mi];
        let default_run = m
            .manifest
            .package
            .as_ref()
            .and_then(|p| p.extra.get("default-run"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        for b in &m.manifest.bin {
            if let Some(name) = &b.name {
                bins.push((name.clone(), default_run.clone()));
            }
        }
    }
    if let Some((_, Some(default))) = bins.iter().find(|(_, d)| d.is_some()) {
        return Ok(default.clone());
    }
    match bins.len() {
        0 => bail!(
            "a bin target must be available for `rune run` (none found in the selected package)"
        ),
        1 => Ok(bins.remove(0).0),
        _ => {
            let names: Vec<_> = bins.iter().map(|(n, _)| n.as_str()).collect();
            bail!(
                "`rune run` could not determine which binary to run; use `--bin` or set \
                 `default-run` (available: {})",
                names.join(", ")
            )
        }
    }
}

/// The runtime environment Cargo gives programs it runs.
pub fn apply_run_env(
    cmd: &mut std::process::Command,
    member: &crate::workspace::Member,
    sess: &Session,
) {
    cmd.env("CARGO_MANIFEST_DIR", &member.dir)
        .env("CARGO_PKG_NAME", &member.name)
        .env("CARGO_PKG_VERSION", member.version.to_string())
        .env("CARGO_PKG_VERSION_MAJOR", member.version.major.to_string())
        .env("CARGO_PKG_VERSION_MINOR", member.version.minor.to_string())
        .env("CARGO_PKG_VERSION_PATCH", member.version.patch.to_string())
        .env("CARGO_PKG_VERSION_PRE", member.version.pre.as_str())
        .env("CARGO_PKG_AUTHORS", &member.meta.authors)
        .env("CARGO_PKG_DESCRIPTION", &member.meta.description);
    // Dynamic libraries (proc-macro hosts, dylib dependencies) live in `deps`.
    let deps = sess.config().deps_dir(sess.profile.output_dir_name());
    let mut paths = vec![deps];
    if let Some(existing) = std::env::var_os("LD_LIBRARY_PATH") {
        paths.extend(std::env::split_paths(&existing));
    }
    if let Ok(joined) = std::env::join_paths(paths) {
        cmd.env("LD_LIBRARY_PATH", joined);
    }
}

/// Replaces the current process with `cmd` on Unix; elsewhere spawns and
/// mirrors the exit code. The child's exit status becomes ours.
fn exec(mut cmd: std::process::Command) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        bail!(
            "failed to execute `{}`: {err}",
            cmd.get_program().to_string_lossy()
        );
    }
    #[cfg(not(unix))]
    {
        let status = cmd.status()?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

// ---------------------------------------------------------------------------
// install
// ---------------------------------------------------------------------------

pub fn install(args: InstallArgs) -> Result<()> {
    let mut common = args.common.clone();
    // Installed binaries are release builds unless a profile is named.
    common.release = args.common.profile.is_none();
    common.manifest_path = Some(args.path.join("Cargo.toml"));
    let sess = Session::new(&common, "release")?;

    let targets = TargetSelection {
        bins: args.bin.is_empty(),
        bin: args.bin.clone(),
        ..Default::default()
    };
    let plan = plan_for(&sess, Mode::Build, &targets)?;
    let bins: Vec<_> = plan
        .requested
        .iter()
        .filter(|r| r.kind == TargetKind::Bin && !r.harness)
        .collect();
    if bins.is_empty() {
        bail!("there is nothing to install: the package has no binary targets");
    }
    sess.ws.config.ensure_dirs(sess.profile.output_dir_name())?;
    let (result, _) = drive(&sess, &plan.graph, |_| {})?;
    ensure_success(&result)?;

    let bin_dir = install_dir(args.root.as_deref())?;
    std::fs::create_dir_all(&bin_dir)?;
    for r in bins {
        let unit = &plan.graph.node(r.node).unit;
        let src = out_dir_for(sess.config(), &sess.profile, unit).join(&r.target);
        let dest = bin_dir.join(&r.target);
        if dest.exists() && !args.force {
            bail!(
                "binary `{}` already exists in destination `{}` (use --force to overwrite)",
                r.target,
                bin_dir.display()
            );
        }
        // Copy, don't link: the installed file must survive `rune clean`.
        let _ = std::fs::remove_file(&dest);
        std::fs::copy(&src, &dest)
            .with_context(|| format!("failed to install {}", dest.display()))?;
        ui::action("installed", dest.display());
    }
    Ok(())
}

fn install_dir(root: Option<&Path>) -> Result<PathBuf> {
    if let Some(root) = root {
        return Ok(root.join("bin"));
    }
    if let Some(root) = std::env::var_os("CARGO_INSTALL_ROOT") {
        return Ok(PathBuf::from(root).join("bin"));
    }
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| home::home_dir().map(|h| h.join(".cargo")))
        .context("could not determine the install directory; pass --root")?;
    Ok(cargo_home.join("bin"))
}

// ---------------------------------------------------------------------------
// doc
// ---------------------------------------------------------------------------

/// `rune doc`: builds documentation with `rustdoc` for the selected packages'
/// libraries (and, unless `--no-deps`, everything they depend on).
pub fn doc(args: DocArgs) -> Result<()> {
    let sess = Session::new(&args.common, "dev")?;
    let plan = plan_for(
        &sess,
        Mode::Build,
        &TargetSelection {
            lib: true,
            ..Default::default()
        },
    )?;
    sess.ws.config.ensure_dirs(sess.profile.output_dir_name())?;
    let (result, _) = drive(&sess, &plan.graph, |_| {})?;
    ensure_success(&result)?;

    let doc_dir = sess.config().target_dir.join("doc");
    std::fs::create_dir_all(&doc_dir)?;
    let rustdoc = std::env::var_os("RUSTDOC")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("rustdoc"));
    let deps_dir = sess.config().deps_dir(sess.profile.output_dir_name());

    let topo = plan
        .graph
        .topological_sort()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let requested: std::collections::HashSet<_> = plan.requested.iter().map(|r| r.node).collect();
    let mut documented = 0;
    for id in topo {
        let node = plan.graph.node(id);
        let unit = &node.unit;
        if unit.kind != TargetKind::Lib || unit.harness {
            continue;
        }
        if args.no_deps && !requested.contains(&id) {
            continue;
        }
        if unit.crate_type == rune_common::CrateType::ProcMacro && !requested.contains(&id) {
            continue; // documenting proc-macro dependencies needs extra plumbing
        }
        let externs = rune_scheduler::executor::extern_deps_for(
            &plan.graph,
            id,
            &result.artifact_paths,
            &Default::default(),
            rune_scheduler::executor::ExternMode::Link,
        );
        let mut cmd = std::process::Command::new(&rustdoc);
        cmd.arg(&unit.source_root)
            .arg("--crate-name")
            .arg(unit.rustc_crate_name())
            .arg("--edition")
            .arg(unit.edition.as_str())
            .arg("-o")
            .arg(&doc_dir)
            .arg("-L")
            .arg(format!("dependency={}", deps_dir.display()))
            .arg("--cap-lints")
            .arg("allow");
        if unit.crate_type == rune_common::CrateType::ProcMacro {
            cmd.arg("--crate-type")
                .arg("proc-macro")
                .arg("--extern")
                .arg("proc_macro");
        }
        for f in &unit.features {
            cmd.arg("--cfg").arg(format!("feature=\"{f}\""));
        }
        for (name, path) in externs {
            cmd.arg("--extern")
                .arg(format!("{name}={}", path.display()));
        }
        cmd.env("CARGO_MANIFEST_DIR", &unit.manifest_dir)
            .env("CARGO_PKG_NAME", &unit.crate_id.name)
            .env("CARGO_PKG_VERSION", unit.crate_id.version.to_string());
        let out = cmd.output().with_context(|| {
            format!(
                "failed to run `{}` — is rustdoc installed?",
                rustdoc.display()
            )
        })?;
        if !out.status.success() {
            if requested.contains(&id) {
                ui::error(format!("rustdoc failed for {}", unit.crate_id.name));
                eprintln!("{}", String::from_utf8_lossy(&out.stderr));
                bail!("could not document `{}`", unit.crate_id.name);
            }
            continue;
        }
        if requested.contains(&id) {
            ui::action("documenting", &unit.crate_id.name);
        }
        documented += 1;
    }
    ui::summary(format!(
        "documented {documented} crates → {}",
        doc_dir.display()
    ));

    if args.open {
        if let Some(r) = plan.requested.first() {
            let unit = &plan.graph.node(r.node).unit;
            let index = doc_dir.join(unit.rustc_crate_name()).join("index.html");
            let _ = std::process::Command::new("xdg-open").arg(index).spawn();
        }
    }
    Ok(())
}
