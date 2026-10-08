//! `rune watch`: re-run commands on file changes, found by polling sizes and mtimes.

use crate::args::WatchArgs;
use crate::ui;
use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const IGNORED_DIRS: &[&str] = &[
    "target",
    "node_modules",
    ".git",
    ".rune",
    ".hg",
    ".jj",
    ".idea",
    ".vscode",
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State(BTreeMap<PathBuf, (u64, u128)>);

pub struct Snapshot {
    root: PathBuf,
    extra_ignored: Vec<PathBuf>,
}

impl Snapshot {
    pub fn new(root: &Path, extra_ignored: Vec<PathBuf>) -> Self {
        Self {
            root: root.to_path_buf(),
            extra_ignored,
        }
    }

    pub fn state(&self) -> State {
        let mut entries = BTreeMap::new();
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(read) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in read.flatten() {
                let path = entry.path();
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let Ok(ft) = entry.file_type() else { continue };
                if ft.is_dir() {
                    if IGNORED_DIRS.contains(&name.as_ref())
                        || self.extra_ignored.iter().any(|p| p == &path)
                    {
                        continue;
                    }
                    stack.push(path);
                } else if !is_editor_scratch(&name)
                    && !is_generated_by_rune(&name)
                    && !self.extra_ignored.iter().any(|p| p == &path)
                {
                    let Ok(meta) = entry.metadata() else { continue };
                    let mtime = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map_or(0, |d| d.as_nanos());
                    entries.insert(path, (meta.len(), mtime));
                }
            }
        }
        State(entries)
    }

    pub fn changes(&self, base: &State, now: &State) -> Vec<PathBuf> {
        if base == now {
            return Vec::new();
        }
        let mut changed: Vec<PathBuf> = now
            .0
            .iter()
            .filter(|(p, v)| base.0.get(*p) != Some(*v))
            .map(|(p, _)| p.clone())
            .chain(base.0.keys().filter(|p| !now.0.contains_key(*p)).cloned())
            .collect();
        let ignored = git_ignored(&self.root, &changed);
        changed.retain(|p| !ignored.contains(p));
        changed
    }
}

fn git_ignored(root: &Path, paths: &[PathBuf]) -> HashSet<PathBuf> {
    let in_repository = root.ancestors().any(|d| d.join(".git").exists());
    if !in_repository || paths.is_empty() {
        return HashSet::new();
    }
    let mut ignored = HashSet::new();
    // As arguments, not stdin: git may stop reading early and the write would raise SIGPIPE.
    for chunk in paths.chunks(256) {
        let asked: HashMap<String, &PathBuf> = chunk
            .iter()
            .map(|p| {
                let rel = p.strip_prefix(root).unwrap_or(p);
                (rel.to_string_lossy().replace('\\', "/"), p)
            })
            .collect();
        let Ok(out) = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["-c", "core.quotePath=false", "check-ignore", "--"])
            .args(asked.keys())
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
        else {
            return HashSet::new();
        };
        if !out.status.success() {
            continue;
        }
        ignored.extend(
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|l| asked.get(&git_unquote(l)).map(|p| (*p).clone())),
        );
    }
    ignored
}

