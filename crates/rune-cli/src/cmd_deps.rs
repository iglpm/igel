//! Dependency management: `add`, `remove`, `update`, `tree`, `why`, `fetch`,
//! `generate-lockfile`, `metadata`, `locate-project`.

use anyhow::{Context, Result, bail};
use colored::Colorize;
use rune_common::settings::Settings;
use rune_registry::SparseIndexClient;
use rune_resolver::lockfile::LockfileManager;
use rune_resolver::{TreeOptions, render_tree, trace_why};
use std::collections::BTreeMap;
use std::fs;
use toml_edit::{Array, DocumentMut, Item, Table, Value};

use crate::args::*;
use crate::graph;
use crate::ui;
use crate::workspace::Workspace;

fn load(m: &ManifestArgs) -> Result<(Workspace, bool)> {
    let ws = Workspace::load(m.manifest_path.as_deref())?;
    let offline = m.offline || ws.config.settings.offline();
    Ok((ws, offline))
}

// ---------------------------------------------------------------------------
// add / remove
// ---------------------------------------------------------------------------

/// The member whose `Cargo.toml` `add`/`remove` should edit.
fn target_member(ws: &Workspace, package: Option<&str>) -> Result<usize> {
    if let Some(name) = package {
        return ws
            .member_named(name)
            .with_context(|| format!("package `{name}` is not a member of this workspace"));
    }
    match ws.current {
        Some(i) => Ok(i),
        None if ws.members.len() == 1 => Ok(0),
        None => bail!(
            "this is a virtual workspace; choose a package with `-p` (members: {})",
            ws.members
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

pub fn add(args: AddArgs) -> Result<()> {
    let (ws, offline) = load(&args.manifest)?;
    let mi = target_member(&ws, args.package.as_deref())?;
    let manifest_path = ws.members[mi].dir.join("Cargo.toml");

    let text = fs::read_to_string(&manifest_path)?;
    let mut doc: DocumentMut = text
        .parse()
        .with_context(|| format!("Failed to parse {}", manifest_path.display()))?;

    let table_name = if args.dev {
        "dev-dependencies"
    } else if args.build {
        "build-dependencies"
    } else {
        "dependencies"
    };
    if doc.get(table_name).is_none() {
        doc[table_name] = Item::Table(Table::new());
    }

    let client = SparseIndexClient::default_client().offline(offline);
    if args.crate_specs.len() > 1 && args.version.is_some() {
        bail!("`--version` can only be used when adding a single dependency");
    }

    for spec in &args.crate_specs {
        let (name, ver_req) = match spec.split_once('@') {
            Some((n, v)) => (n.to_string(), Some(v.to_string())),
            None => (spec.clone(), args.version.clone()),
        };

        let mut inline = toml_edit::InlineTable::new();
        let version_shown;
        if let Some(path) = &args.path {
            let rel = path.to_string_lossy().to_string();
            inline.insert("path", Value::from(rel.clone()));
            version_shown = format!("path {rel}");
        } else {
            ui::action("updating", format!("`{name}` from crates.io index"));
            let index = client
                .refresh_crate(&name)
                .with_context(|| format!("could not find `{name}` in the registry"))?;
            let requirement = match ver_req {
                Some(v) => v,
                None => {
                    // Latest non-yanked, non-prerelease release.
                    let latest = index
                        .available_versions()
                        .into_iter()
                        .filter_map(|r| semver::Version::parse(&r.vers).ok())
                        .filter(|v| v.pre.is_empty())
                        .max()
                        .or_else(|| {
                            index
                                .available_versions()
                                .into_iter()
                                .filter_map(|r| semver::Version::parse(&r.vers).ok())
                                .max()
                        })
                        .with_context(|| format!("no available versions of `{name}`"))?;
                    latest.to_string()
                }
            };
            version_shown = requirement.clone();
            inline.insert("version", Value::from(requirement));
        }

        if !args.features.is_empty() {
            let mut feats = Array::new();
            for f in &args.features {
                feats.push(f.as_str());
            }
            inline.insert("features", Value::Array(feats));
        }
        if args.no_default_features {
            inline.insert("default-features", Value::from(false));
        }
        if args.optional {
            inline.insert("optional", Value::from(true));
        }

        // A bare `name = "1.0"` when there's nothing else to say.
        let item = if inline.len() == 1 && inline.contains_key("version") {
            Item::Value(inline.get("version").unwrap().clone())
        } else {
            Item::Value(Value::InlineTable(inline))
        };
        doc[table_name][&name] = item;
        ui::action(
            "added",
            format!("{} {} to [{}]", name.bold(), version_shown, table_name),
        );
    }

    fs::write(&manifest_path, doc.to_string())?;

    // Re-resolve so the lockfile and downloads are ready.
    let ws = Workspace::load(Some(&manifest_path))?;
    refresh_lockfile(&ws, offline, false, &[])?;
    Ok(())
}

pub fn remove(args: RemoveArgs) -> Result<()> {
    let (ws, offline) = load(&args.manifest)?;
    let mi = target_member(&ws, args.package.as_deref())?;
    let manifest_path = ws.members[mi].dir.join("Cargo.toml");
    let text = fs::read_to_string(&manifest_path)?;
    let mut doc: DocumentMut = text
        .parse()
        .with_context(|| format!("Failed to parse {}", manifest_path.display()))?;

    let tables: Vec<&str> = if args.dev {
        vec!["dev-dependencies"]
    } else if args.build {
        vec!["build-dependencies"]
    } else {
        vec!["dependencies", "dev-dependencies", "build-dependencies"]
    };

    let mut any = false;
    for name in &args.names {
        let mut removed = false;
        for t in &tables {
            if let Some(table) = doc.get_mut(t).and_then(|t| t.as_table_like_mut()) {
                if table.remove(name).is_some() {
                    removed = true;
                    ui::action("removed", format!("{} from [{}]", name.bold(), t));
                }
            }
        }
        // Also `[target.'cfg(..)'.dependencies]` and friends.
        if let Some(targets) = doc.get_mut("target").and_then(|t| t.as_table_like_mut()) {
            let keys: Vec<String> = targets.iter().map(|(k, _)| k.to_string()).collect();
            for key in keys {
                let Some(per_target) = targets.get_mut(&key).and_then(|i| i.as_table_like_mut())
                else {
                    continue;
                };
                for t in &tables {
                    if let Some(dep_table) =
                        per_target.get_mut(t).and_then(|i| i.as_table_like_mut())
                    {
                        if dep_table.remove(name).is_some() {
                            removed = true;
                            ui::action(
                                "removed",
                                format!("{} from [target.{key}.{t}]", name.bold()),
                            );
                        }
                    }
                }
            }
        }
        if !removed {
            ui::warn(format!(
                "package `{name}` not found in {}",
                tables.join(" or ")
            ));
        }
        any |= removed;
    }
    if any {
        fs::write(&manifest_path, doc.to_string())?;
        let ws = Workspace::load(Some(&manifest_path))?;
        refresh_lockfile(&ws, offline, false, &[])?;
    }
    Ok(())
}

/// Re-resolves and stores the lockfile, downloading whatever is now needed.
fn refresh_lockfile(ws: &Workspace, offline: bool, refresh: bool, only: &[String]) -> Result<()> {
    let mut existing = LockfileManager::load_fast(ws.root());
    if let Some(lock) = existing.as_mut() {
        if !only.is_empty() {
            lock.packages.retain(|p| !only.contains(&p.name));
        }
    }
    let seed = if refresh && only.is_empty() {
        None
    } else {
        existing.as_ref()
    };
    let (resolved, lock) = graph::resolve_fresh(ws, offline, seed, refresh)?;
    graph::store_resolution(ws, &lock)?;
    graph::download_all(&resolved, offline)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// update / fetch / generate-lockfile
// ---------------------------------------------------------------------------

pub fn update(args: UpdateArgs) -> Result<()> {
    let (ws, offline) = load(&args.manifest)?;
    if offline {
        bail!("`rune update` needs the network to look for new releases (drop --offline)");
    }
    let before = LockfileManager::load_fast(ws.root());
    ui::action("updating", "crates.io index");

    let mut seed = before.clone();
    if let Some(lock) = seed.as_mut() {
        if !args.packages.is_empty() {
            for p in &args.packages {
                if !lock.packages.iter().any(|x| &x.name == p) {
                    bail!("package `{p}` is not in the lockfile");
                }
            }
            lock.packages.retain(|p| !args.packages.contains(&p.name));
        }
    }
    let seed_for_resolve = if args.packages.is_empty() {
        None
    } else {
        seed.as_ref()
    };
    let (resolved, lock) = graph::resolve_fresh(&ws, false, seed_for_resolve, true)?;
    graph::store_resolution(&ws, &lock)?;
    graph::download_all(&resolved, false)?;

    // Report what moved, like Cargo does.
    let index = |l: &rune_resolver::Lockfile| -> BTreeMap<String, Vec<String>> {
        let mut m: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for p in l.packages.iter().filter(|p| p.source.is_some()) {
            m.entry(p.name.clone()).or_default().push(p.version.clone());
        }
        m
    };
    let old = before.as_ref().map(index).unwrap_or_default();
    let new = index(&lock);
    let mut changes = 0;
    for (name, versions) in &new {
        match old.get(name) {
            None => {
                for v in versions {
                    ui::action("adding", format!("{name} v{v}"));
                    changes += 1;
                }
            }
            Some(o) if o != versions => {
                ui::action(
                    "updating",
                    format!("{name} v{} -> v{}", o.join(", v"), versions.join(", v")),
                );
                changes += 1;
            }
            _ => {}
        }
    }
    for (name, versions) in &old {
        if !new.contains_key(name) {
            ui::action("removing", format!("{name} v{}", versions.join(", v")));
            changes += 1;
        }
    }
    if changes == 0 {
        ui::action(
            "up to date",
            format!("{} packages in lockfile", lock.packages.len()),
        );
    } else {
        ui::action(
            "updated",
            format!(
                "{} packages in lockfile ({changes} changed)",
                lock.packages.len()
            ),
        );
    }
    Ok(())
}

pub fn fetch(args: ManifestArgs) -> Result<()> {
    let (ws, offline) = load(&args)?;
    let existing = LockfileManager::load_fast(ws.root());
    let (resolved, lock) = graph::resolve_fresh(&ws, offline, existing.as_ref(), false)?;
    graph::store_resolution(&ws, &lock)?;
    let n = graph::download_all(&resolved, offline)?;
    ui::action("fetched", format!("{n} crates"));
    Ok(())
}

pub fn generate_lockfile(args: ManifestArgs) -> Result<()> {
    let (ws, offline) = load(&args)?;
    let (_, lock) = graph::resolve_fresh(&ws, offline, None, !offline)?;
    graph::store_resolution(&ws, &lock)?;
    ui::action(
        "generated",
        format!("lockfile with {} packages", lock.packages.len()),
    );
    Ok(())
}

pub fn locate_project(args: ManifestArgs) -> Result<()> {
    let root = crate::workspace::find_project_root(args.manifest_path.as_deref())
        .context("could not find `Cargo.toml` in the current directory or any parent")?;
    println!(
        "{}",
        serde_json::json!({ "root": root.join("Cargo.toml").display().to_string() })
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// tree / why
// ---------------------------------------------------------------------------

fn resolved_for_inspection(ws: &Workspace, offline: bool) -> Result<rune_resolver::ResolvedGraph> {
    let existing = LockfileManager::load_fast(ws.root());
    let (resolved, _) = graph::resolve_fresh(ws, offline, existing.as_ref(), false)?;
    Ok(resolved)
}

pub fn tree(args: TreeArgs) -> Result<()> {
    let (ws, offline) = load(&args.manifest)?;
    let resolved = resolved_for_inspection(&ws, offline)?;
    let selected = ws.select(&args.package, args.workspace, &[])?;
    let roots: Vec<String> = selected.iter().map(|&i| ws.members[i].id()).collect();
    print!(
        "{}",
        render_tree(
            &resolved,
            &roots,
            &TreeOptions {
                max_depth: args.depth,
                features: args.show_features,
                duplicates: args.duplicates,
                invert: args.invert.clone(),
                dev: args.dev,
            }
        )
    );
    Ok(())
}

pub fn why(args: WhyArgs) -> Result<()> {
    let (ws, offline) = load(&args.manifest)?;
    let resolved = resolved_for_inspection(&ws, offline)?;
    let roots: Vec<String> = ws.members.iter().map(|m| m.id()).collect();
    let chains = trace_why(&resolved, &roots, &args.package);
    if chains.is_empty() {
        ui::warn(format!(
            "package `{}` is not part of the dependency graph",
            args.package
        ));
    } else {
        println!("{} is required by:", args.package.bold());
        for chain in chains {
            println!("  {}", chain.join(" -> ".dimmed().to_string().as_str()));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// metadata
// ---------------------------------------------------------------------------

pub fn metadata(args: MetadataArgs) -> Result<()> {
    if args.format_version != 1 {
        bail!("only `--format-version 1` is supported");
    }
    let (ws, offline) = load(&args.manifest)?;
    let root = ws.root().display().to_string();

    let member_id = |m: &crate::workspace::Member| {
        format!("path+file://{}#{}@{}", m.dir.display(), m.name, m.version)
    };
    let target_json = |name: &str,
                       kind: &str,
                       crate_types: Vec<String>,
                       path: &std::path::Path,
                       edition: &str| {
        serde_json::json!({
            "kind": [kind],
            "crate_types": crate_types,
            "name": name,
            "src_path": path.display().to_string(),
            "edition": edition,
            "doc": true, "doctest": kind == "lib", "test": true,
        })
    };

    let mut packages = Vec::new();
    for m in &ws.members {
        let mut targets = Vec::new();
        if let Some(lib) = &m.manifest.lib {
            let kind = if lib.proc_macro { "proc-macro" } else { "lib" };
            targets.push(target_json(
                lib.name.as_deref().unwrap_or(&m.name),
                kind,
                vec![kind.to_string()],
                &m.dir.join(
                    lib.path
                        .as_deref()
                        .unwrap_or(std::path::Path::new("src/lib.rs")),
                ),
                m.edition.as_str(),
            ));
        }
        for (kind, list) in [
            ("bin", &m.manifest.bin),
            ("example", &m.manifest.example),
            ("test", &m.manifest.test),
            ("bench", &m.manifest.bench),
        ] {
            for t in list {
                targets.push(target_json(
                    t.name.as_deref().unwrap_or(""),
                    kind,
                    vec![if kind == "example" { "bin" } else { kind }.to_string()],
                    &m.dir
                        .join(t.path.as_deref().unwrap_or(std::path::Path::new(""))),
                    m.edition.as_str(),
                ));
            }
        }
        let deps: Vec<_> = {
            let mut v: Vec<_> = m.manifest.dependencies.values().collect();
            v.sort_by(|a, b| (&a.name, a.kind as u8).cmp(&(&b.name, b.kind as u8)));
            v.into_iter()
                .map(|d| {
                    serde_json::json!({
                        "name": d.name,
                        "req": d.spec.version.as_ref().map(|v| v.to_string()).unwrap_or_else(|| "*".into()),
                        "kind": match d.kind {
                            rune_manifest::DependencyKind::Normal => serde_json::Value::Null,
                            rune_manifest::DependencyKind::Dev => "dev".into(),
                            rune_manifest::DependencyKind::Build => "build".into(),
                        },
                        "optional": d.spec.optional,
                        "uses_default_features": d.spec.default_features,
                        "features": d.spec.features,
                        "target": d.target,
                        "path": d.spec.path.as_ref().map(|p| p.display().to_string()),
                    })
                })
                .collect()
        };
        packages.push(serde_json::json!({
            "name": m.name,
            "version": m.version.to_string(),
            "id": member_id(m),
            "source": serde_json::Value::Null,
            "description": m.meta.description,
            "license": m.meta.license,
            "dependencies": deps,
            "targets": targets,
            "features": m.manifest.features,
            "manifest_path": m.dir.join("Cargo.toml").display().to_string(),
            "edition": m.edition.as_str(),
            "authors": m.meta.authors.split(':').filter(|s| !s.is_empty()).collect::<Vec<_>>(),
        }));
    }

    let mut resolve = serde_json::Value::Null;
    if !args.no_deps {
        let existing = LockfileManager::load_fast(ws.root());
        let (resolved, _) = graph::resolve_fresh(&ws, offline, existing.as_ref(), false)?;
        let reg = rune_registry::CrateDownloader::default_downloader().offline(true);
        let mut nodes = Vec::new();
        let mut pkgs: Vec<_> = resolved
            .packages
            .values()
            .filter(|p| matches!(p.source, rune_resolver::resolver::PackageSource::Registry))
            .collect();
        pkgs.sort_by(|a, b| (&a.name, &a.version).cmp(&(&b.name, &b.version)));
        for p in pkgs {
            let id = format!(
                "registry+https://github.com/rust-lang/crates.io-index#{}@{}",
                p.name, p.version
            );
            let manifest_path = reg
                .download_and_extract(&p.name, &p.version.to_string(), p.checksum.as_deref())
                .ok()
                .map(|d| d.join("Cargo.toml").display().to_string());
            packages.push(serde_json::json!({
                "name": p.name,
                "version": p.version.to_string(),
                "id": id,
                "source": "registry+https://github.com/rust-lang/crates.io-index",
                "dependencies": [],
                "targets": [],
                "features": {},
                "manifest_path": manifest_path,
            }));
            let deps: Vec<String> = p
                .dependencies
                .iter()
                .filter_map(|d| {
                    resolved
                        .find_matching(d.package.as_deref().unwrap_or(&d.name), &d.req)
                        .map(|t| {
                            format!(
                                "registry+https://github.com/rust-lang/crates.io-index#{}@{}",
                                t.name, t.version
                            )
                        })
                })
                .collect();
            nodes.push(serde_json::json!({
                "id": id, "dependencies": deps, "features": p.features,
            }));
        }
        resolve = serde_json::json!({ "nodes": nodes, "root": serde_json::Value::Null });
    }

    let out = serde_json::json!({
        "packages": packages,
        "workspace_members": ws.members.iter().map(member_id).collect::<Vec<_>>(),
        "workspace_default_members": ws.members.iter().map(member_id).collect::<Vec<_>>(),
        "resolve": resolve,
        "target_directory": ws.config.target_dir.display().to_string(),
        "version": 1,
        "workspace_root": root,
        "metadata": serde_json::Value::Null,
    });
    println!("{}", serde_json::to_string(&out)?);
    let _ = Settings::default();
    Ok(())
}
