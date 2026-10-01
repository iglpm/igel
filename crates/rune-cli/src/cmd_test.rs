//! `rune test` and `rune bench`.
//!
//! Test binaries are nodes in the same build graph as everything else, so the
//! scheduler builds them as soon as their dependencies are ready and this
//! module queues each one **the moment it finishes linking** — testing starts
//! while the rest of the graph is still compiling, instead of after a full
//! build.
//!
//! One test process runs at a time by default, as with Cargo (tests in
//! different binaries may share ports, files or a database); `test.jobs`
//! allows more. Each binary's result is printed as soon as it is in, with a
//! live line below showing what runs and how many tests passed so far;
//! reports (`--junit`, JSON) list binaries in a fixed order (package, then
//! library, binaries, integration tests, doctests).

use anyhow::{Result, bail};
use colored::Colorize;
use crossbeam_channel::{Receiver, Sender, unbounded};
use rune_common::TargetKind;
use rune_graph::NodeId;
use rune_scheduler::PipelineEvent;
use rune_scheduler::executor::out_dir_for;
use std::collections::HashMap;
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

/// How `rune coverage` changes a test run.
pub struct Instrument {
    /// Flags for the workspace's own crates (not dependencies, not build
    /// scripts or proc-macros).
    pub rustflags: Vec<String>,
    /// Environment for every test program.
    pub env: Vec<(String, String)>,
}

/// What a test run did.
pub struct RunOutcome {
    pub failed: bool,
    /// Every program the run built from the workspace's crates: test
    /// binaries, and the binaries tests may start.
    pub executables: Vec<PathBuf>,
}

pub fn run(args: TestArgs, mode: Mode) -> Result<()> {
    if run_with(args, mode, None)?.failed {
        // Cargo exits with 101 when tests fail; scripts rely on that.
        std::process::exit(101);
    }
    Ok(())
}

