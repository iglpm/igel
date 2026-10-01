//! What is measured. Every scenario is the same command for both tools, run
//! from the same state, so the comparison is like for like.

use anyhow::{Result, bail};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::tool::Ctx;
use crate::workload::{Workload, apply_edit};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Wall time of a command.
    Time,
    /// Bytes on disk after a cold build.
    Disk,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Needs {
    Nothing,
    LeafEdit,
    CoreEdit,
    TestEdit,
}

pub struct Scenario {
    pub id: &'static str,
    pub category: &'static str,
    pub title: &'static str,
    /// One sentence: what situation this is.
    pub about: &'static str,
    pub kind: Kind,
    /// Expensive (a build from scratch): fewer runs, no warm-up.
    pub heavy: bool,
    /// Only in the `full` suite.
    pub full_only: bool,
    pub needs: Needs,
    /// The command measured.
    pub args: &'static [&'static str],
    /// Brings the project into the starting state, once.
    pub prepare: fn(&Ctx, &Workload) -> Result<()>,
    /// Before each run (warm-ups included): `i` counts runs.
    pub before: fn(&Ctx, &Workload, usize) -> Result<()>,
}

fn nothing(_: &Ctx, _: &Workload) -> Result<()> {
    Ok(())
}
fn nothing_each(_: &Ctx, _: &Workload, _: usize) -> Result<()> {
    Ok(())
}

/// Runs a setup command, failing loudly: a scenario must not measure from a
/// broken state.
pub fn setup(ctx: &Ctx, args: &[&str]) -> Result<()> {
    let out = crate::measure::run(
        &mut ctx.tool.command(ctx, args),
        std::time::Duration::from_secs(3600),
    )?;
    if !out.sample.ok {
        let tail: String = out
            .output
            .lines()
            .rev()
            .take(30)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        bail!(
            "`{} {}` failed while preparing:\n{tail}",
            ctx.tool.name,
            args.join(" ")
        );
    }
    Ok(())
}

/// Edit states that never repeat within a benchmark run (a repeated state
/// would be a legitimate cache hit, not an edit).
static NEXT_EDIT: AtomicU64 = AtomicU64::new(1_000);

fn fresh_edit() -> u64 {
    NEXT_EDIT.fetch_add(1, Ordering::Relaxed)
}

fn edit(ctx: &Ctx, file: Option<&Path>, n: u64) -> Result<()> {
    match file {
        Some(f) => apply_edit(&ctx.dir, f, n),
        None => bail!("this workload names no file to edit"),
    }
}

/// Writes a file no crate reads (`NOTES.md`) into the package of `file`.
fn touch_unread(ctx: &Ctx, file: Option<&Path>, n: u64) -> Result<()> {
    let Some(file) = file else {
        bail!("this workload names no file to edit");
    };
    let path = ctx.dir.join(file);
    let Some(package) = path
        .ancestors()
        .skip(1)
        .find(|d| d.join("Cargo.toml").is_file())
    else {
        bail!("{} is in no package", file.display());
    };
    std::fs::write(package.join("NOTES.md"), format!("notes {n}\n"))?;
    Ok(())
}

