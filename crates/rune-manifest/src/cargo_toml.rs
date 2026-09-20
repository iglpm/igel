use crate::dependency::{Dependency, DependencyKind, DependencySpec};
use crate::error::ManifestError;
use crate::target::TargetSpec;
use crate::workspace::WorkspaceManifest;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// An inheritable string property (either a direct string or `{ workspace = true }`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum InheritableString {
    Direct(String),
    Inherited { workspace: bool },
}

impl InheritableString {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            InheritableString::Direct(s) => Some(s.as_str()),
            InheritableString::Inherited { .. } => None,
        }
    }
}

/// The `[package]` section of a Cargo.toml manifest.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct PackageManifest {
    /// Package name.
    pub name: String,
    /// Package version.
    #[serde(default)]
    pub version: Option<InheritableString>,
    /// Rust edition.
    #[serde(default)]
    pub edition: Option<InheritableString>,
    /// Build script path or bool.
    #[serde(default)]
    pub build: Option<toml::Value>,
    /// Links field.
    pub links: Option<String>,
    /// Auto-discovery of binaries.
    pub autobins: Option<bool>,
    /// Auto-discovery of the library (`autolib`, Cargo 1.83+).
    pub autolib: Option<bool>,
    /// Minimum supported Rust version.
    pub rust_version: Option<InheritableString>,
    /// Path to workspace root.
    pub workspace: Option<String>,
    /// Feature resolver version (`"1"`, `"2"`, `"3"`); only meaningful on the
    /// root package of a workspace, exactly as in Cargo.
    pub resolver: Option<String>,
    /// Binary `cargo run` picks when a package has several.
    pub default_run: Option<String>,
    /// Description.
    #[serde(default)]
    pub description: Option<InheritableString>,
    /// License.
    #[serde(default)]
    pub license: Option<InheritableString>,
    /// Catch-all for extra metadata and arbitrary fields.
    #[serde(flatten)]
    pub extra: HashMap<String, toml::Value>,
}

impl PackageManifest {
    /// The package version, following `version.workspace = true` into
    /// `[workspace.package]`.
    pub fn version_in(&self, ws: Option<&WorkspaceManifest>) -> Option<semver::Version> {
        self.version_str().and_then(|s| s.parse().ok()).or_else(|| {
            ws.and_then(|w| w.package.as_ref())
                .and_then(|p| p.version.clone())
        })
    }

    /// The package edition, following `edition.workspace = true`; defaults to
    /// 2015 when unspecified, as rustc and Cargo do.
    pub fn edition_in(&self, ws: Option<&WorkspaceManifest>) -> String {
        self.edition_str()
            .map(str::to_string)
            .or_else(|| {
                ws.and_then(|w| w.package.as_ref())
                    .and_then(|p| p.edition.clone())
            })
            .unwrap_or_else(|| "2015".to_string())
    }

