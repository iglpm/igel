//! `rune-bench`: measures Rune against Cargo (`rune-bench run|report|compare|list`).

mod measure;
mod report;
mod results;
mod runner;
mod scenario;
mod stats;
mod tool;
mod workload;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "rune-bench",
    version,
    about = "Benchmarks Rune against Cargo on generated and real projects"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the benchmarks and write results.json, report.md and report.html
    Run(Box<RunArgs>),
    /// Render a results file again
    Report {
        results: PathBuf,
        #[arg(long, value_enum, default_value = "terminal")]
        format: Format,
    },
    /// Compare two results files; exits 1 when a measurement got slower
    Compare {
        before: PathBuf,
        after: PathBuf,
        /// Which tool's numbers to compare
        #[arg(long, default_value = "rune")]
        tool: String,
        /// Slowdown (in percent) beyond the runs' own noise that counts as a
        /// regression
        #[arg(long, default_value_t = 10.0)]
        threshold: f64,
    },
    /// List the workloads and scenarios
    List,
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    Terminal,
    Markdown,
    Html,
    Json,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Suite {
    /// tiny and medium workloads, the main scenarios, few runs (~10 min)
    Quick,
    /// every workload and scenario, more runs (an hour or more)
    Full,
}

#[derive(clap::Args)]
struct RunArgs {
    #[arg(long, value_enum, default_value = "quick")]
    suite: Suite,
    /// Only these workloads (repeatable): tiny, medium, large
    #[arg(long = "workload", value_name = "NAME")]
    workloads: Vec<String>,
    /// Only these scenarios (repeatable; see `rune-bench list`)
    #[arg(long = "scenario", value_name = "ID")]
    scenarios: Vec<String>,
    /// Benchmark a real project (copied first; registry dependencies need
    /// the network the first time)
    #[arg(long, value_name = "DIR")]
    project: Option<PathBuf>,
    /// For --project: the file edited by the edit-the-app scenarios
    /// (default: src/main.rs or src/lib.rs)
    #[arg(long, value_name = "FILE")]
    edit_leaf: Option<PathBuf>,
    /// For --project: a file most of the project depends on, for the
    /// edit-the-core and branch-switch scenarios
    #[arg(long, value_name = "FILE")]
    edit_core: Option<PathBuf>,
    /// For --project: an integration test edited by the edit-test scenario
    /// (default: the first of `tests/*.rs`)
    #[arg(long, value_name = "FILE")]
    edit_test: Option<PathBuf>,
    /// The rune binary (default: `rune` on PATH, else ./target/release/rune)
    #[arg(long)]
    rune: Option<PathBuf>,
    /// The cargo binary (default: `cargo` on PATH)
    #[arg(long)]
    cargo: Option<PathBuf>,
    /// Runs of each cheap scenario (default: 5 quick, 15 full)
    #[arg(long)]
    runs: Option<usize>,
    /// Runs of each expensive scenario — builds from scratch (default: 3 quick, 5 full)
    #[arg(long)]
    runs_heavy: Option<usize>,
    /// Unrecorded runs before each cheap scenario
    #[arg(long)]
    warmup: Option<usize>,
    /// Give up on one command after this many seconds
    #[arg(long, default_value_t = 3600)]
    timeout: u64,
    /// Where results go (default: ./rune-bench-results/<date-time>)
    #[arg(long)]
    out: Option<PathBuf>,
    /// Keep the workloads' copies and build output afterwards
    #[arg(long)]
    keep_work: bool,
    /// Print the output of failed runs
    #[arg(long, short)]
    verbose: bool,
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Run(a) => run(*a),
        Command::Report { results, format } => load(&results).map(|r| {
            print!("{}", render(&r, format));
        }),
        Command::Compare {
            before,
            after,
            tool,
            threshold,
        } => (|| {
            let (old, new) = (load(&before)?, load(&after)?);
            let (table, regressions) = report::compare(&old, &new, &tool, threshold / 100.0);
            print!("{table}");
            if regressions > 0 {
                eprintln!("\n✗ {regressions} measurement(s) of {tool} got slower");
                std::process::exit(1);
            }
            eprintln!("\n✓ no regressions");
            Ok(())
        })(),
        Command::List => {
            list();
            Ok(())
        }
    };
    if let Err(e) = result {
        eprintln!("✗ {e:#}");
        std::process::exit(2);
    }
}

