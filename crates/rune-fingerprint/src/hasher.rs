use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;
use walkdir::WalkDir;

use crate::cache::{FileEntry, FingerprintCache};
use rune_common::hash::{HashBuilder, xxhash3_file};

#[derive(Error, Debug)]
pub enum HasherError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

type Result<T> = std::result::Result<T, HasherError>;

/// All inputs that go into a crate's fingerprint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FingerprintInputs {
    pub source_hash: String,
    pub dependency_fingerprints: Vec<String>,
    pub compiler_version: String,
    pub features: Vec<String>,
    pub profile_hash: String,
    pub env_vars: Vec<(String, String)>,
    pub build_script_output_hash: Option<String>,
    pub edition: String,
    pub target: Option<String>,
}

/// The result of fingerprinting a crate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrateFingerprint {
    pub inputs: FingerprintInputs,
    pub hash: String,
    pub timestamp: u64,
}

static COMPILER_VERSION: OnceLock<String> = OnceLock::new();

/// Retrieves and caches the compiler version output from `rustc -vV`.
pub fn get_compiler_version() -> String {
    COMPILER_VERSION
        .get_or_init(|| {
            // The compiler actually used: `RUSTC` may name a wrapper such as
            // `clippy-driver`, whose output must never be confused with plain
            // `rustc`'s in the cache.
            let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
            Command::new(&rustc)
                .arg("-vV")
                .output()
                .map(|out| {
                    format!(
                        "{}|{}",
                        rustc.to_string_lossy(),
                        String::from_utf8_lossy(&out.stdout).trim()
                    )
                })
                .unwrap_or_else(|_| "unknown".to_string())
        })
        .clone()
}

/// Directories that are known *not* to be inputs of a package: build output,
/// version-control metadata, editor state, and dependency caches of other
/// ecosystems. `.cargo/` is here because its build-relevant content
/// (`rustflags`, `[env]`, linker, wrappers, ...) is already part of every
/// unit's key as *values*, so hashing the file too would only invalidate
/// builds when a comment changes.
///
/// Notably absent: hidden directories in general. A crate can legitimately
/// `include_str!(".version")` or read `.config/settings.toml` from its build
/// script, so an arbitrary dot-file or dot-directory is an input until proven
/// otherwise.
const IGNORED_DIRS: &[&str] = &[
    "target",
    ".git",
    ".hg",
    ".svn",
    ".rune",
    ".cargo",
    ".idea",
    ".vscode",
    "node_modules",
];

/// Files that are editor and OS droppings rather than inputs: swap files,
/// backup files and Finder metadata. They churn constantly and no build reads
/// them.
fn is_editor_dropping(name: &str) -> bool {
    name.ends_with('~')
        || name == ".DS_Store"
        || name.starts_with(".#")
        || (name.starts_with('#') && name.ends_with('#'))
        || (name.starts_with('.')
            && (name.ends_with(".swp") || name.ends_with(".swo") || name.ends_with(".swx")))
}

/// Computes a hash over every input file of the package rooted at `package_dir`.
///
/// This is deliberately broader than "the `.rs` files under `src/`": a crate
/// can `include_str!`/`include_bytes!` a data file, read a schema at build
/// time, or keep its library root somewhere other than `src/`, and any of
/// those changing must invalidate the cache. So every regular file in the
/// package is hashed, except:
///
/// * known non-input directories (see [`IGNORED_DIRS`]) and editor droppings
///   (swap and backup files) — but *not* hidden files or directories in
///   general,
/// * nested packages (a subdirectory with its own `Cargo.toml` is a different
///   crate with its own cache key — workspace members live in such directories),
/// * lock files and `Rune.toml`, which don't affect compilation of this crate.
///
/// A file's hash is re-used from `cache` while its modification time and size
/// are unchanged, so a warm run only `stat`s the tree.
pub fn compute_package_hash(package_dir: &Path, cache: &mut FingerprintCache) -> Result<String> {
    if let Some(hash) = cache.package_hashes.get(package_dir) {
        return Ok(hash.clone());
    }
    let hash = compute_package_hash_uncached(package_dir, cache)?;
    cache
        .package_hashes
        .insert(package_dir.to_path_buf(), hash.clone());
    Ok(hash)
}