    /// Package metadata exposed to the compiler as `CARGO_PKG_*`.
    pub fn meta_in(&self, ws: Option<&WorkspaceManifest>) -> rune_common::PackageMeta {
        let wsp = ws.and_then(|w| w.package.as_ref());
        // A string field, possibly `{ workspace = true }`.
        let text = |key: &str, from_ws: Option<&String>| -> String {
            match self.extra.get(key) {
                Some(toml::Value::String(s)) => s.clone(),
                Some(toml::Value::Table(t))
                    if t.get("workspace").and_then(|v| v.as_bool()) == Some(true) =>
                {
                    from_ws.cloned().unwrap_or_default()
                }
                _ => String::new(),
            }
        };
        let description = match &self.description {
            Some(InheritableString::Direct(s)) => s.clone(),
            Some(InheritableString::Inherited { .. }) => {
                wsp.and_then(|p| p.description.clone()).unwrap_or_default()
            }
            None => String::new(),
        };
        let license = match &self.license {
            Some(InheritableString::Direct(s)) => s.clone(),
            Some(InheritableString::Inherited { .. }) => {
                wsp.and_then(|p| p.license.clone()).unwrap_or_default()
            }
            None => String::new(),
        };
        let rust_version = match &self.rust_version {
            Some(InheritableString::Direct(s)) => s.clone(),
            Some(InheritableString::Inherited { .. }) => {
                wsp.and_then(|p| p.rust_version.clone()).unwrap_or_default()
            }
            None => String::new(),
        };
        let authors = match self.extra.get("authors") {
            Some(toml::Value::Array(a)) => a
                .iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(":"),
            Some(toml::Value::Table(t))
                if t.get("workspace").and_then(|v| v.as_bool()) == Some(true) =>
            {
                wsp.and_then(|p| p.authors.clone())
                    .map(|a| a.join(":"))
                    .unwrap_or_default()
            }
            _ => String::new(),
        };
        rune_common::PackageMeta {
            name: self.name.clone(),
            authors,
            description,
            homepage: text("homepage", wsp.and_then(|p| p.homepage.as_ref())),
            repository: text("repository", wsp.and_then(|p| p.repository.as_ref())),
            license,
            license_file: text("license-file", wsp.and_then(|p| p.license_file.as_ref())),
            readme: match self.extra.get("readme") {
                Some(toml::Value::String(s)) => s.clone(),
                Some(toml::Value::Boolean(true)) => "README.md".to_string(),
                Some(toml::Value::Table(t))
                    if t.get("workspace").and_then(|v| v.as_bool()) == Some(true) =>
                {
                    wsp.and_then(|p| p.readme.clone()).unwrap_or_default()
                }
                _ => String::new(),
            },
            rust_version,
        }
    }

    /// Returns the version string if directly defined.
    pub fn version_str(&self) -> Option<&str> {
        self.version.as_ref().and_then(|v| v.as_str())
    }

    /// Returns the edition string if directly defined.
    pub fn edition_str(&self) -> Option<&str> {
        self.edition.as_ref().and_then(|e| e.as_str())
    }

    /// Resolves the effective build script path for this package, following
    /// Cargo's own `package.build` semantics:
    /// - `build = "path/to/script.rs"` uses that explicit, non-default path
    ///   (e.g. `rustversion` uses `build = "build/build.rs"`).
    /// - `build = true` uses the default `build.rs`.
    /// - `build = false` disables build scripts outright, even if a
    ///   `build.rs` physically exists at the crate root.
    /// - Unset (the common case) auto-detects `build.rs` at the crate root,
    ///   only if the file actually exists.
    ///
    /// A caller that instead assumes every build script lives at
    /// `<manifest_dir>/build.rs` will silently skip build scripts declared
    /// at a custom path, which produces at-compile-time failures (e.g. a
    /// crate expecting `OUT_DIR` to be set) that are hard to trace back to
    /// this root cause.
    pub fn resolve_build_script(&self, manifest_dir: &Path) -> Option<PathBuf> {
        match &self.build {
            Some(toml::Value::String(s)) => Some(manifest_dir.join(s)),
            Some(toml::Value::Boolean(true)) => Some(manifest_dir.join("build.rs")),
            Some(toml::Value::Boolean(false)) => None,
            _ => {
                let default = manifest_dir.join("build.rs");
                default.exists().then_some(default)
            }
        }
    }
}

/// Build profile overrides.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ProfileOverride {
    /// Optimization level.
    pub opt_level: Option<toml::Value>,
    /// Debug info.
    pub debug: Option<toml::Value>,
    /// `[profile.X.package.<name>]` / `[profile.X.package."*"]` overrides.
    #[serde(default)]
    pub package: HashMap<String, ProfileOverride>,
    /// `[profile.X.build-override]`: applies to build scripts, proc-macros and
    /// everything only they depend on.
    #[serde(default)]
    pub build_override: Option<Box<ProfileOverride>>,
    #[serde(flatten)]
    pub extra: HashMap<String, toml::Value>,
}

/// Target-specific dependencies (e.g. `[target.'cfg(unix)'.dependencies]`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TargetDeps {
    /// Normal dependencies.
    #[serde(default)]
    pub dependencies: HashMap<String, toml::Value>,
    /// Development dependencies.
    #[serde(default)]
    pub dev_dependencies: HashMap<String, toml::Value>,
    /// Build dependencies.
    #[serde(default)]
    pub build_dependencies: HashMap<String, toml::Value>,
    #[serde(flatten)]
    pub extra: HashMap<String, toml::Value>,
}

