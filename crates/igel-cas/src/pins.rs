//! Pinned projects: entries their builds need are never garbage-collected.

use crate::store::CasStore;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pin {
    pub project: PathBuf,
    pub pinned_at: u64,
    pub keys: Vec<String>,
}

fn pin_path(store: &CasStore, project: &Path) -> PathBuf {
    let id = blake3::hash(project.to_string_lossy().as_bytes()).to_hex();
    store
        .root()
        .join("pins")
        .join(format!("{}.json", &id[..16]))
}

pub fn pin(store: &CasStore, project: &Path, keys: Vec<String>) -> std::io::Result<()> {
    let path = pin_path(store, project);
    fs::create_dir_all(path.parent().expect("pins dir"))?;
    let mut keys = keys;
    keys.sort();
    keys.dedup();
    let pin = Pin {
        project: project.to_path_buf(),
        pinned_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        keys,
    };
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&tmp, serde_json::to_vec(&pin)?)?;
    fs::rename(tmp, path)
}

pub fn unpin(store: &CasStore, project: &Path) -> bool {
    fs::remove_file(pin_path(store, project)).is_ok()
}

pub fn list(store: &CasStore) -> Vec<Pin> {
    let Ok(dir) = fs::read_dir(store.root().join("pins")) else {
        return Vec::new();
    };
    let mut pins: Vec<Pin> = dir
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| serde_json::from_slice(&fs::read(e.path()).ok()?).ok())
        .filter(|p: &Pin| p.project.is_dir())
        .collect();
    pins.sort_by(|a, b| a.project.cmp(&b.project));
    pins
}

pub fn prune(store: &CasStore) {
    let Ok(dir) = fs::read_dir(store.root().join("pins")) else {
        return;
    };
    for e in dir.filter_map(|e| e.ok()) {
        let stale = fs::read(e.path())
            .ok()
            .and_then(|b| serde_json::from_slice::<Pin>(&b).ok())
            .is_some_and(|p| !p.project.is_dir());
        if stale {
            let _ = fs::remove_file(e.path());
        }
    }
}

pub fn pinned_keys(store: &CasStore) -> HashSet<String> {
    list(store).into_iter().flat_map(|p| p.keys).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_protect_keys_while_the_project_exists() {
        let dir = tempfile::tempdir().unwrap();
        let store = CasStore::new(dir.path().join("store")).unwrap();
        let project = dir.path().join("proj");
        fs::create_dir_all(&project).unwrap();
        pin(&store, &project, vec!["b".into(), "a".into(), "a".into()]).unwrap();
        assert_eq!(list(&store)[0].keys, ["a", "b"]);
        assert!(pinned_keys(&store).contains("a"));

        fs::remove_dir_all(&project).unwrap();
        assert!(pinned_keys(&store).is_empty());
        prune(&store);
        assert!(!unpin(&store, &project));
    }
}
