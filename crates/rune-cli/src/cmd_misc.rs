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

fn scaffold(
    dir: &Path,
    name: &str,
    lib: bool,
    edition: &str,
    gitignore: bool,
    manifest_file: &str,
) -> Result<()> {
    validate_package_name(name)?;
    if let Some(existing) = rune_common::manifest_file::find_manifest(dir) {
        bail!(
            "`{}` already exists in `{}`",
            existing
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("a manifest"),
            dir.display()
        );
    }
    let _: Edition = edition.parse().map_err(|e: String| anyhow::anyhow!(e))?;
    fs::create_dir_all(dir.join("src"))?;

    let manifest = format!(
        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"{edition}\"\n\n[dependencies]\n"
    );
    let manifest_path = dir.join(manifest_file);
    // A `Rune.toml` may already exist as a settings file; keep those settings.
    let content = match fs::read_to_string(&manifest_path) {
        Ok(existing) if !existing.trim().is_empty() => format!("{manifest}\n{existing}"),
        _ => manifest,
    };
    fs::write(&manifest_path, content)?;

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
        manifest_file_for(&args.manifest),
    )
}

/// `--manifest cargo|rune` → the file name.
fn manifest_file_for(choice: &str) -> &'static str {
    if choice == "rune" {
        rune_common::manifest_file::RUNE_TOML
    } else {
        rune_common::manifest_file::CARGO_TOML
    }
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
    scaffold(
        &abs,
        &name,
        lib,
        &args.edition,
        !args.no_gitignore,
        manifest_file_for(&args.manifest),
    )
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
        None => bail!(
            "could not find `Cargo.toml` or `Rune.toml` in the current directory or any parent"
        ),
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
pub fn fmt(mut args: FmtArgs) -> Result<()> {
    // `rune fmt -- --check`, as CI writes it for `cargo fmt`: the check flag
    // after `--` means the same as before it; the rest goes to rustfmt.
    if let Some(i) = args.rustfmt_args.iter().position(|a| a == "--check") {
        args.rustfmt_args.remove(i);
        args.check = true;
    }
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
        cmd.args(&args.rustfmt_args);
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
            if let Some((v, origin)) = effective.get(&key)
                && *v != stored
            {
                ui::warn(format!("the effective value is still {v}, from {origin}"));
            }
            if scope == Scope::Global
                && root.is_some()
                && let Some(proj) = root.as_ref().map(|r| r.join(settings::PROJECT_FILE))
                && settings::read_from_file(&proj, &key)
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
        CasCommands::Import { path } => {
            let stats = cas
                .import_bundle(&path)
                .with_context(|| format!("cannot import {}", path.display()))?;
            ui::summary(format!(
                "imported {} entries ({}){}",
                stats.entries,
                format_size_human(stats.bytes),
                if stats.skipped > 0 {
                    format!(" · {} already present or damaged", stats.skipped)
                } else {
                    String::new()
                }
            ));
        }
        CasCommands::Export(_) => unreachable!("handled by cmd_build::cas_export"),
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
        CasCommands::Du(args) => {
            let u = rune_cas::usage::usage(&cas);
            ui::heading("content-addressable store usage");
            println!("  location     {}", config.cas_dir.display());
            println!("  entries      {}", u.entries);
            println!("  size         {}", format_size_human(u.bytes));
            if u.packages.is_empty() {
                return Ok(());
            }
            println!();
            println!(
                "  {:<32} {:>8} {:>10}  last used",
                "package", "entries", "size"
            );
            for p in u.packages.iter().take(args.top) {
                println!(
                    "  {:<32} {:>8} {:>10}  {}",
                    p.name,
                    p.entries,
                    format_size_human(p.bytes),
                    format_idle(p.idle_secs)
                );
            }
            if u.packages.len() > args.top {
                let rest: u64 = u.packages[args.top..].iter().map(|p| p.bytes).sum();
                println!(
                    "  … {} more packages, {}",
                    u.packages.len() - args.top,
                    format_size_human(rest)
                );
            }
            println!();
            for b in &u.idle {
                if b.entries > 0 {
                    println!(
                        "  {:>10} unused for {:<8} ({} entries)",
                        format_size_human(b.bytes),
                        format_idle_threshold(b.min_idle_secs),
                        b.entries
                    );
                }
            }
            if let Some(b) = u.idle.iter().rev().find(|b| b.entries > 0) {
                ui::detail(format!(
                    "free it with: rune cas gc --max-age {}",
                    b.min_idle_secs
                ));
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

pub(crate) fn format_size_human(bytes: u64) -> String {
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
    let root = find_project_root(None).context(
        "could not find `Cargo.toml` or `Rune.toml` in the current directory or any parent",
    )?;
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
        scaffold(&p, "demo", true, "2021", false, "Cargo.toml").unwrap();
        assert!(p.join("Cargo.toml").is_file());
        assert!(
            fs::read_to_string(p.join("src/lib.rs"))
                .unwrap()
                .contains("#[test]")
        );
        // A second init must not clobber it.
        assert!(scaffold(&p, "demo", true, "2021", false, "Cargo.toml").is_err());
    }

    #[test]
    fn human_sizes() {
        assert_eq!(format_size_human(512), "512 B");
        assert_eq!(format_size_human(2048), "2.00 KiB");
    }
}

/// "3 min ago", "2 days ago", … for a duration in seconds.
fn format_idle(secs: u64) -> String {
    match secs {
        0..=59 => "just now".to_string(),
        60..=3_599 => format!("{} min ago", secs / 60),
        3_600..=86_399 => format!("{} h ago", secs / 3_600),
        _ => format!("{} days ago", secs / 86_400),
    }
}

fn format_idle_threshold(secs: u64) -> String {
    match secs {
        0..=3_599 => format!("{} min", secs / 60),
        3_600..=86_399 => format!("{} h", secs / 3_600),
        86_400 => "1 day".to_string(),
        _ => format!("{} days", secs / 86_400),
    }
}

#[cfg(test)]
mod du_format_tests {
    use super::*;

    #[test]
    fn idle_times_read_naturally() {
        assert_eq!(format_idle(5), "just now");
        assert_eq!(format_idle(600), "10 min ago");
        assert_eq!(format_idle(7_200), "2 h ago");
        assert_eq!(format_idle(3 * 86_400), "3 days ago");
        assert_eq!(format_idle_threshold(86_400), "1 day");
        assert_eq!(format_idle_threshold(30 * 86_400), "30 days");
    }
}

/// `rune doctor`.
pub fn doctor() -> Result<()> {
    use crate::doctor::{Environment, Level, diagnose, fs_type_of};

    let root = find_project_root(None);
    let settings = Settings::load(root.as_deref())?;
    let config =
        RuneConfig::new(root.clone().unwrap_or_else(|| PathBuf::from("."))).with_settings(settings);

    // rustc -vV
    let vv =
        std::process::Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
            .arg("-vV")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
    let field = |name: &str| {
        vv.as_deref()?
            .lines()
            .find_map(|l| l.strip_prefix(name))
            .map(|v| v.trim().to_string())
    };
    let host = field("host:");

    // Project-level configuration, when we are inside a project.
    let ws = crate::workspace::Workspace::load(None).ok();
    let mut rustflags = crate::session::rustflags_from_env_public();
    let mut configured_linker = None;
    let mut dev_debug_full_explicit = false;
    if let Some(ws) = &ws {
        rustflags.extend(
            ws.cargo_config
                .string_list(&["build", "rustflags"])
                .unwrap_or_default(),
        );
        if let Some(h) = &host {
            configured_linker = ws.cargo_config.string(&["target", h, "linker"]);
            rustflags.extend(
                ws.cargo_config
                    .string_list(&["target", h, "rustflags"])
                    .unwrap_or_default(),
            );
        }
        dev_debug_full_explicit = ws
            .root_manifest
            .profile
            .get("dev")
            .and_then(|t| t.debug.as_ref())
            .is_some_and(|d| {
                matches!(d, toml::Value::Boolean(true) | toml::Value::Integer(2))
                    || d.as_str() == Some("full")
            });
    }

    let cas = CasStore::new(config.cas_dir.clone())?;
    let usage = rune_cas::usage::usage(&cas);
    let idle_30d = usage
        .idle
        .iter()
        .find(|b| b.min_idle_secs == 30 * 86_400)
        .map_or(0, |b| b.bytes);
    let home = config
        .cas_dir
        .parent()
        .unwrap_or(&config.cas_dir)
        .to_path_buf();
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();

    let env = Environment {
        rustc_release: field("release:"),
        host,
        on_path: Box::new(which_on_path),
        rustflags,
        configured_linker,
        dev_debug_full_explicit,
        full_dependency_debuginfo: config.settings.dependency_debuginfo(),
        home: home.display().to_string(),
        home_fs: fs_type_of(&home, &mounts),
        cas_bytes: usage.bytes,
        cas_idle_bytes: idle_30d,
        jobs: config.jobs,
        cpus: std::thread::available_parallelism().map_or(1, |n| n.get()),
        rustc_wrapper: std::env::var("RUSTC_WRAPPER")
            .ok()
            .filter(|w| !w.is_empty()),
        shadowed_cargo_toml: ws.as_ref().is_some_and(|ws| {
            use rune_common::manifest_file::{
                CARGO_TOML, RUNE_TOML, find_manifest, text_is_manifest,
            };
            let root = ws.root();
            find_manifest(root).is_some_and(|m| m.file_name().is_some_and(|n| n == RUNE_TOML))
                && std::fs::read_to_string(root.join(CARGO_TOML))
                    .is_ok_and(|t| text_is_manifest(&t))
        }),
    };

    let findings = diagnose(&env);
    let warnings = findings.iter().filter(|f| f.level == Level::Warn).count();
    for f in &findings {
        match f.level {
            Level::Ok => println!("  {} {}", "✓".green().bold(), f.title),
            Level::Note => println!("  {} {}", "·".dimmed(), f.title),
            Level::Warn => println!("  {} {}", "!".yellow().bold(), f.title.bold()),
        }
        if f.level != Level::Ok
            && let Some(d) = &f.detail
        {
            println!("      {d}");
        }
        if let Some(fix) = &f.fix {
            for (i, line) in fix.lines().enumerate() {
                println!("      {}{line}", if i == 0 { "fix: " } else { "     " });
            }
        }
    }
    println!();
    if warnings == 0 {
        ui::summary("nothing to fix");
    } else {
        ui::summary(format!("{warnings} thing(s) worth fixing"));
    }
    Ok(())
}

/// Whether an executable called `tool` is in a `PATH` directory.
fn which_on_path(tool: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| {
            let p = dir.join(tool);
            p.is_file()
        })
    })
}

