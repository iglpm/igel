//! Core types shared across all Rune crates.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Component, Path, PathBuf};

// ---------------------------------------------------------------------------
// CrateId — uniquely identifies a crate in the dependency graph
// ---------------------------------------------------------------------------

/// Uniquely identifies a crate by name, version, and source.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CrateId {
    /// The crate name (e.g., "serde").
    pub name: String,
    /// The semver version (e.g., "1.0.210").
    pub version: semver::Version,
    /// Where the crate comes from.
    pub source: CrateSource,
}

impl CrateId {
    /// Create a new `CrateId`.
    pub fn new(name: impl Into<String>, version: semver::Version, source: CrateSource) -> Self {
        Self {
            name: name.into(),
            version,
            source,
        }
    }

    /// Create a `CrateId` for a local path dependency.
    pub fn local(name: impl Into<String>, version: semver::Version) -> Self {
        Self::new(name, version, CrateSource::Local)
    }

    /// Returns the normalized crate name (hyphens → underscores),
    /// matching Rust's identifier rules.
    pub fn crate_name_normalized(&self) -> String {
        self.name.replace('-', "_")
    }
}

impl fmt::Display for CrateId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} v{}", self.name, self.version)
    }
}

// ---------------------------------------------------------------------------
// CrateSource — where a crate comes from
// ---------------------------------------------------------------------------

/// The origin of a crate, with everything that makes its *content* immutable.
///
/// A cache key for a non-local crate never reads its files, so the source
/// identity has to pin the content on its own: a registry package by index and
/// archive checksum, a Git package by the exact commit (a branch name would
/// change under it) plus the state of its submodules.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CrateSource {
    /// A crate from crates.io or another registry.
    Registry {
        /// Canonical index URL (`sparse+https://index.crates.io/`, ...).
        index: String,
        /// SHA-256 of the `.crate` archive, when known.
        checksum: Option<String>,
    },
    /// A local path dependency or workspace member: mutable, hashed by content.
    Local,
    /// A git dependency, resolved to an exact commit.
    Git {
        url: String,
        /// The resolved commit (never a branch or tag name).
        commit: String,
        /// Where the package lives inside the repository.
        subdir: Option<String>,
        /// Commits of every checked-out submodule, `path=commit` joined by `,`.
        submodules: String,
    },
}

impl CrateSource {
    /// A crates.io registry package.
    pub fn crates_io(checksum: Option<String>) -> Self {
        Self::Registry {
            index: CRATES_IO_INDEX.to_string(),
            checksum,
        }
    }

    /// A stable string describing this source, for hashing.
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

/// The canonical index URL of crates.io.
pub const CRATES_IO_INDEX: &str = "sparse+https://index.crates.io/";

impl Default for CrateSource {
    fn default() -> Self {
        Self::crates_io(None)
    }
}

// ---------------------------------------------------------------------------
// Edition — Rust edition
// ---------------------------------------------------------------------------

/// Rust edition for a crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum Edition {
    /// Rust 2015 edition.
    #[serde(rename = "2015")]
    E2015,
    /// Rust 2018 edition.
    #[serde(rename = "2018")]
    E2018,
    /// Rust 2021 edition.
    #[serde(rename = "2021")]
    #[default]
    E2021,
    /// Rust 2024 edition.
    #[serde(rename = "2024")]
    E2024,
}

impl Edition {
    /// Returns the edition as a string suitable for `--edition` flag.
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

// ---------------------------------------------------------------------------
// CrateType — what kind of artifact to produce
// ---------------------------------------------------------------------------

/// The type of artifact to compile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CrateType {
    /// Rust library (`rlib`).
    Lib,
    /// Rust dynamic library.
    Dylib,
    /// C-compatible dynamic library.
    Cdylib,
    /// C-compatible static library.
    Staticlib,
    /// Procedural macro (host-target shared library).
    ProcMacro,
    /// Executable binary.
    Bin,
    /// Rust library (synonym for `lib`).
    Rlib,
}

impl CrateType {
    /// Returns the `--crate-type` flag value for rustc.
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

    /// Whether this crate type runs on the host (not the target).
    pub fn is_host_artifact(&self) -> bool {
        matches!(self, Self::ProcMacro)
    }
}

impl fmt::Display for CrateType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.rustc_crate_type())
    }
}

// ---------------------------------------------------------------------------
// Profile — build profile settings
// ---------------------------------------------------------------------------

