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
    /// The compiler as configured (`RUSTC`, `build.rustc`, a toolchain file,
    /// or plain `rustc` from `PATH`) — what the identity names.
    pub rustc: PathBuf,
    /// The binary actually started: for a `rustc` found on `PATH`, the
    /// toolchain's own `rustc` in its sysroot, not a `rustup` proxy. Cargo
    /// does the same: a proxy costs ~7 ms per start, and resolves the
    /// toolchain again each time — from the directory it is started in, so a
    /// dependency shipping its own `rust-toolchain.toml` could get another
    /// compiler, and a `rustup update` during a build would mix two.
    pub exec: PathBuf,
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
    /// The compiler's sysroot, when it could be determined.
    pub sysroot: Option<PathBuf>,
    /// Which entry of the probe cache describes this compiler.
    probe_key: String,
    identity: String,
}

impl Toolchain {
    /// Why this compiler cannot be used, when it cannot: not installed, or a
    /// `rustup` without a toolchain for here. Asked once before building, so
    /// the answer is a sentence instead of an I/O error from the first
    /// compiler start.
    pub fn unusable_reason(&self) -> Option<String> {
        if !self.verbose_version.is_empty() {
            return None;
        }
        let configured = self.rustc.components().count() > 1;
        match Command::new(&self.rustc).arg("-vV").output() {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(if configured {
                format!(
                    "the Rust compiler `{}` does not exist (set by `RUSTC` or `build.rustc`)",
                    self.rustc.display()
                )
            } else {
                "no Rust compiler found: `rustc` is not on PATH. Install Rust with \
                 rustup (https://rustup.rs), or point `RUSTC` at a compiler"
                    .to_string()
            }),
            Err(e) => Some(format!(
                "the Rust compiler `{}` cannot be started: {e}",
                self.rustc.display()
            )),
            Ok(out) if !out.status.success() => {
                let why = String::from_utf8_lossy(&out.stderr);
                let why = why.trim();
                let mut msg = format!("`{} -vV` failed", self.rustc.display());
                if !why.is_empty() {
                    msg.push_str(":\n");
                    msg.push_str(why);
                }
                if why.contains("rustup") || why.contains("toolchain") {
                    msg.push_str("\n(install a toolchain with `rustup default stable`)");
                }
                Some(msg)
            }
            Ok(_) => None,
        }
    }

    /// Discovers the toolchain for a project rooted at `project_root`.
    pub fn discover(cfg: &CargoConfig, project_root: &Path) -> Toolchain {
        Self::discover_with_cache(cfg, project_root, &probe_cache_path())
    }

