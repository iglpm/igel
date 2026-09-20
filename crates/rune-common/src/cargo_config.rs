//! Hierarchical `.cargo/config.toml` support.
//!
//! A project's build can be steered from configuration files as well as the
//! manifest: `build.rustc`, `build.rustc-wrapper`, `build.rustflags`,
//! `[target.<triple>] linker/runner/rustflags`, `[env]`, `[source.*]`,
//! `[registries.*]`, `[patch]`, credentials. Rune reads the same files Cargo
//! does, with Cargo's rules:
//!
//! * `.cargo/config.toml` (or the legacy `.cargo/config`) in the working
//!   directory and every parent, then `$CARGO_HOME/config.toml`;
//! * closer files win for scalars, tables merge, and arrays are joined with the
//!   lower-precedence entries first;
//! * `CARGO_*` environment variables override the files;
//! * relative paths in path-like keys are relative to the directory *containing*
//!   `.cargo/`, unless they are bare program names.

use std::path::{Path, PathBuf};
use toml::{Table, Value};

/// The merged view of every config file that applies.
#[derive(Debug, Clone, Default)]
pub struct CargoConfig {
    table: Table,
    /// The files that contributed, closest first.
    pub files: Vec<PathBuf>,
}

/// One `[env]` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvEntry {
    pub name: String,
    pub value: String,
    /// Overwrite a variable that is already set.
    pub force: bool,
}

