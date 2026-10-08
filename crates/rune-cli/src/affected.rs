//! `rune affected`: the packages owning changed files and everything that depends on them.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

pub struct Pkg {
    pub name: String,
    pub dir: PathBuf,
    pub deps: Vec<String>,
}

fn changes_everything(rel: &Path) -> bool {
    let name = rel.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let in_cargo_dir = rel.components().any(|c| c.as_os_str() == ".cargo");
    matches!(
        name,
        "Cargo.lock" | "rust-toolchain" | "rust-toolchain.toml" | "Rune.toml"
    ) || (in_cargo_dir && matches!(name, "config" | "config.toml"))
        || rel == Path::new("Cargo.toml")
}

pub fn affected(pkgs: &[Pkg], root: &Path, changed: &[PathBuf]) -> Vec<String> {
    let mut hit: HashSet<usize> = HashSet::new();
    let mut everything = false;
    for path in changed {
        let abs = if path.is_absolute() {
            path.clone()
        } else {
            root.join(path)
        };
        let rel = abs.strip_prefix(root).unwrap_or(&abs);
        if changes_everything(rel) {
            everything = true;
            break;
        }
        let owner = pkgs
            .iter()
            .enumerate()
            .filter(|(_, p)| abs.starts_with(&p.dir))
            .max_by_key(|(_, p)| p.dir.components().count());
        if let Some((i, _)) = owner {
            hit.insert(i);
        }
    }
    if everything {
        return pkgs
            .iter()
            .map(|p| p.name.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
    }

    let by_name: HashMap<&str, usize> = pkgs
        .iter()
        .enumerate()
        .map(|(i, p)| (p.name.as_str(), i))
        .collect();
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); pkgs.len()];
    for (i, p) in pkgs.iter().enumerate() {
        for dep in &p.deps {
            if let Some(&d) = by_name.get(dep.as_str()) {
                dependents[d].push(i);
            }
        }
    }
    let mut stack: Vec<usize> = hit.iter().copied().collect();
    while let Some(i) = stack.pop() {
        for &d in &dependents[i] {
            if hit.insert(d) {
                stack.push(d);
            }
        }
    }
    hit.into_iter()
        .map(|i| pkgs[i].name.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

pub fn parse_partition(s: &str) -> Option<(usize, usize)> {
    let (n, m) = s.split_once('/')?;
    let (n, m): (usize, usize) = (n.trim().parse().ok()?, m.trim().parse().ok()?);
    (m > 0 && (1..=m).contains(&n)).then_some((n, m))
}

/// Every `m`-th of the sorted list from the `n`-th: deterministic across CI machines.
pub fn partition(names: &[String], n: usize, m: usize) -> Vec<String> {
    let mut sorted: Vec<&String> = names.iter().collect();
    sorted.sort();
    sorted
        .into_iter()
        .enumerate()
        .filter(|(i, _)| i % m == n - 1)
        .map(|(_, s)| s.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partitions_cover_everything_once_and_are_balanced() {
        let names: Vec<String> = ["e", "a", "d", "b", "c"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let shards: Vec<Vec<String>> = (1..=2).map(|n| partition(&names, n, 2)).collect();
        assert_eq!(shards[0], ["a", "c", "e"]);
        assert_eq!(shards[1], ["b", "d"]);
        assert_eq!(parse_partition("2/3"), Some((2, 3)));
        assert_eq!(parse_partition("0/3"), None);
        assert_eq!(parse_partition("4/3"), None);
        assert_eq!(parse_partition("x"), None);
    }

    fn pkg(name: &str, dir: &str, deps: &[&str]) -> Pkg {
        Pkg {
            name: name.into(),
            dir: PathBuf::from("/ws").join(dir),
            deps: deps.iter().map(|d| d.to_string()).collect(),
        }
    }

    fn workspace() -> Vec<Pkg> {
        vec![
            pkg("a", "crates/a", &["serde"]),
            pkg("b", "crates/b", &["a"]),
            pkg("c", "crates/c", &["b", "tokio"]),
            pkg("d", "crates/d", &[]),
            pkg("e", "crates/a/e", &[]),
        ]
    }

    fn run(changed: &[&str]) -> Vec<String> {
        let changed: Vec<PathBuf> = changed.iter().map(PathBuf::from).collect();
        affected(&workspace(), Path::new("/ws"), &changed)
    }

    #[test]
    fn a_change_affects_its_package_and_everything_above_it() {
        assert_eq!(run(&["crates/a/src/lib.rs"]), ["a", "b", "c"]);
        assert_eq!(run(&["crates/b/src/lib.rs"]), ["b", "c"]);
        assert_eq!(run(&["crates/c/src/main.rs"]), ["c"]);
    }

    #[test]
    fn unrelated_packages_are_left_out() {
        assert_eq!(run(&["crates/d/src/lib.rs"]), ["d"]);
    }

    #[test]
    fn a_file_belongs_to_the_deepest_package_containing_it() {
        assert_eq!(
            run(&["crates/a/e/src/lib.rs"]),
            ["e"],
            "e is nested in a but is its own package"
        );
        assert_eq!(
            run(&["crates/a/src/lib.rs", "crates/a/e/x.rs"]),
            ["a", "b", "c", "e"]
        );
    }

    #[test]
    fn files_outside_every_package_change_nothing() {
        assert!(run(&["README.md", ".github/workflows/ci.yml", "docs/x.md"]).is_empty());
    }

    #[test]
    fn workspace_wide_files_affect_everything() {
        for f in [
            "Cargo.lock",
            "Cargo.toml",
            "rust-toolchain.toml",
            ".cargo/config.toml",
            "Rune.toml",
        ] {
            assert_eq!(run(&[f]), ["a", "b", "c", "d", "e"], "{f}");
        }
        assert_eq!(run(&["crates/d/Cargo.toml"]), ["d"]);
    }

    #[test]
    fn absolute_paths_work_and_dependency_cycles_terminate() {
        let mut pkgs = workspace();
        pkgs[0].deps.push("c".into());
        let got = affected(
            &pkgs,
            Path::new("/ws"),
            &[PathBuf::from("/ws/crates/b/x.rs")],
        );
        assert_eq!(got, ["a", "b", "c"]);
    }
}
