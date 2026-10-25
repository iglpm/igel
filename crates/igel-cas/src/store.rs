use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tracing::debug;

#[derive(Debug, Error)]
pub enum CasError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("Integrity error: {0}")]
    Integrity(String),
}

pub type Result<T> = std::result::Result<T, CasError>;

#[derive(Debug, Clone)]
pub struct CasStore {
    root: PathBuf,
    stats_lock: Arc<Mutex<()>>,
    pending: Arc<PendingStats>,
    verify: bool,
    remote: Option<crate::remote::Remote>,
    publisher: Option<Arc<crate::remote::Publisher>>,
}

#[derive(Debug, Default)]
struct PendingStats {
    hits: AtomicU64,
    misses: AtomicU64,
    entries: AtomicU64,
    bytes: AtomicU64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactManifest {
    pub cache_key: String,
    pub crate_name: String,
    pub crate_version: String,
    pub artifacts: Vec<ArtifactEntry>,
    pub stored_at: u64,
    pub total_size: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub compressed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stored_size: Option<u64>,
    /// Content of every crate it was compiled against (`--extern`). rustc output isn't always reproducible
    /// and rustc only loads a crate next to the exact builds it was compiled against.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub compiled_against: BTreeMap<String, String>,
}

impl ArtifactManifest {
    pub fn disk_size(&self) -> u64 {
        match self.stored_size {
            Some(n) if self.compressed => n,
            _ => self.total_size,
        }
    }

    pub fn content(&self) -> Option<Content> {
        let chosen = content_file(self.artifacts.iter().map(|a| Path::new(&a.filename)))?;
        self.artifacts
            .iter()
            .find(|a| Path::new(&a.filename) == chosen)
            .map(|a| Content {
                file: a.filename.clone(),
                id: a.hash.clone(),
            })
    }

    pub fn matches_dependencies(&self, deps: &BTreeMap<String, String>) -> bool {
        self.compiled_against == *deps
    }
}

pub fn content_file<'a>(files: impl IntoIterator<Item = &'a Path>) -> Option<&'a Path> {
    let rank = |p: &Path| match p.extension().and_then(|e| e.to_str()) {
        Some("rmeta") => Some(0),
        Some("rlib") => Some(1),
        Some("dll" | "so" | "dylib") => Some(2),
        _ => None,
    };
    files
        .into_iter()
        .filter_map(|p| rank(p).map(|r| (r, p)))
        .min_by_key(|(r, p)| (*r, p.to_path_buf()))
        .map(|(_, p)| p)
}

/// One build of a unit as dependents see it: its `.rmeta` (or library) by name and hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Content {
    pub file: String,
    pub id: String,
}

pub fn content_of_files(files: &[PathBuf]) -> Option<Content> {
    let chosen = content_file(files.iter().map(PathBuf::as_path))?;
    Some(Content {
        file: chosen.file_name()?.to_string_lossy().into_owned(),
        id: hash_file(chosen).ok()?,
    })
}