fn render(r: &results::Results, format: Format) -> String {
    match format {
        Format::Terminal => report::terminal(r),
        Format::Markdown => report::markdown(r),
        Format::Html => report::html(r),
        Format::Json => serde_json::to_string_pretty(r).unwrap_or_default(),
    }
}

fn load(path: &Path) -> Result<results::Results> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let r: results::Results =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    if r.format != results::FORMAT {
        bail!(
            "{} is in results format {}, not {}",
            path.display(),
            r.format,
            results::FORMAT
        );
    }
    Ok(r)
}

fn list() {
    println!("Workloads:");
    for w in workload::builtin() {
        println!("  {:<8} {}", w.name, w.description);
    }
    println!("  (or any project: --project DIR)\n\nScenarios:");
    for s in scenario::all() {
        println!(
            "  {:<16} {:<16} {}{}",
            s.id,
            s.category,
            s.about,
            if s.full_only { " [full suite]" } else { "" }
        );
    }
}

fn run(a: RunArgs) -> Result<()> {
    let full = a.suite == Suite::Full;
    let rune_bin = a.rune.clone().or_else(|| {
        tool::which("rune").or_else(|| {
            let local = PathBuf::from("target/release/rune");
            local.is_file().then_some(local)
        })
    });
    let rune = tool::Tool::find("rune", rune_bin.as_deref())?;
    let cargo = tool::Tool::find("cargo", a.cargo.as_deref())?;
    let tools = vec![rune, cargo];

    let mut workloads: Vec<workload::Workload> = Vec::new();
    if let Some(p) = &a.project {
        workloads.push(workload::project(
            p,
            a.edit_leaf.clone(),
            a.edit_core.clone(),
            a.edit_test.clone(),
        )?);
    }
    if a.project.is_none() || !a.workloads.is_empty() {
        for w in workload::builtin() {
            let wanted = if a.workloads.is_empty() {
                full || w.name != "large"
            } else {
                a.workloads.contains(&w.name)
            };
            if wanted {
                workloads.push(w);
            }
        }
    }
    for name in &a.workloads {
        if !workload::builtin().iter().any(|w| &w.name == name) {
            bail!("unknown workload `{name}` (see `rune-bench list`)");
        }
    }

    let all = scenario::all();
    for id in &a.scenarios {
        if !all.iter().any(|s| s.id == id) {
            bail!("unknown scenario `{id}` (see `rune-bench list`)");
        }
    }
    let scenarios: Vec<&scenario::Scenario> = all
        .iter()
        .filter(|s| {
            if a.scenarios.is_empty() {
                full || !s.full_only
            } else {
                a.scenarios.iter().any(|id| id == s.id)
            }
        })
        .collect();

    let out = a
        .out
        .clone()
        .unwrap_or_else(|| PathBuf::from("rune-bench-results").join(timestamp()));
    std::fs::create_dir_all(&out)?;
    let need: u64 = workloads
        .iter()
        .map(|w| match (&w.source, w.name.as_str()) {
            (workload::Source::Project(_), _) => 8 << 30,
            (_, "large") => 12 << 30,
            (_, "medium") => 3 << 30,
            _ => 1 << 30,
        })
        .max()
        .unwrap_or(1 << 30);
    if let Some(free) = free_bytes(&out)
        && free < need
    {
        bail!(
            "only {} free where the results go ({}); this run needs about {} \
             (pick another place with --out)",
            stats::bytes(free as f64),
            out.display(),
            stats::bytes(need as f64)
        );
    }
    let cfg = runner::Config {
        runs_heavy: a.runs_heavy.unwrap_or(if full { 5 } else { 3 }).max(1),
        runs_light: a.runs.unwrap_or(if full { 15 } else { 5 }).max(1),
        warmup: a.warmup.unwrap_or(if full { 2 } else { 1 }),
        timeout: Duration::from_secs(a.timeout),
        work: out.join("work"),
        verbose: a.verbose,
        live: std::io::IsTerminal::is_terminal(&std::io::stderr()),
    };

    let mut results = results::Results {
        format: results::FORMAT,
        meta: meta(
            &tools,
            if full { "full" } else { "quick" },
            &cfg,
            &workloads,
        ),
        measurements: Vec::new(),
    };
    eprintln!("rune-bench: {} vs {}", tools[0].version, tools[1].version);
    eprintln!(
        "  {} workload(s) × {} scenario(s); results in {}",
        workloads.len(),
        scenarios.len(),
        out.display()
    );
    let write = |r: &results::Results| -> Result<()> {
        std::fs::write(out.join("results.json"), serde_json::to_string_pretty(r)?)?;
        std::fs::write(out.join("report.md"), report::markdown(r))?;
        std::fs::write(out.join("report.html"), report::html(r))?;
        Ok(())
    };
    let ran = runner::run(
        &tools,
        &workloads,
        &scenarios,
        &cfg,
        &mut results,
        &mut |r| {
            if let Err(e) = write(r) {
                eprintln!("  ! could not write the results so far: {e:#}");
            }
        },
    );
    workload::forget_pristine();
    if !a.keep_work
        && let Err(e) = tool::remove(&cfg.work)
    {
        eprintln!("  ! could not remove {}: {e:#}", cfg.work.display());
    }
    ran?;
    write(&results)?;
    print!("{}", report::terminal(&results));
    eprintln!(
        "\nWrote {}/results.json, report.md and report.html",
        out.display()
    );
    Ok(())
}

