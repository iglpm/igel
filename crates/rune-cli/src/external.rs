//! Unknown `rune <name>`: an `[alias]` from `.cargo/config.toml`, or a `rune-<name>`/`cargo-<name>`
//! executable on `PATH`.

use clap::CommandFactory;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub fn expand(mut args: Vec<OsString>) -> Vec<OsString> {
    for _ in 0..16 {
        let Some(pos) = subcommand_position(&args) else {
            return args;
        };
        let Some(name) = args[pos].to_str().map(str::to_string) else {
            return args;
        };
        if is_builtin(&name) {
            return args;
        }
        if let Some(expansion) = alias(&name) {
            args.splice(pos..=pos, expansion.into_iter().map(OsString::from));
            continue;
        }
        if let Some(exe) = find_external(&name) {
            run_external(&exe, &name, &args[pos + 1..]);
        }
        return args;
    }
    crate::ui::error("aliases expand into each other endlessly");
    std::process::exit(101);
}

fn subcommand_position(args: &[OsString]) -> Option<usize> {
    let mut i = 1;
    while i < args.len() {
        let a = args[i].to_str()?;
        if a == "--color" {
            i += 2;
            continue;
        }
        if a.starts_with('-') {
            if a.starts_with("--color=") {
                i += 1;
                continue;
            }
            return None;
        }
        return Some(i);
    }
    None
}

fn is_builtin(name: &str) -> bool {
    name == "help" || crate::args::Cli::command().find_subcommand(name).is_some()
}

fn alias(name: &str) -> Option<Vec<String>> {
    let cwd = std::env::current_dir().ok()?;
    let config = rune_common::cargo_config::CargoConfig::load(&cwd);
    match config.file_value(&["alias", name])? {
        toml::Value::String(s) => Some(s.split_whitespace().map(str::to_string).collect()),
        toml::Value::Array(a) => Some(
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
        ),
        _ => None,
    }
}

fn find_external(name: &str) -> Option<PathBuf> {
    let exe = |base: &str| format!("{base}-{name}{}", std::env::consts::EXE_SUFFIX);
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    if let Some(home) = rune_registry::source::cargo_home() {
        dirs.push(home.join("bin"));
    }
    for base in ["rune", "cargo"] {
        for dir in &dirs {
            let candidate = dir.join(exe(base));
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

fn run_external(exe: &Path, name: &str, rest: &[OsString]) -> ! {
    let rune = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("rune"));
    let cargo = match std::env::var("RUNE_PLUGIN_CARGO").as_deref() {
        Ok("rune") => rune.clone(),
        _ => {
            rune_common::toolchain::find_program(Path::new("cargo")).unwrap_or_else(|| rune.clone())
        }
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg(name)
        .args(rest)
        .env("CARGO", &cargo)
        .env("RUNE", &rune);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        crate::ui::error(format!("failed to run `{}`: {err}", exe.display()));
        std::process::exit(101);
    }
    #[cfg(not(unix))]
    {
        match cmd.status() {
            Ok(s) => std::process::exit(s.code().unwrap_or(101)),
            Err(e) => {
                crate::ui::error(format!("failed to run `{}`: {e}", exe.display()));
                std::process::exit(101);
            }
        }
    }
}
