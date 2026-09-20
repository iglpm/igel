use crate::dag::{BuildGraph, NodeId};
use std::collections::HashMap;

/// Compute the critical path weight for each node.
/// The critical path weight of a node is:
///   its own cost + max(critical_path_weight of all its dependencies)
/// This gives us a priority: nodes on the critical path should be scheduled first.
pub fn compute_critical_paths(graph: &BuildGraph) -> HashMap<NodeId, f64> {
    compute_critical_paths_with(graph, |node| node.estimated_cost)
}

/// Like [`compute_critical_paths`], but with a caller-supplied per-node cost —
/// e.g. measured compile times from a previous build, with cached (already
/// finished) nodes costing nothing so only the *remaining* work shapes the
/// schedule.
pub fn compute_critical_paths_with(
    graph: &BuildGraph,
    cost: impl Fn(&crate::dag::BuildNode) -> f64,
) -> HashMap<NodeId, f64> {
    let mut weights = HashMap::new();

    // Process in topological order so dependencies are computed before dependents
    if let Ok(order) = graph.topological_sort() {
        for &id in &order {
            let node = graph.node(id);
            let mut max_dep_weight = 0.0_f64;

            for &dep_id in &node.dependencies {
                if let Some(&dep_weight) = weights.get(&dep_id) {
                    if dep_weight > max_dep_weight {
                        max_dep_weight = dep_weight;
                    }
                }
            }

            weights.insert(id, cost(node) + max_dep_weight);
        }
    }

    weights
}

/// Returns nodes sorted by critical path weight (descending).
/// When multiple nodes are ready, the scheduler should pick the one
/// with the highest critical path weight.
pub fn priority_order(graph: &BuildGraph) -> Vec<NodeId> {
    let weights = compute_critical_paths(graph);

    let mut nodes: Vec<NodeId> = graph.nodes().iter().map(|n| n.id).collect();

    nodes.sort_by(|a, b| {
        let weight_a = weights.get(a).copied().unwrap_or(0.0);
        let weight_b = weights.get(b).copied().unwrap_or(0.0);

        // Sort descending
        weight_b
            .partial_cmp(&weight_a)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    nodes
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune_common::{CompilationUnit, CrateId, CrateType, Edition};
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
        let a = graph.add_node(make_unit("a")); // default cost 1.0
        let b = graph.add_node(make_unit("b")); // default cost 1.0
        let c = graph.add_node(make_unit("c")); // default cost 1.0

        graph.node_mut(a).estimated_cost = 2.0;
        graph.node_mut(b).estimated_cost = 3.0;
        graph.node_mut(c).estimated_cost = 1.0;

        // c depends on b, b depends on a
        graph.add_edge(b, a);
        graph.add_edge(c, b);

        let weights = compute_critical_paths(&graph);

        // a: 2.0
        // b: 3.0 + 2.0 = 5.0
        // c: 1.0 + 5.0 = 6.0
        assert_eq!(weights.get(&a).copied().unwrap(), 2.0);
        assert_eq!(weights.get(&b).copied().unwrap(), 5.0);
        assert_eq!(weights.get(&c).copied().unwrap(), 6.0);

        let order = priority_order(&graph);
        assert_eq!(order, vec![c, b, a]);
    }
}