    /// [`Self::discover`], keeping compiler probes in `cache` (see
    /// [`ProbeCache`]).
    pub fn discover_with_cache(cfg: &CargoConfig, project_root: &Path, cache: &Path) -> Toolchain {
        let non_empty = |v: Option<std::ffi::OsString>| v.filter(|s| !s.is_empty());

        let configured: Option<PathBuf> = non_empty(std::env::var_os("RUSTC"))
            .map(PathBuf::from)
            .or_else(|| cfg.string(&["build", "rustc"]).map(PathBuf::from));
        let explicit = configured.is_some();
        let rustc: PathBuf = configured
            .or_else(|| rustup_rustc(project_root))
            .unwrap_or_else(|| PathBuf::from("rustc"));

        let probe = ProbeCache::open(cache).probe(&rustc);
        let sysroot = (!probe.sysroot.is_empty()).then(|| PathBuf::from(&probe.sysroot));
        // An explicitly configured compiler is started exactly as given (it
        // may be a script); `rustc` from `PATH` is resolved to the real one.
        let exec = sysroot
            .as_ref()
            .filter(|_| !explicit)
            .map(|s| s.join("bin").join(exe_name("rustc")))
            .filter(|p| p.is_file())
            .unwrap_or_else(|| rustc.clone());

        let rustdoc: PathBuf = non_empty(std::env::var_os("RUSTDOC"))
            .map(PathBuf::from)
            .or_else(|| cfg.string(&["build", "rustdoc"]).map(PathBuf::from))
            .or_else(|| {
                // `rustdoc` lives next to the `rustc` it belongs to.
                let dir = exec.parent().filter(|d| !d.as_os_str().is_empty())?;
                let sibling = dir.join(exe_name("rustdoc"));
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

        let verbose_version = probe.verbose_version.clone();
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
            exec,
            rustdoc,
            wrapper,
            workspace_wrapper,
            verbose_version,
            host,
            version,
            sysroot,
            probe_key: probe.key.clone(),
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
        out.push_str(
            &self
                .sysroot
                .as_deref()
                .map(driver_fingerprint)
                .unwrap_or_else(|| "unknown".into()),
        );
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

    /// Whether the compiler accepts `-Z` flags: a nightly or locally built
    /// one (or any, with `RUSTC_BOOTSTRAP=1` set by the user).
    pub fn accepts_unstable_flags(&self) -> bool {
        let release = self
            .verbose_version
            .lines()
            .find_map(|l| l.strip_prefix("release: "))
            .unwrap_or("");
        release.contains("nightly")
            || release.contains("-dev")
            || std::env::var("RUSTC_BOOTSTRAP").is_ok_and(|v| v == "1")
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
        chain.push(&self.exec);
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

/// Size and mtime of the compiler's implementation (`librustc_driver`).
fn driver_fingerprint(sysroot: &Path) -> String {
    let mut parts = Vec::new();
    if let Ok(entries) = std::fs::read_dir(sysroot.join("lib")) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("librustc_driver")
                && let Ok(m) = e.metadata()
            {
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
    parts.sort();
    parts.join(",")
}

/// The first line of `program --version`, remembered by the program's path,
/// size and mtime (`$RUNE_HOME/cache/tools.json`): the same binary gives the
/// same answer, and starting a C compiler just to ask costs more than a whole
/// no-op build.
pub fn tool_version(program: &Path) -> String {
    let fingerprint = program_fingerprint(program);
    let path = crate::settings::rune_home()
        .join("cache")
        .join("tools.json");
    let load = || -> HashMap<String, String> {
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    };
    if !fingerprint.ends_with("|missing")
        && let Some(v) = load().get(&fingerprint)
    {
        return v.clone();
    }
    let version = Command::new(program)
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
    if !fingerprint.ends_with("|missing") {
        let _guard = PROBE_LOCK.lock().unwrap();
        let mut all = load();
        all.insert(fingerprint, version.clone());
        while all.len() > 256 {
            let Some(k) = all.keys().next().cloned() else {
                break;
            };
            all.remove(&k);
        }
        if let (Some(dir), Ok(text)) = (path.parent(), serde_json::to_string(&all)) {
            let _ = std::fs::create_dir_all(dir);
            let _ = crate::fsutil::write_atomic(&path, text.as_bytes());
        }
    }
    version
}

/// `rustc` or `rustc.exe`.
fn exe_name(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

/// Where compiler probes are kept.
fn probe_cache_path() -> PathBuf {
    crate::settings::rune_home()
        .join("cache")
        .join("toolchains.json")
}

/// What asking a compiler about itself found.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct Probe {
    /// Which entry this is (see [`ProbeCache::resolution_key`]).
    #[serde(skip)]
    key: String,
    /// `rustc -vV`.
    verbose_version: String,
    /// `rustc --print sysroot`.
    sysroot: String,
    /// Sizes and mtimes of the sysroot's `rustc` and `librustc_driver`: the
    /// entry is only believed while these are unchanged.
    files: String,
    /// `rustc --print=cfg` output by target, specification and flags.
    #[serde(default)]
    cfg: std::collections::BTreeMap<String, String>,
}

/// Compiler probes (`rustc -vV`, `--print sysroot`, `--print=cfg`) kept across
/// runs. Each one starts the compiler — through a `rustup` proxy about
/// 15–20 ms — and every Rune command needs several, which used to be most of
/// a no-op build's time. The answers change only when the compiler does, so
/// an entry is keyed by what decides which compiler `rustc` is (its path,
/// size and mtime; rustup's settings, `RUSTUP_TOOLCHAIN` and the working
/// directory, which select a proxy's toolchain) and trusted only while the
/// toolchain's own files are unchanged — `rustup update` misses and probes
/// again.
struct ProbeCache {
    path: PathBuf,
}

static PROBE_LOCK: Mutex<()> = Mutex::new(());

impl ProbeCache {
    fn open(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
        }
    }

    fn load(&self) -> HashMap<String, Probe> {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn save(&self, entries: &HashMap<String, Probe>) {
        if let (Some(dir), Ok(text)) = (self.path.parent(), serde_json::to_string(entries)) {
            let _ = std::fs::create_dir_all(dir);
            let _ = crate::fsutil::write_atomic(&self.path, text.as_bytes());
        }
    }

    /// What selects the compiler a `rustc` invocation runs.
    fn resolution_key(rustc: &Path) -> String {
        let mut hb = crate::hash::HashBuilder::new();
        hb.update(rustc.to_string_lossy().as_bytes());
        hb.update(b"\0");
        hb.update(program_fingerprint(rustc).as_bytes());
        for var in ["RUSTUP_TOOLCHAIN", "RUSTUP_HOME", "RUSTC_BOOTSTRAP"] {
            hb.update(b"\0");
            hb.update(var.as_bytes());
            hb.update(b"=");
            if let Some(v) = std::env::var_os(var) {
                hb.update(v.to_string_lossy().as_bytes());
            }
        }
        let rustup_home = std::env::var_os("RUSTUP_HOME")
            .map(PathBuf::from)
            .or_else(|| home::home_dir().map(|h| h.join(".rustup")));
        let cwd = std::env::current_dir().ok();
        if let Some(settings) = rustup_home.map(|h| h.join("settings.toml"))
            && let Ok(text) = std::fs::read_to_string(&settings)
        {
            hb.update(b"\0settings=");
            hb.update(text.as_bytes());
            // A `rustup override` for this directory or one above it.
            if let (Some(cwd), Ok(parsed)) = (&cwd, text.parse::<toml::Table>())
                && let Some(overrides) = parsed.get("overrides").and_then(|o| o.as_table())
                && let Some((dir, toolchain)) = overrides
                    .iter()
                    .filter(|(dir, _)| cwd.starts_with(dir.as_str()))
                    .max_by_key(|(dir, _)| dir.len())
            {
                hb.update(b"\0override=");
                hb.update(dir.as_bytes());
                hb.update(b"=");
                hb.update(toolchain.to_string().as_bytes());
            }
        }
        // Which toolchain a proxy picks in this directory: an override (above)
        // or the nearest `rust-toolchain(.toml)`. Not the directory itself, so
        // every project and checkout without either shares the probe.
        if let Some(cwd) = &cwd {
            for dir in cwd.ancestors() {
                let file = ["rust-toolchain.toml", "rust-toolchain"]
                    .iter()
                    .map(|n| dir.join(n))
                    .find(|f| f.is_file());
                if let Some(file) = file {
                    hb.update(b"\0toolchain-file=");
                    hb.update(file.to_string_lossy().as_bytes());
                    hb.update(b"\0");
                    hb.update(&std::fs::read(&file).unwrap_or_default());
                    break;
                }
            }
        }
        hb.finish_blake3()
    }

    /// Fingerprint of the files that make up the compiler in `sysroot`.
    fn sysroot_files(sysroot: &Path) -> String {
        let rustc = sysroot.join("bin").join(exe_name("rustc"));
        format!(
            "{}|{}",
            program_fingerprint(&rustc),
            driver_fingerprint(sysroot)
        )
    }

    /// The probe of `rustc`: remembered, or asked of the compiler (the two
    /// questions in parallel).
    fn probe(&self, rustc: &Path) -> Probe {
        let key = Self::resolution_key(rustc);
        if let Some(mut hit) = self.load().remove(&key)
            && !hit.sysroot.is_empty()
            && Self::sysroot_files(Path::new(&hit.sysroot)) == hit.files
        {
            hit.key = key;
            return hit;
        }
        let ask = |args: &[&str]| -> String {
            Command::new(rustc)
                .args(args)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default()
        };
        // The host's cfg set too: nearly every command needs it next, and
        // asking now, beside the other two, saves a compiler start in a row.
        let (verbose_version, sysroot, host_cfg) = std::thread::scope(|s| {
            let sysroot = s.spawn(|| ask(&["--print", "sysroot"]));
            let cfg = s.spawn(|| ask(&["--print=cfg"]));
            let vv = ask(&["-vV"]);
            (
                vv,
                sysroot.join().unwrap_or_default(),
                cfg.join().unwrap_or_default(),
            )
        });
        let mut probe = Probe {
            key: key.clone(),
            files: if sysroot.is_empty() {
                String::new()
            } else {
                Self::sysroot_files(Path::new(&sysroot))
            },
            verbose_version,
            sysroot,
            cfg: Default::default(),
        };
        // Only a complete answer is worth remembering.
        if !probe.sysroot.is_empty() && !probe.verbose_version.is_empty() {
            let _guard = PROBE_LOCK.lock().unwrap();
            let mut all = self.load();
            if let Some(old) = all.get(&key)
                && old.files == probe.files
            {
                probe.cfg = old.cfg.clone();
            }
            // Filed as `TargetInfo::query` asks for the host without flags.
            if let Some(host) = probe
                .verbose_version
                .lines()
                .find_map(|l| l.strip_prefix("host: "))
                && !host_cfg.is_empty()
            {
                probe
                    .cfg
                    .entry(format!("{}||", host.trim()))
                    .or_insert(host_cfg);
            }
            all.insert(key, probe.clone());
            // Bounded: projects come and go, toolchains get updated.
            while all.len() > 64 {
                let Some(k) = all.keys().next().cloned() else {
                    break;
                };
                all.remove(&k);
            }
            self.save(&all);
        }
        probe
    }

    fn cfg(&self, key: &str, cfg_key: &str) -> Option<String> {
        if key.is_empty() {
            return None;
        }
        self.load().get(key)?.cfg.get(cfg_key).cloned()
    }

    fn remember_cfg(&self, key: &str, cfg_key: &str, text: &str) {
        if key.is_empty() {
            return;
        }
        let _guard = PROBE_LOCK.lock().unwrap();
        let mut all = self.load();
        if let Some(entry) = all.get_mut(key) {
            entry.cfg.insert(cfg_key.to_string(), text.to_string());
            self.save(&all);
        }
    }
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
        let key = format!("{}|{arg}|{}", tc.exec.display(), rustflags.join("\x1f"));
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
            let mut cmd = Command::new(&tc.exec);
            cmd.arg("--print=cfg").arg("--target").arg(&arg).args(flags);
            let out = cmd.output().ok()?;
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
        };
        // The answer depends on the compiler, the target (a custom one by its
        // content) and the flags; the probe cache remembers it across runs.
        let probe_cfg_key = format!(
            "{arg}|{}|{}",
            spec_hash.as_deref().unwrap_or(""),
            rustflags.join("\x1f")
        );
        let probes = ProbeCache::open(&probe_cache_path());
        let text = match probes.cfg(&tc.probe_key, &probe_cfg_key) {
            Some(text) => text,
            None => {
                let text = run(rustflags).or_else(|| run(&[])).unwrap_or_default();
                if !text.is_empty() {
                    probes.remember_cfg(&tc.probe_key, &probe_cfg_key, &text);
                }
                text
            }
        };
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
        if !table_key.starts_with("cfg(")
            && let Ok(v) = std::env::var(CargoConfig::env_name(&["target", table_key, key]))
        {
            return Some(v);
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
        assert_eq!(args[1], tc.exec.to_string_lossy());
        // Dependencies (not workspace members) skip the workspace wrapper.
        let cmd = tc.rustc_command(false);
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, [tc.exec.to_string_lossy().into_owned()]);
    }

    /// A fake `rustc` that counts how often it is started.
    #[cfg(unix)]
    fn fake_rustc(dir: &Path) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let sysroot = dir.join("sysroot");
        std::fs::create_dir_all(sysroot.join("bin")).unwrap();
        std::fs::create_dir_all(sysroot.join("lib")).unwrap();
        std::fs::write(sysroot.join("lib/librustc_driver-abc.so"), "v1").unwrap();
        let count = dir.join("count");
        let rustc = sysroot.join("bin/rustc");
        std::fs::write(
            &rustc,
            format!(
                "#!/bin/sh\necho x >> {count}\ncase \"$1\" in\n\
                 -vV) printf 'rustc 1.2.3\\nhost: x86_64-unknown-linux-gnu\\nrelease: 1.2.3\\n' ;;\n\
                 --print) echo {sysroot} ;;\n\
                 --print=cfg) echo unix ;;\n\
                 esac\n",
                count = count.display(),
                sysroot = sysroot.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&rustc, std::fs::Permissions::from_mode(0o755)).unwrap();
        (rustc, count)
    }

    #[cfg(unix)]
    #[test]
    fn compiler_probes_are_remembered_until_the_toolchain_changes() {
        let dir = tempfile::tempdir().unwrap();
        let (rustc, count) = fake_rustc(dir.path());
        let cache = ProbeCache::open(&dir.path().join("probes.json"));
        let starts = || {
            std::fs::read_to_string(&count)
                .map(|t| t.lines().count())
                .unwrap_or(0)
        };
        let first = cache.probe(&rustc);
        assert_eq!(first.verbose_version.lines().last(), Some("release: 1.2.3"));
        assert!(first.sysroot.ends_with("sysroot"));
        assert_eq!(starts(), 3, "-vV, --print sysroot and the host's cfg");
        assert_eq!(
            cache
                .cfg(&first.key, "x86_64-unknown-linux-gnu||")
                .as_deref(),
            Some("unix"),
            "the host's cfg is remembered with the probe"
        );
        let again = cache.probe(&rustc);
        assert_eq!(again.verbose_version, first.verbose_version);
        assert_eq!(starts(), 3, "a second probe must not start the compiler");
        // The toolchain is updated in place (`rustup update`): probe again.
        // (The new library's size differs, so no waiting for the clock.)
        std::fs::write(
            dir.path().join("sysroot/lib/librustc_driver-abc.so"),
            "v2 is longer",
        )
        .unwrap();
        cache.probe(&rustc);
        assert_eq!(starts(), 6);
    }

    #[test]
    fn rustc_from_path_is_started_from_its_sysroot() {
        let tc = global();
        if std::env::var_os("RUSTC").is_some() {
            return;
        }
        let sysroot = tc.sysroot.as_ref().expect("the sysroot is known");
        assert_eq!(tc.exec, sysroot.join("bin").join(exe_name("rustc")));
        assert!(tc.exec.is_file());
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
