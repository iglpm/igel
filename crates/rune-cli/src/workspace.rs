//! Discovery and loading of the Cargo workspace a command operates on.

use anyhow::{Context, Result, bail};
use rune_common::cargo_config::CargoConfig;
use rune_common::toolchain::{self, Toolchain};
use rune_common::{Edition, PackageMeta, RuneConfig, settings::Settings};
use rune_manifest::{Manifest, WorkspaceManifest};
use std::path::{Path, PathBuf};

/// One package of the workspace (or a path dependency outside of it).
#[derive(Debug, Clone)]
pub struct Member {
    pub dir: PathBuf,
    /// The file the manifest was read from (`Cargo.toml` or `Rune.toml`).
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
        let manifest = Manifest::from_path(&manifest_path)
            .with_context(|| format!("Failed to parse {}", manifest_path.display()))?;
        let pkg = manifest
            .package
            .as_ref()
            .with_context(|| format!("{} has no [package] section", manifest_path.display()))?;
        let version = pkg
            .version_in(ws)
            .unwrap_or_else(|| semver::Version::new(0, 0, 0));
        let edition = pkg.edition_in(ws).parse().unwrap_or(Edition::E2015);
        let meta = pkg.meta_in(ws);
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

    /// `name version` — the id used by the resolver and lockfile.
    pub fn id(&self) -> String {
        format!("{} {}", self.name, self.version)
    }
}

/// A loaded workspace: the root manifest, every member, and user settings.
pub struct Workspace {
    pub config: RuneConfig,
    /// `[patch]` / `[replace]` from the root manifest and configuration.
    pub patches: rune_resolver::Patches,
    /// `.cargo/config.toml` files (and `CARGO_*` overrides) that apply.
    pub cargo_config: CargoConfig,
    pub root_manifest: Manifest,
    /// Every workspace member (a lone package is a one-member workspace),
    /// sorted by directory.
    pub members: Vec<Member>,
    /// Index into `members` of the package the command was started in.
    pub current: Option<usize>,
}

impl Workspace {
    /// Finds and loads the workspace containing `manifest_path`, or the
    /// nearest manifest (`Cargo.toml` or `Rune.toml`) at or above the current directory.
    pub fn load(manifest_path: Option<&Path>) -> Result<Self> {
        let start_manifest = match manifest_path {
            Some(p) => {
                let p = if p.is_dir() {
                    rune_common::manifest_file::manifest_in(p)
                } else {
                    p.to_path_buf()
                };
                p.canonicalize()
                    .with_context(|| format!("manifest path `{}` does not exist", p.display()))?
            }
            None => {
                let cwd = std::env::current_dir()?;
                find_manifest_upwards(&cwd).with_context(|| {
                    format!(
                        "could not find `Cargo.toml` or `Rune.toml` in `{}` or any parent directory",
                        cwd.display()
                    )
                })?
            }
        };
        let start_dir = start_manifest.parent().unwrap().to_path_buf();

        let start = Manifest::from_path(&start_manifest)
            .with_context(|| format!("Failed to parse {}", start_manifest.display()))?;

        let root_dir = if start.workspace.is_some() {
            start_dir.clone()
        } else {
            find_workspace_root(&start_dir, &start).unwrap_or_else(|| start_dir.clone())
        };

        let root_manifest = if root_dir == start_dir {
            start
        } else {
            let p = rune_common::manifest_file::manifest_in(&root_dir);
            Manifest::from_path(&p).with_context(|| format!("Failed to parse {}", p.display()))?
        };

        let settings = Settings::load(Some(&root_dir))?;
        let config = RuneConfig::new(&root_dir).with_settings(settings);

        // Configuration files are found from the working directory upward,
        // as Cargo does. The compiler (`RUSTC`, wrappers, `rust-toolchain`)
        // is fixed for the whole process from here on.
        let cwd = std::env::current_dir().unwrap_or_else(|_| root_dir.clone());
        let cargo_config = CargoConfig::load(&cwd);
        toolchain::set_global(Toolchain::discover(&cargo_config, &root_dir));
        // Alternative registries, mirrors and vendored sources.
        rune_registry::source::configure(rune_registry::source::RegistryDefs::from_config(
            cargo_config.root(),
        ));

        // Root package first (if any), then members; deduplicated by directory.
        let ws_manifest = root_manifest.workspace.as_ref();
        let mut dirs: Vec<PathBuf> = Vec::new();
        if root_manifest.package.is_some() {
            dirs.push(root_dir.clone());
        }
        if let Some(ws) = ws_manifest {
            for d in ws.resolve_members(&root_dir).unwrap_or_default() {
                let d = d.canonicalize().unwrap_or(d);
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

    /// Finds a member by package name.
    pub fn member_named(&self, name: &str) -> Option<usize> {
        self.members.iter().position(|m| m.name == name)
    }

    /// Selects packages exactly as Cargo does: `-p` wins, then `--workspace`,
    /// then the package the command was run in, and for a virtual workspace
    /// `default-members` or else every member.
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
                    let d = d.canonicalize().unwrap_or(d);
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
            return candidate.canonicalize().ok();
        }
        if !cur.pop() {
            return None;
        }
    }
}

/// Walks up from a member looking for the `[workspace]` that contains it.
fn find_workspace_root(member_dir: &Path, member: &Manifest) -> Option<PathBuf> {
    // An explicit `package.workspace = "../.."` wins.
    if let Some(explicit) = member.package.as_ref().and_then(|p| p.workspace.as_ref()) {
        let dir = member_dir.join(explicit);
        return dir.canonicalize().ok();
    }
    let mut cur = member_dir.parent()?.to_path_buf();
    loop {
        if let Some(candidate) = rune_common::manifest_file::find_manifest(&cur) {
            if let Ok(m) = Manifest::from_path(&candidate) {
                if let Some(ws) = &m.workspace {
                    let members = ws.resolve_members(&cur).unwrap_or_default();
                    let is_member = members
                        .iter()
                        .any(|d| d.canonicalize().unwrap_or_else(|_| d.clone()) == member_dir);
                    if is_member {
                        return Some(cur);
                    }
                }
            }
        }
        if !cur.pop() {
            return None;
        }
    }
}

/// Finds the directory a project's `Rune.toml` belongs in: the workspace root
/// if there is one, else the nearest package. Cheaper and more forgiving than
/// [`Workspace::load`] (no member parsing), for commands like `rune config`.
pub fn find_project_root(manifest_path: Option<&Path>) -> Option<PathBuf> {
    let nearest = match manifest_path {
        Some(p) => p.canonicalize().ok()?,
        None => find_manifest_upwards(&std::env::current_dir().ok()?)?,
    };
    let dir = nearest.parent()?.to_path_buf();
    let manifest = Manifest::from_path(&nearest).ok()?;
    if manifest.workspace.is_some() {
        return Some(dir);
    }
    Some(find_workspace_root(&dir, &manifest).unwrap_or(dir))
}
