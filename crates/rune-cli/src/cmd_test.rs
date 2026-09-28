//! `rune test` and `rune bench`.
//!
//! Test binaries are nodes in the same build graph as everything else, so the
//! scheduler builds them as soon as their dependencies are ready and this
//! module starts each one **the moment it finishes linking** — while the rest
//! of the graph is still compiling — instead of waiting for a full build first
//! and then running test binaries one after another.
//!
//! Output is printed in a fixed order (package, then library, binaries,
//! integration tests, doctests) regardless of which binary happens to finish
//! first, so a run's output is predictable and diffable.

use anyhow::{Result, bail};
use colored::Colorize;
use crossbeam_channel::{Receiver, Sender, unbounded};
use rune_common::TargetKind;
use rune_graph::NodeId;
use rune_scheduler::PipelineEvent;
use rune_scheduler::executor::out_dir_for;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::args::TestArgs;
use crate::cmd_build::{apply_run_env, selection};
use crate::graph::{self, DocTest, Mode, Plan, Request, Requested};
use crate::session::{Session, drive};
use crate::ui;

/// One thing to run and where it sorts in the output.
struct Runnable {
    order: usize,
    label: String,
    kind: TargetKind,
    harness: bool,
    member: usize,
    exe_dir_key: NodeId,
}

enum Outcome {
    Passed,
    /// Passed in an earlier run with identical inputs; not run again.
    Cached,
    Failed,
    CompileFailed,
    Skipped,
}

struct TestResult {
    order: usize,
    outcome: Outcome,
    counts: Counts,
    output: String,
    took: Duration,
    /// Tests that failed and then passed when run again (`--retries`).
    flaky: Vec<String>,
}

#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Counts {
    pub passed: u64,
    pub failed: u64,
    pub ignored: u64,
    pub filtered: u64,
}

impl Counts {
    fn add(&mut self, o: Counts) {
        self.passed += o.passed;
        self.failed += o.failed;
        self.ignored += o.ignored;
        self.filtered += o.filtered;
    }
}

/// Parses libtest's `test result: ok. 3 passed; 0 failed; 1 ignored; ...` lines
/// (a binary can print several, e.g. one per doctest batch) and sums them.
pub fn parse_counts(output: &str) -> Counts {
    let mut total = Counts::default();
    for line in output.lines() {
        let Some(rest) = line.trim().strip_prefix("test result:") else {
            continue;
        };
        let mut c = Counts::default();
        for part in rest.split([';', '.']) {
            let mut words = part.split_whitespace();
            let (Some(n), Some(what)) = (words.next(), words.next()) else {
                continue;
            };
            let Ok(n) = n.parse::<u64>() else { continue };
            match what {
                "passed" => c.passed = n,
                "failed" => c.failed = n,
                "ignored" => c.ignored = n,
                "filtered" => c.filtered = n,
                _ => {}
            }
        }
        total.add(c);
    }
    total
}

