//! Dependency management: `add`, `remove`, `update`, `tree`, `why`, `fetch`,
//! `generate-lockfile`, `metadata`, `locate-project`.

use anyhow::{Context, Result, bail};
use colored::Colorize;
use rune_common::settings::Settings;
use rune_resolver::lockfile::LockfileManager;
use rune_resolver::{TreeOptions, render_tree, trace_why};
use std::collections::{BTreeMap, BTreeSet};
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

/// The member whose manifest `add`/`remove` should edit.
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

/// The features of a release — `+` for those that `requested` (and the
/// defaults, when `defaults`) turn on — as `cargo add` lists them, then the
/// release's `rust-version` and publication date when the index has them.
fn release_notes(
    record: &rune_registry::IndexRecord,
    requested: &[String],
    defaults: bool,
) -> Vec<String> {
    let features = record.all_features();
    // Optional dependencies are features too, unless a `dep:` hides them.
    let hidden: std::collections::HashSet<&str> = features
        .values()
        .flatten()
        .filter_map(|v| v.strip_prefix("dep:"))
        .collect();
    let mut all: std::collections::BTreeSet<String> = features
        .keys()
        .filter(|f| *f != "default")
        .cloned()
        .collect();
    for d in &record.deps {
        if d.optional && d.kind.as_deref() != Some("dev") && !hidden.contains(d.name.as_str()) {
            all.insert(d.name.clone());
        }
    }
    let mut on: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut stack: Vec<String> = requested
        .iter()
        .flat_map(|f| f.split([',', ' ']))
        .filter(|f| !f.is_empty() && !f.contains('/'))
        .map(String::from)
        .collect();
    if defaults {
        stack.push("default".into());
    }
    while let Some(f) = stack.pop() {
        if !on.insert(f.clone()) {
            continue;
        }
        for v in features.get(&f).into_iter().flatten() {
            if v.starts_with("dep:") {
                continue;
            }
            // `dep/feature` turns on the optional dependency `dep` (and its
            // implicit feature); `dep?/feature` does not.
            let name = match v.split_once('/') {
                Some((d, _)) if d.ends_with('?') => continue,
                Some((d, _)) => d,
                None => v.as_str(),
            };
            if all.contains(name) {
                stack.push(name.to_string());
            }
        }
    }
    let mut notes = Vec::new();
    // Enabled first, as `cargo add` lists them.
    let (enabled, rest): (Vec<&String>, Vec<&String>) = all.iter().partition(|f| on.contains(*f));
    if all.len() > 40 {
        // `windows-sys` has hundreds: the enabled ones and a count.
        notes.push(format!(
            "{} features in v{}, {} enabled (`rune info {}` lists them)",
            all.len(),
            record.vers,
            enabled.len(),
            record.name
        ));
        notes.extend(enabled.iter().map(|f| format!("  + {f}")));
    } else if !all.is_empty() {
        notes.push(format!("features of v{}:", record.vers));
        notes.extend(enabled.iter().map(|f| format!("  + {f}")));
        notes.extend(rest.iter().map(|f| format!("  - {f}")));
    }
    let mut facts = Vec::new();
    if let Some(rv) = &record.rust_version {
        facts.push(format!("needs Rust {rv}"));
    }
    if let Some(date) = record.pubtime.as_deref().and_then(|t| t.get(..10)) {
        facts.push(format!("published {date}"));
    }
    if !facts.is_empty() {
        notes.push(format!("v{}: {}", record.vers, facts.join(", ")));
    }
    notes
}