impl CargoConfig {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Loads every config file that applies to a build started in `start`.
    pub fn load(start: &Path) -> Self {
        let mut cfg = Self::default();
        let mut dir = Some(start);
        while let Some(d) = dir {
            for name in ["config.toml", "config"] {
                let path = d.join(".cargo").join(name);
                if path.is_file() {
                    cfg.add_file(&path, d);
                    break;
                }
            }
            dir = d.parent();
        }
        let cargo_home = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| home::home_dir().map(|h| h.join(".cargo")));
        if let Some(home) = cargo_home {
            for name in ["config.toml", "config"] {
                let path = home.join(name);
                if path.is_file() && !cfg.files.contains(&path) {
                    let base = home.parent().unwrap_or(&home).to_path_buf();
                    cfg.add_file(&path, &base);
                    break;
                }
            }
        }
        cfg
    }

    /// Parses config text as if it lived in `<base>/.cargo/config.toml`.
    /// Later calls are lower precedence than earlier ones.
    pub fn add_str(&mut self, text: &str, base: &Path) -> Result<(), toml::de::Error> {
        let mut table: Table = toml::from_str(text)?;
        absolutize(&mut table, base);
        merge(&mut self.table, table);
        Ok(())
    }

    fn add_file(&mut self, path: &Path, base: &Path) {
        // An unreadable or malformed config file must not break builds that
        // don't need it; Cargo would error, but Rune only warns via `files`.
        if let Ok(text) = std::fs::read_to_string(path) {
            if self.add_str(&text, base).is_ok() {
                self.files.push(path.to_path_buf());
            }
        }
    }

    /// Looks up a value by path, e.g. `["build", "rustc"]`, ignoring the environment.
    pub fn file_value(&self, path: &[&str]) -> Option<&Value> {
        let mut cur: &Table = &self.table;
        for (i, seg) in path.iter().enumerate() {
            let v = cur.get(*seg)?;
            if i + 1 == path.len() {
                return Some(v);
            }
            cur = v.as_table()?;
        }
        None
    }

    /// The `CARGO_*` variable that overrides `path`.
    pub fn env_name(path: &[&str]) -> String {
        let mut s = String::from("CARGO");
        for seg in path {
            s.push('_');
            s.extend(seg.chars().map(|c| {
                if c.is_alphanumeric() {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                }
            }));
        }
        s
    }

    /// A string setting, environment first.
    pub fn string(&self, path: &[&str]) -> Option<String> {
        if let Ok(v) = std::env::var(Self::env_name(path)) {
            return Some(v);
        }
        self.file_value(path)
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    /// A boolean setting, environment first.
    pub fn bool(&self, path: &[&str]) -> Option<bool> {
        if let Ok(v) = std::env::var(Self::env_name(path)) {
            return match v.as_str() {
                "true" | "1" | "yes" | "on" => Some(true),
                "false" | "0" | "no" | "off" => Some(false),
                _ => None,
            };
        }
        self.file_value(path).and_then(Value::as_bool)
    }

    /// A list of strings (`["a", "b"]`, or one string split on whitespace),
    /// environment first (whitespace-separated).
    pub fn string_list(&self, path: &[&str]) -> Option<Vec<String>> {
        if let Ok(v) = std::env::var(Self::env_name(path)) {
            return Some(v.split_whitespace().map(str::to_string).collect());
        }
        match self.file_value(path)? {
            Value::Array(a) => Some(
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect(),
            ),
            Value::String(s) => Some(s.split_whitespace().map(str::to_string).collect()),
            _ => None,
        }
    }

    /// A sub-table (`[registries]`, `[source]`, ...).
    pub fn table(&self, path: &[&str]) -> Option<&Table> {
        self.file_value(path).and_then(Value::as_table)
    }

    /// The whole merged document.
    pub fn root(&self) -> &Table {
        &self.table
    }

    /// `[target.<triple>]` and `[target.'cfg(..)']` tables that apply to a
    /// target: the exact-triple table first, then every matching `cfg` table
    /// in key order (Cargo rejects conflicting `linker` values among them).
    pub fn target_tables<'a>(
        &'a self,
        triple: &str,
        matches_cfg: &dyn Fn(&str) -> bool,
    ) -> Vec<(&'a str, &'a Table)> {
        let Some(targets) = self.table(&["target"]) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if let Some((k, t)) = targets
            .get_key_value(triple)
            .and_then(|(k, v)| v.as_table().map(|t| (k.as_str(), t)))
        {
            out.push((k, t));
        }
        let mut cfgs: Vec<_> = targets
            .iter()
            .filter(|(k, _)| k.starts_with("cfg("))
            .collect();
        cfgs.sort_by(|a, b| a.0.cmp(b.0));
        for (k, v) in cfgs {
            if let (Some(t), true) = (v.as_table(), matches_cfg(k)) {
                out.push((k.as_str(), t));
            }
        }
        out
    }

    /// The `[env]` table.
    pub fn env_entries(&self) -> Vec<EnvEntry> {
        let Some(env) = self.table(&["env"]) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (name, v) in env {
            match v {
                Value::String(s) => out.push(EnvEntry {
                    name: name.clone(),
                    value: s.clone(),
                    force: false,
                }),
                Value::Table(t) => {
                    if let Some(value) = t.get("value").and_then(Value::as_str) {
                        out.push(EnvEntry {
                            name: name.clone(),
                            value: value.to_string(),
                            force: t.get("force").and_then(Value::as_bool).unwrap_or(false),
                        });
                    }
                }
                _ => {}
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }
}

/// Merges `lower` (lower precedence) into `higher`: scalars keep the higher
/// value, tables merge recursively, arrays are joined lower-first.
fn merge(higher: &mut Table, lower: Table) {
    for (key, low) in lower {
        match higher.get_mut(&key) {
            None => {
                higher.insert(key, low);
            }
            Some(Value::Table(h)) => {
                if let Value::Table(l) = low {
                    merge(h, l);
                }
            }
            Some(Value::Array(h)) => {
                if let Value::Array(mut l) = low {
                    l.append(h);
                    *h = l;
                }
            }
            Some(_) => {}
        }
    }
}

fn is_relative_path(p: &str) -> bool {
    !Path::new(p).is_absolute() && p.contains('/')
}

fn abs(p: &str, base: &Path) -> String {
    if is_relative_path(p) {
        base.join(p).to_string_lossy().into_owned()
    } else {
        p.to_string()
    }
}

/// Rewrites config-relative paths (see the module docs) to absolute ones, so a
/// merged table no longer needs to remember which file a value came from.
fn absolutize(root: &mut Table, base: &Path) {
    // build.<key>
    if let Some(Value::Table(build)) = root.get_mut("build") {
        for key in [
            "rustc",
            "rustc-wrapper",
            "rustc-workspace-wrapper",
            "rustdoc",
        ] {
            if let Some(Value::String(s)) = build.get_mut(key) {
                *s = abs(s, base);
            }
        }
    }
    // target.<x>.linker / runner
    if let Some(Value::Table(targets)) = root.get_mut("target") {
        for (_, t) in targets.iter_mut() {
            let Value::Table(t) = t else { continue };
            if let Some(Value::String(s)) = t.get_mut("linker") {
                *s = abs(s, base);
            }
            match t.get_mut("runner") {
                Some(Value::String(s)) => {
                    let mut parts = s.splitn(2, char::is_whitespace);
                    let program = abs(parts.next().unwrap_or(""), base);
                    *s = match parts.next() {
                        Some(rest) => format!("{program} {rest}"),
                        None => program,
                    };
                }
                Some(Value::Array(a)) => {
                    if let Some(Value::String(s)) = a.first_mut() {
                        *s = abs(s, base);
                    }
                }
                _ => {}
            }
        }
    }
    // source.<x>.directory / local-registry
    if let Some(Value::Table(sources)) = root.get_mut("source") {
        for (_, t) in sources.iter_mut() {
            let Value::Table(t) = t else { continue };
            for key in ["directory", "local-registry"] {
                if let Some(Value::String(s)) = t.get_mut(key) {
                    // Always config-relative, slash or not.
                    if !Path::new(s.as_str()).is_absolute() {
                        *s = base.join(&*s).to_string_lossy().into_owned();
                    }
                }
            }
        }
    }
    // patch.<source>.<name>.path
    if let Some(Value::Table(patches)) = root.get_mut("patch") {
        for (_, srcs) in patches.iter_mut() {
            let Value::Table(srcs) = srcs else { continue };
            for (_, dep) in srcs.iter_mut() {
                if let Value::Table(dep) = dep {
                    if let Some(Value::String(s)) = dep.get_mut("path") {
                        if !Path::new(s.as_str()).is_absolute() {
                            *s = base.join(&*s).to_string_lossy().into_owned();
                        }
                    }
                }
            }
        }
    }
    // paths = [..]
    if let Some(Value::Array(paths)) = root.get_mut("paths") {
        for p in paths.iter_mut() {
            if let Value::String(s) = p {
                if !Path::new(s.as_str()).is_absolute() {
                    *s = base.join(&*s).to_string_lossy().into_owned();
                }
            }
        }
    }
    // [env] entries with `relative = true`
    if let Some(Value::Table(env)) = root.get_mut("env") {
        for (_, v) in env.iter_mut() {
            if let Value::Table(t) = v {
                if t.get("relative").and_then(Value::as_bool) == Some(true) {
                    if let Some(Value::String(s)) = t.get_mut("value") {
                        *s = base.join(&*s).to_string_lossy().into_owned();
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(files: &[(&str, &str)]) -> CargoConfig {
        // Closest first, like `load`.
        let mut c = CargoConfig::empty();
        for (base, text) in files {
            c.add_str(text, Path::new(base)).unwrap();
        }
        c
    }

    #[test]
    fn closer_files_win_scalars_and_arrays_join_lower_first() {
        let c = cfg(&[
            (
                "/proj",
                "[build]\nrustc = \"near\"\nrustflags = [\"-Cnear\"]\n",
            ),
            (
                "/",
                "[build]\nrustc = \"far\"\nrustflags = [\"-Cfar\"]\njobs = 3\n",
            ),
        ]);
        assert_eq!(
            c.file_value(&["build", "rustc"]).unwrap().as_str(),
            Some("near")
        );
        assert_eq!(
            c.string_list(&["build", "rustflags"]).unwrap(),
            ["-Cfar", "-Cnear"]
        );
        assert_eq!(
            c.file_value(&["build", "jobs"]).unwrap().as_integer(),
            Some(3)
        );
    }

    #[test]
    fn relative_paths_are_relative_to_the_dir_holding_dot_cargo() {
        let c = cfg(&[(
            "/work/proj",
            r#"
            [build]
            rustc = "tools/rustc"
            rustc-wrapper = "sccache"
            [target.aarch64-unknown-linux-gnu]
            linker = "../cross/gcc"
            runner = "run/qemu -L /sysroot"
            [source.vendored]
            directory = "vendor"
            "#,
        )]);
        assert_eq!(
            c.string(&["build", "rustc"]).unwrap(),
            "/work/proj/tools/rustc"
        );
        assert_eq!(
            c.string(&["build", "rustc-wrapper"]).unwrap(),
            "sccache",
            "bare program names are looked up on PATH"
        );
        assert_eq!(
            c.string(&["target", "aarch64-unknown-linux-gnu", "linker"])
                .unwrap(),
            "/work/proj/../cross/gcc"
        );
        assert_eq!(
            c.string(&["target", "aarch64-unknown-linux-gnu", "runner"])
                .unwrap(),
            "/work/proj/run/qemu -L /sysroot"
        );
        assert_eq!(
            c.string(&["source", "vendored", "directory"]).unwrap(),
            "/work/proj/vendor"
        );
    }

    #[test]
    fn target_tables_match_triple_then_cfgs() {
        let c = cfg(&[(
            "/p",
            r#"
            [target.x86_64-unknown-linux-gnu]
            linker = "a"
            [target.'cfg(unix)']
            rustflags = ["-Cunix"]
            [target.'cfg(windows)']
            rustflags = ["-Cwin"]
            "#,
        )]);
        let tables = c.target_tables("x86_64-unknown-linux-gnu", &|k| k == "cfg(unix)");
        let names: Vec<_> = tables.iter().map(|(k, _)| *k).collect();
        assert_eq!(names, ["x86_64-unknown-linux-gnu", "cfg(unix)"]);
    }

    #[test]
    fn env_table_supports_force_and_relative() {
        let c = cfg(&[(
            "/p",
            r#"
            [env]
            A = "1"
            B = { value = "2", force = true }
            C = { value = "sub", relative = true }
            "#,
        )]);
        let env = c.env_entries();
        assert_eq!(
            env[0],
            EnvEntry {
                name: "A".into(),
                value: "1".into(),
                force: false
            }
        );
        assert!(env[1].force);
        assert_eq!(env[2].value, "/p/sub");
    }

    #[test]
    fn env_name_mangling() {
        assert_eq!(
            CargoConfig::env_name(&["target", "aarch64-unknown-linux-gnu", "linker"]),
            "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER"
        );
        assert_eq!(
            CargoConfig::env_name(&["build", "rustc-wrapper"]),
            "CARGO_BUILD_RUSTC_WRAPPER"
        );
    }
}
