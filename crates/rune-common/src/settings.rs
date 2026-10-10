//! `rune config` settings, lowest to highest precedence: defaults, `~/.rune/config.toml`, `Rune.toml`,
//! `RUNE_*` env vars, CLI flags. Every key is declared once in [`KEYS`].

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item, Table, Value as TomlValue};

pub const PROJECT_FILE: &str = "Rune.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    Bool,
    Uint,
    Path,
    Size,
    Text,
    Threads,
    Duration,
}

impl ValueKind {
    fn describe(self) -> &'static str {
        match self {
            ValueKind::Bool => "a boolean (true/false)",
            ValueKind::Uint => "a non-negative integer",
            ValueKind::Path => "a path",
            ValueKind::Text => "text",
            ValueKind::Size => "a size such as `20GB` or `512MiB`, or `auto`",
            ValueKind::Threads => "`off`, `auto` or a number of threads",
            ValueKind::Duration => "a duration such as `14d`, `72h` or `2w`",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FrontendThreads {
    #[default]
    Off,
    Auto,
    Fixed(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Bool(bool),
    Uint(u64),
    Path(String),
    Size(u64),
    Text(String),
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Bool(b) => write!(f, "{b}"),
            Value::Uint(n) => write!(f, "{n}"),
            Value::Path(p) => write!(f, "{p}"),
            Value::Size(n) => f.write_str(&format_size(*n)),
            Value::Text(t) => write!(f, "{t}"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct KeyDef {
    pub name: &'static str,
    pub kind: ValueKind,
    pub default: &'static str,
    pub description: &'static str,
}

pub const KEYS: &[KeyDef] = &[
    KeyDef {
        name: "build.release-rebuild-all",
        kind: ValueKind::Bool,
        default: "false",
        description: "Rebuild every crate (dependencies included) for release builds instead of \
                      restoring them from the content-addressable store. Off by default: release \
                      builds reuse cached artifacts just like dev builds do.",
    },
    KeyDef {
        name: "build.feature-unification",
        kind: ValueKind::Text,
        default: "selected",
        description: "Which packages' feature requests a build honors. `selected` (the \
                      default): only the packages being built, so `-p a` compiles its \
                      dependencies with just the features `a` needs. `workspace`: every \
                      member's, so building any subset of the workspace reuses the same \
                      dependency builds (what `cargo hakari` does, without a generated crate). \
                      Cargo's `resolver.feature-unification` config is honored too.",
    },
    KeyDef {
        name: "build.sandbox",
        kind: ValueKind::Bool,
        default: "false",
        description: "Run build scripts and the compiler (which runs proc-macros) confined: no \
                      network, and no writes outside the build directory (a build script: its \
                      OUT_DIR) and the temporary directory. Linux: user namespaces; macOS: \
                      sandbox-exec.",
    },
    KeyDef {
        name: "build.sandbox-network",
        kind: ValueKind::Text,
        default: "",
        description: "Packages whose build scripts may use the network in the sandbox \
                      (comma-separated names), for those that download what they build.",
    },
    KeyDef {
        name: "build.low-priority",
        kind: ValueKind::Bool,
        default: "false",
        description: "Run compilers and build scripts at low scheduling priority, so the rest of \
                      the machine (editor, browser, a call) stays responsive during a big build; \
                      the build takes the cores nothing else wants. `rune watch` always does.",
    },
    KeyDef {
        name: "build.dependency-debuginfo",
        kind: ValueKind::Bool,
        default: "false",
        description: "Compile registry and git dependencies with full debuginfo in the dev profile. \
                      Off by default: dependencies get `line-tables-only` (backtraces and \
                      breakpoints keep working, local-variable inspection inside dependency code \
                      does not), which makes cold builds about 20% faster. A `debug` key in a \
                      profile always wins, and your own crates are never affected.",
    },
    KeyDef {
        name: "build.jobs",
        kind: ValueKind::Uint,
        default: "CPUs + one third",
        description: "Default number of parallel compiler jobs (`-j`).",
    },
    KeyDef {
        name: "build.frontend-threads",
        kind: ValueKind::Threads,
        default: "off",
        description: "Threads for rustc's parallel front end (`-Z threads`), which only nightly \
                      compilers accept; ignored on stable. `auto`: a crate that starts while \
                      cores are idle — typically the last, largest crates of a build, which \
                      otherwise type-check on one core — gets as many threads as there are idle \
                      cores (at most 8); crates started while the machine is busy get one. A \
                      number: that many for every crate. Whether it pays depends on the crate: \
                      on Rune's own build (4 cores) it kept the cores busy but saved no time.",
    },
    KeyDef {
        name: "build.linker",
        kind: ValueKind::Text,
        default: "hyprlnk",
        description: "Which linker links binaries, tests and dynamic libraries. `hyprlnk`: the \
                      linker built into Rune, with nothing to install. It takes precedence over \
                      `target.<triple>.linker`; on Linux that names the C compiler hyprlnk \
                      links through (for the C runtime), `cc` otherwise. hyprlnk links Linux \
                      (glibc and musl on x86-64, x86, AArch64, ARM, RISC-V 64, POWER \
                      little-endian, s390x and LoongArch), Windows (`*-pc-windows-msvc`: x64, \
                      ARM64, x86), PDBs included, and macOS (`*-apple-darwin`: arm64, arm64e, \
                      x86_64), debug maps and code signatures included; other targets keep \
                      their linker. `default`: the linker `rustc` and `.cargo/config.toml` \
                      choose.",
    },
    KeyDef {
        name: "build.target-dir",
        kind: ValueKind::Path,
        default: ".rune/target",
        description: "Directory for build output. Relative paths are relative to the project root.",
    },
    KeyDef {
        name: "cas.enabled",
        kind: ValueKind::Bool,
        default: "true",
        description: "Use the content-addressable store to reuse compiled artifacts.",
    },
    KeyDef {
        name: "cas.dir",
        kind: ValueKind::Path,
        default: "~/.rune/cas",
        description: "Location of the content-addressable store.",
    },
    KeyDef {
        name: "cas.remote",
        kind: ValueKind::Path,
        default: "none",
        description: "A store shared with other machines, consulted before compiling and \
                      published to after: a directory (network share, CI cache volume) or the \
                      URL of a Cairn server (`http://host:7070`). Several URLs separated by \
                      commas shard entries across the servers. Share only between machines you \
                      would run each other's binaries on.",
    },
    KeyDef {
        name: "cas.remote-token",
        kind: ValueKind::Text,
        default: "none",
        description: "Bearer token for a Cairn server. Prefer the RUNE_CAS_REMOTE_TOKEN \
                      environment variable: anything in Rune.toml is readable by everyone who can \
                      read the project.",
    },
    KeyDef {
        name: "cas.remote-replicas",
        kind: ValueKind::Uint,
        default: "1",
        description: "With several Cairn URLs in `cas.remote`: how many servers hold each entry \
                      (2 keeps everything available when one server is down).",
    },
    KeyDef {
        name: "cas.remote-write",
        kind: ValueKind::Bool,
        default: "true",
        description: "Publish new entries to `cas.remote`. Set to false on machines that should \
                      only read the shared store (e.g. developers, when only CI writes).",
    },
    KeyDef {
        name: "cas.max-size",
        kind: ValueKind::Size,
        default: "auto",
        description: "Evict the least recently used entries after a build when the store grows \
                      beyond this size. `auto` (the default): a tenth of the disk it is on, at \
                      least 5 GiB and at most 50 GiB; `0`: unlimited.",
    },
    KeyDef {
        name: "cas.compress-after",
        kind: ValueKind::Duration,
        default: "7d",
        description: "Compress (zstd) store entries no build has used for this long; the next \
                      build that needs one decompresses it. `0`: never compress.",
    },
    KeyDef {
        name: "cas.max-age",
        kind: ValueKind::Duration,
        default: "30d",
        description: "Remove store entries no build has used for this long (`30d`, `2w`; `0` \
                      keeps everything). Checked at most once a day, in the background after a \
                      build, like Cargo's automatic cleanup of its caches.",
    },
    KeyDef {
        name: "cas.verify",
        kind: ValueKind::Bool,
        default: "false",
        description: "Re-hash every artifact restored from the content-addressable store \
                      (catches bit rot and tampering; costs a read of every restored file). \
                      Entries are always checked against their key and recorded sizes.",
    },
    KeyDef {
        name: "resolve.min-release-age",
        kind: ValueKind::Duration,
        default: "0",
        description: "Only choose releases published at least this long ago (`14d`, `72h`, `2w`) \
                      when resolving new versions; locked versions are kept. A malicious release \
                      is usually noticed and yanked within days. When nothing that old fits a \
                      requirement, the newest release that does is used, with a warning.",
    },
    KeyDef {
        name: "net.offline",
        kind: ValueKind::Bool,
        default: "false",
        description: "Never touch the network; resolve and build from local caches only.",
    },
    KeyDef {
        name: "resolve.write-cargo-lock",
        kind: ValueKind::Bool,
        default: "true",
        description: "Create and update `Cargo.lock` when the set of locked packages changes, as \
                      Cargo does, so the project keeps working with plain `cargo`. A lockfile \
                      that is already up to date is never touched.",
    },
    KeyDef {
        name: "test.cache",
        kind: ValueKind::Bool,
        default: "false",
        description: "Skip test binaries that passed before with an identical binary, package \
                      files, arguments and RUST_/CARGO_/RUNE_ environment (`rune test --cached`). \
                      Only enable it if your tests depend on nothing else: no network, clock, or \
                      files outside the package.",
    },
    KeyDef {
        name: "audit.ignore",
        kind: ValueKind::Text,
        default: "",
        description: "Advisories `rune audit` accepts (comma-separated IDs, e.g. \
                      `RUSTSEC-2023-0071`): reviewed, and not relevant to this project.",
    },
    KeyDef {
        name: "test.jobs",
        kind: ValueKind::Uint,
        default: "1",
        description: "How many test binaries `rune test` runs at once. 1 (the default) is \
                      Cargo's behaviour and safe for tests that share ports, files or databases \
                      across binaries; more finishes sooner when they don't. Each binary still \
                      runs its own tests in parallel, and building continues meanwhile. With \
                      `test.schedule = tests` the default is one per core.",
    },
    KeyDef {
        name: "test.timeout",
        kind: ValueKind::Duration,
        default: "0",
        description: "Stop a test binary (or doc-test run) that runs longer than this (`90s`, \
                      `10m`) and report it as failed, so a hung test cannot hang the run or a CI \
                      job. `0` (the default, as with Cargo): no limit.",
    },
    KeyDef {
        name: "test.schedule",
        kind: ValueKind::Text,
        default: "binaries",
        description: "How `rune test` spreads tests over the cores. `binaries`: each test \
                      binary runs as one process, as with Cargo. `tests`: each binary's tests \
                      are split into small batches, each its own process, scheduled across \
                      every binary at once (like cargo-nextest) — faster when a few binaries \
                      hold most tests, but tests no longer share a process.",
    },
    KeyDef {
        name: "test.doctests",
        kind: ValueKind::Bool,
        default: "true",
        description: "Run documentation tests as part of `rune test`.",
    },
];

pub fn key_def(name: &str) -> Option<&'static KeyDef> {
    KEYS.iter().find(|k| k.name == name)
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("unknown setting `{key}`{}", suggestion(.key))]
    UnknownKey { key: String },
    #[error("invalid value `{value}` for `{key}`: expected {expected}")]
    InvalidValue {
        key: String,
        value: String,
        expected: &'static str,
    },
    #[error("{path}: {message}")]
    File { path: PathBuf, message: String },
}

fn suggestion(key: &str) -> String {
    let leaf = key.rsplit('.').next().unwrap_or(key);
    match KEYS.iter().find(|k| k.name.ends_with(leaf)) {
        Some(k) => format!(" (did you mean `{}`?)", k.name),
        None => String::new(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    Default,
    Global(PathBuf),
    Project(PathBuf),
    Env(String),
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Origin::Default => f.write_str("default"),
            Origin::Global(p) => write!(f, "global ({})", p.display()),
            Origin::Project(p) => write!(f, "project ({})", p.display()),
            Origin::Env(v) => write!(f, "environment (${v})"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Global,
    Project,
}

#[derive(Debug, Clone, Default)]
pub struct Settings {
    values: BTreeMap<String, (Value, Origin)>,
    project_root: Option<PathBuf>,
}

impl Settings {
    /// A malformed global file is an error: silently using defaults would hide intended settings.
    pub fn load(project_root: Option<&Path>) -> Result<Self, SettingsError> {
        Self::load_from(project_root, &global_config_path(), &|k| {
            std::env::var(k).ok()
        })
    }

    pub fn load_from(
        project_root: Option<&Path>,
        global: &Path,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, SettingsError> {
        let mut settings = Settings {
            values: BTreeMap::new(),
            project_root: project_root.map(Path::to_path_buf),
        };

        settings.merge_file(global, |p| Origin::Global(p.to_path_buf()), None)?;
        if let Some(root) = project_root {
            settings.merge_file(
                &root.join(PROJECT_FILE),
                |p| Origin::Project(p.to_path_buf()),
                Some(root),
            )?;
        }

        for key in KEYS {
            let var = env_var_name(key.name);
            if let Some(raw) = env(&var) {
                if raw.is_empty() {
                    continue;
                }
                let value = parse_value(key, &raw)?;
                settings
                    .values
                    .insert(key.name.to_string(), (value, Origin::Env(var)));
            }
        }
        Ok(settings)
    }

    fn merge_file(
        &mut self,
        path: &Path,
        origin: impl Fn(&Path) -> Origin,
        path_base: Option<&Path>,
    ) -> Result<(), SettingsError> {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Ok(()); // absent file: nothing to merge
        };
        let doc: DocumentMut =
            text.parse()
                .map_err(|e: toml_edit::TomlError| SettingsError::File {
                    path: path.to_path_buf(),
                    message: e.to_string(),
                })?;
        for key in KEYS {
            let Some(item) = lookup(&doc, key.name) else {
                continue;
            };
            let value = from_toml(key, item).map_err(|expected| SettingsError::File {
                path: path.to_path_buf(),
                message: format!("`{}` must be {expected}", key.name),
            })?;
            let value = match (value, path_base) {
                (Value::Path(p), Some(base)) if Path::new(&p).is_relative() && !is_url_list(&p) => {
                    Value::Path(base.join(p).to_string_lossy().into_owned())
                }
                (v, _) => v,
            };
            self.values
                .insert(key.name.to_string(), (value, origin(path)));
        }
        Ok(())
    }

    pub fn get(&self, key: &str) -> Option<&(Value, Origin)> {
        self.values.get(key)
    }

    pub fn override_value(&mut self, key: &str, value: Value, origin: Origin) {
        self.values.insert(key.to_string(), (value, origin));
    }

    fn bool(&self, key: &str, default: bool) -> bool {
        match self.values.get(key) {
            Some((Value::Bool(b), _)) => *b,
            _ => default,
        }
    }

    fn uint(&self, key: &str) -> Option<u64> {
        match self.values.get(key) {
            Some((Value::Uint(n), _)) | Some((Value::Size(n), _)) => Some(*n),
            _ => None,
        }
    }

    fn path(&self, key: &str) -> Option<PathBuf> {
        match self.values.get(key) {
            Some((Value::Path(p), _)) => Some(expand_home(p)),
            _ => None,
        }
    }

    pub fn cas_remote(&self) -> Option<String> {
        self.path("cas.remote")
            .map(|p| p.to_string_lossy().into_owned())
    }

    pub fn cas_remote_token(&self) -> Option<String> {
        match self.values.get("cas.remote-token") {
            Some((Value::Text(t), _)) => Some(t.clone()),
            _ => None,
        }
    }

    pub fn cas_remote_replicas(&self) -> usize {
        self.uint("cas.remote-replicas")
            .map_or(1, |n| n.max(1) as usize)
    }

    pub fn cas_remote_write(&self) -> bool {
        self.bool("cas.remote-write", true)
    }

    pub fn test_cache(&self) -> bool {
        self.bool("test.cache", false)
    }

    pub fn dependency_debuginfo(&self) -> bool {
        self.bool("build.dependency-debuginfo", false)
    }

    /// `build.feature-unification`, when set: `selected` or `workspace`.
    pub fn feature_unification(&self) -> Option<String> {
        match self.values.get("build.feature-unification") {
            Some((Value::Text(t), _)) => Some(t.clone()),
            _ => None,
        }
    }

    pub fn sandbox(&self) -> bool {
        self.bool("build.sandbox", false)
    }

    pub fn sandbox_network(&self) -> Vec<String> {
        match self.values.get("build.sandbox-network") {
            Some((Value::Text(t), _)) => t
                .split([',', ' '])
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        }
    }

    pub fn low_priority(&self) -> bool {
        self.bool("build.low-priority", false)
    }

    pub fn release_rebuild_all(&self) -> bool {
        self.bool("build.release-rebuild-all", false)
    }

    pub fn min_release_age(&self) -> Option<std::time::Duration> {
        match self.values.get("resolve.min-release-age") {
            Some((Value::Text(t), _)) => crate::time::parse_duration(t)
                .filter(|s| *s > 0)
                .map(std::time::Duration::from_secs),
            _ => None,
        }
    }

    pub fn frontend_threads(&self) -> FrontendThreads {
        match self.values.get("build.frontend-threads") {
            Some((Value::Text(t), _)) if t == "auto" => FrontendThreads::Auto,
            Some((Value::Text(t), _)) => t
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 1)
                .map_or(FrontendThreads::Off, FrontendThreads::Fixed),
            _ => FrontendThreads::Off,
        }
    }

    /// `build.linker`, set or default.
    pub fn linker(&self) -> String {
        match self.values.get("build.linker") {
            Some((Value::Text(t), _)) => t.clone(),
            _ => "hyprlnk".into(),
        }
    }

    pub fn jobs(&self) -> Option<usize> {
        self.uint("build.jobs")
            .filter(|n| *n > 0)
            .map(|n| n as usize)
    }

    pub fn target_dir(&self) -> Option<PathBuf> {
        self.path("build.target-dir")
    }

    pub fn cas_enabled(&self) -> bool {
        self.bool("cas.enabled", true)
    }

    pub fn cas_dir(&self) -> Option<PathBuf> {
        self.path("cas.dir")
    }

    pub fn cas_max_size(&self) -> Option<u64> {
        self.uint("cas.max-size")
    }

    /// When unset: a tenth of the disk, between 5 and 50 GiB. `None`: unlimited.
    pub fn cas_max_size_for(&self, store: &std::path::Path) -> Option<u64> {
        match self.cas_max_size() {
            Some(0) => None,
            Some(n) => Some(n),
            None => {
                let disk = crate::sysinfo::disk_size(store)?;
                Some((disk / 10).clamp(5 << 30, 50 << 30))
            }
        }
    }

    pub fn cas_max_age(&self) -> Option<u64> {
        match self.values.get("cas.max-age") {
            Some((Value::Text(t), _)) => crate::time::parse_duration(t).filter(|s| *s > 0),
            _ => Some(30 * 86_400),
        }
    }

    pub fn cas_compress_after(&self) -> Option<u64> {
        match self.values.get("cas.compress-after") {
            Some((Value::Text(t), _)) => crate::time::parse_duration(t).filter(|s| *s > 0),
            _ => Some(7 * 86_400),
        }
    }

    pub fn audit_ignore(&self) -> Vec<String> {
        match self.values.get("audit.ignore") {
            Some((Value::Text(t), _)) => t
                .split([',', ' '])
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        }
    }

    pub fn test_jobs(&self) -> Option<usize> {
        self.uint("test.jobs")
            .map(|n| n as usize)
            .filter(|n| *n > 0)
    }

    pub fn test_timeout(&self) -> Option<u64> {
        match self.values.get("test.timeout") {
            Some((Value::Text(t), _)) => crate::time::parse_duration(t).filter(|s| *s > 0),
            _ => None,
        }
    }

    pub fn test_schedule(&self) -> Option<String> {
        match self.values.get("test.schedule") {
            Some((Value::Text(t), _)) => Some(t.clone()),
            _ => None,
        }
    }

    pub fn cas_verify(&self) -> bool {
        self.bool("cas.verify", false)
    }

    pub fn offline(&self) -> bool {
        self.bool("net.offline", false)
    }

    pub fn write_cargo_lock(&self) -> bool {
        self.bool("resolve.write-cargo-lock", true)
    }

    pub fn doctests(&self) -> bool {
        self.bool("test.doctests", true)
    }

    pub fn project_root(&self) -> Option<&Path> {
        self.project_root.as_deref()
    }

    pub fn effective(&self) -> Vec<(&'static KeyDef, String, Origin)> {
        KEYS.iter()
            .map(|def| match self.values.get(def.name) {
                Some((v, o)) => (def, v.to_string(), o.clone()),
                None => (def, def.default.to_string(), Origin::Default),
            })
            .collect()
    }
}

pub fn global_config_path() -> PathBuf {
    rune_home().join("config.toml")
}

pub fn rune_home() -> PathBuf {
    if let Some(dir) = std::env::var_os("RUNE_HOME") {
        return PathBuf::from(dir);
    }
    home::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".rune")
}

pub fn config_path(scope: Scope, project_root: Option<&Path>) -> Option<PathBuf> {
    match scope {
        Scope::Global => Some(global_config_path()),
        Scope::Project => project_root.map(|r| r.join(PROJECT_FILE)),
    }
}

pub fn read_from_file(path: &Path, key: &str) -> Result<Option<Value>, SettingsError> {
    let def = key_def(key).ok_or_else(|| SettingsError::UnknownKey { key: key.into() })?;
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(None);
    };
    let doc: DocumentMut = text
        .parse()
        .map_err(|e: toml_edit::TomlError| SettingsError::File {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;
    match lookup(&doc, key) {
        None => Ok(None),
        Some(item) => from_toml(def, item)
            .map(Some)
            .map_err(|expected| SettingsError::File {
                path: path.to_path_buf(),
                message: format!("`{key}` must be {expected}"),
            }),
    }
}

pub fn set_in_file(path: &Path, key: &str, raw: &str) -> Result<Value, SettingsError> {
    let def = key_def(key).ok_or_else(|| SettingsError::UnknownKey { key: key.into() })?;
    let value = parse_value(def, raw)?;

    let mut doc = load_doc(path)?;
    let (table_name, leaf) = split_key(key);
    if !doc.contains_key(table_name) {
        doc[table_name] = Item::Table(Table::new());
    }
    if doc[table_name].is_array_of_tables() {
        // `[[test]]` targets own the name `test` in a manifest `Rune.toml`.
        return Err(SettingsError::File {
            path: path.to_path_buf(),
            message: format!(
                "`{table_name}` is a list of targets (`[[{table_name}]]`) in this manifest, so the \
                 `{key}` setting cannot be stored here; use `--global`, or the `{}` environment variable",
                env_var_name(key)
            ),
        });
    }
    let table = doc[table_name]
        .as_table_like_mut()
        .ok_or_else(|| SettingsError::File {
            path: path.to_path_buf(),
            message: format!("`{table_name}` must be a table"),
        })?;
    table.insert(leaf, Item::Value(to_toml(&value)));

    write_doc(path, &doc)?;
    Ok(value)
}

pub fn unset_in_file(path: &Path, key: &str) -> Result<bool, SettingsError> {
    key_def(key).ok_or_else(|| SettingsError::UnknownKey { key: key.into() })?;
    if !path.exists() {
        return Ok(false);
    }
    let mut doc = load_doc(path)?;
    let (table_name, leaf) = split_key(key);
    let removed = doc
        .get_mut(table_name)
        .and_then(|t| t.as_table_like_mut())
        .is_some_and(|t| t.remove(leaf).is_some());
    if removed {
        if doc
            .get(table_name)
            .and_then(|t| t.as_table_like())
            .is_some_and(|t| t.is_empty())
        {
            doc.remove(table_name);
        }
        write_doc(path, &doc)?;
    }
    Ok(removed)
}

fn load_doc(path: &Path) -> Result<DocumentMut, SettingsError> {
    match std::fs::read_to_string(path) {
        Ok(text) => text
            .parse()
            .map_err(|e: toml_edit::TomlError| SettingsError::File {
                path: path.to_path_buf(),
                message: e.to_string(),
            }),
        Err(_) => Ok(DocumentMut::new()),
    }
}

fn write_doc(path: &Path, doc: &DocumentMut) -> Result<(), SettingsError> {
    let io_err = |e: std::io::Error| SettingsError::File {
        path: path.to_path_buf(),
        message: e.to_string(),
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io_err)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, doc.to_string()).map_err(io_err)?;
    std::fs::rename(&tmp, path).map_err(io_err)
}

fn split_key(key: &str) -> (&str, &str) {
    key.split_once('.').unwrap_or(("", key))
}

fn lookup<'a>(doc: &'a DocumentMut, key: &str) -> Option<&'a Item> {
    let (table, leaf) = split_key(key);
    doc.get(table)?.as_table_like()?.get(leaf)
}

fn from_toml(def: &KeyDef, item: &Item) -> Result<Value, &'static str> {
    let expected = def.kind.describe();
    let value = item.as_value().ok_or(expected)?;
    match def.kind {
        ValueKind::Bool => value.as_bool().map(Value::Bool).ok_or(expected),
        ValueKind::Uint => value
            .as_integer()
            .and_then(|n| u64::try_from(n).ok())
            .map(Value::Uint)
            .ok_or(expected),
        ValueKind::Path => value
            .as_str()
            .map(|s| Value::Path(s.to_string()))
            .ok_or(expected),
        ValueKind::Text => value
            .as_str()
            .map(|s| Value::Text(s.to_string()))
            .ok_or(expected),
        ValueKind::Size => match value {
            TomlValue::Integer(n) => u64::try_from(*n.value())
                .map(Value::Size)
                .map_err(|_| expected),
            TomlValue::String(s) if s.value() == "auto" => Ok(Value::Text("auto".into())),
            TomlValue::String(s) => parse_size(s.value()).map(Value::Size).ok_or(expected),
            _ => Err(expected),
        },
        ValueKind::Threads => match value {
            TomlValue::Integer(n) => parse_threads(&n.value().to_string()).ok_or(expected),
            TomlValue::String(s) => parse_threads(s.value()).ok_or(expected),
            _ => Err(expected),
        },
        ValueKind::Duration => match value {
            TomlValue::Integer(n) => u64::try_from(*n.value())
                .map(|n| Value::Text(n.to_string()))
                .map_err(|_| expected),
            TomlValue::String(s) => crate::time::parse_duration(s.value())
                .map(|_| Value::Text(s.value().trim().to_string()))
                .ok_or(expected),
            _ => Err(expected),
        },
    }
}

fn parse_threads(raw: &str) -> Option<Value> {
    let raw = raw.trim().to_ascii_lowercase();
    match raw.as_str() {
        "off" | "false" | "no" | "0" | "1" => Some(Value::Text("off".into())),
        "auto" | "on" | "true" | "yes" => Some(Value::Text("auto".into())),
        n => n
            .parse::<u64>()
            .ok()
            .filter(|n| (2..=256).contains(n))
            .map(|n| Value::Text(n.to_string())),
    }
}

fn to_toml(value: &Value) -> TomlValue {
    match value {
        Value::Bool(b) => TomlValue::from(*b),
        Value::Uint(n) => TomlValue::from(*n as i64),
        Value::Path(p) => TomlValue::from(p.as_str()),
        Value::Text(t) => TomlValue::from(t.as_str()),
        Value::Size(n) => TomlValue::from(format_size(*n)),
    }
}

pub fn parse_value(def: &KeyDef, raw: &str) -> Result<Value, SettingsError> {
    let bad = || SettingsError::InvalidValue {
        key: def.name.to_string(),
        value: raw.to_string(),
        expected: def.kind.describe(),
    };
    let raw = raw.trim();
    match def.kind {
        ValueKind::Bool => match raw.to_ascii_lowercase().as_str() {
            "true" | "on" | "yes" | "1" | "enable" | "enabled" => Ok(Value::Bool(true)),
            "false" | "off" | "no" | "0" | "disable" | "disabled" => Ok(Value::Bool(false)),
            _ => Err(bad()),
        },
        ValueKind::Uint => raw.parse::<u64>().map(Value::Uint).map_err(|_| bad()),
        ValueKind::Path if !raw.is_empty() => Ok(Value::Path(raw.to_string())),
        ValueKind::Path => Err(bad()),
        ValueKind::Text if !raw.is_empty() => Ok(Value::Text(raw.to_string())),
        ValueKind::Text => Err(bad()),
        ValueKind::Size if raw == "auto" => Ok(Value::Text("auto".into())),
        ValueKind::Size => parse_size(raw).map(Value::Size).ok_or_else(bad),
        ValueKind::Threads => parse_threads(raw).ok_or_else(bad),
        ValueKind::Duration => crate::time::parse_duration(raw)
            .map(|_| Value::Text(raw.to_string()))
            .ok_or_else(bad),
    }
}

pub fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (num, unit) = text.split_at(split);
    let num: f64 = num.parse().ok()?;
    if num < 0.0 {
        return None;
    }
    let mult: f64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kib" => 1024.0,
        "m" | "mib" => 1024.0 * 1024.0,
        "g" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tib" => 1024.0f64.powi(4),
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        "tb" => 1e12,
        _ => return None,
    };
    Some((num * mult) as u64)
}

