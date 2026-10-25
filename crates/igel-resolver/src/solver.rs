//! PubGrub solver for the conflicts the resolution walk can't settle by itself: one package per crate
//! and compatible range.

use crate::resolver::{DependencyResolver, PackageSource, ResolvedGraph};
use pubgrub::{
    DefaultStringReporter, Dependencies, DependencyConstraints, DependencyProvider,
    PackageResolutionStatistics, PubGrubError, Ranges, Reporter,
};
use semver::{Version, VersionReq};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::fmt;
use std::str::FromStr;

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Pkg {
    Root,
    Crate {
        registry: String,
        name: String,
        class: (u64, u64, u64),
    },
    Choice {
        registry: String,
        name: String,
        req: String,
    },
}

impl fmt::Display for Pkg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Pkg::Root => f.write_str("the workspace"),
            Pkg::Crate { name, .. } | Pkg::Choice { name, .. } => f.write_str(name),
        }
    }
}

pub(crate) fn compat_class(v: &Version) -> (u64, u64, u64) {
    match (v.major, v.minor) {
        (0, 0) => (0, 0, v.patch),
        (0, m) => (0, m, 0),
        (m, _) => (m, 0, 0),
    }
}

pub(crate) struct Requirement<'a> {
    pub from: Option<&'a str>,
    pub registry: &'a str,
    pub name: &'a str,
    pub req: &'a VersionReq,
}

struct Provider<'r> {
    resolver: RefCell<&'r mut DependencyResolver>,
    root: Vec<(String, String, VersionReq)>,
    active: HashMap<(String, String), HashSet<String>>,
    current: HashMap<(String, String), Vec<Version>>,
}

impl Provider<'_> {
    fn versions(&self, registry: &str, name: &str) -> Vec<Version> {
        let mut r = self.resolver.borrow_mut();
        let Ok(index) = r.index_for(registry, name) else {
            return Vec::new();
        };
        let mut v: Vec<Version> = index
            .available_versions()
            .into_iter()
            .filter_map(|rec| Version::parse(&rec.vers).ok())
            .collect();
        v.sort_by(|a, b| b.cmp(a));
        v
    }

    fn constraint(&self, registry: &str, name: &str, req: &VersionReq) -> (Pkg, Ranges<Version>) {
        let matching: Vec<Version> = self
            .versions(registry, name)
            .into_iter()
            .filter(|v| req.matches(v))
            .collect();
        let classes: HashSet<(u64, u64, u64)> = matching.iter().map(compat_class).collect();
        if classes.len() > 1 {
            let range = matching
                .into_iter()
                .fold(Ranges::empty(), |acc, v| acc.union(&Ranges::singleton(v)));
            return (
                Pkg::Choice {
                    registry: registry.to_string(),
                    name: name.to_string(),
                    req: req.to_string(),
                },
                range,
            );
        }
        let class = match matching.first() {
            Some(v) => compat_class(v),
            None => req
                .comparators
                .first()
                .map(|c| {
                    compat_class(&Version::new(
                        c.major,
                        c.minor.unwrap_or(0),
                        c.patch.unwrap_or(0),
                    ))
                })
                .unwrap_or((0, 0, 0)),
        };
        let range = matching
            .into_iter()
            .filter(|v| compat_class(v) == class)
            .fold(Ranges::empty(), |acc, v| acc.union(&Ranges::singleton(v)));
        (
            Pkg::Crate {
                registry: registry.to_string(),
                name: name.to_string(),
                class,
            },
            range,
        )
    }
}

