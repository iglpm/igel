//! Build graph, topological sort, feature unification and critical paths.

pub mod critical_path;
pub mod dag;
pub mod features;

pub use critical_path::{
    compute_critical_paths, compute_critical_paths_pipelined, compute_critical_paths_with,
    priority_order,
};
pub use dag::{BuildGraph, BuildNode, NodeId};