/// Build profile configuration, mirroring Cargo profiles.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Profile {
    /// Profile name (e.g., "dev", "release").
    pub name: String,
    /// Optimization level: 0, 1, 2, 3, "s", "z".
    pub opt_level: OptLevel,
    /// Debug info level.
    pub debuginfo: DebugInfo,
    /// Whether debug assertions are enabled.
    pub debug_assertions: bool,
    /// Whether overflow checks are enabled.
    pub overflow_checks: bool,
    /// LTO mode.
    pub lto: LtoMode,
    /// Panic strategy.
    pub panic: PanicStrategy,
    /// Number of codegen units (None = compiler default).
    pub codegen_units: Option<u32>,
    /// Whether incremental compilation is enabled.
    pub incremental: bool,
    /// Rpath setting.
    pub rpath: bool,
    /// `-C strip=` value (`none`, `debuginfo`, `symbols`), if set.
    #[serde(default)]
    pub strip: Option<String>,
    /// `-C split-debuginfo=` value (`off`, `packed`, `unpacked`), if set.
    #[serde(default)]
    pub split_debuginfo: Option<String>,
    /// `trim-paths`: sources are named relative to their package or
    /// workspace in the output (debuginfo, panic messages), not by where
    /// they happen to be on this machine.
    #[serde(default)]
    pub trim_paths: bool,
}

impl Profile {
    /// The default `dev` profile.
    pub fn dev() -> Self {
        Self {
            name: "dev".into(),
            opt_level: OptLevel::O0,
            debuginfo: DebugInfo::Full,
            debug_assertions: true,
            overflow_checks: true,
            lto: LtoMode::Off,
            panic: PanicStrategy::Unwind,
            // Left to rustc: 16 for ordinary compiles, 256 automatically when
            // incremental compilation is on (which is how Cargo behaves too).
            codegen_units: None,
            incremental: true,
            rpath: false,
            strip: None,
            split_debuginfo: None,
            trim_paths: false,
        }
    }

    /// The default `release` profile.
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

    /// The built-in `test` profile (inherits `dev`).
    pub fn test() -> Self {
        Self {
            name: "test".into(),
            ..Self::dev()
        }
    }

    /// The built-in `bench` profile (inherits `release`).
    pub fn bench() -> Self {
        Self {
            name: "bench".into(),
            ..Self::release()
        }
    }

    /// Stable fingerprint of every setting that changes generated code.
    ///
    /// Used in cache keys: two profiles that differ in *any* codegen-relevant
    /// setting (LTO, panic strategy, codegen-units, overflow checks, ...) must
    /// never share a cached artifact. The profile *name* is deliberately not
    /// included — a custom profile identical to `release` produces identical
    /// artifacts and should share its cache entries.
    pub fn codegen_fingerprint(&self) -> String {
        // Only mentioned when on, so that other profiles keep their keys.
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

    /// The `-C split-debuginfo` a unit built for `target` gets: the profile's
    /// own setting, or else Cargo's platform default — `unpacked` on Apple
    /// targets whenever there is debuginfo. rustc's own default there is
    /// `packed`, which runs `dsymutil` over every binary it links (about 10 %
    /// of a cold debug build on macOS).
    pub fn split_debuginfo_for(&self, target: &str) -> Option<&str> {
        match &self.split_debuginfo {
            Some(split) => Some(split),
            None if self.debuginfo != DebugInfo::None && target.contains("-apple-") => {
                Some("unpacked")
            }
            None => None,
        }
    }

    /// Whether this is a "release-like" profile (optimized).
    pub fn is_release(&self) -> bool {
        self.name == "release"
            || matches!(
                self.opt_level,
                OptLevel::O2 | OptLevel::O3 | OptLevel::Os | OptLevel::Oz
            )
    }

    /// Returns the directory name for build outputs: `debug` for `dev`/`test`,
    /// `release` for `release`/`bench`, and the profile's own name for custom
    /// profiles (so `--profile profiling` never overwrites `release` output).
    pub fn output_dir_name(&self) -> &str {
        match self.name.as_str() {
            "dev" | "test" => "debug",
            "release" | "bench" => "release",
            other => other,
        }
    }
}

/// Optimization level.
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
    /// Returns the `-C opt-level=` value.
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

/// Debug info level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DebugInfo {
    None,
    LineTablesOnly,
    Limited,
    Full,
}

