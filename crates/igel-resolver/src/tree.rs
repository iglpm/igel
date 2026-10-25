//! `igl tree` and `igl why`.

use crate::resolver::{ResolvedDependencyRef, ResolvedGraph, ResolvedPackage};
use colored::*;
use igel_manifest::dependency::DependencyKind;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Prefix {
    #[default]
    Indent,
    Depth,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edges {
    pub normal: bool,
    pub build: bool,
    pub dev: bool,
    pub features: bool,
}

impl Default for Edges {
    fn default() -> Self {
        Self {
            normal: true,
            build: true,
            dev: true,
            features: false,
        }
    }
}

impl Edges {
    pub fn parse(values: &[String]) -> Result<Self, String> {
        let words: Vec<&str> = values
            .iter()
            .flat_map(|v| v.split(','))
            .map(str::trim)
            .filter(|w| !w.is_empty())
            .collect();
        let mut e = Self::default();
        let named: Vec<&str> = words
            .iter()
            .copied()
            .filter(|w| matches!(*w, "normal" | "build" | "dev"))
            .collect();
        if !named.is_empty() {
            e.normal = named.contains(&"normal");
            e.build = named.contains(&"build");
            e.dev = named.contains(&"dev");
        }
        for w in words {
            match w {
                "all" | "normal" | "build" | "dev" => {}
                "features" => e.features = true,
                "no-normal" => e.normal = false,
                "no-build" => e.build = false,
                "no-dev" => e.dev = false,
                "no-proc-macro" | "public" => {
                    return Err(format!("`-e {w}` is not supported yet"));
                }
                other => {
                    return Err(format!(
                        "unknown edge kind `{other}` (expected all, normal, build, dev, \
                         features, no-normal, no-build or no-dev)"
                    ));
                }
            }
        }
        Ok(e)
    }

    fn follows(&self, kind: DependencyKind) -> bool {
        match kind {
            DependencyKind::Normal => self.normal,
            DependencyKind::Build => self.build,
            DependencyKind::Dev => self.dev,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct TreeOptions {
    pub max_depth: Option<usize>,
    pub features: bool,
    pub duplicates: bool,
    pub invert: Option<String>,
    pub dev: bool,
    pub edges: Edges,
    pub prefix: Prefix,
    pub target: Option<String>,
    pub members: Vec<String>,
}

fn id_of(pkg: &ResolvedPackage) -> String {
    format!("{} {}", pkg.name, pkg.version)
}

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

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Node {
    Pkg(String),
    Feat(String, String),
}

struct View<'a> {
    graph: &'a ResolvedGraph,
    opts: &'a TreeOptions,
    members: HashSet<String>,
    edges: Edges,
}

impl<'a> View<'a> {
    fn new(graph: &'a ResolvedGraph, roots: &[String], opts: &'a TreeOptions) -> Self {
        let members = if opts.members.is_empty() {
            roots.iter().cloned().collect()
        } else {
            opts.members.iter().cloned().collect()
        };
        let mut edges = opts.edges;
        edges.dev |= opts.dev;
        Self {
            graph,
            opts,
            members,
            edges,
        }
    }

    fn pkg(&self, id: &str) -> Option<&'a ResolvedPackage> {
        self.graph.packages.get(id)
    }

    fn activated(&self, p: &ResolvedPackage, dep: &ResolvedDependencyRef) -> bool {
        if !dep.optional || p.declared_features.is_empty() {
            return true;
        }
        let name = dep.name.as_str();
        p.features.iter().any(|f| {
            f == name
                || p.declared_features.get(f).is_some_and(|vals| {
                    vals.iter().any(|v| {
                        v == name
                            || v.strip_prefix("dep:") == Some(name)
                            || v.split_once('/').is_some_and(|(d, _)| d == name)
                    })
                })
        })
    }

    fn dep_edges(
        &self,
        p: &'a ResolvedPackage,
    ) -> Vec<(&'a ResolvedDependencyRef, &'a ResolvedPackage)> {
        let mut out: Vec<(&ResolvedDependencyRef, &ResolvedPackage)> = Vec::new();
        for dep in &p.dependencies {
            if !self.edges.follows(dep.kind)
                || (dep.kind == DependencyKind::Dev && !self.members.contains(&id_of(p)))
            {
                continue;
            }
            if let Some(t) = &self.opts.target
                && !crate::resolver::matches_target(dep.target.as_deref(), t)
            {
                continue;
            }
            if !self.activated(p, dep) {
                continue;
            }
            let real = dep.package.as_deref().unwrap_or(&dep.name);
            if let Some(found) = self.graph.find_matching(real, &dep.req) {
                out.push((dep, found));
            }
        }
        out.sort_by(|a, b| (&a.1.name, &a.1.version).cmp(&(&b.1.name, &b.1.version)));
        out
    }

    fn enabled(&self, p: &ResolvedPackage, f: &str) -> bool {
        p.features.iter().any(|x| x == f)
            || (p.declared_features.is_empty() && p.features.is_empty())
    }

    fn children(&self, node: &Node) -> Vec<(DependencyKind, Node)> {
        let mut out: Vec<(DependencyKind, Node)> = Vec::new();
        let mut push = |kind: DependencyKind, n: Node| {
            if !out.iter().any(|(k, m)| *k == kind && *m == n) {
                out.push((kind, n));
            }
        };
        match node {
            Node::Pkg(id) => {
                let Some(p) = self.pkg(id) else { return out };
                for (dep, child) in self.dep_edges(p) {
                    let cid = id_of(child);
                    if !self.edges.features {
                        push(dep.kind, Node::Pkg(cid));
                        continue;
                    }
                    if dep.optional && !p.declared_features.is_empty() {
                        continue;
                    }
                    let mut feats: Vec<&str> = dep.features.iter().map(String::as_str).collect();
                    if dep.default_features
                        && (child.declared_features.contains_key("default")
                            || child.features.iter().any(|f| f == "default"))
                    {
                        feats.push("default");
                    }
                    feats.retain(|f| !f.contains('/') && self.enabled(child, f));
                    if feats.is_empty() {
                        push(dep.kind, Node::Pkg(cid.clone()));
                    }
                    for f in feats {
                        push(dep.kind, Node::Feat(cid.clone(), f.to_string()));
                    }
                }
            }
            Node::Feat(id, f) => {
                let Some(p) = self.pkg(id) else { return out };
                push(DependencyKind::Normal, Node::Pkg(id.clone()));
                let edges = self.dep_edges(p);
                let dep_named = |name: &str| {
                    edges
                        .iter()
                        .filter(|(d, _)| d.name == name)
                        .collect::<Vec<_>>()
                };
                for v in p.declared_features.get(f).into_iter().flatten() {
                    if let Some(name) = v.strip_prefix("dep:") {
                        for (d, c) in dep_named(name) {
                            push(d.kind, Node::Pkg(id_of(c)));
                        }
                    } else if let Some((name, g)) = v.split_once('/') {
                        for (d, c) in dep_named(name.trim_end_matches('?')) {
                            if self.enabled(c, g) {
                                push(d.kind, Node::Feat(id_of(c), g.to_string()));
                            }
                        }
                    } else if p.declared_features.contains_key(v) {
                        if self.enabled(p, v) {
                            push(DependencyKind::Normal, Node::Feat(id.clone(), v.clone()));
                        }
                    } else {
                        for (d, c) in dep_named(v) {
                            push(d.kind, Node::Pkg(id_of(c)));
                        }
                    }
                }
            }
        }
        out.sort_by(|a, b| self.order(&a.1).cmp(&self.order(&b.1)));
        out
    }

    fn order<'n>(&self, node: &'n Node) -> (&'a str, Option<&'a semver::Version>, &'n str, u8) {
        let (id, feature, rank) = match node {
            Node::Pkg(id) => (id, "", 0),
            Node::Feat(id, f) => (id, f.as_str(), 1),
        };
        match self.pkg(id) {
            Some(p) => (p.name.as_str(), Some(&p.version), feature, rank),
            None => ("", None, feature, rank),
        }
    }

    fn label(&self, node: &Node) -> String {
        match node {
            Node::Pkg(id) => match self.pkg(id) {
                Some(p) => {
                    let mut s = format!("{} v{}", p.name.bold(), p.version);
                    match &p.source {
                        crate::resolver::PackageSource::Path(dir) => {
                            s.push_str(&format!(" ({})", dir.display()));
                        }
                        crate::resolver::PackageSource::Git { url, commit, .. } => {
                            s.push_str(&format!(" ({url}#{})", commit.get(..8).unwrap_or(commit)));
                        }
                        crate::resolver::PackageSource::Registry(_) => {}
                    }
                    if self.opts.features && !p.features.is_empty() {
                        s.push_str(&format!(" [{}]", p.features.join(", ").dimmed()));
                    }
                    s
                }
                None => id.clone(),
            },
            Node::Feat(id, f) => {
                let name = self.pkg(id).map_or(id.as_str(), |p| p.name.as_str());
                format!("{} feature \"{}\"", name.bold(), f)
            }
        }
    }

    fn reverse(&self, roots: &[String]) -> BTreeMap<Node, Vec<(DependencyKind, Node)>> {
        let mut starts: Vec<Node> = roots.iter().cloned().map(Node::Pkg).collect();
        if roots.is_empty() {
            starts.extend(self.graph.packages.keys().cloned().map(Node::Pkg));
        }
        if self.edges.features {
            for m in &self.members {
                if let Some(p) = self.pkg(m) {
                    for f in &p.features {
                        starts.push(Node::Feat(m.clone(), f.clone()));
                    }
                }
            }
        }
        let mut rev: BTreeMap<Node, Vec<(DependencyKind, Node)>> = BTreeMap::new();
        let mut seen: HashSet<Node> = HashSet::new();
        let mut queue: VecDeque<Node> = starts.into_iter().collect();
        while let Some(n) = queue.pop_front() {
            if !seen.insert(n.clone()) {
                continue;
            }
            for (kind, c) in self.children(&n) {
                let parents = rev.entry(c.clone()).or_default();
                if !parents.iter().any(|(k, p)| *k == kind && *p == n) {
                    parents.push((kind, n.clone()));
                }
                queue.push_back(c);
            }
        }
        for parents in rev.values_mut() {
            parents.sort_by(|a, b| self.order(&a.1).cmp(&self.order(&b.1)));
        }
        rev
    }
}

fn ids_for_name(graph: &ResolvedGraph, name: &str) -> Vec<String> {
    let mut ids: Vec<String> = graph
        .packages
        .values()
        .filter(|p| {
            p.name == name
                || id_of(p) == name
                || name
                    .split_once('@')
                    .is_some_and(|(n, v)| p.name == n && p.version.to_string() == v)
        })
        .map(id_of)
        .collect();
    ids.sort();
    ids
}

struct Printer<'v, 'a, F: Fn(&Node) -> Vec<(DependencyKind, Node)>> {
    view: &'v View<'a>,
    next: F,
    expanded: HashSet<Node>,
    out: String,
}

impl<F: Fn(&Node) -> Vec<(DependencyKind, Node)>> Printer<'_, '_, F> {
    fn line(&mut self, prefix: &str, depth: usize, branch: &str, text: &str) {
        match self.view.opts.prefix {
            Prefix::Indent => self
                .out
                .push_str(&format!("{prefix}{}{text}\n", branch.green())),
            Prefix::Depth => self.out.push_str(&format!("{depth}{text}\n")),
            Prefix::None => self.out.push_str(&format!("{text}\n")),
        }
    }

    fn root(&mut self, node: &Node, cyan: bool) {
        let text = self.view.label(node);
        let text = if cyan { text.cyan().to_string() } else { text };
        self.line("", 0, "", &text);
        self.expanded.insert(node.clone());
        self.below(node, "", 1);
    }

    fn below(&mut self, node: &Node, prefix: &str, depth: usize) {
        if depth > self.view.opts.max_depth.unwrap_or(usize::MAX) {
            return;
        }
        let kids = (self.next)(node);
        for kind in [
            DependencyKind::Normal,
            DependencyKind::Build,
            DependencyKind::Dev,
        ] {
            let group: Vec<&Node> = kids
                .iter()
                .filter(|(k, _)| *k == kind)
                .map(|(_, n)| n)
                .collect();
            if group.is_empty() {
                continue;
            }
            let heading = match kind {
                DependencyKind::Normal => None,
                DependencyKind::Build => Some("[build-dependencies]"),
                DependencyKind::Dev => Some("[dev-dependencies]"),
            };
            if let Some(h) = heading
                && matches!(node, Node::Pkg(_))
                && self.view.opts.prefix == Prefix::Indent
            {
                self.line(prefix, depth, "", h);
            }
            for (i, child) in group.iter().enumerate() {
                let last = i + 1 == group.len();
                let seen = !self.expanded.insert((*child).clone());
                let mut text = self.view.label(child);
                if seen && !(self.next)(child).is_empty() {
                    text.push_str(&" (*)".yellow().to_string());
                }
                self.line(prefix, depth, if last { "└── " } else { "├── " }, &text);
                if !seen {
                    let next = format!("{prefix}{}", if last { "    " } else { "│   " });
                    self.below(child, &next, depth + 1);
                }
            }
        }
    }
}

pub fn render_tree(graph: &ResolvedGraph, roots: &[String], opts: &TreeOptions) -> String {
    let view = View::new(graph, roots, opts);

    if opts.duplicates || opts.invert.is_some() {
        let rev = view.reverse(roots);
        let up =
            |n: &Node| -> Vec<(DependencyKind, Node)> { rev.get(n).cloned().unwrap_or_default() };
        let mut printer = Printer {
            view: &view,
            next: up,
            expanded: HashSet::new(),
            out: String::new(),
        };
        if opts.duplicates {
            let mut by_name: BTreeMap<&str, Vec<&ResolvedPackage>> = BTreeMap::new();
            for p in graph.packages.values() {
                by_name.entry(&p.name).or_default().push(p);
            }
            for (_, mut versions) in by_name {
                if versions.len() < 2 {
                    continue;
                }
                versions.sort_by(|a, b| a.version.cmp(&b.version));
                for v in versions {
                    printer.expanded.clear();
                    printer.root(&Node::Pkg(id_of(v)), false);
                }
                printer.out.push('\n');
            }
            if printer.out.is_empty() {
                printer.out.push_str("no duplicate packages\n");
            }
            return printer.out;
        }
        let target = opts.invert.as_deref().unwrap_or_default();
        for id in ids_for_name(graph, target) {
            printer.expanded.clear();
            printer.root(&Node::Pkg(id), false);
        }
        if printer.out.is_empty() {
            printer.out.push_str(&format!(
                "package `{target}` is not in the dependency graph\n"
            ));
        }
        return printer.out;
    }

    let mut printer = Printer {
        view: &view,
        next: |n: &Node| view.children(n),
        expanded: HashSet::new(),
        out: String::new(),
    };
    for root in roots {
        if graph.packages.contains_key(root) {
            printer.root(&Node::Pkg(root.clone()), true);
        }
    }
    printer.out
}

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
                source: PackageSource::crates_io(),
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
                        target: None,
                        registry: None,
                        path: None,
                    })
                    .collect(),
                features: vec![],
                declared_features: Default::default(),
            },
        )
    }

    fn graph() -> ResolvedGraph {
        let packages: HashMap<_, _> = [
            pkg("app", "0.1.0", &[("a", "1"), ("b", "1")]),
            pkg("a", "1.0.0", &[("shared", "1")]),
            pkg("b", "1.0.0", &[("shared", "1"), ("old", "0.1")]),
            pkg("old", "0.1.0", &[("shared", "0.9")]),
            pkg("shared", "1.2.0", &[("leaf", "1")]),
            pkg("shared", "0.9.0", &[]),
            pkg("leaf", "1.0.0", &[]),
        ]
        .into_iter()
        .collect();
        ResolvedGraph {
            root_packages: vec!["app".into()],
            packages,
        }
    }

    fn plain(s: String) -> String {
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
            out.contains("shared v1.2.0 (*)"),
            "second occurrence of shared 1.2.0 is collapsed: {out}"
        );
        assert!(!out.contains("leaf v1.0.0 (*)"), "{out}");
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

    fn dep(
        name: &str,
        kind: DependencyKind,
        features: &[&str],
        optional: bool,
    ) -> ResolvedDependencyRef {
        ResolvedDependencyRef {
            name: name.into(),
            req: "*".into(),
            kind,
            optional,
            features: features.iter().map(|f| f.to_string()).collect(),
            default_features: true,
            package: None,
            target: None,
            registry: None,
            path: None,
        }
    }

    fn featured() -> ResolvedGraph {
        let mk = |name: &str,
                  deps: Vec<ResolvedDependencyRef>,
                  features: &[&str],
                  declared: &[(&str, &[&str])]| {
            let (id, mut p) = pkg(name, "1.0.0", &[]);
            p.dependencies = deps;
            p.features = features.iter().map(|f| f.to_string()).collect();
            p.declared_features = declared
                .iter()
                .map(|(k, v)| (k.to_string(), v.iter().map(|x| x.to_string()).collect()))
                .collect();
            (id, p)
        };
        let mut win = dep("winapi", DependencyKind::Normal, &[], false);
        win.target = Some("cfg(windows)".into());
        let packages: HashMap<_, _> = [
            mk(
                "app",
                vec![
                    dep("serde", DependencyKind::Normal, &["derive"], false),
                    dep("cc", DependencyKind::Build, &[], false),
                    dep("tmp", DependencyKind::Dev, &[], false),
                ],
                &[],
                &[],
            ),
            mk(
                "serde",
                vec![dep("serde_derive", DependencyKind::Normal, &[], true)],
                &["default", "derive", "std"],
                &[
                    ("default", &["std"]),
                    ("std", &[]),
                    ("derive", &["serde_derive"]),
                ],
            ),
            mk("serde_derive", vec![], &[], &[]),
            mk(
                "cc",
                vec![dep("tmp", DependencyKind::Dev, &[], false), win],
                &[],
                &[],
            ),
            mk("tmp", vec![], &[], &[]),
            mk("winapi", vec![], &[], &[]),
        ]
        .into_iter()
        .collect();
        ResolvedGraph {
            root_packages: vec!["app".into()],
            packages,
        }
    }

    #[test]
    fn edges_parse_like_cargo() {
        assert_eq!(Edges::parse(&[]).unwrap(), Edges::default());
        let e = Edges::parse(&["normal".into()]).unwrap();
        assert!(e.normal && !e.build && !e.dev);
        let e = Edges::parse(&["no-dev,features".into()]).unwrap();
        assert!(e.normal && e.build && !e.dev && e.features);
        assert!(Edges::parse(&["bogus".into()]).is_err());
    }

    #[test]
    fn build_and_dev_dependencies_get_their_sections() {
        colored::control::set_override(false);
        let opts = TreeOptions {
            target: Some("x86_64-unknown-linux-gnu".into()),
            ..Default::default()
        };
        let out = plain(render_tree(&featured(), &["app 1.0.0".into()], &opts));
        let lines: Vec<&str> = out.lines().collect();
        let build = lines
            .iter()
            .position(|l| *l == "[build-dependencies]")
            .unwrap();
        let dev = lines
            .iter()
            .position(|l| *l == "[dev-dependencies]")
            .unwrap();
        assert!(lines[1].contains("serde v1.0.0") && build < dev, "{out}");
        assert!(lines[build + 1].contains("cc v1.0.0"), "{out}");
        assert_eq!(out.matches("tmp v1.0.0").count(), 1, "{out}");
        assert!(!out.contains("winapi"), "{out}");
        assert!(out.contains("serde_derive v1.0.0"), "{out}");

        let no_dev = TreeOptions {
            edges: Edges::parse(&["no-dev".into()]).unwrap(),
            ..opts.clone()
        };
        let out = plain(render_tree(&featured(), &["app 1.0.0".into()], &no_dev));
        assert!(
            !out.contains("[dev-dependencies]") && !out.contains("tmp"),
            "{out}"
        );

        let flat = TreeOptions {
            prefix: Prefix::Depth,
            target: None,
            ..opts
        };
        let out = plain(render_tree(&featured(), &["app 1.0.0".into()], &flat));
        assert!(out.lines().any(|l| l == "1serde v1.0.0"), "{out}");
        assert!(out.contains("winapi"), "every platform: {out}");
    }

    #[test]
    fn features_as_nodes_say_who_turned_each_on() {
        colored::control::set_override(false);
        let opts = TreeOptions {
            edges: Edges::parse(&["features".into()]).unwrap(),
            ..Default::default()
        };
        let out = plain(render_tree(&featured(), &["app 1.0.0".into()], &opts));
        assert!(out.contains("├── serde feature \"default\""), "{out}");
        assert!(out.contains("serde feature \"derive\""), "{out}");
        assert!(out.contains("serde feature \"std\""), "{out}");

        let why = TreeOptions {
            invert: Some("serde_derive".into()),
            ..opts
        };
        let out = plain(render_tree(&featured(), &["app 1.0.0".into()], &why));
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "serde_derive v1.0.0", "{out}");
        assert!(lines[1].ends_with("serde feature \"derive\""), "{out}");
        assert!(lines[2].ends_with("app v1.0.0"), "{out}");
    }
}
