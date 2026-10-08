use crate::store::{ArtifactManifest, CasStore};
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info};
use walkdir::WalkDir;

pub struct GcConfig {
    pub max_size_bytes: Option<u64>,
    pub max_age_secs: Option<u64>,
    pub compress_idle_secs: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct GcReport {
    pub entries_removed: u64,
    pub bytes_freed: u64,
    pub entries_remaining: u64,
    pub bytes_remaining: u64,
    pub entries_pinned: u64,
    pub entries_compressed: u64,
    pub bytes_compressed_away: u64,
}

pub fn run_gc(store: &CasStore, config: &GcConfig) -> crate::store::Result<GcReport> {
    info!("Running CAS garbage collection");

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let objects_dir = store.root().join("objects");
    if !objects_dir.exists() {
        return Ok(GcReport::default());
    }

    let mut report = GcReport::default();

    let mut entries = Vec::new();
    for entry in WalkDir::new(&objects_dir).min_depth(2).max_depth(2) {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        if entry.file_type().is_dir() {
            let manifest_path = entry.path().join("manifest.json");
            if manifest_path.exists()
                && let Ok(content) = fs::read_to_string(&manifest_path)
                && let Ok(mut manifest) = serde_json::from_str::<ArtifactManifest>(&content)
            {
                // Last *use*, not last store: a cache hit bumps the manifest's mtime.
                let used = fs::metadata(&manifest_path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                manifest.stored_at = manifest.stored_at.max(used);
                entries.push((entry.path().to_path_buf(), manifest));
            }
        }
    }

    entries.sort_by_key(|(_, m)| m.stored_at);

    crate::pins::prune(store);
    let pinned = crate::pins::pinned_keys(store);
    let is_pinned = |m: &ArtifactManifest| pinned.contains(&m.cache_key);
    report.entries_pinned = entries.iter().filter(|(_, m)| is_pinned(m)).count() as u64;

    let mut current_total_size: u64 = entries.iter().map(|(_, m)| m.disk_size()).sum();
    let mut remaining_entries = Vec::new();

    for (path, manifest) in entries {
        let age = now.saturating_sub(manifest.stored_at);
        let mut remove = false;

        if let Some(max_age) = config.max_age_secs
            && age > max_age
            && !is_pinned(&manifest)
        {
            remove = true;
            debug!(
                "Evicting {} due to age ({}s > {}s)",
                manifest.cache_key, age, max_age
            );
        }

        if remove {
            if fs::remove_dir_all(&path).is_ok() {
                report.entries_removed += 1;
                report.bytes_freed += manifest.disk_size();
                current_total_size = current_total_size.saturating_sub(manifest.disk_size());
            }
        } else {
            remaining_entries.push((path, manifest));
        }
    }

    if let Some(max_size) = config.max_size_bytes {
        for (path, manifest) in remaining_entries.iter() {
            if current_total_size <= max_size {
                break;
            }
            if is_pinned(manifest) {
                continue;
            }
            if fs::remove_dir_all(path).is_ok() {
                report.entries_removed += 1;
                report.bytes_freed += manifest.disk_size();
                current_total_size = current_total_size.saturating_sub(manifest.disk_size());
                debug!("Evicting {} due to size constraint", manifest.cache_key);
            }
        }
    }

    let mut final_remaining: Vec<_> = remaining_entries
        .into_iter()
        .filter(|(p, _)| p.exists())
        .collect();

    if let Some(idle) = config.compress_idle_secs {
        for (path, manifest) in final_remaining.iter_mut() {
            if manifest.compressed || now.saturating_sub(manifest.stored_at) <= idle {
                continue;
            }
            match store.compress(path) {
                Ok(0) => {}
                Ok(saved) => {
                    report.entries_compressed += 1;
                    report.bytes_compressed_away += saved;
                    manifest.compressed = true;
                    manifest.stored_size = Some(manifest.total_size - saved);
                }
                Err(e) => debug!("could not compress {}: {e}", manifest.cache_key),
            }
        }
    }

    report.entries_remaining = final_remaining.len() as u64;
    report.bytes_remaining = final_remaining.iter().map(|(_, m)| m.disk_size()).sum();

    store.flush_stats();
    if let Ok(mut stats) = store.stats() {
        stats.total_entries = report.entries_remaining;
        stats.total_size_bytes = report.bytes_remaining;
        stats.last_gc = Some(now);
        let _ = store.save_stats(&stats);
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_gc_age_eviction() {
        let dir = tempdir().unwrap();
        let store = CasStore::new(dir.path().to_path_buf()).unwrap();

        let src_dir = tempdir().unwrap();
        let file = src_dir.path().join("lib.rlib");
        fs::write(&file, "test data").unwrap();

        store
            .put("key1", "foo", "1.0", std::slice::from_ref(&file))
            .unwrap();
        store.put("key2", "bar", "1.0", &[file]).unwrap();

        let manifest_path = store.object_dir("key1").join("manifest.json");
        let mut manifest: ArtifactManifest =
            serde_json::from_str(&fs::read_to_string(&manifest_path).unwrap()).unwrap();
        manifest.stored_at -= 10;
        fs::write(&manifest_path, serde_json::to_string(&manifest).unwrap()).unwrap();
        let past = filetime::FileTime::from_unix_time(manifest.stored_at as i64, 0);
        filetime::set_file_mtime(&manifest_path, past).unwrap();

        let config = GcConfig {
            max_size_bytes: None,
            max_age_secs: Some(1),
            compress_idle_secs: None,
        };

        let report = run_gc(&store, &config).unwrap();
        assert_eq!(report.entries_removed, 1);
        assert_eq!(report.entries_remaining, 1);

        assert!(store.get("key1").is_none());
        assert!(store.get("key2").is_some());
    }

    fn age(store: &CasStore, key: &str, secs: u64) {
        let manifest_path = store.object_dir(key).join("manifest.json");
        let used = fs::metadata(&manifest_path).unwrap().modified().unwrap();
        let past =
            filetime::FileTime::from_system_time(used - std::time::Duration::from_secs(secs));
        filetime::set_file_mtime(&manifest_path, past).unwrap();
        let mut m: ArtifactManifest =
            serde_json::from_str(&fs::read_to_string(&manifest_path).unwrap()).unwrap();
        m.stored_at -= secs;
        fs::write(&manifest_path, serde_json::to_string(&m).unwrap()).unwrap();
        filetime::set_file_mtime(&manifest_path, past).unwrap();
    }

    #[test]
    fn pinned_entries_survive_and_idle_ones_are_compressed_then_restored() {
        let dir = tempdir().unwrap();
        let store = CasStore::new(dir.path().join("store")).unwrap();
        let src = dir.path().join("libfoo.rlib");
        let data = "compressible rlib contents ".repeat(4_000);
        fs::write(&src, &data).unwrap();
        for k in ["pinned", "idle", "fresh"] {
            store.put(k, k, "1.0", std::slice::from_ref(&src)).unwrap();
        }
        age(&store, "pinned", 100 * 86_400);
        age(&store, "idle", 10 * 86_400);
        let project = dir.path().join("proj");
        fs::create_dir_all(&project).unwrap();
        crate::pins::pin(&store, &project, vec!["pinned".into()]).unwrap();

        let report = run_gc(
            &store,
            &GcConfig {
                max_size_bytes: None,
                max_age_secs: Some(30 * 86_400),
                compress_idle_secs: Some(7 * 86_400),
            },
        )
        .unwrap();
        assert_eq!(report.entries_removed, 0);
        assert_eq!(report.entries_pinned, 1);
        assert_eq!(report.entries_compressed, 2);
        let idle = store.object_dir("idle");
        assert!(!idle.join("libfoo.rlib").exists());
        assert!(idle.join("libfoo.rlib.zst").exists());
        assert!(report.bytes_remaining < 2 * data.len() as u64);

        let out = dir.path().join("target");
        store.link_to("idle", &out).unwrap();
        assert_eq!(fs::read_to_string(out.join("libfoo.rlib")).unwrap(), data);
        assert!(!idle.join("libfoo.rlib.zst").exists());

        crate::pins::unpin(&store, &project);
        let report = run_gc(
            &store,
            &GcConfig {
                max_size_bytes: None,
                max_age_secs: Some(30 * 86_400),
                compress_idle_secs: None,
            },
        )
        .unwrap();
        assert_eq!(report.entries_removed, 1);
        assert!(store.get("pinned").is_none());
    }
}
