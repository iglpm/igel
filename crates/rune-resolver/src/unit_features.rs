//! Per-build feature resolution, following Cargo's *feature resolver*.
//!
//! Version resolution (`resolver.rs`) decides which package versions a
//! workspace uses and is independent of the command line: like `Cargo.lock`,
//! it covers every platform and every feature. What a *particular build*
//! compiles depends on the selection, and that is decided here:
//!
//! * only the selected packages' requested features count (`-p a` no longer
//!   enables features that only `b` asked a shared dependency for),
//! * with resolver 2/3, a package built for the **host** (build scripts,
//!   proc-macros, and everything only they use) gets its own feature set,
//!   separate from the same package built for the **target**,
//! * dependencies gated on `[target.'cfg(..)']` count only for the platform
//!   they are built for,
//! * dev-dependency features count only when dev targets are being built.
//!
//! The result names every *(package, role)* pair to compile, its features, and
//! the dependency edges between them — the skeleton of the unit graph.

use rune_manifest::dependency::DependencyKind;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

/// One dependency declaration of a package.
#[derive(Debug, Clone)]
pub struct DepDecl {
    /// The name the package uses for the dependency: the `[dependencies]`
    /// key, and the spelling used in `[features]` (`dep:alias`, `alias/feat`).
    pub alias: String,
    /// Declared with `package = "..."`: the alias is then the `--extern` name.
    pub renamed: bool,
    pub kind: DependencyKind,
    /// `[target.'cfg(unix)'.dependencies]` or a bare triple.
    pub target: Option<String>,
    pub optional: bool,
    pub features: Vec<String>,
    pub default_features: bool,
    /// Index of the resolved package (into the slice given to [`resolve`]), or
    /// `None` when the dependency cannot be satisfied (it is then skipped).
    pub resolved: Option<usize>,
}

/// What the feature resolver needs to know about one resolved package.
#[derive(Debug, Clone, Default)]
pub struct PackageView {
    pub name: String,
    /// Declared `[features]`.
    pub features: HashMap<String, Vec<String>>,
    pub deps: Vec<DepDecl>,
    /// The package's library is a proc-macro: always built for the host.
    pub proc_macro: bool,
}

/// What a root package (a selected workspace member) asks for.
#[derive(Debug, Clone, Default)]
pub struct RootRequest {
    pub package: usize,
    /// Feature strings in `[features]` syntax: `a`, `dep:x`, `x/f`, `x?/f`.
    pub features: Vec<String>,
    pub default_features: bool,
    /// Dev-dependencies are in play for this root (tests/examples/benches).
    pub dev: bool,
}

/// How the graph is being built.
pub struct FeatureOptions<'a> {
    /// `1`, `2` or `3` (`3` behaves like `2` here).
    pub resolver: u8,
    /// Whether a target-gated dependency applies to the platform a unit is
    /// built for (`true` = the host platform).
    pub platform_matches: &'a dyn Fn(&str, bool) -> bool,
    /// Lockfile mode: a weak dependency feature (`dep?/feat`) also puts the
    /// dependency into the graph. Cargo's lockfile does this — a package only
    /// weakly referenced still gets locked — while a build does not compile it.
    pub weak_activates: bool,
}

/// A package built for one role.
pub type UnitRef = (usize, bool);

/// A dependency edge between two roles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub from: UnitRef,
    pub to: UnitRef,
    /// Index into `PackageView::deps` of `from`.
    pub dep: usize,
}

/// The outcome of feature resolution.
#[derive(Debug, Default)]
pub struct FeatureResolution {
    /// Features per (package, for_host), sorted.
    pub features: BTreeMap<UnitRef, Vec<String>>,
    pub edges: Vec<Edge>,
    /// `(package, dependency package, feature)`: a feature a package asked a
    /// dependency for that the dependency does not have.
    pub unknown_features: Vec<(usize, usize, String)>,
    /// `(unit, index into its PackageView::deps)`: a dependency the build
    /// needs (not optional, or switched on, for this platform) that the
    /// resolution has no package for. Building anyway would only end in
    /// rustc's "can't find crate".
    pub unresolved: Vec<(UnitRef, usize)>,
}