/// The raw layout of a Cargo.toml file before resolving targets and dependencies.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct RawManifest {
    pub package: Option<PackageManifest>,
    pub workspace: Option<WorkspaceManifest>,
    #[serde(default)]
    pub dependencies: HashMap<String, toml::Value>,
    #[serde(default)]
    pub dev_dependencies: HashMap<String, toml::Value>,
    #[serde(default)]
    pub build_dependencies: HashMap<String, toml::Value>,
    #[serde(default)]
    pub features: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub target: HashMap<String, TargetDeps>,
    #[serde(default)]
    pub profile: Option<toml::Value>,
    #[serde(default)]
    pub patch: HashMap<String, HashMap<String, DependencySpec>>,
    #[serde(default)]
    pub replace: HashMap<String, DependencySpec>,
    #[serde(default)]
    pub lints: Option<toml::Value>,
    pub lib: Option<TargetSpec>,
    #[serde(default)]
    pub bin: Vec<TargetSpec>,
    #[serde(default)]
    pub example: Vec<TargetSpec>,
    #[serde(default)]
    pub test: Vec<TargetSpec>,
    #[serde(default)]
    pub bench: Vec<TargetSpec>,
    #[serde(flatten)]
    pub extra: HashMap<String, toml::Value>,
}

/// The fully parsed Cargo.toml manifest.
#[derive(Debug, Clone)]
pub struct Manifest {
    /// The `[package]` section.
    pub package: Option<PackageManifest>,
    /// Flattened and parsed dependencies.
    pub dependencies: HashMap<String, Dependency>,
    /// Crate features.
    pub features: HashMap<String, Vec<String>>,
    /// The `[workspace]` section.
    pub workspace: Option<WorkspaceManifest>,
    /// Target-specific dependencies.
    pub target: HashMap<String, TargetDeps>,
    /// Profiles.
    pub profile: HashMap<String, ProfileOverride>,
    /// Dependency patches (`[patch.<source>]`).
    pub patch: HashMap<String, HashMap<String, DependencySpec>>,
    /// The deprecated `[replace]` table (`"name:version" = { ... }`).
    pub replace: HashMap<String, DependencySpec>,
    /// The raw `[lints]` table.
    pub lints: Option<toml::Value>,
    /// The `[lib]` target.
    pub lib: Option<TargetSpec>,
    /// The `[[bin]]` targets.
    pub bin: Vec<TargetSpec>,
    /// The `[[example]]` targets.
    pub example: Vec<TargetSpec>,
    /// The `[[test]]` targets.
    pub test: Vec<TargetSpec>,
    /// The `[[bench]]` targets.
    pub bench: Vec<TargetSpec>,
}