fn compute_package_hash_uncached(
    package_dir: &Path,
    cache: &mut FingerprintCache,
) -> Result<String> {
    let mut files = Vec::new();
    let walker = WalkDir::new(package_dir)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 || !e.file_type().is_dir() {
                return true;
            }
            let name = e.file_name().to_string_lossy();
            if IGNORED_DIRS.contains(&name.as_ref()) {
                return false;
            }
            // A nested package is not part of this one.
            !e.path().join("Cargo.toml").is_file()
        });
    for entry in walker.filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        if is_editor_dropping(&name) {
            continue;
        }
        // The manifest itself is not an input: every compilation-relevant
        // value in it (features, dependencies and their aliases, targets,
        // profiles, lints, metadata) is already part of the unit's key, so
        // hashing the file too would only rebuild a package when a comment or
        // an unrelated section changes. When something *does* read the file
        // (`include_str!("../Cargo.toml")`, a build script), it is recorded as
        // a declared input by whoever observed the read.
        if name == "Cargo.toml" && entry.depth() == 1 {
            continue;
        }
        // Lock files and Rune's own settings file don't affect compilation;
        // editing them (e.g. `rune config set ...`) must not invalidate the
        // cache of the package they sit in.
        if name == "Cargo.lock" || name == "Rune.lock.bin" || name == "Rune.toml" {
            continue;
        }
        files.push(entry.into_path());
    }
    hash_files(package_dir, files, cache)
}

/// Computes a hash of all `.rs` files under `source_root`.
///
/// Kept for callers that want just a source-tree hash; see
/// [`compute_package_hash`] for what cache keys use.
pub fn compute_source_hash(
    source_dir: &Path,
    source_root: &Path,
    cache: &mut FingerprintCache,
) -> Result<String> {
    let mut files = Vec::new();
    for entry in WalkDir::new(source_root)
        .sort_by_file_name()
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.file_type().is_file()
            && entry.path().extension().and_then(|s| s.to_str()) == Some("rs")
        {
            files.push(entry.into_path());
        }
    }
    hash_files(source_dir, files, cache)
}

fn hash_files(
    base: &Path,
    mut files: Vec<PathBuf>,
    cache: &mut FingerprintCache,
) -> Result<String> {
    files.sort();

    // Stat + (if changed) hash every file in parallel. `cache` is only read
    // here; results are folded back in sequentially below.
    let cache_ref: &FingerprintCache = cache;
    let hashed: Vec<FileEntry> = files
        .into_par_iter()
        .map(|path| {
            let (mtime, size) = std::fs::metadata(&path)
                .ok()
                .map(|m| {
                    let mtime = m
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_nanos() as u64)
                        .unwrap_or(0);
                    (mtime, m.len())
                })
                .unwrap_or((0, 0));

            let cached = cache_ref
                .files
                .get(&path)
                .filter(|c| mtime != 0 && c.mtime == mtime && c.size == size);
            let hash = match cached {
                Some(c) => c.hash,
                None => xxhash3_file(&path).unwrap_or(0),
            };
            FileEntry {
                path,
                mtime,
                size,
                hash,
            }
        })
        .collect();

    let mut hb = HashBuilder::new();
    for entry in hashed {
        let relative = entry.path.strip_prefix(base).unwrap_or(&entry.path);
        hb.update(relative.to_string_lossy().as_bytes());
        hb.update(&entry.hash.to_le_bytes());
        cache.mark_visited(&entry.path);
        cache.record(entry);
    }

    Ok(hb.finish_blake3())
}

