//! External inputs: what a unit read beyond its source (env vars, system files, native libs, C tools).
//! Learned from build script declarations and dep-info, stored beside the base key; the effective key
//! adds their current values, so an artifact is found again only when they are unchanged.

use rune_common::BuildScriptOutput;
use rune_common::hash::HashBuilder;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclaredInputs {
    pub env: Vec<String>,
    pub files: Vec<PathBuf>,
    pub native: Vec<PathBuf>,
}

impl DeclaredInputs {
    pub fn is_empty(&self) -> bool {
        self.env.is_empty() && self.files.is_empty() && self.native.is_empty()
    }

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

    pub fn fingerprint(&self) -> String {
        self.fingerprint_in(None)
    }

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

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ScriptRecord {
    pub inputs: DeclaredInputs,
    pub watches_files: bool,
}

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
        if !abs.starts_with(out_dir) {
            inputs.files.push(abs);
        }
    }
    inputs.files.extend(sources.iter().cloned());
    // Relative to the package: the record is shared by every package with the same script base.
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

/// Keyed by `base` plus the contents of the `rerun-if-changed` files, or of every package file if none.
pub fn script_result_key(
    base: &str,
    record: Option<&ScriptRecord>,
    package_hash: impl FnOnce() -> String,
) -> String {
    script_result_key_in(base, record, None, package_hash)
}

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

/// The files a local crate's compilation read (dep-info), keying its next lookup by exactly these.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadRecord {
    pub files: Vec<PathBuf>,
}

impl ReadRecord {
    pub fn from_dep_info(
        dep_info: &Path,
        cwd: &Path,
        manifest_dir: &Path,
        target_dir: &Path,
    ) -> Self {
        let target_dir = normalize(&cwd.join(target_dir));
        let mut files: Vec<PathBuf> = dep_info_sources(dep_info)
            .into_iter()
            .map(|f| normalize(&cwd.join(f)))
            .filter(|f| !f.starts_with(&target_dir))
            .map(|f| relative_to(&f, manifest_dir))
            .collect();
        files.sort();
        files.dedup();
        Self { files }
    }

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

pub fn effective_key(
    base: &str,
    inputs: Option<&DeclaredInputs>,
    has_build_script: bool,
) -> String {
    // No recorded inputs and an empty set must give the same key.
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
        let is_manifest = abs == rune_common::manifest_file::manifest_in(manifest_dir);
        if is_manifest || (!abs.starts_with(manifest_dir) && !abs.starts_with(out_dir)) {
            inputs.files.push(abs);
        }
    }
    // Without `rerun-if-changed`, Cargo reruns a script on any package change, manifest included.
    if out.rerun_if_changed.is_empty() {
        inputs
            .files
            .push(rune_common::manifest_file::manifest_in(manifest_dir));
    }

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

/// Variables that change Apple output without appearing in dep-info.
const APPLE_ENVIRONMENT: [&str; 6] = [
    "MACOSX_DEPLOYMENT_TARGET",
    "IPHONEOS_DEPLOYMENT_TARGET",
    "TVOS_DEPLOYMENT_TARGET",
    "WATCHOS_DEPLOYMENT_TARGET",
    "XROS_DEPLOYMENT_TARGET",
    "SDKROOT",
];

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

/// The C toolchain a build script may drive; only in keys of units with build scripts.
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

/// `/usr/bin/cc` is a shim: fingerprint the real `clang` and SDK (without starting `xcrun`).
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
        let record = ReadRecord::from_dep_info(&d, &p("/w"), &p("/w/pkg"), &p("/w/.rune/target"));
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
        std::fs::write(&clang, "clang 18, a longer binary").unwrap();
        let after = apple_developer_identity(dev.path(), None);
        assert_ne!(before, after);
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
        assert_eq!(
            effective_key("base", None, true),
            effective_key("base", Some(&DeclaredInputs::default()), true)
        );
        assert_eq!(
            effective_key("base", Some(&DeclaredInputs::default()), false),
            "base"
        );
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

        out.rerun_if_changed.push(PathBuf::from("Cargo.toml"));
        let with_manifest = from_script_output(&out, &pkg, &out_dir);
        assert!(with_manifest.files.contains(&pkg.join("Cargo.toml")));
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
