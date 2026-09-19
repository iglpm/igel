use crate::dependency::DependencySpec;
use crate::error::ManifestError;
use glob::glob;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Workspace package fields that can be inherited by member packages.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct WorkspacePackage {
    /// Workspace version.
    pub version: Option<semver::Version>,
    /// Workspace edition.
    pub edition: Option<String>,
    /// Workspace authors.
    pub authors: Option<Vec<String>>,
    /// Workspace description.
    pub description: Option<String>,
    /// Workspace license.
    pub license: Option<String>,
    /// Workspace repository URL.
    pub repository: Option<String>,
    /// Workspace rust-version.
    pub rust_version: Option<String>,
}

/// Workspace manifest definition.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct WorkspaceManifest {
    /// Patterns for workspace members.
    #[serde(default)]
    pub members: Vec<String>,
    /// Patterns for excluded packages.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Patterns for default members.
    #[serde(default)]
    pub default_members: Vec<String>,
    /// The dependency resolver version.
    pub resolver: Option<String>,
    /// Shared workspace package fields.
    pub package: Option<WorkspacePackage>,
    /// Shared workspace dependencies.
    #[serde(default)]
    pub dependencies: HashMap<String, DependencySpec>,
    /// Shared `[workspace.lints]`.
    #[serde(default)]
    pub lints: Option<toml::Value>,
}

impl WorkspaceManifest {
    /// Resolves the paths of workspace members based on the `members` glob
    /// patterns, honoring `exclude`. Results are sorted and deduplicated so
    /// the order never depends on directory iteration order.
    pub fn resolve_members(&self, root: &Path) -> Result<Vec<PathBuf>, ManifestError> {
        self.expand(root, &self.members)
    }

    /// Resolves `default-members` (empty means "no explicit default").
    pub fn resolve_default_members(&self, root: &Path) -> Result<Vec<PathBuf>, ManifestError> {
        self.expand(root, &self.default_members)
    }

    fn expand(&self, root: &Path, patterns: &[String]) -> Result<Vec<PathBuf>, ManifestError> {
        let excluded: Vec<PathBuf> = self
            .exclude
            .iter()
            .flat_map(|pattern| {
                glob(&root.join(pattern).to_string_lossy())
                    .into_iter()
                    .flatten()
                    .flatten()
            })
            .collect();

        let mut paths = Vec::new();
        for pattern in patterns {
            let full_pattern = root.join(pattern).to_string_lossy().to_string();
            for entry in glob(&full_pattern)? {
                match entry {
                    Ok(path) => {
                        if path.join("Cargo.toml").exists() && !excluded.contains(&path) {
                            paths.push(path);
                        }
                    }
                    Err(e) => return Err(ManifestError::Parse(e.to_string())),
                }
            }
        }
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_workspace() {
        let toml_str = r#"
        members = ["crates/*"]
        
        [package]
        version = "1.0.0"
        
        [dependencies]
        serde = "1.0"
        "#;

        let ws: WorkspaceManifest = toml::from_str(toml_str).unwrap();
        assert_eq!(ws.members, vec!["crates/*"]);
        assert_eq!(ws.package.unwrap().version.unwrap().to_string(), "1.0.0");
        assert!(ws.dependencies.contains_key("serde"));
    }

    #[test]
    fn test_members_are_sorted_and_respect_exclude() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["zeta", "alpha", "skipme", "notapackage"] {
            std::fs::create_dir_all(dir.path().join("crates").join(name)).unwrap();
            if name != "notapackage" {
                std::fs::write(dir.path().join("crates").join(name).join("Cargo.toml"), "")
                    .unwrap();
            }
        }
        let ws = WorkspaceManifest {
            members: vec!["crates/*".into()],
            exclude: vec!["crates/skipme".into()],
            ..Default::default()
        };
        let members = ws.resolve_members(dir.path()).unwrap();
        let names: Vec<_> = members
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(names, ["alpha", "zeta"]);
    }
}
