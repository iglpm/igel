//! Moving store entries between machines as one file: `rune cas export`
//! writes the entries a build needs into a tar archive (gzip-compressed when
//! the name ends in `.gz`), `rune cas import` adds an archive's entries to the
//! local store — what a CI cache step saves and restores.
//!
//! An archive holds object directories exactly as the store lays them out
//! (`objects/<xx>/<key>/…`). Importing treats it as untrusted: only paths of
//! that shape are accepted, every artifact is checked against the hash its
//! entry records, and an entry the store already has is left as it is (the
//! first result stored for a key stands, as everywhere in the store).

use crate::store::{CasStore, hash_file};
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

/// What an export or import did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BundleStats {
    /// Object directories written or added.
    pub entries: usize,
    /// Object directories skipped (absent when exporting; present already, or
    /// damaged, when importing).
    pub skipped: usize,
    pub bytes: u64,
}

impl CasStore {
    /// Writes the object directories of `keys` that this store has into a
    /// tar archive (gzip-compressed when `path` ends in `.gz`).
    pub fn export_bundle(&self, keys: &[String], path: &Path) -> std::io::Result<BundleStats> {
        let file = fs::File::create(path)?;
        let gz = path.extension().is_some_and(|e| e == "gz" || e == "tgz");
        let writer: Box<dyn Write> = if gz {
            Box::new(flate2::write::GzEncoder::new(
                file,
                flate2::Compression::fast(),
            ))
        } else {
            Box::new(std::io::BufWriter::new(file))
        };
        let mut tar = tar::Builder::new(writer);
        tar.follow_symlinks(false);
        let mut stats = BundleStats::default();
        let mut seen = std::collections::HashSet::new();
        for key in keys {
            if !seen.insert(key.as_str()) {
                continue;
            }
            let dir = self.object_dir(key);
            if !dir.is_dir() {
                stats.skipped += 1;
                continue;
            }
            let rel = dir.strip_prefix(self.root()).unwrap_or(&dir).to_path_buf();
            for entry in walkdir::WalkDir::new(&dir).sort_by_file_name() {
                let entry = entry.map_err(std::io::Error::other)?;
                let name = entry.file_name().to_string_lossy();
                // Half-written files of a concurrent store.
                if name.starts_with('.') && name.ends_with(".tmp") {
                    continue;
                }
                if entry.file_type().is_file() {
                    let inner = rel.join(entry.path().strip_prefix(&dir).unwrap());
                    stats.bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
                    tar.append_path_with_name(entry.path(), &inner)?;
                }
            }
            stats.entries += 1;
        }
        let writer = tar.into_inner()?;
        drop(writer);
        Ok(stats)
    }

    /// Adds the entries of an archive written by [`Self::export_bundle`].
    pub fn import_bundle(&self, path: &Path) -> std::io::Result<BundleStats> {
        let file = fs::File::open(path)?;
        let mut magic = [0u8; 2];
        let is_gz = fs::File::open(path)?.read_exact(&mut magic).is_ok() && magic == [0x1f, 0x8b];
        let reader: Box<dyn Read> = if is_gz {
            Box::new(flate2::read::GzDecoder::new(file))
        } else {
            Box::new(std::io::BufReader::new(file))
        };
        let staging = self.root().join(format!(".import-{}", std::process::id()));
        let _ = fs::remove_dir_all(&staging);
        fs::create_dir_all(&staging)?;
        let result = self.import_into(reader, &staging);
        let _ = fs::remove_dir_all(&staging);
        result
    }