fn git_unquote(line: &str) -> String {
    let Some(inner) = line.strip_prefix('"').and_then(|l| l.strip_suffix('"')) else {
        return line.to_string();
    };
    let mut bytes = Vec::new();
    let mut chars = inner.bytes().peekable();
    while let Some(b) = chars.next() {
        if b != b'\\' {
            bytes.push(b);
            continue;
        }
        match chars.next() {
            Some(b'n') => bytes.push(b'\n'),
            Some(b't') => bytes.push(b'\t'),
            Some(b'r') => bytes.push(b'\r'),
            Some(d @ b'0'..=b'7') => {
                let mut value = u32::from(d - b'0');
                for _ in 0..2 {
                    match chars.peek() {
                        Some(&o @ b'0'..=b'7') => {
                            value = value * 8 + u32::from(o - b'0');
                            chars.next();
                        }
                        _ => break,
                    }
                }
                bytes.push(value as u8);
            }
            Some(other) => bytes.push(other),
            None => bytes.push(b'\\'),
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn describe(root: &Path, changed: &[PathBuf]) -> String {
    let first = changed
        .first()
        .map(|p| p.strip_prefix(root).unwrap_or(p).display().to_string())
        .unwrap_or_default();
    match changed.len() {
        0 | 1 => first,
        n => format!("{first} and {} more", n - 1),
    }
}

fn is_editor_scratch(name: &str) -> bool {
    name.ends_with('~')
        || name.ends_with(".swp")
        || name.ends_with(".swx")
        || name.ends_with(".tmp")
        || name.starts_with(".#")
        || (name.starts_with('#') && name.ends_with('#'))
        || name == "4913" // vim's write test file
}

fn is_generated_by_rune(name: &str) -> bool {
    matches!(name, "Cargo.lock" | "Rune.lock.bin")
}

fn split_command(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_string).collect()
}

enum Outcome {
    Finished { ok: bool },
    Interrupted(Vec<PathBuf>),
}

pub fn watch(args: WatchArgs) -> Result<()> {
    let ws = crate::workspace::Workspace::load(None)
        .context("`rune watch` needs to be run inside a project")?;
    let root = ws.config.project_root.clone();
    rune_common::sysinfo::lower_priority();
    let commands: Vec<Vec<String>> = if args.exec.is_empty() {
        vec![vec!["check".to_string()]]
    } else {
        args.exec.iter().map(|c| split_command(c)).collect()
    };
    let exe = std::env::current_exe().context("cannot find the rune executable")?;
    let mut ignored = vec![ws.config.target_dir.clone()];
    ignored.extend(args.ignore.iter().map(|p| root.join(p)));
    let snapshot = Snapshot::new(&root, ignored);
    let interval = Duration::from_millis(args.interval_ms.max(20));

    let mut runs = 0u32;
    loop {
        let mut before = snapshot.state();
        if args.clear {
            print!("\x1b[2J\x1b[H");
        }
        let started = Instant::now();
        let outcome = run_round(&exe, &commands, &root, &snapshot, &mut before, interval)?;
        runs += 1;
        match &outcome {
            Outcome::Finished { ok: true } => ui::summary(format!(
                "finished in {:.2}s · waiting for changes",
                started.elapsed().as_secs_f64()
            )),
            Outcome::Finished { ok: false } => ui::warn(format!(
                "failed after {:.2}s · waiting for changes",
                started.elapsed().as_secs_f64()
            )),
            Outcome::Interrupted(changed) => ui::action(
                "restarting",
                format!("{} changed", describe(&root, changed)),
            ),
        }
        if args.max_runs.is_some_and(|max| runs >= max) {
            return Ok(());
        }
        if !matches!(outcome, Outcome::Interrupted(_)) {
            let changed = wait_for_change(&snapshot, snapshot.state(), interval);
            ui::action("changed", describe(&root, &changed));
        }
        settle(&snapshot, interval);
    }
}

fn run_round(
    exe: &Path,
    commands: &[Vec<String>],
    root: &Path,
    snapshot: &Snapshot,
    baseline: &mut State,
    interval: Duration,
) -> Result<Outcome> {
    for command in commands {
        ui::action("running", format!("rune {}", command.join(" ")));
        let mut child: Child = Command::new(exe)
            .args(command)
            .current_dir(root)
            .spawn()
            .with_context(|| format!("failed to start `rune {}`", command.join(" ")))?;
        let mut last_poll = Instant::now();
        loop {
            if let Some(status) = child.try_wait()? {
                if !status.success() {
                    return Ok(Outcome::Finished { ok: false });
                }
                break;
            }
            if last_poll.elapsed() >= interval {
                last_poll = Instant::now();
                let now = snapshot.state();
                let changed = snapshot.changes(baseline, &now);
                if !changed.is_empty() {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok(Outcome::Interrupted(changed));
                }
                *baseline = now;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    Ok(Outcome::Finished { ok: true })
}

fn wait_for_change(snapshot: &Snapshot, mut baseline: State, interval: Duration) -> Vec<PathBuf> {
    loop {
        std::thread::sleep(interval);
        let now = snapshot.state();
        let changed = snapshot.changes(&baseline, &now);
        if !changed.is_empty() {
            return changed;
        }
        baseline = now;
    }
}

fn settle(snapshot: &Snapshot, interval: Duration) {
    let mut last = snapshot.state();
    loop {
        std::thread::sleep(interval.min(Duration::from_millis(150)));
        let now = snapshot.state();
        if now == last {
            return;
        }
        last = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "fn a() {}").unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
        dir
    }

    fn sig(dir: &tempfile::TempDir) -> State {
        Snapshot::new(dir.path(), vec![]).state()
    }

    #[test]
    fn an_unchanged_project_has_a_stable_signature() {
        let p = project();
        assert_eq!(sig(&p), sig(&p));
    }

    #[test]
    fn edits_new_files_and_deletions_are_all_changes() {
        let p = project();
        let base = sig(&p);
        std::fs::write(p.path().join("src/lib.rs"), "fn a() { 1; }").unwrap();
        let edited = sig(&p);
        assert_ne!(base, edited, "an edit");
        std::fs::write(p.path().join("src/new.rs"), "").unwrap();
        let added = sig(&p);
        assert_ne!(edited, added, "a new file");
        std::fs::remove_file(p.path().join("src/new.rs")).unwrap();
        assert_ne!(added, sig(&p), "a deletion");
    }

    #[test]
    fn build_output_vcs_data_and_editor_scratch_files_are_ignored() {
        let p = project();
        let base = sig(&p);
        for dir in ["target", ".git", ".rune", "node_modules"] {
            std::fs::create_dir_all(p.path().join(dir)).unwrap();
            std::fs::write(p.path().join(dir).join("noise"), "x").unwrap();
        }
        for file in [
            "src/.lib.rs.swp",
            "src/lib.rs~",
            "src/.#lib.rs",
            "src/#lib.rs#",
            "4913",
        ] {
            std::fs::write(p.path().join(file), "x").unwrap();
        }
        assert_eq!(base, sig(&p));
    }

    #[test]
    fn a_custom_target_directory_is_ignored_too() {
        let p = project();
        let out = p.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        let snap = Snapshot::new(p.path(), vec![out.clone()]);
        let base = snap.state();
        std::fs::write(out.join("artifact"), "x").unwrap();
        assert_eq!(base, snap.state());
    }

    #[test]
    fn what_a_run_writes_itself_is_not_a_change() {
        let p = project();
        let base = sig(&p);
        for f in ["Cargo.lock", "Rune.lock.bin"] {
            std::fs::write(p.path().join(f), "# created by the run").unwrap();
        }
        assert_eq!(base, sig(&p));
    }

    #[test]
    fn explicitly_ignored_files_and_directories_are_not_changes() {
        let p = project();
        std::fs::create_dir_all(p.path().join("generated")).unwrap();
        std::fs::write(p.path().join("out.txt"), "1").unwrap();
        let snap = Snapshot::new(
            p.path(),
            vec![p.path().join("generated"), p.path().join("out.txt")],
        );
        let base = snap.state();
        std::fs::write(p.path().join("generated/x.rs"), "x").unwrap();
        std::fs::write(p.path().join("out.txt"), "2222").unwrap();
        assert_eq!(base, snap.state());
    }

    #[test]
    fn changes_to_files_git_ignores_do_not_matter() {
        let p = project();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(p.path())
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        if !git(&["init", "-q"]) {
            return;
        }
        std::fs::write(p.path().join(".gitignore"), "*.log\nreports/\n").unwrap();
        let snap = Snapshot::new(p.path(), vec![]);
        let base = snap.state();
        std::fs::create_dir_all(p.path().join("reports")).unwrap();
        std::fs::write(p.path().join("reports/junit.xml"), "x").unwrap();
        std::fs::write(p.path().join("test.log"), "x").unwrap();
        let now = snap.state();
        assert_ne!(base, now);
        let changes = snap.changes(&base, &now);
        assert!(changes.is_empty(), "{changes:?}");
        std::fs::write(p.path().join("src/lib.rs"), "fn b() {}").unwrap();
        let changed = snap.changes(&base, &snap.state());
        assert_eq!(changed, [p.path().join("src/lib.rs")]);
        assert_eq!(
            describe(p.path(), &changed),
            Path::new("src").join("lib.rs").display().to_string()
        );
    }

    #[test]
    fn git_quoted_paths_are_read_back() {
        assert_eq!(git_unquote("src/lib.rs"), "src/lib.rs");
        assert_eq!(git_unquote(r#""a\\b.log""#), r"a\b.log");
        assert_eq!(git_unquote(r#""say \"hi\".txt""#), r#"say "hi".txt"#);
        assert_eq!(git_unquote(r#""tab\there""#), "tab\there");
        assert_eq!(git_unquote(r#""m\303\274nchen""#), "münchen");
    }

    #[test]
    fn commands_split_on_whitespace() {
        assert_eq!(split_command("test --lib  foo"), ["test", "--lib", "foo"]);
    }
}
