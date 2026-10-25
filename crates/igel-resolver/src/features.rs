use std::collections::{BTreeSet, HashMap, HashSet};

#[derive(Debug, Clone, Default)]
pub struct DynamicFeatureResolver {
    declared_features: HashMap<String, HashMap<String, Vec<String>>>,
    requested_features: HashMap<String, BTreeSet<String>>,
    use_default_features: HashMap<String, bool>,
    /// Local dependency aliases (`package = "..."` renames): `[features]` spells activations with the alias.
    package_aliases: HashMap<String, HashMap<String, String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResolvedPackageFeatures {
    pub package_name: String,
    pub active_features: Vec<String>,
    pub activated_dependencies: Vec<String>,
    pub cfg_flags: Vec<String>,
}

impl DynamicFeatureResolver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_package_features(
        &mut self,
        package_name: &str,
        features: HashMap<String, Vec<String>>,
    ) {
        self.declared_features
            .insert(package_name.to_string(), features);
    }

    pub fn register_package_aliases(
        &mut self,
        package_name: &str,
        aliases: HashMap<String, String>,
    ) {
        if aliases.is_empty() {
            return;
        }
        self.package_aliases
            .entry(package_name.to_string())
            .or_default()
            .extend(aliases);
    }

    fn resolve_alias(&self, pkg: &str, dep_name: &str) -> String {
        let bare_pkg = pkg.split_whitespace().next().unwrap_or(pkg);
        self.package_aliases
            .get(pkg)
            .or_else(|| self.package_aliases.get(bare_pkg))
            .and_then(|aliases| aliases.get(dep_name))
            .cloned()
            .unwrap_or_else(|| dep_name.to_string())
    }

    pub fn request_features(
        &mut self,
        package_name: &str,
        features: &[String],
        default_features: bool,
    ) {
        let entry = self
            .requested_features
            .entry(package_name.to_string())
            .or_default();
        for feat in features {
            entry.insert(feat.clone());
        }

        let def_entry = self
            .use_default_features
            .entry(package_name.to_string())
            .or_insert(false);
        if default_features {
            *def_entry = true;
        }
    }