    fn import_into(&self, reader: Box<dyn Read>, staging: &Path) -> std::io::Result<BundleStats> {
        let mut archive = tar::Archive::new(reader);
        let mut dirs: Vec<PathBuf> = Vec::new();
        for entry in archive.entries()? {
            let mut entry = entry?;
            // Linker output often has holes, which archives keep as GNU
            // sparse entries; reading them gives the whole file.
            let kind = entry.header().entry_type();
            if !(kind.is_file() || kind.is_gnu_sparse()) {
                continue;
            }
            let rel = entry.path()?.into_owned();
            // `objects/<xx>/<key>/<file…>`, nothing else, nowhere else.
            let parts: Vec<&str> = rel
                .components()
                .map(|c| match c {
                    Component::Normal(s) => s.to_str(),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()
                .unwrap_or_default();
            if parts.len() < 4 || parts[0] != "objects" || !parts[2].starts_with(parts[1]) {
                return Err(std::io::Error::other(format!(
                    "not a store bundle: unexpected path `{}`",
                    rel.display()
                )));
            }
            let object = PathBuf::from(parts[0]).join(parts[1]).join(parts[2]);
            if !dirs.contains(&object) {
                dirs.push(object);
            }
            let dest = staging.join(&rel);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            let executable = entry.header().mode().is_ok_and(|m| m & 0o111 != 0);
            let mut out = fs::File::create(&dest)?;
            std::io::copy(&mut entry, &mut out)?;
            drop(out);
            // Stored objects are read-only; a binary or proc-macro keeps its
            // execute bit, or a restored program could not run.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = if executable { 0o555 } else { 0o444 };
                fs::set_permissions(&dest, fs::Permissions::from_mode(mode))?;
            }
            #[cfg(not(unix))]
            {
                let _ = executable;
                super::store::make_read_only_pub(&dest);
            }
        }

        let mut stats = BundleStats::default();
        for object in dirs {
            let staged = staging.join(&object);
            let target = self.root().join(&object);
            if target.exists() || !Self::staged_entry_is_sound(&staged) {
                stats.skipped += 1;
                continue;
            }
            let bytes: u64 = walkdir::WalkDir::new(&staged)
                .into_iter()
                .flatten()
                .filter(|e| e.file_type().is_file())
                .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
                .sum();
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            // A concurrent import or build may have stored it meanwhile: the
            // rename fails and theirs stands.
            if fs::rename(&staged, &target).is_ok() {
                stats.entries += 1;
                stats.bytes += bytes;
            } else {
                stats.skipped += 1;
            }
        }
        Ok(stats)
    }

    /// Whatever slots an object directory has must be whole: artifacts of the
    /// size and hash the manifest records, every file a tree lists, values
    /// that parse.
    fn staged_entry_is_sound(dir: &Path) -> bool {
        if let Ok(text) = fs::read_to_string(dir.join("manifest.json")) {
            let Ok(manifest) = serde_json::from_str::<crate::store::ArtifactManifest>(&text) else {
                return false;
            };
            let key_matches = dir
                .file_name()
                .is_some_and(|n| n.to_string_lossy() == manifest.cache_key);
            if !key_matches {
                return false;
            }
            for a in &manifest.artifacts {
                let p = dir.join(&a.filename);
                let size = fs::metadata(&p).map(|m| m.len()).ok();
                let hash = hash_file(&p).ok();
                if size != Some(a.size) || hash.as_deref() != Some(a.hash.as_str()) {
                    tracing::debug!(
                        "bundle entry {} rejected: {} is {size:?} bytes, hash {hash:?}; recorded {} bytes, {}",
                        manifest.cache_key,
                        a.filename,
                        a.size,
                        a.hash
                    );
                    return false;
                }
            }
        }
        if let Ok(text) = fs::read_to_string(dir.join("tree.json")) {
            let Ok(rels) = serde_json::from_str::<Vec<String>>(&text) else {
                return false;
            };
            if !rels.iter().all(|r| dir.join("tree").join(r).is_file()) {
                return false;
            }
        }
        if let Ok(text) = fs::read_to_string(dir.join("value.json"))
            && serde_json::from_str::<serde_json::Value>(&text).is_err()
        {
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn store_with_entry() -> (tempfile::TempDir, CasStore) {
        let dir = tempdir().unwrap();
        let store = CasStore::new(dir.path().to_path_buf()).unwrap();
        let src = tempdir().unwrap();
        let file = src.path().join("libfoo-abc.rlib");
        fs::write(&file, "compiled bytes").unwrap();
        store.put("k1", "foo", "1.0.0", &[file]).unwrap();
        store.put_value("k1-warnings", &"a warning").unwrap();
        let out = src.path().join("out");
        fs::create_dir_all(&out).unwrap();
        fs::write(out.join("gen.rs"), "pub const X: u8 = 1;").unwrap();
        store.put_dir_tree("k1-buildscript", &out).unwrap();
        (dir, store)
    }

    #[test]
    fn entries_travel_through_a_bundle_intact() {
        for name in ["b.tar", "b.tar.gz"] {
            let (_a_dir, a) = store_with_entry();
            let bundle = tempdir().unwrap();
            let path = bundle.path().join(name);
            let keys = ["k1", "k1-warnings", "k1-buildscript", "missing"].map(String::from);
            let out = a.export_bundle(&keys, &path).unwrap();
            assert_eq!((out.entries, out.skipped), (3, 1), "{name}");

            let b_dir = tempdir().unwrap();
            let b = CasStore::new(b_dir.path().to_path_buf()).unwrap();
            let got = b.import_bundle(&path).unwrap();
            assert_eq!(got.entries, 3, "{name}");
            let target = tempdir().unwrap();
            let linked = b.link_to("k1", target.path()).unwrap();
            assert_eq!(fs::read_to_string(&linked[0]).unwrap(), "compiled bytes");
            assert_eq!(b.get_value::<String>("k1-warnings").unwrap(), "a warning");
            assert!(b.restore_dir_tree("k1-buildscript", target.path()).unwrap());
            // Importing again adds nothing.
            assert_eq!(b.import_bundle(&path).unwrap().skipped, 3);
        }
    }

    #[cfg(unix)]
    #[test]
    fn sparse_files_like_linker_output_survive_a_bundle() {
        let dir = tempdir().unwrap();
        let store = CasStore::new(dir.path().to_path_buf()).unwrap();
        let src = tempdir().unwrap();
        let bin = src.path().join("app-abc");
        {
            // A hole in the middle, as linkers leave them.
            use std::io::{Seek, SeekFrom};
            let mut f = fs::File::create(&bin).unwrap();
            f.write_all(b"head").unwrap();
            f.seek(SeekFrom::Start(1 << 20)).unwrap();
            f.write_all(b"tail").unwrap();
        }
        store
            .put("kb", "app", "1.0.0", std::slice::from_ref(&bin))
            .unwrap();
        let path = src.path().join("b.tar");
        store.export_bundle(&["kb".into()], &path).unwrap();
        let other = tempdir().unwrap();
        let b = CasStore::new(other.path().to_path_buf()).unwrap();
        assert_eq!(b.import_bundle(&path).unwrap().entries, 1);
        let out = tempdir().unwrap();
        let linked = b.link_to("kb", out.path()).unwrap();
        assert_eq!(fs::read(&linked[0]).unwrap(), fs::read(&bin).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn programs_stay_executable_and_data_stays_read_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let store = CasStore::new(dir.path().to_path_buf()).unwrap();
        let src = tempdir().unwrap();
        let bin = src.path().join("tool-abc");
        let lib = src.path().join("libx-abc.rlib");
        fs::write(&bin, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(&lib, "rlib").unwrap();
        store.put("kx", "x", "1.0.0", &[bin, lib]).unwrap();
        let path = src.path().join("b.tar.gz");
        store.export_bundle(&["kx".into()], &path).unwrap();
        let other = tempdir().unwrap();
        let b = CasStore::new(other.path().to_path_buf()).unwrap();
        b.import_bundle(&path).unwrap();
        let obj = b.object_dir("kx");
        let mode = |f: &str| fs::metadata(obj.join(f)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode("tool-abc"), 0o555);
        assert_eq!(mode("libx-abc.rlib"), 0o444);
    }

    #[test]
    fn a_tampered_artifact_is_not_imported() {
        let (_a_dir, a) = store_with_entry();
        let bundle = tempdir().unwrap();
        let path = bundle.path().join("b.tar");
        a.export_bundle(&["k1".into()], &path).unwrap();
        // Flip the artifact's bytes inside the archive (same length).
        let mut bytes = fs::read(&path).unwrap();
        let at = bytes
            .windows(14)
            .position(|w| w == b"compiled bytes")
            .unwrap();
        bytes[at] = b'C';
        fs::write(&path, bytes).unwrap();
        let b_dir = tempdir().unwrap();
        let b = CasStore::new(b_dir.path().to_path_buf()).unwrap();
        let got = b.import_bundle(&path).unwrap();
        assert_eq!((got.entries, got.skipped), (0, 1));
        assert!(b.get("k1").is_none());
    }

    #[test]
    fn paths_outside_the_object_layout_are_refused() {
        let bundle = tempdir().unwrap();
        let path = bundle.path().join("evil.tar");
        let mut tar = tar::Builder::new(fs::File::create(&path).unwrap());
        let mut header = tar::Header::new_gnu();
        header.set_size(1);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "stats.json", &b"x"[..])
            .unwrap();
        tar.finish().unwrap();
        let dir = tempdir().unwrap();
        let store = CasStore::new(dir.path().to_path_buf()).unwrap();
        assert!(store.import_bundle(&path).is_err());
    }
}
