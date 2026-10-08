//! Discovery and loading of the workspace a command operates on.

use anyhow::{Context, Result, bail};
use rune_common::cargo_config::CargoConfig;
use rune_common::toolchain::{self, Toolchain};
use rune_common::{Edition, PackageMeta, RuneConfig, settings::Settings};
use rune_manifest::{Manifest, WorkspaceManifest};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Member {
    pub dir: PathBuf,
    pub manifest_path: PathBuf,
    pub manifest: Manifest,
    pub name: String,
    pub version: semver::Version,
    pub edition: Edition,
    pub meta: PackageMeta,
}

impl Member {
    pub(crate) fn load(dir: &Path, ws: Option<&WorkspaceManifest>) -> Result<Self> {
        let manifest_path = rune_common::manifest_file::manifest_in(dir);
        let manifest = read_manifest(&manifest_path)?;
        let pkg = manifest
            .package
            .as_ref()
            .with_context(|| format!("{} has no [package] section", manifest_path.display()))?;
        if let Some(v) = pkg.version_str()
            && let Err(e) = v.parse::<semver::Version>()
        {
            bail!(
                "invalid `version = \"{v}\"` in {}: {e} (versions look like `1.2.3`)",
                manifest_path.display()
            );
        }
        let version = pkg
            .version_in(ws)
            .unwrap_or_else(|| semver::Version::new(0, 0, 0));
        let edition_text = pkg.edition_in(ws);
        let edition: Edition = edition_text.parse().map_err(|_| {
            anyhow::anyhow!(
                "unknown `edition = \"{edition_text}\"` in {}: the editions are 2015, 2018, \
                 2021 and 2024",
                manifest_path.display()
            )
        })?;
        let meta = pkg.meta_in(ws);
        validate(&manifest, &manifest_path)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            manifest_path,
            name: pkg.name.clone(),
            version,
            edition,
            meta,
            manifest,
        })
    }

    pub fn id(&self) -> String {
        format!("{} {}", self.name, self.version)
    }
}

fn validate(manifest: &Manifest, path: &Path) -> Result<()> {
    if manifest.lib.is_none() && manifest.bin.is_empty() {
        bail!(
            "failed to parse manifest at `{}`: no targets specified in the manifest; either \
             src/lib.rs, src/main.rs, a [lib] section, or [[bin]] section must be present",
            path.display()
        );
    }
    let mut deps: std::collections::HashMap<&str, bool> = std::collections::HashMap::new();
    for d in manifest.dependencies.values() {
        let optional = deps.entry(d.name.as_str()).or_insert(false);
        *optional |= d.spec.optional;
    }
    for t in manifest.target.values() {
        for (name, v) in t
            .dependencies
            .iter()
            .chain(&t.dev_dependencies)
            .chain(&t.build_dependencies)
        {
            let opt = v.get("optional").and_then(|o| o.as_bool()).unwrap_or(false);
            *deps.entry(name.as_str()).or_insert(false) |= opt;
        }
    }
    let mut names: Vec<&String> = manifest.features.keys().collect();
    names.sort();
    for feature in names {
        for entry in &manifest.features[feature] {
            let problem = if let Some(dep) = entry.strip_prefix("dep:") {
                match deps.get(dep) {
                    Some(true) => None,
                    Some(false) => Some(format!(
                        "includes `{entry}`, but `{dep}` is not an optional dependency"
                    )),
                    None => Some(format!(
                        "includes `{entry}`, but `{dep}` is not listed as a dependency"
                    )),
                }
            } else if let Some((dep, _)) = entry.split_once('/') {
                let dep = dep.trim_end_matches('?');
                (!deps.contains_key(dep))
                    .then(|| format!("includes `{entry}`, but `{dep}` is not a dependency"))
            } else {
                (!manifest.features.contains_key(entry.as_str())
                    && deps.get(entry.as_str()) != Some(&true))
                .then(|| {
                    format!("includes `{entry}` which is neither a dependency nor another feature")
                })
            };
            if let Some(p) = problem {
                bail!(
                    "failed to parse manifest at `{}`: feature `{feature}` {p}",
                    path.display()
                );
            }
        }
    }
    Ok(())
}

pub struct Workspace {
    pub config: RuneConfig,
    pub patches: rune_resolver::Patches,
    pub cargo_config: CargoConfig,
    pub root_manifest: Manifest,
    pub members: Vec<Member>,
    pub current: Option<usize>,
}

