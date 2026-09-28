//! `build`, `check`, `clippy`, `run`, `install`, `doc`.

use anyhow::{Context, Result, bail};
use colored::Colorize;
use rune_common::TargetKind;
use rune_scheduler::executor::bin_dir_for;
use std::path::PathBuf;

use crate::args::{BuildArgs, DocArgs, RunArgs, TargetArgs};
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
            ignore_rust_version: sess.ignore_rust_version,
            doctests: false,
        },
    )
}

pub fn build(args: BuildArgs, mode: Mode) -> Result<()> {
    build_with(args, mode, |_| Ok(()))
}

/// [`build`], with a chance to adjust the plan before it runs.
fn build_with(
    args: BuildArgs,
    mode: Mode,
    adjust: impl FnOnce(&mut Plan) -> Result<()>,
) -> Result<()> {
    let sess = Session::new(&args.common, "dev")?;
    let mut plan = plan_for(&sess, mode, &selection(&args.targets))?;
    adjust(&mut plan)?;
    sess.ws.config.ensure_dirs(sess.profile.output_dir_name())?;

    let (result, _) = if args.deps_only {
        // Everything that is not a workspace member, and what those need.
        let roots: Vec<_> = plan
            .graph
            .nodes()
            .iter()
            .filter(|n| !n.unit.ext.member)
            .map(|n| n.id)
            .collect();
        let (deps, _) = plan.graph.subgraph(&roots);
        drive(&sess, &deps, |_| {})?
    } else {
        drive(&sess, &plan.graph, |_| {})?
    };
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
    // A slow build that left most cores idle says why.
    if !sess.json
        && let Some(hint) = result.schedule.as_ref().and_then(|s| s.hint())
    {
        ui::hint(hint);
    }
}

/// `rune clippy`: lints the workspace's crates with `clippy-driver`.
///
/// Reuses the exact same resolve/graph/executor pipeline as `rune check`,
/// with `clippy-driver` as `RUSTC_WORKSPACE_WRAPPER` — exactly what `cargo
/// clippy` does. Only workspace members go through it; dependencies are
/// compiled by rustc as for `rune check` (and share its cached results), and
/// build scripts see the real compiler as `RUSTC`: one that runs `$RUSTC
/// --version` (libc, serde, proc-macro2, …) would otherwise read clippy's
/// version and fail. Lint flags after `--`
/// (`-D warnings`, `-W clippy::pedantic`) go to the workspace's own crates,
/// and are part of their keys: a result cached without `-D warnings` is never
/// replayed as a pass with it.
pub fn clippy(args: crate::args::ClippyArgs) -> Result<()> {
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
        std::env::set_var("RUSTC_WORKSPACE_WRAPPER", &clippy_driver);
    }
    if args.fix {
        return fix_loop(&args.build, &args.args, &FixVcs::default(), "clippy");
    }
    build_with(args.build, Mode::Check, |plan| {
        add_flags(plan, &args.args, |n| n.unit.ext.member);
        Ok(())
    })
}

/// `rune rustc -- <ARGS>`: builds, with extra rustc arguments for the
/// selected targets only (their keys include them).
pub fn rustc(args: crate::args::RustcArgs) -> Result<()> {
    let extra = args.args.clone();
    build_with(args.build, Mode::Build, move |plan| {
        let targets: std::collections::HashSet<_> = plan.requested.iter().map(|r| r.node).collect();
        if targets.len() > 1 && !extra.is_empty() {
            let names: Vec<String> = plan
                .requested
                .iter()
                .map(|r| format!("{} `{}`", r.kind.as_str(), r.target))
                .collect();
            bail!(
                "extra arguments to `rustc` can only be passed to one target; this selects {}.\n\
                 Pick one with `--lib`, `--bin NAME`, `--example NAME`, `--test NAME` or `--bench NAME`",
                names.join(", ")
            );
        }
        add_flags(plan, &extra, |n| targets.contains(&n.id));
        Ok(())
    })
}

/// Settings of `rune fix` about uncommitted changes.
#[derive(Default)]
pub struct FixVcs {
    pub allow_dirty: bool,
    pub allow_staged: bool,
    pub allow_no_vcs: bool,
}

/// `rune fix`: applies the compiler's machine-applicable suggestions.
pub fn fix(args: crate::args::FixArgs) -> Result<()> {
    let vcs = FixVcs {
        allow_dirty: args.allow_dirty,
        allow_staged: args.allow_staged,
        allow_no_vcs: args.allow_no_vcs,
    };
    fix_loop(&args.build, &[], &vcs, "rustc")
}

