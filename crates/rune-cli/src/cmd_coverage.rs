//! `rune coverage`: source-based code coverage of the workspace's tests, like
//! `cargo llvm-cov`.
//!
//! The workspace's own crates are compiled with `-C instrument-coverage`
//! (dependencies are not: nothing of theirs would be reported, and they stay
//! shared with ordinary builds in the store), the tests run with
//! `LLVM_PROFILE_FILE` pointing into `<target>/coverage`, and the profiles are
//! merged and reported with the toolchain's own `llvm-profdata` and
//! `llvm-cov` (the `llvm-tools` component, installed on first use).
//!
//! The instrumented build has a directory of its own, `<target>/coverage/
//! build`: its crates have the same file names as the ordinary build's, and
//! sharing one directory made each command replace the other's files (and
//! a `rune test` running beside `rune coverage` fail).

use anyhow::{Context, Result, bail};
use colored::Colorize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::args::{CoverageArgs, TestArgs};
use crate::graph::Mode;
use crate::ui;
use crate::workspace::Workspace;

/// `llvm-profdata` and `llvm-cov` from the toolchain.
struct LlvmTools {
    profdata: PathBuf,
    cov: PathBuf,
}

fn find_tools(install: bool) -> Result<LlvmTools> {
    let tc = rune_common::toolchain::global();
    let exe = |name: &str| format!("{name}{}", std::env::consts::EXE_SUFFIX);
    let in_sysroot = |sysroot: &Path| -> Option<LlvmTools> {
        let bin = sysroot.join("lib/rustlib").join(&tc.host).join("bin");
        let tools = LlvmTools {
            profdata: bin.join(exe("llvm-profdata")),
            cov: bin.join(exe("llvm-cov")),
        };
        (tools.profdata.is_file() && tools.cov.is_file()).then_some(tools)
    };
    // Explicit paths win, as with cargo-llvm-cov.
    if let (Ok(p), Ok(c)) = (std::env::var("LLVM_PROFDATA"), std::env::var("LLVM_COV")) {
        return Ok(LlvmTools {
            profdata: p.into(),
            cov: c.into(),
        });
    }
    if let Some(sysroot) = &tc.sysroot {
        if let Some(t) = in_sysroot(sysroot) {
            return Ok(t);
        }
        // A rustup toolchain: add the component.
        if install && sysroot.components().any(|c| c.as_os_str() == "toolchains") {
            ui::action("installing", "the llvm-tools component (once)");
            let toolchain = sysroot
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let status = Command::new("rustup")
                .args(["component", "add", "llvm-tools", "--toolchain", &toolchain])
                .status();
            if status.is_ok_and(|s| s.success())
                && let Some(t) = in_sysroot(sysroot)
            {
                return Ok(t);
            }
        }
    }
    bail!(
        "llvm-profdata and llvm-cov were not found: install them with \
         `rustup component add llvm-tools`, or point LLVM_PROFDATA and LLVM_COV at them"
    )
}

/// Summary numbers of one file or the total.
#[derive(Default, Clone, Copy)]
struct Counts {
    covered: u64,
    count: u64,
}

impl Counts {
    fn percent(self) -> f64 {
        if self.count == 0 {
            100.0
        } else {
            self.covered as f64 * 100.0 / self.count as f64
        }
    }
}

struct FileSummary {
    path: String,
    lines: Counts,
    functions: Counts,
    regions: Counts,
    /// Lines that never ran.
    missing: Vec<u64>,
}

fn counts(v: &serde_json::Value, key: &str) -> Counts {
    Counts {
        covered: v
            .pointer(&format!("/{key}/covered"))
            .and_then(|x| x.as_u64())
            .unwrap_or(0),
        count: v
            .pointer(&format!("/{key}/count"))
            .and_then(|x| x.as_u64())
            .unwrap_or(0),
    }
}

