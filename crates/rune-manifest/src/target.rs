use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetKind {
    Lib,
    Bin,
    Example,
    Test,
    Bench,
    CustomBuild,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TargetSpec {
    pub name: Option<String>,
    pub path: Option<PathBuf>,
    #[serde(default, rename = "crate-type", alias = "crate_type")]
    pub crate_types: Vec<String>,
    #[serde(default)]
    pub required_features: Vec<String>,
    #[serde(default = "default_true")]
    pub harness: bool,
    pub edition: Option<String>,
    #[serde(default = "default_true")]
    pub doc: bool,
    #[serde(default = "default_true")]
    pub doctest: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test: Option<bool>,
    #[serde(default = "default_true")]
    pub bench: bool,
    #[serde(default)]
    pub proc_macro: bool,
    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, toml::Value>,
}

impl TargetSpec {
    pub fn tested(&self, default: bool) -> bool {
        self.test.unwrap_or(default)
    }
}

fn default_true() -> bool {
    true
}

impl Default for TargetSpec {
    /// Same defaults serde applies: a derived `Default` would turn off testing of discovered targets.
    fn default() -> Self {
        Self {
            name: None,
            path: None,
            crate_types: Vec::new(),
            required_features: Vec::new(),
            harness: true,
            edition: None,
            doc: true,
            doctest: true,
            test: None,
            bench: true,
            proc_macro: false,
            extra: Default::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn programmatic_default_matches_parsed_default() {
        let parsed: TargetSpec = toml::from_str(r#"name = "x""#).unwrap();
        let built = TargetSpec {
            name: Some("x".into()),
            ..Default::default()
        };
        assert_eq!(
            parsed, built,
            "auto-discovered targets must behave like declared ones"
        );
    }

    #[test]
    fn test_target_spec_defaults() {
        let toml_str = r#"name = "my-bin""#;
        let spec: TargetSpec = toml::from_str(toml_str).unwrap();
        assert_eq!(spec.name.as_deref(), Some("my-bin"));
        assert!(spec.harness);
        assert!(spec.doc);
        assert!(!spec.proc_macro);
    }
}
