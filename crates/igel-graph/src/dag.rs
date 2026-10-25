use igel_common::{CompilationUnit, IgelError};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub usize);

pub struct BuildNode {
    pub id: NodeId,
    pub unit: CompilationUnit,
    pub dependencies: Vec<NodeId>,
    pub dependents: Vec<NodeId>,
    pub extern_aliases: HashMap<NodeId, String>,
    /// Dependencies linked into the crate itself (as opposed to only its build script).
    pub normal_dependencies: HashSet<NodeId>,
    /// Dependencies linked into this unit's build script (host). May overlap `normal_dependencies`.
    pub build_dependencies: HashSet<NodeId>,
    pub build_aliases: HashMap<NodeId, String>,
    pub estimated_cost: f64,
}

pub struct BuildGraph {
    nodes: Vec<BuildNode>,
    name_index: HashMap<String, Vec<NodeId>>,
}

impl BuildGraph {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            name_index: HashMap::new(),
        }
    }

    pub fn add_node(&mut self, unit: CompilationUnit) -> NodeId {
        let id = NodeId(self.nodes.len());
        let name = unit.crate_id.name.clone();

        let node = BuildNode {
            id,
            unit,
            dependencies: Vec::new(),
            dependents: Vec::new(),
            extern_aliases: HashMap::new(),
            normal_dependencies: HashSet::new(),
            build_dependencies: HashSet::new(),
            build_aliases: HashMap::new(),
            estimated_cost: 1.0,
        };

        self.nodes.push(node);
        self.name_index.entry(name).or_default().push(id);

        id
    }

    pub fn add_edge_with_extern(&mut self, from: NodeId, to: NodeId, extern_name: Option<&str>) {
        if !self.nodes[from.0].dependencies.contains(&to) {
            self.nodes[from.0].dependencies.push(to);
        }
        self.nodes[from.0].normal_dependencies.insert(to);
        if let Some(ext) = extern_name {
            self.nodes[from.0]
                .extern_aliases
                .insert(to, ext.to_string());
        }
        if !self.nodes[to.0].dependents.contains(&from) {
            self.nodes[to.0].dependents.push(from);
        }
    }

    pub fn add_edge(&mut self, from: NodeId, to: NodeId) {
        self.add_edge_with_extern(from, to, None);
    }

    pub fn add_build_edge(&mut self, from: NodeId, to: NodeId, extern_name: Option<&str>) {
        if !self.nodes[from.0].dependencies.contains(&to) {
            self.nodes[from.0].dependencies.push(to);
        }
        self.nodes[from.0].build_dependencies.insert(to);
        if let Some(ext) = extern_name {
            self.nodes[from.0].build_aliases.insert(to, ext.to_string());
        }
        if !self.nodes[to.0].dependents.contains(&from) {
            self.nodes[to.0].dependents.push(from);
        }
    }

    pub fn is_normal_edge(&self, from: NodeId, to: NodeId) -> bool {
        self.nodes[from.0].normal_dependencies.contains(&to)
    }

    pub fn node(&self, id: NodeId) -> &BuildNode {
        &self.nodes[id.0]
    }

    pub fn node_mut(&mut self, id: NodeId) -> &mut BuildNode {
        &mut self.nodes[id.0]
    }

    pub fn nodes(&self) -> &[BuildNode] {
        &self.nodes
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    fn describe_cycle(&self, in_degrees: &[usize]) -> String {
        let Some(start) = self.nodes.iter().find(|n| in_degrees[n.id.0] > 0) else {
            return "a dependency cycle".to_string();
        };
        let mut path: Vec<NodeId> = vec![start.id];
        loop {
            let cur = &self.nodes[path[path.len() - 1].0];
            let Some(&next) = cur.dependencies.iter().find(|d| in_degrees[d.0] > 0) else {
                break;
            };
            if let Some(i) = path.iter().position(|p| *p == next) {
                path.drain(..i);
                path.push(next);
                break;
            }
            path.push(next);
        }
        let name = |id: &NodeId| {
            let u = &self.nodes[id.0].unit;
            format!("{} v{}", u.crate_id.name, u.crate_id.version)
        };
        let mut names: Vec<String> = path.iter().map(name).collect();
        names.dedup();
        names.join(" → ")
    }

    pub fn topological_sort(&self) -> Result<Vec<NodeId>, IgelError> {
        let mut in_degrees = vec![0; self.nodes.len()];
        for node in &self.nodes {
            in_degrees[node.id.0] = node.dependencies.len();
        }

        let mut queue: Vec<NodeId> = self
            .nodes
            .iter()
            .filter(|n| in_degrees[n.id.0] == 0)
            .map(|n| n.id)
            .collect();

        let mut sorted = Vec::new();

        while let Some(id) = queue.pop() {
            sorted.push(id);
            for &dep_of_id in &self.nodes[id.0].dependents {
                in_degrees[dep_of_id.0] -= 1;
                if in_degrees[dep_of_id.0] == 0 {
                    queue.push(dep_of_id);
                }
            }
        }

        if sorted.len() != self.nodes.len() {
            return Err(IgelError::DependencyCycle {
                cycle: self.describe_cycle(&in_degrees),
            });
        }

        Ok(sorted)
    }

    /// The subgraph reachable from `roots`, and the old-id → new-id mapping.
    pub fn subgraph(&self, roots: &[NodeId]) -> (BuildGraph, HashMap<NodeId, NodeId>) {
        let mut keep: HashSet<NodeId> = HashSet::new();
        let mut stack: Vec<NodeId> = roots.to_vec();
        while let Some(id) = stack.pop() {
            if keep.insert(id) {
                stack.extend(self.nodes[id.0].dependencies.iter().copied());
            }
        }

        let mut out = BuildGraph::new();
        let mut map: HashMap<NodeId, NodeId> = HashMap::new();
        for node in &self.nodes {
            if keep.contains(&node.id) {
                let new_id = out.add_node(node.unit.clone());
                out.nodes[new_id.0].estimated_cost = node.estimated_cost;
                map.insert(node.id, new_id);
            }
        }
        for node in &self.nodes {
            let Some(&new_from) = map.get(&node.id) else {
                continue;
            };
            for &dep in &node.dependencies {
                let Some(&new_to) = map.get(&dep) else {
                    continue;
                };
                if node.normal_dependencies.contains(&dep) {
                    out.add_edge_with_extern(
                        new_from,
                        new_to,
                        node.extern_aliases.get(&dep).map(String::as_str),
                    );
                }
                if node.build_dependencies.contains(&dep) {
                    out.add_build_edge(
                        new_from,
                        new_to,
                        node.build_aliases.get(&dep).map(String::as_str),
                    );
                }
            }
        }
        (out, map)
    }

    pub fn find_by_name(&self, name: &str) -> Option<&[NodeId]> {
        self.name_index.get(name).map(|v| v.as_slice())
    }

    pub fn roots(&self) -> Vec<NodeId> {
        self.nodes
            .iter()
            .filter(|n| n.dependents.is_empty())
            .map(|n| n.id)
            .collect()
    }

    pub fn leaves(&self) -> Vec<NodeId> {
        self.nodes
            .iter()
            .filter(|n| n.dependencies.is_empty())
            .map(|n| n.id)
            .collect()
    }

    pub fn ready_set(&self, completed: &HashSet<NodeId>) -> Vec<NodeId> {
        self.nodes
            .iter()
            .filter(|n| !completed.contains(&n.id))
            .filter(|n| n.dependencies.iter().all(|dep| completed.contains(dep)))
            .map(|n| n.id)
            .collect()
    }
}

