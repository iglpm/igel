use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use thiserror::Error;
use tracing::debug;

const BINARY_LOCK_MAGIC: &[u8; 9] = b"RUNELOCK\x01";

#[derive(Debug, Error)]
pub enum LockfileError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Bincode error: {0}")]
    Bincode(#[from] bincode::Error),
    #[error("TOML error: {0}")]
    TomlDe(#[from] toml::de::Error),
    #[error("TOML ser error: {0}")]
    TomlSer(#[from] toml::ser::Error),
    #[error("Invalid binary lockfile format")]
    InvalidMagic,
}

pub type Result<T> = std::result::Result<T, LockfileError>;

/// Represents a package pinned in the lockfile.
///
/// Deliberately carries no `skip_serializing_if` on any field, even though
/// this struct is also exported as TOML/JSON where that would be cosmetically
/// nice (an explicit `dependencies = []` instead of omitting the key). This
/// struct's *primary* serialization is `bincode` (`Rune.lock.bin`), a
/// positional format with no field names or "this key is absent" marker: a
/// derived bincode deserializer just reads the next field in declared order,
/// with no way to know a writer skipped one. `skip_serializing_if` is
/// serialize-only — it has no matching "field was skipped" signal a
/// deserializer can act on — so a package with, say, an empty `dependencies`
/// (skipped) followed by a non-empty `features` desyncs the byte stream by
/// exactly one missing length-prefixed vec, and every field of every
/// subsequent package in the file is then read from the wrong offset. This
/// happened for real: any local (path-sourced) package has `source: None`,
/// and once one was written, every alphabetically-later package's fields
/// (dependencies, features, ...) came back corrupted or empty on the very
/// next read of the exact same, unchanged file — deterministic given the
/// bytes, but invisible unless you compare two reads of one file byte-for-
/// byte, since it looks like "sometimes wrong" from the outside.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LockPackage {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub checksum: Option<String>,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub features: Vec<String>,
}

/// The in-memory representation of a resolved lockfile.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Lockfile {
    #[serde(default)]
    pub version: u32,
    #[serde(rename = "package", default)]
    pub packages: Vec<LockPackage>,
}

impl Lockfile {
    pub fn new() -> Self {
        Self {
            version: 3,
            packages: Vec::new(),
        }
    }

    /// Finds a package by name in the lockfile.
    pub fn find_package(&self, name: &str) -> Option<&LockPackage> {
        self.packages.iter().find(|p| p.name == name)
    }

    /// Adds or replaces a package in the lockfile, keeping packages ordered
    /// by `(name, version)`.
    ///
    /// Uses a binary search to find the slot, instead of a linear scan plus a
    /// full re-sort on every insert (which made building a lockfile of N
    /// packages O(N² log N)). The order is also *total*: sorting by name alone
    /// left same-named packages in whatever order they were inserted, which —
    /// coming from a `HashMap` walk — differed run to run and made otherwise
    /// identical lockfiles compare unequal, forcing needless rewrites.
    pub fn add_or_replace(&mut self, package: LockPackage) {
        let key = |p: &LockPackage| (p.name.clone(), p.version.clone());
        match self
            .packages
            .binary_search_by(|p| key(p).cmp(&key(&package)))
        {
            Ok(pos) => self.packages[pos] = package,
            Err(pos) => self.packages.insert(pos, package),
        }
    }
}

/// Manager for reading, writing, and synchronizing binary and Cargo.lock files.
pub struct LockfileManager;

impl LockfileManager {
    /// Loads a lockfile, preferring `Rune.lock.bin` for maximum speed, falling back to `Cargo.lock`.
    pub fn load_fast(project_root: &Path) -> Option<Lockfile> {
        let bin_path = project_root.join("Rune.lock.bin");
        if bin_path.exists() {
            if let Ok(lock) = Self::read_binary(&bin_path) {
                return Some(lock);
            }
        }

        let cargo_path = project_root.join("Cargo.lock");
        if cargo_path.exists() {
            if let Ok(lock) = Self::read_cargo(&cargo_path) {
                return Some(lock);
            }
        }

        None
    }

    /// Loads only `Rune.lock.bin` (never falling back to `Cargo.lock`).
    ///
    /// The binary lockfile is the only one that carries Rune's per-package
    /// resolved feature sets, so it is the only one a build may trust as a
    /// complete substitute for re-resolving.
    pub fn load_binary(project_root: &Path) -> Option<Lockfile> {
        Self::read_binary(&project_root.join("Rune.lock.bin")).ok()
    }

    /// Reads a binary lockfile (`Rune.lock.bin`).
    pub fn read_binary(path: &Path) -> Result<Lockfile> {
        let mut file = File::open(path)?;
        let mut magic = [0u8; 9];
        file.read_exact(&mut magic)?;
        if &magic != BINARY_LOCK_MAGIC {
            return Err(LockfileError::InvalidMagic);
        }

        let mut data = Vec::new();
        file.read_to_end(&mut data)?;
        let lockfile: Lockfile = bincode::deserialize(&data)?;
        Ok(lockfile)
    }