/// `1-3, 7, 9-12`.
fn ranges(lines: &[u64]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let start = lines[i];
        let mut end = start;
        while i + 1 < lines.len() && lines[i + 1] == end + 1 {
            i += 1;
            end = lines[i];
        }
        out.push(if start == end {
            start.to_string()
        } else {
            format!("{start}-{end}")
        });
        i += 1;
    }
    out.join(", ")
}

/// Per-file lines with their execution counts, from an LCOV report (a line
/// listed more than once — several instantiations — counts once, with the
/// executions added up).
fn lcov_lines(lcov: &str) -> BTreeMap<String, Vec<(u64, u64)>> {
    let mut files: BTreeMap<String, BTreeMap<u64, u64>> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in lcov.lines() {
        if let Some(f) = line.strip_prefix("SF:") {
            current = Some(f.to_string());
        } else if let Some(da) = line.strip_prefix("DA:")
            && let Some(f) = &current
        {
            let mut parts = da.split(',');
            if let (Some(l), Some(c)) = (parts.next(), parts.next())
                && let (Ok(l), Ok(c)) = (l.parse::<u64>(), c.parse::<u64>())
            {
                *files.entry(f.clone()).or_default().entry(l).or_default() += c;
            }
        } else if line == "end_of_record" {
            current = None;
        }
    }
    files
        .into_iter()
        .map(|(f, ls)| (f, ls.into_iter().collect()))
        .collect()
}

/// A readable Rust symbol (`covp::classify`), or the input unchanged.
fn demangle(name: &str) -> String {
    format!("{:#}", rustc_demangle::demangle(name))
}

/// The helper `llvm-cov show -Xdemangler` runs.
pub fn demangle_stdin() {
    use std::io::{BufRead, Write};
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines().map_while(Result::ok) {
        let _ = writeln!(out, "{}", demangle(line.trim()));
    }
}

/// An LCOV report with readable function names, and each function listed
/// once however many test binaries instantiated it.
fn tidy_lcov(lcov: &str) -> String {
    let mut out = String::with_capacity(lcov.len());
    let mut record: Vec<String> = Vec::new();
    let flush = |record: &mut Vec<String>, out: &mut String| {
        let mut fns: BTreeMap<(u64, String), ()> = BTreeMap::new();
        let mut hits: BTreeMap<String, u64> = BTreeMap::new();
        let mut lines: BTreeMap<u64, u64> = BTreeMap::new();
        let mut rest: Vec<&str> = Vec::new();
        for l in record.iter() {
            if let Some(f) = l.strip_prefix("FN:") {
                if let Some((line, name)) = f.split_once(',') {
                    fns.insert((line.parse().unwrap_or(0), demangle(name)), ());
                }
            } else if let Some(f) = l.strip_prefix("FNDA:") {
                if let Some((n, name)) = f.split_once(',') {
                    *hits.entry(demangle(name)).or_default() += n.parse::<u64>().unwrap_or(0);
                }
            } else if let Some(d) = l.strip_prefix("DA:") {
                let mut p = d.split(',');
                if let (Some(Ok(line)), Some(Ok(n))) = (
                    p.next().map(str::parse::<u64>),
                    p.next().map(str::parse::<u64>),
                ) {
                    *lines.entry(line).or_default() += n;
                }
            } else if !(l.starts_with("FNF:")
                || l.starts_with("FNH:")
                || l.starts_with("LF:")
                || l.starts_with("LH:")
                || l.as_str() == "end_of_record")
            {
                rest.push(l);
            }
        }
        let (sf, others): (Vec<&str>, Vec<&str>) =
            rest.into_iter().partition(|l| l.starts_with("SF:"));
        for l in sf {
            out.push_str(l);
            out.push('\n');
        }
        let mut names: Vec<&String> = Vec::new();
        for (line, name) in fns.keys() {
            if !names.contains(&name) {
                names.push(name);
                out.push_str(&format!("FN:{line},{name}\n"));
            }
        }
        for name in &names {
            out.push_str(&format!(
                "FNDA:{},{name}\n",
                hits.get(*name).copied().unwrap_or(0)
            ));
        }
        out.push_str(&format!("FNF:{}\n", names.len()));
        out.push_str(&format!(
            "FNH:{}\n",
            names
                .iter()
                .filter(|n| hits.get(**n).is_some_and(|h| *h > 0))
                .count()
        ));
        for l in others {
            out.push_str(l);
            out.push('\n');
        }
        for (line, n) in &lines {
            out.push_str(&format!("DA:{line},{n}\n"));
        }
        out.push_str(&format!("LF:{}\n", lines.len()));
        out.push_str(&format!(
            "LH:{}\n",
            lines.values().filter(|n| **n > 0).count()
        ));
        out.push_str("end_of_record\n");
        record.clear();
    };
    for line in lcov.lines() {
        record.push(line.to_string());
        if line == "end_of_record" {
            flush(&mut record, &mut out);
        }
    }
    out
}

