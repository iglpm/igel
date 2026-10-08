use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CrateId {
    pub name: String,
    pub version: semver::Version,
    pub source: CrateSource,
}

impl CrateId {
    pub fn new(name: impl Into<String>, version: semver::Version, source: CrateSource) -> Self {
        Self {
            name: name.into(),
            version,
            source,
        }
    }

    pub fn local(name: impl Into<String>, version: semver::Version) -> Self {
        Self::new(name, version, CrateSource::Local)
    }

    pub fn crate_name_normalized(&self) -> String {
        self.name.replace('-', "_")
    }
}

impl fmt::Display for CrateId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} v{}", self.name, self.version)
    }
}

/// Origin of a crate, pinning its content: registry index + checksum, or exact git commit + submodules.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CrateSource {
    Registry {
        index: String,
        checksum: Option<String>,
    },
    Local,
    Git {
        url: String,
        commit: String,
        subdir: Option<String>,
        submodules: String,
    },
}

impl CrateSource {
    pub fn crates_io(checksum: Option<String>) -> Self {
        Self::Registry {
            index: CRATES_IO_INDEX.to_string(),
            checksum,
        }
    }

    pub fn identity(&self) -> String {
        match self {
            Self::Registry { index, checksum } => {
                format!("registry|{index}|{}", checksum.as_deref().unwrap_or("-"))
            }
            Self::Local => "local".to_string(),
            Self::Git {
                url,
                commit,
                subdir,
                submodules,
            } => format!(
                "git|{url}|{commit}|{}|{submodules}",
                subdir.as_deref().unwrap_or("")
            ),
        }
    }
}

pub const CRATES_IO_INDEX: &str = "sparse+https://index.crates.io/";

impl Default for CrateSource {
    fn default() -> Self {
        Self::crates_io(None)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum Edition {
    #[serde(rename = "2015")]
    E2015,
    #[serde(rename = "2018")]
    E2018,
    #[serde(rename = "2021")]
    #[default]
    E2021,
    #[serde(rename = "2024")]
    E2024,
}

impl Edition {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::E2015 => "2015",
            Self::E2018 => "2018",
            Self::E2021 => "2021",
            Self::E2024 => "2024",
        }
    }
}

impl fmt::Display for Edition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Edition {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "2015" => Ok(Self::E2015),
            "2018" => Ok(Self::E2018),
            "2021" => Ok(Self::E2021),
            "2024" => Ok(Self::E2024),
            other => Err(format!("unknown edition: {other}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CrateType {
    Lib,
    Dylib,
    Cdylib,
    Staticlib,
    ProcMacro,
    Bin,
    Rlib,
}

impl CrateType {
    pub fn rustc_crate_type(&self) -> &'static str {
        match self {
            Self::Lib | Self::Rlib => "lib",
            Self::Dylib => "dylib",
            Self::Cdylib => "cdylib",
            Self::Staticlib => "staticlib",
            Self::ProcMacro => "proc-macro",
            Self::Bin => "bin",
        }
    }

    pub fn is_host_artifact(&self) -> bool {
        matches!(self, Self::ProcMacro)
    }
}

impl fmt::Display for CrateType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.rustc_crate_type())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    pub opt_level: OptLevel,
    pub debuginfo: DebugInfo,
    pub debug_assertions: bool,
    pub overflow_checks: bool,
    pub lto: LtoMode,
    pub panic: PanicStrategy,
    pub codegen_units: Option<u32>,
    pub incremental: bool,
    pub rpath: bool,
    #[serde(default)]
    pub strip: Option<String>,
    #[serde(default)]
    pub split_debuginfo: Option<String>,
    #[serde(default)]
    pub trim_paths: bool,
}

impl Profile {
    pub fn dev() -> Self {
        Self {
            name: "dev".into(),
            opt_level: OptLevel::O0,
            debuginfo: DebugInfo::Full,
            debug_assertions: true,
            overflow_checks: true,
            lto: LtoMode::Off,
            panic: PanicStrategy::Unwind,
            // Left to rustc (16, or 256 with incremental), as Cargo does.
            codegen_units: None,
            incremental: true,
            rpath: false,
            strip: None,
            split_debuginfo: None,
            trim_paths: false,
        }
    }

