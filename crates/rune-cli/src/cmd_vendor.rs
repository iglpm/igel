//! `rune vendor`: every registry and git dependency, unpacked into one
//! directory, with the configuration that makes builds (Rune's and Cargo's)
//! read them from there — offline builds, audits, air-gapped CI.
//!
//! Each package gets a `.cargo-checksum.json` (`files`: SHA-256 of every file;
//! `package`: the registry checksum, `null` for git), which is what Cargo
//! verifies a vendored crate against.

use crate::args::VendorArgs;
use crate::graph;
use crate::ui;
use crate::workspace::Workspace;
use anyhow::{Context, Result, bail};
use rune_resolver::{LockfileManager, PackageSource};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

pub fn vendor(args: VendorArgs) -> Result<()> {
    let ws = Workspace::load(args.manifest.manifest_path.as_deref())?;
    let offline = args.manifest.offline;
    let existing = LockfileManager::load_fast(ws.root());
    let (resolved, lock) = graph::resolve_fresh(&ws, offline, existing.as_ref(), false)?;
    graph::store_resolution(&ws, &lock)?;

    let dest = if args.path.is_absolute() {
        args.path.clone()
    } else {
        ws.root().join(&args.path)
    };
    // Never vendor into something that is not ours to overwrite.
    if dest == ws.root() || ws.members.iter().any(|m| m.dir.starts_with(&dest)) {
        bail!(
            "refusing to vendor into `{}`: it contains the workspace",
            dest.display()
        );
    }

    // Registry packages, all downloaded at once.
    let registry: Vec<(String, String, Option<String>, String)> = resolved
        .packages
        .values()
        .filter_map(|p| match &p.source {
            PackageSource::Registry(index) => Some((
                p.name.clone(),
                p.version.to_string(),
                p.checksum.clone(),
                index.clone(),
            )),
            _ => None,
        })
        .collect();
    let dirs = rune_registry::download_and_extract_grouped(&registry, offline)
        .context("failed to download the registry packages")?;

    let mut packages: Vec<Vendored> = registry
        .iter()
        .zip(dirs)
        .map(|((n, v, c, _), d)| (n.clone(), v.clone(), d, c.clone(), None))
        .collect();
    let mut git_sources: BTreeMap<String, GitSource> = BTreeMap::new();
    for p in resolved.packages.values() {
        if let PackageSource::Git {
            url,
            reference,
            commit: _,
        } = &p.source
        {
            let r = rune_registry::git::GitRef::from_query(reference.as_deref());
            let checkout = rune_registry::git::checkout(url, &r)
                .with_context(|| format!("failed to check out `{url}`"))?;
            let dir = rune_registry::git::find_package(&checkout, &p.name)?;
            let source = GitSource {
                url: url.clone(),
                query: reference.clone(),
            };
            git_sources.insert(source.key(), source.clone());
            packages.push((
                p.name.clone(),
                p.version.to_string(),
                dir,
                None,
                Some(source),
            ));
        }
    }
    packages.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));

    let mut count: HashMap<&str, usize> = HashMap::new();
    for (name, ..) in &packages {
        *count.entry(name).or_default() += 1;
    }
    if !args.no_delete {
        let _ = std::fs::remove_dir_all(&dest);
    }
    std::fs::create_dir_all(&dest)?;
    for (name, version, src, cksum, git) in &packages {
        let dir_name = if args.versioned_dirs || count[name.as_str()] > 1 {
            format!("{name}-{version}")
        } else {
            name.clone()
        };
        let out = dest.join(&dir_name);
        let _ = make_writable_and_remove(&out);
        let mut files = BTreeMap::new();
        copy_package(src, &out, Path::new(""), &mut files)
            .with_context(|| format!("cannot vendor {name} v{version}"))?;
        let checksum = serde_json::json!({
            "files": files,
            "package": if git.is_some() { None } else { cksum.clone() },
        });
        std::fs::write(
            out.join(".cargo-checksum.json"),
            serde_json::to_string(&checksum)?,
        )?;
    }
    ui::action(
        "vendored",
        format!("{} packages into {}", packages.len(), dest.display()),
    );

    // What to put in `.cargo/config.toml`.
    let shown = pathdiff(&dest, ws.root());
    let mut config = String::from("[source.crates-io]\nreplace-with = \"vendored-sources\"\n");
    for source in git_sources.values() {
        config.push_str(&format!(
            "\n[source.\"{}\"]\ngit = \"{}\"\n",
            source.key(),
            source.url
        ));
        if let Some((k, v)) = source.query.as_deref().and_then(|q| q.split_once('=')) {
            config.push_str(&format!("{k} = \"{v}\"\n"));
        }
        config.push_str("replace-with = \"vendored-sources\"\n");
    }
    config.push_str(&format!(
        "\n[source.vendored-sources]\ndirectory = \"{shown}\"\n"
    ));
    eprintln!("To use the vendored sources, add this to .cargo/config.toml:\n");
    println!("{config}");
    Ok(())
}

/// `(name, version, source dir, registry checksum, git source)`.
type Vendored = (String, String, PathBuf, Option<String>, Option<GitSource>);

#[derive(Clone)]
struct GitSource {
    url: String,
    query: Option<String>,
}

impl GitSource {
    fn key(&self) -> String {
        match &self.query {
            Some(q) => format!("git+{}?{q}", self.url),
            None => format!("git+{}", self.url),
        }
    }
}

/// Copies a package, recording each file's SHA-256 under its relative path.
/// VCS files are left out: they would confuse the project's own repository.
fn copy_package(
    src: &Path,
    dest: &Path,
    rel: &Path,
    files: &mut BTreeMap<String, String>,
) -> Result<()> {
    std::fs::create_dir_all(dest.join(rel))?;
    for entry in std::fs::read_dir(src.join(rel))? {
        let entry = entry?;
        let name = entry.file_name();
        if matches!(
            name.to_str(),
            Some(".git" | ".gitignore" | ".gitattributes" | ".cargo-ok" | ".cargo-checksum.json")
        ) {
            continue;
        }
        let rel_path = rel.join(&name);
        let ty = entry.file_type()?;
        if ty.is_dir() {
            if rel.as_os_str().is_empty() && matches!(name.to_str(), Some("target" | ".rune")) {
                continue;
            }
            copy_package(src, dest, &rel_path, files)?;
        } else if ty.is_file() {
            let to = dest.join(&rel_path);
            std::fs::copy(entry.path(), &to)?;
            let mut perms = std::fs::metadata(&to)?.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            std::fs::set_permissions(&to, perms)?;
            files.insert(
                rel_path.to_string_lossy().replace('\\', "/"),
                rune_registry::download::sha256_file(&to)?,
            );
        }
    }
    Ok(())
}

fn make_writable_and_remove(dir: &Path) -> std::io::Result<()> {
    if dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    Ok(())
}

/// `path` relative to `base` when it is inside it, else as is.
fn pathdiff(path: &Path, base: &Path) -> String {
    path.strip_prefix(base)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}