impl Default for BuildGraph {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use igel_common::{CompilationUnit, CrateId, CrateType, Edition};
    use std::path::PathBuf;

    fn make_unit(name: &str) -> CompilationUnit {
        CompilationUnit {
            crate_id: CrateId::local(name, semver::Version::new(0, 1, 0)),
            crate_type: CrateType::Bin,
            source_root: PathBuf::from("src/main.rs"),
            manifest_dir: PathBuf::from("."),
            features: Vec::new(),
            profile_name: "dev".to_string(),
            target: None,
            edition: Edition::E2021,
            is_build_script: false,
            check_only: false,
            links: None,
            build_script: None,
            meta: Default::default(),
            lint_args: Vec::new(),
            target_name: String::new(),
            kind: Default::default(),
            harness: false,
            ext: Default::default(),
        }
    }

    #[test]
    fn test_topological_sort() {
        let mut graph = BuildGraph::new();
        let a = graph.add_node(make_unit("a"));
        let b = graph.add_node(make_unit("b"));
        let c = graph.add_node(make_unit("c"));

        graph.add_edge(b, a);
        graph.add_edge(c, b);

        let order = graph.topological_sort().unwrap();
        assert_eq!(order, vec![a, b, c]);
    }

    #[test]
    fn test_cycle_detection() {
        let mut graph = BuildGraph::new();
        let a = graph.add_node(make_unit("a"));
        let b = graph.add_node(make_unit("b"));

        graph.add_edge(a, b);
        graph.add_edge(b, a);

        assert!(graph.topological_sort().is_err());
    }

    #[test]
    fn test_roots_and_leaves() {
        let mut graph = BuildGraph::new();
        let a = graph.add_node(make_unit("a"));
        let b = graph.add_node(make_unit("b"));
        let c = graph.add_node(make_unit("c"));

        graph.add_edge(b, a);
        graph.add_edge(c, a);

        let roots = graph.roots();
        assert_eq!(roots.len(), 2);
        assert!(roots.contains(&b));
        assert!(roots.contains(&c));

        let leaves = graph.leaves();
        assert_eq!(leaves, vec![a]);
    }

    #[test]
    fn test_subgraph_keeps_only_reachable_nodes() {
        let mut graph = BuildGraph::new();
        let a = graph.add_node(make_unit("a"));
        let b = graph.add_node(make_unit("b"));
        let c = graph.add_node(make_unit("c"));
        let unrelated = graph.add_node(make_unit("unrelated"));
        graph.add_edge_with_extern(c, b, Some("bee"));
        graph.add_edge(b, a);
        graph.add_edge(unrelated, a);

        let (sub, map) = graph.subgraph(&[c]);
        assert_eq!(sub.node_count(), 3);
        assert!(!map.contains_key(&unrelated));
        let new_c = map[&c];
        let new_b = map[&b];
        assert_eq!(sub.node(new_c).dependencies, vec![new_b]);
        assert_eq!(sub.node(new_c).extern_aliases[&new_b], "bee");
        assert!(sub.topological_sort().is_ok());
    }

    #[test]
    fn test_ready_set() {
        let mut graph = BuildGraph::new();
        let a = graph.add_node(make_unit("a"));
        let b = graph.add_node(make_unit("b"));
        let c = graph.add_node(make_unit("c"));

        graph.add_edge(b, a);
        graph.add_edge(c, b);

        let mut completed = HashSet::new();
        let ready = graph.ready_set(&completed);
        assert_eq!(ready, vec![a]);

        completed.insert(a);
        let ready = graph.ready_set(&completed);
        assert_eq!(ready, vec![b]);

        completed.insert(b);
        let ready = graph.ready_set(&completed);
        assert_eq!(ready, vec![c]);
    }
}