    pub fn release() -> Self {
        Self {
            name: "release".into(),
            opt_level: OptLevel::O3,
            debuginfo: DebugInfo::None,
            debug_assertions: false,
            overflow_checks: false,
            lto: LtoMode::Off,
            panic: PanicStrategy::Unwind,
            codegen_units: None,
            incremental: false,
            rpath: false,
            strip: None,
            split_debuginfo: None,
            trim_paths: false,
        }
    }

    pub fn test() -> Self {
        Self {
            name: "test".into(),
            ..Self::dev()
        }
    }

    pub fn bench() -> Self {
        Self {
            name: "bench".into(),
            ..Self::release()
        }
    }

    /// Every codegen-relevant setting, for cache keys. The profile name is deliberately excluded.
    pub fn codegen_fingerprint(&self) -> String {
        let trim = if self.trim_paths { "|trim" } else { "" };
        format!(
            "opt={}|debug={}|da={}|oc={}|lto={}|panic={}|cgu={:?}|rpath={}|strip={:?}|split={:?}{trim}",
            self.opt_level.as_str(),
            self.debuginfo.as_str(),
            self.debug_assertions,
            self.overflow_checks,
            self.lto.as_flag().unwrap_or("off"),
            self.panic.as_str(),
            self.codegen_units,
            self.rpath,
            self.strip,
            self.split_debuginfo,
        )
    }

    /// Cargo's default on Apple targets is `unpacked` (rustc's `packed` runs dsymutil on every link).
    pub fn split_debuginfo_for(&self, target: &str) -> Option<&str> {
        match &self.split_debuginfo {
            Some(split) => Some(split),
            None if self.debuginfo != DebugInfo::None && target.contains("-apple-") => {
                Some("unpacked")
            }
            None => None,
        }
    }

    pub fn is_release(&self) -> bool {
        self.name == "release"
            || matches!(
                self.opt_level,
                OptLevel::O2 | OptLevel::O3 | OptLevel::Os | OptLevel::Oz
            )
    }