#[derive(Debug, Default, Clone)]
pub struct PutOptions {
    pub adopt: bool,
    pub compiled_against: BTreeMap<String, String>,
    pub replace_stale: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PutOutcome {
    Stored,
    Kept,
    KeptStale,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactEntry {
    pub filename: String,
    pub size: u64,
    pub hash: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CasStats {
    pub total_entries: u64,
    pub total_size_bytes: u64,
    pub hits: u64,
    pub misses: u64,
    pub last_gc: Option<u64>,
}

pub struct CasEntry {
    pub manifest: ArtifactManifest,
    pub object_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Linked {
    pub files: Vec<PathBuf>,
    pub manifest: ArtifactManifest,
}

/// Objects are read-only so a stray write through a hard link fails instead of corrupting the store.
#[cfg_attr(unix, allow(dead_code))]
pub(crate) fn make_read_only_pub(path: &Path) {
    make_read_only(path);
}

fn make_read_only(path: &Path) {
    if let Ok(meta) = fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_readonly(true);
        let _ = fs::set_permissions(path, perms);
    }
}

/// Whether `dest` already is `src`: a hard link to it, or the copy made where links can't be
/// (another file system, or another mount of it, as with a container's bind mounts). Without
/// the second, every build there would copy and hash each artifact again.
fn already_linked(src: &Path, dest: &Path) -> bool {
    hard_linked(src, dest) || igel_common::fsutil::is_copy_of(src, dest)
}

fn hard_linked(src: &Path, dest: &Path) -> bool {
    same_file::is_same_file(src, dest).unwrap_or(false)
}

impl CasStore {
    pub fn new(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(root.join("objects"))?;

        let stats_file = root.join("stats.json");
        if !stats_file.exists() {
            let stats = CasStats::default();
            let json = serde_json::to_string_pretty(&stats)?;
            fs::write(&stats_file, json)?;
        }

        Ok(Self {
            root,
            stats_lock: Arc::new(Mutex::new(())),
            pending: Arc::new(PendingStats::default()),
            verify: false,
            remote: None,
            publisher: None,
        })
    }

    pub fn with_remote(self, root: PathBuf, write: bool) -> Self {
        self.with_remote_store(crate::remote::Remote::new(root, write))
    }

    pub fn with_remote_store(mut self, remote: crate::remote::Remote) -> Self {
        if remote.writes() {
            self.publisher = Some(Arc::new(crate::remote::Publisher::start(2)));
        }
        self.remote = Some(remote);
        self
    }

    fn pull(&self, cache_key: &str, slot: crate::remote::Slot) -> bool {
        self.remote
            .as_ref()
            .is_some_and(|r| r.pull(cache_key, &self.object_dir(cache_key), slot))
    }

    fn publish(&self, cache_key: &str, slot: crate::remote::Slot) {
        let Some(r) = &self.remote else { return };
        if !r.writes() {
            return;
        }
        let dir = self.object_dir(cache_key);
        match (&self.publisher, slot) {
            (Some(p), crate::remote::Slot::Artifacts | crate::remote::Slot::Tree) => {
                p.submit(r.clone(), cache_key.to_string(), dir, slot)
            }
            _ => r.push(cache_key, &dir, slot),
        }
    }

    pub fn flush_remote(&self) {
        if let Some(p) = &self.publisher {
            p.flush();
        }
    }

    pub fn with_verify(mut self, verify: bool) -> Self {
        self.verify = verify;
        self
    }

    fn is_intact(entry: &CasEntry, cache_key: &str) -> bool {
        entry.manifest.cache_key == cache_key
            && entry.manifest.artifacts.iter().all(|a| {
                fs::metadata(entry.object_dir.join(&a.filename))
                    .map(|m| m.len() == a.size)
                    .unwrap_or(false)
            })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn object_dir(&self, cache_key: &str) -> PathBuf {
        let prefix = if cache_key.len() >= 2 {
            &cache_key[0..2]
        } else {
            cache_key
        };
        self.root.join("objects").join(prefix).join(cache_key)
    }

    pub fn manifest(&self, cache_key: &str) -> Option<ArtifactManifest> {
        let text = fs::read_to_string(self.object_dir(cache_key).join("manifest.json")).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn get(&self, cache_key: &str) -> Option<CasEntry> {
        let dir = self.object_dir(cache_key);
        let content = match fs::read_to_string(dir.join("manifest.json")) {
            Ok(c) => c,
            Err(_) if self.pull(cache_key, crate::remote::Slot::Artifacts) => {
                fs::read_to_string(dir.join("manifest.json")).ok()?
            }
            Err(_) => return None,
        };
        let mut manifest = serde_json::from_str::<ArtifactManifest>(&content).ok()?;
        if manifest.compressed {
            manifest = self.inflate(&dir, manifest)?;
        }
        Some(CasEntry {
            manifest,
            object_dir: dir,
        })
    }

    fn inflate(&self, dir: &Path, mut manifest: ArtifactManifest) -> Option<ArtifactManifest> {
        for a in &manifest.artifacts {
            let dest = dir.join(&a.filename);
            if fs::metadata(&dest).is_ok_and(|m| m.len() == a.size) {
                continue;
            }
            let packed = dir.join(format!("{}.zst", a.filename));
            let tmp = dir.join(format!(
                ".{}.{}.{}.tmp",
                a.filename,
                std::process::id(),
                unique()
            ));
            let result = (|| -> std::io::Result<()> {
                let mut input = File::open(&packed)?;
                let mut out = File::create(&tmp)?;
                zstd::stream::copy_decode(&mut input, &mut out)?;
                Ok(())
            })();
            let sound = result.is_ok()
                && fs::metadata(&tmp).is_ok_and(|m| m.len() == a.size)
                && hash_file(&tmp).is_ok_and(|h| h == a.hash);
            if !sound {
                let _ = fs::remove_file(&tmp);
                debug!("compressed entry {} is damaged", manifest.cache_key);
                return None;
            }
            make_read_only(&tmp);
            igel_common::fsutil::rename_over(&tmp, &dest).ok()?;
        }
        manifest.compressed = false;
        manifest.stored_size = None;
        let tmp = dir.join(format!(".manifest.{}.{}.tmp", std::process::id(), unique()));
        fs::write(&tmp, serde_json::to_string(&manifest).ok()?).ok()?;
        fs::rename(&tmp, dir.join("manifest.json")).ok()?;
        for a in &manifest.artifacts {
            let _ = fs::remove_file(dir.join(format!("{}.zst", a.filename)));
        }
        Some(manifest)
    }

    pub fn compress(&self, dir: &Path) -> Result<u64> {
        let content = fs::read_to_string(dir.join("manifest.json"))?;
        let mut manifest: ArtifactManifest = serde_json::from_str(&content)?;
        if manifest.compressed || manifest.artifacts.is_empty() {
            return Ok(0);
        }
        let mut packed_total = 0;
        let mut written = Vec::new();
        for a in &manifest.artifacts {
            let src = dir.join(&a.filename);
            let dest = dir.join(format!("{}.zst", a.filename));
            let tmp = dir.join(format!(
                ".{}.zst.{}.{}.tmp",
                a.filename,
                std::process::id(),
                unique()
            ));
            let mut input = File::open(&src)?;
            let out = File::create(&tmp)?;
            let mut enc = zstd::stream::Encoder::new(out, 3)?;
            std::io::copy(&mut input, &mut enc)?;
            enc.finish()?;
            packed_total += fs::metadata(&tmp)?.len();
            fs::rename(&tmp, &dest)?;
            written.push(dest);
        }
        if packed_total > manifest.total_size - manifest.total_size / 10 {
            for w in written {
                let _ = fs::remove_file(w);
            }
            return Ok(0);
        }
        manifest.compressed = true;
        manifest.stored_size = Some(packed_total);
        let tmp = dir.join(format!(".manifest.{}.{}.tmp", std::process::id(), unique()));
        fs::write(&tmp, serde_json::to_string(&manifest)?)?;
        let used = fs::metadata(dir.join("manifest.json"))?.modified()?;
        fs::rename(&tmp, dir.join("manifest.json"))?;
        let _ = filetime::set_file_mtime(
            dir.join("manifest.json"),
            filetime::FileTime::from_system_time(used),
        );
        for a in &manifest.artifacts {
            let _ = fs::remove_file(dir.join(&a.filename));
        }
        Ok(manifest.total_size - packed_total)
    }

    pub fn contains(&self, cache_key: &str) -> bool {
        self.object_dir(cache_key).join("manifest.json").is_file()
    }

    pub fn contains_tree(&self, cache_key: &str) -> bool {
        let dir = self.object_dir(cache_key);
        dir.join("tree.json").is_file() && dir.join("value.json").is_file()
    }

    /// Temp names + rename, manifest last: objects are hard-linked into target dirs and must never change
    /// in place, and readers never see a half-written entry.
    pub fn put(
        &self,
        cache_key: &str,
        crate_name: &str,
        crate_version: &str,
        source_files: &[PathBuf],
    ) -> Result<()> {
        self.put_unit(
            cache_key,
            crate_name,
            crate_version,
            source_files,
            &PutOptions::default(),
        )
        .map(drop)
    }

    /// Like [`Self::put`], but takes the files themselves (hard links) instead of copies.
    pub fn put_adopted(
        &self,
        cache_key: &str,
        crate_name: &str,
        crate_version: &str,
        source_files: &[PathBuf],
    ) -> Result<()> {
        let options = PutOptions {
            adopt: true,
            ..PutOptions::default()
        };
        self.put_unit(cache_key, crate_name, crate_version, source_files, &options)
            .map(drop)
    }

    pub fn put_unit(
        &self,
        cache_key: &str,
        crate_name: &str,
        crate_version: &str,
        source_files: &[PathBuf],
        options: &PutOptions,
    ) -> Result<PutOutcome> {
        let dir = self.object_dir(cache_key);
        // Objects are immutable: the first result for a key stands, unless it is damaged or (with
        // `replace_stale`) compiled against dependency builds the store no longer holds.
        if let Some(existing) = self.get(cache_key) {
            if Self::is_intact(&existing, cache_key) {
                let current = existing
                    .manifest
                    .matches_dependencies(&options.compiled_against);
                if current || !options.replace_stale {
                    self.publish(cache_key, crate::remote::Slot::Artifacts);
                    return Ok(if current {
                        PutOutcome::Kept
                    } else {
                        PutOutcome::KeptStale
                    });
                }
                debug!(
                    "replacing {cache_key} ({} {}): it was compiled against other builds of its dependencies",
                    existing.manifest.crate_name, existing.manifest.crate_version
                );
                let _ = fs::remove_file(dir.join("manifest.json"));
            }
            let _ = fs::remove_dir_all(&dir);
        }
        fs::create_dir_all(&dir)?;

        let mut artifacts = Vec::new();
        let mut total_size = 0;

        for src in source_files {
            if !src.exists() || src.is_dir() {
                continue;
            }

            let file_name = src
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();

            let dest = dir.join(&file_name);
            let tmp = dir.join(format!(
                ".{file_name}.{}.{}.tmp",
                std::process::id(),
                unique()
            ));
            let _ = fs::remove_file(&tmp);
            if !options.adopt || fs::hard_link(src, &tmp).is_err() {
                fs::copy(src, &tmp)?;
            }
            let size = fs::metadata(&tmp)?.len();
            total_size += size;
            let hash = hash_file(&tmp)?;
            make_read_only(&tmp);
            fs::rename(&tmp, &dest)?;

            artifacts.push(ArtifactEntry {
                filename: file_name,
                size,
                hash,
            });
        }

        let stored_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let manifest = ArtifactManifest {
            cache_key: cache_key.to_string(),
            crate_name: crate_name.to_string(),
            crate_version: crate_version.to_string(),
            artifacts,
            stored_at,
            total_size,
            compressed: false,
            stored_size: None,
            compiled_against: options.compiled_against.clone(),
        };

        let tmp_manifest = dir.join(format!(".manifest.{}.{}.tmp", std::process::id(), unique()));
        fs::write(&tmp_manifest, serde_json::to_string(&manifest)?)?;
        fs::rename(&tmp_manifest, dir.join("manifest.json"))?;

        self.pending.entries.fetch_add(1, Ordering::Relaxed);
        self.pending.bytes.fetch_add(total_size, Ordering::Relaxed);
        self.publish(cache_key, crate::remote::Slot::Artifacts);
        Ok(PutOutcome::Stored)
    }

    pub fn put_dir_tree(&self, cache_key: &str, source_dir: &Path) -> Result<()> {
        let object_dir = self.object_dir(cache_key);
        if let Ok(text) = fs::read_to_string(object_dir.join("tree.json"))
            && let Ok(rels) = serde_json::from_str::<Vec<String>>(&text)
            && rels
                .iter()
                .all(|r| object_dir.join("tree").join(r).is_file())
        {
            self.publish(cache_key, crate::remote::Slot::Tree);
            return Ok(());
        }
        fs::create_dir_all(&object_dir)?;
        let tree_root = object_dir.join("tree");
        // Build beside and swap by rename so concurrent readers never see a half-deleted tree.
        let staging = object_dir.join(format!(".tree.{}.tmp", std::process::id()));
        if staging.exists() {
            fs::remove_dir_all(&staging)?;
        }
        fs::create_dir_all(&staging)?;

        let mut relative_paths = Vec::new();
        for entry in walkdir::WalkDir::new(source_dir)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Ok(rel) = path.strip_prefix(source_dir) else {
                continue;
            };
            let dest = staging.join(rel);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(path, &dest)?;
            make_read_only(&dest);
            relative_paths.push(rel.to_string_lossy().into_owned());
        }

        if tree_root.exists() {
            fs::remove_dir_all(&tree_root)?;
        }
        fs::rename(&staging, &tree_root)?;
        fs::write(
            object_dir.join("tree.json"),
            serde_json::to_string(&relative_paths)?,
        )?;
        self.publish(cache_key, crate::remote::Slot::Tree);
        Ok(())
    }

    pub fn restore_dir_tree(&self, cache_key: &str, dest_dir: &Path) -> Result<bool> {
        let object_dir = self.object_dir(cache_key);
        let manifest_path = object_dir.join("tree.json");
        if !manifest_path.exists() && !self.pull(cache_key, crate::remote::Slot::Tree) {
            return Ok(false);
        }
        let tree_root = object_dir.join("tree");
        let relative_paths: Vec<String> =
            serde_json::from_str(&fs::read_to_string(&manifest_path)?)?;

        fs::create_dir_all(dest_dir)?;
        for rel in relative_paths {
            let src = tree_root.join(&rel);
            if !src.exists() {
                return Ok(false);
            }
            let dest = dest_dir.join(&rel);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            if already_linked(&src, &dest) {
                continue;
            }
            igel_common::fsutil::replace_with_link(&src, &dest)?;
        }
        Ok(true)
    }

    pub fn link_to(&self, cache_key: &str, target_dir: &Path) -> Result<Vec<PathBuf>> {
        self.link_entry(cache_key, target_dir).map(|l| l.files)
    }

    pub fn link_entry(&self, cache_key: &str, target_dir: &Path) -> Result<Linked> {
        let entry = self
            .get(cache_key)
            .ok_or_else(|| CasError::Integrity(format!("Cache key {} not found", cache_key)))?;

        fs::create_dir_all(target_dir)?;
        let mut linked_files = Vec::new();

        if !Self::is_intact(&entry, cache_key) {
            return Err(CasError::Integrity(format!(
                "entry {cache_key} is damaged or is not the requested one"
            )));
        }

        for artifact in &entry.manifest.artifacts {
            let src = entry.object_dir.join(&artifact.filename);
            let dest = target_dir.join(&artifact.filename);

            if self.verify && hash_file(&src)? != artifact.hash {
                return Err(CasError::Integrity(format!(
                    "artifact {} of {cache_key} does not match its recorded hash",
                    artifact.filename
                )));
            }
            if already_linked(&src, &dest) {
                linked_files.push(dest);
                continue;
            }

            igel_common::fsutil::replace_with_link(&src, &dest)?;
            if hard_linked(&src, &dest) {
                linked_files.push(dest);
                continue;
            }
            debug!("Hardlink failed for {}, fell back to copy.", src.display());

            let dest_hash = hash_file(&dest)?;

            if dest_hash != artifact.hash {
                return Err(CasError::Integrity(format!(
                    "Artifact {} hash mismatch after link/copy",
                    artifact.filename
                )));
            }

            linked_files.push(dest);
        }

        self.pending.hits.fetch_add(1, Ordering::Relaxed);
        touch_if_stale(&entry.object_dir.join("manifest.json"));
        Ok(Linked {
            files: linked_files,
            manifest: entry.manifest,
        })
    }

    pub fn stats(&self) -> Result<CasStats> {
        let stats_path = self.root.join("stats.json");
        if stats_path.exists() {
            let content = fs::read_to_string(&stats_path)?;
            let stats = serde_json::from_str(&content)?;
            Ok(stats)
        } else {
            Ok(CasStats::default())
        }
    }

    pub(crate) fn save_stats(&self, stats: &CasStats) -> Result<()> {
        let json = serde_json::to_string_pretty(stats)?;
        let temp_path = self.root.join("stats.json.tmp");
        fs::write(&temp_path, json.as_bytes())?;
        fs::rename(temp_path, self.root.join("stats.json"))?;
        Ok(())
    }

    pub fn get_value<T: serde::de::DeserializeOwned>(&self, cache_key: &str) -> Option<T> {
        let path = self.object_dir(cache_key).join("value.json");
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) if self.pull(cache_key, crate::remote::Slot::Value) => {
                fs::read_to_string(&path).ok()?
            }
            Err(_) => return None,
        };
        serde_json::from_str(&content).ok()
    }

    pub fn put_value<T: Serialize>(&self, cache_key: &str, value: &T) -> Result<()> {
        let dir = self.object_dir(cache_key);
        fs::create_dir_all(&dir)?;
        let json = serde_json::to_string(value)?;
        let tmp = dir.join(format!(".value.{}.tmp", std::process::id()));
        fs::write(&tmp, json)?;
        fs::rename(&tmp, dir.join("value.json"))?;
        self.publish(cache_key, crate::remote::Slot::Value);
        Ok(())
    }

    pub fn record_hit(&self) {
        self.pending.hits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_miss(&self) {
        self.pending.misses.fetch_add(1, Ordering::Relaxed);
    }

    pub fn flush_stats(&self) {
        self.flush_remote();
        let hits = self.pending.hits.swap(0, Ordering::Relaxed);
        let misses = self.pending.misses.swap(0, Ordering::Relaxed);
        let entries = self.pending.entries.swap(0, Ordering::Relaxed);
        let bytes = self.pending.bytes.swap(0, Ordering::Relaxed);
        if hits == 0 && misses == 0 && entries == 0 && bytes == 0 {
            return;
        }
        let _guard = self.stats_lock.lock().unwrap();
        let mut stats = self.stats().unwrap_or_default();
        stats.hits += hits;
        stats.misses += misses;
        stats.total_entries += entries;
        stats.total_size_bytes += bytes;
        let _ = self.save_stats(&stats);
    }
}

/// Bumps the manifest's mtime (GC's last-use time), only when hours stale.
fn touch_if_stale(manifest: &Path) {
    const FRESH_FOR: std::time::Duration = std::time::Duration::from_secs(6 * 3600);
    let stale = fs::metadata(manifest)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age > FRESH_FOR);
    if stale {
        let _ = filetime::set_file_mtime(manifest, filetime::FileTime::now());
    }
}

fn unique() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

pub(crate) fn hash_file(path: &Path) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(File::open(path)?)?;
    Ok(hasher.finalize().to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_cas_store() {
        let dir = tempdir().unwrap();
        let store = CasStore::new(dir.path().to_path_buf()).unwrap();

        let src_dir = tempdir().unwrap();
        let file1 = src_dir.path().join("libfoo.rlib");
        fs::write(&file1, "foo content").unwrap();

        let sources = vec![file1];

        let key = "abcdef123456";
        store.put(key, "foo", "1.0.0", &sources).unwrap();

        let entry = store.get(key).unwrap();
        assert_eq!(entry.manifest.crate_name, "foo");
        assert_eq!(entry.manifest.artifacts.len(), 1);

        let target_dir = tempdir().unwrap();
        let linked = store.link_to(key, target_dir.path()).unwrap();
        assert_eq!(linked.len(), 1);

        let content = fs::read_to_string(&linked[0]).unwrap();
        assert_eq!(content, "foo content");
    }

    #[test]
    fn objects_are_immutable_and_never_written_through_old_hardlinks() {
        let dir = tempdir().unwrap();
        let store = CasStore::new(dir.path().to_path_buf()).unwrap();
        let src_dir = tempdir().unwrap();
        let file = src_dir.path().join("libfoo.rlib");

        fs::write(&file, "v1").unwrap();
        store
            .put("keyv", "foo", "1.0.0", std::slice::from_ref(&file))
            .unwrap();

        let build = tempdir().unwrap();
        let linked = store.link_to("keyv", build.path()).unwrap();

        fs::write(&file, "v2").unwrap();
        store.put("keyv", "foo", "1.0.0", &[file]).unwrap();

        assert_eq!(fs::read_to_string(&linked[0]).unwrap(), "v1");
        let other_build = tempdir().unwrap();
        let relinked = store.link_to("keyv", other_build.path()).unwrap();
        assert_eq!(fs::read_to_string(&relinked[0]).unwrap(), "v1");

        assert!(
            fs::metadata(&linked[0]).unwrap().permissions().readonly(),
            "stored objects must be read-only"
        );
        let probe = src_dir.path().join("probe");
        fs::write(&probe, "x").unwrap();
        make_read_only(&probe);
        if fs::OpenOptions::new().write(true).open(&probe).is_err() {
            assert!(
                fs::OpenOptions::new().write(true).open(&linked[0]).is_err(),
                "stored objects must be read-only"
            );
        }
    }

    #[test]
    fn adopted_files_become_the_object_without_a_copy() {
        let dir = tempdir().unwrap();
        let store = CasStore::new(dir.path().to_path_buf()).unwrap();
        let build = tempdir().unwrap();
        let out = build.path().join("libfoo-abc.rlib");
        fs::write(&out, "compiled").unwrap();
        store
            .put_adopted("k", "foo", "1.0.0", std::slice::from_ref(&out))
            .unwrap();
        let obj = store.object_dir("k").join("libfoo-abc.rlib");
        assert!(already_linked(&out, &obj), "the output itself is stored");
        assert!(fs::metadata(&out).unwrap().permissions().readonly());
        let linked = store.link_to("k", build.path()).unwrap();
        assert_eq!(fs::read_to_string(&linked[0]).unwrap(), "compiled");
    }

    #[test]
    fn a_damaged_entry_is_a_miss_and_can_be_published_again() {
        let dir = tempdir().unwrap();
        let store = CasStore::new(dir.path().to_path_buf()).unwrap();
        let src = tempdir().unwrap();
        let file = src.path().join("libbar.rlib");
        fs::write(&file, "intact").unwrap();
        store
            .put("k", "bar", "1.0.0", std::slice::from_ref(&file))
            .unwrap();

        let obj = store.object_dir("k").join("libbar.rlib");
        fs::remove_file(&obj).unwrap();
        assert!(store.link_to("k", tempdir().unwrap().path()).is_err());

        store.put("k", "bar", "1.0.0", &[file]).unwrap();
        let target = tempdir().unwrap();
        let ok = store.link_to("k", target.path()).unwrap();
        assert_eq!(fs::read_to_string(&ok[0]).unwrap(), "intact");
    }

    fn deps(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn an_entry_records_what_it_was_compiled_against() {
        let dir = tempdir().unwrap();
        let store = CasStore::new(dir.path().to_path_buf()).unwrap();
        let build = tempdir().unwrap();
        let rmeta = build.path().join("libmid-1.rmeta");
        let rlib = build.path().join("libmid-1.rlib");
        fs::write(&rmeta, "meta").unwrap();
        fs::write(&rlib, "lib").unwrap();
        let against = deps(&[("libdep-1.rmeta", "aaaa")]);
        let options = PutOptions {
            adopt: true,
            compiled_against: against.clone(),
            replace_stale: true,
        };
        let outcome = store
            .put_unit("k", "mid", "1.0.0", &[rlib, rmeta.clone()], &options)
            .unwrap();
        assert_eq!(outcome, PutOutcome::Stored);

        let target = tempdir().unwrap();
        let linked = store.link_entry("k", target.path()).unwrap();
        assert!(linked.manifest.matches_dependencies(&against));
        assert!(
            !linked
                .manifest
                .matches_dependencies(&deps(&[("libdep-1.rmeta", "bbbb")]))
        );
        assert!(!linked.manifest.matches_dependencies(&BTreeMap::new()));
        let content = linked.manifest.content().unwrap();
        assert_eq!(content.file, "libmid-1.rmeta");
        assert_eq!(content.id, hash_file(&rmeta).unwrap());
        assert_eq!(content_of_files(&linked.files), Some(content));
    }

    #[test]
    fn a_stale_entry_is_replaced_only_when_the_caller_knows_it_is() {
        let dir = tempdir().unwrap();
        let store = CasStore::new(dir.path().to_path_buf()).unwrap();
        let src = tempdir().unwrap();
        let file = src.path().join("libmid-1.rlib");
        fs::write(&file, "against old").unwrap();
        let put = |against: &[(&str, &str)], replace_stale: bool| {
            store
                .put_unit(
                    "k",
                    "mid",
                    "1.0.0",
                    std::slice::from_ref(&file),
                    &PutOptions {
                        adopt: false,
                        compiled_against: deps(against),
                        replace_stale,
                    },
                )
                .unwrap()
        };
        assert_eq!(put(&[("dep-1", "old")], true), PutOutcome::Stored);
        let earlier_dir = tempdir().unwrap();
        let earlier = store.link_to("k", earlier_dir.path()).unwrap();

        fs::write(&file, "rebuilt").unwrap();
        assert_eq!(put(&[("dep-1", "old")], true), PutOutcome::Kept);
        assert_eq!(put(&[("dep-1", "new")], false), PutOutcome::KeptStale);
        let read = |s: &CasStore| {
            let target = tempdir().unwrap();
            let out = s.link_to("k", target.path()).unwrap();
            fs::read_to_string(&out[0]).unwrap()
        };
        assert_eq!(read(&store), "against old");

        fs::write(&file, "against new").unwrap();
        assert_eq!(put(&[("dep-1", "new")], true), PutOutcome::Stored);
        assert_eq!(read(&store), "against new");
        let entry = store.get("k").unwrap();
        assert!(
            entry
                .manifest
                .matches_dependencies(&deps(&[("dep-1", "new")]))
        );
        assert_eq!(fs::read_to_string(&earlier[0]).unwrap(), "against old");
    }

    #[test]
    fn the_content_id_is_the_metadata_else_the_library() {
        let p = |names: &[&str]| -> Vec<PathBuf> { names.iter().map(PathBuf::from).collect() };
        let pick = |names: &[&str]| {
            let files = p(names);
            content_file(files.iter().map(PathBuf::as_path)).map(Path::to_path_buf)
        };
        assert_eq!(
            pick(&["x.d", "libx.rlib", "libx.rmeta"]),
            Some(PathBuf::from("libx.rmeta"))
        );
        assert_eq!(
            pick(&["x.d", "libx.rlib"]),
            Some(PathBuf::from("libx.rlib"))
        );
        assert_eq!(
            pick(&["x.d", "x.dll", "x.dll.lib"]),
            Some(PathBuf::from("x.dll"))
        );
        assert_eq!(pick(&["x.d", "x.exe", "x.pdb"]), None);
        assert_eq!(pick(&[]), None);
    }

    #[test]
    fn entries_written_before_records_existed_still_read() {
        let dir = tempdir().unwrap();
        let store = CasStore::new(dir.path().to_path_buf()).unwrap();
        let obj = store.object_dir("old");
        fs::create_dir_all(&obj).unwrap();
        fs::write(obj.join("liba.rlib"), "a").unwrap();
        fs::write(
            obj.join("manifest.json"),
            format!(
                r#"{{"cache_key":"old","crate_name":"a","crate_version":"1.0.0","artifacts":[{{"filename":"liba.rlib","size":1,"hash":"{}"}}],"stored_at":0,"total_size":1}}"#,
                hash_file(&obj.join("liba.rlib")).unwrap()
            ),
        )
        .unwrap();
        let target = tempdir().unwrap();
        let linked = store.link_entry("old", target.path()).unwrap();
        assert!(linked.manifest.compiled_against.is_empty());
        assert!(linked.manifest.matches_dependencies(&BTreeMap::new()));
        assert!(!linked.manifest.matches_dependencies(&deps(&[("b-1", "x")])));
    }

    #[test]
    fn verification_catches_content_that_changed_without_changing_size() {
        let dir = tempdir().unwrap();
        let store = CasStore::new(dir.path().to_path_buf())
            .unwrap()
            .with_verify(true);
        let src = tempdir().unwrap();
        let file = src.path().join("libbaz.rlib");
        fs::write(&file, "aaaa").unwrap();
        store.put("kv", "baz", "1.0.0", &[file]).unwrap();
        assert!(store.link_to("kv", tempdir().unwrap().path()).is_ok());

        let obj = store.object_dir("kv").join("libbaz.rlib");
        #[allow(clippy::permissions_set_readonly_false)]
        {
            let mut perms = fs::metadata(&obj).unwrap().permissions();
            perms.set_readonly(false);
            fs::set_permissions(&obj, perms).unwrap();
        }
        fs::write(&obj, "bbbb").unwrap();
        assert!(store.link_to("kv", tempdir().unwrap().path()).is_err());
        let lax = CasStore::new(dir.path().to_path_buf()).unwrap();
        assert!(lax.link_to("kv", tempdir().unwrap().path()).is_ok());
    }

    #[test]
    fn stats_are_batched_until_flushed() {
        let dir = tempdir().unwrap();
        let store = CasStore::new(dir.path().to_path_buf()).unwrap();
        let src = tempdir().unwrap();
        let f = src.path().join("a.rlib");
        fs::write(&f, "abc").unwrap();
        store.put("k1", "a", "1", &[f]).unwrap();
        store.link_to("k1", tempdir().unwrap().path()).unwrap();
        store.record_miss();

        assert_eq!(
            store.stats().unwrap().hits,
            0,
            "nothing written before flush"
        );
        store.flush_stats();
        let stats = store.stats().unwrap();
        assert_eq!((stats.hits, stats.misses, stats.total_entries), (1, 1, 1));
        store.flush_stats();
        assert_eq!(store.stats().unwrap().hits, 1);
    }
}