impl DebugInfo {
    /// Returns the `-C debuginfo=` value.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "0",
            Self::LineTablesOnly => "line-tables-only",
            Self::Limited => "1",
            Self::Full => "2",
        }
    }
}

/// LTO (link-time optimization) mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LtoMode {
    Off,
    Thin,
    Fat,
}

impl LtoMode {
    /// Returns the `-C lto=` value, or `None` if off.
    pub fn as_flag(&self) -> Option<&'static str> {
        match self {
            Self::Off => None,
            Self::Thin => Some("thin"),
            Self::Fat => Some("fat"),
        }
    }
}

/// Panic strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PanicStrategy {
    Unwind,
    Abort,
}

impl PanicStrategy {
    /// Returns the `-C panic=` value.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Unwind => "unwind",
            Self::Abort => "abort",
        }
    }
}

// ---------------------------------------------------------------------------
// TargetTriple
// ---------------------------------------------------------------------------

/// A target triple (e.g., `x86_64-unknown-linux-gnu`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TargetTriple(pub String);

impl TargetTriple {
    /// The host triple, as the build's compiler reports it.
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

// ---------------------------------------------------------------------------
// PackageMeta — manifest metadata exposed to the compiler as `CARGO_PKG_*`
// ---------------------------------------------------------------------------

/// Manifest metadata that crates read at compile time via `env!("CARGO_PKG_…")`
/// (e.g. clap's `command!()` reads the description and authors).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct PackageMeta {
    /// The *package* name (`CARGO_PKG_NAME`). Empty means "same as the unit's
    /// crate name". Differs from the crate name for a `[[bin]]` or test
    /// target named other than its package.
    pub name: String,
    /// `CARGO_PKG_AUTHORS` — authors joined with `:`.
    pub authors: String,
    pub description: String,
    pub homepage: String,
    pub repository: String,
    pub license: String,
    pub license_file: String,
    pub readme: String,
    pub rust_version: String,
}

// ---------------------------------------------------------------------------
// TargetKind — which manifest target a unit was built from
// ---------------------------------------------------------------------------

/// The manifest target a compilation unit comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum TargetKind {
    /// `[lib]`.
    #[default]
    Lib,
    /// `[[bin]]`.
    Bin,
    /// `[[example]]`.
    Example,
    /// `[[test]]` (integration test).
    Test,
    /// `[[bench]]`.
    Bench,
}

impl TargetKind {
    /// Short lowercase name used in output and cache keys.
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

// ---------------------------------------------------------------------------
// UnitExtras — per-unit build configuration beyond the package identity
// ---------------------------------------------------------------------------

/// Everything about how one unit is compiled that is *not* implied by the
/// request-wide profile and target. All of it is part of the unit's identity.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct UnitExtras {
    /// Compiled for the host machine: a build script's dependencies, a
    /// proc-macro and everything only it uses. Such a unit never gets
    /// `--target`, whatever the request says.
    pub for_host: bool,
    /// The unit's effective profile when it differs from the request's:
    /// `[profile.*.package.*]` and `[profile.*.build-override]` applied.
    pub profile: Option<Profile>,
    /// Further `--crate-type`s beyond [`CompilationUnit::crate_type`]
    /// (`[lib] crate-type = ["cdylib", "rlib"]`).
    pub extra_crate_types: Vec<CrateType>,
    /// A workspace member (as opposed to a dependency): the
    /// `RUSTC_WORKSPACE_WRAPPER` applies, and warnings are shown.
    pub member: bool,
    /// The effective `rustflags` for this unit's role (env, config, target).
    pub rustflags: Vec<String>,
    /// `target.<triple>.linker` for units that link.
    pub linker: Option<String>,
    /// Flags and linker for this unit's *build script*, which always runs on
    /// the host (so, when cross-compiling, differ from `rustflags`/`linker`).
    pub script_rustflags: Vec<String>,
    pub script_linker: Option<String>,
    /// The profile the package's build script is compiled with
    /// (`build-override`).
    pub script_profile: Option<Profile>,
    /// `crate-name-hash` of the unit that runs this package's build script,
    /// when it is not this unit. A package's script runs once and every unit
    /// of the package — library, binaries, tests — shares its output (and its
    /// `OUT_DIR`), as in Cargo.
    pub script_owner: Option<String>,
    /// The package's declared `[features]` (and optional dependencies), for
    /// `--check-cfg cfg(feature, values(...))`.
    pub declared_features: Vec<String>,
    /// Hash of the normalized custom target specification, if the target is a
    /// JSON file: the name alone can't identify it.
    pub target_spec: Option<String>,
    /// Environment from the `[env]` configuration table, `(name, value)`. It
    /// reaches the compiler and can end up in the binary through `env!`, so
    /// it is part of the unit's identity.
    pub env: Vec<(String, String)>,
    /// Whether the unit is part of `--target` builds' *cross* graph, i.e.
    /// host units are distinct from target units even without differing
    /// features.
    pub cross: bool,
}