impl Workspace {
    pub fn load(manifest_path: Option<&Path>) -> Result<Self> {
        let start_manifest = match manifest_path {
            Some(p) => {
                let p = if p.is_dir() {
                    rune_common::manifest_file::manifest_in(p)
                } else {
                    p.to_path_buf()
                };
                rune_common::fsutil::canonicalize(&p)
                    .with_context(|| format!("manifest path `{}` does not exist", p.display()))?
            }
            None => {
                let cwd = std::env::current_dir()?;
                find_manifest_upwards(&cwd).with_context(|| {
                    format!(
                        "could not find `Cargo.toml` or `Rune.toml` in `{}` or any parent directory \
                         (`rune new <name>` creates a package, `rune init` makes this directory one)",
                        cwd.display()
                    )
                })?
            }
        };
        let start_dir = start_manifest.parent().unwrap().to_path_buf();

        let start = read_manifest(&start_manifest)?;

        let root_dir = if start.workspace.is_some() {
            start_dir.clone()
        } else {
            find_workspace_root(&start_dir, &start).unwrap_or_else(|| start_dir.clone())
        };

        let mut root_manifest = if root_dir == start_dir {
            start
        } else {
            let p = rune_common::manifest_file::manifest_in(&root_dir);
            read_manifest(&p)?
        };

        let settings = Settings::load(Some(&root_dir))?;
        let config = RuneConfig::new(&root_dir).with_settings(settings);

        let cwd = std::env::current_dir().unwrap_or_else(|_| root_dir.clone());
        let cargo_config = CargoConfig::load(&cwd);
        if let Some(e) = cargo_config.error() {
            anyhow::bail!(e);
        }
        apply_profile_config(&mut root_manifest.profile, &cargo_config, std::env::vars())?;
        rune_common::net::configure(rune_common::net::HttpConfig::from_config(&cargo_config));
        toolchain::set_global(Toolchain::discover(&cargo_config, &root_dir));
        rune_registry::source::configure(rune_registry::source::RegistryDefs::from_config(
            cargo_config.root(),
        ));

        let ws_manifest = root_manifest.workspace.as_ref();
        let mut dirs: Vec<PathBuf> = Vec::new();
        if root_manifest.package.is_some() {
            dirs.push(root_dir.clone());
        }
        if let Some(ws) = ws_manifest {
            for d in ws.resolve_members(&root_dir).unwrap_or_default() {
                let d = rune_common::fsutil::canonicalize(&d).unwrap_or(d);
                if !dirs.contains(&d) {
                    dirs.push(d);
                }
            }
        }
        if dirs.is_empty() {
            bail!(
                "`{}` is a virtual workspace with no members",
                rune_common::manifest_file::manifest_in(&root_dir).display()
            );
        }

        let mut members = Vec::with_capacity(dirs.len());
        for d in &dirs {
            members.push(Member::load(d, ws_manifest)?);
        }
        members.sort_by(|a, b| a.dir.cmp(&b.dir));
        for (i, a) in members.iter().enumerate() {
            if let Some(b) = members[i + 1..].iter().find(|b| b.name == a.name) {
                bail!(
                    "two packages named `{}` in this workspace:\n- {}\n- {}",
                    a.name,
                    a.manifest_path.display(),
                    b.manifest_path.display()
                );
            }
        }

        let current = members.iter().position(|m| m.dir == start_dir);
        let patches = rune_resolver::Patches::collect(
            &root_dir,
            &root_manifest,
            cargo_config.table(&["patch"]),
        );
        Ok(Self {
            config,
            patches,
            cargo_config,
            root_manifest,
            members,
            current,
        })
    }

    pub fn root(&self) -> &Path {
        &self.config.project_root
    }

    pub fn manifest_path(&self) -> PathBuf {
        rune_common::manifest_file::manifest_in(&self.config.project_root)
    }

    pub fn workspace_manifest(&self) -> Option<&WorkspaceManifest> {
        self.root_manifest.workspace.as_ref()
    }

    pub fn member_named(&self, name: &str) -> Option<usize> {
        self.members.iter().position(|m| m.name == name)
    }