/// Checks, applies suggestions, and checks again until none are left (at
/// most four rounds, as rustfix does).
fn fix_loop(args: &BuildArgs, flags: &[String], vcs: &FixVcs, what: &str) -> Result<()> {
    let sess = Session::new(&args.common, "dev")?;
    crate::fix::check_vcs(
        sess.ws.root(),
        vcs.allow_dirty,
        vcs.allow_staged,
        vcs.allow_no_vcs,
    )?;
    sess.ws.config.ensure_dirs(sess.profile.output_dir_name())?;
    let roots: Vec<PathBuf> = sess.ws.members.iter().map(|m| m.dir.clone()).collect();
    let mut total = 0;
    for _ in 0..4 {
        let mut plan = plan_for(&sess, Mode::Check, &selection(&args.targets))?;
        add_flags(&mut plan, flags, |n| n.unit.ext.member);
        let diagnostics = std::cell::RefCell::new(Vec::<String>::new());
        let (result, _) = drive(&sess, &plan.graph, |event| match event {
            rune_scheduler::PipelineEvent::BuildFinished {
                node_id,
                diagnostics: d,
                ..
            }
            | rune_scheduler::PipelineEvent::CacheHit {
                node_id,
                diagnostics: d,
                ..
            }
            | rune_scheduler::PipelineEvent::BuildFailed {
                node_id,
                diagnostics: d,
                ..
            } if plan.graph.node(*node_id).unit.ext.member => {
                diagnostics.borrow_mut().extend(d.iter().cloned());
            }
            _ => {}
        })?;
        let applied = crate::fix::apply(&diagnostics.borrow(), &roots)?;
        total += applied;
        if applied == 0 {
            ensure_success(&result)?;
            break;
        }
    }
    ui::summary(format!("applied {total} {what} suggestion(s)"));
    Ok(())
}

/// Adds `flags` to the rustc invocation of every node `pick` selects.
fn add_flags(plan: &mut Plan, flags: &[String], pick: impl Fn(&rune_graph::BuildNode) -> bool) {
    if flags.is_empty() {
        return;
    }
    let ids: Vec<_> = plan
        .graph
        .nodes()
        .iter()
        .filter(|n| pick(n))
        .map(|n| n.id)
        .collect();
    for id in ids {
        plan.graph
            .node_mut(id)
            .unit
            .ext
            .rustflags
            .extend(flags.iter().cloned());
    }
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
    let exe = bin_dir_for(sess.config(), &sess.profile, unit).join(unit.executable_name(&name));
    let member = &sess.ws.members[node.member];

    ui::action("running", exe.display());
    let mut cmd = sess.command_for(&exe);
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
    // Where the program's dynamic libraries are, as Cargo sets it up: the
    // native libraries build scripts produced, `deps/` (dylib dependencies)
    // and the profile directory (the workspace's own cdylibs).
    let dir = sess.profile.output_dir_name();
    let mut paths: Vec<PathBuf> = sess.native_dirs.lock().unwrap().clone();
    paths.push(sess.config().deps_dir(dir));
    paths.push(sess.config().profile_target_dir(dir));
    let var = dylib_path_var();
    match std::env::var_os(var) {
        Some(existing) => paths.extend(std::env::split_paths(&existing)),
        // Setting the fallback path replaces macOS's default one.
        None if var == "DYLD_FALLBACK_LIBRARY_PATH" => {
            if let Some(home) = home::home_dir() {
                paths.push(home.join("lib"));
            }
            paths.push(PathBuf::from("/usr/local/lib"));
            paths.push(PathBuf::from("/usr/lib"));
        }
        None => {}
    }
    if let Ok(joined) = std::env::join_paths(paths) {
        cmd.env(var, joined);
    }
}

