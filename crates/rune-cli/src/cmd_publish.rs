//! `rune package`, `rune publish` and the other registry commands (`login`,
//! `logout`, `yank`, `owner`, `search`, `info`).
//!
//! A package is what Cargo would upload: the files version control knows
//! (respecting `include`/`exclude`), a normalized `Cargo.toml` with workspace
//! inheritance resolved and path/git dependencies turned into registry ones,
//! the original manifest as `Cargo.toml.orig`, a `Cargo.lock` and
//! `.cargo_vcs_info.json`. It is written to `target/package/NAME-VERSION.crate`
//! and verified by building what it unpacks to.

use crate::args::{InfoArgs, LoginArgs, OwnerArgs, PackageArgs, PublishArgs, SearchArgs, YankArgs};
use crate::session::{Session, drive, ensure_success};
use crate::ui;
use crate::workspace::{Member, Workspace};
use anyhow::{Context, Result, bail};
use colored::Colorize;
use rune_registry::api::RegistryApi;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Cargo's fixed modification time for packaged files, so that a package's
/// bytes depend only on its contents.
const PACKAGE_MTIME: u64 = 1_153_704_088;

// ---------------------------------------------------------------------------
// package / publish
// ---------------------------------------------------------------------------

pub fn package(args: PackageArgs) -> Result<()> {
    let ws = Workspace::load(args.manifest.manifest_path.as_deref())?;
    for member in selected(&ws, &args.package, args.workspace)? {
        let mut pkg = prepare(&ws, member, args.allow_dirty)?;
        if args.list {
            for f in pkg.listing() {
                println!("{f}");
            }
            continue;
        }
        let crate_file = pkg.write(&ws, &args.manifest)?;
        if !args.no_verify {
            verify(&pkg, &ws, args.manifest.offline)?;
        }
        ui::action(
            "packaged",
            format!(
                "{} files, {} ({})",
                pkg.listing().len(),
                crate_file.display(),
                human_size(std::fs::metadata(&crate_file)?.len())
            ),
        );
    }
    Ok(())
}

pub fn publish(args: PublishArgs) -> Result<()> {
    let ws = Workspace::load(args.manifest.manifest_path.as_deref())?;
    let registry = args.registry.clone();
    let defs = rune_registry::source::defs();
    let index = defs.index_for_name(registry.as_deref())?;
    for member in selected(&ws, &args.package, false)? {
        let mut pkg = prepare(&ws, member, args.allow_dirty)?;
        check_publish_allowed(&pkg, registry.as_deref().unwrap_or("crates-io"))?;
        let crate_file = pkg.write(&ws, &args.manifest)?;
        if !args.no_verify {
            verify(&pkg, &ws, args.manifest.offline)?;
        }
        let metadata = pkg.metadata(&index)?;
        if args.dry_run {
            ui::action(
                "ready",
                format!(
                    "{} v{} ({}); not uploaded (--dry-run)",
                    pkg.name,
                    pkg.version,
                    crate_file.display()
                ),
            );
            continue;
        }
        let mut api = RegistryApi::for_index(&index)?;
        if let Some(token) = &args.token {
            api = api.with_token(token);
        }
        ui::action(
            "uploading",
            format!("{} v{} to {}", pkg.name, pkg.version, api.name),
        );
        let bytes = std::fs::read(&crate_file)?;
        for w in api.publish(&metadata, &bytes)? {
            ui::warn(w);
        }
        ui::action("uploaded", format!("{} v{}", pkg.name, pkg.version));
        wait_until_available(&index, &pkg.name, &pkg.version);
    }
    Ok(())
}

/// The packages a command applies to: `-p` names, else `--workspace`, else
/// the package in the current directory (or the only one).
fn selected<'a>(ws: &'a Workspace, names: &[String], all: bool) -> Result<Vec<&'a Member>> {
    if all {
        return Ok(ws.members.iter().collect());
    }
    if !names.is_empty() {
        return names
            .iter()
            .map(|n| {
                ws.members
                    .iter()
                    .find(|m| &m.name == n)
                    .with_context(|| format!("package `{n}` is not a member of the workspace"))
            })
            .collect();
    }
    if let Some(i) = ws.current {
        return Ok(vec![&ws.members[i]]);
    }
    match ws.members.as_slice() {
        [only] => Ok(vec![only]),
        _ => bail!("this is a workspace: name the package with `-p NAME` (or pass --workspace)"),
    }
}

/// A package ready to be written: its files and generated contents.
struct Package {
    name: String,
    version: String,
    dir: PathBuf,
    /// `(path in the package, source file)`.
    files: Vec<(String, PathBuf)>,
    /// `(path in the package, contents)`: `Cargo.toml`, `Cargo.toml.orig`,
    /// `.cargo_vcs_info.json`, `Cargo.lock` (added when written).
    generated: Vec<(String, Vec<u8>)>,
    manifest: toml::Table,
}

impl Package {
    fn listing(&self) -> Vec<String> {
        let mut all: BTreeSet<String> = self.files.iter().map(|(p, _)| p.clone()).collect();
        all.extend(self.generated.iter().map(|(p, _)| p.clone()));
        all.insert("Cargo.lock".into());
        all.into_iter().collect()
    }

    /// Whether the packaged manifest names `name` as a dependency of any kind.
    fn depends_on(&self, name: &str) -> bool {
        let in_table = |t: Option<&toml::Value>| {
            t.and_then(|t| t.as_table()).is_some_and(|t| {
                t.iter().any(|(k, v)| {
                    k == name || v.get("package").and_then(|p| p.as_str()) == Some(name)
                })
            })
        };
        ["dependencies", "dev-dependencies", "build-dependencies"]
            .iter()
            .any(|k| in_table(self.manifest.get(*k)))
            || self
                .manifest
                .get("target")
                .and_then(|t| t.as_table())
                .is_some_and(|targets| {
                    targets.values().any(|t| {
                        ["dependencies", "dev-dependencies", "build-dependencies"]
                            .iter()
                            .any(|k| in_table(t.get(*k)))
                    })
                })
    }

