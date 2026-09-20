use serde::{Deserialize, Deserializer, Serialize};
use std::path::PathBuf;
use std::str::FromStr;

/// The kind of dependency (normal, dev, build).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DependencyKind {
    /// Normal dependency.
    Normal,
    /// Development dependency.
    Dev,
    /// Build dependency.
    Build,
}

/// A dependency specification in a Cargo.toml manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct DependencySpec {
    /// Semantic version requirement.
    pub version: Option<semver::VersionReq>,
    /// Local path to the crate.
    pub path: Option<PathBuf>,
    /// Git repository URL.
    pub git: Option<String>,
    /// Git branch.
    pub branch: Option<String>,
    /// Git tag.
    pub tag: Option<String>,
    /// Git revision.
    pub rev: Option<String>,
    /// Enabled features.
    pub features: Vec<String>,
    /// Whether the dependency is optional.
    pub optional: bool,
    /// Whether default features are enabled.
    pub default_features: bool,
    /// Package name, if different from the dependency name.
    pub package: Option<String>,
    /// Registry to use.
    pub registry: Option<String>,
    /// Whether this dependency is inherited from the workspace.
    pub workspace: bool,
}

impl DependencySpec {
    /// Merges fields inherited from a `[workspace.dependencies]` entry into
    /// this spec, for a member's `dep.workspace = true` reference.
    ///
    /// There is exactly one correct place to call this: right after parsing
    /// any manifest that might contain a `workspace = true` dependency,
    /// before that spec is used for anything (resolution, feature
    /// unification, or being carried into a nested local-path traversal).
    /// A second, independent traversal that re-reads the same manifest
    /// without calling this — e.g. a resolver walking `path = "..."`
    /// dependencies on its own — silently reverts every inherited field
    /// back to the member's unmerged (default) values, which is exactly how
    /// this was missed once already: the CLI's root-dependency collection
    /// merged correctly, but the resolver's separate local-path-dependency
    /// walk (taken for every internal `path` dependency reachable
    /// transitively, which includes every workspace member depending on
    /// another) read the same `workspace = true` spec straight off disk and
    /// never merged it, undoing `default-features = false` the moment a
    /// crate was reached that way instead of as a declared root dependency.
    pub fn merge_workspace(&mut self, ws_dep: &DependencySpec) {
        if !self.workspace {
            return;
        }
        if self.version.is_none() {
            self.version = ws_dep.version.clone();
        }
        if self.path.is_none() && ws_dep.path.is_some() {
            self.path = ws_dep.path.clone();
        }
        if self.git.is_none() && ws_dep.git.is_some() {
            self.git = ws_dep.git.clone();
            self.branch = ws_dep.branch.clone();
            self.tag = ws_dep.tag.clone();
            self.rev = ws_dep.rev.clone();
        }
        if self.registry.is_none() {
            self.registry = ws_dep.registry.clone();
        }
        if self.package.is_none() {
            self.package = ws_dep.package.clone();
        }
        if self.features.is_empty() {
            self.features = ws_dep.features.clone();
        }
        // Not distinguishable from "member explicitly re-set the default",
        // since the parser has no way to tell an unset field from one that
        // was explicitly written as `true`; always inheriting the workspace
        // value is correct for the overwhelmingly common bare
        // `dep.workspace = true` case.
        self.default_features = ws_dep.default_features;
        if !self.optional && ws_dep.optional {
            self.optional = ws_dep.optional;
        }
    }
}

impl<'de> Deserialize<'de> for DependencySpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            String(String),
            // `rename_all` on the enum itself only renames variant names, not
            // the fields of a struct-like variant — it must be repeated here
            // for `default-features` (and any other hyphenated key) to
            // actually bind. Without it, serde looks for a literal
            // `default_features` key, never finds it in real Cargo.toml
            // files (which always write the hyphenated form), and silently
            // falls back to `default_true()` — so every `default-features =
            // false` in every manifest, root or transitive, was being
            // ignored and every dependency resolved with its full default
            // feature set regardless of what the manifest actually said.
            #[serde(rename_all = "kebab-case")]
            Table {
                version: Option<String>,
                path: Option<PathBuf>,
                git: Option<String>,
                branch: Option<String>,
                tag: Option<String>,
                rev: Option<String>,
                #[serde(default)]
                features: Vec<String>,
                #[serde(default)]
                optional: bool,
                #[serde(default = "default_true")]
                default_features: bool,
                package: Option<String>,
                registry: Option<String>,
                #[serde(default)]
                workspace: bool,
            },
        }

        fn default_true() -> bool {
            true
        }

        match Repr::deserialize(deserializer)? {
            Repr::String(s) => {
                let version = semver::VersionReq::from_str(&s).map_err(serde::de::Error::custom)?;
                Ok(DependencySpec {
                    version: Some(version),
                    default_features: true,
                    ..Default::default()
                })
            }
            Repr::Table {
                version,
                path,
                git,
                branch,
                tag,
                rev,
                features,
                optional,
                default_features,
                package,
                registry,
                workspace,
            } => {
                let version = version
                    .map(|v| semver::VersionReq::from_str(&v).map_err(serde::de::Error::custom))
                    .transpose()?;
                Ok(DependencySpec {
                    version,
                    path,
                    git,
                    branch,
                    tag,
                    rev,
                    features,
                    optional,
                    default_features,
                    package,
                    registry,
                    workspace,
                })
            }
        }
    }
}

