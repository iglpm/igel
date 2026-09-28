//! `rune unused`: dependencies a package declares but never uses.
//!
//! Asks the compiler rather than searching the source for names (which is
//! fast but fooled by macros, renames and `extern crate`): the workspace's
//! crates are checked with rustc's `unused_crate_dependencies` lint, which
//! reports every `--extern` crate a compilation never referred to. A
//! dependency is unused when *every* compilation that was given it — the
//! library, binaries, tests, examples and benchmarks for `[dependencies]`;
//! tests, examples and benchmarks for `[dev-dependencies]`; the build script
//! for `[build-dependencies]` — reports it. This is `rune check
//! --all-targets` with one lint added for the workspace's own crates, so the
//! dependencies themselves come from the store as usual.

use anyhow::{Context, Result};
use colored::Colorize;
use rune_graph::{BuildGraph, NodeId};
use rune_scheduler::PipelineEvent;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use toml_edit::{DocumentMut, Item};

use crate::args::UnusedArgs;
use crate::cmd_build::plan_for;
use crate::graph::{Mode, TargetSelection};
use crate::session::{Session, drive};
use crate::ui;

const LINT: &str = "unused_crate_dependencies";

/// One dependency entry of a member's manifest.
#[derive(Debug, Clone)]
struct Entry {
    /// `[dependencies]`, `[target.'cfg(unix)'.dev-dependencies]`, …
    section: String,
    /// The path of tables to the entry.
    table: Vec<String>,
    key: String,
    kind: Kind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Normal,
    Dev,
    Build,
}

/// The dependency entries of a manifest.
fn entries(doc: &DocumentMut) -> Vec<Entry> {
    let mut out = Vec::new();
    let mut add = |prefix: Vec<String>, label: String, item: &Item| {
        for (name, kind) in [
            ("dependencies", Kind::Normal),
            ("dev-dependencies", Kind::Dev),
            ("build-dependencies", Kind::Build),
        ] {
            let Some(table) = item.get(name).and_then(Item::as_table_like) else {
                continue;
            };
            let mut path = prefix.clone();
            path.push(name.to_string());
            for (key, _) in table.iter() {
                out.push(Entry {
                    section: format!("[{label}{name}]"),
                    table: path.clone(),
                    key: key.to_string(),
                    kind,
                });
            }
        }
    };
    add(Vec::new(), String::new(), doc.as_item());
    if let Some(targets) = doc.get("target").and_then(Item::as_table_like) {
        for (platform, item) in targets.iter() {
            add(
                vec!["target".into(), platform.to_string()],
                format!("target.'{platform}'."),
                item,
            );
        }
    }
    out
}

/// The crate names rustc reports as unused in these diagnostics (JSON lines
/// from a compilation, or rendered text from a build script's).
fn reported_unused(lines: &[String]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut take = |message: &str| {
        if let Some(rest) = message.split("extern crate `").nth(1)
            && let Some(name) = rest.split('`').next()
            && message.contains("is unused in crate")
        {
            out.insert(name.to_string());
        }
    };
    for line in lines {
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(v) => {
                if v["code"]["code"].as_str() == Some(LINT)
                    && let Some(m) = v["message"].as_str()
                {
                    take(m);
                }
            }
            Err(_) => {
                for l in line.lines() {
                    take(l);
                }
            }
        }
    }
    out
}

/// The `--extern` names a unit gets for its dependencies, and for its build
/// script's.
fn externs(graph: &BuildGraph, id: NodeId, build: bool) -> BTreeSet<String> {
    let node = graph.node(id);
    let (set, aliases) = if build {
        (&node.build_dependencies, &node.build_aliases)
    } else {
        (&node.normal_dependencies, &node.extern_aliases)
    };
    set.iter()
        .filter(|d| {
            // A package's binaries and tests link its own library; that is not
            // a declared dependency.
            build || graph.node(**d).unit.manifest_dir != node.unit.manifest_dir
        })
        .map(|d| {
            aliases
                .get(d)
                .map(|a| a.replace('-', "_"))
                .unwrap_or_else(|| graph.node(*d).unit.rustc_crate_name())
        })
        .collect()
}