/// Computes the final crate fingerprint from its inputs.
pub fn compute_fingerprint(inputs: FingerprintInputs) -> CrateFingerprint {
    let mut hb = HashBuilder::new();

    hb.update(inputs.source_hash.as_bytes());
    for dep in &inputs.dependency_fingerprints {
        hb.update(dep.as_bytes());
    }
    hb.update(inputs.compiler_version.as_bytes());
    for feat in &inputs.features {
        hb.update(feat.as_bytes());
    }
    hb.update(inputs.profile_hash.as_bytes());
    for (k, v) in &inputs.env_vars {
        hb.update(k.as_bytes());
        hb.update(b"=");
        hb.update(v.as_bytes());
    }
    if let Some(ref build_hash) = inputs.build_script_output_hash {
        hb.update(build_hash.as_bytes());
    }
    hb.update(inputs.edition.as_bytes());
    if let Some(ref target) = inputs.target {
        hb.update(target.as_bytes());
    }

    let hash = hb.finish_blake3();
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    CrateFingerprint {
        inputs,
        hash,
        timestamp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_fingerprint() {
        let inputs = FingerprintInputs {
            source_hash: "hash1".into(),
            dependency_fingerprints: vec![],
            compiler_version: "1.70.0".into(),
            features: vec![],
            profile_hash: "prof".into(),
            env_vars: vec![],
            build_script_output_hash: None,
            edition: "2021".into(),
            target: None,
        };

        let fp = compute_fingerprint(inputs.clone());
        assert_eq!(fp.inputs, inputs);
        assert!(!fp.hash.is_empty());
    }

    fn write(dir: &Path, rel: &str, content: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    #[test]
    fn package_hash_sees_non_src_inputs_but_ignores_output_and_nested_packages() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        write(d, "Cargo.toml", "[package]\nname=\"a\"\n");
        write(d, "src/lib.rs", "pub fn f() {}");
        write(d, "data/schema.json", "{}");
        write(d, "target/debug/junk", "1");
        write(d, ".git/HEAD", "ref: main");
        write(d, ".cargo/config.toml", "# not an input");
        write(d, ".version", "1");
        write(d, ".config/settings.toml", "a = 1");
        write(d, "member/Cargo.toml", "[package]\nname=\"b\"\n");
        write(d, "member/src/lib.rs", "pub fn g() {}");

        let mut cache = FingerprintCache::new();
        let base = compute_package_hash(d, &mut cache).unwrap();
        // The memo is per-run; a real second run starts from a fresh cache.
        let rehash = |cache: &mut FingerprintCache| {
            cache.package_hashes.clear();
            compute_package_hash(d, cache).unwrap()
        };

        // Ignored: build output, VCS and tool directories, editor droppings,
        // nested package, lockfile, and the manifest itself.
        write(
            d,
            "Cargo.toml",
            "[package]\nname=\"a\"\n# an unrelated edit\n",
        );
        write(d, "target/debug/junk", "2");
        write(d, ".git/HEAD", "ref: other");
        write(d, ".cargo/config.toml", "# still not an input, changed");
        write(d, ".src.rs.swp", "swap");
        write(d, "src/lib.rs~", "backup");
        write(d, "member/src/lib.rs", "pub fn g() { changed() }");
        write(d, "Cargo.lock", "# lock");
        assert_eq!(rehash(&mut cache), base);

        // Seen: a data file outside `src/` (think `include_str!("../data/schema.json")`).
        write(d, "data/schema.json", "{\"changed\":true}");
        let after_data = rehash(&mut cache);
        assert_ne!(after_data, base);

        // Seen: hidden files and directories are inputs like any other.
        write(d, ".version", "2");
        let after_dotfile = rehash(&mut cache);
        assert_ne!(after_dotfile, after_data);
        write(d, ".config/settings.toml", "a = 2");
        let after_dotdir = rehash(&mut cache);
        assert_ne!(after_dotdir, after_dotfile);
        let after_data = after_dotdir;

        write(d, "src/lib.rs", "pub fn f() { 1 }");
        assert_ne!(rehash(&mut cache), after_data);
    }

    #[test]
    fn same_size_edit_in_the_same_second_is_still_detected() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        write(d, "Cargo.toml", "");
        write(d, "src/lib.rs", "aaaa");
        let mut cache = FingerprintCache::new();
        let first = compute_package_hash(d, &mut cache).unwrap();
        // Same length, written immediately: only a sub-second mtime (or a
        // content hash) can tell these apart.
        write(d, "src/lib.rs", "bbbb");
        cache.package_hashes.clear();
        assert_ne!(compute_package_hash(d, &mut cache).unwrap(), first);
    }

    #[test]
    fn unchanged_cache_is_not_rewritten_and_stale_entries_are_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join("pkg");
        write(&d, "Cargo.toml", "");
        write(&d, "src/a.rs", "1");
        write(&d, "src/b.rs", "2");
        let cache_path = dir.path().join("cache.json");

        let mut cache = FingerprintCache::new();
        compute_package_hash(&d, &mut cache).unwrap();
        cache.save_if_changed(&cache_path).unwrap();
        let first_write = std::fs::metadata(&cache_path).unwrap().modified().unwrap();

        // Reload, nothing changed: no rewrite.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut cache = FingerprintCache::load(&cache_path).unwrap();
        compute_package_hash(&d, &mut cache).unwrap();
        cache.save_if_changed(&cache_path).unwrap();
        assert_eq!(
            std::fs::metadata(&cache_path).unwrap().modified().unwrap(),
            first_write
        );

        // A deleted file's entry is dropped on the next save.
        std::fs::remove_file(d.join("src/b.rs")).unwrap();
        let mut cache = FingerprintCache::load(&cache_path).unwrap();
        compute_package_hash(&d, &mut cache).unwrap();
        cache.save_if_changed(&cache_path).unwrap();
        let reloaded = FingerprintCache::load(&cache_path).unwrap();
        assert!(!reloaded.files.keys().any(|p| p.ends_with("b.rs")));
        assert!(reloaded.files.keys().any(|p| p.ends_with("a.rs")));
    }
}
