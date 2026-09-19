//! # rune-resolver
//!
//! High-speed SemVer dependency resolver, dynamic feature unifier,
//! dual binary (`Rune.lock.bin`) & standard (`Cargo.lock`) lockfile manager,
//! dependency tree visualizer, and reverse-dependency tracer for Rune.

pub mod features;
pub mod lockfile;
pub mod resolver;
pub mod tree;

pub use features::{DynamicFeatureResolver, ResolvedPackageFeatures};
pub use lockfile::{LockPackage, Lockfile, LockfileManager};
pub use resolver::{DependencyResolver, ResolvedGraph, ResolvedPackage};
pub use tree::{TreeOptions, render_dependency_tree, render_tree, trace_why};
