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
        self.fingerprint_in(None)
    }

    /// Like [`Self::fingerprint`], with relative paths read below `root`
    /// (the package they belong to) and hashed under their relative name, so
    /// the same package in another checkout gets the same key.
    pub fn fingerprint_in(&self, root: Option<&Path>) -> String {
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
            match root {
                Some(r) if path.is_relative() => hb.update(hash_path(&r.join(path)).as_bytes()),
                _ => hb.update(hash_path(path).as_bytes()),
            }
        }
        for path in &self.native {
            hb.update(b"\0native=");
            hb.update(path.to_string_lossy().as_bytes());
            hb.update(stat_fingerprint(path).as_bytes());
        }
        hb.finish_blake3()
    }
}

/// What a build script's result was found to depend on, recorded after it
/// runs (`<script base>-script-record`) and used to key its next lookup.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ScriptRecord {
    /// Declared files (inside the package too), environment variables, native
    /// libraries, and the script's own source files.
    pub inputs: DeclaredInputs,
    /// It declared `rerun-if-changed`: then only those files matter, not every
    /// file of the package.
    pub watches_files: bool,
}

/// The record of a script run: what it declared (`rerun-if-*`), the native
/// libraries it points the linker at, and `sources`, the files its own
/// compilation read.
pub fn script_record(
    out: &BuildScriptOutput,
    manifest_dir: &Path,
    out_dir: &Path,
    sources: &[PathBuf],
) -> ScriptRecord {
    let mut inputs = DeclaredInputs {
        env: out.rerun_if_env_changed.clone(),
        native: from_script_output(out, manifest_dir, out_dir).native,
        ..Default::default()
    };
    for p in &out.rerun_if_changed {
        let abs = if p.is_absolute() {
            p.clone()
        } else {
            manifest_dir.join(p)
        };
        // What it writes itself is an output, not an input.
        if !abs.starts_with(out_dir) {
            inputs.files.push(abs);
        }
    }
    inputs.files.extend(sources.iter().cloned());
    // Files of the package itself are recorded relative to it: the record is
    // shared by every package with the same script base (another checkout, or
    // another project with a package of the same name), and each must be keyed
    // by its own files — never by the absolute paths of the one that ran first.
    for f in &mut inputs.files {
        if let Ok(rel) = f.strip_prefix(manifest_dir) {
            *f = rel.to_path_buf();
        }
    }
    inputs.merge(DeclaredInputs::default());
    ScriptRecord {
        inputs,
        watches_files: !out.rerun_if_changed.is_empty(),
    }
}

/// The key a build script's result is stored under. `base` covers everything
/// the script can observe apart from files: the unit's configuration, its
/// build-dependencies and the `DEP_*` values it receives. A script that
/// declared `rerun-if-changed` is then keyed by the contents of exactly
/// those files (and its own sources), so editing the rest of the package
/// does not run it again; one that declared nothing is keyed by every file
/// of the package, as Cargo reruns it on any change (`package_hash`).
pub fn script_result_key(
    base: &str,
    record: Option<&ScriptRecord>,
    package_hash: impl FnOnce() -> String,
) -> String {
    script_result_key_in(base, record, None, package_hash)
}

/// [`script_result_key`] for the package in `manifest_dir`, whose files the
/// record names relative to it.
pub fn script_result_key_in(
    base: &str,
    record: Option<&ScriptRecord>,
    manifest_dir: Option<&Path>,
    package_hash: impl FnOnce() -> String,
) -> String {
    let keyed = |base: &str, inputs: Option<&DeclaredInputs>| -> String {
        let declared = inputs.filter(|i| !i.is_empty());
        let mut hb = HashBuilder::new();
        hb.update(base.as_bytes());
        if let Some(inputs) = declared {
            hb.update(b"\0inputs=");
            hb.update(inputs.fingerprint_in(manifest_dir).as_bytes());
        }
        hb.update(b"\0native-tools=");
        hb.update(native_tools_identity().as_bytes());
        hb.finish_blake3()
    };
    match record {
        Some(r) if r.watches_files => keyed(&format!("{base}\0watched"), Some(&r.inputs)),
        _ => {
            let mut hb = HashBuilder::new();
            hb.update(base.as_bytes());
            hb.update(b"\0package=");
            hb.update(package_hash().as_bytes());
            keyed(&hb.finish_blake3(), record.map(|r| &r.inputs))
        }
    }
}

