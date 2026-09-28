//! `[patch]` and `[replace]`: substituting a dependency's source.
//!
//! ```toml
//! [patch.crates-io]
//! serde = { path = "../serde" }          # a fork checked out next door
//!
//! [patch."https://github.com/org/repo"]
//! inner = { git = "https://github.com/me/inner" }
//! ```
//!
//! A patch replaces *every* dependency on that package from that source —
//! including those of registry crates deep in the graph — provided the
//! patched package's version satisfies the requirement. Patches come from the
//! workspace root manifest and from configuration (`[patch]` in
//! `.cargo/config.toml`, which wins), and alter the resolved graph and hence
//! every unit key above the patched package.

use rune_manifest::dependency::DependencySpec;
use rune_manifest::{Manifest, WorkspaceManifest};
use semver::{Version, VersionReq};
use std::path::{Path, PathBuf};

/// Where the patched dependency would otherwise come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchSource {
    CratesIo,
    /// An alternative registry, by its configured name or index URL.
    Registry(String),
    /// A git repository URL.
    Git(String),
}

impl PatchSource {
    /// Interprets a `[patch.<key>]` table name.
    pub fn from_key(key: &str) -> PatchSource {
        match key {
            "crates-io"
            | "https://github.com/rust-lang/crates.io-index"
            | "sparse+https://index.crates.io/" => PatchSource::CratesIo,
            k if k.starts_with("sparse+") || k.starts_with("registry+") => {
                PatchSource::Registry(k.to_string())
            }
            k if k.contains("://") || k.contains('@') => PatchSource::Git(k.to_string()),
            k => PatchSource::Registry(k.to_string()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PatchEntry {
    pub source: PatchSource,
    pub name: String,
    /// `path` (absolute) or `git`, as written.
    pub spec: DependencySpec,
    /// `[replace]` targets exactly one version.
    pub exact: Option<Version>,
}

/// Every patch that applies to a build.
#[derive(Debug, Clone, Default)]
pub struct Patches {
    entries: Vec<PatchEntry>,
}

impl Patches {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &[PatchEntry] {
        &self.entries
    }

    /// Collects patches from the workspace root manifest (paths relative to
    /// `root`) and from configuration, which takes precedence.
    pub fn collect(root: &Path, manifest: &Manifest, config: Option<&toml::Table>) -> Patches {
        let mut entries = Vec::new();

        // Configuration first: `find` returns the first match.
        if let Some(cfg) = config {
            for (source, table) in cfg {
                let Some(table) = table.as_table() else {
                    continue;
                };
                for (name, value) in table {
                    if let Ok(spec) = value.clone().try_into::<DependencySpec>() {
                        entries.push(PatchEntry {
                            source: PatchSource::from_key(source),
                            name: name.clone(),
                            spec: absolutize(spec, root),
                            exact: None,
                        });
                    }
                }
            }
        }

        let mut sources: Vec<_> = manifest.patch.iter().collect();
        sources.sort_by(|a, b| a.0.cmp(b.0));
        for (source, table) in sources {
            let mut names: Vec<_> = table.iter().collect();
            names.sort_by(|a, b| a.0.cmp(b.0));
            for (name, spec) in names {
                entries.push(PatchEntry {
                    source: PatchSource::from_key(source),
                    name: spec.package.clone().unwrap_or_else(|| name.clone()),
                    spec: absolutize(spec.clone(), root),
                    exact: None,
                });
            }
        }

        // The deprecated `[replace]` table: `"name:version" = { ... }`.
        let mut replaces: Vec<_> = manifest.replace.iter().collect();
        replaces.sort_by(|a, b| a.0.cmp(b.0));
        for (key, spec) in replaces {
            // `name:version`, or `url#name:version` for other sources.
            let tail = key.rsplit('#').next().unwrap_or(key);
            if let Some((name, version)) = tail.rsplit_once(':')
                && let Ok(v) = version.parse::<Version>()
            {
                entries.push(PatchEntry {
                    source: PatchSource::CratesIo,
                    name: name.to_string(),
                    spec: absolutize(spec.clone(), root),
                    exact: Some(v),
                });
            }
        }
        Patches { entries }
    }

    /// The patch for a dependency on `name` from `source`, if any. It is only
    /// *used* if the patched package's version satisfies `req` (checked by
    /// [`PatchEntry::locate`]).
    pub fn find(&self, name: &str, source: &PatchSource) -> Vec<&PatchEntry> {
        self.entries
            .iter()
            .filter(|e| e.name == name && e.source == *source)
            .collect()
    }
}

fn absolutize(mut spec: DependencySpec, root: &Path) -> DependencySpec {
    if let Some(p) = spec.path.take() {
        spec.path = Some(if p.is_absolute() { p } else { root.join(p) });
    }
    spec
}

impl PatchEntry {
    /// The directory of the patched package, with its version, when it
    /// satisfies `req` (and the entry's exact version, for `[replace]`).
    pub fn locate(&self, req: &VersionReq) -> Result<Option<(PathBuf, Version)>, String> {
        let dir = if let Some(p) = &self.spec.path {
            p.clone()
        } else if let Some(url) = &self.spec.git {
            let reference = rune_registry::git::GitRef::from_spec(
                self.spec.branch.as_deref(),
                self.spec.tag.as_deref(),
                self.spec.rev.as_deref(),
            );
            let co = rune_registry::git::checkout(url, &reference).map_err(|e| e.to_string())?;
            rune_registry::git::find_package(&co, &self.name).map_err(|e| e.to_string())?
        } else {
            return Ok(None);
        };
        let manifest = Manifest::from_path(&rune_common::manifest_file::manifest_in(&dir))
            .map_err(|e| format!("patch `{}`: {e}", self.name))?;
        let canon = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        let enclosing = rune_manifest::find_enclosing_workspace(&canon, &manifest);
        let ws: Option<&WorkspaceManifest> = enclosing.as_ref().map(|(_, w)| w);
        let version = manifest
            .package
            .as_ref()
            .and_then(|p| p.version_in(ws))
            .unwrap_or_else(|| Version::new(0, 0, 0));
        if let Some(exact) = &self.exact
            && *exact != version
        {
            return Ok(None);
        }
        Ok(req.matches(&version).then_some((dir, version)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(toml: &str) -> Manifest {
        Manifest::from_str(toml, Path::new("Cargo.toml")).unwrap()
    }

    #[test]
    fn manifest_patches_become_entries_with_absolute_paths() {
        let m = manifest(
            r#"
            [patch.crates-io]
            serde = { path = "../serde" }
            [patch."https://github.com/org/repo"]
            inner = { git = "https://github.com/me/inner", branch = "fix" }
            [patch.my-registry]
            corp = { path = "corp" }
            [replace]
            "old:1.2.3" = { path = "vendored/old" }
            "#,
        );
        let p = Patches::collect(Path::new("/w"), &m, None);
        let serde = &p.find("serde", &PatchSource::CratesIo)[0];
        assert_eq!(serde.spec.path.as_deref(), Some(Path::new("/w/../serde")));
        assert_eq!(
            p.find(
                "inner",
                &PatchSource::Git("https://github.com/org/repo".into())
            )
            .len(),
            1
        );
        assert_eq!(
            p.find("corp", &PatchSource::Registry("my-registry".into()))
                .len(),
            1
        );
        let old = &p.find("old", &PatchSource::CratesIo)[0];
        assert_eq!(old.exact, Some(Version::new(1, 2, 3)));
        assert!(
            p.find("serde", &PatchSource::Registry("x".into()))
                .is_empty()
        );
    }

    #[test]
    fn configuration_patches_take_precedence() {
        let m = manifest("[patch.crates-io]\nfoo = { path = \"manifest\" }\n");
        let cfg: toml::Table =
            toml::from_str("[crates-io]\nfoo = { path = \"/from/config\" }\n").unwrap();
        let p = Patches::collect(Path::new("/w"), &m, Some(&cfg));
        let found = p.find("foo", &PatchSource::CratesIo);
        assert_eq!(
            found[0].spec.path.as_deref(),
            Some(Path::new("/from/config"))
        );
        assert_eq!(
            found[1].spec.path.as_deref(),
            Some(Path::new("/w/manifest"))
        );
    }

    #[test]
    fn a_patch_is_used_only_if_its_version_satisfies_the_requirement() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"foo\"\nversion = \"1.4.0\"\n",
        )
        .unwrap();
        let entry = PatchEntry {
            source: PatchSource::CratesIo,
            name: "foo".into(),
            spec: DependencySpec {
                path: Some(dir.path().to_path_buf()),
                ..Default::default()
            },
            exact: None,
        };
        let ok = entry.locate(&VersionReq::parse("^1.2").unwrap()).unwrap();
        assert_eq!(ok.unwrap().1, Version::new(1, 4, 0));
        assert!(
            entry
                .locate(&VersionReq::parse("^2").unwrap())
                .unwrap()
                .is_none()
        );
        // `[replace]` targets one exact version.
        let replace = PatchEntry {
            exact: Some(Version::new(1, 3, 0)),
            ..entry
        };
        assert!(
            replace
                .locate(&VersionReq::parse("^1").unwrap())
                .unwrap()
                .is_none()
        );
    }
}