/// Whether the package's Rust sources name `krate` as a path (`krate::`,
/// `use krate`, `extern crate krate`): code the compiler did not see in this
/// configuration — under a `cfg` for another platform or a feature that is
/// off — may still use it.
fn mentioned_in_sources(dir: &std::path::Path, krate: &str) -> bool {
    let needles = [
        format!("{krate}::"),
        format!("use {krate}"),
        format!("extern crate {krate}"),
    ];
    let boundary = |text: &str, at: usize| {
        at == 0
            || !text[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_')
    };
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            e.depth() == 0 || !(name.starts_with('.') || name == "target")
        })
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "rs"))
        .any(|e| {
            let Ok(text) = std::fs::read_to_string(e.path()) else {
                return false;
            };
            needles.iter().any(|n| {
                text.match_indices(n.as_str())
                    .any(|(at, _)| boundary(&text, at))
            })
        })
}

/// What checking found for one member.
#[derive(Default)]
struct Findings {
    unused: Vec<Entry>,
    /// Unused in what was compiled, but named in the sources (probably used
    /// under a `cfg` this build did not enable): reported, never removed.
    unused_here: Vec<Entry>,
    /// Ordinary dependencies only tests, examples or benchmarks use.
    dev_only: Vec<Entry>,
    /// Entries no compilation was given (disabled optional dependencies,
    /// other platforms, a kind of target the package does not have).
    unchecked: Vec<Entry>,
}

