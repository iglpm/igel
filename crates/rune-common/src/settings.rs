//! User-facing settings: the `rune config` system.
//!
//! Settings are layered, from lowest to highest precedence:
//!
//! 1. built-in defaults,
//! 2. the **global** file, `~/.rune/config.toml` (or `$RUNE_HOME/config.toml`),
//! 3. the **project** file, `Rune.toml` next to the project's `Cargo.toml`,
//! 4. `RUNE_*` environment variables (e.g. `RUNE_BUILD_RELEASE_REBUILD_ALL=1`),
//! 5. command-line flags (applied by the CLI on top of what this module returns).
//!
//! `Rune.toml` is a Rune-only file: Cargo ignores it, so setting a value there
//! can never break a plain `cargo build` of the same project.
//!
//! Every key is declared once in [`KEYS`]. That table drives validation,
//! `rune config list`, and the typed accessors on [`Settings`], so a key can't
//! be documented but unsupported, or supported but undocumented.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item, Table, Value as TomlValue};

/// The name of the project-level settings file.
pub const PROJECT_FILE: &str = "Rune.toml";

/// What kind of value a setting holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    /// `true` / `false` (also accepts `on`/`off`, `yes`/`no`, `1`/`0` on the CLI).
    Bool,
    /// A non-negative integer.
    Uint,
    /// A filesystem path. Relative paths in `Rune.toml` are relative to the project.
    Path,
    /// A byte size such as `20GB`, `512MiB`, or a plain number of bytes.
    Size,
}

impl ValueKind {
    fn describe(self) -> &'static str {
        match self {
            ValueKind::Bool => "a boolean (true/false)",
            ValueKind::Uint => "a non-negative integer",
            ValueKind::Path => "a path",
            ValueKind::Size => "a size such as `20GB` or `512MiB`",
        }
    }
}

/// A typed setting value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Bool(bool),
    Uint(u64),
    Path(String),
    Size(u64),
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Bool(b) => write!(f, "{b}"),
            Value::Uint(n) => write!(f, "{n}"),
            Value::Path(p) => write!(f, "{p}"),
            Value::Size(n) => f.write_str(&format_size(*n)),
        }
    }
}

/// Static description of one setting.
#[derive(Debug, Clone, Copy)]
pub struct KeyDef {
    /// Dotted key, e.g. `build.release-rebuild-all`.
    pub name: &'static str,
    pub kind: ValueKind,
    /// Human-readable default, shown by `rune config list`.
    pub default: &'static str,
    pub description: &'static str,
}

/// Every setting Rune understands.
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
        name: "build.jobs",
        kind: ValueKind::Uint,
        default: "CPUs + one third",
        description: "Default number of parallel compiler jobs (`-j`).",
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
        name: "cas.max-size",
        kind: ValueKind::Size,
        default: "unlimited",
        description: "Evict the least recently stored entries after a build when the store grows \
                      beyond this size.",
    },
    KeyDef {
        name: "net.offline",
        kind: ValueKind::Bool,
        default: "false",
        description: "Never touch the network; resolve and build from local caches only.",
    },
    KeyDef {
        name: "test.doctests",
        kind: ValueKind::Bool,
        default: "true",
        description: "Run documentation tests as part of `rune test`.",
    },
];

/// Looks up the definition of a key.
pub fn key_def(name: &str) -> Option<&'static KeyDef> {
    KEYS.iter().find(|k| k.name == name)
}

/// Error from reading, validating, or writing settings.
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
    // Cheap "did you mean": a known key that ends in the same leaf name.
    let leaf = key.rsplit('.').next().unwrap_or(key);
    match KEYS.iter().find(|k| k.name.ends_with(leaf)) {
        Some(k) => format!(" (did you mean `{}`?)", k.name),
        None => String::new(),
    }
}

/// Where a value came from.
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

/// Which file a `set`/`unset` should touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Global,
    Project,
}

/// Fully layered settings, ready to query.
#[derive(Debug, Clone, Default)]
pub struct Settings {
    /// Key → (value, where it came from). Only keys that were explicitly set.
    values: BTreeMap<String, (Value, Origin)>,
    project_root: Option<PathBuf>,
}

impl Settings {
    /// Loads global, project, and environment layers.
    ///
    /// A malformed *global* file is reported as an error rather than ignored:
    /// silently falling back to defaults would make e.g. an intended
    /// `release-rebuild-all = true` quietly not apply.
    pub fn load(project_root: Option<&Path>) -> Result<Self, SettingsError> {
        Self::load_from(project_root, &global_config_path(), &|k| {
            std::env::var(k).ok()
        })
    }

