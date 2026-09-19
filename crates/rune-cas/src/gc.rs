use crate::store::{ArtifactManifest, CasStore};
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info};
use walkdir::WalkDir;

pub struct GcConfig {
    /// Maximum total size in bytes. Entries are evicted LRU until under this limit.
    pub max_size_bytes: Option<u64>,
    /// Maximum age in seconds. Entries older than this are evicted.
    pub max_age_secs: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct GcReport {
    pub entries_removed: u64,
    pub bytes_freed: u64,
    pub entries_remaining: u64,
    pub bytes_remaining: u64,
}

/// Run garbage collection on the CAS store.
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

    // 1. Gather all entries
    let mut entries = Vec::new();
    for entry in WalkDir::new(&objects_dir).min_depth(2).max_depth(2) {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        if entry.file_type().is_dir() {
            let manifest_path = entry.path().join("manifest.json");
            if manifest_path.exists() {
                if let Ok(content) = fs::read_to_string(&manifest_path) {
                    if let Ok(mut manifest) = serde_json::from_str::<ArtifactManifest>(&content) {
                        // Last *use*, not last store: a cache hit bumps the
                        // manifest's mtime (see `CasStore::link_to`).
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
        }
    }

    // Sort entries by stored_at ascending (oldest first) for LRU eviction
    entries.sort_by_key(|(_, m)| m.stored_at);

    // 2. Remove entries older than max_age_secs
    let mut current_total_size: u64 = entries.iter().map(|(_, m)| m.total_size).sum();
    let mut remaining_entries = Vec::new();

    for (path, manifest) in entries {
        let age = now.saturating_sub(manifest.stored_at);
        let mut remove = false;

        if let Some(max_age) = config.max_age_secs {
            if age > max_age {
                remove = true;
                debug!(
                    "Evicting {} due to age ({}s > {}s)",
                    manifest.cache_key, age, max_age
                );
            }
        }

        if remove {
            if fs::remove_dir_all(&path).is_ok() {
                report.entries_removed += 1;
                report.bytes_freed += manifest.total_size;
                current_total_size = current_total_size.saturating_sub(manifest.total_size);
            }
        } else {
            remaining_entries.push((path, manifest));
        }
    }

    // 3. Remove oldest entries if still over max_size_bytes
    if let Some(max_size) = config.max_size_bytes {
        for (path, manifest) in remaining_entries.iter() {
            if current_total_size <= max_size {
                break;
            }
            if fs::remove_dir_all(path).is_ok() {
                report.entries_removed += 1;
                report.bytes_freed += manifest.total_size;
                current_total_size = current_total_size.saturating_sub(manifest.total_size);
                debug!("Evicting {} due to size constraint", manifest.cache_key);
            }
        }
    }

    // Recalculate remaining
    let final_remaining: Vec<_> = remaining_entries
        .into_iter()
        .filter(|(p, _)| p.exists())
        .collect();

    report.entries_remaining = final_remaining.len() as u64;
    report.bytes_remaining = final_remaining.iter().map(|(_, m)| m.total_size).sum();

    // Update stats
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

        store.put("key1", "foo", "1.0", &[file.clone()]).unwrap();
        store.put("key2", "bar", "1.0", &[file]).unwrap();

        // Age key1 by ten seconds instead of sleeping: rewrite its manifest
        // (and mtime, which GC treats as last use) into the past.
        let manifest_path = store.object_dir("key1").join("manifest.json");
        let mut manifest: ArtifactManifest =
            serde_json::from_str(&fs::read_to_string(&manifest_path).unwrap()).unwrap();
        manifest.stored_at -= 10;
        fs::write(&manifest_path, serde_json::to_string(&manifest).unwrap()).unwrap();
        let past = filetime::FileTime::from_unix_time(manifest.stored_at as i64, 0);
        filetime::set_file_mtime(&manifest_path, past).unwrap();

        let config = GcConfig {
            max_size_bytes: None,
            max_age_secs: Some(1), // Evict older than 1s
        };

        let report = run_gc(&store, &config).unwrap();
        assert_eq!(report.entries_removed, 1);
        assert_eq!(report.entries_remaining, 1);

        assert!(store.get("key1").is_none());
        assert!(store.get("key2").is_some());
    }
}
