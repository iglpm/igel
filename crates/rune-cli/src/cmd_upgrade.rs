//! `rune outdated` and `rune upgrade`: which declared dependencies have newer
//! releases, and raising manifest requirements to them.
//!
//! Only what the manifests say is considered — `[dependencies]`,
//! `[dev-dependencies]`, `[build-dependencies]` (also under `[target.*]`) of
//! every member, and `[workspace.dependencies]` — never what they pull in:
//! those are the requirements a user can change. Path and git dependencies
//! have no releases to compare with.

use anyhow::{Context, Result, bail};
use colored::Colorize;
use rune_registry::IndexRecord;
use semver::{Version, VersionReq};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use toml_edit::{DocumentMut, Item, Value};

use crate::args::{OutdatedArgs, UpgradeArgs};
use crate::ui;
use crate::workspace::Workspace;

/// A registry dependency as a manifest declares it.
#[derive(Debug, Clone)]
struct Declared {
    manifest: PathBuf,
    /// Where the entry sits: `["dependencies"]`,
    /// `["target", "cfg(unix)", "dev-dependencies"]`,
    /// `["workspace", "dependencies"]`.
    table: Vec<String>,
    /// The key in that table (a rename's alias).
    key: String,
    /// The crate's name in the registry.
    package: String,
    requirement: String,
    registry: Option<String>,
    /// `normal`, `dev`, `build` or `workspace`.
    kind: &'static str,
    /// The member declaring it, or `workspace`.
    owner: String,
    /// The owner's `rust-version`, if any.
    msrv: Option<Version>,
}

/// What the registry has for one declared dependency.
#[derive(Debug, Clone, serde::Serialize)]
struct Status {
    name: String,
    requirement: String,
    kind: &'static str,
    owner: String,
    /// The newest locked version the requirement accepts.
    locked: Option<String>,
    /// The newest release the requirement accepts.
    compatible: Option<String>,
    /// The newest release (a pre-release only when there is no other), or
    /// with an MSRV, the newest the MSRV allows.
    latest: Option<String>,
    /// The newest release ignoring the MSRV, when that differs.
    latest_ignoring_msrv: Option<String>,
}

impl Status {
    fn is_outdated(&self) -> bool {
        let newer = |a: &Option<String>, b: &Option<String>| match (a, b) {
            (Some(a), Some(b)) => match (Version::parse(a), Version::parse(b)) {
                (Ok(a), Ok(b)) => b > a,
                _ => false,
            },
            _ => false,
        };
        newer(&self.locked, &self.compatible)
            || newer(&self.compatible, &self.latest)
            || self.latest_ignoring_msrv.is_some()
    }
}