pub fn run_with(args: TestArgs, mode: Mode, instrument: Option<&Instrument>) -> Result<RunOutcome> {
    let bench = mode == Mode::Bench;
    let mut sess = Session::new(&args.common, if bench { "bench" } else { "test" })?;
    if let Some(i) = instrument {
        sess.run_env = i.env.clone();
    }

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
    let mut plan = graph::plan(
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
    if let Some(i) = instrument {
        let ids: Vec<NodeId> = plan.graph.nodes().iter().map(|n| n.id).collect();
        for id in ids {
            let unit = &mut plan.graph.node_mut(id).unit;
            if unit.ext.member && !unit.ext.for_host && !unit.is_build_script {
                unit.ext.rustflags.extend(i.rustflags.iter().cloned());
            }
        }
    }
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
        return Ok(RunOutcome {
            failed: false,
            executables: Vec::new(),
        });
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
            /// `--schedule tests`: only these tests, one batch of the binary.
            batch: Option<Batch>,
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

    // `--schedule tests`: binaries are split into batches of tests, which the
    // runners take from one queue. How many batches each binary was split
    // into, for the printer to put its results back together.
    let schedule = args
        .schedule
        .clone()
        .or_else(|| sess.ws.config.settings.test_schedule())
        .unwrap_or_else(|| "binaries".into());
    if !matches!(schedule.as_str(), "binaries" | "tests") {
        anyhow::bail!("unknown test schedule `{schedule}`: expected `binaries` or `tests`");
    }
    let split = schedule == "tests" && !bench && splittable(&args.test_args);
    // One test process at a time unless asked otherwise, as with Cargo: the
    // tests of different binaries may share a port, a file or a database,
    // and running two binaries at once made such suites fail now and then.
    // The build goes on meanwhile, so the next binary is ready when the
    // runner is. Split batches are one binary's tests, which libtest runs
    // in parallel anyway.
    // `--timeout` / `test.timeout`: how long one test process may run.
    let timeout: Option<Duration> = match &args.timeout {
        Some(t) => {
            let secs = rune_common::time::parse_duration(t).ok_or_else(|| {
                anyhow::anyhow!(
                    "invalid --timeout `{t}`: expected a duration such as `90s` or `10m`"
                )
            })?;
            (secs > 0).then(|| Duration::from_secs(secs))
        }
        None => sess
            .ws
            .config
            .settings
            .test_timeout()
            .map(Duration::from_secs),
    };
    let runner_count = args
        .test_jobs
        .or_else(|| sess.ws.config.settings.test_jobs())
        .unwrap_or(if split { sess.jobs } else { 1 })
        .clamp(1, 64);
    let progress = crate::test_progress::TestProgress::new(total_slots, !sess.quiet && !sess.json);
    let process_ids = std::sync::atomic::AtomicUsize::new(0);
    // How long each binary ran last time: the scheduler builds the ones the
    // run will wait longest on first (and the programs their package's tests
    // wait for), so they start while the rest still compiles.
    let times_path = sess.config().fingerprint_dir.join("test-times.json");
    let mut run_times: HashMap<String, f64> = std::fs::read_to_string(&times_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    if !args.no_run {
        let mut costs: HashMap<NodeId, f64> = HashMap::new();
        for r in &runnables {
            let Some(&secs) = run_times.get(&r.label) else {
                continue;
            };
            let mut add = |node: NodeId| {
                let c = costs.entry(node).or_default();
                *c = c.max(secs);
            };
            add(r.exe_dir_key);
            for c in plan.requested.iter().filter(|c| {
                c.member == r.member
                    && !is_runnable(c, bench)
                    && matches!(c.kind, TargetKind::Example | TargetKind::Bin)
            }) {
                add(c.node);
            }
        }
        sess.extra_costs = costs;
    }
    let parts_of: std::sync::Mutex<HashMap<usize, usize>> = Default::default();
    let executables: std::cell::RefCell<Vec<PathBuf>> = Default::default();
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
            let progress = &progress;
            let process_ids = &process_ids;
            let labels = &labels;
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
                            let id = process_ids.fetch_add(1, Ordering::Relaxed);
                            progress.begin(id, &labels[order]);
                            let r = run_doctest(
                                sess,
                                &plan_ref.doc_tests[doc],
                                &externs,
                                &script,
                                args_ref,
                                &|line| progress.line(id, line),
                                timeout,
                            );
                            progress.end(id);
                            TestResult { order, ..r }
                        }
                        Job::Binary {
                            order,
                            exe,
                            member,
                            harness,
                            batch,
                        } => {
                            let run_args: Vec<String> = match &batch {
                                Some(b) => batch_args(args_ref, &b.tests),
                                None => libtest_args.clone(),
                            };
                            let libtest_args = &run_args;
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
                            let id = process_ids.fetch_add(1, Ordering::Relaxed);
                            progress.begin(id, &labels[order]);
                            let ran = run_streaming(
                                &mut cmd,
                                &|line| progress.line(id, line),
                                timeout,
                            );
                            progress.end(id);
                            match ran {
                                Ok(out) => {
                                    let mut text = out.stdout;
                                    text.push_str(&out.stderr);
                                    let mut unfinished: Vec<String> = Vec::new();
                                    if out.timed_out {
                                        if harness {
                                            // Which tests never reported: the
                                            // binary's list minus those done.
                                            let mut list = sess.command_for(&exe);
                                            list.args(libtest_args)
                                                .args(["--list", "--format", "terse"])
                                                .current_dir(&member.dir)
                                                .stdin(Stdio::null());
                                            apply_run_env(&mut list, member, sess);
                                            let done: std::collections::HashSet<String> =
                                                crate::test_report::parse_cases(&text)
                                                    .into_iter()
                                                    .map(|c| c.name)
                                                    .collect();
                                            if let Ok(listed) = run_streaming(
                                                &mut list,
                                                &|_| {},
                                                Some(Duration::from_secs(30)),
                                            ) {
                                                unfinished = listed
                                                    .stdout
                                                    .lines()
                                                    .filter_map(|l| l.strip_suffix(": test"))
                                                    .filter(|n| !done.contains(*n))
                                                    .map(str::to_string)
                                                    .collect();
                                            }
                                        }
                                        text.push_str(&timed_out_note(&text, timeout, &unfinished));
                                    } else if let Some(why) = killed_by(&out.status) {
                                        // libtest never got to report: say
                                        // what ended it, as Cargo does.
                                        text.push_str(&format!(
                                            "\nthe test binary was killed by {why} before it finished \
                                             (a test aborted, overflowed its stack or was killed)\n"
                                        ));
                                    }
                                    let mut counts = parse_counts(&text);
                                    let crashed = !out.timed_out
                                        && killed_by(&out.status).is_some()
                                        && !text.contains("test result:");
                                    if crashed {
                                        // No summary: count what finished, and
                                        // the test that took the binary down.
                                        use crate::test_report::Status;
                                        let cases = crate::test_report::parse_cases(&text);
                                        let n = |s: Status| {
                                            cases.iter().filter(|c| c.status == s).count() as u64
                                        };
                                        counts.passed = n(Status::Passed);
                                        counts.ignored = n(Status::Ignored);
                                        counts.failed = n(Status::Failed) + 1;
                                    }
                                    if out.timed_out {
                                        // libtest never printed its summary:
                                        // count what finished, and each test
                                        // that did not as failed.
                                        use crate::test_report::Status;
                                        let cases = crate::test_report::parse_cases(&text);
                                        let n = |s: Status| {
                                            cases.iter().filter(|c| c.status == s).count() as u64
                                        };
                                        counts.passed = n(Status::Passed);
                                        counts.ignored = n(Status::Ignored);
                                        counts.failed =
                                            n(Status::Failed) + unfinished.len().max(1) as u64;
                                    }
                                    if let Some(b) = &batch {
                                        // Each batch reports the rest of the
                                        // binary as filtered out.
                                        counts.filtered = b.filtered;
                                    }
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
                                            let out =
                                                run_streaming(&mut again, &|_| {}, timeout).ok()?;
                                            let mut text = out.stdout;
                                            text.push_str(&out.stderr);
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

        // Printer thread: prints each binary's result the moment it is in
        // (a failure shows while the rest still runs); reports list them in
        // `order`.
        let printer = {
            let res_rx = res_rx.clone();
            let parts_of = &parts_of;
            let labels = &labels;
            let progress = &progress;
            let json = sess.json;
            scope.spawn(move || {
                let mut settled = 0usize;
                let mut counts = Counts::default();
                let mut failed: Vec<(usize, String)> = Vec::new();
                let mut ran = 0usize;
                let mut suites: Vec<(usize, crate::test_report::Suite)> = Vec::new();
                let mut flaky: Vec<String> = Vec::new();
                let mut partial: HashMap<usize, (usize, TestResult)> = HashMap::new();
                while settled < total_slots {
                    let Ok(r) = res_rx.recv() else { break };
                    let parts = parts_of.lock().unwrap().get(&r.order).copied().unwrap_or(1);
                    let r = if parts > 1 {
                        let order = r.order;
                        let merged = match partial.remove(&order) {
                            Some((n, acc)) => (n + 1, merge(acc, r)),
                            None => (1, r),
                        };
                        if merged.0 < parts {
                            partial.insert(order, merged);
                            continue;
                        }
                        merged.1
                    } else {
                        r
                    };
                    settled += 1;
                    progress.slot_done(r.counts);
                    let label = &labels[r.order];
                    if !json {
                        print_result(label, &r, verbose);
                    }
                    if !matches!(r.outcome, Outcome::Skipped) {
                        ran += 1;
                        let suite = suite_of(label, &r);
                        if json {
                            // One line per binary, after the build's
                            // compiler messages, for tools.
                            ui::out(serde_json::json!({
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
                            }));
                        }
                        suites.push((r.order, suite));
                    }
                    counts.add(r.counts);
                    if matches!(r.outcome, Outcome::Failed | Outcome::CompileFailed) {
                        failed.push((r.order, label.clone()));
                    }
                    flaky.extend(r.flaky.iter().map(|t| format!("{t} ({label})")));
                }
                suites.sort_by_key(|(order, _)| *order);
                failed.sort();
                let suites: Vec<_> = suites.into_iter().map(|(_, s)| s).collect();
                let failed: Vec<_> = failed.into_iter().map(|(_, l)| l).collect();
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
            if let Some(e) = &exe {
                executables.borrow_mut().push(e.clone());
            }
            match exe {
                Some(exe) if !args.no_run => {
                    let batches = if split && r.harness {
                        split_into_batches(sess, &exe, r.member, &args, runner_count)
                    } else {
                        Vec::new()
                    };
                    if batches.len() > 1 {
                        parts_of.lock().unwrap().insert(r.order, batches.len());
                        for b in batches {
                            let _ = job_tx.send(Job::Binary {
                                order: r.order,
                                exe: exe.clone(),
                                member: r.member,
                                harness: true,
                                batch: Some(b),
                            });
                        }
                    } else {
                        let _ = job_tx.send(Job::Binary {
                            order: r.order,
                            exe,
                            member: r.member,
                            harness: r.harness,
                            batch: batches.into_iter().next(),
                        });
                    }
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
        // Doc tests run after every test binary, as with Cargo: a doc-test
        // run is often ready long before the binaries link, and run first
        // it would decide what `--no-fail-fast`-less runs report.
        let build_over = std::cell::Cell::new(false);
        let try_dispatch_docs = || {
            if !build_over.get() && !dispatched.borrow().iter().all(|d| *d) {
                return;
            }
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

        // Every binary is settled: the doc tests whose dependencies built go.
        build_over.set(true);
        try_dispatch_docs();

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
        progress.finish();
        counts_total = counts;
        failed_labels = failed;
        binaries_run = ran;
        report_suites = suites;
        flaky_tests = flaky;
        built_ok = build.success;
        Ok(build)
    })?;

    if !built_ok {
        bail!("could not compile the test targets");
    }
    let mut executables = executables.into_inner();
    // The workspace's own programs, which tests may start.
    for (id, paths) in &build_result.artifact_paths {
        let unit = &plan.graph.node(*id).unit;
        if unit.ext.member && !unit.ext.for_host && unit.crate_type == rune_common::CrateType::Bin {
            executables.extend(paths.iter().filter(|p| is_program(p)).cloned());
        }
    }
    executables.sort();
    executables.dedup();

    let elapsed = start.elapsed().as_secs_f64();
    // Remembered for the next run's scheduling (cached results ran nothing).
    let mut learned = false;
    for suite in report_suites.iter().filter(|s| s.seconds > 0.0) {
        run_times.insert(suite.name.clone(), suite.seconds);
        learned = true;
    }
    if learned && let Ok(text) = serde_json::to_string(&run_times) {
        let _ = rune_common::fsutil::write_atomic(&times_path, text.as_bytes());
    }
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
        ui::err("");
        ui::error(format!(
            "{} failed: {}",
            if failed_labels.len() == 1 {
                "1 test binary"
            } else {
                "test binaries"
            },
            failed_labels.join(", ")
        ));
        ui::err(format!(
            "  {}",
            format!(
                "{} passed · {} failed · {} ignored · {:.2}s",
                counts_total.passed, counts_total.failed, counts_total.ignored, elapsed
            )
            .dimmed()
        ));
        return Ok(RunOutcome {
            failed: true,
            executables,
        });
    }
    if args.no_run {
        return Ok(RunOutcome {
            failed: false,
            executables,
        });
    }
    ui::result(format!(
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
    if let Some(filter) = &args.filter
        && counts_total.passed + counts_total.failed + counts_total.ignored == 0
        && binaries_run > 0
    {
        ui::hint(format!(
            "no test matched `{filter}` ({} filtered out); `rune test -- --list` lists them",
            counts_total.filtered
        ));
    }
    Ok(RunOutcome {
        failed: false,
        executables,
    })
}

/// Why a test process that ran out of time has no result: how long it had,
/// and how far its tests got.
fn timed_out_note(output: &str, timeout: Option<Duration>, unfinished: &[String]) -> String {
    let limit = timeout.map(ui::secs).unwrap_or_default();
    let total: u64 = output
        .lines()
        .filter_map(|l| l.strip_prefix("running "))
        .filter_map(|rest| rest.split_whitespace().next()?.parse::<u64>().ok())
        .sum();
    let done = output
        .lines()
        .filter(|l| l.starts_with("test ") && l.contains(" ... "))
        .count();
    let progress = if total > 0 {
        format!(" ({done} of {total} tests had finished)")
    } else {
        String::new()
    };
    let mut note = format!(
        "\nstopped after {limit}: the test binary ran longer than the timeout \
         (`--timeout`, `test.timeout`){progress}\n"
    );
    if !unfinished.is_empty() {
        let shown: Vec<&str> = unfinished.iter().take(10).map(String::as_str).collect();
        let more = unfinished.len().saturating_sub(shown.len());
        note.push_str(&format!(
            "still running or not started: {}{}\n",
            shown.join(", "),
            if more > 0 {
                format!(" and {more} more")
            } else {
                String::new()
            }
        ));
    }
    note
}

/// `signal 6 (SIGABRT)` when a process was killed by a signal.
fn killed_by(status: &std::process::ExitStatus) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        let sig = status.signal()?;
        let name = match sig {
            libc::SIGABRT => "SIGABRT",
            libc::SIGSEGV => "SIGSEGV",
            libc::SIGKILL => "SIGKILL",
            libc::SIGBUS => "SIGBUS",
            libc::SIGILL => "SIGILL",
            libc::SIGFPE => "SIGFPE",
            libc::SIGTERM => "SIGTERM",
            libc::SIGINT => "SIGINT",
            _ => "",
        };
        Some(if name.is_empty() {
            format!("signal {sig}")
        } else {
            format!("signal {sig} ({name})")
        })
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        None
    }
}

/// What a test process printed and how it ended.
struct Captured {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
    /// Stopped for running longer than the timeout.
    timed_out: bool,
}

/// Runs `cmd` with its output captured, passing each line of its stdout to
/// `on_line` as it is printed. Unlike `Command::output`, it returns soon
/// after the process exits even when something the process started still
/// holds the pipes open (a server a test did not stop): what arrives in the
/// moment after the exit is kept, and the rest is not waited for. A process
/// still running after `timeout` is killed.
fn run_streaming(
    cmd: &mut Command,
    on_line: &dyn Fn(&str),
    timeout: Option<Duration>,
) -> std::io::Result<Captured> {
    use crossbeam_channel::RecvTimeoutError;
    use std::io::{BufRead, Read};
    enum Chunk {
        Out(Vec<u8>),
        Err(Vec<u8>),
        Closed,
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = spawn_retrying(cmd)?;
    let started = Instant::now();
    let mut timed_out = false;
    // Kills the process once it has had its time.
    let mut enforce = |child: &mut std::process::Child| {
        if !timed_out && timeout.is_some_and(|t| started.elapsed() >= t) {
            timed_out = true;
            let _ = child.kill();
        }
    };
    let (tx, rx) = unbounded::<Chunk>();
    if let Some(out) = child.stdout.take() {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut r = std::io::BufReader::new(out);
            loop {
                let mut line = Vec::new();
                match r.read_until(b'\n', &mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if tx.send(Chunk::Out(line)).is_err() {
                            return;
                        }
                    }
                }
            }
            let _ = tx.send(Chunk::Closed);
        });
    } else {
        let _ = tx.send(Chunk::Closed);
    }
    if let Some(mut err) = child.stderr.take() {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 16 * 1024];
            loop {
                match err.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(Chunk::Err(buf[..n].to_vec())).is_err() {
                            return;
                        }
                    }
                }
            }
            let _ = tx.send(Chunk::Closed);
        });
    } else {
        let _ = tx.send(Chunk::Closed);
    }
    drop(tx);

    let (mut out, mut err) = (Vec::new(), Vec::new());
    let mut open = 2;
    let mut exited: Option<(std::process::ExitStatus, Instant)> = None;
    while open > 0 {
        // Once the process is gone, only what is already in the pipes is
        // still to come: stop at the first quiet half second.
        let wait = if exited.is_some() {
            Duration::from_millis(500)
        } else {
            Duration::from_millis(100)
        };
        match rx.recv_timeout(wait) {
            Ok(Chunk::Out(line)) => {
                on_line(&String::from_utf8_lossy(&line));
                out.extend_from_slice(&line);
            }
            Ok(Chunk::Err(bytes)) => err.extend_from_slice(&bytes),
            Ok(Chunk::Closed) => open -= 1,
            Err(RecvTimeoutError::Timeout) if exited.is_some() => break,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        match exited {
            None => {
                if let Some(status) = child.try_wait()? {
                    exited = Some((status, Instant::now()));
                } else {
                    enforce(&mut child);
                }
            }
            // Something the test started keeps printing: enough.
            Some((_, at)) if at.elapsed() > Duration::from_secs(5) => break,
            Some(_) => {}
        }
    }
    // (It closed its output but runs on.)
    let status = match exited {
        Some((status, _)) => status,
        None => loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            enforce(&mut child);
            std::thread::sleep(Duration::from_millis(50));
        },
    };
    Ok(Captured {
        status,
        stdout: String::from_utf8_lossy(&out).into_owned(),
        stderr: String::from_utf8_lossy(&err).into_owned(),
        timed_out,
    })
}

