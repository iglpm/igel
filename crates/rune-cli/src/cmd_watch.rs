//! `rune watch`: re-run commands whenever the project's files change.
//!
//! Changes are found by polling file sizes and modification times — one walk of
//! a project costs about a millisecond — so there is no watcher daemon, no
//! platform-specific code and no missed events on network or container file
//! systems. Because every run is a normal `rune` invocation backed by the
//! content-addressed store, going back to code that was built before (undoing an
//! edit, switching branches) costs nothing.

use crate::args::WatchArgs;
use crate::ui;
use anyhow::{Context, Result};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// Directories that never contain sources.
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

/// Decides what counts as a change.
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

    /// A fingerprint of every relevant file's path, size and modification time.
    pub fn signature(&self) -> u64 {
        let mut entries: Vec<(PathBuf, u64, u128)> = Vec::new();
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
                    entries.push((path, meta.len(), mtime));
                }
            }
        }
        entries.sort();
        let mut h = DefaultHasher::new();
        entries.hash(&mut h);
        h.finish()
    }
}

/// Swap files, backups and lock files editors create while saving.
fn is_editor_scratch(name: &str) -> bool {
    name.ends_with('~')
        || name.ends_with(".swp")
        || name.ends_with(".swx")
        || name.ends_with(".tmp")
        || name.starts_with(".#")
        || (name.starts_with('#') && name.ends_with('#'))
        || name == "4913" // vim's write test file
}

/// Files a run of `rune` itself creates or rewrites in the project. Reacting to
/// them would make every run trigger the next one.
fn is_generated_by_rune(name: &str) -> bool {
    matches!(name, "Cargo.lock" | "Rune.lock.bin")
}

/// `-x "test --lib"` → `["test", "--lib"]`.
fn split_command(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_string).collect()
}

/// What one round of commands did.
enum Outcome {
    Finished {
        ok: bool,
    },
    /// Files changed while it ran; it was stopped.
    Interrupted,
}

pub fn watch(args: WatchArgs) -> Result<()> {
    let ws = crate::workspace::Workspace::load(None)
        .context("`rune watch` needs to be run inside a project")?;
    let root = ws.config.project_root.clone();
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
        let before = snapshot.signature();
        if args.clear {
            print!("\x1b[2J\x1b[H");
        }
        let started = Instant::now();
        let outcome = run_round(&exe, &commands, &root, &snapshot, before, interval)?;
        runs += 1;
        match outcome {
            Outcome::Finished { ok: true } => ui::summary(format!(
                "finished in {:.2}s · waiting for changes",
                started.elapsed().as_secs_f64()
            )),
            Outcome::Finished { ok: false } => ui::warn(format!(
                "failed after {:.2}s · waiting for changes",
                started.elapsed().as_secs_f64()
            )),
            Outcome::Interrupted => ui::action("restarting", "files changed"),
        }
        if args.max_runs.is_some_and(|max| runs >= max) {
            return Ok(());
        }
        if !matches!(outcome, Outcome::Interrupted) {
            wait_for_change(&snapshot, snapshot.signature(), interval);
        }
        // Editors save in bursts (write, rename, chmod); let it settle.
        settle(&snapshot, interval);
    }
}

/// Runs the commands in order, stopping at the first failure — or as soon as the
/// files change, so a stale run is not waited for.
fn run_round(
    exe: &Path,
    commands: &[Vec<String>],
    root: &Path,
    snapshot: &Snapshot,
    baseline: u64,
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
                if snapshot.signature() != baseline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok(Outcome::Interrupted);
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    Ok(Outcome::Finished { ok: true })
}

fn wait_for_change(snapshot: &Snapshot, baseline: u64, interval: Duration) {
    while snapshot.signature() == baseline {
        std::thread::sleep(interval);
    }
}

/// Returns once the files have stopped changing for one interval.
fn settle(snapshot: &Snapshot, interval: Duration) {
    let mut last = snapshot.signature();
    loop {
        std::thread::sleep(interval.min(Duration::from_millis(150)));
        let now = snapshot.signature();
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

    fn sig(dir: &tempfile::TempDir) -> u64 {
        Snapshot::new(dir.path(), vec![]).signature()
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
        let base = snap.signature();
        std::fs::write(out.join("artifact"), "x").unwrap();
        assert_eq!(base, snap.signature());
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
        let base = snap.signature();
        std::fs::write(p.path().join("generated/x.rs"), "x").unwrap();
        std::fs::write(p.path().join("out.txt"), "2222").unwrap();
        assert_eq!(base, snap.signature());
    }

    #[test]
    fn commands_split_on_whitespace() {
        assert_eq!(split_command("test --lib  foo"), ["test", "--lib", "foo"]);
    }
}