    /// Writes a binary lockfile (`Rune.lock.bin`).
    pub fn write_binary(lock: &Lockfile, path: &Path) -> Result<()> {
        let serialized = bincode::serialize(lock)?;
        let mut file = File::create(path)?;
        file.write_all(BINARY_LOCK_MAGIC)?;
        file.write_all(&serialized)?;
        file.sync_data()?;
        Ok(())
    }

    /// Reads standard `Cargo.lock` (TOML format).
    pub fn read_cargo(path: &Path) -> Result<Lockfile> {
        let content = fs::read_to_string(path)?;
        let lockfile: Lockfile = toml::from_str(&content)?;
        Ok(lockfile)
    }

    /// Writes standard `Cargo.lock` (TOML format) with Cargo-compatible formatting.
    pub fn write_cargo(lock: &Lockfile, path: &Path) -> Result<()> {
        let toml_str = toml::to_string_pretty(lock)?;
        let mut file = File::create(path)?;
        file.write_all(b"# This file is automatically @generated by Rune.\n# It is not intended for manual editing.\n")?;
        file.write_all(toml_str.as_bytes())?;
        file.sync_data()?;
        Ok(())
    }

    /// Writes the resolved lockfile to `Rune.lock.bin`.
    ///
    /// Deliberately does **not** also overwrite `Cargo.lock`, despite the
    /// name this replaced (`sync_all`) suggesting otherwise. Rune's resolver
    /// doesn't yet reproduce Cargo's resolution with full fidelity (see the
    /// feature-activation gaps tracked separately), so silently regenerating
    /// a project's real, git-tracked `Cargo.lock` from Rune's own result on
    /// every `build`/`add`/`remove`/`update` risked handing a plain `cargo
    /// build` a lockfile subtly different from what Cargo's own resolver
    /// would have produced — for an existing Cargo project that never asked
    /// to adopt Rune, this is real, unannounced data loss on a file other
    /// tooling and CI depend on. `Cargo.lock` is only ever written by an
    /// explicit, user-initiated `rune lockfile export --format cargo`.
    pub fn sync_all(project_root: &Path, lock: &Lockfile) -> Result<()> {
        let bin_path = project_root.join("Rune.lock.bin");
        Self::write_binary(lock, &bin_path)?;
        debug!("Wrote Rune.lock.bin at {}", project_root.display());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_binary_lockfile_roundtrip() {
        let dir = tempdir().unwrap();
        let bin_path = dir.path().join("Rune.lock.bin");

        let mut lock = Lockfile::new();
        lock.add_or_replace(LockPackage {
            name: "serde".to_string(),
            version: "1.0.210".to_string(),
            source: Some("registry+https://github.com/rust-lang/crates.io-index".to_string()),
            checksum: Some("abc123def456".to_string()),
            dependencies: vec!["serde_derive 1.0.210".to_string()],
            features: vec!["default".to_string(), "std".to_string()],
        });

        LockfileManager::write_binary(&lock, &bin_path).unwrap();
        let loaded = LockfileManager::read_binary(&bin_path).unwrap();

        assert_eq!(lock, loaded);
    }

    #[test]
    fn packages_stay_sorted_and_insertion_order_does_not_matter() {
        let pkg = |name: &str, version: &str| LockPackage {
            name: name.into(),
            version: version.into(),
            source: None,
            checksum: None,
            dependencies: vec![],
            features: vec![],
        };
        let mut a = Lockfile::new();
        for (n, v) in [
            ("b", "1.0.0"),
            ("a", "2.0.0"),
            ("a", "1.0.0"),
            ("c", "0.1.0"),
        ] {
            a.add_or_replace(pkg(n, v));
        }
        let mut b = Lockfile::new();
        for (n, v) in [
            ("c", "0.1.0"),
            ("a", "1.0.0"),
            ("b", "1.0.0"),
            ("a", "2.0.0"),
        ] {
            b.add_or_replace(pkg(n, v));
        }
        assert_eq!(a, b, "same set of packages must give the same lockfile");
        let names: Vec<_> = a
            .packages
            .iter()
            .map(|p| format!("{} {}", p.name, p.version))
            .collect();
        assert_eq!(names, ["a 1.0.0", "a 2.0.0", "b 1.0.0", "c 0.1.0"]);

        // Replacing keeps a single entry.
        let mut replaced = pkg("a", "1.0.0");
        replaced.features = vec!["x".into()];
        a.add_or_replace(replaced);
        assert_eq!(a.packages.len(), 4);
        assert_eq!(a.packages[0].features, vec!["x"]);
    }

    #[test]
    fn test_cargo_lockfile_roundtrip() {
        let dir = tempdir().unwrap();
        let cargo_path = dir.path().join("Cargo.lock");

        let mut lock = Lockfile::new();
        lock.add_or_replace(LockPackage {
            name: "serde".to_string(),
            version: "1.0.210".to_string(),
            source: Some("registry+https://github.com/rust-lang/crates.io-index".to_string()),
            checksum: Some("abc123def456".to_string()),
            dependencies: vec!["serde_derive 1.0.210".to_string()],
            features: vec!["default".to_string(), "std".to_string()],
        });

        LockfileManager::write_cargo(&lock, &cargo_path).unwrap();
        let loaded = LockfileManager::read_cargo(&cargo_path).unwrap();

        assert_eq!(lock.packages.len(), loaded.packages.len());
        assert_eq!(lock.packages[0].name, loaded.packages[0].name);
    }
}