/// `cmd.spawn()`, tried again for a moment while the executable is busy
/// (`ETXTBSY`): a binary that just finished linking still is when a process
/// forked in that instant holds it open until it execs.
fn spawn_retrying(cmd: &mut Command) -> std::io::Result<std::process::Child> {
    #[cfg(unix)]
    {
        let mut delay = Duration::from_millis(10);
        for _ in 0..8 {
            match cmd.spawn() {
                Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => {
                    std::thread::sleep(delay);
                    delay *= 2;
                }
                r => return r,
            }
        }
    }
    cmd.spawn()
}

/// A file a test can run: executable, and not a library or debug file.
fn is_program(p: &std::path::Path) -> bool {
    if matches!(
        p.extension().and_then(|e| e.to_str()),
        Some("rlib" | "rmeta" | "d" | "so" | "dylib" | "a" | "pdb" | "dSYM")
    ) {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        p.extension().is_some_and(|e| e == "exe")
    }
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

/// One batch of a test binary's tests (`--schedule tests`).
struct Batch {
    tests: Vec<String>,
    /// What the binary's own summary would call "filtered out", reported by
    /// the first batch only (so the parts add up to the whole binary).
    filtered: u64,
}

/// Libtest arguments `--schedule tests` knows how to carry over to a batch;
/// anything else (positional filters, `--format`, ...) runs whole binaries.
fn splittable(test_args: &[String]) -> bool {
    let mut it = test_args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--ignored" | "--include-ignored" | "--nocapture" | "--show-output" | "--exact" => {}
            "--skip" => {
                it.next();
            }
            _ => return false,
        }
    }
    true
}