// ---------------------------------------------------------------------------
// CompilationUnit — a single unit of compilation
// ---------------------------------------------------------------------------

/// Represents a single unit of work for the compiler.
///
/// A compilation unit is the combination of a crate, its active features,
/// the target, profile, and crate type. Two compilation units with different
/// features or profiles are treated as distinct.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CompilationUnit {
    /// Which crate this unit belongs to.
    pub crate_id: CrateId,
    /// What kind of artifact to produce.
    pub crate_type: CrateType,
    /// The root source file (e.g., `src/lib.rs` or `src/main.rs`).
    pub source_root: PathBuf,
    /// The manifest directory.
    pub manifest_dir: PathBuf,
    /// Active features for this compilation.
    pub features: Vec<String>,
    /// Build profile.
    pub profile_name: String,
    /// Target triple (None = host).
    pub target: Option<TargetTriple>,
    /// Edition.
    pub edition: Edition,
    /// Whether this is a build script (build.rs) unit.
    pub is_build_script: bool,
    /// `rune check`: typecheck only, emitting `.rmeta` but skipping codegen
    /// and linking entirely — never produces a runnable binary or a linkable
    /// `.rlib`. Kept as a distinct field (rather than folded into an
    /// existing one) so it can feed both `metadata_hash` and the emit-flag
    /// decision without the two ever disagreeing about which mode a given
    /// unit was built in.
    pub check_only: bool,
    /// The `links` field from Cargo.toml, if any.
    pub links: Option<String>,
    /// The resolved build script entry point for this crate, if it has one.
    ///
    /// Resolved from the package's `build` manifest field (which may point
    /// anywhere, e.g. `build = "build/build.rs"`), not assumed to always be
    /// `<manifest_dir>/build.rs`. `None` means the crate has no build script,
    /// whether because it never had one or because `build = false` disabled
    /// an otherwise-present one.
    pub build_script: Option<PathBuf>,
    /// `CARGO_PKG_*` metadata from the package manifest.
    pub meta: PackageMeta,
    /// Extra `rustc` arguments from the package's `[lints]` tables (only ever
    /// set for local packages; dependencies compile with `--cap-lints allow`).
    pub lint_args: Vec<String>,
    /// The Cargo target name (`[lib] name`, a `[[bin]]`/`[[test]]` name, ...),
    /// which is what `rustc --crate-name` receives. Empty means "derive it from
    /// the crate id", which is right for ordinary registry libraries.
    pub target_name: String,
    /// Which manifest target this unit builds.
    pub kind: TargetKind,
    /// Compile as a test harness (`rustc --test`): the unit produces a
    /// runnable test executable for its target instead of the target itself.
    pub harness: bool,
    /// Per-unit build configuration (host role, effective profile, flags).
    pub ext: UnitExtras,
}

impl CompilationUnit {
    /// The crate name rustc compiles this unit as (`--crate-name`, and the
    /// default `--extern` name dependents use): the Cargo target name with
    /// hyphens turned into underscores.
    pub fn rustc_crate_name(&self) -> String {
        if self.target_name.is_empty() {
            self.crate_id.crate_name_normalized()
        } else {
            self.target_name.replace('-', "_")
        }
    }

