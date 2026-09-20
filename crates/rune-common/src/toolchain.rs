//! The compiler a build uses, and everything about it that can change output.
//!
//! An artifact is only reusable when it was produced by an *equivalent*
//! compiler, so this module models the toolchain explicitly instead of
//! hashing `rustc -vV` and hoping:
//!
//! * which `rustc` runs (`RUSTC`, `build.rustc`, a `rust-toolchain(.toml)`
//!   resolved through `rustup`), and which `rustdoc`;
//! * `RUSTC_WRAPPER` / `build.rustc-wrapper` (`sccache`, `clippy-driver`, …) and
//!   `RUSTC_WORKSPACE_WRAPPER`, kept apart from the compiler and from each
//!   other, because each can change what gets compiled;
//! * the target: its `cfg` set as the compiler reports it, and for a custom
//!   target JSON the *normalized specification*, not just its file name;
//! * the effective `rustflags`, linker and runner for each role.
//!
//! [`Toolchain::identity`] folds the compiler side into one string that goes
//! into every cache key.

use crate::cargo_config::CargoConfig;
use crate::cfgexpr::{self, Cfg};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

/// A discovered compiler installation plus its wrappers.
#[derive(Debug, Clone)]
pub struct Toolchain {
    pub rustc: PathBuf,
    pub rustdoc: PathBuf,
    /// `RUSTC_WRAPPER` / `build.rustc-wrapper`.
    pub wrapper: Option<PathBuf>,
    /// `RUSTC_WORKSPACE_WRAPPER` / `build.rustc-workspace-wrapper`: applied to
    /// workspace members only.
    pub workspace_wrapper: Option<PathBuf>,
    /// The full `rustc -vV` output.
    pub verbose_version: String,
    /// The host triple `rustc` reports.
    pub host: String,
    pub version: semver::Version,
    identity: String,
}

impl Toolchain {
    /// Discovers the toolchain for a project rooted at `project_root`.
    pub fn discover(cfg: &CargoConfig, project_root: &Path) -> Toolchain {
        let non_empty = |v: Option<std::ffi::OsString>| v.filter(|s| !s.is_empty());

        let rustc: PathBuf = non_empty(std::env::var_os("RUSTC"))
            .map(PathBuf::from)
            .or_else(|| cfg.string(&["build", "rustc"]).map(PathBuf::from))
            .or_else(|| rustup_rustc(project_root))
            .unwrap_or_else(|| PathBuf::from("rustc"));

        let rustdoc: PathBuf = non_empty(std::env::var_os("RUSTDOC"))
            .map(PathBuf::from)
            .or_else(|| cfg.string(&["build", "rustdoc"]).map(PathBuf::from))
            .or_else(|| {
                // `rustdoc` lives next to the `rustc` it belongs to.
                let dir = rustc.parent().filter(|d| !d.as_os_str().is_empty())?;
                let sibling = dir.join("rustdoc");
                sibling.is_file().then_some(sibling)
            })
            .unwrap_or_else(|| PathBuf::from("rustdoc"));

        // An empty variable means "no wrapper", overriding the config file.
        let wrapper = match std::env::var_os("RUSTC_WRAPPER") {
            Some(v) if v.is_empty() => None,
            Some(v) => Some(PathBuf::from(v)),
            None => cfg
                .string(&["build", "rustc-wrapper"])
                .filter(|s| !s.is_empty())
                .map(PathBuf::from),
        };
        let workspace_wrapper = match std::env::var_os("RUSTC_WORKSPACE_WRAPPER") {
            Some(v) if v.is_empty() => None,
            Some(v) => Some(PathBuf::from(v)),
            None => cfg
                .string(&["build", "rustc-workspace-wrapper"])
                .filter(|s| !s.is_empty())
                .map(PathBuf::from),
        };

        let verbose_version = Command::new(&rustc)
            .arg("-vV")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        let host = verbose_version
            .lines()
            .find_map(|l| l.strip_prefix("host: "))
            .unwrap_or("x86_64-unknown-linux-gnu")
            .trim()
            .to_string();
        let version = verbose_version
            .lines()
            .find_map(|l| l.strip_prefix("release: "))
            .and_then(|v| semver::Version::parse(v.trim()).ok())
            .unwrap_or_else(|| semver::Version::new(0, 0, 0));

        let mut tc = Toolchain {
            rustc,
            rustdoc,
            wrapper,
            workspace_wrapper,
            verbose_version,
            host,
            version,
            identity: String::new(),
        };
        tc.identity = tc.compute_identity();
        tc
    }