/// Lists a harness binary's tests (honoring the filter, `--skip`,
/// `--ignored`) and splits them into batches small enough that every core
/// stays busy until the end of the run.
fn split_into_batches(
    sess: &Session,
    exe: &std::path::Path,
    member: usize,
    args: &TestArgs,
    cores: usize,
) -> Vec<Batch> {
    let list = |with_filter: bool| -> Option<Vec<String>> {
        let m = &sess.ws.members[member];
        let mut cmd = sess.command_for(exe);
        cmd.args(["--list", "--format", "terse"]);
        if with_filter && let Some(f) = &args.filter {
            cmd.arg(f);
        }
        cmd.args(&args.test_args)
            .current_dir(&m.dir)
            .stdin(Stdio::null());
        apply_run_env(&mut cmd, m, sess);
        let out = run_streaming(&mut cmd, &|_| {}, None)
            .ok()
            .filter(|o| o.status.success())?;
        Some(
            out.stdout
                .lines()
                .filter_map(|l| l.strip_suffix(": test"))
                .map(str::to_string)
                .collect(),
        )
    };
    let Some(tests) = list(true) else {
        return Vec::new();
    };
    if tests.is_empty() {
        return Vec::new();
    }
    let filtered = match &args.filter {
        Some(_) => list(false).map_or(0, |all| all.len().saturating_sub(tests.len())) as u64,
        None => 0,
    };
    // A few batches per core: a slow batch is balanced by the others.
    let size = tests.len().div_ceil(cores * 4).max(1);
    tests
        .chunks(size)
        .enumerate()
        .map(|(i, chunk)| Batch {
            tests: chunk.to_vec(),
            filtered: if i == 0 { filtered } else { 0 },
        })
        .collect()
}