/// Every registry dependency declared by the selected members (and the
/// workspace's own table when `with_workspace`).
fn declared(ws: &Workspace, members: &[usize], with_workspace: bool) -> Result<Vec<Declared>> {
    let mut out = Vec::new();
    let ws_msrv = ws
        .workspace_manifest()
        .and_then(|w| w.package.as_ref())
        .and_then(|p| p.rust_version.as_deref())
        .and_then(rune_common::toolchain::parse_rust_version);
    for &mi in members {
        let m = &ws.members[mi];
        let text = std::fs::read_to_string(&m.manifest_path)?;
        let doc: DocumentMut = text
            .parse()
            .with_context(|| format!("failed to parse {}", m.manifest_path.display()))?;
        let msrv = rune_common::toolchain::parse_rust_version(&m.meta.rust_version);
        let mut tables: Vec<(Vec<String>, &'static str, &Item)> = Vec::new();
        for (name, kind) in [
            ("dependencies", "normal"),
            ("dev-dependencies", "dev"),
            ("build-dependencies", "build"),
        ] {
            if let Some(t) = doc.get(name) {
                tables.push((vec![name.to_string()], kind, t));
            }
        }
        if let Some(targets) = doc.get("target").and_then(Item::as_table_like) {
            for (platform, t) in targets.iter() {
                for (name, kind) in [
                    ("dependencies", "normal"),
                    ("dev-dependencies", "dev"),
                    ("build-dependencies", "build"),
                ] {
                    if let Some(deps) = t.get(name) {
                        tables.push((
                            vec!["target".into(), platform.to_string(), name.into()],
                            kind,
                            deps,
                        ));
                    }
                }
            }
        }
        for (path, kind, item) in tables {
            let Some(table) = item.as_table_like() else {
                continue;
            };
            for (key, entry) in table.iter() {
                if let Some(d) = registry_entry(key, entry) {
                    out.push(Declared {
                        manifest: m.manifest_path.clone(),
                        table: path.clone(),
                        key: key.to_string(),
                        package: d.0,
                        requirement: d.1,
                        registry: d.2,
                        kind,
                        owner: m.name.clone(),
                        msrv: msrv.clone().or_else(|| ws_msrv.clone()),
                    });
                }
            }
        }
    }
    if with_workspace {
        let path = ws.manifest_path();
        let text = std::fs::read_to_string(&path)?;
        let doc: DocumentMut = text
            .parse()
            .with_context(|| format!("failed to parse {}", path.display()))?;
        if let Some(table) = doc
            .get("workspace")
            .and_then(|w| w.get("dependencies"))
            .and_then(Item::as_table_like)
        {
            for (key, entry) in table.iter() {
                if let Some(d) = registry_entry(key, entry) {
                    out.push(Declared {
                        manifest: path.clone(),
                        table: vec!["workspace".into(), "dependencies".into()],
                        key: key.to_string(),
                        package: d.0,
                        requirement: d.1,
                        registry: d.2,
                        kind: "workspace",
                        owner: "workspace".into(),
                        msrv: ws_msrv.clone(),
                    });
                }
            }
        }
    }
    Ok(out)
}

/// `(package, requirement, registry)` of a registry dependency entry; `None`
/// for path/git dependencies and `workspace = true` (whose requirement lives
/// in `[workspace.dependencies]`).
fn registry_entry(key: &str, entry: &Item) -> Option<(String, String, Option<String>)> {
    if let Some(req) = entry.as_str() {
        return Some((key.to_string(), req.to_string(), None));
    }
    let t = entry.as_table_like()?;
    if t.get("path").is_some() || t.get("git").is_some() || t.get("workspace").is_some() {
        return None;
    }
    let req = t.get("version")?.as_str()?.to_string();
    let package = t
        .get("package")
        .and_then(Item::as_str)
        .unwrap_or(key)
        .to_string();
    let registry = t.get("registry").and_then(Item::as_str).map(str::to_string);
    Some((package, req, registry))
}

/// The newest release in `versions` accepted by `pick`, preferring stable
/// ones.
fn newest<'a>(
    versions: &'a [(Version, &'a IndexRecord)],
    pick: impl Fn(&Version, &IndexRecord) -> bool,
) -> Option<&'a Version> {
    let ok: Vec<&Version> = versions
        .iter()
        .filter(|(v, r)| pick(v, r))
        .map(|(v, _)| v)
        .collect();
    ok.iter()
        .filter(|v| v.pre.is_empty())
        .max()
        .or_else(|| ok.iter().max())
        .copied()
}

fn fits_msrv(record: &IndexRecord, msrv: Option<&Version>) -> bool {
    match (msrv, record.rust_version.as_deref()) {
        (Some(msrv), Some(needs)) => {
            rune_common::toolchain::parse_rust_version(needs).is_none_or(|n| n <= *msrv)
        }
        _ => true,
    }
}