    /// Selects packages as Cargo does: `-p`, then `--workspace`, then the current package, then
    /// `default-members` or every member.
    pub fn select(
        &self,
        packages: &[String],
        workspace: bool,
        exclude: &[String],
    ) -> Result<Vec<usize>> {
        let mut chosen: Vec<usize> = if !packages.is_empty() {
            let mut out = Vec::new();
            for spec in packages {
                let name = spec.split('@').next().unwrap_or(spec);
                match self.member_named(name) {
                    Some(i) => out.push(i),
                    None => {
                        let known: Vec<_> = self.members.iter().map(|m| m.name.as_str()).collect();
                        if let Some(near) = crate::cmd_deps::closest(name, known.iter().copied()) {
                            bail!(
                                "package ID specification `{spec}` did not match any packages in \
                                 this workspace; did you mean `{near}`?"
                            );
                        }
                        bail!(
                            "package ID specification `{spec}` did not match any packages in this \
                             workspace (members: {})",
                            known.join(", ")
                        );
                    }
                }
            }
            out
        } else if workspace {
            (0..self.members.len()).collect()
        } else if let Some(cur) = self.current {
            vec![cur]
        } else {
            let defaults: Vec<usize> = self
                .workspace_manifest()
                .map(|w| w.resolve_default_members(self.root()).unwrap_or_default())
                .unwrap_or_default()
                .into_iter()
                .filter_map(|d| {
                    let d = rune_common::fsutil::canonicalize(&d).unwrap_or(d);
                    self.members.iter().position(|m| m.dir == d)
                })
                .collect();
            if defaults.is_empty() {
                (0..self.members.len()).collect()
            } else {
                defaults
            }
        };

        for spec in exclude {
            chosen.retain(|&i| self.members[i].name != *spec);
        }
        chosen.sort_unstable();
        chosen.dedup();
        if chosen.is_empty() {
            bail!("no packages selected");
        }
        Ok(chosen)
    }
}

fn find_manifest_upwards(start: &Path) -> Option<PathBuf> {
    let mut cur = start.to_path_buf();
    loop {
        if let Some(candidate) = rune_common::manifest_file::find_manifest(&cur) {
            return rune_common::fsutil::canonicalize(&candidate).ok();
        }
        if !cur.pop() {
            return None;
        }
    }
}

fn find_workspace_root(member_dir: &Path, member: &Manifest) -> Option<PathBuf> {
    if let Some(explicit) = member.package.as_ref().and_then(|p| p.workspace.as_ref()) {
        let dir = member_dir.join(explicit);
        return rune_common::fsutil::canonicalize(&dir).ok();
    }
    let mut cur = member_dir.parent()?.to_path_buf();
    loop {
        if let Some(candidate) = rune_common::manifest_file::find_manifest(&cur)
            && let Ok(m) = Manifest::from_path(&candidate)
            && let Some(ws) = &m.workspace
        {
            let members = ws.resolve_members(&cur).unwrap_or_default();
            let is_member = members.iter().any(|d| {
                rune_common::fsutil::canonicalize(d).unwrap_or_else(|_| d.clone()) == member_dir
            });
            if is_member {
                return Some(cur);
            }
        }
        if !cur.pop() {
            return None;
        }
    }
}

pub fn find_project_root(manifest_path: Option<&Path>) -> Option<PathBuf> {
    let nearest = match manifest_path {
        Some(p) => rune_common::fsutil::canonicalize(p).ok()?,
        None => find_manifest_upwards(&std::env::current_dir().ok()?)?,
    };
    let dir = nearest.parent()?.to_path_buf();
    let manifest = Manifest::from_path(&nearest).ok()?;
    if manifest.workspace.is_some() {
        return Some(dir);
    }
    Some(find_workspace_root(&dir, &manifest).unwrap_or(dir))
}

