//! Dependency resolution, feature unification, lockfiles, `igl tree`/`igl why`.

pub mod features;
pub mod lockfile;
pub mod patch;
pub mod resolver;
mod solver;
pub mod tree;
pub mod unit_features;

pub use features::{DynamicFeatureResolver, ResolvedPackageFeatures};
pub use lockfile::{LockPackage, Lockfile, LockfileManager};
pub use patch::{PatchEntry, PatchSource, Patches};
pub use resolver::{DependencyResolver, PackageSource, ResolvedGraph, ResolvedPackage};
pub use tree::{TreeOptions, render_dependency_tree, render_tree, trace_why};
