use serde::{Deserialize, Serialize};
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

/// Content-Addressable Store for Rune build artifacts.
///
/// Stores compiled artifacts (`.rlib`, `.rmeta`, `.d`, binaries) keyed by
/// a content hash of all inputs. Enables artifact reuse across projects
/// and machines.
#[derive(Debug, Clone)]
pub struct CasStore {
    root: PathBuf,
    /// Guards read-modify-write access to `stats.json` against the build
    /// executor's concurrent worker threads within this process. Cross-process
    /// races (e.g. two `rune build` invocations at once) can still interleave;
    /// stats are diagnostic counters, not correctness-critical state, so that
    /// residual race is an accepted tradeoff rather than a bug to chase.
    stats_lock: Arc<Mutex<()>>,
    /// Counters accumulated in memory during a build and merged into
    /// `stats.json` once by [`CasStore::flush_stats`]. Rewriting the stats
    /// file after every single cache hit made a fully cached build of a few
    /// hundred crates do a few hundred read-modify-write round trips on one
    /// file for no benefit.
    pending: Arc<PendingStats>,
    /// Re-hash every artifact on every hit (`cas.verify`). Off by default: an
    /// entry is always verified against its key and the sizes in its manifest;
    /// this additionally catches bit rot and tampering, at the cost of reading
    /// every restored file.
    verify: bool,
    /// A store shared with other machines, consulted on a local miss and
    /// published to after a store (`cas.remote`).
    remote: Option<crate::remote::Remote>,
    /// Copies new entries to `remote` off the compilers' critical path.
    publisher: Option<Arc<crate::remote::Publisher>>,
}

#[derive(Debug, Default)]
struct PendingStats {
    hits: AtomicU64,
    misses: AtomicU64,
    entries: AtomicU64,
    bytes: AtomicU64,
}

/// Metadata about a cached artifact set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactManifest {
    /// The fingerprint/cache key that produced these artifacts.
    pub cache_key: String,
    /// Crate name.
    pub crate_name: String,
    /// Crate version.
    pub crate_version: String,
    /// List of artifact files in this entry.
    pub artifacts: Vec<ArtifactEntry>,
    /// When this entry was stored.
    pub stored_at: u64,
    /// Size in bytes of all artifacts combined.
    pub total_size: u64,
    /// The artifacts are stored zstd-compressed, as `<filename>.zst`: the
    /// entry went unused for a while (see [`CasStore::compress`]). The next
    /// use decompresses them again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub compressed: bool,
    /// Bytes on disk while compressed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stored_size: Option<u64>,
}