pub fn unused(args: UnusedArgs) -> Result<()> {
    let mut sess = Session::new(&args.common, "dev")?;
    // The lint's findings are the report; other warnings belong to `check`.
    sess.show_warnings = false;
    sess.ws.config.ensure_dirs(sess.profile.output_dir_name())?;
    let targets = TargetSelection {
        lib: false,
        bins: false,
        bin: Vec::new(),
        examples: false,
        example: Vec::new(),
        tests: false,
        test: Vec::new(),
        benches: false,
        bench: Vec::new(),
        all_targets: true,
        doc_only: false,
    };
    let mut plan = plan_for(&sess, Mode::Check, &targets)?;
    let lint = format!("-W{}", LINT.replace('_', "-"));
    for id in plan.graph.nodes().iter().map(|n| n.id).collect::<Vec<_>>() {
        let unit = &mut plan.graph.node_mut(id).unit;
        if unit.ext.member {
            unit.ext.rustflags.push(lint.clone());
            unit.ext.script_rustflags.push(lint.clone());
        }
    }
    ui::action(
        "checking",
        format!("{} package(s) for unused dependencies", sess.selected.len()),
    );

    let mut diagnostics: HashMap<NodeId, Vec<String>> = HashMap::new();
    let mut script_output: HashMap<NodeId, Vec<String>> = HashMap::new();
    let mut failed: HashSet<NodeId> = HashSet::new();
    let graph = &plan.graph;
    drive(&sess, graph, |event| match event {
        PipelineEvent::BuildFinished {
            node_id,
            diagnostics: d,
            ..
        }
        | PipelineEvent::CacheHit {
            node_id,
            diagnostics: d,
            ..
        } if graph.node(*node_id).unit.ext.member => {
            diagnostics
                .entry(*node_id)
                .or_default()
                .extend(d.iter().cloned());
        }
        PipelineEvent::BuildFailed { node_id, .. } => {
            failed.insert(*node_id);
        }
        PipelineEvent::ScriptWarnings { node_id, warnings } => {
            script_output
                .entry(*node_id)
                .or_default()
                .push(warnings.clone());
        }
        _ => {}
    })?;

    // Per member: what each of its compilations was given and left unused.
    let mut report: BTreeMap<String, (PathBuf, Findings)> = BTreeMap::new();
    let mut incomplete = Vec::new();
    for &mi in &sess.selected {
        let member = &sess.ws.members[mi];
        let text = std::fs::read_to_string(&member.manifest_path)?;
        let doc: DocumentMut = text
            .parse()
            .with_context(|| format!("failed to parse {}", member.manifest_path.display()))?;
        let units: Vec<NodeId> = graph
            .nodes()
            .iter()
            .filter(|n| n.unit.ext.member && n.unit.manifest_dir == member.dir)
            .map(|n| n.id)
            .collect();
        if units.iter().any(|u| failed.contains(u)) {
            incomplete.push(member.name.clone());
            continue;
        }
        // (given, left unused) by each compilation, and by the build script.
        let mut compilations: Vec<(bool, BTreeSet<String>, BTreeSet<String>)> = Vec::new();
        for &u in &units {
            let unit = &graph.node(u).unit;
            let dev_capable = unit.harness
                || matches!(
                    unit.kind,
                    rune_common::TargetKind::Test
                        | rune_common::TargetKind::Bench
                        | rune_common::TargetKind::Example
                );
            compilations.push((
                dev_capable,
                externs(graph, u, false),
                reported_unused(diagnostics.get(&u).map_or(&[][..], |d| d.as_slice())),
            ));
        }
        let script = units
            .iter()
            .find(|u| {
                let unit = &graph.node(**u).unit;
                unit.build_script.is_some() && unit.ext.script_owner.is_none()
            })
            .map(|&u| {
                (
                    externs(graph, u, true),
                    reported_unused(script_output.get(&u).map_or(&[][..], |d| d.as_slice())),
                )
            });

        let mut findings = Findings::default();
        for entry in entries(&doc) {
            let name = entry.key.replace('-', "_");
            match entry.kind {
                Kind::Build => match &script {
                    Some((given, unused)) if given.contains(&name) => {
                        if unused.contains(&name) {
                            findings.unused.push(entry);
                        }
                    }
                    _ => findings.unchecked.push(entry),
                },
                kind => {
                    let given: Vec<&(bool, BTreeSet<String>, BTreeSet<String>)> = compilations
                        .iter()
                        .filter(|(dev, g, _)| g.contains(&name) && (kind == Kind::Normal || *dev))
                        .collect();
                    if given.is_empty() {
                        findings.unchecked.push(entry);
                        continue;
                    }
                    let used_by: Vec<&&(bool, BTreeSet<String>, BTreeSet<String>)> = given
                        .iter()
                        .filter(|(_, _, u)| !u.contains(&name))
                        .collect();
                    if used_by.is_empty() {
                        findings.unused.push(entry);
                    } else if kind == Kind::Normal
                        && used_by.iter().all(|(dev, _, _)| *dev)
                        && given.iter().any(|(dev, _, _)| !*dev)
                    {
                        findings.dev_only.push(entry);
                    }
                }
            }
        }
        let (here, unused): (Vec<Entry>, Vec<Entry>) = std::mem::take(&mut findings.unused)
            .into_iter()
            .partition(|e| mentioned_in_sources(&member.dir, &e.key.replace('-', "_")));
        findings.unused = unused;
        findings.unused_here = here;
        report.insert(
            member.name.clone(),
            (member.manifest_path.clone(), findings),
        );
    }

    let unused_count: usize = report.values().map(|(_, f)| f.unused.len()).sum();
    let describe = |list: &[Entry]| {
        list.iter()
            .map(|e| format!("{} {}", e.key.bold(), e.section.dimmed()))
            .collect::<Vec<_>>()
            .join(", ")
    };
    for (name, (_, f)) in &report {
        if f.unused.is_empty() && f.dev_only.is_empty() && f.unused_here.is_empty() {
            continue;
        }
        println!("{}", name.bold());
        if !f.unused.is_empty() {
            println!("  unused: {}", describe(&f.unused));
        }
        if !f.unused_here.is_empty() {
            println!(
                "  unused in this configuration, but named in the source (another platform or \
                 feature?): {}",
                describe(&f.unused_here)
            );
        }
        if !f.dev_only.is_empty() {
            println!(
                "  only used by tests, examples or benchmarks (could be dev-dependencies): {}",
                describe(&f.dev_only)
            );
        }
    }
    for name in &incomplete {
        ui::warn(format!(
            "`{name}` does not compile, so its dependencies were not checked"
        ));
    }
    let unchecked: usize = report.values().map(|(_, f)| f.unchecked.len()).sum();
    if unchecked > 0 {
        ui::hint(format!(
            "{unchecked} dependencies were not given to any compilation here (optional ones \
             whose feature is off, other platforms' — `--all-features` checks more)"
        ));
    }

    if unused_count == 0 {
        ui::summary("no unused dependencies");
        return Ok(());
    }
    if args.fix {
        let mut removed = 0;
        for (path, f) in report.values() {
            if f.unused.is_empty() {
                continue;
            }
            let text = std::fs::read_to_string(path)?;
            let mut doc: DocumentMut = text.parse()?;
            for e in &f.unused {
                let mut item: &mut Item = doc.as_item_mut();
                for seg in &e.table {
                    item = &mut item[seg.as_str()];
                }
                if let Some(t) = item.as_table_like_mut()
                    && t.remove(&e.key).is_some()
                {
                    removed += 1;
                }
            }
            std::fs::write(path, doc.to_string())?;
        }
        let ws = crate::workspace::Workspace::load(Some(&sess.ws.manifest_path()))?;
        crate::cmd_deps::refresh_lockfile(&ws, sess.offline, false, &[])?;
        ui::summary(format!("removed {removed} unused dependencies"));
        return Ok(());
    }
    ui::hint(
        "a crate that is only linked, never named (a `-sys` crate for its library, one used \
         for its side effects), looks unused; keep it with `use the_crate as _;`, or remove the \
         rest with `rune unused --fix`",
    );
    ui::error(format!("{unused_count} unused dependencies"));
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lint_is_read_from_json_and_from_rendered_text() {
        let json = r#"{"$message_type":"diagnostic","message":"extern crate `serde_json` is unused in crate `app`","code":{"code":"unused_crate_dependencies","explanation":null},"level":"warning"}"#;
        let other = r#"{"$message_type":"diagnostic","message":"unused variable: `x`","code":{"code":"unused_variables"},"level":"warning"}"#;
        let got = reported_unused(&[json.into(), other.into()]);
        assert_eq!(got.into_iter().collect::<Vec<_>>(), ["serde_json"]);
        let text = "warning: extern crate `cc` is unused in crate `build_script_build`\n  |\n";
        assert_eq!(
            reported_unused(&[text.into()])
                .into_iter()
                .collect::<Vec<_>>(),
            ["cc"]
        );
    }

    #[test]
    fn every_kind_of_dependency_table_is_found() {
        let doc: DocumentMut = "[dependencies]\na = \"1\"\n[dev-dependencies]\nb-c = \"1\"\n\
             [build-dependencies]\nd = \"1\"\n[target.'cfg(unix)'.dependencies]\ne = \"1\"\n"
            .parse()
            .unwrap();
        let got: Vec<(String, String)> = entries(&doc)
            .into_iter()
            .map(|e| (e.key, e.section))
            .collect();
        assert_eq!(
            got,
            [
                ("a".to_string(), "[dependencies]".to_string()),
                ("b-c".into(), "[dev-dependencies]".into()),
                ("d".into(), "[build-dependencies]".into()),
                ("e".into(), "[target.'cfg(unix)'.dependencies]".into()),
            ]
        );
    }
}
