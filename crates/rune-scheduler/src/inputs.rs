//! External inputs of a build: what a unit read that is not in its source.
//!
//! A unit's cache key covers its source, its dependencies and its
//! configuration, but a build script or a macro can also read the *outside
//! world*: an environment variable, a header under `/usr/include`, a system
//! library found through `pkg-config`, the C compiler it drives. Reusing an
//! artifact after one of those changed would be reusing a wrong answer, and the
//! spec's rule is that correctness beats hit rate.
//!
//! Nobody can know a unit's outside reads before running it, so Rune *learns*
//! them from what the unit declares:
//!
//! * a build script's `rerun-if-env-changed` and `rerun-if-changed`, and the
//!   native libraries it asks the linker for;
//! * `env!` / `option_env!` reads, which `rustc` reports in its dep-info;
//! * the native tools (`cc`, `ar`) any build script may drive.
//!
//! After a build these are stored as [`DeclaredInputs`] next to the unit's
//! *base* key. The **effective** key is the base key plus the *current values*
//! of those inputs — so an artifact is only ever found again when everything it
//! depended on still has the value it had.

use rune_common::BuildScriptOutput;
use rune_common::hash::HashBuilder;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// What a unit was observed to depend on beyond its source and dependencies.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclaredInputs {
    /// Environment variables whose values matter (`None` value = unset).
    pub env: Vec<String>,
    /// Files or directories whose contents matter, as absolute paths.
    pub files: Vec<PathBuf>,
    /// Native library files the build linked against, from outside the package
    /// (fingerprinted by size and modification time).
    pub native: Vec<PathBuf>,
}

impl DeclaredInputs {
    pub fn is_empty(&self) -> bool {
        self.env.is_empty() && self.files.is_empty() && self.native.is_empty()
    }

    /// Merges another set in (sorted, without duplicates).
    pub fn merge(&mut self, other: DeclaredInputs) {
        self.env.extend(other.env);
        self.files.extend(other.files);
        self.native.extend(other.native);
        self.env.sort();
        self.env.dedup();
        self.files.sort();
        self.files.dedup();
        self.native.sort();
        self.native.dedup();
    }

    /// The current value of every declared input, as one hash.
    pub fn fingerprint(&self) -> String {
        let mut hb = HashBuilder::new();
        for name in &self.env {
            hb.update(b"\0env=");
            hb.update(name.as_bytes());
            hb.update(b"=");
            match std::env::var_os(name) {
                Some(v) => {
                    hb.update(b"1");
                    hb.update(v.to_string_lossy().as_bytes());
                }
                None => hb.update(b"unset"),
            }
        }
        for path in &self.files {
            hb.update(b"\0file=");
            hb.update(path.to_string_lossy().as_bytes());
            hb.update(hash_path(path).as_bytes());
        }
        for path in &self.native {
            hb.update(b"\0native=");
            hb.update(path.to_string_lossy().as_bytes());
            hb.update(stat_fingerprint(path).as_bytes());
        }
        hb.finish_blake3()
    }
}

/// The key a unit is stored and looked up under: its base key plus the
/// current values of what it declared, plus — for a unit with a build script —
/// the identity of the native toolchain the script may drive.
pub fn effective_key(
    base: &str,
    inputs: Option<&DeclaredInputs>,
    has_build_script: bool,
) -> String {
    // "No inputs recorded" and "an empty set of inputs" are the same thing and
    // must give the same key: the first build of a unit learns its inputs
    // *after* it was keyed with none, and it must find itself again.
    let declared = inputs.filter(|i| !i.is_empty());
    if declared.is_none() && !has_build_script {
        return base.to_string();
    }
    let mut hb = HashBuilder::new();
    hb.update(base.as_bytes());
    if let Some(inputs) = declared {
        hb.update(b"\0inputs=");
        hb.update(inputs.fingerprint().as_bytes());
    }
    if has_build_script {
        hb.update(b"\0native-tools=");
        hb.update(native_tools_identity().as_bytes());
    }
    hb.finish_blake3()
}

/// Content hash of a file, or of every file below a directory (sorted).
fn hash_path(path: &Path) -> String {
    match std::fs::metadata(path) {
        Ok(m) if m.is_file() => rune_common::hash::xxhash3_file(path)
            .map(|h| format!("{h:016x}"))
            .unwrap_or_else(|_| "unreadable".into()),
        Ok(m) if m.is_dir() => {
            let mut hb = HashBuilder::new();
            let mut files: Vec<PathBuf> = walkdir::WalkDir::new(path)
                .sort_by_file_name()
                .into_iter()
                .flatten()
                .filter(|e| e.file_type().is_file())
                .map(|e| e.into_path())
                .collect();
            files.sort();
            for f in files {
                hb.update(
                    f.strip_prefix(path)
                        .unwrap_or(&f)
                        .to_string_lossy()
                        .as_bytes(),
                );
                hb.update(hash_path(&f).as_bytes());
            }
            hb.finish_blake3()
        }
        _ => "missing".to_string(),
    }
}

