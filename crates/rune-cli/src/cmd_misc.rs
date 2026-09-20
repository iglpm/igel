//! `new`, `init`, `clean`, `fmt`, `config`, `cas`, `lockfile`.

use anyhow::{Context, Result, bail};
use colored::Colorize;
use rune_cas::CasStore;
use rune_cas::gc::{GcConfig, run_gc};
use rune_common::settings::{
    self, KEYS, Origin, Scope, Settings, config_path, format_size, key_def, parse_size,
};
use rune_common::{Edition, RuneConfig};
use rune_resolver::lockfile::LockfileManager;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::args::*;
use crate::ui;
use crate::workspace::{Workspace, find_project_root};

// ---------------------------------------------------------------------------
// new / init
// ---------------------------------------------------------------------------

const RUST_KEYWORDS: &[&str] = &[
    "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn", "for",
    "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return",
    "self", "Self", "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where",
    "while", "async", "await", "dyn", "abstract", "become", "box", "do", "final", "macro",
    "override", "priv", "typeof", "unsized", "virtual", "yield", "try", "gen",
];

/// Validates a package name the way Cargo does.
pub fn validate_package_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("package name cannot be empty");
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !(first.is_alphabetic() || first == '_') {
        bail!(
            "invalid character `{first}` in package name: `{name}`, the first character must be a Unicode XID start character or `_`"
        );
    }
    if let Some(bad) = chars.find(|c| !(c.is_alphanumeric() || *c == '_' || *c == '-')) {
        bail!(
            "invalid character `{bad}` in package name: `{name}`, characters must be Unicode XID characters, `-`, or `_`"
        );
    }
    if RUST_KEYWORDS.contains(&name.replace('-', "_").as_str()) {
        bail!("the name `{name}` cannot be used as a package name, it is a Rust keyword");
    }
    Ok(())
}

fn scaffold(dir: &Path, name: &str, lib: bool, edition: &str, gitignore: bool) -> Result<()> {
    validate_package_name(name)?;
    if dir.join("Cargo.toml").exists() {
        bail!("`Cargo.toml` already exists in `{}`", dir.display());
    }
    let _: Edition = edition.parse().map_err(|e: String| anyhow::anyhow!(e))?;
    fs::create_dir_all(dir.join("src"))?;

    fs::write(
        dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"{edition}\"\n\n[dependencies]\n"
        ),
    )?;

    let source = if lib {
        dir.join("src/lib.rs")
    } else {
        dir.join("src/main.rs")
    };
    if !source.exists() {
        fs::write(
            &source,
            if lib {
                "pub fn add(left: u64, right: u64) -> u64 {\n    left + right\n}\n\n\
                 #[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn it_works() {\n        \
                 assert_eq!(add(2, 2), 4);\n    }\n}\n"
            } else {
                "fn main() {\n    println!(\"Hello, world!\");\n}\n"
            },
        )?;
    }

    if gitignore {
        let path = dir.join(".gitignore");
        if !path.exists() {
            fs::write(path, "/target\n/.rune\nRune.lock.bin\n")?;
        }
        // `git init` is a convenience; never a reason to fail.
        if !dir.join(".git").exists() && !inside_git_repo(dir) {
            let _ = std::process::Command::new("git")
                .arg("init")
                .arg("--quiet")
                .arg(dir)
                .status();
        }
    }
    ui::action(
        "created",
        format!(
            "{} `{name}` package",
            if lib {
                "library"
            } else {
                "binary (application)"
            }
        ),
    );
    Ok(())
}

fn inside_git_repo(dir: &Path) -> bool {
    let mut cur = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    loop {
        if cur.join(".git").exists() {
            return true;
        }
        if !cur.pop() {
            return false;
        }
    }
}

pub fn new(args: NewArgs) -> Result<()> {
    if args.path.exists() {
        bail!(
            "destination `{}` already exists (use `rune init` to initialize it)",
            args.path.display()
        );
    }
    let name = match &args.name {
        Some(n) => n.clone(),
        None => args
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string)
            .context("cannot infer a package name from the path; pass --name")?,
    };
    scaffold(
        &args.path,
        &name,
        args.lib,
        &args.edition,
        !args.no_gitignore,
    )
}