    /// Every crate type this unit produces, primary first.
    pub fn all_crate_types(&self) -> impl Iterator<Item = CrateType> + '_ {
        std::iter::once(self.crate_type).chain(self.ext.extra_crate_types.iter().copied())
    }

    /// The triple the unit is compiled for: its `--target`, or the host for
    /// host units (build-script dependencies, proc-macros) and native builds.
    pub fn compiled_for(&self) -> &str {
        match &self.target {
            Some(t) if !self.ext.for_host && self.crate_type != CrateType::ProcMacro => &t.0,
            _ => &crate::toolchain::global().host,
        }
    }

    /// The file name of the executable this (binary or test) unit links,
    /// given its hash-free stem: `.exe` on Windows, `.wasm` for bare wasm.
    pub fn executable_name(&self, stem: &str) -> String {
        format!("{stem}{}", exe_suffix(self.compiled_for()))
    }

    /// The profile this unit compiles with: its own override, else `base`.
    pub fn effective_profile<'a>(&'a self, base: &'a Profile) -> &'a Profile {
        self.ext.profile.as_ref().unwrap_or(base)
    }

    /// Returns a unique disambiguation hash for this unit,
    /// used in `-C metadata=` and `-C extra-filename=`.
    pub fn metadata_hash(&self) -> String {
        use crate::hash::HashBuilder;
        let mut hb = HashBuilder::new();
        hb.update(self.crate_id.name.as_bytes());
        hb.update(self.crate_id.version.to_string().as_bytes());
        // The same name and version from two sources (a registry crate and
        // the git checkout patched over it) are different crates.
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
        // Two packages may both have e.g. `tests/it.rs`; the package name
        // keeps their outputs (which share a directory) apart.
        hb.update(self.meta.name.as_bytes());
        hb.update(self.kind.as_str().as_bytes());
        hb.update(if self.harness { b"harness" } else { b"plain" });
        // Only mixed in when set, so ordinary units keep their identity.
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
        // Take the first 16 hex chars of the blake3 hash
        let hash = hb.finish_blake3();
        hash[..16].to_string()
    }

    /// Whether this unit's own compilation must produce a real linked
    /// artifact rather than just type-checked `.rmeta`.
    ///
    /// True for a proc-macro (loaded into the compiler process itself,
    /// which can't substitute metadata for the genuine compiled dylib)
    /// regardless of mode, and, outside `rune check`, for anything else
    /// that's actually linked (a binary or [cd]ylib). `rune check`
    /// deliberately answers `false` even for those, which is what lets it
    /// skip codegen for the *entire* graph instead of just its leaf
    /// (library) crates — the difference between `rune check` actually
    /// being faster than `rune build` and being a relabeled copy of it.
    ///
    /// Deliberately independent of whether the crate merely *has* a build
    /// script: a build script is a separately scheduled step that needs its
    /// *build*-dependencies linked, and says nothing about whether *this*
    /// crate's own `rustc` invocation needs `.rlib`s instead of `.rmeta`s
    /// from its dependencies.
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

// ---------------------------------------------------------------------------
// BuildScriptOutput — parsed build.rs stdout directives
// ---------------------------------------------------------------------------

/// Parsed output from a `build.rs` build script execution.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BuildScriptOutput {
    /// Libraries to link (`-l` flags).
    pub link_libs: Vec<LinkLib>,
    /// Library search paths (`-L` flags).
    pub link_search: Vec<LinkSearch>,
    /// Arguments passed straight to the linker (`cargo::rustc-link-arg*`),
    /// each with the kinds of target it applies to.
    pub link_args: Vec<LinkArg>,
    /// `--cfg` flags to pass to rustc.
    pub cfgs: Vec<String>,
    /// `--check-cfg` flags.
    pub check_cfgs: Vec<String>,
    /// Environment variables to set during compilation.
    pub env_vars: Vec<(String, String)>,
    /// Paths that trigger re-run of the build script.
    pub rerun_if_changed: Vec<PathBuf>,
    /// Environment variables that trigger re-run.
    pub rerun_if_env_changed: Vec<String>,
    /// Metadata exported to dependent crates via `DEP_<LINKS>_<KEY>`.
    pub metadata: Vec<(String, String)>,
    /// Warnings emitted by the build script.
    pub warnings: Vec<String>,
    /// Errors the script reported with `cargo::error=`; any of them fails the build.
    pub errors: Vec<String>,
    /// What the compiler printed when compiling the script (warnings about
    /// `build.rs` itself), kept so they can be shown again when the result is
    /// restored, like a crate's warnings.
    #[serde(default)]
    pub compile_warnings: String,
    /// The OUT_DIR the script ran with when its output was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_out_dir: Option<PathBuf>,
    /// The CARGO_MANIFEST_DIR the script ran with when its output was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_manifest_dir: Option<PathBuf>,
}

impl BuildScriptOutput {
    pub const OUT_DIR_PLACEHOLDER: &'static str = "${RUNE_OUT_DIR}";
    pub const MANIFEST_DIR_PLACEHOLDER: &'static str = "${RUNE_MANIFEST_DIR}";