/// The files a local crate's compilation read, from rustc's dep-info: its
/// modules and whatever `include_str!`/`include_bytes!` pulled in. Recorded
/// after it compiles (under `<read record key>-read`) and used to key its
/// next lookup by exactly these files' contents, so a change to a file it
/// never read — an integration test, the README, a log — does not compile
/// it again (and everything above it), as with Cargo.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadRecord {
    /// Relative to the package directory (with `..` for files beside it), so
    /// another checkout finds the same record; absolute when the file shares
    /// nothing but the root with the package.
    pub files: Vec<PathBuf>,
}

impl ReadRecord {
    /// The record of a dep-info file written by a rustc that ran in `cwd`
    /// (which its relative paths are relative to), for the package in
    /// `manifest_dir`.
    pub fn from_dep_info(dep_info: &Path, cwd: &Path, manifest_dir: &Path) -> Self {
        let mut files: Vec<PathBuf> = dep_info_sources(dep_info)
            .into_iter()
            .map(|f| relative_to(&cwd.join(f), manifest_dir))
            .collect();
        files.sort();
        files.dedup();
        Self { files }
    }

    /// Where each recorded file is now.
    pub fn paths(&self, manifest_dir: &Path) -> Vec<(String, PathBuf)> {
        self.files
            .iter()
            .map(|f| {
                (
                    f.to_string_lossy().replace('\\', "/"),
                    normalize(&manifest_dir.join(f)),
                )
            })
            .collect()
    }
}

/// `path` without `.` and `..` components (lexically).
fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// `path` relative to the directory `base`, through their common ancestor;
/// `path` itself when they share no more than the root.
pub fn relative_to(path: &Path, base: &Path) -> PathBuf {
    let (path, base) = (normalize(path), normalize(base));
    let p: Vec<_> = path.components().collect();
    let b: Vec<_> = base.components().collect();
    let common = p.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let shares_more_than_root = p
        .iter()
        .take(common)
        .any(|c| matches!(c, std::path::Component::Normal(_)));
    if !shares_more_than_root {
        return path;
    }
    let mut out = PathBuf::new();
    for _ in common..b.len() {
        out.push("..");
    }
    for c in &p[common..] {
        out.push(c);
    }
    out
}

/// The source files a rustc dep-info file lists (`path:` lines).
pub fn dep_info_sources(dep_info: &Path) -> Vec<PathBuf> {
    let Ok(text) = std::fs::read_to_string(dep_info) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.strip_suffix(':'))
        .filter(|l| !l.contains(": "))
        .map(|l| PathBuf::from(l.replace("\\ ", " ")))
        .collect()
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
pub(crate) fn hash_path(path: &Path) -> String {
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
                        .push(rune_common::fsutil::canonicalize(&file).unwrap_or(file));
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
/// Relative paths in it are relative to `base`, the directory the compiler
/// ran in.
pub fn reads_manifest(dep_info: &Path, manifest: &Path, base: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(dep_info) else {
        return false;
    };
    let wanted =
        rune_common::fsutil::canonicalize(manifest).unwrap_or_else(|_| manifest.to_path_buf());
    text.lines()
        .filter(|l| !l.starts_with('#'))
        .flat_map(|l| l.split_whitespace())
        .map(|t| t.trim_end_matches(':'))
        .filter(|t| t.ends_with("Cargo.toml") || t.ends_with("Rune.toml"))
        .any(|t| {
            let p = base.join(t);
            rune_common::fsutil::canonicalize(&p).unwrap_or(p) == wanted
        })
}

/// Variables that change what rustc emits for an Apple target without rustc
/// listing them in its dep-info: the minimum OS version written into every
/// object file, and the SDK binaries are linked against.
const APPLE_ENVIRONMENT: [&str; 6] = [
    "MACOSX_DEPLOYMENT_TARGET",
    "IPHONEOS_DEPLOYMENT_TARGET",
    "TVOS_DEPLOYMENT_TARGET",
    "WATCHOS_DEPLOYMENT_TARGET",
    "XROS_DEPLOYMENT_TARGET",
    "SDKROOT",
];

/// The [`APPLE_ENVIRONMENT`] variables that are set, for a unit compiled for
/// `triple`; nothing for other targets, or when none is set (so keys do not
/// change where they don't matter).
pub fn apple_environment(triple: &str) -> &'static [(&'static str, String)] {
    static SET: OnceLock<Vec<(&'static str, String)>> = OnceLock::new();
    if !triple.contains("-apple-") {
        return &[];
    }
    SET.get_or_init(|| {
        APPLE_ENVIRONMENT
            .iter()
            .filter_map(|k| Some((*k, std::env::var(k).ok()?)))
            .collect()
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
            let version = rune_common::toolchain::tool_version(&path);
            out.push_str(&version);
            out.push('\n');
        }
        if cfg!(target_os = "macos") {
            let developer_dir = std::env::var_os("DEVELOPER_DIR")
                .map(PathBuf::from)
                .or_else(|| std::fs::read_link("/var/db/xcode_select_link").ok())
                .unwrap_or_else(|| PathBuf::from("/Library/Developer/CommandLineTools"));
            let sdkroot = std::env::var_os("SDKROOT").map(PathBuf::from);
            out.push_str(&apple_developer_identity(
                &developer_dir,
                sdkroot.as_deref(),
            ));
        }
        out
    })
}