pub fn add(args: AddArgs) -> Result<()> {
    let (ws, offline) = load(&args.manifest)?;
    let mi = target_member(&ws, args.package.as_deref())?;
    let manifest_path = ws.members[mi].manifest_path.clone();

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
    // `--target 'cfg(unix)'` puts it under `[target.'cfg(unix)'.dependencies]`.
    let table: &mut Item = match &args.target {
        Some(t) => {
            if doc.get("target").is_none() {
                let mut targets = Table::new();
                targets.set_implicit(true);
                doc["target"] = Item::Table(targets);
            }
            if doc["target"].get(t).is_none() {
                let mut platform = Table::new();
                platform.set_implicit(true);
                // `[target.'cfg(unix)'.dependencies]`, spelled as Cargo does.
                let key = toml_edit::Key::parse(&format!("'{t}'"))
                    .ok()
                    .and_then(|mut k| k.pop())
                    .unwrap_or_else(|| toml_edit::Key::new(t.as_str()));
                doc["target"]
                    .as_table_mut()
                    .context("`target` in the manifest is not a table")?
                    .insert_formatted(&key, Item::Table(platform));
            }
            &mut doc["target"][t.as_str()][table_name]
        }
        None => &mut doc[table_name],
    };
    if table.is_none() {
        *table = Item::Table(Table::new());
    }

    if args.crate_specs.len() > 1
        && (args.version.is_some()
            || args.rename.is_some()
            || args.git.is_some()
            || args.path.is_some())
    {
        bail!(
            "`--version`, `--rename`, `--git` and `--path` can only be used when adding a single dependency"
        );
    }
    let defs = rune_registry::source::defs();
    let index_url = defs.index_for_name(args.registry.as_deref())?;
    let client = rune_registry::source::IndexClient::for_registry(&index_url, offline)?;
    // Dependencies the workspace declares once for all its members.
    let ws_deps: Vec<String> = ws
        .workspace_manifest()
        .map(|w| w.dependencies.keys().cloned().collect())
        .unwrap_or_default();

    let mut added = Vec::new();
    // What to say under each added crate: its features and release facts.
    let mut notes: Vec<Vec<String>> = Vec::new();
    for spec in &args.crate_specs {
        let mut chosen: Option<rune_registry::IndexRecord> = None;
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
        } else if let Some(url) = &args.git {
            inline.insert("git", Value::from(url.as_str()));
            for (key, value) in [
                ("branch", &args.branch),
                ("tag", &args.tag),
                ("rev", &args.rev),
            ] {
                if let Some(v) = value {
                    inline.insert(key, Value::from(v.as_str()));
                }
            }
            version_shown = format!("git {url}");
        } else if ver_req.is_none() && args.registry.is_none() && ws_deps.contains(&name) {
            inline.insert("workspace", Value::from(true));
            version_shown = "from the workspace".to_string();
        } else {
            let looking = ui::Activity::start(
                "looking up",
                format!("`{name}` in the {} index", defs.name_of(&index_url)),
            );
            let index = match client.refresh_crate(&name) {
                Ok(index) => index,
                Err(rune_registry::index::IndexError::CrateNotFound(_)) => {
                    looking.clear();
                    let registry = defs.name_of(&index_url);
                    let similar = similar_crates(&index_url, &name);
                    if similar.is_empty() {
                        bail!("no crate named `{name}` in the {registry} registry");
                    }
                    bail!(
                        "no crate named `{name}` in the {registry} registry; did you mean {}?",
                        similar
                            .iter()
                            .map(|s| format!("`{s}`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
                Err(e) => {
                    return Err(anyhow::Error::new(e)
                        .context(format!("could not look up `{name}` in the registry")));
                }
            };
            looking.clear();
            let requirement = match ver_req {
                Some(v) => v,
                None => {
                    // Latest non-yanked, non-prerelease release — old enough
                    // for `resolve.min-release-age`, when one is.
                    let cutoff = ws.config.settings.min_release_age().map(|age| {
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map_or(0, |d| d.as_secs())
                            .saturating_sub(age.as_secs())
                    });
                    let mature = |r: &&rune_registry::IndexRecord| {
                        cutoff.is_none_or(|c| {
                            r.pubtime
                                .as_deref()
                                .and_then(rune_common::time::parse_rfc3339)
                                .is_none_or(|t| t <= c)
                        })
                    };
                    let stable_mature = index
                        .available_versions()
                        .into_iter()
                        .filter(mature)
                        .filter_map(|r| semver::Version::parse(&r.vers).ok())
                        .filter(|v| v.pre.is_empty())
                        .max();
                    let latest = stable_mature
                        .or_else(|| {
                            index
                                .available_versions()
                                .into_iter()
                                .filter_map(|r| semver::Version::parse(&r.vers).ok())
                                .filter(|v| v.pre.is_empty())
                                .max()
                        })
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
            chosen = semver::VersionReq::parse(&requirement)
                .ok()
                .and_then(|req| {
                    index
                        .available_versions()
                        .into_iter()
                        .filter(|r| semver::Version::parse(&r.vers).is_ok_and(|v| req.matches(&v)))
                        .max_by_key(|r| semver::Version::parse(&r.vers).ok())
                        .cloned()
                });
            inline.insert("version", Value::from(requirement));
            if let Some(r) = &args.registry {
                inline.insert("registry", Value::from(r.as_str()));
            }
        }
        let key = match &args.rename {
            Some(alias) => {
                inline.insert("package", Value::from(name.as_str()));
                alias.clone()
            }
            None => name.clone(),
        };

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
        table[&key] = item;
        let section = match &args.target {
            Some(t) => format!("target.'{t}'.{table_name}"),
            None => table_name.to_string(),
        };
        added.push(format!("{} {} to [{}]", key.bold(), version_shown, section));
        notes.push(
            chosen
                .map(|r| release_notes(&r, &args.features, !args.no_default_features))
                .unwrap_or_default(),
        );
    }
    for (line, notes) in added.iter().zip(&notes) {
        ui::action(if args.dry_run { "would add" } else { "added" }, line);
        for n in notes {
            ui::hint_line(n);
        }
    }
    if args.dry_run {
        ui::warn("aborting add (--dry-run)");
        return Ok(());
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
    let manifest_path = ws.members[mi].manifest_path.clone();
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
            if let Some(table) = doc.get_mut(t).and_then(|t| t.as_table_like_mut())
                && table.remove(name).is_some()
            {
                removed = true;
                ui::action("removed", format!("{} from [{}]", name.bold(), t));
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
                        && dep_table.remove(name).is_some()
                    {
                        removed = true;
                        ui::action(
                            "removed",
                            format!("{} from [target.{key}.{t}]", name.bold()),
                        );
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
pub(crate) fn refresh_lockfile(
    ws: &Workspace,
    offline: bool,
    refresh: bool,
    only: &[String],
) -> Result<()> {
    let mut existing = LockfileManager::load_fast(ws.root());
    if let Some(lock) = existing.as_mut()
        && !only.is_empty()
    {
        lock.packages.retain(|p| !only.contains(&p.name));
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

    // `NAME` or `NAME@VERSION` (to pick one of several locked versions),
    // positional or with `-p` as in older Cargo.
    let specs: Vec<String> = args.packages.iter().chain(&args.package).cloned().collect();
    if args.precise.is_some() && specs.len() != 1 {
        bail!("`--precise` needs exactly one package: `rune update -p NAME --precise VERSION`");
    }
    let mut seed = before.clone();
    let mut preferred = Vec::new();
    let mut precise_target: Option<(String, semver::Version)> = None;
    match seed.as_mut() {
        Some(lock) => {
            for spec in &specs {
                let (name, version) = match spec.split_once('@') {
                    Some((n, v)) => (n, Some(v)),
                    None => (spec.as_str(), None),
                };
                let selects = |p: &rune_resolver::LockPackage| {
                    p.name == name && version.is_none_or(|v| p.version == v)
                };
                let matching: Vec<rune_resolver::LockPackage> = lock
                    .packages
                    .iter()
                    .filter(|p| selects(p))
                    .cloned()
                    .collect();
                if matching.is_empty() {
                    bail!("package `{spec}` is not in the lockfile");
                }
                if let Some(precise) = &args.precise {
                    if matching.len() > 1 {
                        bail!(
                            "`{name}` is locked at several versions ({}); name one with `{name}@VERSION`",
                            matching
                                .iter()
                                .map(|p| p.version.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                    }
                    let v = semver::Version::parse(precise)
                        .with_context(|| format!("`{precise}` is not a version"))?;
                    let registry = graph::index_of_lock_source(matching[0].source.as_deref());
                    preferred.push((registry, name.to_string(), v.clone()));
                    precise_target = Some((name.to_string(), v));
                }
                lock.packages.retain(|p| !selects(p));
            }
        }
        None if args.precise.is_some() => {
            bail!("there is no lockfile yet: run `rune generate-lockfile` first")
        }
        None => {}
    }
    let seed_for_resolve = if specs.is_empty() {
        None
    } else {
        seed.as_ref()
    };
    let (resolved, lock) =
        graph::resolve_fresh_with(&ws, false, seed_for_resolve, true, &preferred)?;
    if let Some((name, v)) = &precise_target
        && !lock
            .packages
            .iter()
            .any(|p| p.name == *name && p.version == v.to_string())
    {
        bail!(
            "cannot update `{name}` to {v}: it does not exist in the registry, or the \
                 dependency graph's requirements rule it out (see `rune tree -i {name}`)"
        );
    }
    if !args.dry_run {
        graph::store_resolution(&ws, &lock)?;
        graph::download_all(&resolved, false)?;
    }

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
    if args.dry_run {
        ui::warn("not updating the lockfile (--dry-run)");
    } else if changes == 0 {
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
    ui::summary(format!(
        "{} available offline",
        ui::plural(n, "registry crate")
    ));
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
    let root = crate::workspace::find_project_root(args.manifest_path.as_deref()).context(
        "could not find `Cargo.toml` or `Rune.toml` in the current directory or any parent",
    )?;
    println!(
        "{}",
        serde_json::json!({ "root": rune_common::manifest_file::manifest_in(&root).display().to_string() })
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
    if args.exec_surface {
        return crate::cmd_surface::exec_surface(&args);
    }
    let (ws, offline) = load(&args.manifest)?;
    let edges = rune_resolver::tree::Edges::parse(&args.edges).map_err(|e| anyhow::anyhow!(e))?;
    let target = match args.target.as_deref() {
        Some("all") => None,
        Some(t) => Some(t.to_string()),
        None => Some(rune_common::toolchain::global().host.to_string()),
    };
    // What a build on that platform uses, with the features it is built with.
    let resolved = graph::tree_view(&ws, offline, target.as_deref())?;
    let selected = ws.select(&args.package, args.workspace, &[])?;
    let roots: Vec<String> = selected.iter().map(|&i| ws.members[i].id()).collect();
    let prefix = match args.prefix.as_str() {
        "depth" => rune_resolver::tree::Prefix::Depth,
        "none" => rune_resolver::tree::Prefix::None,
        _ => rune_resolver::tree::Prefix::Indent,
    };
    // The resolved graph knows registry packages' feature tables; the
    // workspace's own come from their manifests.
    let mut resolved = resolved;
    for m in &ws.members {
        if let Some(p) = resolved.packages.get_mut(&m.id())
            && p.declared_features.is_empty()
        {
            p.declared_features = m
                .manifest
                .features
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
        }
    }
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
                edges,
                prefix,
                target,
                members: ws.members.iter().map(|m| m.id()).collect(),
            }
        )
    );
    Ok(())
}

/// `rune features [DEP]`: a package's features, which are on in a build on
/// this platform, and what turned each on.
pub fn features(args: crate::args::FeaturesArgs) -> Result<()> {
    let (ws, offline) = load(&args.manifest)?;
    let target = match args.target.as_deref() {
        Some(t) => t.to_string(),
        None => rune_common::toolchain::global().host.to_string(),
    };
    let mut resolved = graph::tree_view(&ws, offline, Some(&target))?;
    let members: Vec<String> = ws.members.iter().map(|m| m.id()).collect();
    // The workspace's own feature tables come from their manifests.
    for m in &ws.members {
        if let Some(p) = resolved.packages.get_mut(&m.id())
            && p.declared_features.is_empty()
        {
            p.declared_features = m
                .manifest
                .features
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
        }
    }

    let Some(wanted) = &args.package else {
        // The workspace's own packages: what they declare.
        let (with, without): (Vec<_>, Vec<_>) = ws
            .members
            .iter()
            .partition(|m| !m.manifest.features.is_empty());
        for (i, m) in with.iter().enumerate() {
            if i > 0 {
                ui::out("");
            }
            let declared: BTreeMap<&String, &Vec<String>> = m.manifest.features.iter().collect();
            let default: BTreeSet<&str> = declared
                .get(&"default".to_string())
                .map(|d| d.iter().map(String::as_str).collect())
                .unwrap_or_default();
            ui::out(format!(
                "{} {}  {}",
                ui::brand(&m.name),
                m.version.to_string().dimmed(),
                if declared.is_empty() {
                    "no features".dimmed().to_string()
                } else {
                    format!(
                        "{}, {} on by default",
                        ui::plural(declared.len(), "feature"),
                        default.len()
                    )
                    .dimmed()
                    .to_string()
                }
            ));
            let w = declared.keys().map(|k| k.len()).max().unwrap_or(0);
            for (name, enables) in &declared {
                let on = name.as_str() == "default" || default.contains(name.as_str());
                let line = format!(
                    "  {} {}  {}",
                    if on {
                        "+".green().bold().to_string()
                    } else {
                        " ".into()
                    },
                    if on {
                        format!("{name:<w$}").bold().to_string()
                    } else {
                        format!("{name:<w$}")
                    },
                    enables.join(", ").dimmed()
                );
                ui::out(line.trim_end());
            }
        }
        if !without.is_empty() {
            if !with.is_empty() {
                ui::out("");
            }
            let names: Vec<String> = without.iter().map(|m| m.name.clone()).collect();
            ui::out(format!(
                "{} {}",
                names.join(", "),
                if names.len() == 1 {
                    "declares no features"
                } else {
                    "declare no features"
                }
                .dimmed()
            ));
        }
        return Ok(());
    };

    let found: Vec<&rune_resolver::ResolvedPackage> = resolved
        .packages
        .values()
        .filter(|p| p.name == *wanted)
        .collect();
    if found.is_empty() {
        bail!(
            "`{wanted}` is not in what a build for {target} uses (see `rune tree`, or `rune why {wanted}`)"
        );
    }
    for (n, pkg) in found.iter().enumerate() {
        if n > 0 {
            ui::out("");
        }
        let on: BTreeSet<&str> = pkg.features.iter().map(String::as_str).collect();
        let mut all: BTreeSet<&str> = pkg.declared_features.keys().map(String::as_str).collect();
        all.extend(on.iter().copied());
        // Optional dependencies are features too, when nothing names them
        // with `dep:`.
        ui::out(format!(
            "{} {}  {}",
            ui::brand(&pkg.name),
            pkg.version.to_string().dimmed(),
            format!("{} of {} features on", on.len(), all.len()).dimmed()
        ));
        let w = all.iter().map(|f| f.len()).max().unwrap_or(0);
        for f in &all {
            let is_on = on.contains(f);
            if !is_on && !args.all {
                continue;
            }
            let reasons = if is_on {
                who_enables(&resolved, pkg, f, &members)
            } else {
                Vec::new()
            };
            let mark = if is_on {
                "+".green().bold().to_string()
            } else {
                "-".dimmed().to_string()
            };
            let name = if is_on {
                format!("{f:<w$}").bold().to_string()
            } else {
                format!("{f:<w$}").dimmed().to_string()
            };
            let why = if reasons.is_empty() {
                String::new()
            } else {
                format!(
                    "{} {}",
                    "←".dimmed(),
                    reasons.join(&", ".dimmed().to_string())
                )
            };
            ui::out(format!("  {mark} {name}  {why}").trim_end());
        }
        let off = all.len() - on.len();
        if off > 0 && !args.all {
            ui::out(format!(
                "  {}",
                format!("{} off (`--all` lists them)", off).dimmed()
            ));
        }
    }
    Ok(())
}

/// What turns on feature `f` of `pkg` in `graph`, as display fragments:
/// another feature of the package itself, dependents that ask for it, the
/// dependents' default features, or a dependent's own feature (`serde/derive`).
fn who_enables(
    graph: &rune_resolver::ResolvedGraph,
    pkg: &rune_resolver::ResolvedPackage,
    f: &str,
    members: &[String],
) -> Vec<String> {
    let label = |p: &rune_resolver::ResolvedPackage| -> String {
        let id = format!("{} {}", p.name, p.version);
        if members.contains(&id) {
            p.name.bold().to_string()
        } else {
            p.name.clone()
        }
    };
    let pkg_on: BTreeSet<&str> = pkg.features.iter().map(String::as_str).collect();
    let mut internal: Vec<String> = Vec::new();
    let mut declared: Vec<String> = Vec::new();
    let mut defaults: Vec<String> = Vec::new();
    let mut via_feature: Vec<String> = Vec::new();
    // Inside the package: another feature that lists this one.
    let mut own: Vec<(&String, &Vec<String>)> = pkg.declared_features.iter().collect();
    own.sort();
    for (g, values) in own {
        if g != f && pkg_on.contains(g.as_str()) && values.iter().any(|v| v == f) {
            internal.push(format!("feature `{g}`"));
        }
    }
    let mut parents: Vec<&rune_resolver::ResolvedPackage> = graph.packages.values().collect();
    parents.sort_by(|a, b| (&a.name, &a.version).cmp(&(&b.name, &b.version)));
    for parent in parents {
        let parent_on: BTreeSet<&str> = parent.features.iter().map(String::as_str).collect();
        for dep in &parent.dependencies {
            let real = dep.package.as_deref().unwrap_or(&dep.name);
            if real != pkg.name
                || !semver::VersionReq::parse(&dep.req).is_ok_and(|r| r.matches(&pkg.version))
            {
                continue;
            }
            // An optional dependency the parent does not turn on adds nothing.
            let active = !dep.optional
                || parent_on.contains(dep.name.as_str())
                || parent.declared_features.iter().any(|(g, vals)| {
                    parent_on.contains(g.as_str())
                        && vals.iter().any(|v| v == &format!("dep:{}", dep.name))
                });
            if !active {
                continue;
            }
            let who = match dep.kind {
                rune_manifest::DependencyKind::Build => format!("{} (build)", label(parent)),
                rune_manifest::DependencyKind::Dev => format!("{} (dev)", label(parent)),
                _ => label(parent),
            };
            if dep.features.iter().any(|x| x == f) {
                declared.push(who.clone());
            }
            if f == "default" && dep.default_features {
                defaults.push(who.clone());
            }
            let mut parent_features: Vec<(&String, &Vec<String>)> =
                parent.declared_features.iter().collect();
            parent_features.sort();
            for (g, vals) in parent_features {
                if !parent_on.contains(g.as_str()) {
                    continue;
                }
                let hit = vals.iter().any(|v| {
                    v.split_once('/')
                        .is_some_and(|(d, feat)| feat == f && d.trim_end_matches('?') == dep.name)
                });
                if hit {
                    via_feature.push(format!(
                        "{} {}",
                        label(parent),
                        format!("feature `{g}`").dimmed()
                    ));
                }
            }
        }
    }
    let list = |mut names: Vec<String>| -> String {
        names.dedup();
        const SHOWN: usize = 3;
        let more = names.len().saturating_sub(SHOWN);
        names.truncate(SHOWN);
        let mut s = names.join(", ");
        if more > 0 {
            s.push_str(&format!(" +{more}").dimmed().to_string());
        }
        s
    };
    let mut out = internal;
    if !declared.is_empty() {
        out.push(list(declared));
    }
    if !defaults.is_empty() {
        out.push(format!(
            "{} {}",
            "default features of".dimmed(),
            list(defaults)
        ));
    }
    if !via_feature.is_empty() {
        out.push(list(via_feature));
    }
    out
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
        ui::out(format!(
            "{} {}",
            ui::brand(&args.package),
            format!(
                "is in the graph through {}:",
                ui::plural(chains.len(), "path")
            )
            .dimmed()
        ));
        let arrow = " → ".dimmed().to_string();
        for chain in chains {
            let last = chain.len().saturating_sub(1);
            let steps: Vec<String> = chain
                .iter()
                .enumerate()
                .map(|(i, step)| {
                    let (name, version) = step.rsplit_once(' ').unwrap_or((step, ""));
                    let name = if i == last {
                        ui::brand(name)
                    } else {
                        name.to_string()
                    };
                    if version.is_empty() {
                        name
                    } else {
                        format!("{name} {}", version.dimmed())
                    }
                })
                .collect();
            ui::out(format!("  {}", steps.join(&arrow)));
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

    let member_id = |m: &crate::workspace::Member| path_id(&m.dir, &m.name, &m.version);

    let mut packages = Vec::new();
    let mut resolve = serde_json::Value::Null;
    if args.no_deps {
        for m in &ws.members {
            packages.push(metadata_package(m, &member_id(m), None));
        }
    } else {
        let existing = LockfileManager::load_fast(ws.root());
        let (resolved, lock) = graph::resolve_fresh(&ws, offline, existing.as_ref(), false)?;
        let reg_dl = rune_registry::CrateDownloader::default_downloader().offline(true);
        // Every package of the resolution, with its manifest where it is on
        // disk (a registry crate once downloaded, a git checkout, a path).
        let mut entries: Vec<(
            &rune_resolver::ResolvedPackage,
            String,
            Option<crate::workspace::Member>,
        )> = resolved
            .packages
            .values()
            .map(|p| {
                let (id, dir) = match &p.source {
                    rune_resolver::PackageSource::Path(d) => {
                        (path_id(d, &p.name, &p.version), Some(d.clone()))
                    }
                    rune_resolver::PackageSource::Registry(_) => {
                        let src = p.source.lock_source().unwrap_or_default();
                        let dir = reg_dl
                            .download_and_extract(
                                &p.name,
                                &p.version.to_string(),
                                p.checksum.as_deref(),
                            )
                            .ok();
                        (format!("{src}#{}@{}", p.name, p.version), dir)
                    }
                    rune_resolver::PackageSource::Git { url, reference, .. } => {
                        let src = match reference {
                            Some(r) => format!("git+{url}?{r}"),
                            None => format!("git+{url}"),
                        };
                        (format!("{src}#{}@{}", p.name, p.version), None)
                    }
                };
                let member = ws
                    .members
                    .iter()
                    .find(|m| Some(&m.dir) == dir.as_ref())
                    .cloned()
                    .or_else(|| {
                        dir.as_ref()
                            .and_then(|d| crate::workspace::Member::load(d, None).ok())
                    });
                let id = match &member {
                    Some(m) if ws.members.iter().any(|w| w.dir == m.dir) => member_id(m),
                    _ => id,
                };
                (p, id, member)
            })
            .collect();
        entries.sort_by(|a, b| (&a.0.name, &a.0.version).cmp(&(&b.0.name, &b.0.version)));

        let mut nodes = Vec::new();
        for (p, id, member) in &entries {
            let src = p.source.lock_source();
            match member {
                Some(m) => packages.push(metadata_package(m, id, src.as_deref())),
                None => packages.push(serde_json::json!({
                    "name": p.name,
                    "version": p.version.to_string(),
                    "id": id,
                    "source": src,
                    "dependencies": [],
                    "targets": [],
                    "features": p.declared_features,
                    "manifest_path": serde_json::Value::Null,
                })),
            }
            // The locked edges, with the name the package uses for each.
            let locked = lock.packages.iter().find(|l| {
                l.name == p.name && l.version == p.version.to_string() && l.source == src
            });
            let mut deps = Vec::new();
            let mut dep_ids = Vec::new();
            for d in locked
                .map(|l| l.dependencies.as_slice())
                .unwrap_or_default()
            {
                let mut parts = d.split(' ');
                let name = parts.next().unwrap_or_default();
                let target = match parts.next() {
                    Some(v) => entries
                        .iter()
                        .find(|(q, _, _)| q.name == name && q.version.to_string() == v),
                    None => entries.iter().find(|(q, _, _)| q.name == name),
                };
                let Some((tp, tid, tm)) = target else {
                    continue;
                };
                dep_ids.push((*tid).clone());
                let lib_name = tm
                    .as_ref()
                    .and_then(|m| m.manifest.lib.as_ref())
                    .and_then(|l| l.name.clone())
                    .unwrap_or_else(|| tp.name.clone());
                let mut extern_name = lib_name.replace('-', "_");
                let mut kinds = Vec::new();
                if let Some(m) = member {
                    for dep in m.manifest.dependencies.values() {
                        let real = dep.spec.package.as_deref().unwrap_or(&dep.name);
                        if real != tp.name {
                            continue;
                        }
                        if dep.spec.package.is_some() {
                            extern_name = dep.name.replace('-', "_");
                        }
                        kinds.push(serde_json::json!({
                            "kind": match dep.kind {
                                rune_manifest::DependencyKind::Normal => serde_json::Value::Null,
                                rune_manifest::DependencyKind::Dev => "dev".into(),
                                rune_manifest::DependencyKind::Build => "build".into(),
                            },
                            "target": dep.target,
                        }));
                    }
                }
                if kinds.is_empty() {
                    kinds.push(serde_json::json!({ "kind": null, "target": null }));
                }
                deps.push(serde_json::json!({
                    "name": extern_name,
                    "pkg": tid,
                    "dep_kinds": kinds,
                }));
            }
            dep_ids.sort();
            dep_ids.dedup();
            nodes.push(serde_json::json!({
                "id": id,
                "dependencies": dep_ids,
                "deps": deps,
                "features": p.features,
            }));
        }
        let root_id = ws
            .root_manifest
            .package
            .as_ref()
            .and_then(|_| ws.members.iter().find(|m| m.dir == ws.root()))
            .map(member_id);
        resolve = serde_json::json!({ "nodes": nodes, "root": root_id });
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

/// Cargo's package id of a local package: `path+file:///w/foo#0.1.0`, with
/// the name spelled out when the directory is named differently.
fn path_id(dir: &std::path::Path, name: &str, version: &semver::Version) -> String {
    if dir.file_name().is_some_and(|n| n == name) {
        format!("path+file://{}#{version}", dir.display())
    } else {
        format!("path+file://{}#{name}@{version}", dir.display())
    }
}

/// One package as `cargo metadata` describes it.
fn metadata_package(
    m: &crate::workspace::Member,
    id: &str,
    source: Option<&str>,
) -> serde_json::Value {
    let target_json = |name: &str,
                       kind: &str,
                       crate_types: Vec<String>,
                       path: &std::path::Path,
                       required: &[String]| {
        serde_json::json!({
            "kind": [kind],
            "crate_types": crate_types,
            "name": name,
            "src_path": path.display().to_string(),
            "edition": m.edition.as_str(),
            "doc": kind == "lib" || kind == "proc-macro" || kind == "bin",
            "doctest": kind == "lib" || kind == "proc-macro",
            "test": kind != "example",
            "required-features": required,
        })
    };
    let mut targets = Vec::new();
    if let Some(lib) = &m.manifest.lib {
        let kind = if lib.proc_macro { "proc-macro" } else { "lib" };
        targets.push(target_json(
            lib.name.as_deref().unwrap_or(&m.name.replace('-', "_")),
            kind,
            vec![kind.to_string()],
            &m.dir.join(
                lib.path
                    .as_deref()
                    .unwrap_or(std::path::Path::new("src/lib.rs")),
            ),
            &[],
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
                &t.required_features,
            ));
        }
    }
    if let Some(script) = m
        .manifest
        .package
        .as_ref()
        .and_then(|p| p.resolve_build_script(&m.dir))
    {
        targets.push(target_json(
            "build-script-build",
            "custom-build",
            vec!["bin".into()],
            &script,
            &[],
        ));
    }
    let mut deps: Vec<_> = m.manifest.dependencies.values().collect();
    deps.sort_by(|a, b| (&a.name, a.kind as u8).cmp(&(&b.name, b.kind as u8)));
    let deps: Vec<_> = deps
        .into_iter()
        .map(|d| {
            serde_json::json!({
                "name": d.spec.package.as_deref().unwrap_or(&d.name),
                "rename": d.spec.package.as_ref().map(|_| d.name.clone()),
                "source": if d.spec.path.is_some() {
                    serde_json::Value::Null
                } else if let Some(g) = &d.spec.git {
                    format!("git+{g}").into()
                } else {
                    "registry+https://github.com/rust-lang/crates.io-index".into()
                },
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
                "registry": d.spec.registry,
                "path": d.spec.path.as_ref().map(|p| p.display().to_string()),
            })
        })
        .collect();
    let pkg = m.manifest.package.as_ref();
    let text = |s: &str| (!s.is_empty()).then(|| s.to_string());
    serde_json::json!({
        "name": m.name,
        "version": m.version.to_string(),
        "id": id,
        "source": source,
        "description": text(&m.meta.description),
        "license": text(&m.meta.license),
        "license_file": text(&m.meta.license_file),
        "repository": text(&m.meta.repository),
        "homepage": text(&m.meta.homepage),
        "readme": text(&m.meta.readme),
        "rust_version": text(&m.meta.rust_version),
        "dependencies": deps,
        "targets": targets,
        "features": m.manifest.features,
        "manifest_path": m.manifest_path.display().to_string(),
        "edition": m.edition.as_str(),
        "links": pkg.and_then(|p| p.extra.get("links")).and_then(|v| v.as_str()),
        "default_run": pkg.and_then(|p| p.default_run.clone()),
        "metadata": pkg.and_then(|p| p.extra.get("metadata")).map(|v| serde_json::to_value(v).unwrap_or_default()),
        "authors": m.meta.authors.split(':').filter(|s| !s.is_empty()).collect::<Vec<_>>(),
        "categories": [],
        "keywords": [],
        "publish": serde_json::Value::Null,
    })
}

// ---------------------------------------------------------------------------
// pkgid / verify-project
// ---------------------------------------------------------------------------

/// `rune pkgid [SPEC]`: the fully qualified package id spec of a workspace
/// member or a package in the dependency graph, as Cargo prints it
/// (`path+file:///…#name@1.0.0`, `registry+https://…#serde@1.0.228`).
pub fn pkgid(args: PkgidArgs) -> Result<()> {
    let (ws, offline) = load(&args.manifest)?;
    let spec = args.spec.clone().or(args.package.clone());
    let (want_name, want_version) = match spec.as_deref().map(|s| s.split_once('@')) {
        Some(Some((n, v))) => (Some(n.to_string()), Some(v.to_string())),
        Some(None) => (spec.clone(), None),
        None => (None, None),
    };
    let path_id = |m: &crate::workspace::Member| {
        let dir_name = m.dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let url = format!("path+file://{}", m.dir.display());
        if dir_name == m.name {
            format!("{url}#{}", m.version)
        } else {
            format!("{url}#{}@{}", m.name, m.version)
        }
    };
    let Some(name) = want_name else {
        let member = match ws.current {
            Some(i) => &ws.members[i],
            None if ws.members.len() == 1 => &ws.members[0],
            None => bail!("this is a workspace: name a package (`rune pkgid NAME`)"),
        };
        println!("{}", path_id(member));
        return Ok(());
    };
    if let Some(m) = ws.members.iter().find(|m| {
        m.name == name
            && want_version
                .as_ref()
                .is_none_or(|v| m.version.to_string() == *v)
    }) {
        println!("{}", path_id(m));
        return Ok(());
    }
    let existing = LockfileManager::load_fast(ws.root());
    let (resolved, _) = graph::resolve_fresh(&ws, offline, existing.as_ref(), false)?;
    let mut hits: Vec<_> = resolved
        .packages
        .values()
        .filter(|p| {
            p.name == name
                && want_version
                    .as_ref()
                    .is_none_or(|v| p.version.to_string() == *v)
        })
        .collect();
    hits.sort_by(|a, b| a.version.cmp(&b.version));
    match hits.as_slice() {
        [] => bail!(
            "package ID specification `{}` did not match any packages",
            spec.unwrap_or_default()
        ),
        [p] => {
            let source = match &p.source {
                rune_resolver::PackageSource::Registry(index) => {
                    if index == rune_common::CRATES_IO_INDEX {
                        "registry+https://github.com/rust-lang/crates.io-index".to_string()
                    } else {
                        index.clone()
                    }
                }
                rune_resolver::PackageSource::Git { url, reference, .. } => match reference {
                    Some(q) => format!("git+{url}?{q}"),
                    None => format!("git+{url}"),
                },
                rune_resolver::PackageSource::Path(dir) => format!("path+file://{}", dir.display()),
            };
            println!("{source}#{}@{}", p.name, p.version);
            Ok(())
        }
        many => bail!(
            "there are multiple `{name}` packages in the dependency graph; pick one with `{name}@VERSION`: {}",
            many.iter()
                .map(|p| format!("{name}@{}", p.version))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// `rune verify-project`: prints `{"success":"true"}` when the manifest (and
/// its workspace) loads, else `{"invalid":"<why>"}` and exits 1.
pub fn verify_project(args: ManifestArgs) -> Result<()> {
    match Workspace::load(args.manifest_path.as_deref()) {
        Ok(_) => {
            println!("{}", serde_json::json!({ "success": "true" }));
            Ok(())
        }
        Err(e) => {
            let why = match e.chain().nth(1) {
                Some(_) => format!("{e}: {}", e.root_cause()),
                None => e.to_string(),
            };
            println!("{}", serde_json::json!({ "invalid": why.trim() }));
            std::process::exit(1);
        }
    }
}

/// Published crates whose names are close to `name` (a typo, `_` for `-`),
/// from a registry search; empty when the registry has no search or is
/// unreachable.
fn similar_crates(index_url: &str, name: &str) -> Vec<String> {
    let Ok(api) = rune_registry::api::RegistryApi::for_index(index_url) else {
        return Vec::new();
    };
    let wanted = name.to_lowercase().replace('_', "-");
    let limit = (wanted.len() / 3).clamp(1, 3);
    // Search is by words, so a typo finds nothing: try the name cut short
    // too (`serdee` → `serde`, `serd`).
    let chars: Vec<char> = name.chars().collect();
    let mut queries = vec![name.to_string()];
    for cut in 1..=2 {
        if chars.len() > cut + 2 {
            queries.push(chars[..chars.len() - cut].iter().collect());
        }
    }
    let mut close: Vec<(usize, String)> = Vec::new();
    for q in queries {
        let Ok((hits, _)) = api.search(&q, 20) else {
            return Vec::new();
        };
        for h in hits {
            let d = edit_distance(&wanted, &h.name.to_lowercase().replace('_', "-"));
            if d <= limit && !close.iter().any(|(_, n)| *n == h.name) {
                close.push((d, h.name));
            }
        }
        if close.iter().any(|(d, _)| *d <= 1) {
            break;
        }
    }
    close.sort();
    close.into_iter().take(3).map(|(_, n)| n).collect()
}

/// The candidate closest to `wanted` within a typo's distance, if any.
pub fn closest<'a>(wanted: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let wanted = wanted.to_lowercase().replace('_', "-");
    candidates
        .into_iter()
        .map(|c| {
            (
                edit_distance(&wanted, &c.to_lowercase().replace('_', "-")),
                c,
            )
        })
        .filter(|(d, c)| *d > 0 && *d <= (c.len() / 3).clamp(1, 3))
        .min()
        .map(|(_, c)| c)
}

/// Levenshtein distance.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            cur.push((prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

#[cfg(test)]
mod similar_tests {
    use super::edit_distance;

    #[test]
    fn typos_are_close() {
        assert_eq!(edit_distance("serdee", "serde"), 1);
        assert_eq!(edit_distance("anyhw", "anyhow"), 1);
        assert_eq!(edit_distance("reqwset", "reqwest"), 2);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("tokio", "tokio"), 0);
        assert_eq!(
            super::closest("rune-clii", ["rune-cas", "rune-cli"]),
            Some("rune-cli")
        );
        assert_eq!(super::closest("zzz", ["rune-cas", "rune-cli"]), None);
    }
}