#[derive(Default)]
struct State {
    enabled: BTreeSet<String>,
    default_requested: bool,
    /// Optional dependencies switched on (aliases).
    activated: BTreeSet<String>,
    /// Features requested on a dependency by this unit's own `[features]`.
    dep_features: BTreeMap<String, BTreeSet<String>>,
}

/// Runs the feature resolver.
pub fn resolve(
    pkgs: &[PackageView],
    roots: &[RootRequest],
    opts: &FeatureOptions<'_>,
) -> FeatureResolution {
    let unify_host = opts.resolver >= 2;
    let role = |host: bool| host && unify_host;

    let mut states: BTreeMap<UnitRef, State> = BTreeMap::new();
    let mut queue: VecDeque<UnitRef> = VecDeque::new();
    let mut edges: BTreeSet<(UnitRef, UnitRef, usize)> = BTreeSet::new();
    let mut unknown: BTreeSet<(usize, usize, String)> = BTreeSet::new();
    let mut unresolved: BTreeSet<(UnitRef, usize)> = BTreeSet::new();
    let has_feature = |p: &PackageView, f: &str| {
        f == "default"
            || f.contains('/')
            || f.starts_with("dep:")
            || p.features.contains_key(f)
            || p.deps.iter().any(|d| d.optional && d.alias == f)
    };

    // Roots.
    let root_pkgs: BTreeSet<usize> = roots.iter().map(|r| r.package).collect();
    let dev_roots: BTreeSet<usize> = roots.iter().filter(|r| r.dev).map(|r| r.package).collect();
    for root in roots {
        let unit = (root.package, false);
        let st = states.entry(unit).or_default();
        st.default_requested |= root.default_features;
        for f in &root.features {
            request(st, f);
        }
        queue.push_back(unit);
    }

    // Fixed point: expanding a unit's features may activate dependencies,
    // which request features on their targets, and so on. Every step only
    // adds to monotone sets over a finite universe, so this terminates.
    while let Some(unit) = queue.pop_front() {
        let (pi, host) = unit;
        let pkg = &pkgs[pi];

        // 1. Expand this unit's own enabled features to a fixed point.
        expand(
            pkg,
            states.get_mut(&unit).expect("queued unit has state"),
            opts.weak_activates,
        );

        // 2. Walk its dependencies.
        let (activated, dep_features) = {
            let st = &states[&unit];
            (st.activated.clone(), st.dep_features.clone())
        };
        for (di, dep) in pkg.deps.iter().enumerate() {
            if dep.optional && !activated.contains(&dep.alias) {
                continue;
            }
            // Dev-dependencies belong to roots. Resolver 2 counts them only
            // when dev targets are built; resolver 1 unifies their features
            // for every selected root regardless, as Cargo does.
            if dep.kind == DependencyKind::Dev {
                let counts = !host
                    && if opts.resolver >= 2 {
                        dev_roots.contains(&pi)
                    } else {
                        root_pkgs.contains(&pi)
                    };
                if !counts {
                    continue;
                }
            }
            if let Some(t) = &dep.target
                && !(opts.platform_matches)(t, host)
            {
                continue;
            }
            // Only now, when the build would follow it, does a dependency
            // without a resolved package matter (an optional one nothing
            // switches on is routinely left out of the lockfile).
            let Some(target_pkg) = dep.resolved else {
                unresolved.insert((unit, di));
                continue;
            };
            let child_host =
                role(host || dep.kind == DependencyKind::Build || pkgs[target_pkg].proc_macro);
            let child = (target_pkg, child_host);
            edges.insert((unit, child, di));

            let is_new = !states.contains_key(&child);
            let child_state = states.entry(child).or_default();
            let before = signature(child_state);
            if dep.default_features {
                child_state.default_requested = true;
            }
            for f in &dep.features {
                request(child_state, f);
            }
            if let Some(extra) = dep_features.get(&dep.alias) {
                for f in extra {
                    request(child_state, f);
                }
            }
            for f in dep
                .features
                .iter()
                .chain(dep_features.get(&dep.alias).into_iter().flatten())
            {
                if !has_feature(&pkgs[target_pkg], f) {
                    unknown.insert((pi, target_pkg, f.clone()));
                }
            }
            if is_new || signature(child_state) != before {
                queue.push_back(child);
            }
        }
    }

    let features = states
        .into_iter()
        .map(|(unit, st)| {
            // `dep:x` and `x/f` are instructions, not features.
            let names: Vec<String> = st
                .enabled
                .into_iter()
                .filter(|f| !f.starts_with("dep:") && !f.contains('/'))
                .collect();
            (unit, names)
        })
        .collect();
    FeatureResolution {
        features,
        unknown_features: unknown.into_iter().collect(),
        unresolved: unresolved.into_iter().collect(),
        edges: edges
            .into_iter()
            .map(|(from, to, dep)| Edge { from, to, dep })
            .collect(),
    }
}