impl Manifest {
    /// Reads and parses a manifest from the given file path.
    pub fn from_path(path: &Path) -> Result<Self, ManifestError> {
        let content = fs::read_to_string(path).map_err(|source| ManifestError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let mut manifest = Self::from_str(&content, path)?;

        let parent = path.parent().unwrap_or_else(|| Path::new(""));
        manifest.discover_targets(parent);

        Ok(manifest)
    }

    /// The feature resolver version this manifest asks for: the workspace's
    /// (or root package's) `resolver`, else the one implied by the edition
    /// (`1` before 2021, `2` for 2021, `3` for 2024), as in Cargo.
    pub fn resolver_version(&self) -> u8 {
        let explicit = self
            .workspace
            .as_ref()
            .and_then(|w| w.resolver.as_deref())
            .or_else(|| self.package.as_ref().and_then(|p| p.resolver.as_deref()));
        match explicit {
            Some("1") => 1,
            Some("2") => 2,
            Some("3") => 3,
            _ => match self
                .package
                .as_ref()
                .and_then(|p| p.edition_str())
                .unwrap_or("2015")
            {
                "2024" => 3,
                "2021" => 2,
                _ => 1,
            },
        }
    }

    /// `rustc` flags for this package's `[lints]`, following
    /// `lints.workspace = true` into `[workspace.lints]`.
    pub fn lint_args(&self, ws: Option<&WorkspaceManifest>) -> Vec<String> {
        crate::lints::lint_args(self.lints.as_ref(), ws.and_then(|w| w.lints.as_ref()))
    }

    /// Fills in Cargo's automatic target discovery for `dir`, the directory
    /// holding this manifest.
    ///
    /// Mirrors Cargo: `src/lib.rs`, `src/main.rs`, `src/bin/*.rs`,
    /// `src/bin/*/main.rs`, and `examples/`, `tests/`, `benches/` in the same
    /// two layouts. Explicit `[[bin]]`-style entries are kept and win over an
    /// inferred target of the same name; an explicit entry that omits `path`
    /// gets Cargo's conventional one. Discovery for a kind is skipped when the
    /// package sets `autobins`/`autoexamples`/`autotests`/`autobenches` to
    /// `false`.
    pub fn discover_targets(&mut self, dir: &Path) {
        let Some(pkg) = &self.package else { return };
        let pkg_name = pkg.name.clone();
        let auto = |key: &str| pkg.extra.get(key).and_then(|v| v.as_bool()).unwrap_or(true);
        let autobins = pkg.autobins.unwrap_or(true);
        let autoexamples = auto("autoexamples");
        let autotests = auto("autotests");
        let autobenches = auto("autobenches");
        let autolib = pkg.autolib.unwrap_or(true);

        match &mut self.lib {
            Some(lib) => {
                lib.name.get_or_insert_with(|| pkg_name.replace('-', "_"));
                lib.path.get_or_insert_with(|| PathBuf::from("src/lib.rs"));
            }
            None if autolib && dir.join("src/lib.rs").exists() => {
                self.lib = Some(TargetSpec {
                    name: Some(pkg_name.replace('-', "_")),
                    path: Some(PathBuf::from("src/lib.rs")),
                    ..Default::default()
                });
            }
            None => {}
        }

        // Binaries: `src/main.rs` is named after the package; `src/bin/*` after
        // the file or directory.
        let mut inferred_bins = Vec::new();
        if autobins {
            if dir.join("src/main.rs").exists() {
                inferred_bins.push((pkg_name.clone(), PathBuf::from("src/main.rs")));
            }
            inferred_bins.extend(scan_targets(dir, "src/bin"));
        }
        let explicit = std::mem::take(&mut self.bin);
        self.bin = merge_targets(explicit, inferred_bins, |name| {
            if *name == pkg_name && dir.join("src/main.rs").exists() {
                PathBuf::from("src/main.rs")
            } else if dir.join(format!("src/bin/{name}.rs")).exists() {
                PathBuf::from(format!("src/bin/{name}.rs"))
            } else if dir.join(format!("src/bin/{name}/main.rs")).exists() {
                PathBuf::from(format!("src/bin/{name}/main.rs"))
            } else {
                PathBuf::from("src/main.rs")
            }
        });

        let scan = |dirname: &str, enabled: bool| {
            if enabled {
                scan_targets(dir, dirname)
            } else {
                Vec::new()
            }
        };
        let default_path = |kind: &'static str| {
            move |name: &String| {
                if dir.join(format!("{kind}/{name}.rs")).exists() {
                    PathBuf::from(format!("{kind}/{name}.rs"))
                } else {
                    PathBuf::from(format!("{kind}/{name}/main.rs"))
                }
            }
        };
        self.example = merge_targets(
            std::mem::take(&mut self.example),
            scan("examples", autoexamples),
            default_path("examples"),
        );
        self.test = merge_targets(
            std::mem::take(&mut self.test),
            scan("tests", autotests),
            default_path("tests"),
        );
        self.bench = merge_targets(
            std::mem::take(&mut self.bench),
            scan("benches", autobenches),
            default_path("benches"),
        );
    }