    /// Makes this output fit to store: the store is shared by every project
    /// (and machine) and finds a script's result by keys that do not say where
    /// the project lives, so paths into the script's `OUT_DIR` or its package
    /// become placeholders that [`Self::relocate`] fills in for the project
    /// that restores it. Both spellings of each directory are recognized, the
    /// one the script was given and its 8.3 short form on Windows, which tools
    /// such as CMake report.
    pub fn normalize(&mut self, out_dir: &Path, manifest_dir: &Path) {
        let mut map = Vec::new();
        // `OUT_DIR` first: it may lie inside the package (a root package's
        // target directory).
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

    /// Whether this output was stored by [`Self::normalize`]. Entries written
    /// before it hold the absolute paths of whichever project ran the script
    /// first, and must not be used anywhere else.
    pub fn is_portable(&self) -> bool {
        self.recorded_out_dir.is_some()
    }

    /// Points the placeholders written by [`Self::normalize`] at this
    /// project's `OUT_DIR` and package.
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

    /// Whether a directive still names one of `dirs` (in either spelling, see
    /// [`text_names_dir`]) — after [`Self::normalize`], a path it could not
    /// make portable, such as one into another package's `OUT_DIR`.
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

    /// Replaces every directory `from` that starts a path, or appears in a
    /// string value, with `to` — the first that matches, for a path.
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

/// `path` relative to `prefix`, comparing components (case-insensitively on
/// Windows, where `C:\` and `c:\` are the same directory).
fn path_strip_prefix(path: &Path, prefix: &Path) -> Option<PathBuf> {
    let mut rest = path.components();
    for want in prefix.components() {
        if !component_eq(&rest.next()?, &want) {
            return None;
        }
    }
    Some(rest.as_path().to_path_buf())
}

/// Whether `text` (a value, or a generated source file) names the existing
/// directory `dir`: in its own spelling or its 8.3 short form, and on Windows
/// with either slash, escaped backslashes (`C:\\dir` in a Rust string
/// literal) or another case.
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

/// Replaces each occurrence of the directory `dir` in `s` that is a whole
/// path prefix (followed by a separator or a character that cannot continue a
/// file name), in either slash direction and, on Windows, ignoring case.
fn replace_dir_in_str(s: &str, dir: &Path, replacement: &str) -> String {
    let dir = dir.display().to_string();
    if dir.is_empty() {
        return s.to_string();
    }
    let fold = |t: &str| {
        // ASCII folding keeps byte offsets the same in both strings (full
        // Unicode lowercasing can change lengths, and the offsets index `s`).
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

/// Which artifacts a `cargo::rustc-link-arg*` directive applies to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LinkArgScope {
    /// `rustc-link-arg`: every linked artifact of the package.
    All,
    /// `rustc-cdylib-link-arg` / `rustc-link-arg-cdylib`.
    Cdylib,
    /// `rustc-link-arg-bins`.
    Bins,
    /// `rustc-link-arg-bin=NAME`.
    Bin(String),
    /// `rustc-link-arg-tests`.
    Tests,
    /// `rustc-link-arg-examples`.
    Examples,
    /// `rustc-link-arg-benches`.
    Benches,
}

/// A linker argument requested by a build script.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkArg {
    pub scope: LinkArgScope,
    pub arg: String,
}

/// A native library to link.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkLib {
    /// Library kind (static, dylib, framework, etc.).
    pub kind: Option<String>,
    /// Library name.
    pub name: String,
}

/// A library search path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkSearch {
    /// Path kind (native, dependency, framework, etc.).
    pub kind: Option<String>,
    /// Search path.
    pub path: PathBuf,
}

// ---------------------------------------------------------------------------
// RustcArtifact — JSON artifact event from rustc
// ---------------------------------------------------------------------------

/// A JSON artifact event emitted by rustc via `--json=artifacts`.
#[derive(Debug, Clone, Deserialize)]
pub struct RustcArtifact {
    /// The artifact file path.
    pub artifact: PathBuf,
    /// The emit kind ("metadata", "link", "dep-info").
    pub emit: String,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The file-name suffix of an executable for a target triple.
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
        // No debuginfo, nothing to split.
        assert_eq!(
            Profile::release().split_debuginfo_for("aarch64-apple-darwin"),
            None
        );
        // An explicit setting always wins.
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
                // Another package's `OUT_DIR` is not this script's to move.
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
            // 8.3 names are disabled on this volume.
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
