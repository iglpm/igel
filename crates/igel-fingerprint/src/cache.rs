use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

use crate::hasher::FingerprintInputs;

#[derive(Error, Debug)]
pub enum CacheError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileEntry {
    pub path: PathBuf,
    /// Nanoseconds: two edits within one second must not look unchanged.
    pub mtime: u64,
    #[serde(default)]
    pub size: u64,
    pub hash: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FingerprintCache {
    pub inputs: Option<FingerprintInputs>,
    pub hash: Option<String>,
    pub files: HashMap<PathBuf, FileEntry>,
    #[serde(skip)]
    visited: HashSet<PathBuf>,
    #[serde(skip)]
    dirty: bool,
    #[serde(skip)]
    pub(crate) package_hashes: HashMap<PathBuf, String>,
    #[serde(skip)]
    pub(crate) package_files: HashMap<PathBuf, Vec<PathBuf>>,
}

impl FingerprintCache {
    pub fn package_file_hashes(&self, package_dir: &std::path::Path) -> Vec<(String, String)> {
        let Some(files) = self.package_files.get(package_dir) else {
            return Vec::new();
        };
        let mut out: Vec<(String, String)> = files
            .iter()
            .filter_map(|f| {
                let rel = f.strip_prefix(package_dir).ok()?;
                let entry = self.files.get(f)?;
                Some((
                    rel.to_string_lossy().replace('\\', "/"),
                    format!("{:016x}", entry.hash),
                ))
            })
            .collect();
        out.sort();
        out
    }

    pub fn new() -> Self {
        Self::default()
    }

    pub fn load(path: &Path) -> Option<Self> {
        let data = fs::read_to_string(path).ok()?;
        serde_json::from_str(&data).ok()
    }

    pub fn mark_visited(&mut self, path: &Path) {
        self.visited.insert(path.to_path_buf());
    }

    pub fn record(&mut self, entry: FileEntry) {
        if self.files.get(&entry.path) != Some(&entry) {
            self.dirty = true;
            self.files.insert(entry.path.clone(), entry);
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), CacheError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let data = if self.visited.is_empty() {
            serde_json::to_string(self)?
        } else {
            let mut pruned = self.clone();
            pruned.files.retain(|p, _| self.visited.contains(p));
            serde_json::to_string(&pruned)?
        };
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, data)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn save_if_changed(&self, path: &Path) -> Result<(), CacheError> {
        let prunable =
            !self.visited.is_empty() && self.files.keys().any(|p| !self.visited.contains(p));
        if self.dirty || prunable || !path.exists() {
            self.save(path)?;
        }
        Ok(())
    }

    pub fn is_fresh(&self, current: &FingerprintInputs) -> bool {
        self.inputs.as_ref() == Some(current)
    }

    pub fn file_needs_rehash(entry: &FileEntry) -> bool {
        let Ok(meta) = fs::metadata(&entry.path) else {
            return true;
        };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        mtime != entry.mtime || mtime == 0 || meta.len() != entry.size
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_cache_save_load() {
        let dir = tempdir().unwrap();
        let cache_path = dir.path().join("cache.json");

        let mut cache = FingerprintCache::new();
        cache.hash = Some("testhash".to_string());

        cache.save(&cache_path).unwrap();

        let loaded = FingerprintCache::load(&cache_path).unwrap();
        assert_eq!(loaded.hash.unwrap(), "testhash");
    }
}