    pub fn resolve(&self) -> HashMap<String, ResolvedPackageFeatures> {
        let mut unified: HashMap<String, BTreeSet<String>> = HashMap::new();
        let mut activated_deps: HashMap<String, BTreeSet<String>> = HashMap::new();

        let mut by_bare: HashMap<&str, Vec<&String>> = HashMap::new();
        for key in self.declared_features.keys() {
            by_bare.entry(bare_name(key)).or_default().push(key);
        }
        for keys in by_bare.values_mut() {
            keys.sort(); // deterministic choice when several versions exist
        }

        for (pkg, reqs) in &self.requested_features {
            let matching_keys: Vec<&String> = by_bare
                .get(pkg.as_str())
                .map(|keys| keys.to_vec())
                .unwrap_or_default();

            let target_pkgs: Vec<String> = if matching_keys.is_empty() {
                vec![pkg.clone()]
            } else {
                matching_keys.into_iter().cloned().collect()
            };

            for target_pkg in target_pkgs {
                let active = unified.entry(target_pkg).or_default();
                for req in reqs {
                    active.insert(req.clone());
                }
            }
        }

        for (target_pkg, decl) in &self.declared_features {
            let prefix = bare_name(target_pkg);
            let use_default = self
                .use_default_features
                .get(target_pkg)
                .or_else(|| self.use_default_features.get(prefix))
                .copied()
                .unwrap_or(true);

            if use_default && decl.contains_key("default") {
                let active = unified.entry(target_pkg.clone()).or_default();
                // `default` is itself a feature (crates may declare `default = []` to detect it).
                active.insert("default".to_string());
                for d in &decl["default"] {
                    active.insert(d.clone());
                }
            }
        }

        let mut done: HashSet<(String, String)> = HashSet::new();
        loop {
            let mut work: Vec<(String, Vec<String>)> = Vec::new();
            for (pkg, active) in &unified {
                let fresh: Vec<String> = active
                    .iter()
                    .filter(|f| !done.contains(&(pkg.clone(), (*f).clone())))
                    .cloned()
                    .collect();
                if !fresh.is_empty() {
                    work.push((pkg.clone(), fresh));
                }
            }
            if work.is_empty() {
                break;
            }
            work.sort_by(|a, b| a.0.cmp(&b.0));

            for (pkg, feats) in work {
                for feat in &feats {
                    done.insert((pkg.clone(), feat.clone()));
                }
                let bare_pkg = bare_name(&pkg).to_string();
                let decl_opt = self.declared_features.get(&pkg).or_else(|| {
                    self.declared_features.get(bare_pkg.as_str()).or_else(|| {
                        by_bare
                            .get(bare_pkg.as_str())
                            .and_then(|keys| keys.first())
                            .and_then(|k| self.declared_features.get(*k))
                    })
                });
                let Some(decl) = decl_opt else { continue };

                for feat in &feats {
                    activated_deps
                        .entry(pkg.clone())
                        .or_default()
                        .insert(feat.clone());
                    if bare_pkg != pkg {
                        activated_deps
                            .entry(bare_pkg.clone())
                            .or_default()
                            .insert(feat.clone());
                    }

                    let Some(sub_features) = decl.get(feat) else {
                        continue;
                    };
                    for sub in sub_features {
                        if let Some(dep_pkg) = sub.strip_prefix("dep:") {
                            activated_deps
                                .entry(pkg.clone())
                                .or_default()
                                .insert(dep_pkg.to_string());
                            if bare_pkg != pkg {
                                activated_deps
                                    .entry(bare_pkg.clone())
                                    .or_default()
                                    .insert(dep_pkg.to_string());
                            }
                        } else if let Some((dep_pkg, dep_feat)) = sub.split_once('/') {
                            let clean_dep = dep_pkg.strip_suffix('?').unwrap_or(dep_pkg);
                            activated_deps
                                .entry(pkg.clone())
                                .or_default()
                                .insert(clean_dep.to_string());

                            let real_dep = self.resolve_alias(&pkg, clean_dep);

                            unified
                                .entry(real_dep.clone())
                                .or_default()
                                .insert(dep_feat.to_string());
                            if let Some(keys) = by_bare.get(real_dep.as_str()) {
                                for target_key in keys {
                                    unified
                                        .entry((*target_key).clone())
                                        .or_default()
                                        .insert(dep_feat.to_string());
                                }
                            }
                        } else {
                            unified.entry(pkg.clone()).or_default().insert(sub.clone());
                            if bare_pkg != pkg {
                                unified
                                    .entry(bare_pkg.clone())
                                    .or_default()
                                    .insert(sub.clone());
                            }
                        }
                    }
                }
            }
        }

        // Key order: with several versions, the bare-name entry must not depend on `HashMap` order.
        let mut result = HashMap::new();
        let mut unified: Vec<(String, BTreeSet<String>)> = unified.into_iter().collect();
        unified.sort_by(|a, b| a.0.cmp(&b.0));
        for (pkg, feats) in unified {
            let active_features: Vec<String> = feats.into_iter().collect();
            let act_deps: Vec<String> = activated_deps
                .remove(&pkg)
                .map(|s| s.into_iter().collect())
                .unwrap_or_default();
            let cfg_flags: Vec<String> = active_features
                .iter()
                .map(|f| format!("feature=\"{f}\""))
                .collect();

            let res = ResolvedPackageFeatures {
                package_name: pkg.clone(),
                active_features,
                activated_dependencies: act_deps,
                cfg_flags,
            };

            result.insert(pkg.clone(), res.clone());

            let bare = bare_name(&pkg).to_string();
            if bare != pkg {
                result.entry(bare).or_insert_with(|| {
                    let mut b_res = res.clone();
                    b_res.package_name = bare_name(&pkg).to_string();
                    b_res
                });
            }

            if let Some(keys) = by_bare.get(pkg.as_str()) {
                for decl_key in keys.iter().filter(|k| k.as_str() != pkg) {
                    result.entry((*decl_key).clone()).or_insert_with(|| {
                        let mut d_res = res.clone();
                        d_res.package_name = (*decl_key).clone();
                        d_res
                    });
                }
            }
        }

        result
    }
}