fn signature(st: &State) -> (usize, bool, usize, usize) {
    (
        st.enabled.len(),
        st.default_requested,
        st.activated.len(),
        st.dep_features.values().map(BTreeSet::len).sum(),
    )
}

/// Records a requested feature string on a unit. `x/f` and `x?/f` requests
/// (from `--features dep/feat` or a dependent's `features = [...]`) are kept
/// as feature strings and split during expansion.
fn request(st: &mut State, feature: &str) {
    st.enabled.insert(feature.to_string());
}

/// Expands `st.enabled` (which may contain `dep:x` / `x/f` strings) through
/// the package's `[features]` table until nothing changes, filling `activated`
/// and `dep_features`.
fn expand(pkg: &PackageView, st: &mut State, weak_activates: bool) {
    // Which optional dependencies are only reachable through `dep:` syntax.
    let explicit_dep_syntax: BTreeSet<&str> = pkg
        .features
        .values()
        .flatten()
        .filter_map(|s| s.strip_prefix("dep:"))
        .collect();
    let optional: BTreeSet<&str> = pkg
        .deps
        .iter()
        .filter(|d| d.optional)
        .map(|d| d.alias.as_str())
        .collect();

    if st.default_requested && pkg.features.contains_key("default") {
        st.enabled.insert("default".to_string());
    }

    let mut done: BTreeSet<String> = BTreeSet::new();
    loop {
        let todo: Vec<String> = st.enabled.difference(&done).cloned().collect();
        if todo.is_empty() {
            break;
        }
        for feature in todo {
            done.insert(feature.clone());

            // Strings that came from a request rather than the table.
            if let Some(dep) = feature.strip_prefix("dep:") {
                st.activated.insert(dep.to_string());
                continue;
            }
            if let Some((dep, sub)) = feature.split_once('/') {
                apply_dep_feature(st, &optional, dep, sub, weak_activates);
                continue;
            }

            // An optional dependency is an implicit feature unless the table
            // uses `dep:name` for it.
            if optional.contains(feature.as_str())
                && !explicit_dep_syntax.contains(feature.as_str())
            {
                st.activated.insert(feature.clone());
            }
            if let Some(subs) = pkg.features.get(&feature) {
                for sub in subs {
                    st.enabled.insert(sub.clone());
                }
            }
        }
    }
}

