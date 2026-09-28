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

/// A cached file entry containing its last known modification time and content hash.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileEntry {
    pub path: PathBuf,
    /// Modification time in nanoseconds since the epoch. Whole seconds are
    /// not enough: two edits within the same second (an editor save followed
    /// by a code generator, say) would look unchanged.
    pub mtime: u64,
    /// File length in bytes; a second cheap signal alongside `mtime`.
    #[serde(default)]
    pub size: u64,
    pub hash: u64,
}

/// Persistent cache for crate fingerprints and file hashes.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FingerprintCache {
    /// The last recorded inputs for the crate.
    pub inputs: Option<FingerprintInputs>,
    /// The final crate fingerprint hash.
    pub hash: Option<String>,
    /// Cached file entries to speed up source hashing.
    pub files: HashMap<PathBuf, FileEntry>,
    /// Files seen since this cache was loaded; used to drop entries for files
    /// that no longer exist (or are no longer hashed) when saving.
    #[serde(skip)]
    visited: HashSet<PathBuf>,
    /// Whether anything changed since load, so an unchanged cache isn't
    /// rewritten on every build.
    #[serde(skip)]
    dirty: bool,
    /// Package hashes already computed during this run, so a package with
    /// several targets (a library plus binaries) is walked and hashed once.
    #[serde(skip)]
    pub(crate) package_hashes: HashMap<PathBuf, String>,
    /// The files each package hashed this run consists of (what `rune
    /// explain` compares file by file).
    #[serde(skip)]
    pub(crate) package_files: HashMap<PathBuf, Vec<PathBuf>>,
}

impl FingerprintCache {
    /// Every file of a package hashed this run, relative to the package, with
    /// its content hash (hex), sorted. Empty if the package was not hashed.
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

    /// Creates a new, empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Loads the cache from a JSON file.
    pub fn load(path: &Path) -> Option<Self> {
        let data = fs::read_to_string(path).ok()?;
        serde_json::from_str(&data).ok()
    }

    /// Records that `path` was looked at during this run.
    pub fn mark_visited(&mut self, path: &Path) {
        self.visited.insert(path.to_path_buf());
    }

    /// Stores or refreshes a file's entry, tracking whether anything changed.
    pub fn record(&mut self, entry: FileEntry) {
        if self.files.get(&entry.path) != Some(&entry) {
            self.dirty = true;
            self.files.insert(entry.path.clone(), entry);
        }
    }

    /// Saves the cache to a JSON file, unconditionally.
    ///
    /// Entries for files that were not visited this run are dropped first
    /// (only when *something* was visited, so saving a cache that was never
    /// used for hashing can't wipe it).
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

    /// Saves only if something changed since load: a fully up-to-date build
    /// should not rewrite this file at all.
    pub fn save_if_changed(&self, path: &Path) -> Result<(), CacheError> {
        let prunable =
            !self.visited.is_empty() && self.files.keys().any(|p| !self.visited.contains(p));
        if self.dirty || prunable || !path.exists() {
            self.save(path)?;
        }
        Ok(())
    }

    /// Checks if the cached fingerprint is still fresh for the given inputs.
    pub fn is_fresh(&self, current: &FingerprintInputs) -> bool {
        self.inputs.as_ref() == Some(current)
    }

    /// Determines if a file needs to be rehashed based on its modification time.
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