    fn stem(&self) -> String {
        format!("{}-{}", self.name, self.version)
    }

    /// Writes the unpacked package (with its `Cargo.lock`) to
    /// `target/package/NAME-VERSION/` and the `.crate` beside it.
    fn write(&mut self, ws: &Workspace, manifest: &crate::args::ManifestArgs) -> Result<PathBuf> {
        let out = ws.config.target_dir.join("package");
        let dir = out.join(self.stem());
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        for (rel, src) in &self.files {
            let dest = dir.join(rel);
            std::fs::create_dir_all(dest.parent().unwrap())?;
            std::fs::copy(src, &dest).with_context(|| format!("cannot copy {}", src.display()))?;
        }
        for (rel, content) in &self.generated {
            std::fs::write(dir.join(rel), content)?;
        }
        // The lockfile of the package on its own, as a user installing it
        // would resolve it.
        ui::action("locking", format!("{} v{}", self.name, self.version));
        let staged = Workspace::load(Some(&dir.join("Cargo.toml")))?;
        let existing = rune_resolver::LockfileManager::load_fast(ws.root());
        let (_, lock) = match crate::graph::resolve_fresh(
            &staged,
            manifest.offline,
            existing.as_ref(),
            false,
        ) {
            Ok(r) => r,
            Err(e) => {
                // The usual cause: a path dependency on another workspace
                // member that is not on the registry yet.
                let unpublished: Vec<&str> = ws
                    .members
                    .iter()
                    .filter(|m| m.name != self.name && self.depends_on(&m.name))
                    .map(|m| m.name.as_str())
                    .collect();
                let hint = if unpublished.is_empty() {
                    String::new()
                } else {
                    format!(
                        "\n\nit depends on {} from this workspace; the registry must have \
                             the required version first (publish {} before `{}`)",
                        unpublished.join(", "),
                        if unpublished.len() == 1 { "it" } else { "them" },
                        self.name
                    )
                };
                bail!(
                    "could not resolve the dependencies of the packaged `{}`: {}{hint}",
                    self.name,
                    e.root_cause()
                );
            }
        };
        let text = lock.to_cargo_string(4);
        std::fs::write(dir.join("Cargo.lock"), &text)?;
        self.generated
            .push(("Cargo.lock".into(), text.into_bytes()));

        let crate_file = out.join(format!("{}.crate", self.stem()));
        let tmp = out.join(format!(".{}.crate.tmp", self.stem()));
        self.tarball(&tmp)?;
        std::fs::rename(&tmp, &crate_file)?;
        self.dir = dir;
        Ok(crate_file)
    }

    fn tarball(&self, path: &Path) -> Result<()> {
        let file = std::fs::File::create(path)?;
        let gz = flate2::write::GzEncoder::new(file, flate2::Compression::best());
        let mut tar = tar::Builder::new(gz);
        let mut entries: Vec<(String, Vec<u8>, u32)> = Vec::new();
        for (rel, src) in &self.files {
            let bytes = std::fs::read(src)?;
            entries.push((rel.clone(), bytes, mode_of(src)));
        }
        for (rel, content) in &self.generated {
            entries.push((rel.clone(), content.clone(), 0o644));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        for (rel, bytes, mode) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(mode);
            header.set_mtime(PACKAGE_MTIME);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_cksum();
            tar.append_data(
                &mut header,
                format!("{}/{rel}", self.stem()),
                bytes.as_slice(),
            )?;
        }
        tar.into_inner()?.finish()?;
        Ok(())
    }

    /// What the registry's publish endpoint wants to know besides the file.
    fn metadata(&self, publishing_to: &str) -> Result<Value> {
        let package = self.manifest.get("package").and_then(|p| p.as_table());
        let field = |k: &str| package.and_then(|p| p.get(k)).cloned();
        let text = |k: &str| field(k).and_then(|v| v.as_str().map(str::to_string));
        let list = |k: &str| {
            field(k)
                .and_then(|v| v.as_array().cloned())
                .unwrap_or_default()
                .into_iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        };
        let readme_file = text("readme").filter(|r| r != "false");
        let readme = readme_file
            .as_ref()
            .and_then(|r| std::fs::read_to_string(self.dir.join(r)).ok());

        let mut deps = Vec::new();
        let mut add = |table: Option<&toml::Value>, kind: &str, target: Option<&str>| {
            let Some(table) = table.and_then(|t| t.as_table()) else {
                return;
            };
            for (key, spec) in table {
                let (req, t) = match spec {
                    toml::Value::String(v) => (v.clone(), None),
                    toml::Value::Table(t) => (
                        t.get("version")
                            .and_then(|v| v.as_str())
                            .unwrap_or("*")
                            .to_string(),
                        Some(t),
                    ),
                    _ => continue,
                };
                let get = |k: &str| t.and_then(|t| t.get(k));
                let package = get("package").and_then(|v| v.as_str());
                let registry = get("registry-index")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .or_else(|| {
                        let name = get("registry")?.as_str()?;
                        rune_registry::source::defs()
                            .index_for_name(Some(name))
                            .ok()
                    })
                    // Dependencies from the registry being published to
                    // carry no registry; crates.io ones do elsewhere.
                    .or_else(|| {
                        (publishing_to != rune_common::CRATES_IO_INDEX)
                            .then(|| "https://github.com/rust-lang/crates.io-index".to_string())
                    })
                    .filter(|r| r != publishing_to);
                // As Cargo sends it: `1.0` becomes `^1.0`.
                let req = semver::VersionReq::parse(&req)
                    .map(|r| r.to_string())
                    .unwrap_or(req);
                deps.push(json!({
                    "name": package.unwrap_or(key),
                    "version_req": req,
                    "features": get("features")
                        .and_then(|v| v.as_array())
                        .map(|a| a.iter().filter_map(|f| f.as_str()).collect::<Vec<_>>())
                        .unwrap_or_default(),
                    "optional": get("optional").and_then(|v| v.as_bool()).unwrap_or(false),
                    "default_features": get("default-features")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(true),
                    "target": target,
                    "kind": kind,
                    "registry": registry,
                    "explicit_name_in_toml": package.map(|_| key.as_str()),
                }));
            }
        };
        for (key, kind) in [
            ("dependencies", "normal"),
            ("dev-dependencies", "dev"),
            ("build-dependencies", "build"),
        ] {
            add(self.manifest.get(key), kind, None);
        }
        if let Some(targets) = self.manifest.get("target").and_then(|t| t.as_table()) {
            for (target, tables) in targets {
                for (key, kind) in [
                    ("dependencies", "normal"),
                    ("dev-dependencies", "dev"),
                    ("build-dependencies", "build"),
                ] {
                    add(tables.get(key), kind, Some(target));
                }
            }
        }
        let features: serde_json::Map<String, Value> = self
            .manifest
            .get("features")
            .and_then(|f| f.as_table())
            .map(|t| {
                t.iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            json!(
                                v.as_array()
                                    .map(|a| a
                                        .iter()
                                        .filter_map(|x| x.as_str())
                                        .collect::<Vec<_>>())
                                    .unwrap_or_default()
                            ),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(json!({
            "name": self.name,
            "vers": self.version,
            "deps": deps,
            "features": features,
            "authors": list("authors"),
            "description": text("description"),
            "documentation": text("documentation"),
            "homepage": text("homepage"),
            "readme": readme,
            "readme_file": readme_file,
            "keywords": list("keywords"),
            "categories": list("categories"),
            "license": text("license"),
            "license_file": text("license-file"),
            "repository": text("repository"),
            "badges": {},
            "links": text("links"),
            "rust_version": text("rust-version"),
        }))
    }
}

fn mode_of(path: &Path) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0) {
            return 0o755;
        }
    }
    let _ = path;
    0o644
}