/// Looks every declared dependency up in its registry (one request per crate,
/// concurrently) and compares.
fn statuses(ws: &Workspace, deps: &[Declared], offline: bool) -> Result<Vec<Status>> {
    let defs = rune_registry::source::defs();
    let lock = rune_resolver::lockfile::LockfileManager::load_fast(ws.root());
    // Crates by registry, each looked up once.
    let mut by_registry: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for d in deps {
        let index = defs.index_for_name(d.registry.as_deref())?;
        let names = by_registry.entry(index).or_default();
        if !names.contains(&d.package) {
            names.push(d.package.clone());
        }
    }
    let mut records: HashMap<(String, String), rune_registry::CrateIndex> = HashMap::new();
    for (index, names) in &by_registry {
        let checking = ui::Activity::start(
            "checking",
            format!(
                "{} in the {} index",
                ui::plural(names.len(), "crate"),
                defs.name_of(index)
            ),
        );
        let client = rune_registry::source::IndexClient::for_registry(index, offline)?;
        let fetched = client.fetch_many_with(names, !offline);
        checking.clear();
        for (name, found) in names.iter().zip(fetched) {
            match found {
                Ok(ci) => {
                    records.insert((index.clone(), name.clone()), ci);
                }
                Err(e) => ui::warn(format!("could not look up `{name}`: {e}")),
            }
        }
    }

    let mut out = Vec::new();
    for d in deps {
        let index = defs.index_for_name(d.registry.as_deref())?;
        let Some(ci) = records.get(&(index, d.package.clone())) else {
            continue;
        };
        let req = VersionReq::parse(&d.requirement).ok();
        let versions: Vec<(Version, &IndexRecord)> = ci
            .available_versions()
            .into_iter()
            .filter_map(|r| Version::parse(&r.vers).ok().map(|v| (v, r)))
            .collect();
        let accepts = |v: &Version| req.as_ref().is_some_and(|r| r.matches(v));
        let compatible = newest(&versions, |v, r| {
            accepts(v) && fits_msrv(r, d.msrv.as_ref())
        });
        let latest = newest(&versions, |_, r| fits_msrv(r, d.msrv.as_ref()));
        let latest_any = newest(&versions, |_, _| true);
        let locked = lock.as_ref().and_then(|l| {
            l.packages
                .iter()
                .filter(|p| p.name == d.package && p.source.is_some())
                .filter_map(|p| Version::parse(&p.version).ok())
                .filter(|v| accepts(v))
                .max()
        });
        out.push(Status {
            name: if d.key == d.package {
                d.package.clone()
            } else {
                format!("{} ({})", d.key, d.package)
            },
            requirement: d.requirement.clone(),
            kind: d.kind,
            owner: d.owner.clone(),
            locked: locked.map(|v| v.to_string()),
            compatible: compatible.map(Version::to_string),
            latest: latest.map(Version::to_string),
            latest_ignoring_msrv: latest_any
                .filter(|a| Some(*a) != latest)
                .map(Version::to_string),
        });
    }
    Ok(out)
}

fn selected_members(ws: &Workspace, packages: &[String]) -> Result<Vec<usize>> {
    if packages.is_empty() {
        return Ok((0..ws.members.len()).collect());
    }
    packages
        .iter()
        .map(|p| {
            ws.member_named(p)
                .with_context(|| format!("package `{p}` is not a member of this workspace"))
        })
        .collect()
}

pub fn outdated(args: OutdatedArgs) -> Result<()> {
    let ws = Workspace::load(args.manifest.manifest_path.as_deref())?;
    let offline = args.manifest.offline || ws.config.settings.offline();
    let members = selected_members(&ws, &args.package)?;
    let deps = declared(&ws, &members, args.package.is_empty())?;
    let mut rows: Vec<Status> = statuses(&ws, &deps, offline)?
        .into_iter()
        .filter(Status::is_outdated)
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name).then(a.owner.cmp(&b.owner)));

    if args.format == "json" {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else if rows.is_empty() {
        ui::summary("every dependency is at its newest release");
    } else {
        let dash = || "-".to_string();
        let table: Vec<[String; 7]> = rows
            .iter()
            .map(|r| {
                [
                    r.name.clone(),
                    r.requirement.clone(),
                    r.locked.clone().unwrap_or_else(dash),
                    r.compatible.clone().unwrap_or_else(dash),
                    match &r.latest_ignoring_msrv {
                        Some(any) => format!(
                            "{} ({any} needs a newer Rust)",
                            r.latest.clone().unwrap_or_else(dash)
                        ),
                        None => r.latest.clone().unwrap_or_else(dash),
                    },
                    r.kind.to_string(),
                    r.owner.clone(),
                ]
            })
            .collect();
        let header = [
            "name",
            "requirement",
            "locked",
            "compatible",
            "latest",
            "kind",
            "declared by",
        ];
        let mut widths = header.map(str::len);
        for row in &table {
            for (w, cell) in widths.iter_mut().zip(row) {
                *w = (*w).max(cell.chars().count());
            }
        }
        let line = |cells: Vec<String>| {
            cells
                .iter()
                .zip(widths)
                .map(|(c, w)| format!("{c:<w$}"))
                .collect::<Vec<_>>()
                .join("  ")
                .trim_end()
                .to_string()
        };
        println!(
            "{}",
            line(header.iter().map(|h| h.to_string()).collect()).bold()
        );
        for row in table {
            println!("{}", line(row.to_vec()));
        }
        let behind_lock = rows
            .iter()
            .filter(|r| matches!((&r.locked, &r.compatible), (Some(a), Some(b)) if a != b))
            .count();
        let hints = [
            (behind_lock > 0)
                .then(|| "`rune update` moves the lockfile to the compatible releases".to_string()),
            rows.iter().any(|r| r.compatible != r.latest).then(|| {
                "`rune upgrade --incompatible` raises requirements to the latest ones".to_string()
            }),
        ];
        for h in hints.into_iter().flatten() {
            ui::hint(h);
        }
    }
    if args.exit_code && !rows.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}