/// The arguments that run exactly `tests` of a harness binary.
fn batch_args(args: &TestArgs, tests: &[String]) -> Vec<String> {
    let mut out = vec!["--exact".to_string()];
    // Each batch is one of many processes sharing the cores.
    out.push("--test-threads".into());
    out.push(args.test_threads.unwrap_or(1).to_string());
    let mut it = args.test_args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--skip" => {
                it.next();
            }
            "--exact" => {}
            _ => out.push(a.clone()),
        }
    }
    out.extend(tests.iter().cloned());
    out
}

/// Puts the results of two batches of one binary together.
fn merge(a: TestResult, b: TestResult) -> TestResult {
    use Outcome::*;
    let outcome = match (&a.outcome, &b.outcome) {
        (CompileFailed, _) | (_, CompileFailed) => CompileFailed,
        (Failed, _) | (_, Failed) => Failed,
        (Skipped, Skipped) => Skipped,
        (Cached, Cached) => Cached,
        _ => Passed,
    };
    let mut counts = a.counts;
    counts.add(b.counts);
    let mut output = a.output;
    if !b.output.is_empty() {
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(&b.output);
    }
    let mut flaky = a.flaky;
    flaky.extend(b.flaky);
    TestResult {
        order: a.order,
        outcome,
        counts,
        output,
        // The batches ran side by side: the longest is the binary's time.
        took: a.took.max(b.took),
        flaky,
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
                ui::out(&r.output);
            }
        }
        Outcome::Failed => {
            ui::error(format!("FAILED {} · {}", label.bold(), took));
            ui::out(&r.output);
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
    // Block doc comments (`/*! ... */`), whose lines need no leading `*`.
    if (text.contains("/*!") || text.contains("/**")) && text.contains("```") {
        return true;
    }
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
    on_line: &dyn Fn(&str),
    timeout: Option<Duration>,
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

    match run_streaming(&mut cmd, on_line, timeout) {
        Ok(out) => {
            let mut text = out.stdout;
            text.push_str(&out.stderr);
            if out.timed_out {
                text.push_str(&timed_out_note(&text, timeout, &[]));
            }
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

        // A block doc comment whose lines have no leading `*` (grep-searcher).
        std::fs::write(
            src.join("lib.rs"),
            "/*!\nDocs.\n\n```\nassert!(true);\n```\n*/\n",
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