/// A Cobertura XML report from LCOV line data.
fn cobertura(lines: &BTreeMap<String, Vec<(u64, u64)>>, root: &Path) -> String {
    let (mut valid, mut covered) = (0u64, 0u64);
    let mut classes = String::new();
    for (file, ls) in lines {
        let hit = ls.iter().filter(|(_, c)| *c > 0).count() as u64;
        valid += ls.len() as u64;
        covered += hit;
        let rel = Path::new(file)
            .strip_prefix(root)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| file.clone());
        let rate = if ls.is_empty() {
            1.0
        } else {
            hit as f64 / ls.len() as f64
        };
        let esc = xml_escape(&rel);
        classes.push_str(&format!(
            "        <class name=\"{esc}\" filename=\"{esc}\" line-rate=\"{rate:.4}\" branch-rate=\"0\" complexity=\"0\">\n          <methods/>\n          <lines>\n"
        ));
        for (l, c) in ls {
            classes.push_str(&format!(
                "            <line number=\"{l}\" hits=\"{c}\" branch=\"false\"/>\n"
            ));
        }
        classes.push_str("          </lines>\n        </class>\n");
    }
    let rate = if valid == 0 {
        1.0
    } else {
        covered as f64 / valid as f64
    };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    format!(
        "<?xml version=\"1.0\"?>\n<!DOCTYPE coverage SYSTEM \"http://cobertura.sourceforge.net/xml/coverage-04.dtd\">\n\
         <coverage line-rate=\"{rate:.4}\" branch-rate=\"0\" lines-covered=\"{covered}\" lines-valid=\"{valid}\" \
         branches-covered=\"0\" branches-valid=\"0\" complexity=\"0\" version=\"rune-{}\" timestamp=\"{stamp}\">\n\
         <sources><source>{}</source></sources>\n  <packages>\n    <package name=\"workspace\" line-rate=\"{rate:.4}\" branch-rate=\"0\" complexity=\"0\">\n      <classes>\n{classes}      </classes>\n    </package>\n  </packages>\n</coverage>\n",
        env!("CARGO_PKG_VERSION"),
        xml_escape(&root.display().to_string())
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// What llvm-cov leaves out of a report: sources outside the workspace's own
/// code (the standard library, dependencies, generated files in the target
/// directory) and tests, examples and benches, as cargo-llvm-cov does.
fn ignore_regex(ws: &Workspace, user: Option<&str>) -> String {
    let target = regex_escape(&ws.config.target_dir.display().to_string());
    let home = regex_escape(&rune_common::settings::rune_home().display().to_string());
    let mut parts = vec![
        "^/rustc/".to_string(),
        r"(^|/)\.cargo/(registry|git)/".to_string(),
        r"(^|/)\.rustup/".to_string(),
        format!("^{target}/"),
        format!("^{home}/"),
        r"(^|/)(tests|examples|benches)/".to_string(),
    ];
    if let Some(u) = user {
        parts.push(u.to_string());
    }
    parts.join("|")
}

fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

pub fn coverage(args: CoverageArgs) -> Result<()> {
    let mut ws = Workspace::load(args.common.manifest_path.as_deref())?;
    if let Some(dir) = crate::session::target_dir_override(&args.common)? {
        ws.config.target_dir = dir;
    }
    let tools = find_tools(true)?;
    let dir = ws.config.target_dir.join("coverage");
    let profraw_dir = dir.join("profraw");
    // Profiles of an earlier run would mix in.
    let _ = std::fs::remove_dir_all(&profraw_dir);
    std::fs::create_dir_all(&profraw_dir)?;

    let instrument = crate::cmd_test::Instrument {
        rustflags: vec![
            "-C".into(),
            "instrument-coverage".into(),
            "--cfg".into(),
            "coverage".into(),
            "--check-cfg".into(),
            "cfg(coverage)".into(),
        ],
        env: vec![(
            "LLVM_PROFILE_FILE".into(),
            profraw_dir
                .join("rune-%p-%16m.profraw")
                .to_string_lossy()
                .into_owned(),
        )],
    };
    let mut common = args.common.clone();
    common.target_dir = Some(dir.join("build"));
    let test_args = TestArgs {
        common,
        targets: args.targets.clone(),
        filter: args.filter.clone(),
        doc: false,
        no_doc: true,
        no_run: false,
        no_fail_fast: args.no_fail_fast,
        test_threads: None,
        // A remembered pass runs nothing, and records nothing.
        cached: Some(false),
        test_args: args.test_args.clone(),
        retries: 0,
        junit: None,
        schedule: Some("binaries".into()),
        test_jobs: args.test_jobs,
        timeout: args.timeout.clone(),
    };
    let outcome = crate::cmd_test::run_with(test_args, Mode::Test, Some(&instrument))?;
    if outcome.failed && !args.no_fail_fast {
        ui::hint("no coverage report: a test failed (`--no-fail-fast` reports what ran)");
        std::process::exit(101);
    }
    if args.no_report {
        ui::detail(format!("profiles kept in {}", profraw_dir.display()));
        return Ok(());
    }

    // Merge. A profile cut short (a test binary killed mid-write) is left
    // out with a warning instead of failing the report.
    let profraws: Vec<PathBuf> = std::fs::read_dir(&profraw_dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "profraw"))
        .collect();
    if profraws.is_empty() {
        bail!("the tests recorded no coverage (did any run?)");
    }
    let profdata = dir.join("rune.profdata");
    let what = ui::plural(profraws.len(), "profile");
    let activity = ui::Activity::start("merging", &what);
    let merged = Command::new(&tools.profdata)
        .args(["merge", "-sparse", "--failure-mode=all", "-o"])
        .arg(&profdata)
        .args(&profraws)
        .output()
        .context("running llvm-profdata")?;
    let took = activity.elapsed();
    activity.clear();
    if !merged.status.success() {
        bail!(
            "llvm-profdata merge failed: {}",
            String::from_utf8_lossy(&merged.stderr).trim()
        );
    }
    ui::action("merged", format!("{what} · {}", ui::secs(took).dimmed()));
    for line in String::from_utf8_lossy(&merged.stderr).lines() {
        if let Some(w) = line.trim().strip_prefix("warning: ") {
            ui::warn(format!("a coverage profile was left out: {w}"));
        }
    }

    // The programs whose coverage mappings describe the code.
    let objects: Vec<&PathBuf> = outcome.executables.iter().filter(|p| p.is_file()).collect();
    if objects.is_empty() {
        bail!("no instrumented programs to report on");
    }
    let ignore = ignore_regex(&ws, args.ignore_filename_regex.as_deref());
    let llvm_cov = |sub: &str, extra: &[&str]| -> Command {
        let mut c = Command::new(&tools.cov);
        c.arg(sub).args(extra);
        c.arg(format!("-instr-profile={}", profdata.display()));
        c.arg(format!("-ignore-filename-regex={ignore}"));
        for (i, o) in objects.iter().enumerate() {
            if i > 0 {
                c.arg("-object");
            }
            c.arg(o);
        }
        c.current_dir(ws.root());
        c
    };
    let run = |mut c: Command, what: &str| -> Result<Vec<u8>> {
        let out = c
            .output()
            .with_context(|| format!("running llvm-cov {what}"))?;
        if !out.status.success() {
            bail!(
                "llvm-cov {what} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(out.stdout)
    };

    // The summary and the per-line counts are separate exports; llvm-cov
    // runs on one core, so both run at once.
    let what = format!("on {}", ui::plural(objects.len(), "program"));
    let activity = ui::Activity::start("reporting", &what);
    let (summary, lcov) = std::thread::scope(|scope| {
        let lcov = scope.spawn(|| run(llvm_cov("export", &["-format=lcov"]), "export"));
        let summary = run(
            llvm_cov("export", &["-format=text", "-summary-only"]),
            "export",
        );
        (
            summary,
            lcov.join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("llvm-cov export panicked"))),
        )
    });
    let took = activity.elapsed();
    activity.clear();
    let summary: serde_json::Value = serde_json::from_slice(&summary?)?;
    let lcov = String::from_utf8(lcov?).unwrap_or_default();
    ui::action("reported", format!("{what} · {}", ui::secs(took).dimmed()));
    let lines = lcov_lines(&lcov);

    let root = ws.root();
    let mut files: Vec<FileSummary> = summary
        .pointer("/data/0/files")
        .and_then(|f| f.as_array())
        .into_iter()
        .flatten()
        .map(|f| {
            let name = f["filename"].as_str().unwrap_or_default().to_string();
            let mut missing: Vec<u64> = lines
                .get(&name)
                .into_iter()
                .flatten()
                .filter(|(_, c)| *c == 0)
                .map(|(l, _)| *l)
                .collect();
            missing.sort_unstable();
            missing.dedup();
            FileSummary {
                path: Path::new(&name)
                    .strip_prefix(root)
                    .map(|p| p.display().to_string())
                    .unwrap_or(name),
                lines: counts(&f["summary"], "lines"),
                functions: counts(&f["summary"], "functions"),
                regions: counts(&f["summary"], "regions"),
                missing,
            }
        })
        .collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let totals = summary
        .pointer("/data/0/totals")
        .cloned()
        .unwrap_or_default();
    let total = FileSummary {
        path: "total".into(),
        lines: counts(&totals, "lines"),
        functions: counts(&totals, "functions"),
        regions: counts(&totals, "regions"),
        missing: Vec::new(),
    };
    print_table(&files, &total, args.show_missing_lines);

    // Reports.
    if args.lcov || args.json || args.cobertura {
        let (ext, body): (&str, Vec<u8>) = if args.lcov {
            ("lcov.info", tidy_lcov(&lcov).into_bytes())
        } else if args.json {
            (
                "coverage.json",
                run(llvm_cov("export", &["-format=text"]), "export")?,
            )
        } else {
            ("cobertura.xml", cobertura(&lines, root).into_bytes())
        };
        let path = args.output_path.clone().unwrap_or_else(|| dir.join(ext));
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
        ui::action("wrote", path.display());
    }
    if args.html {
        let out_dir = args.output_dir.clone().unwrap_or_else(|| dir.join("html"));
        let _ = std::fs::remove_dir_all(&out_dir);
        let activity = ui::Activity::start("writing", "the HTML report");
        run(
            llvm_cov(
                "show",
                &[
                    "-format=html",
                    &format!("-output-dir={}", out_dir.display()),
                    "-show-line-counts-or-regions",
                    "-show-instantiations=false",
                    &format!("-Xdemangler={}", std::env::current_exe()?.display()),
                    "-Xdemangler=__demangle",
                ],
            ),
            "show",
        )?;
        activity.clear();
        ui::action("wrote", out_dir.join("index.html").display());
    }

    if outcome.failed {
        std::process::exit(101);
    }
    if let Some(min) = args.fail_under {
        let got = total.lines.percent();
        if got < min {
            ui::error(format!(
                "line coverage {got:.2} % is below the required {min} %"
            ));
            std::process::exit(1);
        }
    }
    Ok(())
}