/// `[profile.*]` from config and `CARGO_PROFILE_*` env over the manifest's, key by key, as Cargo does.
fn apply_profile_config(
    profiles: &mut std::collections::HashMap<String, rune_manifest::cargo_toml::ProfileOverride>,
    config: &CargoConfig,
    env: impl Iterator<Item = (String, String)>,
) -> Result<()> {
    let mut layers: toml::Table = config
        .file_value(&["profile"])
        .and_then(|v| v.as_table())
        .cloned()
        .unwrap_or_default();

    let mut names: Vec<String> = ["dev", "release", "test", "bench"]
        .iter()
        .map(|s| s.to_string())
        .chain(profiles.keys().cloned())
        .chain(layers.keys().cloned())
        .collect();
    names.sort_by_key(|n| std::cmp::Reverse(n.len()));
    names.dedup();
    let upper = |s: &str| s.to_ascii_uppercase().replace('-', "_");
    for (var, value) in env {
        let Some(rest) = var.strip_prefix("CARGO_PROFILE_") else {
            continue;
        };
        let Some(name) = names
            .iter()
            .find(|n| rest.starts_with(&format!("{}_", upper(n))))
        else {
            continue;
        };
        let key = &rest[upper(name).len() + 1..];
        let (table_path, key): (Vec<&str>, &str) = match key.strip_prefix("BUILD_OVERRIDE_") {
            Some(k) => (vec!["build-override"], k),
            None => (vec![], key),
        };
        let value = match value.as_str() {
            "true" => toml::Value::Boolean(true),
            "false" => toml::Value::Boolean(false),
            v => v
                .parse::<i64>()
                .map(toml::Value::Integer)
                .unwrap_or_else(|_| toml::Value::String(v.to_string())),
        };
        let mut table = layers
            .entry(name.clone())
            .or_insert_with(|| toml::Value::Table(Default::default()));
        for part in table_path {
            table = table
                .as_table_mut()
                .context("profile configuration is not a table")?
                .entry(part)
                .or_insert_with(|| toml::Value::Table(Default::default()));
        }
        table
            .as_table_mut()
            .context("profile configuration is not a table")?
            .insert(key.to_ascii_lowercase().replace('_', "-"), value);
    }

    for (name, layer) in layers {
        let mut merged = profiles
            .get(&name)
            .map(toml::Value::try_from)
            .transpose()?
            .unwrap_or_else(|| toml::Value::Table(Default::default()));
        deep_merge(&mut merged, layer);
        let parsed: rune_manifest::cargo_toml::ProfileOverride = merged
            .try_into()
            .with_context(|| format!("invalid `[profile.{name}]` in configuration"))?;
        profiles.insert(name, parsed);
    }
    Ok(())
}

fn deep_merge(base: &mut toml::Value, over: toml::Value) {
    match (base, over) {
        (toml::Value::Table(b), toml::Value::Table(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(existing) => deep_merge(existing, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (b, o) => *b = o,
    }
}

pub fn read_manifest(path: &Path) -> anyhow::Result<Manifest> {
    Manifest::from_path(path).map_err(|e| {
        if e.names_path() {
            anyhow::Error::new(e)
        } else {
            anyhow::Error::new(e).context(format!("Failed to parse {}", path.display()))
        }
    })
}

#[cfg(test)]
mod profile_config_tests {
    use super::*;

    #[test]
    fn config_and_environment_override_manifest_profiles() {
        let manifest: toml::Table =
            "[release]\nlto = \"thin\"\nopt-level = 2\n[release.package.foo]\nopt-level = 1\n"
                .parse()
                .unwrap();
        let mut profiles: std::collections::HashMap<
            String,
            rune_manifest::cargo_toml::ProfileOverride,
        > = manifest
            .into_iter()
            .map(|(k, v)| (k, v.try_into().unwrap()))
            .collect();
        let mut config = CargoConfig::default();
        config
            .add_str(
                "[profile.release]\nopt-level = 3\n[profile.release.package.foo]\ndebug = true\n",
                std::path::Path::new("/p"),
            )
            .unwrap();
        let env = vec![
            ("CARGO_PROFILE_RELEASE_LTO".to_string(), "false".to_string()),
            (
                "CARGO_PROFILE_DEV_BUILD_OVERRIDE_OPT_LEVEL".to_string(),
                "3".to_string(),
            ),
            (
                "CARGO_PROFILE_RELEASE_CODEGEN_UNITS".to_string(),
                "1".to_string(),
            ),
        ];
        apply_profile_config(&mut profiles, &config, env.into_iter()).unwrap();
        let release = &profiles["release"];
        assert_eq!(release.opt_level, Some(toml::Value::Integer(3)));
        assert_eq!(release.extra.get("lto"), Some(&toml::Value::Boolean(false)));
        assert_eq!(
            release.extra.get("codegen-units"),
            Some(&toml::Value::Integer(1))
        );
        let foo = &release.package["foo"];
        assert_eq!(foo.opt_level, Some(toml::Value::Integer(1)));
        assert_eq!(foo.debug, Some(toml::Value::Boolean(true)));
        let dev_build = profiles["dev"].build_override.as_ref().unwrap();
        assert_eq!(dev_build.opt_level, Some(toml::Value::Integer(3)));
    }
}