/// On macOS, `/usr/bin/cc` is a shim into the active developer directory
/// (Command Line Tools or Xcode, `xcode-select`): updating those changes the
/// compiler and the SDK without changing the shim. So the real `clang` and
/// the SDK are fingerprinted too — by path, size and mtime, without starting
/// `xcrun` (which would cost every build tens of milliseconds).
fn apple_developer_identity(developer_dir: &Path, sdkroot: Option<&Path>) -> String {
    let canonical =
        |p: &Path| rune_common::fsutil::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let mut out = format!("DEVELOPER_DIR={}\n", canonical(developer_dir).display());
    for clang in [
        "usr/bin/clang",
        "Toolchains/XcodeDefault.xctoolchain/usr/bin/clang",
    ] {
        let path = developer_dir.join(clang);
        if path.is_file() {
            out.push_str(&format!(
                "clang={}\n",
                rune_common::toolchain::program_fingerprint(&path)
            ));
        }
    }
    let sdks = match sdkroot {
        Some(sdk) => vec![sdk.to_path_buf()],
        None => vec![
            developer_dir.join("SDKs/MacOSX.sdk"),
            developer_dir.join("Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk"),
        ],
    };
    for sdk in sdks.iter().filter(|s| s.exists()) {
        // `MacOSX.sdk` is a link to the versioned SDK (`MacOSX26.1.sdk`).
        let settings = sdk.join("SDKSettings.json");
        out.push_str(&format!(
            "sdk={}|{}\n",
            canonical(sdk).display(),
            rune_common::toolchain::program_fingerprint(&settings)
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_files_are_recorded_relative_to_the_package() {
        let p = |s: &str| PathBuf::from(s);
        assert_eq!(
            relative_to(&p("/w/pkg/src/lib.rs"), &p("/w/pkg")),
            p("src/lib.rs")
        );
        assert_eq!(
            relative_to(&p("/w/shared.txt"), &p("/w/pkg")),
            p("../shared.txt")
        );
        assert_eq!(
            relative_to(&p("/w/pkg/src/../../other/x.rs"), &p("/w/pkg")),
            p("../other/x.rs")
        );
        // Nothing in common but the root: kept as it is.
        assert_eq!(
            relative_to(&p("/usr/include/x.h"), &p("/w/pkg")),
            p("/usr/include/x.h")
        );

        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join("lib.d");
        std::fs::write(
            &d,
            "/out/libx.rlib: pkg/src/lib.rs pkg/src/m.rs pkg/data.txt\n\n\
             pkg/src/lib.rs:\npkg/src/m.rs:\npkg/data.txt:\n../elsewhere/y.rs:\n\
             # env-dep:X=1\n",
        )
        .unwrap();
        let record = ReadRecord::from_dep_info(&d, &p("/w"), &p("/w/pkg"));
        // (`/w/../elsewhere` shares nothing with `/w/pkg` but the root.)
        assert_eq!(
            record.files,
            [
                p("/elsewhere/y.rs"),
                p("data.txt"),
                p("src/lib.rs"),
                p("src/m.rs")
            ]
        );
        assert_eq!(
            record.paths(&p("/w/pkg"))[1],
            ("data.txt".to_string(), p("/w/pkg/data.txt"))
        );
    }

    #[test]
    fn a_script_that_watches_files_is_keyed_by_them_not_the_package() {
        let dir = tempfile::tempdir().unwrap();
        let build_rs = dir.path().join("build.rs");
        std::fs::write(&build_rs, "fn main() {}").unwrap();
        let out = BuildScriptOutput {
            rerun_if_changed: vec![PathBuf::from("build.rs")],
            ..Default::default()
        };
        let record = script_record(&out, dir.path(), &dir.path().join("out"), &[]);
        assert!(record.watches_files);
        let a = script_result_key_in("base", Some(&record), Some(dir.path()), || {
            "package v1".into()
        });
        let b = script_result_key_in("base", Some(&record), Some(dir.path()), || {
            "package v2".into()
        });
        assert_eq!(a, b, "other files of the package do not matter");
        std::fs::write(&build_rs, "fn main() { println!(); }").unwrap();
        assert_ne!(
            a,
            script_result_key_in("base", Some(&record), Some(dir.path()), || "package v2"
                .into())
        );

        // Without `rerun-if-changed`, every file of the package counts.
        let quiet = script_record(
            &BuildScriptOutput::default(),
            dir.path(),
            &dir.path().join("out"),
            &[],
        );
        assert!(!quiet.watches_files);
        assert_ne!(
            script_result_key("base", Some(&quiet), || "package v1".into()),
            script_result_key("base", Some(&quiet), || "package v2".into())
        );
        assert_ne!(
            script_result_key("base", None, || "package v1".into()),
            script_result_key("base", None, || "package v2".into())
        );
    }

    #[test]
    fn apple_developer_tools_are_identified_by_the_real_compiler_and_sdk() {
        let dev = tempfile::tempdir().unwrap();
        let clang = dev.path().join("usr/bin/clang");
        let sdk = dev.path().join("SDKs/MacOSX26.0.sdk");
        std::fs::create_dir_all(clang.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&sdk).unwrap();
        std::fs::write(&clang, "clang 17").unwrap();
        std::fs::write(sdk.join("SDKSettings.json"), "{}").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&sdk, dev.path().join("SDKs/MacOSX.sdk")).unwrap();
        #[cfg(not(unix))]
        std::fs::create_dir_all(dev.path().join("SDKs/MacOSX.sdk")).unwrap();

        let before = apple_developer_identity(dev.path(), None);
        assert!(before.contains("clang="), "{before}");
        assert!(before.contains("sdk="), "{before}");
        // An update to the command line tools: a new compiler binary.
        std::fs::write(&clang, "clang 18, a longer binary").unwrap();
        let after = apple_developer_identity(dev.path(), None);
        assert_ne!(before, after);
        // An explicit SDKROOT replaces the default SDK.
        let other = dev.path().join("Other.sdk");
        std::fs::create_dir_all(&other).unwrap();
        let chosen = apple_developer_identity(dev.path(), Some(&other));
        assert!(
            chosen.contains("Other.sdk") && !chosen.contains("MacOSX"),
            "{chosen}"
        );
    }

    #[test]
    fn apple_environment_only_applies_to_apple_targets() {
        assert!(apple_environment("x86_64-unknown-linux-gnu").is_empty());
        assert!(apple_environment("wasm32-unknown-unknown").is_empty());
    }

    #[test]
    fn dep_info_lists_the_sources() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join("x.d");
        std::fs::write(
            &d,
            "/o/build_script_build: /p/build.rs /p/build/gen.rs\n\n/p/build.rs:\n/p/build/gen.rs:\n\n# env-dep:X=1\n",
        )
        .unwrap();
        assert_eq!(
            dep_info_sources(&d),
            [
                PathBuf::from("/p/build.rs"),
                PathBuf::from("/p/build/gen.rs")
            ]
        );
    }

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