/// Collects the package's files and generates its manifests.
fn prepare(ws: &Workspace, member: &Member, allow_dirty: bool) -> Result<Package> {
    let raw_text = std::fs::read_to_string(&member.manifest_path)?;
    let raw: toml::Table = raw_text
        .parse()
        .with_context(|| format!("cannot parse {}", member.manifest_path.display()))?;
    let ws_table: Option<toml::Table> = std::fs::read_to_string(ws.manifest_path())
        .ok()
        .and_then(|t| t.parse::<toml::Table>().ok())
        .and_then(|t| t.get("workspace").and_then(|w| w.as_table()).cloned());

    let (manifest, extra) = normalize(&raw, ws_table.as_ref(), ws.root(), &member.dir)?;
    let package = manifest.get("package").and_then(|p| p.as_table());
    for (key, why) in [
        ("description", "a description"),
        ("license", "a license (or `license-file`)"),
    ] {
        let has = package.is_some_and(|p| {
            p.contains_key(key) || (key == "license" && p.contains_key("license-file"))
        });
        if !has {
            ui::warn(format!(
                "`{}` has no {why}; crates.io requires one",
                member.name
            ));
        }
    }

    let vcs = vcs_state(&member.dir)?;
    let listed = match &vcs {
        Some(v) => {
            if !allow_dirty && !v.dirty.is_empty() {
                bail!(
                    "{} files in the working directory contain changes that were not yet \
                     committed into git:\n\n{}\n\ncommit them, or pass --allow-dirty to package anyway",
                    v.dirty.len(),
                    v.dirty.join("\n")
                );
            }
            v.files.clone()
        }
        None => walk(&member.dir),
    };
    let patterns = |key: &str| -> Vec<glob::Pattern> {
        raw.get("package")
            .and_then(|p| p.get(key))
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .flat_map(gitignore_globs)
                    .filter_map(|g| glob::Pattern::new(&g).ok())
                    .collect()
            })
            .unwrap_or_default()
    };
    let (include, exclude) = (patterns("include"), patterns("exclude"));
    let manifest_name = member
        .manifest_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("Cargo.toml")
        .to_string();
    let nested = nested_packages(&member.dir);
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    for rel in listed {
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        let first = rel_str.split('/').next().unwrap_or("");
        if matches!(first, "target" | ".rune" | ".git")
            || rel_str == manifest_name
            || matches!(
                rel_str.as_str(),
                "Cargo.toml"
                    | "Cargo.toml.orig"
                    | "Cargo.lock"
                    | "Rune.lock.bin"
                    | ".cargo_vcs_info.json"
            )
            || nested.iter().any(|n| rel.starts_with(n))
        {
            continue;
        }
        if !include.is_empty() {
            if !include.iter().any(|p| p.matches(&rel_str)) {
                continue;
            }
        } else if exclude.iter().any(|p| p.matches(&rel_str)) {
            continue;
        }
        let src = member.dir.join(&rel);
        if src.is_file() {
            files.push((rel_str, src));
        }
    }
    for (rel, src) in extra {
        if !files.iter().any(|(r, _)| *r == rel) {
            files.push((rel, src));
        }
    }
    files.sort();

    let name = member.name.clone();
    let version = member.version.to_string();
    let mut generated = vec![
        (
            "Cargo.toml".to_string(),
            format!(
                "# THIS FILE IS AUTOMATICALLY GENERATED BY RUNE (as `cargo package` would)\n\
                 #\n\
                 # When uploading crates to the registry, the original manifest is normalized:\n\
                 # workspace inheritance is resolved and path and git dependencies become\n\
                 # registry dependencies. The original is kept as `Cargo.toml.orig`.\n\n{}",
                toml::to_string(&manifest)?
            )
            .into_bytes(),
        ),
        ("Cargo.toml.orig".to_string(), raw_text.into_bytes()),
    ];
    if let Some(v) = &vcs {
        let info = json!({
            "git": { "sha1": v.sha1, "dirty": !v.dirty.is_empty() },
            "path_in_vcs": v.path_in_vcs,
        });
        generated.push((
            ".cargo_vcs_info.json".to_string(),
            serde_json::to_vec_pretty(&info)?,
        ));
    }
    Ok(Package {
        name,
        version,
        dir: member.dir.clone(),
        files,
        generated,
        manifest,
    })
}