    /// Parses a manifest from a string, using `path` for error messages.
    pub fn from_str(content: &str, path: &Path) -> Result<Self, ManifestError> {
        let raw: RawManifest =
            toml::from_str(content).map_err(|source| ManifestError::TomlParse {
                path: path.to_path_buf(),
                source,
            })?;

        let mut parsed_deps = HashMap::new();

        // The map key exists only to give every distinct declaration a
        // unique slot; it is never meant to be read back as the dependency's
        // name (that's `Dependency::name`, set from `name` below). It must
        // include `kind`, not just `target`: a crate frequently declares the
        // same name under both `[dependencies]` and `[dev-dependencies]`
        // (e.g. a normal optional dependency also pinned as a plain
        // dev-dependency for its own tests, as `serde_spanned` does with
        // `serde`) — keying by name alone means the second section parsed
        // silently overwrites the first's entry in this map, and any caller
        // that filters by kind afterward sees only whichever kind happened
        // to be parsed last, which can make a real, active dependency
        // disappear from resolution entirely.
        //
        // A dependency that fails to parse is a hard error, exactly as in
        // Cargo. Silently skipping it (as this used to) made a typo or an
        // unsupported spelling turn into "the dependency just isn't there",
        // which then surfaces much later as an unresolved-import error in
        // the compiler with no pointer back to the real cause.
        let mut parse_deps = |deps: &HashMap<String, toml::Value>,
                              kind: DependencyKind,
                              target: Option<&str>,
                              section: &str|
         -> Result<(), ManifestError> {
            let kind_tag = match kind {
                DependencyKind::Normal => "normal",
                DependencyKind::Dev => "dev",
                DependencyKind::Build => "build",
            };
            for (name, val) in deps {
                let key = match target {
                    Some(t) => format!("{kind_tag}:{t}:{name}"),
                    None => format!("{kind_tag}:{name}"),
                };
                let dep = Dependency::try_from_toml(name, kind, val, target.map(|s| s.to_string()))
                    .map_err(|message| ManifestError::InvalidValue {
                        path: path.to_path_buf(),
                        field: format!("{section}.{name}"),
                        message,
                    })?;
                parsed_deps.insert(key, dep);
            }
            Ok(())
        };

        parse_deps(
            &raw.dependencies,
            DependencyKind::Normal,
            None,
            "dependencies",
        )?;
        parse_deps(
            &raw.dev_dependencies,
            DependencyKind::Dev,
            None,
            "dev-dependencies",
        )?;
        parse_deps(
            &raw.build_dependencies,
            DependencyKind::Build,
            None,
            "build-dependencies",
        )?;

        for (target_name, target_deps) in &raw.target {
            let section = |suffix: &str| format!("target.{target_name}.{suffix}");
            parse_deps(
                &target_deps.dependencies,
                DependencyKind::Normal,
                Some(target_name),
                &section("dependencies"),
            )?;
            parse_deps(
                &target_deps.dev_dependencies,
                DependencyKind::Dev,
                Some(target_name),
                &section("dev-dependencies"),
            )?;
            parse_deps(
                &target_deps.build_dependencies,
                DependencyKind::Build,
                Some(target_name),
                &section("build-dependencies"),
            )?;
        }

        let mut profiles = HashMap::new();
        if let Some(toml::Value::Table(table)) = raw.profile {
            for (k, v) in table {
                if let Ok(po) = v.try_into::<ProfileOverride>() {
                    profiles.insert(k, po);
                }
            }
        }

        Ok(Manifest {
            package: raw.package,
            dependencies: parsed_deps,
            features: raw.features,
            workspace: raw.workspace,
            target: raw.target,
            profile: profiles,
            patch: raw.patch,
            replace: raw.replace,
            lints: raw.lints,
            lib: raw.lib,
            bin: raw.bin,
            example: raw.example,
            test: raw.test,
            bench: raw.bench,
        })
    }
}

/// Finds `dir/<sub>/*.rs` (named by file stem) and `dir/<sub>/*/main.rs`
/// (named by directory), sorted by name so discovery is deterministic.
fn scan_targets(dir: &Path, sub: &str) -> Vec<(String, PathBuf)> {
    let Ok(entries) = fs::read_dir(dir.join(sub)) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if path.is_dir() {
            if path.join("main.rs").exists() {
                found.push((
                    file_name.to_string(),
                    PathBuf::from(sub).join(file_name).join("main.rs"),
                ));
            }
        } else if let Some(stem) = file_name.strip_suffix(".rs") {
            found.push((stem.to_string(), PathBuf::from(sub).join(file_name)));
        }
    }
    found.sort();
    found
}