fn meta(
    tools: &[tool::Tool],
    suite: &str,
    cfg: &runner::Config,
    workloads: &[workload::Workload],
) -> results::Meta {
    let output = |cmd: &str, args: &[&str]| {
        std::process::Command::new(cmd)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let vv = output("rustc", &["-vV"]).unwrap_or_default();
    let field = |name: &str| {
        vv.lines()
            .find_map(|l| l.strip_prefix(name))
            .map(|v| v.trim().to_string())
            .unwrap_or_default()
    };
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf);
    let commit = repo.and_then(|r| {
        std::process::Command::new("git")
            .args(["rev-parse", "--short", "HEAD"])
            .current_dir(r)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    });
    results::Meta {
        started: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
        suite: suite.to_string(),
        tools: tools.to_vec(),
        rustc: vv.lines().next().unwrap_or_default().to_string(),
        host: field("host:"),
        cpus: std::thread::available_parallelism().map_or(1, |n| n.get()),
        memory_bytes: memory(),
        os: output("uname", &["-sr"]).unwrap_or_else(|| std::env::consts::OS.to_string()),
        commit,
        runs_heavy: cfg.runs_heavy,
        runs_light: cfg.runs_light,
        workloads: workloads
            .iter()
            .map(|w| results::WorkloadInfo {
                name: w.name.clone(),
                description: w.description.clone(),
            })
            .collect(),
    }
}

fn free_bytes(path: &Path) -> Option<u64> {
    let out = std::process::Command::new("df")
        .args(["-Pk"])
        .arg(path)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let kib: u64 = text
        .lines()
        .nth(1)?
        .split_whitespace()
        .nth(3)?
        .parse()
        .ok()?;
    Some(kib * 1024)
}

fn memory() -> Option<u64> {
    if let Ok(text) = std::fs::read_to_string("/proc/meminfo") {
        let kb: u64 = text
            .lines()
            .find_map(|l| l.strip_prefix("MemTotal:"))?
            .split_whitespace()
            .next()?
            .parse()
            .ok()?;
        return Some(kb * 1024);
    }
    let out = std::process::Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

fn timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs()) as i64;
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}-{:02}{:02}",
        rest / 3600,
        (rest % 3600) / 60
    )
}