    /// Like [`Self::load`] with an explicit global file and an injectable
    /// environment, so tests never depend on the developer's real config.
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
                // A project-relative path is stored resolved, so callers never
                // need to know which file it came from.
                (Value::Path(p), Some(base)) if Path::new(&p).is_relative() => {
                    Value::Path(base.join(p).to_string_lossy().into_owned())
                }
                (v, _) => v,
            };
            self.values
                .insert(key.name.to_string(), (value, origin(path)));
        }
        Ok(())
    }

    /// The explicitly set value and its origin, if any.
    pub fn get(&self, key: &str) -> Option<&(Value, Origin)> {
        self.values.get(key)
    }

    /// Overrides a value from a higher-precedence source such as a CLI flag.
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

    /// `build.release-rebuild-all`: rebuild every crate for release builds
    /// instead of using the content-addressable store.
    pub fn release_rebuild_all(&self) -> bool {
        self.bool("build.release-rebuild-all", false)
    }

    /// `build.jobs`.
    pub fn jobs(&self) -> Option<usize> {
        self.uint("build.jobs")
            .filter(|n| *n > 0)
            .map(|n| n as usize)
    }

    /// `build.target-dir`, resolved against the project root.
    pub fn target_dir(&self) -> Option<PathBuf> {
        self.path("build.target-dir")
    }

    /// `cas.enabled`.
    pub fn cas_enabled(&self) -> bool {
        self.bool("cas.enabled", true)
    }

    /// `cas.dir`.
    pub fn cas_dir(&self) -> Option<PathBuf> {
        self.path("cas.dir")
    }

    /// `cas.max-size` in bytes.
    pub fn cas_max_size(&self) -> Option<u64> {
        self.uint("cas.max-size")
    }

    /// `net.offline`.
    pub fn offline(&self) -> bool {
        self.bool("net.offline", false)
    }

    /// `test.doctests`.
    pub fn doctests(&self) -> bool {
        self.bool("test.doctests", true)
    }

    /// The project root these settings were loaded for.
    pub fn project_root(&self) -> Option<&Path> {
        self.project_root.as_deref()
    }

    /// One row per known key: `(definition, effective value, origin)`.
    pub fn effective(&self) -> Vec<(&'static KeyDef, String, Origin)> {
        KEYS.iter()
            .map(|def| match self.values.get(def.name) {
                Some((v, o)) => (def, v.to_string(), o.clone()),
                None => (def, def.default.to_string(), Origin::Default),
            })
            .collect()
    }
}

/// Path of the global settings file.
pub fn global_config_path() -> PathBuf {
    rune_home().join("config.toml")
}

/// `$RUNE_HOME`, or `~/.rune`.
pub fn rune_home() -> PathBuf {
    if let Some(dir) = std::env::var_os("RUNE_HOME") {
        return PathBuf::from(dir);
    }
    home::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".rune")
}

/// Path of the settings file for a scope.
pub fn config_path(scope: Scope, project_root: Option<&Path>) -> Option<PathBuf> {
    match scope {
        Scope::Global => Some(global_config_path()),
        Scope::Project => project_root.map(|r| r.join(PROJECT_FILE)),
    }
}

/// Reads the value of `key` in one specific file (not layered).
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

/// Validates `raw` for `key` and writes it into `path`, creating the file if
/// needed and preserving everything else in it (comments, ordering).
pub fn set_in_file(path: &Path, key: &str, raw: &str) -> Result<Value, SettingsError> {
    let def = key_def(key).ok_or_else(|| SettingsError::UnknownKey { key: key.into() })?;
    let value = parse_value(def, raw)?;

    let mut doc = load_doc(path)?;
    let (table_name, leaf) = split_key(key);
    if !doc.contains_key(table_name) {
        doc[table_name] = Item::Table(Table::new());
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

/// Removes `key` from `path`. Returns whether it was present.
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
        // Don't leave behind an empty `[build]` header.
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
    // Write-then-rename so a crash can't leave a truncated settings file.
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
        ValueKind::Size => match value {
            TomlValue::Integer(n) => u64::try_from(*n.value())
                .map(Value::Size)
                .map_err(|_| expected),
            TomlValue::String(s) => parse_size(s.value()).map(Value::Size).ok_or(expected),
            _ => Err(expected),
        },
    }
}

fn to_toml(value: &Value) -> TomlValue {
    match value {
        Value::Bool(b) => TomlValue::from(*b),
        Value::Uint(n) => TomlValue::from(*n as i64),
        Value::Path(p) => TomlValue::from(p.as_str()),
        // Sizes are stored as the friendly string the user typed, normalized.
        Value::Size(n) => TomlValue::from(format_size(*n)),
    }
}

/// Parses user-typed text for a key, enforcing its kind.
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
        ValueKind::Size => parse_size(raw).map(Value::Size).ok_or_else(bad),
    }
}

/// `1024`, `10GB`, `512 MiB`, `1.5G` → bytes. Decimal (`GB`) and binary
/// (`GiB`) suffixes are both accepted; a bare `G` means binary, matching how
/// disk usage is usually reported on Linux.
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

/// Formats bytes back to the most natural exact unit (`20GiB`, `1500MB`, ...).
pub fn format_size(bytes: u64) -> String {
    const UNITS: [(&str, u64); 4] = [
        ("TiB", 1 << 40),
        ("GiB", 1 << 30),
        ("MiB", 1 << 20),
        ("KiB", 1 << 10),
    ];
    for (name, size) in UNITS {
        if bytes >= size && bytes % size == 0 {
            return format!("{}{name}", bytes / size);
        }
    }
    bytes.to_string()
}

/// `build.release-rebuild-all` → `RUNE_BUILD_RELEASE_REBUILD_ALL`.
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

fn expand_home(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = home::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    /// A global path that never exists, keeping tests hermetic.
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

        // Global alone.
        let s = Settings::load_from(None, &global, &no_env).unwrap();
        assert!(!s.release_rebuild_all());
        assert_eq!(s.jobs(), Some(3));

        // Project beats global for the key it sets, global still supplies the rest.
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