pub fn init(args: InitArgs) -> Result<()> {
    fs::create_dir_all(&args.path)?;
    let abs = args.path.canonicalize()?;
    let name = match &args.name {
        Some(n) => n.clone(),
        None => abs
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string)
            .context("cannot infer a package name; pass --name")?,
    };
    // Follow existing sources when initializing a populated directory.
    let lib = args.lib
        || (!args.bin && abs.join("src/lib.rs").exists() && !abs.join("src/main.rs").exists());
    scaffold(&abs, &name, lib, &args.edition, !args.no_gitignore)
}

// ---------------------------------------------------------------------------
// clean
// ---------------------------------------------------------------------------

pub fn clean(args: CleanArgs) -> Result<()> {
    let root = find_project_root(args.manifest.manifest_path.as_deref());
    let mut config = match &root {
        Some(r) => {
            let settings = Settings::load(Some(r))?;
            RuneConfig::new(r).with_settings(settings)
        }
        None => bail!("could not find `Cargo.toml` in the current directory or any parent"),
    };
    if let Some(dir) = &args.target_dir {
        config.target_dir = dir.clone();
    }

    let target = if args.release {
        config.target_dir.join("release")
    } else if let Some(profile) = &args.profile {
        config.target_dir.join(match profile.as_str() {
            "dev" | "test" => "debug",
            "bench" => "release",
            other => other,
        })
    } else {
        config.target_dir.clone()
    };

    if target.exists() {
        fs::remove_dir_all(&target)
            .with_context(|| format!("failed to remove {}", target.display()))?;
        ui::action("cleaned", target.display());
    } else {
        println!("nothing to clean");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// fmt
// ---------------------------------------------------------------------------

/// `rune fmt`: formats every selected package's source with `rustfmt`.
///
/// Discovers each crate's entry points from its manifest rather than walking
/// `.rs` files directly — `rustfmt` follows `mod` declarations from a crate
/// root, so handing it just the entry points formats every file the module
/// tree reaches, as `cargo fmt` does. Crates are grouped by edition since
/// `rustfmt` takes a single `--edition` per invocation.
pub fn fmt(args: FmtArgs) -> Result<()> {
    let ws = Workspace::load(args.manifest.manifest_path.as_deref())?;
    let selected = ws.select(&args.package, args.workspace, &[])?;

    let mut by_edition: HashMap<Edition, Vec<PathBuf>> = HashMap::new();
    for &mi in &selected {
        let m = &ws.members[mi];
        let entries = by_edition.entry(m.edition).or_default();
        if let Some(lib) = &m.manifest.lib {
            entries.push(
                m.dir
                    .join(lib.path.as_deref().unwrap_or(Path::new("src/lib.rs"))),
            );
        }
        for t in m
            .manifest
            .bin
            .iter()
            .chain(&m.manifest.example)
            .chain(&m.manifest.test)
            .chain(&m.manifest.bench)
        {
            if let Some(p) = &t.path {
                entries.push(m.dir.join(p));
            }
        }
    }

    let mut total = 0;
    for entries in by_edition.values_mut() {
        entries.retain(|p| p.exists());
        entries.sort();
        entries.dedup();
        total += entries.len();
    }
    if total == 0 {
        ui::warn("no local crate source found to format");
        return Ok(());
    }

    ui::action(
        if args.check { "checking" } else { "formatting" },
        format!("{} target{}", total, if total == 1 { "" } else { "s" }),
    );

    let rustfmt = std::env::var_os("RUSTFMT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("rustfmt"));

    let mut needs_formatting = false;
    let mut groups: Vec<_> = by_edition
        .into_iter()
        .filter(|(_, v)| !v.is_empty())
        .collect();
    groups.sort_by_key(|(e, _)| e.as_str());
    for (edition, entries) in groups {
        let mut cmd = std::process::Command::new(&rustfmt);
        cmd.arg("--edition").arg(edition.as_str());
        if args.check {
            cmd.arg("--check");
        }
        cmd.args(&entries);
        let status = cmd.status().with_context(|| {
            format!(
                "failed to run `{}` — install it with `rustup component add rustfmt`",
                rustfmt.display()
            )
        })?;
        if !status.success() {
            if args.check {
                needs_formatting = true;
            } else {
                bail!("rustfmt exited with an error");
            }
        }
    }
    if needs_formatting {
        bail!("some files are not formatted (run `rune fmt` to fix)");
    }
    ui::summary(if args.check {
        "formatting OK".to_string()
    } else {
        "formatted".to_string()
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// config
// ---------------------------------------------------------------------------

fn config_scope(
    global: bool,
    project: bool,
    root: &Option<PathBuf>,
    for_write: bool,
) -> Result<Scope> {
    match (global, project) {
        (true, _) => Ok(Scope::Global),
        (_, true) => {
            if root.is_none() {
                bail!("`--project` needs to be run inside a Cargo project");
            }
            Ok(Scope::Project)
        }
        _ if for_write && root.is_some() => Ok(Scope::Project),
        _ => Ok(Scope::Global),
    }
}

pub fn config(cmd: ConfigCommands) -> Result<()> {
    let root = find_project_root(None);
    let settings = Settings::load(root.as_deref())?;

    match cmd {
        ConfigCommands::List {
            global,
            project,
            show_origin,
        } => {
            if global || project {
                let scope = config_scope(global, project, &root, false)?;
                let path = config_path(scope, root.as_deref()).context("no such settings file")?;
                let mut any = false;
                for def in KEYS {
                    if let Some(v) = settings::read_from_file(&path, def.name)? {
                        println!("{} = {}", def.name, v);
                        any = true;
                    }
                }
                if !any {
                    ui::detail(format!("no settings in {}", path.display()));
                }
                return Ok(());
            }
            let rows = settings.effective();
            let width = rows.iter().map(|(d, ..)| d.name.len()).max().unwrap_or(0);
            let vwidth = rows.iter().map(|(_, v, _)| v.len()).max().unwrap_or(0);
            for (def, value, origin) in rows {
                let set = origin != Origin::Default;
                let line = format!("{:<width$} = {:<vwidth$}", def.name, value);
                let line = if set { line.bold().to_string() } else { line };
                if show_origin {
                    println!("{line}  {}", format!("# {origin}").dimmed());
                } else {
                    println!("{line}");
                }
                println!("    {}", def.description.dimmed());
            }
        }
        ConfigCommands::Get {
            key,
            global,
            project,
        } => {
            let def = key_def(&key)
                .ok_or_else(|| settings::SettingsError::UnknownKey { key: key.clone() })?;
            if global || project {
                let scope = config_scope(global, project, &root, false)?;
                let path = config_path(scope, root.as_deref()).context("no such settings file")?;
                match settings::read_from_file(&path, &key)? {
                    Some(v) => println!("{v}"),
                    None => bail!("`{key}` is not set in {}", path.display()),
                }
            } else {
                let value = settings
                    .get(&key)
                    .map(|(v, _)| v.to_string())
                    .unwrap_or_else(|| def.default.to_string());
                println!("{value}");
            }
        }
        ConfigCommands::Set {
            key,
            value,
            global,
            project,
        } => {
            let scope = config_scope(global, project, &root, true)?;
            let path =
                config_path(scope, root.as_deref()).context("no settings file for this scope")?;
            let stored = settings::set_in_file(&path, &key, &value)?;
            ui::action(
                "set",
                format!(
                    "{} = {} ({}: {})",
                    key.bold(),
                    stored,
                    if scope == Scope::Global {
                        "global"
                    } else {
                        "project"
                    },
                    path.display()
                ),
            );
            // Say so if something with higher precedence still wins.
            let effective = Settings::load(root.as_deref())?;
            if let Some((v, origin)) = effective.get(&key) {
                if *v != stored {
                    ui::warn(format!("the effective value is still {v}, from {origin}"));
                }
            }
            if scope == Scope::Global && root.is_some() {
                if let Some(proj) = root.as_ref().map(|r| r.join(settings::PROJECT_FILE)) {
                    if settings::read_from_file(&proj, &key)
                        .ok()
                        .flatten()
                        .is_some()
                    {
                        ui::warn(format!(
                            "this project's {} overrides the global value",
                            settings::PROJECT_FILE
                        ));
                    }
                }
            }
        }
        ConfigCommands::Unset {
            key,
            global,
            project,
        } => {
            let scope = config_scope(global, project, &root, true)?;
            let path =
                config_path(scope, root.as_deref()).context("no settings file for this scope")?;
            if settings::unset_in_file(&path, &key)? {
                ui::action("unset", format!("{} in {}", key.bold(), path.display()));
            } else {
                ui::warn(format!("`{key}` was not set in {}", path.display()));
            }
        }
        ConfigCommands::Path => {
            println!("global:  {}", settings::global_config_path().display());
            match &root {
                Some(r) => println!("project: {}", r.join(settings::PROJECT_FILE).display()),
                None => println!("project: (not inside a Cargo project)"),
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// cas / lockfile
// ---------------------------------------------------------------------------

pub fn cas(cmd: CasCommands) -> Result<()> {
    let root = find_project_root(None);
    let settings = Settings::load(root.as_deref())?;
    let config =
        RuneConfig::new(root.clone().unwrap_or_else(|| PathBuf::from("."))).with_settings(settings);
    let cas = CasStore::new(config.cas_dir.clone())?;

    match cmd {
        CasCommands::Stats => {
            let stats = cas.stats().unwrap_or_default();
            let total = stats.hits + stats.misses;
            let hit_rate = if total > 0 {
                (stats.hits as f64 / total as f64) * 100.0
            } else {
                0.0
            };
            ui::heading("content-addressable store");
            println!("  location     {}", config.cas_dir.display());
            println!("  entries      {}", stats.total_entries);
            println!(
                "  size         {}",
                format_size_human(stats.total_size_bytes)
            );
            println!("  hits         {}", stats.hits.to_string().green());
            println!("  misses       {}", stats.misses.to_string().yellow());
            println!("  hit rate     {hit_rate:.1}%");
            if let Some(max) = config.settings.cas_max_size() {
                println!("  max size     {}", format_size(max));
            }
        }
        CasCommands::Gc(args) => {
            let max_size = match &args.max_size {
                Some(s) => Some(parse_size(s).with_context(|| format!("invalid size `{s}`"))?),
                None => config.settings.cas_max_size(),
            };
            if max_size.is_none() && args.max_age.is_none() {
                bail!(
                    "nothing to do: pass --max-size / --max-age, or set `cas.max-size` with `rune config set`"
                );
            }
            let report = run_gc(
                &cas,
                &GcConfig {
                    max_size_bytes: max_size,
                    max_age_secs: args.max_age,
                },
            )
            .map_err(|e| anyhow::anyhow!("CAS GC failed: {e}"))?;
            ui::action(
                "collected",
                format!(
                    "{} entries, {} freed",
                    report.entries_removed,
                    format_size_human(report.bytes_freed)
                ),
            );
            ui::detail(format!(
                "{} entries remaining, {}",
                report.entries_remaining,
                format_size_human(report.bytes_remaining)
            ));
        }
    }
    Ok(())
}

fn format_size_human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.2} {}", UNITS[i])
    }
}

pub fn lockfile(cmd: LockfileCommands) -> Result<()> {
    let root = find_project_root(None)
        .context("could not find `Cargo.toml` in the current directory or any parent")?;
    match cmd {
        LockfileCommands::Export { format, output } => {
            let lock = LockfileManager::load_fast(&root).context("No lockfile found to export")?;
            let text = match format.as_str() {
                "json" => serde_json::to_string_pretty(&lock)?,
                _ => toml::to_string_pretty(&lock)?,
            };
            match output {
                Some(p) if p.as_os_str() != "-" => {
                    if format == "cargo" {
                        LockfileManager::write_cargo(&lock, &p)?;
                    } else {
                        fs::write(&p, text)?;
                    }
                    ui::action("exported", p.display());
                }
                _ => println!("{text}"),
            }
        }
        LockfileCommands::Migrate => {
            let cargo_path = root.join("Cargo.lock");
            let lock = LockfileManager::read_cargo(&cargo_path)
                .context("Failed to read Cargo.lock for migration")?;
            let bin_path = root.join("Rune.lock.bin");
            LockfileManager::write_binary(&lock, &bin_path)?;
            ui::action(
                "migrated",
                format!(
                    "Cargo.lock to Rune.lock.bin ({} packages)",
                    lock.packages.len()
                ),
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_name_validation() {
        assert!(validate_package_name("my-crate").is_ok());
        assert!(validate_package_name("my_crate2").is_ok());
        assert!(validate_package_name("").is_err());
        assert!(validate_package_name("1abc").is_err());
        assert!(validate_package_name("has space").is_err());
        assert!(validate_package_name("fn").is_err());
        assert!(validate_package_name("self").is_err());
    }

    #[test]
    fn scaffold_creates_a_buildable_layout() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("demo");
        scaffold(&p, "demo", true, "2021", false).unwrap();
        assert!(p.join("Cargo.toml").is_file());
        assert!(
            fs::read_to_string(p.join("src/lib.rs"))
                .unwrap()
                .contains("#[test]")
        );
        // A second init must not clobber it.
        assert!(scaffold(&p, "demo", true, "2021", false).is_err());
    }

    #[test]
    fn human_sizes() {
        assert_eq!(format_size_human(512), "512 B");
        assert_eq!(format_size_human(2048), "2.00 KiB");
    }
}