/// The catalogue, in report order.
pub fn all() -> Vec<Scenario> {
    vec![
        Scenario {
            id: "startup",
            category: "Startup",
            title: "--version",
            about: "Starting the tool at all: paid by every command, editor integration and script.",
            kind: Kind::Time,
            heavy: false,
            full_only: false,
            needs: Needs::Nothing,
            args: &["--version"],
            prepare: nothing,
            before: nothing_each,
        },
        Scenario {
            id: "cold-build",
            category: "Cold build",
            title: "build from scratch",
            about: "No build output and no cache: a new machine or a CI runner without one.",
            kind: Kind::Time,
            heavy: true,
            full_only: false,
            needs: Needs::Nothing,
            args: &["build"],
            prepare: nothing,
            before: |ctx, _, _| ctx.tool.remove_caches(ctx),
        },
        Scenario {
            id: "cold-check",
            category: "Cold build",
            title: "check from scratch",
            about: "Type-checking everything with no build output and no cache.",
            kind: Kind::Time,
            heavy: true,
            full_only: false,
            needs: Needs::Nothing,
            args: &["check"],
            prepare: nothing,
            before: |ctx, _, _| ctx.tool.remove_caches(ctx),
        },
        Scenario {
            id: "cold-release",
            category: "Cold build",
            title: "release build from scratch",
            about: "An optimized build with no build output and no cache.",
            kind: Kind::Time,
            heavy: true,
            full_only: true,
            needs: Needs::Nothing,
            args: &["build", "--release"],
            prepare: nothing,
            before: |ctx, _, _| ctx.tool.remove_caches(ctx),
        },
        Scenario {
            id: "disk",
            category: "Disk",
            title: "disk used by a dev build",
            about: "Build output in the project after a cold build (Rune: plus its shared store).",
            kind: Kind::Disk,
            heavy: true,
            full_only: false,
            needs: Needs::Nothing,
            args: &["build"],
            prepare: nothing,
            before: |ctx, _, _| ctx.tool.remove_caches(ctx),
        },
        Scenario {
            id: "noop-build",
            category: "Nothing changed",
            title: "build",
            about: "Building when nothing changed: the cost of finding out there is nothing to do.",
            kind: Kind::Time,
            heavy: false,
            full_only: false,
            needs: Needs::Nothing,
            args: &["build"],
            prepare: |ctx, _| setup(ctx, &["build"]),
            before: nothing_each,
        },
        Scenario {
            id: "noop-check",
            category: "Nothing changed",
            title: "check",
            about: "What an editor runs on every save, when nothing changed.",
            kind: Kind::Time,
            heavy: false,
            full_only: false,
            needs: Needs::Nothing,
            args: &["check"],
            prepare: |ctx, _| setup(ctx, &["check"]),
            before: nothing_each,
        },
        Scenario {
            id: "noop-test",
            category: "Nothing changed",
            title: "test --no-run",
            about: "Building the tests when nothing changed.",
            kind: Kind::Time,
            heavy: false,
            full_only: false,
            needs: Needs::Nothing,
            args: &["test", "--no-run"],
            prepare: |ctx, _| setup(ctx, &["test", "--no-run"]),
            before: nothing_each,
        },
        Scenario {
            id: "edit-leaf",
            category: "After an edit",
            title: "build after editing the app",
            about: "The edit-compile loop: one crate at the top of the graph changed.",
            kind: Kind::Time,
            heavy: false,
            full_only: false,
            needs: Needs::LeafEdit,
            args: &["build"],
            prepare: |ctx, _| setup(ctx, &["build"]),
            before: |ctx, w, _| edit(ctx, w.leaf_edit.as_deref(), fresh_edit()),
        },
        Scenario {
            id: "edit-core",
            category: "After an edit",
            title: "build after editing the core crate",
            about: "A change at the bottom of the graph: everything above it rebuilds.",
            kind: Kind::Time,
            heavy: false,
            full_only: false,
            needs: Needs::CoreEdit,
            args: &["build"],
            prepare: |ctx, _| setup(ctx, &["build"]),
            before: |ctx, w, _| edit(ctx, w.core_edit.as_deref(), fresh_edit()),
        },
        Scenario {
            id: "edit-leaf-check",
            category: "After an edit",
            title: "check after editing the app",
            about: "An editor's check after a change to one crate.",
            kind: Kind::Time,
            heavy: false,
            full_only: false,
            needs: Needs::LeafEdit,
            args: &["check"],
            prepare: |ctx, _| setup(ctx, &["check"]),
            before: |ctx, w, _| edit(ctx, w.leaf_edit.as_deref(), fresh_edit()),
        },
        Scenario {
            id: "edit-leaf-test",
            category: "After an edit",
            title: "test after editing the app",
            about: "Edit, then run the tests: rebuild what changed and run every test binary.",
            kind: Kind::Time,
            heavy: false,
            full_only: false,
            needs: Needs::LeafEdit,
            args: &["test"],
            prepare: |ctx, _| setup(ctx, &["test", "--no-run"]),
            before: |ctx, w, _| edit(ctx, w.leaf_edit.as_deref(), fresh_edit()),
        },
        Scenario {
            id: "edit-test",
            category: "After an edit",
            title: "test after editing an integration test",
            about: "Fixing a test: only that test needs compiling (its package has a build script).",
            kind: Kind::Time,
            heavy: false,
            full_only: false,
            needs: Needs::TestEdit,
            args: &["test"],
            prepare: |ctx, _| setup(ctx, &["test", "--no-run"]),
            before: |ctx, w, _| edit(ctx, w.test_edit.as_deref(), fresh_edit()),
        },
        Scenario {
            id: "edit-unread",
            category: "After an edit",
            title: "build after a file nothing reads changed",
            about: "Notes or a log written into the core crate's directory: there is nothing to \
                    compile.",
            kind: Kind::Time,
            heavy: false,
            full_only: false,
            needs: Needs::CoreEdit,
            args: &["build"],
            prepare: |ctx, _| setup(ctx, &["build"]),
            before: |ctx, w, _| touch_unread(ctx, w.core_edit.as_deref(), fresh_edit()),
        },
        Scenario {
            id: "fresh-checkout",
            category: "Caching",
            title: "build in a fresh checkout",
            about: "No build output, but this machine built the same code before: a second worktree, \
                    `git clean`, a CI runner with a cache.",
            kind: Kind::Time,
            heavy: true,
            full_only: false,
            needs: Needs::Nothing,
            args: &["build"],
            prepare: |ctx, _| setup(ctx, &["build"]),
            before: |ctx, _, _| ctx.tool.remove_output(ctx),
        },
        Scenario {
            id: "branch-switch",
            category: "Caching",
            title: "build after switching back to a branch",
            about: "Two branches that differ in the core crate, both built before; switching back \
                    and forth.",
            kind: Kind::Time,
            heavy: true,
            full_only: false,
            needs: Needs::CoreEdit,
            args: &["build"],
            prepare: |ctx, w| {
                edit(ctx, w.core_edit.as_deref(), 1)?;
                setup(ctx, &["build"])?;
                edit(ctx, w.core_edit.as_deref(), 2)?;
                setup(ctx, &["build"])
            },
            // State 2 is built: switch to 1, then 2, then 1, …
            before: |ctx, w, i| edit(ctx, w.core_edit.as_deref(), 1 + (i % 2) as u64),
        },
        Scenario {
            id: "test-run",
            category: "Tests",
            title: "test",
            about: "Running every test binary when the tests are already built.",
            kind: Kind::Time,
            heavy: false,
            full_only: false,
            needs: Needs::Nothing,
            args: &["test"],
            prepare: |ctx, _| setup(ctx, &["test", "--no-run"]),
            before: nothing_each,
        },
        Scenario {
            id: "metadata",
            category: "Inspection",
            title: "metadata",
            about: "What IDEs and tools ask on startup (`metadata --format-version 1`).",
            kind: Kind::Time,
            heavy: false,
            full_only: false,
            needs: Needs::Nothing,
            args: &["metadata", "--format-version", "1"],
            prepare: |ctx, _| setup(ctx, &["generate-lockfile"]),
            before: nothing_each,
        },
        Scenario {
            id: "tree",
            category: "Inspection",
            title: "tree",
            about: "Printing the dependency tree.",
            kind: Kind::Time,
            heavy: false,
            full_only: false,
            needs: Needs::Nothing,
            args: &["tree"],
            prepare: |ctx, _| setup(ctx, &["generate-lockfile"]),
            before: nothing_each,
        },
        Scenario {
            id: "lockfile",
            category: "Inspection",
            title: "generate-lockfile",
            about: "Resolving the dependency graph from scratch (may use the network for registry \
                    dependencies).",
            kind: Kind::Time,
            heavy: false,
            full_only: true,
            needs: Needs::Nothing,
            args: &["generate-lockfile"],
            prepare: nothing,
            before: |ctx, _, _| {
                for f in ["Cargo.lock", "Rune.lock.bin"] {
                    let _ = std::fs::remove_file(ctx.dir.join(f));
                }
                Ok(())
            },
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenario_ids_are_unique_and_categories_grouped() {
        let all = all();
        let mut ids: Vec<_> = all.iter().map(|s| s.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), all.len());
        // A category's scenarios are listed together.
        let mut seen: Vec<&str> = Vec::new();
        for s in &all {
            if seen.last() != Some(&s.category) {
                assert!(!seen.contains(&s.category), "{} is split", s.category);
                seen.push(s.category);
            }
        }
    }
}