/// Builds the unpacked package on its own, as a user of it would.
fn verify(pkg: &Package, ws: &Workspace, offline: bool) -> Result<()> {
    ui::action("verifying", format!("{} v{}", pkg.name, pkg.version));
    let common = crate::args::CommonArgs {
        manifest_path: Some(pkg.dir.join("Cargo.toml")),
        target_dir: Some(ws.config.target_dir.join("package").join("target")),
        offline,
        ..Default::default()
    };
    let sess = Session::new(&common, "dev")?;
    let plan = crate::cmd_build::plan_for(&sess, crate::graph::Mode::Build, &Default::default())?;
    sess.ws.config.ensure_dirs(sess.profile.output_dir_name())?;
    let (result, _) = drive(&sess, &plan.graph, |_| {})?;
    ensure_success(&result).context("the packaged crate does not build on its own")?;
    Ok(())
}

fn check_publish_allowed(pkg: &Package, registry: &str) -> Result<()> {
    match pkg.manifest.get("package").and_then(|p| p.get("publish")) {
        Some(toml::Value::Boolean(false)) => {
            bail!(
                "`{}` cannot be published: `package.publish` is false",
                pkg.name
            )
        }
        Some(toml::Value::Array(allowed)) => {
            if !allowed.iter().any(|r| r.as_str() == Some(registry)) {
                bail!(
                    "`{}` cannot be published to `{registry}`: `package.publish` only allows {}",
                    pkg.name,
                    allowed
                        .iter()
                        .filter_map(|r| r.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// After an upload, the index takes a moment to show the new version; wait
/// for it (up to a minute) so that publishing the next crate of a workspace
/// can depend on it.
fn wait_until_available(index: &str, name: &str, version: &str) {
    let Ok(client) = rune_registry::source::IndexClient::for_registry(index, false) else {
        return;
    };
    let start = std::time::Instant::now();
    ui::action("waiting", format!("for {name} v{version} to be available"));
    while start.elapsed() < std::time::Duration::from_secs(60) {
        if client
            .refresh_crate(name)
            .is_ok_and(|i| i.get_version(version).is_some())
        {
            ui::action("published", format!("{name} v{version}"));
            return;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    ui::warn(format!(
        "{name} v{version} was uploaded but is not in the index yet; it should appear shortly"
    ));
}

// ---------------------------------------------------------------------------
// Manifest normalization
// ---------------------------------------------------------------------------

/// The manifest as uploaded: inheritance resolved, path/git dependencies
/// turned into registry ones, workspace-only tables removed. Also returns
/// files the package must include because the manifest names them from the
/// workspace (`readme`, `license-file`).
fn normalize(
    raw: &toml::Table,
    ws: Option<&toml::Table>,
    ws_root: &Path,
    dir: &Path,
) -> Result<(toml::Table, Vec<(String, PathBuf)>)> {
    let mut out = raw.clone();
    let mut extra = Vec::new();
    for key in ["workspace", "patch", "replace"] {
        out.remove(key);
    }
    let ws_package = ws.and_then(|w| w.get("package")).and_then(|p| p.as_table());
    if let Some(package) = out.get_mut("package").and_then(|p| p.as_table_mut()) {
        package.remove("workspace");
        let keys: Vec<String> = package.keys().cloned().collect();
        for key in keys {
            let inherits = package[&key]
                .as_table()
                .and_then(|t| t.get("workspace"))
                .and_then(|w| w.as_bool())
                == Some(true);
            if !inherits {
                continue;
            }
            let value = ws_package
                .and_then(|p| p.get(&key))
                .cloned()
                .with_context(|| {
                    format!("`package.{key}` inherits from the workspace, which does not define it")
                })?;
            // A file named by the workspace is copied into the package.
            let value = match (key.as_str(), &value) {
                ("readme" | "license-file", toml::Value::String(path)) => {
                    let src = ws_root.join(path);
                    let name = src
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(path)
                        .to_string();
                    if src.is_file() {
                        extra.push((name.clone(), src));
                    }
                    toml::Value::String(name)
                }
                _ => value,
            };
            package.insert(key, value);
        }
    }
    if let Some(lints) = out.get("lints").and_then(|l| l.as_table())
        && lints.get("workspace").and_then(|w| w.as_bool()) == Some(true)
    {
        match ws.and_then(|w| w.get("lints")) {
            Some(l) => {
                out.insert("lints".into(), l.clone());
            }
            None => {
                out.remove("lints");
            }
        }
    }
    let ws_deps = ws
        .and_then(|w| w.get("dependencies"))
        .and_then(|d| d.as_table());
    for (key, dev) in [
        ("dependencies", false),
        ("dev-dependencies", true),
        ("build-dependencies", false),
    ] {
        if let Some(t) = out.get_mut(key).and_then(|t| t.as_table_mut()) {
            normalize_deps(t, ws_deps, dev, ws_root, dir)?;
        }
        if let Some(targets) = out.get_mut("target").and_then(|t| t.as_table_mut()) {
            for (_, tables) in targets.iter_mut() {
                if let Some(t) = tables.get_mut(key).and_then(|t| t.as_table_mut()) {
                    normalize_deps(t, ws_deps, dev, ws_root, dir)?;
                }
            }
        }
    }
    Ok((out, extra))
}

fn normalize_deps(
    deps: &mut toml::Table,
    ws_deps: Option<&toml::Table>,
    dev: bool,
    ws_root: &Path,
    dir: &Path,
) -> Result<()> {
    let names: Vec<String> = deps.keys().cloned().collect();
    for name in names {
        let mut spec = match deps[&name].clone() {
            toml::Value::String(v) => {
                let mut t = toml::Table::new();
                t.insert("version".into(), toml::Value::String(v));
                t
            }
            toml::Value::Table(t) => t,
            _ => continue,
        };
        if spec.get("workspace").and_then(|w| w.as_bool()) == Some(true) {
            let base = ws_deps.and_then(|d| d.get(&name)).with_context(|| {
                format!("dependency `{name}` inherits from the workspace, which does not define it")
            })?;
            let mut merged = match base {
                toml::Value::String(v) => {
                    let mut t = toml::Table::new();
                    t.insert("version".into(), toml::Value::String(v.clone()));
                    t
                }
                toml::Value::Table(t) => t.clone(),
                _ => toml::Table::new(),
            };
            // A workspace path is relative to the workspace root.
            if let Some(p) = merged.get("path").and_then(|p| p.as_str()) {
                merged.insert(
                    "path".into(),
                    toml::Value::String(ws_root.join(p).display().to_string()),
                );
            }
            let mut features: Vec<toml::Value> = merged
                .get("features")
                .and_then(|f| f.as_array())
                .cloned()
                .unwrap_or_default();
            for f in spec
                .get("features")
                .and_then(|f| f.as_array())
                .cloned()
                .unwrap_or_default()
            {
                if !features.contains(&f) {
                    features.push(f);
                }
            }
            if !features.is_empty() {
                merged.insert("features".into(), toml::Value::Array(features));
            }
            if let Some(o) = spec.get("optional") {
                merged.insert("optional".into(), o.clone());
            }
            spec = merged;
        }
        let local = spec.contains_key("path") || spec.contains_key("git");
        if local && !spec.contains_key("version") {
            if dev {
                // A dev-dependency that exists only locally is dropped, as
                // with Cargo: the published crate's tests can't use it.
                deps.remove(&name);
                continue;
            }
            let from = spec
                .get("path")
                .and_then(|p| p.as_str())
                .map(|p| format!("path `{}`", dir.join(p).display()))
                .unwrap_or_else(|| "a git repository".into());
            bail!(
                "all dependencies must have a version requirement when packaging; \
                 `{name}` (from {from}) has none — add `version = \"...\"` next to it"
            );
        }
        for key in ["path", "git", "branch", "tag", "rev", "workspace"] {
            spec.remove(key);
        }
        deps.insert(name, toml::Value::Table(spec));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

struct Vcs {
    sha1: String,
    path_in_vcs: String,
    /// Paths (relative to the package) git knows or would add.
    files: Vec<PathBuf>,
    /// `git status` lines for uncommitted changes in the package.
    dirty: Vec<String>,
}

fn vcs_state(dir: &Path) -> Result<Option<Vcs>> {
    let git = |args: &[&str]| -> Option<String> {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let Some(top) = git(&["rev-parse", "--show-toplevel"]) else {
        return Ok(None);
    };
    let top = PathBuf::from(top.trim());
    let sha1 = git(&["rev-parse", "HEAD"])
        .unwrap_or_default()
        .trim()
        .to_string();
    let path_in_vcs = dir
        .canonicalize()
        .ok()
        .and_then(|d| {
            d.strip_prefix(top.canonicalize().ok()?)
                .ok()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
        })
        .unwrap_or_default();
    let files = git(&[
        "ls-files",
        "-z",
        "--cached",
        "--others",
        "--exclude-standard",
        "--",
        ".",
    ])
    .unwrap_or_default()
    .split('\0')
    .filter(|s| !s.is_empty())
    .map(PathBuf::from)
    .collect();
    let dirty = git(&["status", "--porcelain", "--untracked-files=all", "--", "."])
        .unwrap_or_default()
        .lines()
        .map(|l| format!("  {}", l.trim()))
        .collect();
    Ok(Some(Vcs {
        sha1,
        path_in_vcs,
        files,
        dirty,
    }))
}

/// Every file under `dir` (relative), for packages outside version control.
fn walk(dir: &Path) -> Vec<PathBuf> {
    fn go(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let path = e.path();
            let name = e.file_name();
            if dir == root && matches!(name.to_str(), Some("target" | ".rune" | ".git")) {
                continue;
            }
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                go(root, &path, out);
            } else if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_path_buf());
            }
        }
    }
    let mut out = Vec::new();
    go(dir, dir, &mut out);
    out
}

/// Subdirectories that are packages of their own (never part of this one).
fn nested_packages(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for rel in walk(dir) {
        let name = rel.file_name().and_then(|n| n.to_str());
        if matches!(name, Some("Cargo.toml" | "Rune.toml"))
            && let Some(parent) = rel.parent().filter(|p| !p.as_os_str().is_empty())
        {
            out.push(parent.to_path_buf());
        }
    }
    out
}

/// A gitignore-style `include`/`exclude` pattern as globs over paths
/// relative to the package root.
fn gitignore_globs(pattern: &str) -> Vec<String> {
    let p = pattern.trim();
    let dir_only = p.ends_with('/');
    let p = p.trim_end_matches('/');
    let (anchored, p) = match p.strip_prefix('/') {
        Some(rest) => (true, rest),
        None => (p.contains('/'), p),
    };
    let base = if anchored {
        vec![p.to_string()]
    } else {
        vec![p.to_string(), format!("**/{p}")]
    };
    let mut out = Vec::new();
    for b in base {
        out.push(format!("{b}/**"));
        if !dir_only {
            out.push(b);
        }
    }
    out
}

fn human_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}

// ---------------------------------------------------------------------------
// login / logout / yank / owner / search / info
// ---------------------------------------------------------------------------

/// `rune login`: stores a registry token where Cargo keeps it.
pub fn login(args: LoginArgs) -> Result<()> {
    configure_registries();
    let token = match args.token {
        Some(t) => t,
        None => {
            eprintln!("please paste the token (found on https://crates.io/me for crates.io) below");
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            line.trim().to_string()
        }
    };
    if token.is_empty() {
        bail!("please provide a non-empty token");
    }
    let name = args.registry.as_deref().unwrap_or("crates-io");
    let (provider, index_url) = login_provider(name)?;
    let registry = rune_registry::credential::Registry {
        name,
        index_url: &index_url,
    };
    let stored = rune_registry::credential::store(&provider, &registry, &token)
        .map_err(|e| anyhow::anyhow!(e))?;
    if !stored {
        edit_credentials(args.registry.as_deref(), Some(&token))?;
    }
    ui::action("login", format!("token for `{name}` saved"));
    Ok(())
}

pub fn logout(args: LoginArgs) -> Result<()> {
    configure_registries();
    let name = args.registry.as_deref().unwrap_or("crates-io");
    let (provider, index_url) = login_provider(name)?;
    let registry = rune_registry::credential::Registry {
        name,
        index_url: &index_url,
    };
    let erased =
        rune_registry::credential::erase(&provider, &registry).map_err(|e| anyhow::anyhow!(e))?;
    if !erased {
        edit_credentials(args.registry.as_deref(), None)?;
    }
    ui::action("logout", format!("token for `{name}` removed"));
    Ok(())
}

/// The provider `login`/`logout` use for a registry — the first it would be
/// asked for a token — and the index URL providers are given.
fn login_provider(name: &str) -> Result<(rune_registry::credential::Provider, String)> {
    let defs = rune_registry::source::defs();
    let index = defs.index_for_name(Some(name))?;
    let provider = defs
        .providers_for(name)
        .into_iter()
        .next()
        .unwrap_or(rune_registry::credential::Provider::Token);
    Ok((
        provider,
        rune_registry::source::RegistryDefs::provider_index_url(&index),
    ))
}

/// Sets (or with `None`, removes) a token in `$CARGO_HOME/credentials.toml`,
/// keeping everything else in the file as it was.
fn edit_credentials(registry: Option<&str>, token: Option<&str>) -> Result<()> {
    let home = rune_registry::source::cargo_home()
        .context("could not determine CARGO_HOME for the credentials file")?;
    std::fs::create_dir_all(&home)?;
    let path = home.join("credentials.toml");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("cannot parse {}", path.display()))?;
    let table = match registry {
        None | Some("crates-io") => doc
            .entry("registry")
            .or_insert_with(toml_edit::table)
            .as_table_mut()
            .context("`registry` in credentials.toml is not a table")?,
        Some(name) => {
            let regs = doc
                .entry("registries")
                .or_insert_with(toml_edit::table)
                .as_table_mut()
                .context("`registries` in credentials.toml is not a table")?;
            regs.set_implicit(true);
            regs.entry(name)
                .or_insert_with(toml_edit::table)
                .as_table_mut()
                .context("a registry entry in credentials.toml is not a table")?
        }
    };
    match token {
        Some(t) => {
            table.insert("token", toml_edit::value(t));
        }
        None => {
            table.remove("token");
        }
    }
    let tmp = path.with_extension("toml.rune-tmp");
    std::fs::write(&tmp, doc.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn yank(args: YankArgs) -> Result<()> {
    configure_registries();
    let (name, version) = match (args.krate.split_once('@'), &args.version) {
        (Some((n, v)), None) => (n.to_string(), v.to_string()),
        (None, Some(v)) => (args.krate.clone(), v.clone()),
        _ => bail!("name the version to yank: `rune yank NAME@VERSION` (or `--version`)"),
    };
    let api = api_for(args.registry.as_deref(), args.token.as_deref())?;
    api.yank(&name, &version, args.undo)?;
    ui::action(
        if args.undo { "unyanked" } else { "yanked" },
        format!("{name} v{version}"),
    );
    Ok(())
}

pub fn owner(args: OwnerArgs) -> Result<()> {
    configure_registries();
    let name = match &args.krate {
        Some(n) => n.clone(),
        None => {
            let ws = Workspace::load(None)?;
            selected(&ws, &[], false)?[0].name.clone()
        }
    };
    let api = api_for(args.registry.as_deref(), args.token.as_deref())?;
    if !args.add.is_empty() {
        let msg = api.change_owners(&name, &args.add, false)?;
        ui::action(
            "owners",
            if msg.is_empty() {
                format!("invited {}", args.add.join(", "))
            } else {
                msg
            },
        );
    }
    if !args.remove.is_empty() {
        let msg = api.change_owners(&name, &args.remove, true)?;
        ui::action(
            "owners",
            if msg.is_empty() {
                format!("removed {}", args.remove.join(", "))
            } else {
                msg
            },
        );
    }
    if args.list || (args.add.is_empty() && args.remove.is_empty()) {
        for o in api.owners(&name)? {
            println!("{o}");
        }
    }
    Ok(())
}

pub fn search(args: SearchArgs) -> Result<()> {
    configure_registries();
    let api = api_for(args.registry.as_deref(), None)?;
    let query = args.query.join(" ");
    let limit = args.limit.clamp(1, 100);
    let activity = ui::Activity::start("searching", format!("{} for `{query}`", api.name));
    let (hits, total) = api.search(&query, limit)?;
    activity.clear();
    if hits.is_empty() {
        ui::warn(format!("no crates match `{query}`"));
        return Ok(());
    }

    // Piped: Cargo's lines, ready to paste into a manifest.
    if !ui::stdout_is_term() {
        let width = hits
            .iter()
            .map(|h| h.name.len() + h.version.len() + 5)
            .max()
            .unwrap_or(0);
        for h in &hits {
            let left = format!("{} = \"{}\"", h.name, h.version);
            match &h.description {
                Some(d) => {
                    let d: String = d.chars().take(80).collect();
                    ui::out(format!("{left:<width$}    # {d}"));
                }
                None => ui::out(left),
            }
        }
        if total > hits.len() as u64 {
            ui::out(format!(
                "... and {} crates more (use --limit N to see more)",
                total - hits.len() as u64
            ));
        }
        return Ok(());
    }

    let mut page = crate::pager::Page::new(format!("rune search {query}")).keep_on_exit();
    page.line(format!(
        "{} {}",
        ui::brand(format!("{} of {}", hits.len(), thousands(total))),
        format!("crates matching `{query}` on {}", api.name).dimmed()
    ));
    page.blank();
    let left_width = hits
        .iter()
        .map(|h| h.name.len() + 1 + h.version.len())
        .max()
        .unwrap_or(0)
        .clamp(12, 34);
    let text_width = ui::term_width().saturating_sub(left_width + 12).max(24);
    for h in &hits {
        let label = format!("{} {}", ui::brand(&h.name), h.version.dimmed());
        let label_width = h.name.len() + 1 + h.version.len();
        let downloads = if h.downloads > 0 {
            format!("{:>6}", downloads(h.downloads))
                .dimmed()
                .to_string()
        } else {
            "      ".to_string()
        };
        let desc = h.description.as_deref().unwrap_or("");
        let lines = ui::wrap(desc, text_width);
        // A long name gets a line of its own.
        let (first_label, pad) = if label_width > left_width {
            page.line(format!("  {label}"));
            (String::new(), left_width)
        } else {
            (label, left_width - label_width)
        };
        for (i, l) in lines.iter().enumerate() {
            if i == 0 {
                page.line(format!("  {first_label}{:pad$}  {downloads}  {l}", ""));
            } else {
                page.line(format!("  {:w$}  {:6}  {l}", "", "", w = left_width));
            }
        }
    }
    page.blank();
    let mut tips = vec![
        format!("{} for details", "rune info <crate>".bold()),
        format!("{} to use one", "rune add <crate>".bold()),
    ];
    if total > hits.len() as u64 {
        tips.push(format!("{} for more", "--limit 50".bold()));
    }
    page.line(format!("  {}", tips.join(&" · ".dimmed().to_string())));
    page.show();
    Ok(())
}

/// `22,412`.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `12.3M` / `850k` / `42`.
fn downloads(n: u64) -> String {
    match n {
        0..=999 => format!("{n}"),
        1_000..=999_999 => format!("{}k", n / 1_000),
        1_000_000..=99_999_999 => format!("{:.1}M", n as f64 / 1e6),
        _ => format!("{}M", n / 1_000_000),
    }
}

/// `rune info`: a crate's description, links, features and dependencies —
/// of the version the current project locked, else the newest.
pub fn info(args: InfoArgs) -> Result<()> {
    configure_registries();
    let (name, wanted) = match args.krate.split_once('@') {
        Some((n, v)) => (n.to_string(), Some(v.to_string())),
        None => (args.krate.clone(), None),
    };
    let defs = rune_registry::source::defs();
    let index = defs.index_for_name(args.registry.as_deref())?;
    let client = rune_registry::source::IndexClient::for_registry(&index, args.offline)?;
    let entries = client
        .fetch_crate(&name)
        .with_context(|| format!("could not find `{name}` in the registry"))?;
    let locked = wanted.clone().or_else(|| {
        let root = crate::workspace::find_project_root(None)?;
        let lock = rune_resolver::LockfileManager::load_fast(&root)?;
        lock.packages
            .iter()
            .filter(|p| p.name == name && p.source.is_some())
            .map(|p| p.version.clone())
            .max_by_key(|v| semver::Version::parse(v).ok())
    });
    let mut versions: Vec<&rune_registry::IndexRecord> = entries
        .available_versions()
        .into_iter()
        .filter(|r| !r.yanked || Some(&r.vers) == locked.as_ref())
        .collect();
    versions.sort_by_key(|r| semver::Version::parse(&r.vers).ok());
    let record = match &locked {
        Some(v) => versions
            .iter()
            .rev()
            .find(|r| {
                &r.vers == v
                    || semver::VersionReq::parse(v)
                        .ok()
                        .zip(semver::Version::parse(&r.vers).ok())
                        .is_some_and(|(req, ver)| req.matches(&ver))
            })
            .copied(),
        None => versions
            .iter()
            .rev()
            .find(|r| semver::Version::parse(&r.vers).is_ok_and(|v| v.pre.is_empty()))
            .or(versions.last())
            .copied(),
    }
    .with_context(|| format!("no matching version of `{name}`"))?;

    // Descriptions and links come from the web API, when there is one.
    let page = (!args.offline)
        .then(|| RegistryApi::for_index(&index).ok()?.crate_info(&name).ok())
        .flatten();
    let krate = page.as_ref().map(|p| &p["crate"]);
    let keywords: Vec<String> = krate
        .and_then(|k| k["keywords"].as_array())
        .or_else(|| page.as_ref().and_then(|p| p["keywords"].as_array()))
        .map(|a| {
            a.iter()
                .filter_map(|k| k.as_str().or_else(|| k["id"].as_str()))
                .map(|k| format!("#{k}"))
                .collect()
        })
        .unwrap_or_default();
    let version_page = page.as_ref().and_then(|p| {
        p["versions"]
            .as_array()?
            .iter()
            .find(|v| v["num"].as_str() == Some(&record.vers))
            .cloned()
    });

    ui::out(format!(
        "{} {}  {}",
        ui::brand(&name),
        record.vers.dimmed(),
        keywords.join(" ").dimmed()
    ));
    if let Some(d) = krate.and_then(|k| k["description"].as_str()) {
        for l in ui::wrap(d.trim(), ui::term_width().min(100)) {
            ui::out(l);
        }
    }
    ui::out("");
    let mut facts: Vec<(&str, String)> = Vec::new();
    if let Some(l) = version_page.as_ref().and_then(|v| v["license"].as_str()) {
        facts.push(("license", l.to_string()));
    }
    if let Some(rv) = &record.rust_version {
        facts.push(("rust", format!("{rv} or newer")));
    }
    if let Some(t) = record.pubtime.as_deref() {
        facts.push(("published", t.split('T').next().unwrap_or(t).to_string()));
    }
    let newest = versions
        .iter()
        .rev()
        .find(|r| semver::Version::parse(&r.vers).is_ok_and(|v| v.pre.is_empty()))
        .map(|r| r.vers.clone());
    facts.push((
        "versions",
        match &newest {
            Some(n) if *n != record.vers => format!(
                "{} published, newest {n}{}",
                versions.len(),
                if locked.is_some() {
                    " (this project uses the one above)".dimmed().to_string()
                } else {
                    String::new()
                }
            ),
            _ => format!("{} published", versions.len()),
        },
    ));
    for (label, key) in [
        ("docs", "documentation"),
        ("homepage", "homepage"),
        ("repository", "repository"),
    ] {
        if let Some(v) = krate.and_then(|k| k[key].as_str()) {
            facts.push((label, v.to_string()));
        }
    }
    if index == rune_common::CRATES_IO_INDEX {
        facts.push((
            "crates.io",
            format!("https://crates.io/crates/{name}/{}", record.vers),
        ));
    }
    for l in ui::kv_lines(&facts, 2) {
        ui::out(l);
    }

    let features = record.all_features();
    if !features.is_empty() {
        ui::out("");
        ui::out(format!(
            "{} {}",
            "features".bold(),
            "(+ on by default)".dimmed()
        ));
        let default: BTreeSet<String> = features
            .get("default")
            .map(|d| d.iter().cloned().collect())
            .unwrap_or_default();
        let mut names: Vec<&String> = features.keys().collect();
        names.sort_by_key(|n| (n.as_str() != "default", n.as_str()));
        let w = names.iter().map(|n| n.len()).max().unwrap_or(0);
        for f in names {
            let on = f == "default" || default.contains(f);
            let enables = features[f].join(", ");
            let name = format!("{f:<w$}");
            let line = format!(
                "  {} {}  {}",
                if on {
                    "+".green().bold().to_string()
                } else {
                    " ".to_string()
                },
                if on { name.bold().to_string() } else { name },
                enables.dimmed()
            );
            ui::out(line.trim_end());
        }
    }
    let normal: Vec<&rune_registry::IndexDependency> = record
        .deps
        .iter()
        .filter(|d| d.kind.as_deref().unwrap_or("normal") == "normal")
        .collect();
    if !normal.is_empty() {
        ui::out("");
        ui::out("dependencies".bold().to_string());
        let w = normal
            .iter()
            .map(|d| d.package.as_deref().unwrap_or(&d.name).len())
            .max()
            .unwrap_or(0);
        for d in normal {
            let real = d.package.as_deref().unwrap_or(&d.name);
            ui::out(format!(
                "  {real:<w$}  {}{}",
                d.req.dimmed(),
                if d.optional {
                    "  optional".dimmed().to_string()
                } else {
                    String::new()
                }
            ));
        }
    }
    if locked.is_none() {
        ui::out("");
        ui::out(format!(
            "  {} {}",
            format!("rune add {name}").bold(),
            "to use it".dimmed()
        ));
    }
    Ok(())
}

fn api_for(registry: Option<&str>, token: Option<&str>) -> Result<RegistryApi> {
    let defs = rune_registry::source::defs();
    let index = defs.index_for_name(registry)?;
    let api = RegistryApi::for_index(&index)?;
    Ok(match token {
        Some(t) => api.with_token(t),
        None => api,
    })
}

/// Registries and credentials from the configuration of the working
/// directory (these commands need no project).
fn configure_registries() {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let config = rune_common::cargo_config::CargoConfig::load(&cwd);
    if let Some(e) = config.error() {
        ui::warn(e);
    }
    rune_registry::source::configure(rune_registry::source::RegistryDefs::from_config(
        config.root(),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn include_and_exclude_patterns_follow_gitignore_rules() {
        let m = |pat: &str, path: &str| {
            gitignore_globs(pat)
                .iter()
                .any(|g| glob::Pattern::new(g).unwrap().matches(path))
        };
        assert!(m("*.md", "README.md"));
        assert!(m("*.md", "docs/guide.md"));
        assert!(m("/ci", "ci/run.sh"));
        assert!(!m("/ci", "src/ci/x.rs"));
        assert!(m("tests/data/", "tests/data/big.bin"));
        assert!(!m("tests/data/", "tests/data"));
        assert!(m("src/", "src/lib.rs"));
    }

    #[test]
    fn normalization_resolves_inheritance_and_local_dependencies() {
        let raw: toml::Table = r#"
[package]
name = "a"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
serde = { workspace = true, features = ["derive"] }
b = { path = "../b", version = "0.2" }
helper = { git = "https://example.com/h", version = "1" }

[dev-dependencies]
testkit = { path = "../testkit" }

[target.'cfg(unix)'.dependencies]
libc.workspace = true
"#
        .parse()
        .unwrap();
        let ws: toml::Table = r#"
[package]
version = "1.2.3"
edition = "2021"
license = "MIT"

[dependencies]
serde = { version = "1", features = ["std"], default-features = false }
libc = "0.2"
"#
        .parse()
        .unwrap();
        let (out, _) = normalize(&raw, Some(&ws), Path::new("/ws"), Path::new("/ws/a")).unwrap();
        let text = toml::to_string(&out).unwrap();
        let back: toml::Table = text.parse().unwrap();
        assert_eq!(back["package"]["version"].as_str(), Some("1.2.3"));
        assert_eq!(back["package"]["license"].as_str(), Some("MIT"));
        let serde = back["dependencies"]["serde"].as_table().unwrap();
        assert_eq!(serde["version"].as_str(), Some("1"));
        assert_eq!(serde["default-features"].as_bool(), Some(false));
        let feats: Vec<&str> = serde["features"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|f| f.as_str())
            .collect();
        assert_eq!(feats, ["std", "derive"]);
        let b = back["dependencies"]["b"].as_table().unwrap();
        assert!(!b.contains_key("path"));
        assert_eq!(b["version"].as_str(), Some("0.2"));
        assert!(
            !back["dependencies"]["helper"]
                .as_table()
                .unwrap()
                .contains_key("git")
        );
        assert!(
            back.get("dev-dependencies")
                .unwrap()
                .get("testkit")
                .is_none()
        );
        assert_eq!(
            back["target"]["cfg(unix)"]["dependencies"]["libc"]["version"].as_str(),
            Some("0.2")
        );

        let bad: toml::Table = "[package]\nname = \"a\"\nversion = \"1.0.0\"\n[dependencies]\nb = { path = \"../b\" }\n"
            .parse()
            .unwrap();
        let err = normalize(&bad, None, Path::new("/ws"), Path::new("/ws/a")).unwrap_err();
        assert!(err.to_string().contains("version requirement"), "{err}");
    }
}
