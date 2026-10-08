//! Runs every scenario for both tools on every workload, alternating tools between runs.

use anyhow::Result;
use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use crate::measure::{self, Sample};
use crate::results::{Measurement, Results};
use crate::scenario::{Kind, Needs, Scenario};
use crate::stats::{self, Summary};
use crate::tool::{Ctx, Tool};
use crate::workload::Workload;

pub struct Config {
    pub runs_heavy: usize,
    pub runs_light: usize,
    pub warmup: usize,
    pub timeout: Duration,
    pub work: PathBuf,
    pub verbose: bool,
    pub live: bool,
}

pub fn run(
    tools: &[Tool],
    workloads: &[Workload],
    scenarios: &[&Scenario],
    cfg: &Config,
    results: &mut Results,
    on_workload_done: &mut dyn FnMut(&Results),
) -> Result<()> {
    for w in workloads {
        eprintln!("\n▸ {} — {}", w.name, w.description);
        let mut ctxs: Vec<Ctx> = Vec::new();
        for t in tools {
            let root = cfg.work.join(&w.name).join(&t.name);
            crate::tool::remove(&root)?;
            let dir = root.join("src");
            w.materialize(&dir)?;
            ctxs.push(Ctx {
                tool: t,
                dir,
                home: root.join("home"),
            });
        }
        for s in scenarios {
            let missing = match s.needs {
                Needs::Nothing => None,
                Needs::LeafEdit => w
                    .leaf_edit
                    .is_none()
                    .then_some("a file to edit (--edit-leaf)"),
                Needs::CoreEdit => w
                    .core_edit
                    .is_none()
                    .then_some("a core file to edit (--edit-core)"),
                Needs::TestEdit => w
                    .test_edit
                    .is_none()
                    .then_some("integration test to edit (--edit-test)"),
            };
            if let Some(why) = missing {
                eprintln!("  · {:<16} skipped: the workload names no {why}", s.id);
                continue;
            }
            let found = measure_scenario(&ctxs, w, s, cfg);
            results.measurements.extend(found);
            print_line(results, &w.name, s);
        }
        on_workload_done(results);
    }
    Ok(())
}

fn measure_scenario(ctxs: &[Ctx], w: &Workload, s: &Scenario, cfg: &Config) -> Vec<Measurement> {
    let (warmup, runs) = match (s.kind, s.heavy) {
        (Kind::Disk, _) => (0, 1),
        (_, true) => (0, cfg.runs_heavy),
        (_, false) => (cfg.warmup, cfg.runs_light),
    };
    struct Acc {
        samples: Vec<Sample>,
        values: Vec<f64>,
        failures: usize,
        note: Option<String>,
        project_bytes: Option<u64>,
        broken: bool,
    }
    let mut accs: Vec<Acc> = ctxs
        .iter()
        .map(|ctx| {
            let prepared = (s.prepare)(ctx, w);
            Acc {
                samples: Vec::new(),
                values: Vec::new(),
                failures: 0,
                note: prepared.as_ref().err().map(|e| format!("{e:#}")),
                project_bytes: None,
                broken: prepared.is_err(),
            }
        })
        .collect();

    for i in 0..warmup + runs {
        let order: Vec<usize> = if i % 2 == 0 {
            (0..ctxs.len()).collect()
        } else {
            (0..ctxs.len()).rev().collect()
        };
        for t in order {
            let (ctx, acc) = (&ctxs[t], &mut accs[t]);
            if acc.broken {
                continue;
            }
            if let Err(e) = (s.before)(ctx, w, i) {
                acc.note = Some(format!("{e:#}"));
                acc.broken = true;
                continue;
            }
            let label = if i < warmup {
                "warm-up".to_string()
            } else {
                format!("run {}/{runs}", i - warmup + 1)
            };
            if cfg.live {
                eprint!("\r  · {:<16} {:<6} {label:<10}", s.id, ctx.tool.name);
                let _ = std::io::stderr().flush();
            }
            let outcome = match measure::run(&mut ctx.tool.command(ctx, s.args), cfg.timeout) {
                Ok(o) => o,
                Err(e) => {
                    acc.note = Some(format!("{e:#}"));
                    acc.broken = true;
                    continue;
                }
            };
            if cfg.live {
                eprint!(" {:>10}", stats::seconds(outcome.sample.wall));
            }
            if i < warmup {
                continue;
            }
            if !outcome.sample.ok {
                acc.failures += 1;
                acc.note = Some(tail(&outcome.output, 15));
                if cfg.verbose {
                    eprintln!("\n{}", outcome.output);
                }
                continue;
            }
            acc.samples.push(outcome.sample);
            match s.kind {
                Kind::Time => acc.values.push(outcome.sample.wall),
                Kind::Disk => {
                    let (project, total) = ctx.tool.disk_use(ctx);
                    acc.values.push(total as f64);
                    acc.project_bytes = Some(project);
                }
            }
        }
    }
    if cfg.live {
        eprint!("\r{:<60}\r", "");
    }

    ctxs.iter()
        .zip(accs)
        .map(|(ctx, acc)| {
            let cpu: Vec<f64> = acc.samples.iter().map(Sample::cpu).collect();
            Measurement {
                workload: w.name.clone(),
                scenario: s.id.to_string(),
                category: s.category.to_string(),
                title: s.title.to_string(),
                about: s.about.to_string(),
                tool: ctx.tool.name.clone(),
                unit: match s.kind {
                    Kind::Time => "s".into(),
                    Kind::Disk => "bytes".into(),
                },
                summary: Summary::of(&acc.values),
                cpu: Summary::of(&cpu),
                peak_rss_kib: acc.samples.iter().map(|s| s.max_rss_kib).max().unwrap_or(0),
                values: acc.values,
                samples: acc.samples,
                failures: acc.failures,
                note: acc.note,
                project_bytes: acc.project_bytes,
            }
        })
        .collect()
}

fn print_line(results: &Results, workload: &str, s: &Scenario) {
    let get = |tool: &str| results.find(workload, s.id, tool);
    let show = |m: Option<&Measurement>| match m {
        Some(m) if m.summary.n > 0 => match m.unit.as_str() {
            "bytes" => stats::bytes(m.summary.median),
            _ => stats::seconds(m.summary.median),
        },
        Some(m) if m.failures > 0 => "failed".to_string(),
        Some(_) => "—".to_string(),
        None => "".to_string(),
    };
    let (rune, cargo) = (get("rune"), get("cargo"));
    let verdict = match (rune, cargo) {
        (Some(r), Some(c)) => match stats::compare(&r.summary, &c.summary) {
            Some(cmp) if r.unit == "bytes" => {
                format!("{:.1}× the disk", 1.0 / cmp.speedup)
            }
            Some(cmp) if cmp.within_noise => "about the same (within noise)".to_string(),
            Some(cmp) => stats::describe(cmp.speedup),
            None => String::new(),
        },
        _ => String::new(),
    };
    eprintln!(
        "  ✓ {:<16} rune {:>11}   cargo {:>11}   {verdict}",
        s.id,
        show(rune),
        show(cargo)
    );
    for m in [rune, cargo].into_iter().flatten() {
        if let Some(note) = &m.note
            && (m.summary.n == 0 || m.failures > 0)
        {
            eprintln!("      {} : {}", m.tool, note.lines().last().unwrap_or(""));
        }
    }
}

fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}