impl ArtifactManifest {
    /// Bytes the entry's artifacts take on disk.
    pub fn disk_size(&self) -> u64 {
        match self.stored_size {
            Some(n) if self.compressed => n,
            _ => self.total_size,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactEntry {
    /// Filename (e.g., "libfoo-abc123.rlib")
    pub filename: String,
    /// Size in bytes.
    pub size: u64,
    /// Blake3 hash of the artifact file.
    pub hash: String,
}

/// Statistics about CAS usage.
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

/// Stored objects are never modified: taking away write permission makes a
/// stray in-place write (through a hard link from a project directory) fail
/// loudly instead of silently changing an object every project shares.
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

/// True if `dest` is already a hardlink to `src` (same inode on the same
/// device), meaning there is nothing left to do to "link" it. (Elsewhere the
/// link is simply made again, which is correct, only slower.)
fn already_linked(src: &Path, dest: &Path) -> bool {
    same_file::is_same_file(src, dest).unwrap_or(false)
}

impl CasStore {
    /// Create a new CAS store, initializing directories if needed.
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

    /// Consults (and, if `write`, publishes to) a store shared with other
    /// machines. See [`crate::remote`].
    pub fn with_remote(self, root: PathBuf, write: bool) -> Self {
        self.with_remote_store(crate::remote::Remote::new(root, write))
    }

    /// Like [`Self::with_remote`], for any kind of shared store.
    pub fn with_remote_store(mut self, remote: crate::remote::Remote) -> Self {
        if remote.writes() {
            self.publisher = Some(Arc::new(crate::remote::Publisher::start(2)));
        }
        self.remote = Some(remote);
        self
    }

    /// Makes `slot` of `cache_key` local if only the shared store has it.
    fn pull(&self, cache_key: &str, slot: crate::remote::Slot) -> bool {
        self.remote
            .as_ref()
            .is_some_and(|r| r.pull(cache_key, &self.object_dir(cache_key), slot))
    }

    /// Publishes `slot` of `cache_key` to the shared store: in the background
    /// for the big slots, at once for a small value (so a process that ends
    /// right after storing one does not lose it).
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

    /// Waits for background publishing to the shared store to finish.
    pub fn flush_remote(&self) {
        if let Some(p) = &self.publisher {
            p.flush();
        }
    }

    /// Verifies the content hash of every artifact on every hit.
    pub fn with_verify(mut self, verify: bool) -> Self {
        self.verify = verify;
        self
    }

    /// An entry is usable only if it is the one asked for and everything its
    /// manifest lists is present at the recorded size. Anything else — a
    /// half-collected entry, damage on disk — is a miss, never a hit.
    fn is_intact(entry: &CasEntry, cache_key: &str) -> bool {
        entry.manifest.cache_key == cache_key
            && entry.manifest.artifacts.iter().all(|a| {
                fs::metadata(entry.object_dir.join(&a.filename))
                    .map(|m| m.len() == a.size)
                    .unwrap_or(false)
            })
    }

    /// Return the root path.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Compute the object directory for a given cache key (sharded by first 2 chars).
    pub fn object_dir(&self, cache_key: &str) -> PathBuf {
        let prefix = if cache_key.len() >= 2 {
            &cache_key[0..2]
        } else {
            cache_key
        };
        self.root.join("objects").join(prefix).join(cache_key)
    }

    /// Check if artifacts exist for this key and return the entry.
    pub fn get(&self, cache_key: &str) -> Option<CasEntry> {
        let dir = self.object_dir(cache_key);
        // One read instead of `exists()` + read: a miss (the common case on a
        // cold build) costs a single failed `open`.
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

    /// Decompresses an entry [`Self::compress`] compressed, so it can be
    /// hard-linked again. Each file is checked against its recorded hash and
    /// renamed into place; the manifest is rewritten last. `None` if the
    /// entry is damaged (a miss, like any damaged entry).
    fn inflate(&self, dir: &Path, mut manifest: ArtifactManifest) -> Option<ArtifactManifest> {
        for a in &manifest.artifacts {
            let dest = dir.join(&a.filename);
            // Another process may have decompressed it already.
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
            fs::rename(&tmp, &dest).ok()?;
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

    /// Compresses the artifacts of the entry in `dir` with zstd, keeping them
    /// only when that saves at least a tenth. Returns the bytes saved. The
    /// entry stays valid throughout: the compressed files and the manifest
    /// that names them are written before the originals are removed.
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
        // Keep the entry's last-use time: compressing is not a use.
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

    /// Whether an entry exists for `cache_key`.
    pub fn contains(&self, cache_key: &str) -> bool {
        self.object_dir(cache_key).join("manifest.json").is_file()
    }

    /// Whether this store (not a remote one) has both a directory tree and a
    /// value under `cache_key` — a build script's complete result.
    pub fn contains_tree(&self, cache_key: &str) -> bool {
        let dir = self.object_dir(cache_key);
        dir.join("tree.json").is_file() && dir.join("value.json").is_file()
    }

    /// Copy compiled artifacts into the CAS store.
    ///
    /// Every file is written to a temporary name and renamed into place, and
    /// the manifest is written last. This matters because objects are
    /// hard-linked into project target directories: overwriting one *in
    /// place* would silently change every build directory linked to it, while
    /// a rename leaves those links pointing at the old, intact inode. It also
    /// means a concurrent reader never sees a half-written entry.
    pub fn put(
        &self,
        cache_key: &str,
        crate_name: &str,
        crate_version: &str,
        source_files: &[PathBuf],
    ) -> Result<()> {
        self.put_files(cache_key, crate_name, crate_version, source_files, false)
    }

    /// Like [`Self::put`], but the store takes the files themselves (hard
    /// links; a copy only across file systems) instead of copies of them, and
    /// makes them read-only. For outputs the caller will never write again in
    /// place — a compiler's fresh outputs, which a later compilation unlinks
    /// before writing new ones, exactly as it does for restored outputs — this
    /// saves writing every artifact twice (a debug binary can be 100+ MB).
    pub fn put_adopted(
        &self,
        cache_key: &str,
        crate_name: &str,
        crate_version: &str,
        source_files: &[PathBuf],
    ) -> Result<()> {
        self.put_files(cache_key, crate_name, crate_version, source_files, true)
    }

    fn put_files(
        &self,
        cache_key: &str,
        crate_name: &str,
        crate_version: &str,
        source_files: &[PathBuf],
        adopt: bool,
    ) -> Result<()> {
        let dir = self.object_dir(cache_key);
        // Objects are immutable: the first published result for a key stands,
        // and a later build of the same key (a forced rebuild, a racing
        // process) never changes it. A *damaged* entry is not an entry, though:
        // it is removed so it can be published again.
        if let Some(existing) = self.get(cache_key) {
            if Self::is_intact(&existing, cache_key) {
                self.publish(cache_key, crate::remote::Slot::Artifacts);
                return Ok(());
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
            let tmp = dir.join(format!(".{file_name}.{}.tmp", std::process::id()));
            let _ = fs::remove_file(&tmp);
            if !adopt || fs::hard_link(src, &tmp).is_err() {
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
        };

        let tmp_manifest = dir.join(format!(".manifest.{}.tmp", std::process::id()));
        fs::write(&tmp_manifest, serde_json::to_string(&manifest)?)?;
        fs::rename(&tmp_manifest, dir.join("manifest.json"))?;

        self.pending.entries.fetch_add(1, Ordering::Relaxed);
        self.pending.bytes.fetch_add(total_size, Ordering::Relaxed);
        self.publish(cache_key, crate::remote::Slot::Artifacts);
        Ok(())
    }

    /// Caches an entire directory tree (recursively) under `cache_key` —
    /// used for a build script's `OUT_DIR`, which can contain arbitrary
    /// generated files (not just the flat set of `.rlib`/`.rmeta`/`.d`
    /// outputs [`Self::put`] handles) that later `include!()`-based
    /// compilation depends on. Skipping a build script's re-execution
    /// without restoring these is only safe as long as the local
    /// `.rune/target` directory happens to still hold them from whenever it
    /// last actually ran; caching the tree here makes that safe even after
    /// `rune clean`, on a fresh checkout, or when the CAS is shared across
    /// machines.
    pub fn put_dir_tree(&self, cache_key: &str, source_dir: &Path) -> Result<()> {
        let object_dir = self.object_dir(cache_key);
        // Immutable, like artifact entries: a complete tree already stored
        // under this key stands.
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
        // Build the new tree beside the old one and swap by rename, so a
        // reader restoring concurrently sees either the old tree or the new
        // one, never a directory that is half deleted.
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

    /// Restores a directory tree cached by [`Self::put_dir_tree`] into
    /// `dest_dir`. Returns `false` (rather than erroring) when there's
    /// simply no cached entry, so callers can fall through to regenerating
    /// it; returns an error only for an I/O failure while restoring one that
    /// does exist.
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
                // Corrupted or partially-GC'd entry: don't restore a
                // partial tree and claim it as a hit.
                return Ok(false);
            }
            let dest = dest_dir.join(&rel);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            if already_linked(&src, &dest) {
                continue;
            }
            rune_common::fsutil::replace_with_link(&src, &dest)?;
        }
        Ok(true)
    }

    /// Hardlink (or copy) artifacts from CAS into target build directory.
    ///
    /// On a repeat build where the destination is already hardlinked to the
    /// same CAS object (the common "nothing changed" case), this does no
    /// filesystem work at all beyond a single `stat` per artifact: relinking
    /// and re-hashing multi-megabyte `.rlib`s on every invocation, as the
    /// naive version of this function did, is what made even a fully cached
    /// build noticeably slower than Cargo's near-instant no-op rebuild.
    pub fn link_to(&self, cache_key: &str, target_dir: &Path) -> Result<Vec<PathBuf>> {
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

        for artifact in entry.manifest.artifacts {
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

            // Replace atomically: another process may be reading `dest` right
            // now. Try hardlink, fallback to copy. A hardlink shares the
            // source's inode, so its content is the source's content by
            // construction and does not need re-verifying; only the copy
            // fallback (e.g. across filesystems) can actually diverge and is
            // worth the cost of hashing.
            rune_common::fsutil::replace_with_link(&src, &dest)?;
            let hard_linked = already_linked(&src, &dest);
            if hard_linked {
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
        Ok(linked_files)
    }

    /// Read stats from store.
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

    /// Save stats to store.
    ///
    /// Deliberately skips `fsync`: these are diagnostic hit/miss counters,
    /// not build-correctness state, and `record_hit`/`record_miss` are
    /// called once per compilation unit, so forcing a durability barrier on
    /// every single one of them would trade real per-build latency for
    /// durability nobody needs on a counter that can simply be rebuilt.
    pub(crate) fn save_stats(&self, stats: &CasStats) -> Result<()> {
        let json = serde_json::to_string_pretty(stats)?;
        let temp_path = self.root.join("stats.json.tmp");
        fs::write(&temp_path, json.as_bytes())?;
        fs::rename(temp_path, self.root.join("stats.json"))?;
        Ok(())
    }

    /// Reads a cached, arbitrary-value entry (currently used for build
    /// script output) previously stored with [`Self::put_value`].
    ///
    /// This is a separate, lightweight slot from the compiled-artifact entry
    /// at the same object directory: it stores exactly one small JSON blob
    /// rather than a manifest plus copied artifact files, since a build
    /// script's output (link flags, cfgs, env vars) is data, not a file to
    /// hardlink into a build directory.
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

    /// Stores an arbitrary, JSON-serializable value under `cache_key`. See
    /// [`Self::get_value`].
    pub fn put_value<T: Serialize>(&self, cache_key: &str, value: &T) -> Result<()> {
        let dir = self.object_dir(cache_key);
        fs::create_dir_all(&dir)?;
        let json = serde_json::to_string(value)?;
        // Written beside and renamed: a crash mid-write must never leave a
        // truncated value that a later build could mistake for a valid one.
        let tmp = dir.join(format!(".value.{}.tmp", std::process::id()));
        fs::write(&tmp, json)?;
        fs::rename(&tmp, dir.join("value.json"))?;
        self.publish(cache_key, crate::remote::Slot::Value);
        Ok(())
    }

    /// Record a cache hit (in memory; see [`Self::flush_stats`]).
    pub fn record_hit(&self) {
        self.pending.hits.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a cache miss (in memory; see [`Self::flush_stats`]).
    pub fn record_miss(&self) {
        self.pending.misses.fetch_add(1, Ordering::Relaxed);
    }

    /// Merges the counters accumulated since the last flush into
    /// `stats.json` in one write. Call once at the end of a build.
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

/// Marks an entry as recently used by bumping its manifest's mtime — but only
/// if that is more than a few hours stale, so a fully cached build doesn't
/// turn into hundreds of metadata writes. Garbage collection evicts by this
/// timestamp, which is what makes it least-recently-*used* rather than
/// least-recently-*stored*.
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

/// Blake3 of a file's contents, hex encoded.
/// A number no other call in this process gets, for temporary file names.
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

        // A build directory hardlinks the object...
        let build = tempdir().unwrap();
        let linked = store.link_to("keyv", build.path()).unwrap();

        // ...then the same key is published again with different content (a
        // forced or racing rebuild). The first result stands.
        fs::write(&file, "v2").unwrap();
        store.put("keyv", "foo", "1.0.0", &[file]).unwrap();

        assert_eq!(fs::read_to_string(&linked[0]).unwrap(), "v1");
        let other_build = tempdir().unwrap();
        let relinked = store.link_to("keyv", other_build.path()).unwrap();
        assert_eq!(fs::read_to_string(&relinked[0]).unwrap(), "v1");

        // And a stray in-place write through a link fails instead of
        // corrupting the shared object. (A superuser may write read-only
        // files anyway; then only the permission bits can be checked.)
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
        // It restores like any entry, and a restore into the same directory
        // finds it already in place.
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

        // Damage: the artifact disappears (a half-collected entry).
        let obj = store.object_dir("k").join("libbar.rlib");
        fs::remove_file(&obj).unwrap();
        assert!(store.link_to("k", tempdir().unwrap().path()).is_err());

        // Publishing again repairs it instead of trusting the broken entry.
        store.put("k", "bar", "1.0.0", &[file]).unwrap();
        let target = tempdir().unwrap();
        let ok = store.link_to("k", target.path()).unwrap();
        assert_eq!(fs::read_to_string(&ok[0]).unwrap(), "intact");
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

        // Bit rot: same size, different bytes.
        let obj = store.object_dir("kv").join("libbaz.rlib");
        #[allow(clippy::permissions_set_readonly_false)]
        {
            let mut perms = fs::metadata(&obj).unwrap().permissions();
            perms.set_readonly(false);
            fs::set_permissions(&obj, perms).unwrap();
        }
        fs::write(&obj, "bbbb").unwrap();
        assert!(store.link_to("kv", tempdir().unwrap().path()).is_err());
        // Without verification only the size is checked.
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
        store.flush_stats(); // idempotent: counters were drained
        assert_eq!(store.stats().unwrap().hits, 1);
    }
}