/// A dependency as it applies to the package that declares it: relative paths
/// made absolute and `workspace = true` inheritance merged in.
///
/// A `path` written on a member is relative to that member; one inherited from
/// `[workspace.dependencies]` is relative to the workspace root. Both are made
/// absolute against the directory they were *written in* before merging, so
/// every later consumer can just use the result.
pub fn effective_spec(
    dep: &Dependency,
    member_dir: &std::path::Path,
    ws_root: &std::path::Path,
    ws: Option<&crate::WorkspaceManifest>,
) -> DependencySpec {
    let mut spec = dep.spec.clone();
    if let Some(rel) = spec.path.take() {
        spec.path = Some(member_dir.join(rel));
    }
    if let Some(ws_dep) = ws.and_then(|w| w.dependencies.get(&dep.name)) {
        let mut ws_dep = ws_dep.clone();
        if let Some(rel) = ws_dep.path.take() {
            ws_dep.path = Some(ws_root.join(rel));
        }
        spec.merge_workspace(&ws_dep);
    }
    spec
}

/// A full dependency combining name, kind, and specification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    /// Dependency name.
    pub name: String,
    /// Dependency kind.
    pub kind: DependencyKind,
    /// Dependency specification.
    pub spec: DependencySpec,
    /// Target-specific restriction, e.g. `cfg(unix)`.
    pub target: Option<String>,
}

impl Dependency {
    /// Try parsing a dependency from a TOML value.
    pub fn try_from_toml(
        name: &str,
        kind: DependencyKind,
        value: &toml::Value,
        target: Option<String>,
    ) -> Result<Self, String> {
        let spec: DependencySpec = value
            .clone()
            .try_into()
            .map_err(|e: toml::de::Error| e.to_string())?;
        Ok(Self {
            name: name.to_string(),
            kind,
            spec,
            target,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_string_dependency() {
        let val = toml::Value::String("1.0".to_string());
        let dep = Dependency::try_from_toml("serde", DependencyKind::Normal, &val, None).unwrap();
        assert_eq!(dep.spec.version.unwrap().to_string(), "^1.0");
        assert!(dep.spec.default_features);
    }

    #[test]
    fn test_parse_table_dependency() {
        let toml_doc = r#"dep = { version = "1.0", features = ["derive"], optional = true }"#;
        let table: toml::Table = toml::from_str(toml_doc).unwrap();
        let val = &table["dep"];
        let dep = Dependency::try_from_toml("serde", DependencyKind::Normal, val, None).unwrap();
        assert_eq!(dep.spec.version.unwrap().to_string(), "^1.0");
        assert_eq!(dep.spec.features, vec!["derive"]);
        assert!(dep.spec.optional);
    }

    #[test]
    fn test_parse_default_features_false() {
        // Regression test: `default-features` (the hyphenated form Cargo.toml
        // actually uses) must bind to `DependencySpec::default_features`.
        // The inner `Repr::Table` enum variant used to have no `rename_all`
        // of its own, so serde looked for a literal `default_features` key,
        // never found it, and silently defaulted to `true` no matter what
        // the manifest said.
        let toml_doc =
            r#"dep = { version = "0.12", default-features = false, features = ["rustls-tls"] }"#;
        let table: toml::Table = toml::from_str(toml_doc).unwrap();
        let val = &table["dep"];
        let dep = Dependency::try_from_toml("reqwest", DependencyKind::Normal, val, None).unwrap();
        assert!(!dep.spec.default_features);
        assert_eq!(dep.spec.features, vec!["rustls-tls"]);
    }
}