fn print_table(files: &[FileSummary], total: &FileSummary, show_missing: bool) {
    let width = files
        .iter()
        .map(|f| f.path.len())
        .chain([5])
        .max()
        .unwrap_or(5)
        .min(60);
    let pct = |c: Counts| {
        let p = c.percent();
        let s = format!("{p:>6.2} %");
        if !ui::colors() {
            s
        } else if p >= 80.0 {
            s.green().to_string()
        } else if p >= 50.0 {
            s.yellow().to_string()
        } else {
            s.red().to_string()
        }
    };
    let cell = |c: Counts| format!("{:>11} {}", format!("{}/{}", c.covered, c.count), pct(c));
    ui::out(format!(
        "{:<width$}  {:>20}  {:>20}  {:>20}",
        "file".bold(),
        "lines".bold(),
        "functions".bold(),
        "regions".bold()
    ));
    for f in files {
        let mut name = f.path.clone();
        if name.len() > width {
            name = format!("…{}", &name[name.len() - width + 1..]);
        }
        ui::out(format!(
            "{name:<width$}  {}  {}  {}",
            cell(f.lines),
            cell(f.functions),
            cell(f.regions)
        ));
        if show_missing && !f.missing.is_empty() {
            ui::out(format!(
                "  {}",
                format!("not run: {}", ranges(&f.missing)).dimmed()
            ));
        }
    }
    ui::out(format!(
        "{:<width$}  {}  {}  {}",
        total.path.bold(),
        cell(total.lines),
        cell(total.functions),
        cell(total.regions)
    ));
    ui::summary(format!(
        "{:.2} % of lines covered ({} of {}) · {} files",
        total.lines.percent(),
        total.lines.covered,
        total.lines.count,
        files.len()
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_lines_are_shown_as_ranges() {
        assert_eq!(ranges(&[1, 2, 3, 7, 9, 10]), "1-3, 7, 9-10");
        assert_eq!(ranges(&[]), "");
    }

    #[test]
    fn lcov_line_data_is_read_per_file() {
        let l = lcov_lines(
            "SF:/a/src/lib.rs\nDA:1,3\nDA:2,0\nend_of_record\nSF:/a/src/b.rs\nDA:5,1\nend_of_record\n",
        );
        assert_eq!(l["/a/src/lib.rs"], [(1, 3), (2, 0)]);
        let xml = cobertura(&l, Path::new("/a"));
        assert!(
            xml.contains("lines-covered=\"2\" lines-valid=\"3\""),
            "{xml}"
        );
        assert!(xml.contains("filename=\"src/lib.rs\""), "{xml}");
    }

    #[test]
    fn lcov_names_are_demangled_and_instantiations_merged() {
        let raw = "SF:/a/src/lib.rs\nFN:11,_RNvCskpwGOutD9k4_4covp12never_called\nFN:11,_RNvCsjRWvhkrlMiY_4covp12never_called\nFNDA:0,_RNvCskpwGOutD9k4_4covp12never_called\nFNDA:2,_RNvCsjRWvhkrlMiY_4covp12never_called\nFNF:2\nFNH:1\nDA:11,0\nDA:11,2\nDA:12,0\nLF:3\nLH:1\nend_of_record\n";
        let t = tidy_lcov(raw);
        assert!(
            t.contains("FN:11,covp::never_called\nFNDA:2,covp::never_called\nFNF:1\nFNH:1"),
            "{t}"
        );
        assert!(t.contains("DA:11,2\nDA:12,0\nLF:2\nLH:1"), "{t}");
    }

    #[test]
    fn paths_are_escaped_for_llvm_regexes() {
        assert_eq!(regex_escape("/a.b/c+d"), r"/a\.b/c\+d");
    }
}