    /// A string that differs whenever the compiler, its wrappers, or the
    /// compiler's own implementation differ.
    pub fn identity(&self) -> &str {
        &self.identity
    }

    /// The identity relevant to one unit. `RUSTC_WORKSPACE_WRAPPER` only
    /// wraps workspace members, so a dependency's artifacts must survive a
    /// change to it while a member's must not.
    pub fn identity_for(&self, workspace_member: bool) -> String {
        if workspace_member {
            self.identity.clone()
        } else {
            self.identity
                .lines()
                .filter(|l| !l.starts_with("workspace-wrapper="))
                .collect::<Vec<_>>()
                .join("\n")
        }
    }

    fn compute_identity(&self) -> String {
        let mut out = String::new();
        out.push_str("rustc=");
        out.push_str(&self.rustc.to_string_lossy());
        out.push('\n');
        out.push_str(&self.verbose_version);
        out.push('\n');
        // The version output is the same for two builds of the same commit,
        // and empty-ish for a locally linked development toolchain. The
        // compiler's actual implementation lives in `librustc_driver`, so
        // fingerprint that file too.
        out.push_str("driver=");
        out.push_str(&driver_fingerprint(&self.rustc));
        out.push('\n');
        for (label, tool) in [
            ("wrapper", &self.wrapper),
            ("workspace-wrapper", &self.workspace_wrapper),
        ] {
            match tool {
                Some(p) => out.push_str(&format!("{label}={}\n", program_fingerprint(p))),
                None => out.push_str(&format!("{label}=none\n")),
            }
        }
        out
    }

    /// Whether a `rust-version` requirement (`1.85`, `1.85.0`) is met.
    /// Pre-release suffixes on the toolchain (`-nightly`) are ignored, as
    /// Cargo ignores them for this check.
    pub fn satisfies_msrv(&self, msrv: &str) -> bool {
        match parse_rust_version(msrv) {
            Some(min) => {
                let cur = semver::Version::new(
                    self.version.major,
                    self.version.minor,
                    self.version.patch,
                );
                cur >= min
            }
            None => true,
        }
    }

    /// A `Command` that runs the compiler, through the configured wrappers.
    /// The workspace wrapper only applies when `workspace_member` is set.
    /// `wrapper workspace-wrapper rustc args…`, as Cargo orders them.
    pub fn rustc_command(&self, workspace_member: bool) -> Command {
        let mut chain: Vec<&Path> = Vec::new();
        if let Some(w) = &self.wrapper {
            chain.push(w);
        }
        if let (true, Some(w)) = (workspace_member, &self.workspace_wrapper) {
            chain.push(w);
        }
        chain.push(&self.rustc);
        let mut cmd = Command::new(chain[0]);
        for extra in &chain[1..] {
            cmd.arg(extra);
        }
        cmd
    }
}

/// `1.85` → `1.85.0`.
pub fn parse_rust_version(s: &str) -> Option<semver::Version> {
    let mut parts = s.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some(semver::Version::new(major, minor, patch))
}

static GLOBAL: OnceLock<Toolchain> = OnceLock::new();

/// Installs the process-wide toolchain. The first call wins.
pub fn set_global(tc: Toolchain) {
    let _ = GLOBAL.set(tc);
}

/// The process-wide toolchain, discovering one from the environment if none
/// was installed.
pub fn global() -> &'static Toolchain {
    GLOBAL.get_or_init(|| {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Toolchain::discover(&CargoConfig::load(&cwd), &cwd)
    })
}