/// The requirement `req` rewritten to accept `to` as its minimum, keeping its
/// operator and how many version parts it spells out (`1` stays `1`, `0.7`
/// becomes `0.8`, `1.0.100` becomes `1.0.229`). `None` when the requirement is
/// not one simple comparison (`>=1, <3`, `*`), which is left alone.
pub fn upgraded_requirement(req: &str, to: &Version) -> Option<String> {
    let req = req.trim();
    if req.contains(',') || req.contains('*') || req.contains('<') || req.contains('>') {
        return None;
    }
    let (op, rest) = match req.chars().next()? {
        '^' | '~' | '=' => (&req[..1], req[1..].trim()),
        _ => ("", req),
    };
    let (numbers, _pre) = rest.split_once('-').unwrap_or((rest, ""));
    let parts = numbers.split('.').count();
    if !(1..=3).contains(&parts) || numbers.split('.').any(|p| p.parse::<u64>().is_err()) {
        return None;
    }
    let body = match parts {
        1 => to.major.to_string(),
        2 => format!("{}.{}", to.major, to.minor),
        _ => {
            let mut v = format!("{}.{}.{}", to.major, to.minor, to.patch);
            if !to.pre.is_empty() {
                v.push('-');
                v.push_str(to.pre.as_str());
            }
            v
        }
    };
    let new = format!("{op}{body}");
    // The rewritten requirement must accept the release (a `1` for a `0.9`
    // would not).
    VersionReq::parse(&new).ok().filter(|r| r.matches(to))?;
    Some(new)
}