fn bare_name(key: &str) -> &str {
    key.split_whitespace().next().unwrap_or(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_feature_unification() {
        let mut resolver = DynamicFeatureResolver::new();

        let mut serde_feats = HashMap::new();
        serde_feats.insert("default".to_string(), vec!["std".to_string()]);
        serde_feats.insert("derive".to_string(), vec!["serde_derive".to_string()]);
        serde_feats.insert("std".to_string(), vec![]);
        serde_feats.insert("alloc".to_string(), vec![]);

        resolver.register_package_features("serde", serde_feats);
        resolver.request_features("serde", &["derive".to_string()], true);

        let resolved = resolver.resolve();
        let serde_res = resolved.get("serde").unwrap();

        assert!(serde_res.active_features.contains(&"std".to_string()));
        assert!(serde_res.active_features.contains(&"derive".to_string()));
        assert!(
            serde_res
                .active_features
                .contains(&"serde_derive".to_string())
        );
    }

    /// The original quadratic implementation, kept as an oracle.
    fn resolve_reference(r: &DynamicFeatureResolver) -> HashMap<String, ResolvedPackageFeatures> {
        let mut unified: HashMap<String, BTreeSet<String>> = HashMap::new();
        let mut activated_deps: HashMap<String, BTreeSet<String>> = HashMap::new();

        for (pkg, reqs) in &r.requested_features {
            let matching_keys: Vec<String> = r
                .declared_features
                .keys()
                .filter(|k| k.as_str() == pkg || k.starts_with(&format!("{pkg} ")))
                .cloned()
                .collect();
            let target_pkgs = if matching_keys.is_empty() {
                vec![pkg.clone()]
            } else {
                matching_keys
            };
            for target_pkg in target_pkgs {
                let active = unified.entry(target_pkg.clone()).or_default();
                for req in reqs {
                    active.insert(req.clone());
                }
            }
        }
        for (target_pkg, decl) in &r.declared_features {
            let prefix = target_pkg.split_whitespace().next().unwrap_or(target_pkg);
            let use_default = r
                .use_default_features
                .get(target_pkg)
                .or_else(|| r.use_default_features.get(prefix))
                .copied()
                .unwrap_or(true);
            if use_default && decl.contains_key("default") {
                let active = unified.entry(target_pkg.clone()).or_default();
                active.insert("default".to_string());
                for d in &decl["default"] {
                    active.insert(d.clone());
                }
            }
        }
        let mut changed = true;
        while changed {
            changed = false;
            let current_unified = unified.clone();
            for (pkg, active_set) in current_unified {
                let decl_opt = r.declared_features.get(&pkg).or_else(|| {
                    let prefix = pkg.split_whitespace().next().unwrap_or(&pkg);
                    r.declared_features.get(prefix).or_else(|| {
                        r.declared_features
                            .iter()
                            .find(|(k, _)| k.starts_with(&format!("{prefix} ")))
                            .map(|(_, v)| v)
                    })
                });
                if let Some(decl) = decl_opt {
                    for feat in &active_set {
                        if activated_deps
                            .entry(pkg.clone())
                            .or_default()
                            .insert(feat.clone())
                        {
                            changed = true;
                        }
                        let bare_pkg = pkg.split_whitespace().next().unwrap_or(&pkg).to_string();
                        if bare_pkg != pkg
                            && activated_deps
                                .entry(bare_pkg)
                                .or_default()
                                .insert(feat.clone())
                        {
                            changed = true;
                        }
                        if let Some(sub_features) = decl.get(feat) {
                            for sub in sub_features {
                                if let Some(dep_pkg) = sub.strip_prefix("dep:") {
                                    if activated_deps
                                        .entry(pkg.clone())
                                        .or_default()
                                        .insert(dep_pkg.to_string())
                                    {
                                        changed = true;
                                    }
                                    let bare =
                                        pkg.split_whitespace().next().unwrap_or(&pkg).to_string();
                                    if bare != pkg
                                        && activated_deps
                                            .entry(bare)
                                            .or_default()
                                            .insert(dep_pkg.to_string())
                                    {
                                        changed = true;
                                    }
                                } else if let Some((dep_pkg, dep_feat)) = sub.split_once('/') {
                                    let clean_dep = dep_pkg.strip_suffix('?').unwrap_or(dep_pkg);
                                    if activated_deps
                                        .entry(pkg.clone())
                                        .or_default()
                                        .insert(clean_dep.to_string())
                                    {
                                        changed = true;
                                    }
                                    let real_dep = r.resolve_alias(&pkg, clean_dep);
                                    let target_keys: Vec<String> = r
                                        .declared_features
                                        .keys()
                                        .filter(|k| {
                                            k.as_str() == real_dep
                                                || k.starts_with(&format!("{real_dep} "))
                                        })
                                        .cloned()
                                        .collect();
                                    if unified
                                        .entry(real_dep.clone())
                                        .or_default()
                                        .insert(dep_feat.to_string())
                                    {
                                        changed = true;
                                    }
                                    for target_key in target_keys {
                                        if unified
                                            .entry(target_key)
                                            .or_default()
                                            .insert(dep_feat.to_string())
                                        {
                                            changed = true;
                                        }
                                    }
                                } else {
                                    if unified.entry(pkg.clone()).or_default().insert(sub.clone()) {
                                        changed = true;
                                    }
                                    let bare =
                                        pkg.split_whitespace().next().unwrap_or(&pkg).to_string();
                                    if bare != pkg
                                        && unified.entry(bare).or_default().insert(sub.clone())
                                    {
                                        changed = true;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        let mut result = HashMap::new();
        for (pkg, feats) in unified {
            let active_features: Vec<String> = feats.into_iter().collect();
            let act_deps: Vec<String> = activated_deps
                .remove(&pkg)
                .map(|s| s.into_iter().collect())
                .unwrap_or_default();
            let cfg_flags = active_features
                .iter()
                .map(|f| format!("feature=\"{f}\""))
                .collect();
            let res = ResolvedPackageFeatures {
                package_name: pkg.clone(),
                active_features,
                activated_dependencies: act_deps,
                cfg_flags,
            };
            result.insert(pkg.clone(), res.clone());
            let bare = pkg.split_whitespace().next().unwrap_or(&pkg).to_string();
            if bare != pkg {
                result.entry(bare).or_insert_with(|| {
                    let mut b = res.clone();
                    b.package_name = pkg.split_whitespace().next().unwrap().to_string();
                    b
                });
            }
            for decl_key in r.declared_features.keys() {
                if decl_key.starts_with(&format!("{pkg} ")) {
                    result.entry(decl_key.clone()).or_insert_with(|| {
                        let mut d = res.clone();
                        d.package_name = decl_key.clone();
                        d
                    });
                }
            }
        }
        result
    }

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn random_resolver(
        seed: u64,
        multi_version: bool,
    ) -> (DynamicFeatureResolver, Vec<String>, Vec<String>) {
        {
            let mut rng = Lcg(seed + 1);
            let mut r = DynamicFeatureResolver::new();
            let n = 2 + rng.below(9) as usize;
            let names: Vec<String> = (0..n).map(|i| format!("p{i}")).collect();
            let feat_names = ["a", "b", "c", "d", "std", "default"];

            let mut ids = Vec::new();
            for name in &names {
                let versions = if multi_version && rng.below(4) == 0 {
                    vec!["1.0.0", "2.0.0"]
                } else {
                    vec!["1.0.0"]
                };
                for v in versions {
                    let id = format!("{name} {v}");
                    let mut decl: HashMap<String, Vec<String>> = HashMap::new();
                    for f in feat_names {
                        if rng.below(3) == 0 {
                            continue;
                        }
                        let mut subs = Vec::new();
                        for _ in 0..rng.below(4) {
                            let other = &names[rng.below(n as u64) as usize];
                            let feat = feat_names[rng.below(feat_names.len() as u64) as usize];
                            subs.push(match rng.below(4) {
                                0 => feat.to_string(),
                                1 => format!("dep:{other}"),
                                2 => format!("{other}/{feat}"),
                                _ => format!("{other}?/{feat}"),
                            });
                        }
                        decl.insert(f.to_string(), subs);
                    }
                    r.register_package_features(&id, decl);
                    if rng.below(5) == 0 {
                        let other = names[rng.below(n as u64) as usize].clone();
                        r.register_package_aliases(
                            &id,
                            HashMap::from([(format!("alias{}", rng.below(3)), other)]),
                        );
                    }
                    ids.push(id);
                }
            }
            for _ in 0..rng.below(6) + 1 {
                let target = if rng.below(4) == 0 {
                    names[rng.below(n as u64) as usize].clone()
                } else {
                    ids[rng.below(ids.len() as u64) as usize].clone()
                };
                let feats: Vec<String> = (0..rng.below(3))
                    .map(|_| feat_names[rng.below(feat_names.len() as u64) as usize].to_string())
                    .collect();
                r.request_features(&target, &feats, rng.below(2) == 0);
            }

            (r, names, ids)
        }
    }

    fn normalize(
        m: &HashMap<String, ResolvedPackageFeatures>,
    ) -> Vec<(String, Vec<String>, Vec<String>)> {
        let mut v: Vec<_> = m
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    v.active_features.clone(),
                    v.activated_dependencies.clone(),
                )
            })
            .collect();
        v.sort();
        v
    }

    #[test]
    fn optimized_resolver_matches_the_reference_on_random_graphs() {
        // The reference is only deterministic for single-version graphs.
        for seed in 0..400u64 {
            let (r, _, _) = random_resolver(seed, false);
            let fast = r.resolve();
            let slow = resolve_reference(&r);
            assert_eq!(
                normalize(&fast),
                normalize(&slow),
                "diverged for seed {seed}"
            );
        }
    }

    #[test]
    fn resolution_is_deterministic_with_multiple_versions() {
        for seed in 0..100u64 {
            let (r, _, _) = random_resolver(seed, true);
            let first = normalize(&r.resolve());
            for _ in 0..5 {
                assert_eq!(
                    first,
                    normalize(&r.resolve()),
                    "nondeterministic for seed {seed}"
                );
            }
        }
    }
}