/// `rustup which rustc` in `dir`, if a `rust-toolchain(.toml)` applies there.
/// Resolving to the concrete binary makes the choice explicit — and
/// fingerprintable — instead of depending on the `rustc` proxy's working
/// directory.
fn rustup_rustc(dir: &Path) -> Option<PathBuf> {
    let mut cur = Some(dir);
    let mut found = false;
    while let Some(d) = cur {
        if d.join("rust-toolchain.toml").is_file() || d.join("rust-toolchain").is_file() {
            found = true;
            break;
        }
        cur = d.parent();
    }
    if !found {
        return None;
    }
    let out = Command::new("rustup")
        .args(["which", "rustc"])
        .current_dir(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// Finds `program` on `PATH` (or returns it as is when it has a directory).
pub fn find_program(program: &Path) -> Option<PathBuf> {
    if program.components().count() > 1 || program.is_absolute() {
        return program.is_file().then(|| program.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(program))
        .find(|p| p.is_file())
}

/// Path plus size and mtime of a tool: enough to notice it was replaced or
/// updated, without reading it.
pub fn program_fingerprint(program: &Path) -> String {
    match find_program(program).and_then(|p| std::fs::metadata(&p).ok().map(|m| (p, m))) {
        Some((p, m)) => format!(
            "{}|{}|{}",
            p.display(),
            m.len(),
            m.modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0)
        ),
        None => format!("{}|missing", program.display()),
    }
}

fn driver_fingerprint(rustc: &Path) -> String {
    let sysroot = Command::new(rustc)
        .args(["--print", "sysroot"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()));
    let Some(sysroot) = sysroot else {
        return "unknown".into();
    };
    let mut parts = Vec::new();
    if let Ok(entries) = std::fs::read_dir(sysroot.join("lib")) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("librustc_driver") {
                if let Ok(m) = e.metadata() {
                    parts.push(format!(
                        "{name}:{}:{}",
                        m.len(),
                        m.modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_secs())
                            .unwrap_or(0)
                    ));
                }
            }
        }
    }
    parts.sort();
    parts.join(",")
}

// ---------------------------------------------------------------------------
// Targets
// ---------------------------------------------------------------------------

/// A compilation target as the compiler sees it.
#[derive(Debug, Clone)]
pub struct TargetInfo {
    /// The triple, or a custom target specification's file stem.
    pub name: String,
    /// What to pass to `--target` (a triple, or the path of a custom JSON).
    pub arg: String,
    /// For a custom specification: a hash of its *normalized* content.
    pub spec_hash: Option<String>,
    /// Everything `rustc --print=cfg` defines for this target.
    pub cfgs: Vec<Cfg>,
}

static TARGET_CACHE: OnceLock<Mutex<HashMap<String, TargetInfo>>> = OnceLock::new();

impl TargetInfo {
    /// Queries the compiler for `target` (`None` = the host) under `rustflags`
    /// (which can change the cfg set, e.g. `-C target-feature=+crt-static`).
    pub fn query(tc: &Toolchain, target: Option<&str>, rustflags: &[String]) -> TargetInfo {
        let arg = target
            .map(str::to_string)
            .unwrap_or_else(|| tc.host.clone());
        let key = format!("{}|{arg}|{}", tc.rustc.display(), rustflags.join("\x1f"));
        let cache = TARGET_CACHE.get_or_init(Default::default);
        if let Some(hit) = cache.lock().unwrap().get(&key) {
            return hit.clone();
        }

        let is_spec = arg.ends_with(".json");
        let (name, spec_hash) = if is_spec {
            (
                Path::new(&arg)
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| arg.clone()),
                normalized_spec_hash(Path::new(&arg)),
            )
        } else {
            (arg.clone(), None)
        };

