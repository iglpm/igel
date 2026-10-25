//! Where the store's disk space goes: `igl cas du`.

use crate::store::{ArtifactManifest, CasStore};
use std::collections::HashMap;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageUsage {
    pub name: String,
    pub entries: u64,
    pub bytes: u64,
    pub idle_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdleBucket {
    pub min_idle_secs: u64,
    pub entries: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub entries: u64,
    pub bytes: u64,
    pub packages: Vec<PackageUsage>,
    pub idle: Vec<IdleBucket>,
}

const DAY: u64 = 86_400;

pub const IDLE_THRESHOLDS: [u64; 4] = [3_600, DAY, 7 * DAY, 30 * DAY];

pub fn usage(store: &CasStore) -> Usage {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut out = Usage {
        idle: IDLE_THRESHOLDS
            .iter()
            .map(|&t| IdleBucket {
                min_idle_secs: t,
                entries: 0,
                bytes: 0,
            })
            .collect(),
        ..Usage::default()
    };
    let mut by_name: HashMap<String, PackageUsage> = HashMap::new();

    for entry in WalkDir::new(store.root().join("objects"))
        .min_depth(2)
        .max_depth(2)
        .into_iter()
        .flatten()
    {
        if !entry.file_type().is_dir() {
            continue;
        }
        let manifest_path = entry.path().join("manifest.json");
        let Ok(text) = fs::read_to_string(&manifest_path) else {
            continue;
        };
        let Ok(manifest) = serde_json::from_str::<ArtifactManifest>(&text) else {
            continue;
        };
        let used = fs::metadata(&manifest_path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0)
            .max(manifest.stored_at);
        let idle = now.saturating_sub(used);

        out.entries += 1;
        out.bytes += manifest.disk_size();
        for bucket in &mut out.idle {
            if idle >= bucket.min_idle_secs {
                bucket.entries += 1;
                bucket.bytes += manifest.disk_size();
            }
        }
        let pkg = by_name
            .entry(manifest.crate_name.clone())
            .or_insert_with(|| PackageUsage {
                name: manifest.crate_name.clone(),
                entries: 0,
                bytes: 0,
                idle_secs: u64::MAX,
            });
        pkg.entries += 1;
        pkg.bytes += manifest.disk_size();
        pkg.idle_secs = pkg.idle_secs.min(idle);
    }

    out.packages = by_name.into_values().collect();
    out.packages
        .sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn put(store: &CasStore, key: &str, name: &str, bytes: usize) {
        let src = tempdir().unwrap();
        let file = src.path().join(format!("lib{name}-{key}.rlib"));
        fs::write(&file, vec![b'x'; bytes]).unwrap();
        store.put(key, name, "1.0.0", &[file]).unwrap();
    }

    fn age(store: &CasStore, key: &str, secs: u64) {
        let path = store.object_dir(key).join("manifest.json");
        let mut m: ArtifactManifest =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        m.stored_at = m.stored_at.saturating_sub(secs);
        fs::write(&path, serde_json::to_string(&m).unwrap()).unwrap();
        let t = filetime::FileTime::from_unix_time(m.stored_at as i64, 0);
        filetime::set_file_mtime(&path, t).unwrap();
    }

    #[test]
    fn sizes_are_summed_per_package_biggest_first() {
        let dir = tempdir().unwrap();
        let store = CasStore::new(dir.path().to_path_buf()).unwrap();
        put(&store, "a1", "small", 10);
        put(&store, "b1", "big", 1000);
        put(&store, "b2", "big", 500);

        let u = usage(&store);
        assert_eq!(u.entries, 3);
        assert_eq!(u.bytes, 1510);
        assert_eq!(u.packages[0].name, "big");
        assert_eq!(u.packages[0].entries, 2);
        assert_eq!(u.packages[0].bytes, 1500);
        assert_eq!(u.packages[1].name, "small");
    }

    #[test]
    fn idle_buckets_are_cumulative_and_go_by_last_use() {
        let dir = tempdir().unwrap();
        let store = CasStore::new(dir.path().to_path_buf()).unwrap();
        put(&store, "fresh", "fresh", 100);
        put(&store, "old", "old", 200);
        age(&store, "old", 10 * DAY);

        let u = usage(&store);
        let bucket = |secs| u.idle.iter().find(|b| b.min_idle_secs == secs).unwrap();
        assert_eq!(bucket(DAY).entries, 1);
        assert_eq!(bucket(DAY).bytes, 200);
        assert_eq!(
            bucket(7 * DAY).entries,
            1,
            "idle for 10 days is idle for a week too"
        );
        assert_eq!(bucket(30 * DAY).entries, 0);
        let old = u.packages.iter().find(|p| p.name == "old").unwrap();
        assert!(old.idle_secs >= 10 * DAY);
    }

    #[test]
    fn an_empty_store_is_empty() {
        let dir = tempdir().unwrap();
        let store = CasStore::new(dir.path().to_path_buf()).unwrap();
        let u = usage(&store);
        assert_eq!((u.entries, u.bytes), (0, 0));
        assert!(u.packages.is_empty());
    }
}