pub fn upgrade(args: UpgradeArgs) -> Result<()> {
    let ws = Workspace::load(args.manifest.manifest_path.as_deref())?;
    let offline = args.manifest.offline || ws.config.settings.offline();
    let members = selected_members(&ws, &args.package)?;
    let mut deps = declared(&ws, &members, args.package.is_empty())?;
    if !args.dependencies.is_empty() {
        let unknown: Vec<&String> = args
            .dependencies
            .iter()
            .filter(|n| !deps.iter().any(|d| &d.package == *n || &d.key == *n))
            .collect();
        if !unknown.is_empty() {
            bail!(
                "not a registry dependency of the selected packages: {}",
                unknown
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        deps.retain(|d| {
            args.dependencies
                .iter()
                .any(|n| *n == d.package || *n == d.key)
        });
    }
    if args.ignore_rust_version {
        for d in &mut deps {
            d.msrv = None;
        }
    }
    let status = statuses(&ws, &deps, offline)?;

    // Edits grouped by manifest, applied in one write each.
    let mut edits: BTreeMap<PathBuf, Vec<(&Declared, String)>> = BTreeMap::new();
    let mut skipped = Vec::new();
    for (d, s) in deps.iter().zip(&status) {
        let target = if args.incompatible {
            &s.latest
        } else {
            &s.compatible
        };
        let Some(to) = target.as_deref().and_then(|v| Version::parse(v).ok()) else {
            continue;
        };
        if d.requirement.trim_start().starts_with('=') && !args.pinned {
            if VersionReq::parse(&d.requirement).is_ok_and(|r| !r.matches(&to)) {
                skipped.push(format!(
                    "{} (pinned with `=`; `--pinned` upgrades it)",
                    d.key
                ));
            }
            continue;
        }
        match upgraded_requirement(&d.requirement, &to) {
            Some(new) if new != d.requirement.trim() => {
                edits.entry(d.manifest.clone()).or_default().push((d, new));
            }
            Some(_) => {}
            None => skipped.push(format!(
                "{} (`{}` is not a single version requirement)",
                d.key, d.requirement
            )),
        }
    }

    let mut changed_packages: Vec<String> = Vec::new();
    for (manifest, list) in &edits {
        let text = std::fs::read_to_string(manifest)?;
        let mut doc: DocumentMut = text.parse()?;
        for (d, new) in list {
            let mut item: &mut Item = doc.as_item_mut();
            for seg in &d.table {
                item = &mut item[seg.as_str()];
            }
            let entry = &mut item[d.key.as_str()];
            match entry {
                Item::Value(Value::String(_)) => {
                    let decor = entry.as_value().map(|v| v.decor().clone());
                    *entry = toml_edit::value(new.as_str());
                    if let (Some(decor), Some(v)) = (decor, entry.as_value_mut()) {
                        *v.decor_mut() = decor;
                    }
                }
                _ => {
                    let slot = &mut entry["version"];
                    let decor = slot.as_value().map(|v| v.decor().clone());
                    *slot = toml_edit::value(new.as_str());
                    if let (Some(decor), Some(v)) = (decor, slot.as_value_mut()) {
                        *v.decor_mut() = decor;
                    }
                }
            }
            let rel = manifest
                .strip_prefix(ws.root())
                .unwrap_or(manifest)
                .display()
                .to_string();
            ui::action(
                if args.dry_run {
                    "would upgrade"
                } else {
                    "upgrading"
                },
                format!(
                    "{} {} -> {} ({rel}, {})",
                    d.key.bold(),
                    d.requirement,
                    new,
                    d.table.join(".")
                ),
            );
            if !changed_packages.contains(&d.package) {
                changed_packages.push(d.package.clone());
            }
        }
        if !args.dry_run {
            std::fs::write(manifest, doc.to_string())?;
        }
    }
    for s in &skipped {
        ui::hint(format!("left alone: {s}"));
    }
    if edits.is_empty() {
        ui::summary(if args.incompatible {
            "every requirement already allows the latest release"
        } else {
            "every requirement already allows the newest compatible release (`--incompatible` for new major versions)"
        });
        return Ok(());
    }
    if args.dry_run {
        ui::warn("not writing the manifests (--dry-run)");
        return Ok(());
    }
    // The lockfile follows: the upgraded crates resolve again, the rest stay.
    let ws = Workspace::load(args.manifest.manifest_path.as_deref())?;
    crate::cmd_deps::refresh_lockfile(&ws, offline, false, &changed_packages)?;
    ui::summary(format!(
        "upgraded {} requirement(s)",
        edits.values().map(Vec::len).sum::<usize>()
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn requirements_keep_their_operator_and_precision() {
        assert_eq!(upgraded_requirement("1", &v("1.4.2")).as_deref(), Some("1"));
        assert_eq!(
            upgraded_requirement("1.0", &v("1.4.2")).as_deref(),
            Some("1.4")
        );
        assert_eq!(
            upgraded_requirement("1.0.100", &v("1.0.229")).as_deref(),
            Some("1.0.229")
        );
        assert_eq!(
            upgraded_requirement("0.7", &v("0.9.1")).as_deref(),
            Some("0.9")
        );
        assert_eq!(
            upgraded_requirement("^0.4.1", &v("0.4.9")).as_deref(),
            Some("^0.4.9")
        );
        assert_eq!(
            upgraded_requirement("~1.2.3", &v("1.2.9")).as_deref(),
            Some("~1.2.9")
        );
        assert_eq!(
            upgraded_requirement("=1.2.3", &v("1.3.0")).as_deref(),
            Some("=1.3.0")
        );
        assert_eq!(
            upgraded_requirement("1.0.0-alpha.1", &v("1.0.0-beta.2")).as_deref(),
            Some("1.0.0-beta.2")
        );
    }

    #[test]
    fn ranges_and_wildcards_are_left_alone() {
        assert_eq!(upgraded_requirement(">=1, <3", &v("2.5.0")), None);
        assert_eq!(upgraded_requirement("*", &v("2.5.0")), None);
        assert_eq!(upgraded_requirement("1.*", &v("1.5.0")), None);
    }

    #[test]
    fn a_rewrite_that_would_not_accept_the_release_is_refused() {
        // `~1` accepts 1.x only; fine for 1.9, and `~2` for 2.1.
        assert_eq!(
            upgraded_requirement("~1", &v("1.9.0")).as_deref(),
            Some("~1")
        );
        assert_eq!(
            upgraded_requirement("~1", &v("2.1.0")).as_deref(),
            Some("~2")
        );
    }
}