/// The variable the platform's dynamic loader searches.
fn dylib_path_var() -> &'static str {
    if cfg!(windows) {
        "PATH"
    } else if cfg!(target_os = "macos") {
        "DYLD_FALLBACK_LIBRARY_PATH"
    } else {
        "LD_LIBRARY_PATH"
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
// doc
// ---------------------------------------------------------------------------

/// `rune doc`: builds documentation with `rustdoc` for the selected packages'
/// libraries (and, unless `--no-deps`, everything they depend on).
pub fn doc(args: DocArgs) -> Result<()> {
    doc_with(args, &[])
}

/// `rune rustdoc -- <ARGS>`: `rune doc` with extra rustdoc arguments for the
/// selected packages' own documentation.
pub fn rustdoc(args: crate::args::RustdocArgs) -> Result<()> {
    doc_with(args.doc, &args.args)
}

fn doc_with(args: DocArgs, extra: &[String]) -> Result<()> {
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
    let tc = rune_common::toolchain::global();
    let rustdoc = tc.rustdoc.clone();
    let doc_flags = sess.rustdoc_flags();
    // Documentation has an identity of its own: the rustdoc that renders it and
    // its flags, on top of everything that identifies the crate's compilation.
    let rustdoc_id = format!(
        "{}|{}|{}",
        rune_common::toolchain::program_fingerprint(&rustdoc),
        std::process::Command::new(&rustdoc)
            .arg("--version")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default(),
        doc_flags.join("\x1f")
    );
    let stamp_dir = doc_dir.join(".rune-stamps");
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
        // `[lib] doc = false` opts a library out of documentation.
        if sess
            .ws
            .members
            .iter()
            .find(|m| m.dir == unit.manifest_dir)
            .and_then(|m| m.manifest.lib.as_ref())
            .is_some_and(|lib| !lib.doc)
        {
            continue;
        }
        if unit.crate_type == rune_common::CrateType::ProcMacro && !requested.contains(&id) {
            continue; // documenting proc-macro dependencies needs extra plumbing
        }
        // Unchanged since the last `rune doc`? Then so is its documentation.
        let mut hb = rune_common::hash::HashBuilder::new();
        hb.update(
            result
                .cache_keys
                .get(&id)
                .map_or("", String::as_str)
                .as_bytes(),
        );
        hb.update(rustdoc_id.as_bytes());
        if requested.contains(&id) {
            hb.update(format!("{}|{extra:?}", args.document_private_items).as_bytes());
        }
        let stamp_name = format!("{}-{}", unit.rustc_crate_name(), &hb.finish_blake3()[..16]);
        let stamp = stamp_dir.join(&stamp_name);
        if stamp.is_file() && doc_dir.join(unit.rustc_crate_name()).is_dir() {
            if requested.contains(&id) {
                ui::action(
                    "documenting",
                    format!("{} (up to date)", unit.crate_id.name),
                );
            }
            documented += 1;
            continue;
        }
        let externs = rune_scheduler::executor::extern_deps_for(
            &plan.graph,
            id,
            &result.artifact_paths,
            &Default::default(),
            rune_scheduler::executor::ExternMode::Link,
        );
        let mut cmd = std::process::Command::new(&rustdoc);
        if let (Some(target), false) = (&unit.target, unit.ext.for_host) {
            cmd.arg("--target").arg(&target.0);
        }
        cmd.args(&doc_flags);
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
        // What the crate's build script gave its compilation: generated code
        // in `OUT_DIR` (bindgen output), `cfg`s, environment.
        if unit.build_script.is_some() {
            let key = unit
                .ext
                .script_owner
                .clone()
                .unwrap_or_else(|| rune_scheduler::executor::script_key(unit));
            cmd.env(
                "OUT_DIR",
                sess.config()
                    .build_script_out_dir_by_key(sess.profile.output_dir_name(), &key),
            );
        }
        if let Some(out) = result.build_script_outputs.get(&id) {
            for cfg in &out.cfgs {
                cmd.arg("--cfg").arg(cfg);
            }
            for (k, v) in &out.env_vars {
                cmd.env(k, v);
            }
        }
        if requested.contains(&id) {
            if args.document_private_items {
                cmd.arg("--document-private-items");
            }
            cmd.args(extra);
        }
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
        // Record the identity this output corresponds to; older stamps of the
        // same crate no longer describe it.
        let _ = std::fs::create_dir_all(&stamp_dir);
        if let Ok(entries) = std::fs::read_dir(&stamp_dir) {
            for e in entries.flatten() {
                if e.file_name()
                    .to_string_lossy()
                    .starts_with(&format!("{}-", unit.rustc_crate_name()))
                {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        let _ = std::fs::write(&stamp, "");
        documented += 1;
    }
    ui::summary(format!(
        "documented {documented} crates → {}",
        doc_dir.display()
    ));

    if args.open
        && let Some(r) = plan.requested.first()
    {
        let unit = &plan.graph.node(r.node).unit;
        let index = doc_dir.join(unit.rustc_crate_name()).join("index.html");
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else if cfg!(windows) {
            "explorer"
        } else {
            "xdg-open"
        };
        ui::action("opening", index.display());
        let _ = std::process::Command::new(opener).arg(index).spawn();
    }
    Ok(())
}