fn stat_fingerprint(path: &Path) -> String {
    match std::fs::metadata(path) {
        Ok(m) => format!(
            "{}:{}",
            m.len(),
            m.modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0)
        ),
        Err(_) => "missing".to_string(),
    }
}

/// What a build script's output tells us about its outside reads.
pub fn from_script_output(
    out: &BuildScriptOutput,
    manifest_dir: &Path,
    out_dir: &Path,
) -> DeclaredInputs {
    let mut inputs = DeclaredInputs {
        env: out.rerun_if_env_changed.clone(),
        ..Default::default()
    };
    for p in &out.rerun_if_changed {
        let abs = if p.is_absolute() {
            p.clone()
        } else {
            manifest_dir.join(p)
        };
        // Inside the package the package-wide hash already covers it (or the
        // package is immutable); only outside files add information — and the
        // manifest, which that hash deliberately leaves out.
        let is_manifest = abs == rune_common::manifest_file::manifest_in(manifest_dir);
        if is_manifest || (!abs.starts_with(manifest_dir) && !abs.starts_with(out_dir)) {
            inputs.files.push(abs);
        }
    }
    // A script that declares no `rerun-if-changed` is rerun by Cargo whenever
    // *any* file of the package changes — the manifest included.
    if out.rerun_if_changed.is_empty() {
        inputs
            .files
            .push(rune_common::manifest_file::manifest_in(manifest_dir));
    }

    // Native libraries found outside the package: the linker will read them,
    // and a system update can change them.
    let outside = |p: &Path| !p.starts_with(manifest_dir) && !p.starts_with(out_dir);
    let mut dirs: Vec<PathBuf> = out
        .link_search
        .iter()
        .map(|ls| ls.path.clone())
        .filter(|p| outside(p))
        .collect();
    let defaults = [
        "/usr/local/lib",
        "/usr/lib",
        "/usr/lib64",
        "/lib",
        "/lib64",
        "/usr/lib/x86_64-linux-gnu",
        "/usr/lib/aarch64-linux-gnu",
    ];
    for lib in &out.link_libs {
        if lib.kind.as_deref() == Some("framework") {
            continue;
        }
        let name = lib.name.split(':').next_back().unwrap_or(&lib.name);
        let candidates = [
            format!("lib{name}.so"),
            format!("lib{name}.a"),
            format!("lib{name}.dylib"),
            format!("{name}.lib"),
        ];
        let mut found = false;
        for dir in dirs
            .iter()
            .cloned()
            .chain(defaults.iter().map(PathBuf::from))
        {
            for c in &candidates {
                let file = dir.join(c);
                if file.exists() {
                    inputs
                        .native
                        .push(std::fs::canonicalize(&file).unwrap_or(file));
                    found = true;
                }
            }
            if found {
                break;
            }
        }
    }
    dirs.clear();
    inputs.env.sort();
    inputs.env.dedup();
    inputs.files.sort();
    inputs.files.dedup();
    inputs.native.sort();
    inputs.native.dedup();
    inputs
}

/// Environment variables `rustc` reports reading (`env!`, `option_env!`) in a
/// dep-info file, excluding the ones Rune itself provides (their values are
/// already determined by the unit's key).
pub fn env_reads_from_dep_info(dep_info: &Path, provided: &dyn Fn(&str) -> bool) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(dep_info) else {
        return Vec::new();
    };
    let mut names: Vec<String> = text
        .lines()
        .filter_map(|l| l.strip_prefix("# env-dep:"))
        .map(|rest| rest.split('=').next().unwrap_or(rest).to_string())
        .filter(|n| !provided(n))
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Whether a dep-info file lists the package manifest as something the
/// compiler read (`include_str!("../Cargo.toml")`).
pub fn reads_manifest(dep_info: &Path, manifest: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(dep_info) else {
        return false;
    };
    let wanted = std::fs::canonicalize(manifest).unwrap_or_else(|_| manifest.to_path_buf());
    text.lines()
        .filter(|l| !l.starts_with('#'))
        .flat_map(|l| l.split_whitespace())
        .map(|t| t.trim_end_matches(':'))
        .filter(|t| t.ends_with("Cargo.toml") || t.ends_with("Rune.toml"))
        .any(|t| {
            let p = Path::new(t);
            std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()) == wanted
        })
}