/// `rune affected`.
pub fn affected(args: crate::args::AffectedArgs) -> Result<()> {
    let ws = crate::workspace::Workspace::load(args.manifest_path.as_deref())?;
    let root = ws.config.project_root.clone();

    let git = |extra: &[&str]| -> Result<Vec<PathBuf>> {
        let out = std::process::Command::new("git")
            .args(extra)
            .current_dir(&root)
            .output()
            .context("failed to run `git` (needed by `rune affected`)")?;
        if !out.status.success() {
            bail!(
                "`git {}` failed: {}",
                extra.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(PathBuf::from)
            .collect())
    };
    // `--relative` makes paths relative to the project root even when the
    // repository is larger than the workspace.
    let mut changed = git(&["diff", "--name-only", "--relative", &args.since])?;
    changed.extend(git(&["ls-files", "--others", "--exclude-standard"])?);

    let pkgs: Vec<crate::affected::Pkg> = ws
        .members
        .iter()
        .map(|m| crate::affected::Pkg {
            name: m.name.clone(),
            dir: m.dir.clone(),
            deps: m
                .manifest
                .dependencies
                .values()
                .map(|d| d.spec.package.clone().unwrap_or_else(|| d.name.clone()))
                .collect(),
        })
        .collect();
    let mut names = crate::affected::affected(&pkgs, &root, &changed);
    if let Some(spec) = &args.partition {
        let (n, m) = crate::affected::parse_partition(spec)
            .with_context(|| format!("`--partition {spec}`: expected N/M with 1 ≤ N ≤ M"))?;
        names = crate::affected::partition(&names, n, m);
    }

    match args.format.as_str() {
        "json" => println!("{}", serde_json::to_string(&names)?),
        "args" => println!(
            "{}",
            names
                .iter()
                .map(|n| format!("-p {n}"))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        _ => names.iter().for_each(|n| println!("{n}")),
    }
    Ok(())
}

/// `rune script`.
pub fn script(args: crate::args::ScriptArgs) -> Result<()> {
    let file = std::fs::canonicalize(&args.file)
        .with_context(|| format!("cannot read script `{}`", args.file.display()))?;
    let text = std::fs::read_to_string(&file)
        .with_context(|| format!("cannot read script `{}`", file.display()))?;
    let parsed =
        crate::script::parse(&text).map_err(|e| anyhow::anyhow!("{}: {e}", file.display()))?;
    let manifest = crate::script::manifest(&file, &parsed.manifest)
        .map_err(|e| anyhow::anyhow!("{}: {e}", file.display()))?;

    // One package directory per script, named after it and keyed by its path.
    let mut h = rune_common::hash::HashBuilder::new();
    h.update(file.to_string_lossy().as_bytes());
    let id: String = h.finish_blake3().chars().take(12).collect();
    let dir = rune_common::settings::rune_home()
        .join("scripts")
        .join(format!("{}-{id}", crate::script::package_name(&file)));
    std::fs::create_dir_all(dir.join("src"))?;
    // Rewritten only when different: an untouched script is an untouched package.
    write_if_changed(&dir.join("Cargo.toml"), &manifest)?;
    write_if_changed(&dir.join("src/main.rs"), &parsed.source)?;

    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.arg("run")
        .arg("--quiet")
        .arg("--manifest-path")
        .arg(dir.join("Cargo.toml"));
    if args.release {
        cmd.arg("--release");
    }
    if args.offline {
        cmd.arg("--offline");
    }
    cmd.arg("--").args(&args.args);
    let status = cmd.status().context("failed to start the script")?;
    std::process::exit(status.code().unwrap_or(1));
}

fn write_if_changed(path: &Path, content: &str) -> Result<()> {
    if std::fs::read_to_string(path).is_ok_and(|old| old == content) {
        return Ok(());
    }
    std::fs::write(path, content).with_context(|| format!("cannot write {}", path.display()))
}
