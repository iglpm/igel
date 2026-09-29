//! `rune explain`: which units a build would compile, and why — what changed
//! in each since it was last built here (files, dependencies, toolchain,
//! flags, features…). Every build records what each unit's key was made of
//! (`.rune/fingerprints/identities.json`); this compares that with now.

use crate::args::BuildArgs;
use crate::cmd_build::{plan_for, selection};
use crate::graph::Mode;
use crate::session::Session;
use crate::ui;
use anyhow::Result;
use colored::Colorize;
use rune_scheduler::executor::{UnitIdentity, load_identities};
use std::collections::BTreeSet;

pub fn explain(args: BuildArgs) -> Result<()> {
    let sess = Session::new(&args.common, "dev")?;
    let plan = plan_for(&sess, Mode::Build, &selection(&args.targets))?;
    let executor = sess.executor();
    let current = executor.current_identities(&plan.graph)?;
    let previous = load_identities(&sess.config().fingerprint_dir);

    let mut roots = Vec::new();
    let mut downstream = Vec::new();
    let mut restored = 0;
    for (_, label, cur) in &current {
        if executor.is_cached(&cur.key) {
            restored += 1;
            continue;
        }
        let (reasons, only_dependencies) = match previous.get(label) {
            None => (vec!["not built in this project before".to_string()], false),
            Some(prev) if prev.key == cur.key => (
                vec![
                    "built before, but its result is no longer in the store \
                      (removed by `rune cas gc`?)"
                        .to_string(),
                ],
                false,
            ),
            Some(prev) => describe(prev, cur),
        };
        if only_dependencies {
            downstream.push((label, reasons));
        } else {
            roots.push((label, reasons));
        }
    }

    let total = current.len();
    let compiled = roots.len() + downstream.len();
    if compiled == 0 {
        ui::summary(format!(
            "nothing would be compiled: all {total} units are in the store"
        ));
        return Ok(());
    }
    ui::err(format!(
        "{} {} {}",
        ui::brand("▸"),
        format!("{compiled} of {total} units would be compiled").bold(),
        format!("({restored} come from the store)").dimmed()
    ));
    for (label, reasons) in roots.iter().chain(&downstream) {
        ui::out("");
        ui::out(format!("  {}", label.bold()));
        for r in reasons {
            ui::out(format!("    {} {r}", "·".dimmed()));
        }
    }
    Ok(())
}

/// What differs between two identities of a unit, and whether it is only
/// that dependencies changed.
fn describe(prev: &UnitIdentity, cur: &UnitIdentity) -> (Vec<String>, bool) {
    let parts: BTreeSet<&String> = prev.parts.keys().chain(cur.parts.keys()).collect();
    let mut reasons = Vec::new();
    let mut deps = Vec::new();
    for part in parts {
        let (a, b) = (prev.parts.get(part), cur.parts.get(part));
        if a == b {
            continue;
        }
        if let Some(dep) = part
            .strip_prefix("dependency ")
            .or_else(|| part.strip_prefix("build dependency "))
        {
            deps.push(match (a, b) {
                (None, _) => format!("{dep} (new)"),
                (_, None) => format!("{dep} (removed)"),
                _ => dep.to_string(),
            });
            continue;
        }
        reasons.push(match part.as_str() {
            "sources" => format!("sources changed: {}", file_changes(prev, cur)),
            "unit (features, crate type, edition, target)" => {
                "its features, crate type, edition or target changed".to_string()
            }
            "toolchain" => "the toolchain changed (rustc, or a compiler wrapper)".to_string(),
            other => match (a, b) {
                (None, _) => format!("{other}: now set"),
                (_, None) => format!("{other}: no longer set"),
                _ => format!("{other} changed"),
            },
        });
    }
    // What a build script read is looked up under the rest of the key, so
    // it "changes" whenever anything else does: only a cause on its own.
    const OUTSIDE: &str = "outside inputs (environment variables and files it read) changed";
    if reasons.len() > 1 || !deps.is_empty() {
        reasons.retain(|r| r != OUTSIDE);
    }
    let only_dependencies = reasons.is_empty() && !deps.is_empty();
    if !deps.is_empty() {
        reasons.push(format!(
            "{} changed: {}",
            if deps.len() == 1 {
                "a dependency"
            } else {
                "dependencies"
            },
            deps.join(", ")
        ));
    }
    if reasons.is_empty() {
        reasons.push("its key changed".to_string());
    }
    (reasons, only_dependencies)
}

/// `src/a.rs (modified), src/b.rs (added)`, at most eight.
fn file_changes(prev: &UnitIdentity, cur: &UnitIdentity) -> String {
    let mut changes = Vec::new();
    let names: BTreeSet<&String> = prev.files.keys().chain(cur.files.keys()).collect();
    for name in names {
        match (prev.files.get(name), cur.files.get(name)) {
            (Some(a), Some(b)) if a != b => changes.push(format!("{name} (modified)")),
            (None, Some(_)) => changes.push(format!("{name} (added)")),
            (Some(_), None) => changes.push(format!("{name} (removed)")),
            _ => {}
        }
    }
    if changes.is_empty() {
        return "files outside the package (a crate root or build script)".to_string();
    }
    let more = changes.len().saturating_sub(8);
    changes.truncate(8);
    if more > 0 {
        changes.push(format!("and {more} more"));
    }
    changes.join(", ")
}
