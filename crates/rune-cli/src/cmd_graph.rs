//! `rune graph`: the workspace's package graph — or, with `--units`, the
//! compilation units a build would use — as Graphviz DOT, Mermaid or JSON.
//!
//! The package graph is the lockfile's (only what is used, as `rune tree`
//! shows it), with build and development edges drawn apart. The unit graph is
//! exactly what the scheduler would run: every unit with its key and whether
//! the store already has it, so it doubles as a machine-readable build plan.

use anyhow::Result;
use std::collections::BTreeSet;
use std::fmt::Write as _;

use crate::args::GraphArgs;
use crate::cmd_build::{plan_for, selection};
use crate::graph::{self, EdgeKind, Mode};
use crate::session::Session;
use crate::ui;
use crate::workspace::Workspace;

/// A drawable graph: labelled nodes, some highlighted, and kinded edges.
struct Drawing {
    nodes: Vec<Node>,
    edges: BTreeSet<(usize, usize, &'static str)>,
}

struct Node {
    label: String,
    /// `member` (a workspace package), `cached` or `compile` (a unit).
    class: &'static str,
}

impl Drawing {
    fn dot(&self) -> String {
        let mut x = String::from(
            "digraph rune {\n    rankdir=LR;\n    node [shape=box, style=\"rounded,filled\", \
             fillcolor=\"#ffffff\", fontname=\"Helvetica\"];\n",
        );
        for (i, n) in self.nodes.iter().enumerate() {
            let fill = match n.class {
                "member" => "#dbeafe",
                "cached" => "#dcfce7",
                "compile" => "#ffedd5",
                _ => "#ffffff",
            };
            let _ = writeln!(
                x,
                "    n{i} [label=\"{}\", fillcolor=\"{fill}\"];",
                n.label.replace('\\', "\\\\").replace('"', "\\\"")
            );
        }
        for &(from, to, kind) in &self.edges {
            let style = match kind {
                "build" => " [style=dashed, color=\"#b45309\", label=\"build\"]",
                "dev" => " [style=dotted, color=\"#6b7280\", label=\"dev\"]",
                "order" => " [style=dotted, label=\"order\"]",
                _ => "",
            };
            let _ = writeln!(x, "    n{from} -> n{to}{style};");
        }
        x.push_str("}\n");
        x
    }

    fn mermaid(&self) -> String {
        let mut x = String::from("flowchart LR\n");
        for (i, n) in self.nodes.iter().enumerate() {
            let label = n.label.replace('"', "#quot;");
            let _ = writeln!(x, "    n{i}[\"{label}\"]:::{}", n.class);
        }
        for &(from, to, kind) in &self.edges {
            match kind {
                "normal" => {
                    let _ = writeln!(x, "    n{from} --> n{to}");
                }
                other => {
                    let _ = writeln!(x, "    n{from} -. {other} .-> n{to}");
                }
            }
        }
        x.push_str(
            "    classDef member fill:#dbeafe,stroke:#1d4ed8\n    \
             classDef cached fill:#dcfce7,stroke:#15803d\n    \
             classDef compile fill:#ffedd5,stroke:#c2410c\n    \
             classDef package fill:#ffffff,stroke:#6b7280\n",
        );
        x
    }
}

fn edge_kind(kind: EdgeKind) -> &'static str {
    match kind {
        EdgeKind::Normal => "normal",
        EdgeKind::Build => "build",
        EdgeKind::Dev => "dev",
    }
}

pub fn graph(args: GraphArgs) -> Result<()> {
    let (drawing, json) = if args.units {
        units(&args)?
    } else {
        packages(&args)?
    };
    let text = match args.format.as_str() {
        "json" => serde_json::to_string_pretty(&json)? + "\n",
        "mermaid" => drawing.mermaid(),
        _ => drawing.dot(),
    };
    match &args.output {
        Some(path) => {
            std::fs::write(path, text)?;
            ui::summary(format!(
                "{} {} and {} edges written to {}",
                drawing.nodes.len(),
                if args.units { "units" } else { "packages" },
                drawing.edges.len(),
                path.display()
            ));
        }
        None => print!("{text}"),
    }
    Ok(())
}