        let run = |flags: &[String]| -> Option<String> {
            let mut cmd = Command::new(&tc.rustc);
            cmd.arg("--print=cfg").arg("--target").arg(&arg).args(flags);
            let out = cmd.output().ok()?;
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
        };
        let text = run(rustflags).or_else(|| run(&[])).unwrap_or_default();
        let info = TargetInfo {
            name,
            arg,
            spec_hash,
            cfgs: parse_cfg_output(&text),
        };
        cache.lock().unwrap().insert(key, info.clone());
        info
    }

    /// Whether a manifest platform string (`cfg(unix)` or a triple) applies.
    pub fn matches(&self, platform: &str) -> bool {
        cfgexpr::platform_matches(platform, &self.name, &self.cfgs)
    }

    /// `CARGO_CFG_*` for a build script: every cfg, values of a repeated key
    /// joined with commas. `debug_assertions` follows the profile.
    pub fn cargo_cfg_env(&self, debug_assertions: bool) -> Vec<(String, String)> {
        let mut map: std::collections::BTreeMap<String, Vec<String>> = Default::default();
        for (k, v) in &self.cfgs {
            if k == "debug_assertions" || k == "test" || k == "proc_macro" {
                continue;
            }
            let entry = map.entry(k.clone()).or_default();
            if let Some(v) = v {
                entry.push(v.clone());
            }
        }
        if debug_assertions {
            map.entry("debug_assertions".into()).or_default();
        }
        map.into_iter()
            .map(|(k, mut vs)| {
                vs.sort();
                (
                    format!(
                        "CARGO_CFG_{}",
                        k.chars()
                            .map(|c| if c.is_alphanumeric() {
                                c.to_ascii_uppercase()
                            } else {
                                '_'
                            })
                            .collect::<String>()
                    ),
                    vs.join(","),
                )
            })
            .collect()
    }
}

fn parse_cfg_output(text: &str) -> Vec<Cfg> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            match line.split_once('=') {
                Some((k, v)) => Some((
                    k.trim().to_string(),
                    Some(v.trim().trim_matches('"').to_string()),
                )),
                None => Some((line.to_string(), None)),
            }
        })
        .collect()
}

/// Hash of a custom target specification with keys sorted, so reformatting
/// the JSON doesn't invalidate artifacts but changing any field does.
fn normalized_spec_hash(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    // `serde_json::Value` objects are ordered maps, so re-serializing sorts keys.
    let canonical = serde_json::to_string(&value).ok()?;
    Some(crate::hash::blake3_bytes(canonical.as_bytes()))
}

// ---------------------------------------------------------------------------
// Flags, linker, runner
// ---------------------------------------------------------------------------

/// The build roles of a unit that can have different flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Compiled for the target platform.
    Target,
    /// A build script, proc-macro, or something only they use.
    Host,
}

/// Effective `rustflags` for a role, following Cargo's precedence: the first
/// of `CARGO_ENCODED_RUSTFLAGS`, `RUSTFLAGS`, the matching `target.*.rustflags`
/// tables, `build.rustflags` that is set wins. When cross-compiling
/// (`cross`), host artifacts ignore the first, second and last of those, so
/// `RUSTFLAGS=-Ctarget-cpu=…` aimed at the device doesn't break build scripts.
pub fn rustflags(cfg: &CargoConfig, target: &TargetInfo, role: Role, cross: bool) -> Vec<String> {
    let host_in_cross = role == Role::Host && cross;
    if !host_in_cross {
        if let Ok(encoded) = std::env::var("CARGO_ENCODED_RUSTFLAGS") {
            return encoded
                .split('\x1f')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
        }
        if let Ok(flags) = std::env::var("RUSTFLAGS") {
            return flags.split_whitespace().map(str::to_string).collect();
        }
    }
    let tables = cfg.target_tables(&target.name, &|k| target.matches(k));
    let mut from_targets: Vec<String> = Vec::new();
    let mut any = false;
    for (key, table) in &tables {
        // The environment can override a specific triple's flags.
        let env_flags = (!key.starts_with("cfg("))
            .then(|| std::env::var(CargoConfig::env_name(&["target", key, "rustflags"])).ok())
            .flatten();
        if let Some(v) = env_flags {
            any = true;
            from_targets.extend(v.split_whitespace().map(str::to_string));
            continue;
        }
        match table.get("rustflags") {
            Some(toml::Value::Array(a)) => {
                any = true;
                from_targets.extend(a.iter().filter_map(|v| v.as_str().map(str::to_string)));
            }
            Some(toml::Value::String(s)) => {
                any = true;
                from_targets.extend(s.split_whitespace().map(str::to_string));
            }
            _ => {}
        }
    }
    if any {
        return from_targets;
    }
    if !host_in_cross {
        return cfg.string_list(&["build", "rustflags"]).unwrap_or_default();
    }
    Vec::new()
}

