use crate::dependency::DependencySpec;
use crate::error::ManifestError;
use glob::glob;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Workspace package fields that can be inherited by member packages.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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
    /// Workspace homepage.
    pub homepage: Option<String>,
    /// Workspace documentation URL.
    pub documentation: Option<String>,
    /// Workspace readme path.
    pub readme: Option<String>,
    /// Workspace license file path.
    pub license_file: Option<String>,
    /// Everything else that can be inherited (`keywords`, `categories`,
    /// `publish`, `include`, `exclude`, ...); these don't reach the compiler.
    #[serde(flatten)]
    pub extra: HashMap<String, toml::Value>,
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
    /// Free-form `[workspace.metadata]`.
    #[serde(default)]
    pub metadata: Option<toml::Value>,
}

/// Finds the `[workspace]` a package at `dir` (with manifest `manifest`)
/// belongs to: an explicit `package.workspace = "../.."`, else the nearest
/// parent whose `members` include it. Package dependencies that live in some
/// *other* workspace (a path dependency next door, a git repository with its
/// own `[workspace]`) inherit their version, edition and dependencies from
/// that workspace, not from the project being built.
pub fn find_enclosing_workspace(
    dir: &Path,
    manifest: &crate::Manifest,
) -> Option<(PathBuf, WorkspaceManifest)> {
    let load = |root: &Path| -> Option<WorkspaceManifest> {
        crate::Manifest::from_path(&rune_common::manifest_file::manifest_in(root))
            .ok()
            .and_then(|m| m.workspace)
    };
    if let Some(explicit) = manifest.package.as_ref().and_then(|p| p.workspace.as_ref()) {
        let root = dir.join(explicit).canonicalize().ok()?;
        return load(&root).map(|w| (root, w));
    }
    // A package that is itself a workspace root is its own workspace.
    if let Some(ws) = &manifest.workspace {
        return Some((dir.to_path_buf(), ws.clone()));
    }
    let mut cur = dir.parent()?.to_path_buf();
    loop {
        if rune_common::manifest_file::has_manifest(&cur)
            && let Some(ws) = load(&cur)
        {
            let is_member = ws
                .resolve_members(&cur)
                .unwrap_or_default()
                .iter()
                .any(|d| d.canonicalize().unwrap_or_else(|_| d.clone()) == dir);
            if is_member {
                return Some((cur, ws));
            }
        }
        if !cur.pop() {
            return None;
        }
    }
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
                        if rune_common::manifest_file::has_manifest(&path)
                            && !excluded.contains(&path)
                        {
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