fn packages(args: &GraphArgs) -> Result<(Drawing, serde_json::Value)> {
    let ws = Workspace::load(args.common.manifest_path.as_deref())?;
    let offline = args.common.offline || ws.config.settings.offline();
    let pkgs = graph::inventory(&ws, offline)?;
    for p in &args.common.package {
        if ws.member_named(p).is_none() {
            anyhow::bail!("package `{p}` is not a member of this workspace");
        }
    }
    let roots: Vec<usize> = pkgs
        .iter()
        .enumerate()
        .filter(|(_, p)| {
            p.member && (args.common.package.is_empty() || args.common.package.contains(&p.name))
        })
        .map(|(i, _)| i)
        .collect();
    let listed: Vec<usize> = crate::cmd_sbom::reachable(&pkgs, &roots, args.dev)
        .into_iter()
        .filter(|&i| !args.workspace_only || pkgs[i].member)
        .collect();
    // Two versions of one crate need their version to tell them apart; a
    // single one reads better without.
    let versions = |name: &str| listed.iter().filter(|&&i| pkgs[i].name == name).count();
    let position = |i: usize| listed.iter().position(|&j| j == i);

    let mut drawing = Drawing {
        nodes: Vec::new(),
        edges: BTreeSet::new(),
    };
    let mut json_packages = Vec::new();
    for &i in &listed {
        let p = &pkgs[i];
        let label = if versions(&p.name) > 1 || p.member {
            format!("{} {}", p.name, p.version)
        } else {
            p.name.clone()
        };
        drawing.nodes.push(Node {
            label,
            class: if p.member { "member" } else { "package" },
        });
        json_packages.push(serde_json::json!({
            "name": p.name,
            "version": p.version.to_string(),
            "source": p.source,
            "member": p.member,
        }));
        for &(j, kind) in &p.deps {
            // Development dependencies matter for the workspace's own
            // packages only (a dependency's tests are never built).
            if kind == EdgeKind::Dev && !(args.dev && p.member) {
                continue;
            }
            if let (Some(from), Some(to)) = (position(i), position(j)) {
                drawing.edges.insert((from, to, edge_kind(kind)));
            }
        }
    }
    let json = serde_json::json!({
        "version": 1,
        "packages": json_packages,
        "edges": drawing.edges.iter().map(|(from, to, kind)| serde_json::json!({
            "from": from, "to": to, "kind": kind,
        })).collect::<Vec<_>>(),
    });
    Ok((drawing, json))
}

fn units(args: &GraphArgs) -> Result<(Drawing, serde_json::Value)> {
    let sess = Session::new(&args.common, "dev")?;
    let plan = plan_for(&sess, Mode::Build, &selection(&args.targets))?;
    let executor = sess.executor();
    let identities = executor.current_identities(&plan.graph)?;
    let g = &plan.graph;
    let index: std::collections::HashMap<_, usize> = identities
        .iter()
        .enumerate()
        .map(|(i, (id, _, _))| (*id, i))
        .collect();

    let mut drawing = Drawing {
        nodes: Vec::new(),
        edges: BTreeSet::new(),
    };
    let mut json_units = Vec::new();
    for (i, (id, label, identity)) in identities.iter().enumerate() {
        let node = g.node(*id);
        let unit = &node.unit;
        let cached = executor.is_cached(&identity.key);
        drawing.nodes.push(Node {
            label: label.clone(),
            class: if cached { "cached" } else { "compile" },
        });
        let mut deps = Vec::new();
        for d in &node.dependencies {
            let Some(&to) = index.get(d) else { continue };
            let mut kinds = Vec::new();
            if node.normal_dependencies.contains(d) {
                kinds.push("normal");
            }
            if node.build_dependencies.contains(d) {
                kinds.push("build");
            }
            if kinds.is_empty() {
                kinds.push("order");
            }
            for kind in &kinds {
                drawing.edges.insert((i, to, kind));
            }
            let alias = node
                .extern_aliases
                .get(d)
                .or_else(|| node.build_aliases.get(d))
                .cloned()
                .unwrap_or_else(|| g.node(*d).unit.rustc_crate_name());
            deps.push(serde_json::json!({
                "index": to,
                "extern_crate_name": alias,
                "kinds": kinds,
            }));
        }
        json_units.push(serde_json::json!({
            "label": label,
            "package": unit.crate_id.name,
            "version": unit.crate_id.version.to_string(),
            "target": unit.target_name,
            "kind": unit.kind.as_str(),
            "crate_type": unit.crate_type.rustc_crate_type(),
            "test": unit.harness,
            "check": unit.check_only,
            "for_host": unit.ext.for_host,
            "platform": unit.target.as_ref().map(|t| t.0.clone()),
            "features": unit.features,
            "build_script": unit.build_script.is_some(),
            "key": identity.key,
            "cached": cached,
            "dependencies": deps,
        }));
    }
    let roots: Vec<usize> = plan
        .requested
        .iter()
        .filter_map(|r| index.get(&r.node).copied())
        .collect();
    let json = serde_json::json!({
        "version": 1,
        "profile": sess.profile.name,
        "units": json_units,
        "roots": roots,
    });
    Ok((drawing, json))
}
