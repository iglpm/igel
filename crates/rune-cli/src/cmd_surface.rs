//! `rune tree --exec-surface`: crates that run code at build time (build scripts, proc-macros, `links`).

use anyhow::Result;
use colored::Colorize;
use rune_common::CrateType;
use rune_graph::{BuildGraph, NodeId};
use std::collections::{BTreeMap, HashMap, VecDeque};

use crate::args::{CommonArgs, TreeArgs};
use crate::cmd_build::plan_for;
use crate::graph::{Mode, TargetSelection};
use crate::session::Session;

#[derive(Debug, Clone, serde::Serialize)]
struct Item {
    name: String,
    version: String,
    runs: Vec<&'static str>,
    links: Option<String>,
    workspace: bool,
    via: Vec<String>,
}

fn paths_from_members(graph: &BuildGraph) -> HashMap<NodeId, Vec<NodeId>> {
    let mut parent: HashMap<NodeId, Option<NodeId>> = HashMap::new();
    let mut queue: VecDeque<NodeId> = VecDeque::new();
    for n in graph.nodes() {
        if n.unit.ext.member {
            parent.insert(n.id, None);
            queue.push_back(n.id);
        }
    }
    while let Some(id) = queue.pop_front() {
        for &d in &graph.node(id).dependencies {
            if let std::collections::hash_map::Entry::Vacant(e) = parent.entry(d) {
                e.insert(Some(id));
                queue.push_back(d);
            }
        }
    }
    parent
        .keys()
        .map(|&id| {
            let mut path = vec![id];
            let mut cur = id;
            while let Some(Some(p)) = parent.get(&cur) {
                path.push(*p);
                cur = *p;
            }
            path.reverse();
            (id, path)
        })
        .collect()
}

pub fn exec_surface(args: &TreeArgs) -> Result<()> {
    let mut common = CommonArgs::defaults_for(&args.manifest);
    common.package = args.package.clone();
    common.workspace = args.workspace;
    let sess = Session::new(&common, "dev")?;
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
    let plan = plan_for(&sess, Mode::Build, &targets)?;
    let graph = &plan.graph;
    let paths = paths_from_members(graph);

    let mut items: BTreeMap<(String, String), Item> = BTreeMap::new();
    let mut packages: std::collections::HashSet<(String, String)> = Default::default();
    for n in graph.nodes() {
        let u = &n.unit;
        let key = (u.crate_id.name.clone(), u.crate_id.version.to_string());
        packages.insert(key.clone());
        let mut runs = Vec::new();
        if u.build_script.is_some() && u.ext.script_owner.is_none() {
            runs.push("build-script");
        }
        if u.crate_type == CrateType::ProcMacro {
            runs.push("proc-macro");
        }
        if u.links.is_some() {
            runs.push("links");
        }
        if runs.is_empty() {
            continue;
        }
        let via = paths
            .get(&n.id)
            .map(|p| {
                p.iter()
                    .map(|id| graph.node(*id).unit.crate_id.name.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let entry = items.entry(key).or_insert_with(|| Item {
            name: u.crate_id.name.clone(),
            version: u.crate_id.version.to_string(),
            runs: Vec::new(),
            links: u.links.clone(),
            workspace: u.ext.member,
            via: via.clone(),
        });
        for r in runs {
            if !entry.runs.contains(&r) {
                entry.runs.push(r);
            }
        }
        if !via.is_empty() && (entry.via.is_empty() || via.len() < entry.via.len()) {
            entry.via = via;
        }
    }
    let list: Vec<Item> = items.into_values().collect();

    if args.format == "json" {
        println!("{}", serde_json::to_string_pretty(&list)?);
        return Ok(());
    }
    let third_party = list.iter().filter(|i| !i.workspace).count();
    println!(
        "{} of {} packages run code on this machine while building ({} of them your own):",
        list.len(),
        packages.len(),
        list.len() - third_party
    );
    for (label, kind) in [
        (
            "build scripts (programs run before compiling their package)",
            "build-script",
        ),
        ("proc-macros (run inside the compiler)", "proc-macro"),
        ("native code (`links`)", "links"),
    ] {
        let group: Vec<&Item> = list.iter().filter(|i| i.runs.contains(&kind)).collect();
        if group.is_empty() {
            continue;
        }
        println!("\n{} ({})", label.bold(), group.len());
        let width = group
            .iter()
            .map(|i| i.name.len() + i.version.len() + 1)
            .max()
            .unwrap_or(0);
        for i in group {
            let id = format!("{} {}", i.name, i.version);
            let mut line = format!("  {id:<width$}");
            if i.workspace {
                line.push_str(&"  (workspace)".dimmed().to_string());
            } else if i.via.len() > 1 {
                line.push_str(
                    &format!("  ← {}", i.via[..i.via.len() - 1].join(" → "))
                        .dimmed()
                        .to_string(),
                );
            }
            if kind == "links"
                && let Some(l) = &i.links
            {
                line.push_str(&format!("  [links = \"{l}\"]"));
            }
            println!("{line}");
        }
    }
    Ok(())
}
