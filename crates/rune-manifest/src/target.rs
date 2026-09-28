use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The kind of a build target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetKind {
    /// A library target.
    Lib,
    /// A binary target.
    Bin,
    /// An example target.
    Example,
    /// A test target.
    Test,
    /// A benchmark target.
    Bench,
    /// A custom build script target.
    CustomBuild,
}

/// A target specification in a Cargo.toml manifest (e.g., `[[bin]]` or `[lib]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TargetSpec {
    /// Name of the target.
    pub name: Option<String>,
    /// Path to the target's main source file.
    pub path: Option<PathBuf>,
    /// Crate types (e.g., `cdylib`, `rlib`). Cargo spells the key
    /// `crate-type`; `crate_type` is accepted as it is by Cargo itself.
    #[serde(default, rename = "crate-type", alias = "crate_type")]
    pub crate_types: Vec<String>,
    /// Required features for this target to be built.
    #[serde(default)]
    pub required_features: Vec<String>,
    /// Whether the standard test harness is used.
    #[serde(default = "default_true")]
    pub harness: bool,
    /// The Rust edition.
    pub edition: Option<String>,
    /// Whether the target is documented.
    #[serde(default = "default_true")]
    pub doc: bool,
    /// Whether doctests are run for this target.
    #[serde(default = "default_true")]
    pub doctest: bool,
    /// Whether this target is tested by default, when the manifest says;
    /// see [`TargetSpec::tested`] for the default (which depends on the kind).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test: Option<bool>,
    /// Whether this target is benchmarked by default.
    #[serde(default = "default_true")]
    pub bench: bool,
    /// Whether this target is a proc-macro crate.
    #[serde(default)]
    pub proc_macro: bool,
    /// Extra target fields.
    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, toml::Value>,
}

impl TargetSpec {
    /// Whether `cargo test` tests this target by default: what the manifest
    /// says, else `default` — `true` for libraries, binaries and tests,
    /// `false` for examples (they are only built).
    pub fn tested(&self, default: bool) -> bool {
        self.test.unwrap_or(default)
    }
}

fn default_true() -> bool {
    true
}

impl Default for TargetSpec {
    /// The same defaults serde applies to a `[[bin]]`/`[lib]` table with only
    /// a name: harness, docs, doctests, tests and benches all *on*. A derived
    /// `Default` would give every `bool` `false`, which silently turned off
    /// testing for every automatically discovered target.
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