fn apply_dep_feature(
    st: &mut State,
    optional: &BTreeSet<&str>,
    dep: &str,
    sub: &str,
    weak_activates: bool,
) {
    let (dep, weak) = match dep.strip_suffix('?') {
        Some(d) => (d, true),
        None => (dep, false),
    };
    if (!weak || weak_activates) && optional.contains(dep) {
        st.activated.insert(dep.to_string());
    }
    st.dep_features
        .entry(dep.to_string())
        .or_default()
        .insert(sub.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dep(alias: &str, resolved: usize) -> DepDecl {
        DepDecl {
            alias: alias.into(),
            renamed: false,
            kind: DependencyKind::Normal,
            target: None,
            optional: false,
            features: vec![],
            default_features: true,
            resolved: Some(resolved),
        }
    }

    fn pkg(name: &str, feats: &[(&str, &[&str])], deps: Vec<DepDecl>) -> PackageView {
        PackageView {
            name: name.into(),
            features: feats
                .iter()
                .map(|(k, v)| (k.to_string(), v.iter().map(|s| s.to_string()).collect()))
                .collect(),
            deps,
            proc_macro: false,
        }
    }

    fn run(pkgs: &[PackageView], roots: &[RootRequest], resolver: u8) -> FeatureResolution {
        resolve(
            pkgs,
            roots,
            &FeatureOptions {
                resolver,
                platform_matches: &|t, _host| t.contains("unix"),
                weak_activates: false,
            },
        )
    }

    fn root(package: usize) -> RootRequest {
        RootRequest {
            package,
            default_features: true,
            ..Default::default()
        }
    }

    fn feats(r: &FeatureResolution, p: usize, host: bool) -> Vec<String> {
        r.features.get(&(p, host)).cloned().unwrap_or_default()
    }

    #[test]
    fn only_selected_roots_contribute_features() {
        // shared has `x`; `a` asks for it, `b` does not. Selecting only `b`
        // must not turn `x` on.
        let mut a_dep = dep("shared", 2);
        a_dep.features = vec!["x".into()];
        let pkgs = vec![
            pkg("a", &[], vec![a_dep]),
            pkg("b", &[], vec![dep("shared", 2)]),
            pkg("shared", &[("x", &[])], vec![]),
        ];
        let only_b = run(&pkgs, &[root(1)], 2);
        assert_eq!(feats(&only_b, 2, false), Vec::<String>::new());
        let both = run(&pkgs, &[root(0), root(1)], 2);
        assert_eq!(feats(&both, 2, false), ["x"]);
    }

    #[test]
    fn build_dependencies_get_their_own_features_under_resolver_2() {
        // `app` uses `dep` normally (no features) and as a build-dependency
        // with `extra`.
        let mut build = dep("dep", 1);
        build.kind = DependencyKind::Build;
        build.features = vec!["extra".into()];
        let pkgs = vec![
            pkg("app", &[], vec![dep("dep", 1), build]),
            pkg("dep", &[("extra", &[])], vec![]),
        ];
        let v2 = run(&pkgs, &[root(0)], 2);
        assert_eq!(feats(&v2, 1, false), Vec::<String>::new());
        assert_eq!(feats(&v2, 1, true), ["extra"]);
        // Resolver 1 unifies them: one universe.
        let v1 = run(&pkgs, &[root(0)], 1);
        assert_eq!(feats(&v1, 1, false), ["extra"]);
        assert!(!v1.features.contains_key(&(1, true)));
    }

    #[test]
    fn proc_macros_and_everything_below_them_are_host() {
        let mut pm = pkg("pm", &[], vec![dep("helper", 2)]);
        pm.proc_macro = true;
        let pkgs = vec![
            pkg("app", &[], vec![dep("pm", 1)]),
            pm,
            pkg("helper", &[], vec![]),
        ];
        let r = run(&pkgs, &[root(0)], 2);
        assert!(r.features.contains_key(&(1, true)));
        assert!(r.features.contains_key(&(2, true)));
        assert!(!r.features.contains_key(&(1, false)));
        assert!(!r.features.contains_key(&(2, false)));
    }

    #[test]
    fn optional_dependencies_default_features_and_dep_syntax() {
        let mut opt = dep("serde", 1);
        opt.optional = true;
        let mut modern = dep("tokio", 2);
        modern.optional = true;
        modern.default_features = false;
        let pkgs = vec![
            pkg(
                "lib",
                &[
                    ("default", &["std"]),
                    ("std", &[]),
                    // `dep:tokio` keeps the implicit `tokio` feature away.
                    ("async", &["dep:tokio", "tokio/rt"]),
                ],
                vec![opt, modern],
            ),
            pkg(
                "serde",
                &[("default", &["derive"]), ("derive", &[])],
                vec![],
            ),
            pkg("tokio", &[("rt", &[])], vec![]),
        ];
        // Defaults only: the implicit `serde` feature is NOT on.
        let base = run(&pkgs, &[root(0)], 2);
        assert_eq!(feats(&base, 0, false), ["default", "std"]);
        assert!(!base.features.contains_key(&(1, false)));
        assert!(!base.features.contains_key(&(2, false)));

        // Turning on `serde` (implicit) pulls it in with its defaults.
        let with_serde = run(
            &pkgs,
            &[RootRequest {
                features: vec!["serde".into()],
                ..root(0)
            }],
            2,
        );
        assert_eq!(feats(&with_serde, 1, false), ["default", "derive"]);

        // `async` activates tokio without defaults and enables `rt`.
        let with_async = run(
            &pkgs,
            &[RootRequest {
                features: vec!["async".into()],
                ..root(0)
            }],
            2,
        );
        assert_eq!(feats(&with_async, 2, false), ["rt"]);
    }

    #[test]
    fn weak_dependency_features_do_not_activate() {
        let mut opt = dep("x", 1);
        opt.optional = true;
        let pkgs = vec![
            pkg("a", &[("f", &["x?/deep"])], vec![opt]),
            pkg("x", &[("deep", &[])], vec![]),
        ];
        let r = run(
            &pkgs,
            &[RootRequest {
                features: vec!["f".into()],
                ..root(0)
            }],
            2,
        );
        assert!(
            !r.features.contains_key(&(1, false)),
            "`x?/deep` must not activate x"
        );
    }

    #[test]
    fn lock_mode_counts_weak_features_for_presence() {
        let mut opt = dep("x", 1);
        opt.optional = true;
        let pkgs = vec![
            pkg("a", &[("f", &["x?/deep"])], vec![opt]),
            pkg("x", &[("deep", &[])], vec![]),
        ];
        let roots = [RootRequest {
            features: vec!["f".into()],
            ..root(0)
        }];
        let lock_mode = resolve(
            &pkgs,
            &roots,
            &FeatureOptions {
                resolver: 1,
                platform_matches: &|_, _| true,
                weak_activates: true,
            },
        );
        assert!(
            lock_mode.features.contains_key(&(1, false)),
            "a locked graph includes weakly referenced packages"
        );
    }

    #[test]
    fn cli_style_requests_reach_dependencies() {
        // `--features shared/x` on a root that depends on `shared`.
        let pkgs = vec![
            pkg("app", &[], vec![dep("shared", 1)]),
            pkg("shared", &[("x", &[])], vec![]),
        ];
        let r = run(
            &pkgs,
            &[RootRequest {
                features: vec!["shared/x".into()],
                ..root(0)
            }],
            2,
        );
        assert_eq!(feats(&r, 1, false), ["x"]);
    }

    #[test]
    fn platform_gated_and_dev_dependencies() {
        let mut win = dep("winonly", 1);
        win.target = Some("cfg(windows)".into());
        let mut unix = dep("unixonly", 2);
        unix.target = Some("cfg(unix)".into());
        let mut devd = dep("devd", 3);
        devd.kind = DependencyKind::Dev;
        let pkgs = vec![
            pkg("app", &[], vec![win, unix, devd]),
            pkg("winonly", &[], vec![]),
            pkg("unixonly", &[], vec![]),
            pkg("devd", &[], vec![]),
        ];
        let r = run(&pkgs, &[root(0)], 2);
        assert!(!r.features.contains_key(&(1, false)));
        assert!(r.features.contains_key(&(2, false)));
        assert!(
            !r.features.contains_key(&(3, false)),
            "dev deps need a dev root"
        );

        let dev = run(
            &pkgs,
            &[RootRequest {
                dev: true,
                ..root(0)
            }],
            2,
        );
        assert!(dev.features.contains_key(&(3, false)));
        assert!(dev.edges.iter().any(|e| e.to == (3, false)));
    }

    #[test]
    fn cycles_terminate() {
        // Dev-dependency cycles (a dev-depends on b, b depends on a) are legal.
        let mut d = dep("b", 1);
        d.kind = DependencyKind::Dev;
        let pkgs = vec![pkg("a", &[], vec![d]), pkg("b", &[], vec![dep("a", 0)])];
        let r = run(
            &pkgs,
            &[RootRequest {
                dev: true,
                ..root(0)
            }],
            2,
        );
        assert!(r.features.contains_key(&(1, false)));
    }
}