pub fn run(args: TestArgs, mode: Mode) -> Result<()> {
    let bench = mode == Mode::Bench;
    let sess = Session::new(&args.common, if bench { "bench" } else { "test" })?;

    let mut targets = selection(&args.targets);
    targets.doc_only = args.doc;
    let want_doctests = !bench
        && !args.no_doc
        && (args.doc || sess.ws.config.settings.doctests())
        && !(targets.lib
            || targets.bins
            || !targets.bin.is_empty()
            || targets.examples
            || !targets.example.is_empty()
            || targets.tests
            || !targets.test.is_empty());

    warn_cross(&sess);
    let plan = graph::plan(
        &sess.ws,
        &Request {
            profile: &sess.profile,
            target: sess.target.as_deref(),
            mode,
            selected: &sess.selected,
            features: &sess.features,
            targets: &targets,
            offline: sess.offline,
            locked: sess.locked,
            ignore_rust_version: sess.ignore_rust_version,
            doctests: want_doctests,
        },
    )?;
    sess.ws.config.ensure_dirs(sess.profile.output_dir_name())?;

    // -- What will run, in output order.
    let mut runnables: Vec<Runnable> = plan
        .requested
        .iter()
        .filter(|r| is_runnable(r, bench))
        .map(|r| Runnable {
            order: 0,
            label: label_for(&sess, r),
            kind: r.kind,
            harness: r.harness,
            member: r.member,
            exe_dir_key: r.node,
        })
        .collect();
    let kind_rank = |k: TargetKind| match k {
        TargetKind::Lib => 0,
        TargetKind::Bin => 1,
        TargetKind::Example => 2,
        TargetKind::Test => 3,
        TargetKind::Bench => 4,
    };
    runnables.sort_by(|a, b| {
        (a.member, kind_rank(a.kind), &a.label).cmp(&(b.member, kind_rank(b.kind), &b.label))
    });
    for (i, r) in runnables.iter_mut().enumerate() {
        r.order = i;
    }
    let doc_base = runnables.len();
    let total_slots = doc_base + plan.doc_tests.len();
    if total_slots == 0 {
        ui::warn("nothing to test in the selected packages");
        return Ok(());
    }

    let by_node: HashMap<NodeId, usize> = runnables
        .iter()
        .enumerate()
        .map(|(i, r)| (r.exe_dir_key, i))
        .collect();

    let libtest_args = libtest_args(&args, bench);
    let fail_fast = !args.no_fail_fast;
    let stop = AtomicBool::new(false);
    // Passing arguments to the test binaries (`-- --nocapture`, `--list`,
    // `--show-output`, ...) means the user wants to see what they print.
    let verbose = sess.verbose || !args.test_args.is_empty();
    let start = Instant::now();

    // -- Plumbing: jobs → runners → printer.
    enum Job {
        Binary {
            order: usize,
            exe: PathBuf,
            member: usize,
            harness: bool,
        },
        Doc {
            order: usize,
            doc: usize,
            externs: Vec<(String, PathBuf)>,
            script: DocScript,
        },
    }
    let (job_tx, job_rx) = unbounded::<Job>();
    let (res_tx, res_rx) = unbounded::<TestResult>();

    let mut counts_total = Counts::default();
    let mut report_suites: Vec<crate::test_report::Suite> = Vec::new();
    let mut flaky_tests: Vec<String> = Vec::new();
    let mut failed_labels: Vec<String> = Vec::new();
    let mut built_ok = true;
    let mut binaries_run = 0usize;

    let labels: Vec<String> = runnables
        .iter()
        .map(|r| r.label.clone())
        .chain(
            plan.doc_tests
                .iter()
                .map(|d| format!("{} (doc-tests)", sess.ws.members[d.member].name)),
        )
        .collect();

    // `--cached`: remembered passes, keyed by everything Rune can see.
    let test_cas = (!bench
        && args
            .cached
            .unwrap_or_else(|| sess.ws.config.settings.test_cache()))
    .then(|| sess.cas())
    .flatten();
    let package_files = std::sync::Mutex::new(
        rune_fingerprint::FingerprintCache::load(
            &sess.config().fingerprint_dir.join("source-cache.json"),
        )
        .unwrap_or_default(),
    );

    let runner_count = sess.jobs.clamp(1, 16);
    let build_result = std::thread::scope(|scope| -> Result<_> {
        // Runner threads.
        for _ in 0..runner_count {
            let job_rx: Receiver<Job> = job_rx.clone();
            let res_tx: Sender<TestResult> = res_tx.clone();
            let sess = &sess;
            let libtest_args = &libtest_args;
            let stop = &stop;
            let plan_ref = &plan;
            let args_ref = &args;
            let test_cas = &test_cas;
            let package_files = &package_files;
            scope.spawn(move || {
                while let Ok(job) = job_rx.recv() {
                    let order = match &job {
                        Job::Binary { order, .. } | Job::Doc { order, .. } => *order,
                    };
                    if fail_fast && stop.load(Ordering::Relaxed) {
                        let _ = res_tx.send(TestResult {
                            order,
                            outcome: Outcome::Skipped,
                            counts: Counts::default(),
                            output: String::new(),
                            took: Duration::ZERO,
                            flaky: Vec::new(),
                        });
                        continue;
                    }
                    let mut remember_as: Option<String> = None;
                    let result = match job {
                        Job::Doc {
                            order,
                            doc,
                            externs,
                            script,
                        } => {
                            let r = run_doctest(
                                sess,
                                &plan_ref.doc_tests[doc],
                                &externs,
                                &script,
                                args_ref,
                            );
                            TestResult { order, ..r }
                        }
                        Job::Binary {
                            order,
                            exe,
                            member,
                            harness,
                        } => {
                            let member = &sess.ws.members[member];
                            if let (Some(cas), true) = (test_cas, harness) {
                                let package_hash = {
                                    let mut files = package_files.lock().unwrap();
                                    rune_fingerprint::hasher::compute_package_hash(
                                        &member.dir,
                                        &mut files,
                                    )
                                    .ok()
                                };
                                remember_as = package_hash.and_then(|ph| {
                                    crate::test_cache::key(
                                        &exe,
                                        &ph,
                                        libtest_args,
                                        std::env::vars(),
                                    )
                                    .ok()
                                });
                                if let Some(rec) = remember_as
                                    .as_ref()
                                    .and_then(|k| cas.get_value::<crate::test_cache::Record>(k))
                                {
                                    let _ = res_tx.send(TestResult {
                                        order,
                                        outcome: Outcome::Cached,
                                        counts: Counts {
                                            passed: rec.passed,
                                            failed: 0,
                                            ignored: rec.ignored,
                                            filtered: rec.filtered,
                                        },
                                        output: rec.output,
                                        took: Duration::ZERO,
                                        flaky: Vec::new(),
                                    });
                                    continue;
                                }
                            }
                            let mut cmd = sess.command_for(&exe);
                            if harness {
                                cmd.args(libtest_args);
                            } else if bench {
                                cmd.arg("--bench");
                            }
                            let tmp = sess.config().target_dir.join("tmp");
                            let _ = std::fs::create_dir_all(&tmp);
                            cmd.current_dir(&member.dir)
                                .env("CARGO_TARGET_TMPDIR", &tmp)
                                .stdin(Stdio::null());
                            apply_run_env(&mut cmd, member, sess);
                            let t = Instant::now();
                            match cmd.output() {
                                Ok(out) => {
                                    let mut text =
                                        String::from_utf8_lossy(&out.stdout).into_owned();
                                    text.push_str(&String::from_utf8_lossy(&out.stderr));
                                    let counts = parse_counts(&text);
                                    let mut result = TestResult {
                                        order,
                                        outcome: if out.status.success() {
                                            Outcome::Passed
                                        } else {
                                            Outcome::Failed
                                        },
                                        counts,
                                        output: text,
                                        took: t.elapsed(),
                                        flaky: Vec::new(),
                                    };
                                    if harness && args_ref.retries > 0 {
                                        retry_failed(&mut result, args_ref.retries, |names| {
                                            let mut again = sess.command_for(&exe);
                                            again.arg("--exact").args(names);
                                            if let Some(n) = args_ref.test_threads {
                                                again.arg("--test-threads").arg(n.to_string());
                                            }
                                            again
                                                .args(&args_ref.test_args)
                                                .current_dir(&member.dir)
                                                .env("CARGO_TARGET_TMPDIR", &tmp)
                                                .stdin(Stdio::null());
                                            apply_run_env(&mut again, member, sess);
                                            let out = again.output().ok()?;
                                            let mut text =
                                                String::from_utf8_lossy(&out.stdout).into_owned();
                                            text.push_str(&String::from_utf8_lossy(&out.stderr));
                                            Some(text)
                                        });
                                        result.took = t.elapsed();
                                    }
                                    result
                                }
                                Err(e) => TestResult {
                                    order,
                                    outcome: Outcome::Failed,
                                    counts: Counts::default(),
                                    output: format!("failed to execute test binary: {e}"),
                                    took: t.elapsed(),
                                    flaky: Vec::new(),
                                },
                            }
                        }
                    };
                    if matches!(result.outcome, Outcome::Failed) {
                        stop.store(true, Ordering::Relaxed);
                    }
                    if let (Outcome::Passed, Some(key), Some(cas)) =
                        (&result.outcome, &remember_as, test_cas)
                    {
                        let c = result.counts;
                        let _ = cas.put_value(
                            key,
                            &crate::test_cache::Record::new(
                                c.passed,
                                c.ignored,
                                c.filtered,
                                &result.output,
                            ),
                        );
                    }
                    let _ = res_tx.send(result);
                }
            });
        }

        // Printer thread: emits results in `order`.
        let printer = {
            let res_rx = res_rx.clone();
            let labels = &labels;
            let json = sess.json;
            scope.spawn(move || {
                let mut pending: BTreeMap<usize, TestResult> = BTreeMap::new();
                let mut next = 0usize;
                let mut counts = Counts::default();
                let mut failed: Vec<String> = Vec::new();
                let mut ran = 0usize;
                let mut suites: Vec<crate::test_report::Suite> = Vec::new();
                let mut flaky: Vec<String> = Vec::new();
                while next < total_slots {
                    let Ok(r) = res_rx.recv() else { break };
                    pending.insert(r.order, r);
                    while let Some(r) = pending.remove(&next) {
                        if !json {
                            print_result(&labels[next], &r, verbose);
                        }
                        if !matches!(r.outcome, Outcome::Skipped) {
                            ran += 1;
                            let suite = suite_of(&labels[next], &r);
                            if json {
                                // One line per binary, after the build's
                                // compiler messages, for tools.
                                println!(
                                    "{}",
                                    serde_json::json!({
                                        "reason": "test-suite-finished",
                                        "suite": suite.name,
                                        "outcome": match r.outcome {
                                            Outcome::Passed => "passed",
                                            Outcome::Cached => "cached",
                                            Outcome::Failed => "failed",
                                            Outcome::CompileFailed => "not-built",
                                            Outcome::Skipped => "skipped",
                                        },
                                        "seconds": suite.seconds,
                                        "passed": r.counts.passed,
                                        "failed": r.counts.failed,
                                        "ignored": r.counts.ignored,
                                        "tests": suite.cases,
                                        "error": suite.error,
                                    })
                                );
                            }
                            suites.push(suite);
                        }
                        counts.add(r.counts);
                        if matches!(r.outcome, Outcome::Failed | Outcome::CompileFailed) {
                            failed.push(labels[next].clone());
                        }
                        flaky.extend(r.flaky.iter().map(|t| format!("{t} ({})", labels[next])));
                        next += 1;
                    }
                }
                (counts, failed, ran, suites, flaky)
            })
        };

        // Main thread: drive the build, dispatch test binaries as they finish.
        let dispatched = std::cell::RefCell::new(vec![false; runnables.len()]);
        let dispatch = |idx: usize, artifacts: &[PathBuf], plan: &Plan, sess: &Session| {
            if std::mem::replace(&mut dispatched.borrow_mut()[idx], true) {
                return;
            }
            let r = &runnables[idx];
            let exe = find_executable(artifacts, sess, plan, r);
            match exe {
                Some(exe) if !args.no_run => {
                    let _ = job_tx.send(Job::Binary {
                        order: r.order,
                        exe,
                        member: r.member,
                        harness: r.harness,
                    });
                }
                Some(exe) => {
                    ui::action("built", exe.display());
                    let _ = res_tx.send(TestResult {
                        order: r.order,
                        outcome: Outcome::Skipped,
                        counts: Counts::default(),
                        output: String::new(),
                        took: Duration::ZERO,
                        flaky: Vec::new(),
                    });
                }
                None => {
                    let _ = res_tx.send(TestResult {
                        order: r.order,
                        outcome: Outcome::CompileFailed,
                        counts: Counts::default(),
                        output: "the build produced no executable".into(),
                        took: Duration::ZERO,
                        flaky: Vec::new(),
                    });
                }
            }
        };

        // A package's examples and binaries are built before its tests run,
        // as with Cargo, where everything is built first: a test may start
        // one (`target/debug/examples/...`). Other packages' tests still start
        // the moment they link.
        let companions: std::cell::RefCell<HashMap<usize, std::collections::HashSet<NodeId>>> =
            std::cell::RefCell::new(HashMap::new());
        for r in &plan.requested {
            if !is_runnable(r, bench)
                && !r.harness
                && matches!(r.kind, TargetKind::Example | TargetKind::Bin)
            {
                companions
                    .borrow_mut()
                    .entry(r.member)
                    .or_default()
                    .insert(r.node);
            }
        }
        let held: std::cell::RefCell<Vec<(usize, Vec<PathBuf>)>> = Default::default();
        let settle = |node: NodeId| {
            let mut c = companions.borrow_mut();
            for pending in c.values_mut() {
                pending.remove(&node);
            }
            c.retain(|_, pending| !pending.is_empty());
        };
        let may_run = |idx: usize| !companions.borrow().contains_key(&runnables[idx].member);
        let release_held = || {
            let ready: Vec<(usize, Vec<PathBuf>)> = {
                let mut h = held.borrow_mut();
                let (ready, rest): (Vec<_>, Vec<_>) = h.drain(..).partition(|(i, _)| may_run(*i));
                *h = rest;
                ready
            };
            for (idx, artifacts) in ready {
                dispatch(idx, &artifacts, &plan, &sess);
            }
        };

        let artifacts_seen: std::cell::RefCell<HashMap<NodeId, Vec<PathBuf>>> = Default::default();
        let scripts_seen: std::cell::RefCell<HashMap<NodeId, rune_common::BuildScriptOutput>> =
            Default::default();
        let doc_dispatched = std::cell::RefCell::new(vec![false; plan.doc_tests.len()]);
        let doc_slots: Vec<bool> = plan
            .doc_tests
            .iter()
            .map(|d| want_doctests && !args.no_run && has_doctests(&d.lib_source))
            .collect();
        // Doctests with nothing to run are settled immediately.
        for (i, wanted) in doc_slots.iter().enumerate() {
            if !wanted {
                doc_dispatched.borrow_mut()[i] = true;
                let _ = res_tx.send(TestResult {
                    order: doc_base + i,
                    outcome: Outcome::Skipped,
                    counts: Counts::default(),
                    output: String::new(),
                    took: Duration::ZERO,
                    flaky: Vec::new(),
                });
            }
        }
        let try_dispatch_docs = || {
            for (i, doc) in plan.doc_tests.iter().enumerate() {
                if doc_dispatched.borrow()[i] {
                    continue;
                }
                let seen = artifacts_seen.borrow();
                // Wait until every dependency has a finished, linkable artifact.
                let ready: Option<Vec<(String, PathBuf)>> = doc
                    .externs
                    .iter()
                    .map(|(name, node)| {
                        let unit = &plan.graph.node(*node).unit;
                        let want_dylib = unit.crate_type == rune_common::CrateType::ProcMacro;
                        seen.get(node)?
                            .iter()
                            .find(|p| {
                                let ext = p.extension().and_then(|e| e.to_str());
                                if want_dylib {
                                    matches!(ext, Some("so" | "dylib" | "dll"))
                                } else {
                                    ext == Some("rlib")
                                }
                            })
                            .map(|p| (name.clone(), p.clone()))
                    })
                    .collect();
                if let Some(externs) = ready {
                    doc_dispatched.borrow_mut()[i] = true;
                    let script = doc_script(&sess, &plan.graph, doc, &scripts_seen.borrow());
                    let _ = job_tx.send(Job::Doc {
                        order: doc_base + i,
                        doc: i,
                        externs,
                        script,
                    });
                }
            }
        };

        let (build, _failures) = drive(&sess, &plan.graph, |event| match event {
            PipelineEvent::ScriptOutput { node_id, output } => {
                scripts_seen.borrow_mut().insert(*node_id, output.clone());
            }
            PipelineEvent::BuildFinished {
                node_id, artifacts, ..
            }
            | PipelineEvent::CacheHit {
                node_id, artifacts, ..
            } => {
                artifacts_seen
                    .borrow_mut()
                    .insert(*node_id, artifacts.clone());
                settle(*node_id);
                if let Some(&idx) = by_node.get(node_id) {
                    if may_run(idx) {
                        dispatch(idx, artifacts, &plan, &sess);
                    } else {
                        held.borrow_mut().push((idx, artifacts.clone()));
                    }
                }
                release_held();
                try_dispatch_docs();
            }
            PipelineEvent::BuildFailed { node_id, .. } => {
                settle(*node_id);
                release_held();
                if let Some(&idx) = by_node.get(node_id)
                    && !std::mem::replace(&mut dispatched.borrow_mut()[idx], true)
                {
                    let _ = res_tx.send(TestResult {
                        order: runnables[idx].order,
                        outcome: Outcome::CompileFailed,
                        counts: Counts::default(),
                        output: "failed to compile (see the errors above)".into(),
                        took: Duration::ZERO,
                        flaky: Vec::new(),
                    });
                }
            }
            _ => {}
        })?;

        // Whatever still waited on a companion that never reported runs now.
        companions.borrow_mut().clear();
        release_held();

        // Anything never dispatched (a dependency failed first) is skipped.
        for (idx, done) in dispatched.borrow().iter().enumerate() {
            if !done {
                let _ = res_tx.send(TestResult {
                    order: runnables[idx].order,
                    outcome: Outcome::CompileFailed,
                    counts: Counts::default(),
                    output: "not built because a dependency failed to compile".into(),
                    took: Duration::ZERO,
                    flaky: Vec::new(),
                });
            }
        }

        // Doctests that never got their dependencies (the build failed) are
        // skipped.
        for (i, done) in doc_dispatched.borrow().iter().enumerate() {
            if !done {
                let _ = res_tx.send(TestResult {
                    order: doc_base + i,
                    outcome: Outcome::Skipped,
                    counts: Counts::default(),
                    output: String::new(),
                    took: Duration::ZERO,
                    flaky: Vec::new(),
                });
            }
        }
        drop(job_tx);
        drop(res_tx);

        let (counts, failed, ran, suites, flaky) = printer
            .join()
            .map_err(|_| anyhow::anyhow!("printer panicked"))?;
        counts_total = counts;
        failed_labels = failed;
        binaries_run = ran;
        report_suites = suites;
        flaky_tests = flaky;
        built_ok = build.success;
        Ok(build)
    })?;

    let _ = build_result;
    if !built_ok {
        bail!("could not compile the test targets");
    }

    let elapsed = start.elapsed().as_secs_f64();
    if let Some(path) = &args.junit {
        std::fs::write(path, crate::test_report::junit(&report_suites))
            .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", path.display()))?;
    }
    if !flaky_tests.is_empty() {
        ui::warn(format!(
            "{} flaky test(s) failed, then passed when run again: {}",
            flaky_tests.len(),
            flaky_tests.join(", ")
        ));
    }
    // Where the time went, when there was time to spend.
    let mut slowest: Vec<&crate::test_report::Suite> =
        report_suites.iter().filter(|s| s.seconds > 0.0).collect();
    slowest.sort_by(|a, b| b.seconds.total_cmp(&a.seconds));
    if elapsed >= 5.0 && slowest.len() > 1 && !args.common.quiet {
        ui::hint(format!(
            "slowest: {}",
            slowest
                .iter()
                .take(3)
                .map(|s| format!("{} ({:.1}s)", s.name, s.seconds))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !failed_labels.is_empty() {
        eprintln!();
        ui::error(format!(
            "{} failed: {}",
            if failed_labels.len() == 1 {
                "1 test binary"
            } else {
                "test binaries"
            },
            failed_labels.join(", ")
        ));
        ui::detail(format!(
            "{} passed · {} failed · {} ignored · {:.2}s",
            counts_total.passed, counts_total.failed, counts_total.ignored, elapsed
        ));
        // Cargo exits with 101 when tests fail; scripts rely on that.
        std::process::exit(101);
    }
    if args.no_run {
        return Ok(());
    }
    ui::summary(format!(
        "{} passed{} · {} · {:.2}s",
        counts_total.passed,
        if counts_total.ignored > 0 {
            format!(" · {} ignored", counts_total.ignored)
        } else {
            String::new()
        },
        if binaries_run == 1 {
            "1 test binary".to_string()
        } else {
            format!("{binaries_run} test binaries")
        },
        elapsed
    ));
    Ok(())
}

fn warn_cross(sess: &Session) {
    if let Some(t) = &sess.target {
        ui::warn(format!(
            "testing with --target {t}: test binaries are executed directly on this machine"
        ));
    }
}

fn is_runnable(r: &Requested, bench: bool) -> bool {
    match r.kind {
        TargetKind::Lib | TargetKind::Bin => r.harness,
        TargetKind::Test => !bench,
        TargetKind::Bench => bench,
        // Only an example that asks for it (`test = true`) is run.
        TargetKind::Example => r.harness && !bench,
    }
}

fn label_for(sess: &Session, r: &Requested) -> String {
    let pkg = &sess.ws.members[r.member].name;
    match r.kind {
        TargetKind::Lib => format!("{pkg} (unit tests: lib)"),
        TargetKind::Bin => format!("{pkg} (unit tests: bin {})", r.target),
        TargetKind::Test => format!("{pkg} (test {})", r.target),
        TargetKind::Bench => format!("{pkg} (bench {})", r.target),
        TargetKind::Example => format!("{pkg} (example {})", r.target),
    }
}

/// Arguments handed to every libtest harness binary.
fn libtest_args(args: &TestArgs, bench: bool) -> Vec<String> {
    let mut out = Vec::new();
    if bench {
        out.push("--bench".to_string());
    }
    if let Some(filter) = &args.filter {
        out.push(filter.clone());
    }
    if let Some(n) = args.test_threads {
        out.push("--test-threads".into());
        out.push(n.to_string());
    }
    out.extend(args.test_args.iter().cloned());
    out
}

fn find_executable(
    artifacts: &[PathBuf],
    sess: &Session,
    plan: &Plan,
    r: &Runnable,
) -> Option<PathBuf> {
    // The harness binary is the artifact without an extension (`.d`, `.rmeta`
    // and `.rlib` are never executable).
    let found = artifacts.iter().find(|p| {
        matches!(
            p.extension().and_then(|e| e.to_str()),
            None | Some("exe" | "wasm" | "js")
        )
    });
    if let Some(p) = found {
        return Some(p.clone());
    }
    // Fall back to the conventional location.
    let unit = &plan.graph.node(r.exe_dir_key).unit;
    let dir = out_dir_for(sess.config(), &sess.profile, unit);
    let guess = dir.join(unit.executable_name(&format!(
        "{}-{}",
        unit.rustc_crate_name(),
        unit.metadata_hash()
    )));
    guess.exists().then_some(guess)
}

/// Runs the failed tests of a failed binary again, by exact name, up to
/// `retries` times (`rerun` returns what a run printed). A test that passes
/// on a later attempt is flaky; when every failure turns out flaky, the
/// binary passed. A binary that failed without a failing test (a crash) is
/// not retried.
fn retry_failed(
    result: &mut TestResult,
    retries: u32,
    rerun: impl Fn(&[String]) -> Option<String>,
) {
    use crate::test_report::{Status, parse_cases};
    if !matches!(result.outcome, Outcome::Failed) {
        return;
    }
    let mut failing: Vec<String> = parse_cases(&result.output)
        .into_iter()
        .filter(|c| c.status == Status::Failed)
        .map(|c| c.name)
        .collect();
    if failing.is_empty() {
        return;
    }
    let first_failures = failing.len() as u64;
    for attempt in 1..=retries {
        let Some(text) = rerun(&failing) else { break };
        let cases = parse_cases(&text);
        let still: Vec<String> = failing
            .iter()
            .filter(|n| {
                !cases
                    .iter()
                    .any(|c| &c.name == *n && c.status == Status::Passed)
            })
            .cloned()
            .collect();
        result
            .flaky
            .extend(failing.iter().filter(|n| !still.contains(n)).cloned());
        result.output.push_str(&format!(
            "\n--- retry {attempt}: {} failed test(s) run again ---\n{text}",
            failing.len()
        ));
        failing = still;
        if failing.is_empty() {
            break;
        }
    }
    let recovered = first_failures - failing.len() as u64;
    result.counts.failed = failing.len() as u64;
    result.counts.passed += recovered;
    if failing.is_empty() {
        result.outcome = Outcome::Passed;
    }
}

/// A binary's result for reports: its tests (the last run of each counts,
/// so a flaky test shows as passed) or, when it could not run, an error.
fn suite_of(label: &str, r: &TestResult) -> crate::test_report::Suite {
    let mut cases: Vec<crate::test_report::Case> = Vec::new();
    for c in crate::test_report::parse_cases(&r.output) {
        match cases.iter_mut().find(|k| k.name == c.name) {
            Some(k) => {
                k.status = c.status;
                if !c.output.is_empty() {
                    k.output = c.output;
                }
            }
            None => cases.push(c),
        }
    }
    for c in cases.iter_mut() {
        c.flaky = r.flaky.contains(&c.name);
    }
    let error = match r.outcome {
        Outcome::CompileFailed => Some(r.output.clone()),
        Outcome::Failed
            if !cases
                .iter()
                .any(|c| c.status == crate::test_report::Status::Failed) =>
        {
            Some(r.output.clone())
        }
        _ => None,
    };
    crate::test_report::Suite {
        name: label.to_string(),
        seconds: r.took.as_secs_f64(),
        error,
        cases,
    }
}

fn print_result(label: &str, r: &TestResult, verbose: bool) {
    let cached = matches!(r.outcome, Outcome::Cached);
    let took = if cached {
        "cached".to_string()
    } else {
        format!("{:.2}s", r.took.as_secs_f64())
    };
    match r.outcome {
        Outcome::Passed | Outcome::Cached => {
            let c = r.counts;
            ui::action(
                "ok",
                format!(
                    "{} · {} passed{} · {}",
                    label.bold(),
                    c.passed,
                    if c.ignored > 0 {
                        format!(", {} ignored", c.ignored)
                    } else {
                        String::new()
                    },
                    took.dimmed()
                ),
            );
            if verbose && !r.output.is_empty() {
                println!("{}", r.output);
            }
        }
        Outcome::Failed => {
            ui::error(format!("FAILED {} · {}", label.bold(), took));
            println!("{}", r.output);
        }
        Outcome::CompileFailed => {
            ui::error(format!("could not build {}: {}", label.bold(), r.output));
        }
        Outcome::Skipped => {}
    }
}

// ---------------------------------------------------------------------------
// Doctests
// ---------------------------------------------------------------------------

/// Whether any source file under `dir/src` could contain a documentation test:
/// a fenced code block inside a doc comment, or a doc attribute that pulls in
/// a file (`#![doc = include_str!("../README.md")]`). Most crates have none,
/// and starting `rustdoc` just to learn that costs tens of milliseconds each.
/// Whether a source file may contain documentation tests. Only a filter
/// that spares rustdoc runs with nothing to do, so it errs toward yes: a
/// missed test is worse than a needless rustdoc.
fn may_hold_doctests(text: &str) -> bool {
    // Docs in attributes can span lines (`#![cfg_attr(feature = "std", doc =
    // r#"...```rust ...```"#)]`, `#![doc = include_str!("../README.md")]`).
    let doc_attribute = text.contains("doc =") || text.contains("doc=");
    text.lines().any(|line| {
        let t = line.trim_start();
        ((t.starts_with("///") || t.starts_with("//!") || t.starts_with('*')) && t.contains("```"))
            || (doc_attribute && (t.contains("```") || t.contains("include_str!")))
    })
}

fn has_doctests(lib_source: &std::path::Path) -> bool {
    fn scan(dir: &std::path::Path) -> bool {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // Where the library's root is the package directory itself
                // (`[lib] path = "lib.rs"`), skip what is not its source.
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with('.')
                    || matches!(name.as_ref(), "target" | "tests" | "examples" | "benches")
                {
                    continue;
                }
                if scan(&path) {
                    return true;
                }
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs")
                && let Ok(text) = std::fs::read_to_string(&path)
                && may_hold_doctests(&text)
            {
                return true;
            }
        }
        false
    }
    // The directory of the library's root file: `src/` usually, the package
    // directory for crates like `fnv` whose `lib.rs` sits at the top.
    lib_source.parent().is_some_and(scan)
}

/// What a library's build scripts give its doc tests, as they give the
/// library itself: its `cfg`s, environment and `OUT_DIR`, and the native
/// library directories of everything below it.
#[derive(Default)]
struct DocScript {
    args: Vec<String>,
    env: Vec<(String, String)>,
}

fn doc_script(
    sess: &Session,
    graph: &rune_graph::BuildGraph,
    doc: &DocTest,
    scripts: &HashMap<NodeId, rune_common::BuildScriptOutput>,
) -> DocScript {
    let mut out = DocScript::default();
    let Some(&(_, lib)) = doc.externs.iter().find(|(n, _)| *n == doc.crate_name) else {
        return out;
    };
    let unit = &graph.node(lib).unit;
    if unit.build_script.is_some() {
        let key = unit
            .ext
            .script_owner
            .clone()
            .unwrap_or_else(|| rune_scheduler::executor::script_key(unit));
        let dir = sess
            .config()
            .build_script_out_dir_by_key(sess.profile.output_dir_name(), &key);
        out.env.push(("OUT_DIR".into(), dir.display().to_string()));
    }
    if let Some(own) = scripts.get(&lib) {
        for cfg in &own.cfgs {
            out.args.extend(["--cfg".to_string(), cfg.clone()]);
        }
        out.env.extend(own.env_vars.iter().cloned());
    }
    // Native search paths of the library and everything it links.
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![lib];
    while let Some(id) = stack.pop() {
        let node = graph.node(id);
        if !seen.insert(id)
            || (id != lib && node.unit.crate_type == rune_common::CrateType::ProcMacro)
        {
            continue;
        }
        if let Some(o) = scripts.get(&id) {
            for ls in &o.link_search {
                let arg = match &ls.kind {
                    Some(kind) => format!("{kind}={}", ls.path.display()),
                    None => ls.path.display().to_string(),
                };
                if !out.args.windows(2).any(|w| w[0] == "-L" && w[1] == arg) {
                    out.args.extend(["-L".to_string(), arg]);
                }
            }
        }
        stack.extend(
            node.dependencies
                .iter()
                .filter(|d| node.normal_dependencies.contains(d)),
        );
    }
    out
}

fn run_doctest(
    sess: &Session,
    doc: &DocTest,
    externs: &[(String, PathBuf)],
    script: &DocScript,
    args: &TestArgs,
) -> TestResult {
    let start = Instant::now();
    let member = &sess.ws.members[doc.member];
    let rustdoc = rune_common::toolchain::global().rustdoc.clone();
    let deps_dir = sess.config().deps_dir(sess.profile.output_dir_name());

    let mut cmd = Command::new(&rustdoc);
    cmd.args(sess.rustdoc_flags());
    // Relative to the package, as Cargo passes it: doc tests are then named
    // `src/lib.rs - Ident (line 937)`, what `--retries`, filters and CI
    // reports expect.
    cmd.current_dir(&member.dir)
        .arg("--test")
        .arg(
            doc.lib_source
                .strip_prefix(&member.dir)
                .unwrap_or(&doc.lib_source),
        )
        .arg("--crate-name")
        .arg(&doc.crate_name)
        .arg("--edition")
        .arg(doc.edition.as_str())
        .args(if doc.proc_macro {
            &["--crate-type", "proc-macro", "--extern", "proc_macro"][..]
        } else {
            &[][..]
        })
        .arg("-L")
        .arg(format!("dependency={}", deps_dir.display()));
    if let Some(target) = &sess.target {
        cmd.arg("--target").arg(target);
    }
    for f in &doc.features {
        cmd.arg("--cfg").arg(format!("feature=\"{f}\""));
    }
    cmd.args(&script.args);
    for (name, path) in externs {
        cmd.arg("--extern")
            .arg(format!("{name}={}", path.display()));
    }
    let mut test_args: Vec<String> = Vec::new();
    if let Some(filter) = &args.filter {
        test_args.push(filter.clone());
    }
    if let Some(n) = args.test_threads {
        test_args.push(format!("--test-threads={n}"));
    }
    test_args.extend(args.test_args.iter().cloned());
    for a in &test_args {
        cmd.arg("--test-args").arg(a);
    }
    if let Ok(extra) = std::env::var("RUSTDOCFLAGS") {
        cmd.args(extra.split_whitespace());
    }
    apply_run_env(&mut cmd, member, sess);
    cmd.env("CARGO_CRATE_NAME", &doc.crate_name);
    cmd.envs(script.env.iter().map(|(k, v)| (k, v)));
    cmd.stdin(Stdio::null());

    match cmd.output() {
        Ok(out) => {
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            let counts = parse_counts(&text);
            TestResult {
                order: 0,
                outcome: if out.status.success() {
                    Outcome::Passed
                } else {
                    Outcome::Failed
                },
                counts,
                output: text,
                took: start.elapsed(),
                flaky: Vec::new(),
            }
        }
        Err(e) => TestResult {
            order: 0,
            outcome: Outcome::CompileFailed,
            counts: Counts::default(),
            output: format!("failed to run `{}`: {e}", rustdoc.display()),
            took: start.elapsed(),
            flaky: Vec::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_libtest_summary() {
        let out = "running 5 tests\n...\ntest result: ok. 3 passed; 0 failed; 1 ignored; 0 measured; 2 filtered out; finished in 0.01s\n";
        assert_eq!(
            parse_counts(out),
            Counts {
                passed: 3,
                failed: 0,
                ignored: 1,
                filtered: 2
            }
        );
    }

    #[test]
    fn sums_multiple_summaries_and_failures() {
        let out = "test result: FAILED. 1 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n\
                   test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";
        let c = parse_counts(out);
        assert_eq!((c.passed, c.failed), (5, 2));
    }

    #[test]
    fn detects_packages_that_have_doctests() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("lib.rs"), "/// plain docs\npub fn f() {}\n").unwrap();
        assert!(!has_doctests(&src.join("lib.rs")));

        std::fs::write(
            src.join("lib.rs"),
            "/// ```\n/// assert!(true);\n/// ```\npub fn f() {}\n",
        )
        .unwrap();
        assert!(has_doctests(&src.join("lib.rs")));

        std::fs::write(
            src.join("lib.rs"),
            "#![doc = include_str!(\"../README.md\")]\n",
        )
        .unwrap();
        assert!(
            has_doctests(&src.join("lib.rs")),
            "README-driven docs may contain tests"
        );

        std::fs::create_dir_all(src.join("nested")).unwrap();
        std::fs::write(src.join("lib.rs"), "mod nested;\n").unwrap();
        std::fs::write(src.join("nested/mod.rs"), "//! ```\n//! ```\n").unwrap();
        assert!(
            has_doctests(&src.join("lib.rs")),
            "nested modules are scanned"
        );

        // Docs in a `cfg_attr`, over several lines (`fnv`).
        std::fs::write(
            src.join("lib.rs"),
            "#![cfg_attr(feature = \"std\", doc = r#\"\n```rust\nassert!(true);\n```\n\"#)]\n",
        )
        .unwrap();
        assert!(has_doctests(&src.join("lib.rs")));

        // A library whose root is at the top of the package, as in `fnv`.
        let top = tempfile::tempdir().unwrap();
        std::fs::write(top.path().join("lib.rs"), "//! ```\n//! ```\n").unwrap();
        std::fs::create_dir_all(top.path().join("target")).unwrap();
        assert!(has_doctests(&top.path().join("lib.rs")));
    }

    #[test]
    fn ignores_unrelated_output() {
        assert_eq!(parse_counts("hello\ntest foo ... ok\n"), Counts::default());
    }
}