    /// `debug`, `release`, or the custom profile's own name.
    pub fn output_dir_name(&self) -> &str {
        match self.name.as_str() {
            "dev" | "test" => "debug",
            "release" | "bench" => "release",
            other => other,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OptLevel {
    O0,
    O1,
    O2,
    O3,
    Os,
    Oz,
}

impl OptLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::O0 => "0",
            Self::O1 => "1",
            Self::O2 => "2",
            Self::O3 => "3",
            Self::Os => "s",
            Self::Oz => "z",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DebugInfo {
    None,
    LineTablesOnly,
    Limited,
    Full,
}

impl DebugInfo {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "0",
            Self::LineTablesOnly => "line-tables-only",
            Self::Limited => "1",
            Self::Full => "2",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LtoMode {
    Off,
    Thin,
    Fat,
}

impl LtoMode {
    pub fn as_flag(&self) -> Option<&'static str> {
        match self {
            Self::Off => None,
            Self::Thin => Some("thin"),
            Self::Fat => Some("fat"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PanicStrategy {
    Unwind,
    Abort,
}

impl PanicStrategy {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Unwind => "unwind",
            Self::Abort => "abort",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TargetTriple(pub String);

impl TargetTriple {
    pub fn host() -> Option<Self> {
        let host = &crate::toolchain::global().host;
        (!host.is_empty()).then(|| TargetTriple(host.clone()))
    }
}

impl fmt::Display for TargetTriple {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct PackageMeta {
    pub name: String,
    pub authors: String,
    pub description: String,
    pub homepage: String,
    pub repository: String,
    pub license: String,
    pub license_file: String,
    pub readme: String,
    pub rust_version: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum TargetKind {
    #[default]
    Lib,
    Bin,
    Example,
    Test,
    Bench,
}

impl TargetKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Lib => "lib",
            Self::Bin => "bin",
            Self::Example => "example",
            Self::Test => "test",
            Self::Bench => "bench",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct UnitExtras {
    /// Compiled for the host (build script deps, proc-macros): never gets `--target`.
    pub for_host: bool,
    pub profile: Option<Profile>,
    pub extra_crate_types: Vec<CrateType>,
    pub member: bool,
    pub rustflags: Vec<String>,
    pub linker: Option<String>,
    pub script_rustflags: Vec<String>,
    pub script_linker: Option<String>,
    pub script_profile: Option<Profile>,
    /// The unit that runs this package's build script, when not this one: all units share its output.
    pub script_owner: Option<String>,
    pub declared_features: Vec<String>,
    pub target_spec: Option<String>,
    /// `[env]` reaches the compiler (and `env!`), so it is part of the unit's identity.
    pub env: Vec<(String, String)>,
    pub cross: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CompilationUnit {
    pub crate_id: CrateId,
    pub crate_type: CrateType,
    pub source_root: PathBuf,
    pub manifest_dir: PathBuf,
    pub features: Vec<String>,
    pub profile_name: String,
    pub target: Option<TargetTriple>,
    pub edition: Edition,
    pub is_build_script: bool,
    pub check_only: bool,
    pub links: Option<String>,
    pub build_script: Option<PathBuf>,
    pub meta: PackageMeta,
    pub lint_args: Vec<String>,
    pub target_name: String,
    pub kind: TargetKind,
    pub harness: bool,
    pub ext: UnitExtras,
}

impl CompilationUnit {
    pub fn rustc_crate_name(&self) -> String {
        if self.target_name.is_empty() {
            self.crate_id.crate_name_normalized()
        } else {
            self.target_name.replace('-', "_")
        }
    }

    pub fn all_crate_types(&self) -> impl Iterator<Item = CrateType> + '_ {
        std::iter::once(self.crate_type).chain(self.ext.extra_crate_types.iter().copied())
    }

    pub fn compiled_for(&self) -> &str {
        match &self.target {
            Some(t) if !self.ext.for_host && self.crate_type != CrateType::ProcMacro => &t.0,
            _ => &crate::toolchain::global().host,
        }
    }

    pub fn executable_name(&self, stem: &str) -> String {
        format!("{stem}{}", exe_suffix(self.compiled_for()))
    }

    pub fn effective_profile<'a>(&'a self, base: &'a Profile) -> &'a Profile {
        self.ext.profile.as_ref().unwrap_or(base)
    }

    pub fn metadata_hash(&self) -> String {
        use crate::hash::HashBuilder;
        let mut hb = HashBuilder::new();
        hb.update(self.crate_id.name.as_bytes());
        hb.update(self.crate_id.version.to_string().as_bytes());
        // The same name and version from two sources are different crates.
        hb.update(self.crate_id.source.identity().as_bytes());
        hb.update(self.crate_type.rustc_crate_type().as_bytes());
        hb.update(self.profile_name.as_bytes());
        hb.update(self.edition.as_str().as_bytes());
        for feat in &self.features {
            hb.update(feat.as_bytes());
        }
        if let Some(t) = &self.target {
            hb.update(t.0.as_bytes());
        }
        hb.update(if self.is_build_script {
            b"build"
        } else {
            b"target"
        });
        hb.update(if self.check_only { b"check" } else { b"full" });
        hb.update(self.rustc_crate_name().as_bytes());
        hb.update(self.meta.name.as_bytes());
        hb.update(self.kind.as_str().as_bytes());
        hb.update(if self.harness { b"harness" } else { b"plain" });
        if self.ext.for_host {
            hb.update(b"\0host");
        }
        if let Some(p) = &self.ext.profile {
            hb.update(b"\0profile=");
            hb.update(p.codegen_fingerprint().as_bytes());
        }
        for ct in &self.ext.extra_crate_types {
            hb.update(b"\0ct=");
            hb.update(ct.rustc_crate_type().as_bytes());
        }
        if let Some(spec) = &self.ext.target_spec {
            hb.update(b"\0spec=");
            hb.update(spec.as_bytes());
        }
        let hash = hb.finish_blake3();
        hash[..16].to_string()
    }

    /// Whether this unit must produce a linked artifact rather than only `.rmeta`: proc-macros always,
    /// binaries and dylibs except under `rune check`.
    pub fn produces_real_artifact(&self) -> bool {
        self.crate_type == CrateType::ProcMacro
            || (self.harness && !self.check_only)
            || (!self.check_only
                && self.all_crate_types().any(|t| {
                    matches!(
                        t,
                        CrateType::Bin
                            | CrateType::Cdylib
                            | CrateType::Dylib
                            | CrateType::Staticlib
                    )
                }))
    }
}

impl fmt::Display for CompilationUnit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} v{} ({})",
            self.crate_id.name, self.crate_id.version, self.crate_type
        )
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BuildScriptOutput {
    pub link_libs: Vec<LinkLib>,
    pub link_search: Vec<LinkSearch>,
    pub link_args: Vec<LinkArg>,
    pub cfgs: Vec<String>,
    pub check_cfgs: Vec<String>,
    pub env_vars: Vec<(String, String)>,
    pub rerun_if_changed: Vec<PathBuf>,
    pub rerun_if_env_changed: Vec<String>,
    pub metadata: Vec<(String, String)>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
    #[serde(default)]
    pub compile_warnings: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_out_dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_manifest_dir: Option<PathBuf>,
}

impl BuildScriptOutput {
    pub const OUT_DIR_PLACEHOLDER: &'static str = "${RUNE_OUT_DIR}";
    pub const MANIFEST_DIR_PLACEHOLDER: &'static str = "${RUNE_MANIFEST_DIR}";

    /// Replaces paths into `OUT_DIR` and the package with placeholders, so the stored output is portable.
    pub fn normalize(&mut self, out_dir: &Path, manifest_dir: &Path) {
        let mut map = Vec::new();
        for (dir, placeholder) in [
            (out_dir, Self::OUT_DIR_PLACEHOLDER),
            (manifest_dir, Self::MANIFEST_DIR_PLACEHOLDER),
        ] {
            map.push((dir.to_path_buf(), placeholder.to_string()));
            if let Some(short) = crate::fsutil::short_path(dir) {
                map.push((short, placeholder.to_string()));
            }
        }
        self.rewrite(&map);
        self.recorded_out_dir = Some(out_dir.to_path_buf());
        self.recorded_manifest_dir = Some(manifest_dir.to_path_buf());
    }

    /// Older entries hold absolute paths and must not be used elsewhere.
    pub fn is_portable(&self) -> bool {
        self.recorded_out_dir.is_some()
    }

    pub fn relocate(&mut self, out_dir: &Path, manifest_dir: &Path) {
        self.rewrite(&[
            (
                PathBuf::from(Self::OUT_DIR_PLACEHOLDER),
                out_dir.display().to_string(),
            ),
            (
                PathBuf::from(Self::MANIFEST_DIR_PLACEHOLDER),
                manifest_dir.display().to_string(),
            ),
        ]);
        self.recorded_out_dir = Some(out_dir.to_path_buf());
        self.recorded_manifest_dir = Some(manifest_dir.to_path_buf());
    }

    pub fn names_any(&self, dirs: &[&Path]) -> bool {
        let paths = self
            .link_search
            .iter()
            .map(|l| l.path.as_path())
            .chain(self.rerun_if_changed.iter().map(PathBuf::as_path))
            .map(|p| p.to_string_lossy().into_owned());
        let strings = self
            .env_vars
            .iter()
            .chain(&self.metadata)
            .map(|(_, v)| v.clone())
            .chain(self.link_args.iter().map(|a| a.arg.clone()));
        paths
            .chain(strings)
            .any(|s| dirs.iter().any(|d| text_names_dir(&s, d)))
    }

    fn rewrite(&mut self, map: &[(PathBuf, String)]) {
        let path = |p: &Path| {
            map.iter()
                .find_map(|(from, to)| {
                    let rel = path_strip_prefix(p, from)?;
                    Some(if rel.as_os_str().is_empty() {
                        PathBuf::from(to)
                    } else {
                        Path::new(to).join(rel)
                    })
                })
                .unwrap_or_else(|| p.to_path_buf())
        };
        let string = |s: &str| {
            map.iter().fold(s.to_string(), |s, (from, to)| {
                replace_dir_in_str(&s, from, to)
            })
        };
        for ls in &mut self.link_search {
            ls.path = path(&ls.path);
        }
        for p in &mut self.rerun_if_changed {
            *p = path(p);
        }
        for (_, v) in self.env_vars.iter_mut().chain(&mut self.metadata) {
            *v = string(v);
        }
        for arg in &mut self.link_args {
            arg.arg = string(&arg.arg);
        }
    }
}

fn component_eq(a: &Component, b: &Component) -> bool {
    if cfg!(windows) {
        a.as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy())
    } else {
        a == b
    }
}

fn path_strip_prefix(path: &Path, prefix: &Path) -> Option<PathBuf> {
    let mut rest = path.components();
    for want in prefix.components() {
        if !component_eq(&rest.next()?, &want) {
            return None;
        }
    }
    Some(rest.as_path().to_path_buf())
}

pub fn text_names_dir(text: &str, dir: &Path) -> bool {
    let fold = |t: &str| {
        if cfg!(windows) {
            t.replace(r"\\", "/")
                .replace('\\', "/")
                .to_ascii_lowercase()
        } else {
            t.to_string()
        }
    };
    let text = fold(text);
    std::iter::once(dir.to_path_buf())
        .chain(crate::fsutil::short_path(dir))
        .map(|d| fold(&d.display().to_string()))
        .any(|d| !d.is_empty() && text.contains(&d))
}

fn replace_dir_in_str(s: &str, dir: &Path, replacement: &str) -> String {
    let dir = dir.display().to_string();
    if dir.is_empty() {
        return s.to_string();
    }
    let fold = |t: &str| {
        // ASCII folding keeps byte offsets equal.
        let t = if cfg!(windows) {
            t.replace('\\', "/")
        } else {
            t.to_string()
        };
        if cfg!(windows) {
            t.to_ascii_lowercase()
        } else {
            t
        }
    };
    let (hay, needle) = (fold(s), fold(&dir));
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    let mut from = 0;
    while let Some(pos) = hay[from..].find(&needle) {
        let start = from + pos;
        let end = start + needle.len();
        let ends_name = hay[end..]
            .chars()
            .next()
            .is_none_or(|c| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '~')));
        if ends_name {
            out.push_str(&s[last..start]);
            out.push_str(replacement);
            last = end;
        }
        from = end;
    }
    out.push_str(&s[last..]);
    out
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LinkArgScope {
    All,
    Cdylib,
    Bins,
    Bin(String),
    Tests,
    Examples,
    Benches,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkArg {
    pub scope: LinkArgScope,
    pub arg: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkLib {
    pub kind: Option<String>,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkSearch {
    pub kind: Option<String>,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RustcArtifact {
    pub artifact: PathBuf,
    pub emit: String,
}

pub fn exe_suffix(triple: &str) -> &'static str {
    if triple.contains("-windows") {
        ".exe"
    } else if triple.contains("emscripten") {
        ".js"
    } else if triple.starts_with("wasm32") || triple.starts_with("wasm64") {
        ".wasm"
    } else {
        ""
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apple_targets_default_to_unpacked_debuginfo_like_cargo() {
        let dev = Profile::dev();
        assert_eq!(
            dev.split_debuginfo_for("aarch64-apple-darwin"),
            Some("unpacked")
        );
        assert_eq!(dev.split_debuginfo_for("x86_64-unknown-linux-gnu"), None);
        assert_eq!(
            Profile::release().split_debuginfo_for("aarch64-apple-darwin"),
            None
        );
        let packed = Profile {
            split_debuginfo: Some("packed".into()),
            ..Profile::dev()
        };
        assert_eq!(
            packed.split_debuginfo_for("aarch64-apple-darwin"),
            Some("packed")
        );
    }

    #[test]
    fn test_edition_round_trip() {
        for ed in [
            Edition::E2015,
            Edition::E2018,
            Edition::E2021,
            Edition::E2024,
        ] {
            let s = ed.as_str();
            let parsed: Edition = s.parse().unwrap();
            assert_eq!(ed, parsed);
        }
    }

    #[test]
    fn test_crate_name_normalized() {
        let id = CrateId::local("my-crate", semver::Version::new(1, 0, 0));
        assert_eq!(id.crate_name_normalized(), "my_crate");
    }

    #[test]
    fn test_profile_dev_is_not_release() {
        assert!(!Profile::dev().is_release());
    }

    #[test]
    fn test_profile_release_is_release() {
        assert!(Profile::release().is_release());
    }

    #[test]
    fn test_metadata_hash_deterministic() {
        let unit = CompilationUnit {
            crate_id: CrateId::local("test", semver::Version::new(0, 1, 0)),
            crate_type: CrateType::Lib,
            source_root: PathBuf::from("src/lib.rs"),
            manifest_dir: PathBuf::from("."),
            features: vec!["std".into()],
            profile_name: "dev".into(),
            target: None,
            edition: Edition::E2021,
            is_build_script: false,
            check_only: false,
            links: None,
            build_script: None,
            meta: PackageMeta::default(),
            lint_args: Vec::new(),
            target_name: String::new(),
            kind: TargetKind::Lib,
            harness: false,
            ext: UnitExtras::default(),
        };
        let h1 = unit.metadata_hash();
        let h2 = unit.metadata_hash();
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 16);
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    fn base() -> CompilationUnit {
        CompilationUnit {
            crate_id: CrateId::local("t", semver::Version::new(0, 1, 0)),
            crate_type: CrateType::Bin,
            source_root: PathBuf::from("src/main.rs"),
            manifest_dir: PathBuf::from("."),
            features: vec![],
            profile_name: "dev".into(),
            target: None,
            edition: Edition::E2021,
            is_build_script: false,
            check_only: false,
            links: None,
            build_script: None,
            meta: PackageMeta::default(),
            lint_args: Vec::new(),
            target_name: String::new(),
            kind: TargetKind::Bin,
            harness: false,
            ext: UnitExtras::default(),
        }
    }

    #[test]
    fn harness_and_kind_change_the_metadata_hash() {
        let plain = base();
        let mut harness = base();
        harness.harness = true;
        let mut example = base();
        example.kind = TargetKind::Example;
        assert_ne!(plain.metadata_hash(), harness.metadata_hash());
        assert_ne!(plain.metadata_hash(), example.metadata_hash());
    }

    #[test]
    fn harness_needs_real_link_even_for_a_lib() {
        let mut h = base();
        h.crate_type = CrateType::Lib;
        assert!(!h.produces_real_artifact());
        h.harness = true;
        assert!(h.produces_real_artifact());
        h.check_only = true;
        assert!(
            !h.produces_real_artifact(),
            "`rune check` never links a harness"
        );
    }

    fn script_out(project: &Path, key: &str) -> PathBuf {
        project
            .join(".rune/target/debug/build")
            .join(key)
            .join("out")
    }

    #[test]
    fn stored_script_output_follows_the_project_that_restores_it() {
        let (old, new) = (Path::new("/tmp/copy/jolter"), Path::new("/home/me/jolter"));
        let (old_out, new_out) = (
            script_out(old, "cranelift_codegen-4a22"),
            script_out(new, "cranelift_codegen-4a22"),
        );
        let (old_pkg, new_pkg) = (old.join("crates/app"), new.join("crates/app"));
        let other_out = script_out(old, "libz_sys-99");
        let mut out = BuildScriptOutput {
            link_search: vec![LinkSearch {
                kind: Some("native".into()),
                path: old_out.join("x64"),
            }],
            env_vars: vec![
                ("ISLE_DIR".into(), old_out.display().to_string()),
                ("OTHER".into(), "not-a-path".into()),
            ],
            metadata: vec![
                (
                    "include".into(),
                    old_out.join("include").display().to_string(),
                ),
                ("zlib".into(), other_out.display().to_string()),
            ],
            link_args: vec![LinkArg {
                scope: LinkArgScope::Bins,
                arg: format!("/MANIFESTINPUT:{}", old_pkg.join("app.manifest").display()),
            }],
            rerun_if_changed: vec![old_pkg.join("build.rs")],
            ..Default::default()
        };
        assert!(!out.is_portable());

        out.normalize(&old_out, &old_pkg);
        assert!(out.is_portable());
        assert_eq!(out.env_vars[0].1, BuildScriptOutput::OUT_DIR_PLACEHOLDER);
        assert_eq!(
            out.link_search[0].path,
            Path::new(BuildScriptOutput::OUT_DIR_PLACEHOLDER).join("x64")
        );
        let stored = serde_json::to_string(&out).unwrap();

        let mut out: BuildScriptOutput = serde_json::from_str(&stored).unwrap();
        out.relocate(&new_out, &new_pkg);
        assert_eq!(out.link_search[0].path, new_out.join("x64"));
        assert_eq!(out.env_vars[0].1, new_out.display().to_string());
        assert_eq!(out.env_vars[1].1, "not-a-path");
        assert_eq!(
            Path::new(&out.metadata[0].1),
            new_out.join("include").as_path()
        );
        assert_eq!(out.metadata[1].1, other_out.display().to_string());
        assert_eq!(
            out.link_args[0].arg,
            format!("/MANIFESTINPUT:{}", new_pkg.join("app.manifest").display())
        );
        assert_eq!(out.rerun_if_changed[0], new_pkg.join("build.rs"));
    }

    #[test]
    fn output_stored_before_normalizing_is_not_portable() {
        let legacy: BuildScriptOutput =
            serde_json::from_str(r#"{"link_libs":[],"link_search":[],"link_args":[],"cfgs":[],"check_cfgs":[],"env_vars":[["ISLE_DIR","C:\\tmp\\x\\.rune\\target\\debug\\build\\c-1\\out"]],"rerun_if_changed":[],"rerun_if_env_changed":[],"metadata":[],"warnings":[],"errors":[]}"#)
                .unwrap();
        assert!(!legacy.is_portable());
    }

    #[test]
    fn only_whole_directories_are_replaced() {
        let dir = Path::new("/p/out");
        assert_eq!(
            replace_dir_in_str("/p/outer:/p/out", dir, "$O"),
            "/p/outer:$O"
        );
        assert_eq!(replace_dir_in_str("-L/p/out/lib", dir, "$O"), "-L$O/lib");
        assert_eq!(replace_dir_in_str("/p/out.txt", dir, "$O"), "/p/out.txt");
    }

    #[test]
    fn a_directory_named_in_generated_source_is_found() {
        let dir = Path::new("/home/me/hedge/apps/demo");
        let src = r#"include_bytes!("/home/me/hedge/apps/demo/../ui/dist/index.html")"#;
        assert!(text_names_dir(src, dir));
        assert!(!text_names_dir(src, Path::new("/home/me/other")));
        #[cfg(windows)]
        assert!(text_names_dir(
            r#"include_bytes!("C:\\tmp\\Hedge\\apps\\demo\\..\\ui\\index.html")"#,
            Path::new(r"C:\tmp\hedge\apps\demo"),
        ));
    }

    #[cfg(windows)]
    #[test]
    fn windows_directories_match_in_any_case_slash_and_short_form() {
        let dir = Path::new(r"C:\Proj\.rune\out");
        assert_eq!(
            replace_dir_in_str(r"c:/proj/.RUNE/out/inc;C:\Proj\.rune\out", dir, "$O"),
            "$O/inc;$O"
        );

        let tmp = tempfile::tempdir().unwrap();
        let out_dir = tmp.path().join("a long directory name").join("out");
        std::fs::create_dir_all(&out_dir).unwrap();
        let out_dir = crate::fsutil::canonicalize(&out_dir).unwrap();
        let Some(short) = crate::fsutil::short_path(&out_dir) else {
            return;
        };
        let mut out = BuildScriptOutput {
            metadata: vec![("root".into(), short.join("lib").display().to_string())],
            ..Default::default()
        };
        out.normalize(&out_dir, Path::new(r"C:\pkg"));
        assert_eq!(
            out.metadata[0].1,
            format!(r"{}\lib", BuildScriptOutput::OUT_DIR_PLACEHOLDER)
        );
    }
}
