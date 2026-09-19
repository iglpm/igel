//! Rendering of the resolved dependency graph: `rune tree` and `rune why`.

use crate::resolver::{ResolvedGraph, ResolvedPackage};
use colored::*;
use rune_manifest::dependency::DependencyKind;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

/// Options for [`render_tree`].
#[derive(Debug, Clone, Default)]
pub struct TreeOptions {
    /// Stop descending below this depth (roots are depth 0).
    pub max_depth: Option<usize>,
    /// Show each package's active features.
    pub features: bool,
    /// Show only packages that appear in more than one version, as inverted
    /// trees (who pulls in each duplicate).
    pub duplicates: bool,
    /// Show the reverse tree of this package name: what depends on it.
    pub invert: Option<String>,
    /// Include dev-dependencies.
    pub dev: bool,
}

fn id_of(pkg: &ResolvedPackage) -> String {
    format!("{} {}", pkg.name, pkg.version)
}

/// The resolved children of `pkg` as package ids, in a stable order, without
/// duplicates.
fn children(graph: &ResolvedGraph, pkg: &ResolvedPackage, dev: bool) -> Vec<String> {
    let mut out: BTreeSet<(String, String)> = BTreeSet::new();
    for dep in &pkg.dependencies {
        if dep.kind == DependencyKind::Dev && !dev {
            continue;
        }
        let real = dep.package.as_deref().unwrap_or(&dep.name);
        if let Some(found) = graph.find_matching(real, &dep.req) {
            out.insert((found.name.clone(), id_of(found)));
        }
    }
    out.into_iter().map(|(_, id)| id).collect()
}

/// Reverse edges: package id → the ids that depend on it.
fn dependents(graph: &ResolvedGraph, dev: bool) -> BTreeMap<String, BTreeSet<String>> {
    let mut map: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for pkg in graph.packages.values() {
        for child in children(graph, pkg, dev) {
            map.entry(child).or_default().insert(id_of(pkg));
        }
    }
    map
}

fn label(pkg: &ResolvedPackage, opts: &TreeOptions) -> String {
    let mut s = format!("{} v{}", pkg.name.bold(), pkg.version);
    if opts.features && !pkg.features.is_empty() {
        s.push_str(&format!(" [{}]", pkg.features.join(", ").dimmed()));
    }
    s
}

/// Resolves a user-typed root (`name` or `name version`) to package ids.
fn ids_for_name(graph: &ResolvedGraph, name: &str) -> Vec<String> {
    let mut ids: Vec<String> = graph
        .packages
        .values()
        .filter(|p| p.name == name || id_of(p) == name)
        .map(id_of)
        .collect();
    ids.sort();
    ids
}

/// Renders a tree rooted at each id in `roots`.
pub fn render_tree(graph: &ResolvedGraph, roots: &[String], opts: &TreeOptions) -> String {
    let mut out = String::new();

    if opts.duplicates {
        let mut by_name: BTreeMap<&str, Vec<&ResolvedPackage>> = BTreeMap::new();
        for p in graph.packages.values() {
            by_name.entry(&p.name).or_default().push(p);
        }
        let rev = dependents(graph, opts.dev);
        for (_, mut versions) in by_name {
            if versions.len() < 2 {
                continue;
            }
            versions.sort_by(|a, b| a.version.cmp(&b.version));
            for v in versions {
                out.push_str(&format!("{}\n", label(v, opts)));
                render_inverted(
                    graph,
                    &rev,
                    &id_of(v),
                    "",
                    1,
                    opts,
                    &mut HashSet::new(),
                    &mut out,
                );
            }
            out.push('\n');
        }
        if out.is_empty() {
            out.push_str("no duplicate packages\n");
        }
        return out;
    }

    if let Some(target) = &opts.invert {
        let rev = dependents(graph, opts.dev);
        for id in ids_for_name(graph, target) {
            if let Some(pkg) = graph.packages.get(&id) {
                out.push_str(&format!("{}\n", label(pkg, opts)));
                render_inverted(graph, &rev, &id, "", 1, opts, &mut HashSet::new(), &mut out);
            }
        }
        if out.is_empty() {
            out.push_str(&format!(
                "package `{target}` is not in the dependency graph\n"
            ));
        }
        return out;
    }

    let mut expanded: HashSet<String> = HashSet::new();
    for root in roots {
        let Some(pkg) = graph.packages.get(root) else {
            continue;
        };
        out.push_str(&format!("{}\n", label(pkg, opts).cyan()));
        expanded.insert(root.clone());
        render_node(graph, pkg, "", 1, opts, &mut expanded, &mut out);
    }
    out
}