pub fn format_size(bytes: u64) -> String {
    const UNITS: [(&str, u64); 4] = [
        ("TiB", 1 << 40),
        ("GiB", 1 << 30),
        ("MiB", 1 << 20),
        ("KiB", 1 << 10),
    ];
    for (name, size) in UNITS {
        if bytes >= size && bytes.is_multiple_of(size) {
            return format!("{}{name}", bytes / size);
        }
    }
    bytes.to_string()
}

pub fn env_var_name(key: &str) -> String {
    format!(
        "RUNE_{}",
        key.chars()
            .map(|c| if c == '.' || c == '-' {
                '_'
            } else {
                c.to_ascii_uppercase()
            })
            .collect::<String>()
    )
}

pub fn is_url_list(s: &str) -> bool {
    let s = s.trim_start();
    s.starts_with("http://") || s.starts_with("https://")
}

fn expand_home(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/")
        && let Some(home) = home::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn settings_share_a_rune_toml_with_the_manifest_except_where_test_targets_take_the_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Rune.toml");
        std::fs::write(
            &path,
            "[package]\nname = \"a\"\n\n[dependencies]\nserde = \"1\"\n",
        )
        .unwrap();
        set_in_file(&path, "build.jobs", "4").unwrap();
        set_in_file(&path, "test.doctests", "false").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("name = \"a\"") && text.contains("serde") && text.contains("jobs = 4"),
            "{text}"
        );
        let s = Settings::load_from(Some(dir.path()), &dir.path().join("none"), &|_| None).unwrap();
        assert_eq!(s.jobs(), Some(4));
        assert!(!s.doctests());

        let with_targets = dir.path().join("t").join("Rune.toml");
        std::fs::create_dir_all(with_targets.parent().unwrap()).unwrap();
        std::fs::write(
            &with_targets,
            "[package]\nname = \"a\"\n\n[[test]]\nname = \"it\"\npath = \"tests/it.rs\"\n",
        )
        .unwrap();
        let err = set_in_file(&with_targets, "test.doctests", "false")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("list of targets") && err.contains("RUNE_TEST_DOCTESTS"),
            "{err}"
        );
        set_in_file(&with_targets, "build.jobs", "2").unwrap();
        let s = Settings::load_from(
            Some(with_targets.parent().unwrap()),
            &dir.path().join("none"),
            &|_| None,
        )
        .unwrap();
        assert_eq!(s.jobs(), Some(2));
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn no_global() -> PathBuf {
        PathBuf::from("/nonexistent/rune/config.toml")
    }

    #[test]
    fn defaults_are_conservative() {
        let s = Settings::default();
        assert!(!s.release_rebuild_all(), "release rebuild must be opt-in");
        assert!(s.cas_enabled());
        assert!(!s.offline());
        assert!(s.doctests());
        assert_eq!(s.jobs(), None);
    }

    #[test]
    fn set_get_unset_roundtrip_preserves_other_content() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("Rune.toml");
        std::fs::write(&path, "# keep me\n[build]\njobs = 4 # inline\n").unwrap();

        set_in_file(&path, "build.release-rebuild-all", "on").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# keep me"), "{text}");
        assert!(text.contains("jobs = 4 # inline"), "{text}");
        assert!(text.contains("release-rebuild-all = true"), "{text}");
        assert_eq!(
            read_from_file(&path, "build.release-rebuild-all").unwrap(),
            Some(Value::Bool(true))
        );

        assert!(unset_in_file(&path, "build.release-rebuild-all").unwrap());
        assert!(!unset_in_file(&path, "build.release-rebuild-all").unwrap());
        assert_eq!(
            read_from_file(&path, "build.release-rebuild-all").unwrap(),
            None
        );
    }

    #[test]
    fn unset_removes_empty_table_header() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("Rune.toml");
        set_in_file(&path, "net.offline", "true").unwrap();
        unset_in_file(&path, "net.offline").unwrap();
        assert!(!std::fs::read_to_string(&path).unwrap().contains("[net]"));
    }

    #[test]
    fn rejects_bad_values_and_unknown_keys() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("Rune.toml");
        assert!(matches!(
            set_in_file(&path, "build.release-rebuild-all", "maybe"),
            Err(SettingsError::InvalidValue { .. })
        ));
        assert!(matches!(
            set_in_file(&path, "build.nope", "1"),
            Err(SettingsError::UnknownKey { .. })
        ));
        assert!(!path.exists(), "a rejected set must not create the file");
    }

    #[test]
    fn project_overrides_global_and_env_overrides_project() {
        let dir = tempdir().unwrap();
        let global = dir.path().join("global.toml");
        let proj = dir.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(&global, "[build]\nrelease-rebuild-all = false\njobs = 3\n").unwrap();
        std::fs::write(
            proj.join("Rune.toml"),
            "[build]\nrelease-rebuild-all = true\n",
        )
        .unwrap();

        let s = Settings::load_from(None, &global, &no_env).unwrap();
        assert!(!s.release_rebuild_all());
        assert_eq!(s.jobs(), Some(3));

        let s = Settings::load_from(Some(&proj), &global, &no_env).unwrap();
        assert!(s.release_rebuild_all());
        assert_eq!(s.jobs(), Some(3));
        assert!(matches!(
            s.get("build.release-rebuild-all"),
            Some((_, Origin::Project(_)))
        ));

        let env = |k: &str| (k == "RUNE_BUILD_RELEASE_REBUILD_ALL").then(|| "0".to_string());
        let s = Settings::load_from(Some(&proj), &global, &env).unwrap();
        assert!(!s.release_rebuild_all());
        assert!(matches!(
            s.get("build.release-rebuild-all"),
            Some((_, Origin::Env(_)))
        ));
    }

    #[test]
    fn relative_project_paths_are_resolved_against_the_project() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("Rune.toml"),
            "[build]\ntarget-dir = \"out\"\n",
        )
        .unwrap();
        let s = Settings::load_from(Some(dir.path()), &no_global(), &no_env).unwrap();
        assert_eq!(s.target_dir(), Some(dir.path().join("out")));
    }

    #[test]
    fn malformed_project_file_is_an_error_not_a_silent_default() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("Rune.toml"), "[build\n").unwrap();
        assert!(Settings::load_from(Some(dir.path()), &no_global(), &no_env).is_err());
    }

    #[test]
    fn wrong_type_in_file_is_reported_with_key_name() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("Rune.toml"),
            "[build]\nrelease-rebuild-all = \"yes please\"\n",
        )
        .unwrap();
        let err = Settings::load_from(Some(dir.path()), &no_global(), &no_env).unwrap_err();
        assert!(
            err.to_string().contains("build.release-rebuild-all"),
            "{err}"
        );
    }

    #[test]
    fn frontend_threads_accept_off_auto_or_a_count() {
        let def = KEYS
            .iter()
            .find(|k| k.name == "build.frontend-threads")
            .unwrap();
        assert_eq!(
            parse_value(def, "auto").unwrap(),
            Value::Text("auto".into())
        );
        assert_eq!(parse_value(def, "off").unwrap(), Value::Text("off".into()));
        assert_eq!(parse_value(def, "1").unwrap(), Value::Text("off".into()));
        assert_eq!(parse_value(def, "8").unwrap(), Value::Text("8".into()));
        assert!(parse_value(def, "lots").is_err());
    }

    #[test]
    fn release_ages_must_be_durations() {
        let def = KEYS
            .iter()
            .find(|k| k.name == "resolve.min-release-age")
            .unwrap();
        assert!(parse_value(def, "14d").is_ok());
        assert!(parse_value(def, "0").is_ok());
        assert!(parse_value(def, "14 days please").is_err());
    }

    #[test]
    fn size_parsing() {
        assert_eq!(parse_size("1024"), Some(1024));
        assert_eq!(parse_size("10GB"), Some(10_000_000_000));
        assert_eq!(parse_size("2GiB"), Some(2 << 30));
        assert_eq!(parse_size("1.5G"), Some(3 << 29));
        assert_eq!(parse_size("512 mib"), Some(512 << 20));
        assert_eq!(parse_size("ten"), None);
        assert_eq!(parse_size("5 parsecs"), None);
        assert_eq!(format_size(2 << 30), "2GiB");
        assert_eq!(format_size(1500), "1500");
    }

    #[test]
    fn env_var_names() {
        assert_eq!(
            env_var_name("build.release-rebuild-all"),
            "RUNE_BUILD_RELEASE_REBUILD_ALL"
        );
        assert_eq!(env_var_name("cas.max-size"), "RUNE_CAS_MAX_SIZE");
    }

    #[test]
    fn every_key_has_a_valid_definition() {
        for key in KEYS {
            assert!(key.name.contains('.'), "{} must be `table.key`", key.name);
            assert!(!key.description.is_empty());
        }
        let mut names: Vec<_> = KEYS.iter().map(|k| k.name).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), KEYS.len(), "duplicate key");
    }
}
