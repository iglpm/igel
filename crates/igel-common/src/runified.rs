//! rustc-runified (`rustc-runified/` in this repository): rustc, linked against the toolchain's
//! own `rustc_driver`, plus `--runified=` options that a stable rustc only has as `-Z` flags — a
//! parallel front end whose output doesn't depend on the thread count, and compiling an
//! executable before the rlibs it links exist. Igel runs it in place of the toolchain's `rustc`
//! when one is installed for that toolchain (`build.compiler`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::toolchain::program_fingerprint;

/// What `build.compiler` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// rustc-runified when it is installed for the toolchain in use, rustc otherwise.
    Auto,
    /// rustc-runified, or an error saying how to install it.
    Required,
    Rustc,
    /// hyprc, the fast Rust compiler built into Igel.
    Hyprc,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Driver {
    pub path: PathBuf,
    /// `rustc-runified 2027.0.0`
    pub version: String,
    /// `runified` (a patched toolchain) or `upstream`.
    pub toolchain: String,
    pub options: Vec<String>,
    /// Of the executable: part of every cache key it compiles.
    pub digest: String,
}

impl Driver {
    pub fn supports(&self, option: &str) -> bool {
        self.options.iter().any(|o| o == option)
    }

    /// Asks a compiler whether it is rustc-runified (`--runified=info`, which rustc rejects),
    /// with `sysroot`'s compiler libraries. Remembered by the program's path, size and
    /// modification time, answers "no" included: a build that has nothing to do must not start a
    /// process to find out.
    pub fn probe(exe: &Path, sysroot: Option<&Path>) -> Option<Driver> {
        let fingerprint = program_fingerprint(exe);
        if fingerprint.ends_with("|missing") {
            return None;
        }
        let path = crate::settings::igel_home()
            .join("cache")
            .join("runified.json");
        let load = || -> HashMap<String, Option<Driver>> {
            std::fs::read_to_string(&path)
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or_default()
        };
        if let Some(known) = load().get(&fingerprint) {
            return known.clone();
        }
        let mut cmd = Command::new(exe);
        if let Some(sysroot) = sysroot {
            find_libraries_in(&mut cmd, sysroot);
        }
        let driver = cmd
            .arg("--runified=info")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| parse_info(exe, &String::from_utf8_lossy(&o.stdout)))
            .map(|mut d| {
                d.digest = std::fs::read(exe)
                    .map(|bytes| blake3::hash(&bytes).to_hex()[..32].to_string())
                    .unwrap_or_default();
                d
            });
        let mut all = load();
        all.insert(fingerprint, driver.clone());
        while all.len() > 64 {
            let Some(k) = all.keys().next().cloned() else {
                break;
            };
            all.remove(&k);
        }
        if let (Some(dir), Ok(text)) = (path.parent(), serde_json::to_string(&all)) {
            let _ = std::fs::create_dir_all(dir);
            let _ = crate::fsutil::write_atomic(&path, text.as_bytes());
        }
        driver
    }
}

/// The driver links against `librustc_driver-<hash>`, which the toolchain in use has under the
/// same name (that is what found the driver): load it from there, not from wherever the driver
/// was built.
pub fn find_libraries_in(cmd: &mut Command, sysroot: &Path) {
    let (var, dir) = if cfg!(windows) {
        ("PATH", sysroot.join("bin"))
    } else if cfg!(target_os = "macos") {
        ("DYLD_FALLBACK_LIBRARY_PATH", sysroot.join("lib"))
    } else {
        ("LD_LIBRARY_PATH", sysroot.join("lib"))
    };
    let mut paths = vec![dir];
    if let Some(old) = std::env::var_os(var) {
        paths.extend(std::env::split_paths(&old));
    }
    if let Ok(joined) = std::env::join_paths(paths) {
        cmd.env(var, joined);
    }
}

/// `driver: …`, `rustc: …`, `toolchain: …`, `options: a b c`, one per line.
fn parse_info(exe: &Path, text: &str) -> Option<Driver> {
    let field = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key))
            .map(|v| v.trim().to_string())
    };
    Some(Driver {
        path: exe.to_path_buf(),
        version: field("driver:")?,
        toolchain: field("toolchain:").unwrap_or_else(|| "upstream".into()),
        options: field("options:")
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_string)
            .collect(),
        digest: String::new(),
    })
}

/// Which build of the compiler a sysroot holds: `<release>-<hash>-<host>`, the hash being the
/// one in `librustc_driver-<hash>`'s file name. A driver links against exactly one of them,
/// and `rustc -vV` can't tell a patched 1.98.1 from upstream's.
pub fn toolchain_key(sysroot: &Path, release: &str, host: &str) -> Option<String> {
    let hash = ["lib", "bin"].iter().find_map(|dir| {
        std::fs::read_dir(sysroot.join(dir))
            .ok()?
            .flatten()
            .find_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let rest = name
                    .strip_prefix("librustc_driver-")
                    .or_else(|| name.strip_prefix("rustc_driver-"))?;
                Some(rest.split('.').next()?.to_string())
            })
    })?;
    Some(format!("{release}-{hash}-{host}"))
}

/// The second half of an `.rmeta` written early (`--runified=early-metadata`): dependents that
/// need it wait for it.
pub fn metadata_rest(rmeta: &Path) -> PathBuf {
    rmeta.with_extension("rmeta-generics")
}

/// The crate's compilation failed after its early `.rmeta`: an empty rest tells dependents still
/// waiting for it to stop.
pub fn metadata_never_completes(rmeta: &Path) {
    let rest = metadata_rest(rmeta);
    if !rest.exists() {
        let _ = std::fs::write(rest, b"");
    }
}

/// Where the driver for a toolchain is installed (`cargo runified driver --igl` puts it there).
pub fn installed_path(key: &str) -> PathBuf {
    crate::settings::igel_home()
        .join("runified")
        .join(key)
        .join(format!("rustc-runified{}", std::env::consts::EXE_SUFFIX))
}

/// The same toolchain's driver, shipped next to its `rustc` (runified toolchains are).
pub fn shipped_path(sysroot: &Path) -> PathBuf {
    sysroot
        .join("bin")
        .join(format!("rustc-runified{}", std::env::consts::EXE_SUFFIX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_is_parsed() {
        let d = parse_info(
            Path::new("/x/rustc-runified"),
            "driver: rustc-runified 2027.0.0\nrustc: 1.98.1 (48a229cea 2026-09-01)\n\
             toolchain: upstream\noptions: threads no-link link-only\n",
        )
        .unwrap();
        assert_eq!(d.version, "rustc-runified 2027.0.0");
        assert!(d.supports("no-link") && d.supports("threads") && !d.supports("fast"));
        assert_eq!(d.toolchain, "upstream");
        assert_eq!(parse_info(Path::new("rustc"), "rustc 1.98.1\n"), None);
    }

    #[test]
    fn the_toolchain_key_names_the_exact_compiler_build() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("lib")).unwrap();
        std::fs::write(
            dir.path().join("lib/librustc_driver-eaf3ed1d23ca5027.so"),
            "",
        )
        .unwrap();
        assert_eq!(
            toolchain_key(dir.path(), "1.98.1", "x86_64-unknown-linux-gnu").as_deref(),
            Some("1.98.1-eaf3ed1d23ca5027-x86_64-unknown-linux-gnu")
        );
        assert_eq!(
            toolchain_key(Path::new("/nonexistent"), "1.98.1", "x"),
            None
        );
    }
}