impl DependencyProvider for Provider<'_> {
    type P = Pkg;
    type V = Version;
    type VS = Ranges<Version>;
    type M = String;
    type Err = Infallible;
    type Priority = (u32, std::cmp::Reverse<usize>);

    fn prioritize(
        &self,
        package: &Pkg,
        range: &Ranges<Version>,
        stats: &PackageResolutionStatistics,
    ) -> Self::Priority {
        let choices = match package {
            Pkg::Root => 1,
            Pkg::Crate { registry, name, .. } | Pkg::Choice { registry, name, .. } => self
                .versions(registry, name)
                .iter()
                .filter(|v| range.contains(v))
                .count(),
        };
        (stats.conflict_count(), std::cmp::Reverse(choices))
    }

    fn choose_version(
        &self,
        package: &Pkg,
        range: &Ranges<Version>,
    ) -> Result<Option<Version>, Infallible> {
        let (Pkg::Crate { registry, name, .. } | Pkg::Choice { registry, name, .. }) = package
        else {
            return Ok(Some(Version::new(0, 0, 0)));
        };
        let key = (registry.clone(), name.clone());
        if let Some(v) = self
            .current
            .get(&key)
            .and_then(|vs| vs.iter().find(|v| range.contains(v)))
        {
            return Ok(Some(v.clone()));
        }
        Ok(self
            .versions(registry, name)
            .into_iter()
            .find(|v| range.contains(v)))
    }

    fn get_dependencies(
        &self,
        package: &Pkg,
        version: &Version,
    ) -> Result<Dependencies<Pkg, Ranges<Version>, String>, Infallible> {
        let wanted: Vec<(String, String, VersionReq)> = match package {
            Pkg::Root => self.root.clone(),
            Pkg::Choice { registry, name, .. } => {
                let crate_pkg = Pkg::Crate {
                    registry: registry.clone(),
                    name: name.clone(),
                    class: compat_class(version),
                };
                return Ok(Dependencies::Available(
                    [(crate_pkg, Ranges::singleton(version.clone()))]
                        .into_iter()
                        .collect(),
                ));
            }
            Pkg::Crate { registry, name, .. } => {
                let active = self.active.get(&(registry.clone(), name.clone()));
                let mut r = self.resolver.borrow_mut();
                let Ok(index) = r.index_for(registry, name) else {
                    return Ok(Dependencies::Unavailable(format!(
                        "the index entry of {name} could not be read"
                    )));
                };
                let Some(record) = index.get_version(&version.to_string()) else {
                    return Ok(Dependencies::Unavailable(format!(
                        "{name} {version} is gone"
                    )));
                };
                record
                    .deps
                    .iter()
                    .filter(|d| d.kind.as_deref() != Some("dev"))
                    .filter_map(|d| {
                        let package = d.package.clone().unwrap_or_else(|| d.name.clone());
                        if d.optional && !active.is_some_and(|a| a.contains(&package)) {
                            return None;
                        }
                        let dep_registry = d
                            .registry
                            .as_deref()
                            .map(crate::resolver::normalize_registry_pub)
                            .unwrap_or_else(|| registry.clone());
                        Some((
                            dep_registry,
                            package,
                            VersionReq::from_str(&d.req).unwrap_or(VersionReq::STAR),
                        ))
                    })
                    .collect()
            }
        };
        let mut deps: HashMap<Pkg, Ranges<Version>> = HashMap::new();
        for (registry, name, req) in wanted {
            let (pkg, range) = self.constraint(&registry, &name, &req);
            let range = match deps.remove(&pkg) {
                Some(r) => r.intersection(&range),
                None => range,
            };
            deps.insert(pkg, range);
        }
        Ok(Dependencies::Available(
            deps.into_iter().collect::<DependencyConstraints<_, _>>(),
        ))
    }
}

pub(crate) enum Solution {
    Found(Vec<(String, String, Version)>),
    Impossible(String),
}

pub(crate) fn solve(
    resolver: &mut DependencyResolver,
    graph: &ResolvedGraph,
    requirements: &[Requirement<'_>],
) -> Solution {
    let registry_pkg = |id: &str| {
        graph.packages.get(id).and_then(|p| match &p.source {
            PackageSource::Registry(r) => Some((r.clone(), p.name.clone())),
            _ => None,
        })
    };
    let mut root = Vec::new();
    let mut active: HashMap<(String, String), HashSet<String>> = HashMap::new();
    for r in requirements {
        match r.from.and_then(registry_pkg) {
            Some(from) => {
                active.entry(from).or_default().insert(r.name.to_string());
            }
            None => root.push((r.registry.to_string(), r.name.to_string(), r.req.clone())),
        }
    }
    let mut current: HashMap<(String, String), Vec<Version>> = HashMap::new();
    for p in graph.packages.values() {
        if let PackageSource::Registry(r) = &p.source {
            current
                .entry((r.clone(), p.name.clone()))
                .or_default()
                .push(p.version.clone());
        }
    }
    for vs in current.values_mut() {
        vs.sort_by(|a, b| b.cmp(a));
    }
    let provider = Provider {
        resolver: RefCell::new(resolver),
        root,
        active,
        current,
    };
    match pubgrub::resolve(&provider, Pkg::Root, Version::new(0, 0, 0)) {
        Ok(chosen) => Solution::Found(
            chosen
                .into_iter()
                .filter_map(|(p, v)| match p {
                    Pkg::Crate { registry, name, .. } => Some((registry, name, v)),
                    Pkg::Root | Pkg::Choice { .. } => None,
                })
                .collect(),
        ),
        Err(PubGrubError::NoSolution(mut tree)) => {
            tree.collapse_no_versions();
            let report = DefaultStringReporter::report(&tree)
                .replace("the workspace 0.0.0", "the workspace")
                .replace(
                    "the workspace is forbidden",
                    "the workspace cannot be resolved",
                );
            Solution::Impossible(report)
        }
        Err(e) => Solution::Impossible(e.to_string()),
    }
}
