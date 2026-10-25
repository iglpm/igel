use crate::dag::{BuildGraph, BuildNode, NodeId};
use std::collections::HashMap;

/// Longest remaining chain of work from each node to the end of the build ("bottom level").
pub fn compute_critical_paths(graph: &BuildGraph) -> HashMap<NodeId, f64> {
    compute_critical_paths_with(graph, |node| node.estimated_cost)
}

pub fn compute_critical_paths_with(
    graph: &BuildGraph,
    cost: impl Fn(&BuildNode) -> f64,
) -> HashMap<NodeId, f64> {
    compute_critical_paths_pipelined(graph, cost, |dep, _dependent, dep_cost| {
        let _ = dep;
        dep_cost
    })
}

/// Like [`compute_critical_paths_with`], with a delay before a dependent may start (pipelining).
pub fn compute_critical_paths_pipelined(
    graph: &BuildGraph,
    cost: impl Fn(&BuildNode) -> f64,
    delay: impl Fn(&BuildNode, &BuildNode, f64) -> f64,
) -> HashMap<NodeId, f64> {
    let mut weights: HashMap<NodeId, f64> = HashMap::new();
    let Ok(order) = graph.topological_sort() else {
        return weights;
    };
    let costs: HashMap<NodeId, f64> = order
        .iter()
        .map(|&id| (id, cost(graph.node(id)).max(0.0)))
        .collect();
    for &id in &order {
        weights.insert(id, costs[&id]);
    }
    for &id in order.iter().rev() {
        let node = graph.node(id);
        let own = weights[&id];
        for &dep_id in &node.dependencies {
            let dep_cost = costs.get(&dep_id).copied().unwrap_or(0.0);
            let through = delay(graph.node(dep_id), node, dep_cost) + own;
            let entry = weights.entry(dep_id).or_insert(dep_cost);
            if through > *entry {
                *entry = through;
            }
        }
    }
    weights
}

pub fn priority_order(graph: &BuildGraph) -> Vec<NodeId> {
    let weights = compute_critical_paths(graph);

    let mut nodes: Vec<NodeId> = graph.nodes().iter().map(|n| n.id).collect();

    nodes.sort_by(|a, b| {
        let weight_a = weights.get(a).copied().unwrap_or(0.0);
        let weight_b = weights.get(b).copied().unwrap_or(0.0);

        weight_b
            .partial_cmp(&weight_a)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    nodes
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
    fn test_critical_path() {
        let mut graph = BuildGraph::new();
        let a = graph.add_node(make_unit("a"));
        let b = graph.add_node(make_unit("b"));
        let c = graph.add_node(make_unit("c"));

        graph.node_mut(a).estimated_cost = 2.0;
        graph.node_mut(b).estimated_cost = 3.0;
        graph.node_mut(c).estimated_cost = 1.0;

        graph.add_edge(b, a);
        graph.add_edge(c, b);

        let weights = compute_critical_paths(&graph);

        assert_eq!(weights[&c], 1.0);
        assert_eq!(weights[&b], 4.0);
        assert_eq!(weights[&a], 6.0);
        assert_eq!(priority_order(&graph), vec![a, b, c]);
    }

    #[test]
    fn a_short_chain_does_not_outrank_the_long_one() {
        let mut graph = BuildGraph::new();
        let slow = graph.add_node(make_unit("slow"));
        let root = graph.add_node(make_unit("root"));
        let mid = graph.add_node(make_unit("mid"));
        let top = graph.add_node(make_unit("top"));
        graph.node_mut(slow).estimated_cost = 5.0;
        graph.node_mut(root).estimated_cost = 1.0;
        graph.node_mut(mid).estimated_cost = 4.0;
        graph.node_mut(top).estimated_cost = 4.0;
        graph.add_edge(mid, root);
        graph.add_edge(top, mid);

        let w = compute_critical_paths(&graph);
        assert_eq!(w[&slow], 5.0);
        assert_eq!(w[&root], 9.0);
        assert!(w[&root] > w[&slow]);
    }

    #[test]
    fn pipelined_edges_only_wait_for_the_metadata() {
        let mut graph = BuildGraph::new();
        let lib = graph.add_node(make_unit("lib"));
        let app = graph.add_node(make_unit("app"));
        graph.node_mut(lib).estimated_cost = 10.0;
        graph.node_mut(app).estimated_cost = 2.0;
        graph.add_edge(app, lib);

        let w = compute_critical_paths_pipelined(
            &graph,
            |n| n.estimated_cost,
            |_, _, dep_cost| dep_cost * 0.4,
        );
        assert_eq!(w[&lib], 10.0);
        assert_eq!(w[&app], 2.0);

        let w = compute_critical_paths_pipelined(
            &graph,
            |n| n.estimated_cost,
            |_, _, dep_cost| dep_cost,
        );
        assert_eq!(w[&lib], 12.0);
    }
}