fn target_string(cfg: &CargoConfig, target: &TargetInfo, key: &str) -> Option<String> {
    for (table_key, table) in cfg.target_tables(&target.name, &|k| target.matches(k)) {
        if !table_key.starts_with("cfg(") {
            if let Ok(v) = std::env::var(CargoConfig::env_name(&["target", table_key, key])) {
                return Some(v);
            }
        }
        match table.get(key) {
            Some(toml::Value::String(s)) => return Some(s.clone()),
            Some(toml::Value::Array(a)) if key == "runner" => {
                return Some(
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(" "),
                );
            }
            _ => {}
        }
    }
    // `CARGO_TARGET_<TRIPLE>_LINKER` works without any config file.
    std::env::var(CargoConfig::env_name(&["target", &target.name, key])).ok()
}

/// `target.<triple>.linker`.
pub fn linker(cfg: &CargoConfig, target: &TargetInfo) -> Option<PathBuf> {
    target_string(cfg, target, "linker").map(PathBuf::from)
}

/// `target.<triple>.runner`, split into program and arguments.
pub fn runner(cfg: &CargoConfig, target: &TargetInfo) -> Option<Vec<String>> {
    target_string(cfg, target, "runner")
        .map(|s| s.split_whitespace().map(str::to_string).collect())
        .filter(|v: &Vec<String>| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_versions_parse_and_compare() {
        assert_eq!(
            parse_rust_version("1.85"),
            Some(semver::Version::new(1, 85, 0))
        );
        assert_eq!(
            parse_rust_version("1.85.2"),
            Some(semver::Version::new(1, 85, 2))
        );
        assert_eq!(parse_rust_version("nope"), None);
    }

    #[test]
    fn cfg_output_parses_flags_and_key_values() {
        let cfgs = parse_cfg_output(
            "debug_assertions\ntarget_os=\"linux\"\nunix\ntarget_feature=\"sse\"\ntarget_feature=\"sse2\"\n",
        );
        assert!(cfgs.contains(&("unix".into(), None)));
        assert!(cfgs.contains(&("target_os".into(), Some("linux".into()))));
        let info = TargetInfo {
            name: "x".into(),
            arg: "x".into(),
            spec_hash: None,
            cfgs,
        };
        let env: HashMap<_, _> = info.cargo_cfg_env(false).into_iter().collect();
        assert_eq!(env["CARGO_CFG_TARGET_FEATURE"], "sse,sse2");
        assert_eq!(env["CARGO_CFG_UNIX"], "");
        assert!(!env.contains_key("CARGO_CFG_DEBUG_ASSERTIONS"));
        let env: HashMap<_, _> = info.cargo_cfg_env(true).into_iter().collect();
        assert!(env.contains_key("CARGO_CFG_DEBUG_ASSERTIONS"));
        assert!(info.matches("cfg(all(unix, target_feature = \"sse2\"))"));
    }

    #[test]
    fn live_query_of_the_host_matches_the_toolchain() {
        let tc = global();
        let info = TargetInfo::query(tc, None, &[]);
        assert_eq!(info.name, tc.host);
        assert!(!info.cfgs.is_empty(), "rustc --print=cfg returned nothing");
        assert!(info.matches(&tc.host));
        assert_eq!(
            info.matches("cfg(unix)"),
            cfg!(unix),
            "host cfg disagrees with how Rune itself was built"
        );
    }

    #[test]
    fn custom_target_specs_are_hashed_by_content_not_formatting() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.json");
        let b = dir.path().join("b.json");
        let c = dir.path().join("c.json");
        std::fs::write(&a, r#"{"llvm-target":"x","arch":"y"}"#).unwrap();
        std::fs::write(&b, "{\n  \"arch\": \"y\",\n  \"llvm-target\": \"x\"\n}").unwrap();
        std::fs::write(&c, r#"{"llvm-target":"x","arch":"z"}"#).unwrap();
        assert_eq!(normalized_spec_hash(&a), normalized_spec_hash(&b));
        assert_ne!(normalized_spec_hash(&a), normalized_spec_hash(&c));
    }

    #[test]
    fn wrapper_chain_order_matches_cargo() {
        let mut tc = global().clone();
        tc.wrapper = Some(PathBuf::from("sccache"));
        tc.workspace_wrapper = Some(PathBuf::from("clippy-driver"));
        let cmd = tc.rustc_command(true);
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(cmd.get_program().to_string_lossy(), "sccache");
        assert_eq!(args[0], "clippy-driver");
        assert_eq!(args[1], tc.rustc.to_string_lossy());
        // Dependencies (not workspace members) skip the workspace wrapper.
        let cmd = tc.rustc_command(false);
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, [tc.rustc.to_string_lossy().into_owned()]);
    }

    #[test]
    fn identity_changes_with_the_wrapper() {
        let base = global().clone();
        let mut wrapped = base.clone();
        wrapped.wrapper = Some(PathBuf::from("sccache"));
        assert_ne!(base.compute_identity(), wrapped.compute_identity());
        let mut ws = base.clone();
        ws.workspace_wrapper = Some(PathBuf::from("sccache"));
        assert_ne!(
            wrapped.compute_identity(),
            ws.compute_identity(),
            "RUSTC_WRAPPER and RUSTC_WORKSPACE_WRAPPER must not share artifacts"
        );
    }

    #[test]
    fn target_flags_come_from_matching_config_tables() {
        let mut cfg = CargoConfig::empty();
        cfg.add_str(
            r#"
            [build]
            rustflags = ["-Cbuild"]
            [target.'cfg(unix)']
            rustflags = ["-Cunix"]
            [target.wasm32-unknown-unknown]
            rustflags = ["-Cwasm"]
            linker = "wasm-ld"
            "#,
            Path::new("/p"),
        )
        .unwrap();
        let info = |name: &str, unix: bool| TargetInfo {
            name: name.into(),
            arg: name.into(),
            spec_hash: None,
            cfgs: if unix {
                vec![("unix".into(), None)]
            } else {
                vec![]
            },
        };
        // These tests must not see the developer's own RUSTFLAGS.
        if std::env::var_os("RUSTFLAGS").is_some()
            || std::env::var_os("CARGO_ENCODED_RUSTFLAGS").is_some()
        {
            return;
        }
        assert_eq!(
            rustflags(
                &cfg,
                &info("x86_64-unknown-linux-gnu", true),
                Role::Target,
                false
            ),
            ["-Cunix"]
        );
        assert_eq!(
            rustflags(
                &cfg,
                &info("wasm32-unknown-unknown", false),
                Role::Target,
                true
            ),
            ["-Cwasm"]
        );
        // Nothing matches: fall back to build.rustflags…
        assert_eq!(
            rustflags(&cfg, &info("riscv64-none", false), Role::Target, false),
            ["-Cbuild"]
        );
        // …but a host artifact in a cross build doesn't get it.
        assert!(rustflags(&cfg, &info("riscv64-none", false), Role::Host, true).is_empty());
        assert_eq!(
            linker(&cfg, &info("wasm32-unknown-unknown", false)),
            Some(PathBuf::from("wasm-ld"))
        );
    }
}
