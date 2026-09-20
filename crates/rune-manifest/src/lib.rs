//! # rune-manifest
//!
//! Cargo.toml and Rune.toml manifest parser for the Rune build system.

pub mod cargo_toml;
pub mod dependency;
pub mod error;
pub mod lints;
pub mod profile;
pub mod target;
pub mod workspace;

pub use cargo_toml::Manifest;
pub use dependency::{Dependency, DependencyKind, DependencySpec, effective_spec};
pub use error::ManifestError;
pub use profile::{ProfileError, UnitRole, resolve_profile, resolve_unit_profile};
pub use target::TargetSpec;
pub use workspace::{WorkspaceManifest, find_enclosing_workspace};