fn render_node(
    graph: &ResolvedGraph,
    pkg: &ResolvedPackage,
    prefix: &str,
    depth: usize,
    opts: &TreeOptions,
    expanded: &mut HashSet<String>,
    out: &mut String,
) {
    if depth > opts.max_depth.unwrap_or(usize::MAX) {
        return;
    }
    let kids = children(graph, pkg, opts.dev);
    for (i, id) in kids.iter().enumerate() {
        let last = i + 1 == kids.len();
        let Some(child) = graph.packages.get(id) else {
            continue;
        };
        let seen = !expanded.insert(id.clone());
        out.push_str(&format!(
            "{}{}{}{}\n",
            prefix,
            if last { "└── " } else { "├── " }.green(),
            label(child, opts),
            if seen {
                " (*)".yellow().to_string()
            } else {
                String::new()
            }
        ));
        if !seen {
            let next = format!("{}{}", prefix, if last { "    " } else { "│   " });
            render_node(graph, child, &next, depth + 1, opts, expanded, out);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_inverted(
    graph: &ResolvedGraph,
    rev: &BTreeMap<String, BTreeSet<String>>,
    id: &str,
    prefix: &str,
    depth: usize,
    opts: &TreeOptions,
    expanded: &mut HashSet<String>,
    out: &mut String,
) {
    if depth > opts.max_depth.unwrap_or(usize::MAX) {
        return;
    }
    let Some(parents) = rev.get(id) else { return };
    let parents: Vec<&String> = parents.iter().collect();
    for (i, pid) in parents.iter().enumerate() {
        let last = i + 1 == parents.len();
        let Some(parent) = graph.packages.get(*pid) else {
            continue;
        };
        let seen = !expanded.insert((*pid).clone());
        out.push_str(&format!(
            "{}{}{}{}\n",
            prefix,
            if last { "└── " } else { "├── " }.green(),
            label(parent, opts),
            if seen {
                " (*)".yellow().to_string()
            } else {
                String::new()
            }
        ));
        if !seen {
            let next = format!("{}{}", prefix, if last { "    " } else { "│   " });
            render_inverted(graph, rev, pid, &next, depth + 1, opts, expanded, out);
        }
    }
}

/// Compatibility wrapper: renders a tree from `graph.root_packages`.
pub fn render_dependency_tree(graph: &ResolvedGraph, max_depth: Option<usize>) -> String {
    let roots: Vec<String> = graph
        .root_packages
        .iter()
        .flat_map(|name| ids_for_name(graph, name))
        .collect();
    render_tree(
        graph,
        &roots,
        &TreeOptions {
            max_depth,
            ..Default::default()
        },
    )
}

/// Shortest dependency chains from any of `roots` (package ids) to every
/// package named `target` (a bare name or `name version`).
///
/// Returns each chain as a list of `name version` strings, root first. Chains
/// are found breadth-first, so the shortest come out first, and one chain is
/// reported per distinct final version.
pub fn trace_why(graph: &ResolvedGraph, roots: &[String], target: &str) -> Vec<Vec<String>> {
    let targets: HashSet<String> = ids_for_name(graph, target).into_iter().collect();
    let mut results: Vec<Vec<String>> = Vec::new();
    let mut found_for: HashSet<String> = HashSet::new();

    let mut best: HashMap<String, usize> = HashMap::new();
    let mut queue: VecDeque<(String, Vec<String>)> = VecDeque::new();
    for root in roots {
        queue.push_back((root.clone(), vec![root.clone()]));
    }
    while let Some((cur, path)) = queue.pop_front() {
        if targets.contains(&cur) {
            if path.len() > 1 && found_for.insert(format!("{cur}|{}", path[path.len() - 2])) {
                results.push(path);
            }
            continue;
        }
        // Allow revisiting a package only via an equally short path, so all
        // shortest chains are kept but the search stays linear-ish.
        match best.get(&cur) {
            Some(&len) if len < path.len() => continue,
            _ => {
                best.insert(cur.clone(), path.len());
            }
        }
        if let Some(pkg) = graph.packages.get(&cur) {
            for child in children(graph, pkg, false) {
                if path.contains(&child) {
                    continue;
                }
                let mut next = path.clone();
                next.push(child.clone());
                queue.push_back((child, next));
            }
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolver::{PackageSource, ResolvedDependencyRef};
    use semver::Version;

    fn pkg(name: &str, version: &str, deps: &[(&str, &str)]) -> (String, ResolvedPackage) {
        let v: Version = version.parse().unwrap();
        (
            format!("{name} {v}"),
            ResolvedPackage {
                name: name.into(),
                version: v,
                source: PackageSource::Registry,
                checksum: None,
                dependencies: deps
                    .iter()
                    .map(|(n, req)| ResolvedDependencyRef {
                        name: (*n).into(),
                        req: (*req).into(),
                        kind: DependencyKind::Normal,
                        optional: false,
                        features: vec![],
                        default_features: true,
                        package: None,
                    })
                    .collect(),
                features: vec![],
            },
        )
    }

    fn graph() -> ResolvedGraph {
        let packages: HashMap<_, _> = [
            pkg("app", "0.1.0", &[("a", "1"), ("b", "1")]),
            pkg("a", "1.0.0", &[("shared", "1")]),
            pkg("b", "1.0.0", &[("shared", "1"), ("old", "0.1")]),
            pkg("old", "0.1.0", &[("shared", "0.9")]),
            pkg("shared", "1.2.0", &[]),
            pkg("shared", "0.9.0", &[]),
        ]
        .into_iter()
        .collect();
        ResolvedGraph {
            root_packages: vec!["app".into()],
            packages,
        }
    }

    fn plain(s: String) -> String {
        // Strip ANSI so assertions don't depend on the terminal.
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn tree_uses_versions_and_marks_repeats() {
        colored::control::set_override(false);
        let g = graph();
        let out = plain(render_tree(
            &g,
            &["app 0.1.0".into()],
            &TreeOptions::default(),
        ));
        assert!(out.starts_with("app v0.1.0\n"), "{out}");
        assert!(out.contains("├── a v1.0.0"), "{out}");
        assert!(out.contains("shared v1.2.0"), "{out}");
        assert!(out.contains("shared v0.9.0"), "{out}");
        assert!(
            out.contains("(*)"),
            "second occurrence of shared 1.2.0 is collapsed: {out}"
        );
    }

    #[test]
    fn tree_depth_limit() {
        colored::control::set_override(false);
        let out = plain(render_tree(
            &graph(),
            &["app 0.1.0".into()],
            &TreeOptions {
                max_depth: Some(1),
                ..Default::default()
            },
        ));
        assert!(out.contains("a v1.0.0") && !out.contains("shared"), "{out}");
    }

    #[test]
    fn why_finds_shortest_chains() {
        let chains = trace_why(&graph(), &["app 0.1.0".into()], "shared");
        assert!(
            chains
                .iter()
                .any(|c| c == &["app 0.1.0", "a 1.0.0", "shared 1.2.0"]),
            "{chains:?}"
        );
        assert!(
            chains.iter().any(|c| c.last().unwrap() == "shared 0.9.0"),
            "{chains:?}"
        );
        assert!(trace_why(&graph(), &["app 0.1.0".into()], "nope").is_empty());
    }

    #[test]
    fn invert_and_duplicates() {
        colored::control::set_override(false);
        let inv = plain(render_tree(
            &graph(),
            &[],
            &TreeOptions {
                invert: Some("shared".into()),
                ..Default::default()
            },
        ));
        assert!(
            inv.contains("shared v1.2.0") && inv.contains("a v1.0.0"),
            "{inv}"
        );

        let dups = plain(render_tree(
            &graph(),
            &[],
            &TreeOptions {
                duplicates: true,
                ..Default::default()
            },
        ));
        assert!(
            dups.contains("shared v0.9.0") && dups.contains("shared v1.2.0"),
            "{dups}"
        );
        assert!(
            !dups.lines().any(|l| l.starts_with("app")),
            "app is not a duplicate, so it is not a tree root: {dups}"
        );
    }
}