/// Combines explicit targets with inferred ones: an explicit target replaces
/// an inferred one of the same name and has its missing `name`/`path` filled.
fn merge_targets(
    explicit: Vec<TargetSpec>,
    inferred: Vec<(String, PathBuf)>,
    default_path: impl Fn(&String) -> PathBuf,
) -> Vec<TargetSpec> {
    let mut out: Vec<TargetSpec> = Vec::new();
    for mut t in explicit {
        let Some(name) = t.name.clone().or_else(|| {
            t.path
                .as_ref()
                .and_then(|p| p.file_stem())
                .map(|s| s.to_string_lossy().into_owned())
        }) else {
            continue;
        };
        t.name = Some(name.clone());
        if t.path.is_none() {
            t.path = Some(default_path(&name));
        }
        out.push(t);
    }
    for (name, path) in inferred {
        // An explicit target that points at the same file *is* the inferred
        // one (e.g. `[[bin]] name = "tool" path = "src/main.rs"`): building it
        // twice under two names would be wrong.
        if out
            .iter()
            .any(|t| t.name.as_deref() == Some(name.as_str()) || t.path.as_ref() == Some(&path))
        {
            continue;
        }
        out.push(TargetSpec {
            name: Some(name),
            path: Some(path),
            ..Default::default()
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_manifest() {
        let toml = r#"
        [package]
        name = "test-pkg"
        version = "0.1.0"
        
        [dependencies]
        serde = "1.0"
        "#;

        let manifest = Manifest::from_str(toml, Path::new("Cargo.toml")).unwrap();
        assert_eq!(manifest.package.unwrap().name, "test-pkg");
        // The map key is an internal disambiguator (kind + target), not the
        // dependency name — assert against `Dependency::name` instead.
        assert!(manifest.dependencies.values().any(|d| d.name == "serde"));
    }

    #[test]
    fn test_same_name_dependency_and_dev_dependency_both_kept() {
        // Regression test: a crate name appearing in both `[dependencies]`
        // and `[dev-dependencies]` must produce two distinct entries, not
        // have one silently overwrite the other.
        let toml = r#"
        [package]
        name = "test-pkg"
        version = "0.1.0"

        [dependencies.serde]
        version = "1.0"
        optional = true

        [dev-dependencies.serde]
        version = "1.0"
        "#;

        let manifest = Manifest::from_str(toml, Path::new("Cargo.toml")).unwrap();
        let serde_deps: Vec<_> = manifest
            .dependencies
            .values()
            .filter(|d| d.name == "serde")
            .collect();
        assert_eq!(serde_deps.len(), 2);
        assert!(
            serde_deps
                .iter()
                .any(|d| d.kind == DependencyKind::Normal && d.spec.optional)
        );
        assert!(serde_deps.iter().any(|d| d.kind == DependencyKind::Dev));
    }

    /// Cargo 1.94+ accepts TOML 1.1: inline tables may span lines, carry
    /// comments, and end in a trailing comma. Real crates (and real
    /// `Cargo.toml` files written by formatters like `taplo`) use all of it.
    #[test]
    fn test_multiline_inline_tables_and_trailing_commas() {
        let toml = r#"
        [package]
        name = "test-pkg"
        version = "0.1.0"
        authors = [
            "A <a@b.c>",
            "B <b@b.c>",
        ]

        [features]
        default = [
            "one",
            "two", # comment after the last element
        ]
        one = []
        two = ["one",]

        [dependencies]
        itoa = { version = "1", }
        serde = {
            version = "1.0",
            default-features = false,
            features = [
                "derive",
                "std",
            ],   # comment inside the inline table
            optional = true,
        }
        math = { path = "math",
            # a comment on its own line
            features = ["x"] }

        [target.'cfg(unix)'.dependencies]
        libc = {
            version = "0.2",
        }
        "#;

        let manifest = Manifest::from_str(toml, Path::new("Cargo.toml")).unwrap();
        let by_name = |n: &str| {
            manifest
                .dependencies
                .values()
                .find(|d| d.name == n)
                .unwrap_or_else(|| panic!("dependency `{n}` was dropped"))
        };

        assert_eq!(
            by_name("itoa").spec.version.as_ref().unwrap().to_string(),
            "^1"
        );
        let serde = by_name("serde");
        assert!(!serde.spec.default_features);
        assert!(serde.spec.optional);
        assert_eq!(serde.spec.features, vec!["derive", "std"]);
        assert_eq!(by_name("math").spec.features, vec!["x"]);
        assert_eq!(by_name("libc").target.as_deref(), Some("cfg(unix)"));
        assert_eq!(manifest.features["default"], vec!["one", "two"]);
        assert_eq!(manifest.features["two"], vec!["one"]);
    }

    #[test]
    fn test_invalid_dependency_is_a_hard_error() {
        let toml = r#"
        [package]
        name = "test-pkg"
        version = "0.1.0"

        [dependencies]
        broken = { version = "not a version" }
        "#;
        let err = Manifest::from_str(toml, Path::new("Cargo.toml")).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("dependencies.broken"),
            "unhelpful error: {msg}"
        );
    }

    #[test]
    fn test_workspace_dependencies_multiline() {
        let toml = r#"
        [workspace]
        members = [
            "crates/a",
            "crates/b",
        ]

        [workspace.dependencies]
        serde = {
            version = "1",
            features = ["derive"],
        }
        "#;
        let manifest = Manifest::from_str(toml, Path::new("Cargo.toml")).unwrap();
        let ws = manifest.workspace.unwrap();
        assert_eq!(ws.members.len(), 2);
        assert_eq!(ws.dependencies["serde"].features, vec!["derive"]);
    }

    fn write(dir: &Path, rel: &str, content: &str) {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    #[test]
    fn test_target_auto_discovery_matches_cargo_layout() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        write(
            d,
            "Cargo.toml",
            "[package]\nname = \"my-pkg\"\nversion = \"0.1.0\"\n",
        );
        write(d, "src/lib.rs", "");
        write(d, "src/main.rs", "fn main(){}");
        write(d, "src/bin/tool.rs", "fn main(){}");
        write(d, "src/bin/multi/main.rs", "fn main(){}");
        write(d, "src/bin/ignored/helper.rs", "");
        write(d, "examples/demo.rs", "fn main(){}");
        write(d, "tests/it.rs", "");
        write(d, "tests/suite/main.rs", "");
        write(d, "benches/speed.rs", "");

        let m = Manifest::from_path(&d.join("Cargo.toml")).unwrap();
        assert_eq!(m.lib.as_ref().unwrap().name.as_deref(), Some("my_pkg"));
        let bins: Vec<_> = m
            .bin
            .iter()
            .map(|b| (b.name.clone().unwrap(), b.path.clone().unwrap()))
            .collect();
        assert!(
            bins.contains(&("my-pkg".into(), PathBuf::from("src/main.rs"))),
            "{bins:?}"
        );
        assert!(bins.contains(&("tool".into(), PathBuf::from("src/bin/tool.rs"))));
        assert!(bins.contains(&("multi".into(), PathBuf::from("src/bin/multi/main.rs"))));
        assert_eq!(
            bins.len(),
            3,
            "a dir without main.rs is not a binary: {bins:?}"
        );
        assert_eq!(m.example.len(), 1);
        let tests: Vec<_> = m.test.iter().map(|t| t.name.clone().unwrap()).collect();
        assert_eq!(tests, vec!["it", "suite"]);
        assert_eq!(m.bench[0].name.as_deref(), Some("speed"));
    }

    #[test]
    fn test_explicit_target_overrides_inferred_and_gets_default_path() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        write(
            d,
            "Cargo.toml",
            r#"
            [package]
            name = "p"
            version = "0.1.0"
            autotests = false

            [[bin]]
            name = "tool"
            required-features = ["x"]

            [[test]]
            name = "custom"
            path = "elsewhere/t.rs"
            "#,
        );
        write(d, "src/bin/tool.rs", "fn main(){}");
        write(d, "tests/auto.rs", "");

        let m = Manifest::from_path(&d.join("Cargo.toml")).unwrap();
        assert_eq!(m.bin.len(), 1);
        assert_eq!(m.bin[0].path.as_deref(), Some(Path::new("src/bin/tool.rs")));
        assert_eq!(m.bin[0].required_features, vec!["x"]);
        assert_eq!(m.test.len(), 1, "autotests = false disables discovery");
        assert_eq!(m.test[0].name.as_deref(), Some("custom"));
    }

    #[test]
    fn test_explicit_target_on_the_same_path_replaces_the_inferred_one() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        write(
            d,
            "Cargo.toml",
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n[[bin]]\nname = \"renamed\"\npath = \"src/main.rs\"\n",
        );
        write(d, "src/main.rs", "fn main(){}");
        let m = Manifest::from_path(&d.join("Cargo.toml")).unwrap();
        let names: Vec<_> = m.bin.iter().map(|b| b.name.clone().unwrap()).collect();
        assert_eq!(
            names,
            ["renamed"],
            "no second `app` binary from the same file"
        );
    }

    #[test]
    fn crate_type_autolib_replace_and_resolver_are_understood() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        write(
            d,
            "Cargo.toml",
            r#"
            [package]
            name = "p"
            version = "0.1.0"
            edition = "2021"
            autolib = false

            [lib]
            path = "src/x.rs"
            crate-type = ["cdylib", "rlib"]

            [replace]
            "foo:1.0.0" = { git = "https://example.com/foo" }
            "#,
        );
        write(d, "src/lib.rs", "");
        write(d, "src/x.rs", "");
        let m = Manifest::from_path(&d.join("Cargo.toml")).unwrap();
        let lib = m.lib.as_ref().unwrap();
        assert_eq!(lib.crate_types, ["cdylib", "rlib"]);
        assert!(m.replace.contains_key("foo:1.0.0"));
        assert_eq!(m.resolver_version(), 2, "edition 2021 implies resolver 2");

        // `autolib = false` really disables the implicit library.
        write(
            d,
            "Cargo.toml",
            "[package]\nname = \"p\"\nversion = \"0.1.0\"\nautolib = false\n",
        );
        let m = Manifest::from_path(&d.join("Cargo.toml")).unwrap();
        assert!(m.lib.is_none());
        assert_eq!(m.resolver_version(), 1);

        let ws = Manifest::from_str(
            "[workspace]\nmembers = []\nresolver = \"3\"\n",
            Path::new("Cargo.toml"),
        )
        .unwrap();
        assert_eq!(ws.resolver_version(), 3);
    }

    #[test]
    fn workspace_inheritance_covers_homepage_readme_and_license_file() {
        let ws = Manifest::from_str(
            "[workspace]\nmembers = []\n[workspace.package]\nhomepage = \"https://h\"\nreadme = \"R.md\"\nlicense-file = \"L\"\n",
            Path::new("Cargo.toml"),
        )
        .unwrap()
        .workspace
        .unwrap();
        let pkg = Manifest::from_str(
            "[package]\nname = \"m\"\nhomepage.workspace = true\nreadme.workspace = true\nlicense-file.workspace = true\n",
            Path::new("Cargo.toml"),
        )
        .unwrap()
        .package
        .unwrap();
        let meta = pkg.meta_in(Some(&ws));
        assert_eq!(meta.homepage, "https://h");
        assert_eq!(meta.readme, "R.md");
        assert_eq!(meta.license_file, "L");
    }

    #[test]
    fn test_workspace_inheritance_helpers() {
        let toml = r#"
        [workspace]
        members = []
        [workspace.package]
        version = "2.3.4"
        edition = "2024"
        description = "shared"
        authors = ["A", "B"]
        "#;
        let ws = Manifest::from_str(toml, Path::new("Cargo.toml"))
            .unwrap()
            .workspace
            .unwrap();
        let member = r#"
        [package]
        name = "m"
        version.workspace = true
        edition.workspace = true
        description.workspace = true
        authors.workspace = true
        "#;
        let pkg = Manifest::from_str(member, Path::new("Cargo.toml"))
            .unwrap()
            .package
            .unwrap();
        assert_eq!(pkg.version_in(Some(&ws)).unwrap().to_string(), "2.3.4");
        assert_eq!(pkg.edition_in(Some(&ws)), "2024");
        let meta = pkg.meta_in(Some(&ws));
        assert_eq!(meta.description, "shared");
        assert_eq!(meta.authors, "A:B");
        assert_eq!(pkg.edition_in(None), "2015");
    }
}