/// The C toolchain a build script may drive, as one string: the compiler
/// (`$CC`, else `cc`) and archiver, by path, size, mtime and `--version`.
/// Cached for the process. It only enters keys of units *with* build scripts;
/// a system compiler update therefore rebuilds `cc`-driven crates and nothing
/// else — a false negative, which the spec accepts, never a stale artifact.
pub fn native_tools_identity() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| {
        let mut out = String::new();
        for (var, default) in [("CC", "cc"), ("AR", "ar")] {
            let tool = std::env::var(var).unwrap_or_else(|_| default.to_string());
            let program = tool
                .split_whitespace()
                .next()
                .unwrap_or(default)
                .to_string();
            let path = PathBuf::from(&program);
            out.push_str(&format!(
                "{var}={}|{}|",
                program,
                rune_common::toolchain::program_fingerprint(&path)
            ));
            let version = std::process::Command::new(&program)
                .arg("--version")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| {
                    String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .next()
                        .unwrap_or("")
                        .to_string()
                })
                .unwrap_or_default();
            out.push_str(&version);
            out.push('\n');
        }
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_inputs_leave_a_plain_key_alone() {
        assert_eq!(effective_key("base", None, false), "base");
        // Unrecorded and recorded-as-empty are one and the same, or a unit
        // could never find what it stored after its first build.
        assert_eq!(
            effective_key("base", None, true),
            effective_key("base", Some(&DeclaredInputs::default()), true)
        );
        assert_eq!(
            effective_key("base", Some(&DeclaredInputs::default()), false),
            "base"
        );
        // A build script always brings the native toolchain into the key.
        assert_ne!(effective_key("base", None, true), "base");
    }

    #[test]
    fn the_key_follows_the_values_of_declared_environment_variables() {
        let inputs = DeclaredInputs {
            env: vec!["RUNE_TEST_INPUT_VAR".into()],
            ..Default::default()
        };
        // SAFETY-free: the variable name is unique to this test.
        unsafe { std::env::remove_var("RUNE_TEST_INPUT_VAR") };
        let unset = effective_key("k", Some(&inputs), false);
        unsafe { std::env::set_var("RUNE_TEST_INPUT_VAR", "one") };
        let one = effective_key("k", Some(&inputs), false);
        unsafe { std::env::set_var("RUNE_TEST_INPUT_VAR", "two") };
        let two = effective_key("k", Some(&inputs), false);
        unsafe { std::env::remove_var("RUNE_TEST_INPUT_VAR") };
        assert_ne!(unset, one);
        assert_ne!(one, two);
        assert_eq!(unset, effective_key("k", Some(&inputs), false));
    }

    #[test]
    fn declared_files_and_native_libraries_are_fingerprinted() {
        let dir = tempfile::tempdir().unwrap();
        let header = dir.path().join("x.h");
        let lib = dir.path().join("libx.so");
        std::fs::write(&header, "#define A 1").unwrap();
        std::fs::write(&lib, "elf").unwrap();
        let inputs = DeclaredInputs {
            files: vec![header.clone(), dir.path().join("missing.h")],
            native: vec![lib.clone()],
            ..Default::default()
        };
        let before = inputs.fingerprint();
        assert_eq!(before, inputs.fingerprint(), "stable while nothing changes");
        std::fs::write(&header, "#define A 2").unwrap();
        assert_ne!(before, inputs.fingerprint(), "a changed header is noticed");
        let after_header = inputs.fingerprint();
        std::fs::write(&lib, "a longer library file").unwrap();
        assert_ne!(
            after_header,
            inputs.fingerprint(),
            "a changed library is noticed"
        );
    }

    #[test]
    fn script_output_becomes_inputs_but_package_local_paths_do_not() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        let out_dir = dir.path().join("out");
        std::fs::create_dir_all(&pkg).unwrap();
        let external = dir.path().join("system.h");
        let mut out = BuildScriptOutput {
            rerun_if_env_changed: vec!["CC".into(), "CC".into()],
            rerun_if_changed: vec![PathBuf::from("build.rs"), external.clone()],
            ..Default::default()
        };
        let inputs = from_script_output(&out, &pkg, &out_dir);
        assert_eq!(inputs.env, ["CC"]);
        assert_eq!(
            inputs.files,
            std::slice::from_ref(&external),
            "only files outside the package add information"
        );

        // The manifest is the exception: it is left out of the package hash.
        out.rerun_if_changed.push(PathBuf::from("Cargo.toml"));
        let with_manifest = from_script_output(&out, &pkg, &out_dir);
        assert!(with_manifest.files.contains(&pkg.join("Cargo.toml")));
        // And a script with no `rerun-if-changed` at all depends on it too.
        let silent = from_script_output(&BuildScriptOutput::default(), &pkg, &out_dir);
        assert_eq!(silent.files, [pkg.join("Cargo.toml")]);
    }

    #[test]
    fn env_reads_come_from_dep_info_minus_what_rune_provides() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join("x.d");
        std::fs::write(
            &d,
            "x: src/lib.rs\n\n# env-dep:CARGO_PKG_NAME=demo\n# env-dep:BUILD_ID=42\n# env-dep:OPTIONAL_THING\n",
        )
        .unwrap();
        let names = env_reads_from_dep_info(&d, &|n| n.starts_with("CARGO_"));
        assert_eq!(names, ["BUILD_ID", "OPTIONAL_THING"]);
    }
}
