use serde::{Deserialize, Deserializer, Serialize};
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DependencyKind {
    Normal,
    Dev,
    Build,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct DependencySpec {
    pub version: Option<semver::VersionReq>,
    pub path: Option<PathBuf>,
    pub git: Option<String>,
    pub branch: Option<String>,
    pub tag: Option<String>,
    pub rev: Option<String>,
    pub features: Vec<String>,
    pub optional: bool,
    pub default_features: bool,
    pub package: Option<String>,
    pub registry: Option<String>,
    pub workspace: bool,
}

impl DependencySpec {
    /// Merges a `[workspace.dependencies]` entry into a `dep.workspace = true` spec. Must run right after
    /// parsing, before any use: every traversal that reads such a manifest needs it.
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
        #[allow(clippy::large_enum_variant)]
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            String(String),
            // Repeated here: the enum's `rename_all` doesn't apply to variant fields (`default-features`).
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

/// The spec as it applies to its package: paths absolute (relative to where they were written) and
/// workspace inheritance merged.
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    pub name: String,
    pub kind: DependencyKind,
    pub spec: DependencySpec,
    pub target: Option<String>,
}

impl Dependency {
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
        let toml_doc =
            r#"dep = { version = "0.12", default-features = false, features = ["rustls-tls"] }"#;
        let table: toml::Table = toml::from_str(toml_doc).unwrap();
        let val = &table["dep"];
        let dep = Dependency::try_from_toml("reqwest", DependencyKind::Normal, val, None).unwrap();
        assert!(!dep.spec.default_features);
        assert_eq!(dep.spec.features, vec!["rustls-tls"]);
    }
}
