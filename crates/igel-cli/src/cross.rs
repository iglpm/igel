//! Cross-compilation, built in: `igl cross …`, and `--cross` on the build commands. hyprcross,
//! built into Igel, does the work: on this machine when it can build the target, else in a warm
//! container from the target's image, with this Igel building there too. Its settings are
//! `cross.*` and `[cross.target.<triple>]`, in `~/.igel/config.toml` or the project's
//! `Igel.toml`; hyprcross' own `Hyprcross.toml` and cross-rs' `Cross.toml` are read as well.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use hyprcross::builder::{Builder, BuilderKind};
use hyprcross::config::{Config, TargetConfig};
use hyprcross::host::Host;
use igel_common::settings::{self, CrossSettings, Settings};

/// What hyprcross' messages and help call the command.
const COMMAND: &str = "igl cross";

/// Build commands `--cross` works with, by every name, and what hyprcross calls them.
const COMMANDS: &[(&str, &str)] = &[
    ("build", "build"),
    ("b", "build"),
    ("check", "check"),
    ("c", "check"),
    ("test", "test"),
    ("t", "test"),
    ("run", "run"),
    ("r", "run"),
    ("bench", "bench"),
    ("clippy", "clippy"),
    ("doc", "doc"),
];

/// `igl cross …`, `igl <build command> … --cross`, and `igl help cross`: run here, with the
/// exit code. `None` for anything else.
pub fn intercept(args: &[OsString]) -> Option<Result<i32>> {
    let pos = command_position(args)?;
    let name = args[pos].to_str()?;
    let globals = Globals::split(&args[1..pos]);
    if name == "cross" {
        return Some(cross_command(&args[pos + 1..], &globals));
    }
    if name == "help" && args.get(pos + 1).is_some_and(|a| a == "cross") {
        let mut hosted: Vec<OsString> = vec![COMMAND.into()];
        if args.len() > pos + 2 {
            hosted.push("help".into());
            hosted.extend(args[pos + 2..].iter().cloned());
        } else {
            hosted.push("--help".into());
        }
        return Some(run_hosted(hosted));
    }
    let (_, sub) = COMMANDS.iter().find(|(n, _)| *n == name)?;
    let request = match CrossFlags::extract(&args[pos + 1..])? {
        Ok(r) => r,
        Err(e) => return Some(Err(e)),
    };
    Some(cross_build(sub, request, &globals))
}

/// `igl cross <args>` as clap parsed it.
pub fn command(args: Vec<OsString>) -> Result<()> {
    match cross_command(&args, &Globals::default())? {
        0 => Ok(()),
        code => std::process::exit(code),
    }
}

/// The container's agent (`<this executable> __agent <socket> …`): Igel is mounted into warm
/// containers and takes builds there.
pub fn agent(args: Vec<OsString>) -> i32 {
    let mut hosted: Vec<OsString> = vec![COMMAND.into()];
    hosted.extend(args.into_iter().skip(1));
    let host = Host {
        command: COMMAND,
        ..Host::standalone()
    };
    match hyprcross::run_hosted(hosted, host) {
        Ok(code) => code,
        Err(e) => {
            hyprcross::ui::fatal(&e);
            1
        }
    }
}

/// Where the command is: the first argument that isn't one of Igel's global options.
fn command_position(args: &[OsString]) -> Option<usize> {
    let mut i = 1;
    while i < args.len() {
        let a = args[i].to_str()?;
        if a == "--color" || a == "--config" {
            i += 2;
        } else if a.starts_with("--color=") || a.starts_with("--config=") {
            i += 1;
        } else if a.starts_with('-') {
            return None;
        } else {
            return Some(i);
        }
    }
    None
}

/// Igel's global options given before the command: `--color` for hyprcross, `--config` for the
/// Igel that builds.
#[derive(Default)]
struct Globals {
    color: Vec<OsString>,
    config: Vec<OsString>,
}

impl Globals {
    fn split(args: &[OsString]) -> Self {
        let mut g = Globals::default();
        let mut i = 0;
        while i < args.len() {
            let a = args[i].to_string_lossy();
            let (bucket, takes_value) = if a.starts_with("--color") {
                (&mut g.color, a == "--color")
            } else {
                (&mut g.config, a == "--config")
            };
            bucket.push(args[i].clone());
            if takes_value && let Some(v) = args.get(i + 1) {
                bucket.push(v.clone());
                i += 1;
            }
            i += 1;
        }
        g
    }
}

/// `--cross[=<triple>]`, `--cross-engine <engine>` and `--target <triple>` taken from a build
/// command's arguments; everything else stays for the Igel that builds.
#[derive(Debug, PartialEq, Eq)]
struct CrossFlags {
    target: Option<String>,
    engine: Option<String>,
    rest: Vec<OsString>,
}

impl CrossFlags {
    /// `None` without `--cross` (before any `--`).
    fn extract(args: &[OsString]) -> Option<Result<Self>> {
        let mut cross: Option<Option<String>> = None;
        let mut target = None;
        let mut engine = None;
        let mut rest = Vec::new();
        let mut i = 0;
        while i < args.len() {
            let a = &args[i];
            let s = a.to_str().unwrap_or("");
            if s == "--" {
                rest.extend(args[i..].iter().cloned());
                break;
            }
            let next = || args.get(i + 1).and_then(|v| v.to_str()).map(str::to_string);
            if s == "--cross" {
                match next().filter(|v| looks_like_target(v)) {
                    Some(v) => {
                        cross = Some(Some(v));
                        i += 1;
                    }
                    None => cross = Some(None),
                }
            } else if let Some(v) = s.strip_prefix("--cross=") {
                cross = Some(Some(v.to_string()));
            } else if s == "--cross-engine" {
                let Some(v) = next() else {
                    return Some(Err(anyhow::anyhow!(
                        "`--cross-engine` needs a value: auto, direct or container"
                    )));
                };
                engine = Some(v);
                i += 1;
            } else if let Some(v) = s.strip_prefix("--cross-engine=") {
                engine = Some(v.to_string());
            } else if s == "--target" {
                target = next();
                i += 1;
            } else if let Some(v) = s.strip_prefix("--target=") {
                target = Some(v.to_string());
            } else {
                rest.push(a.clone());
            }
            i += 1;
        }
        let cross = cross?;
        let target = match (cross, target) {
            (Some(a), Some(b)) if a != b => {
                return Some(Err(anyhow::anyhow!(
                    "`--cross {a}` and `--target {b}` name different targets"
                )));
            }
            (a, b) => a.or(b),
        };
        Some(Ok(CrossFlags {
            target,
            engine,
            rest,
        }))
    }
}

/// `aarch64-unknown-linux-gnu`, not a test name: dashed, with an architecture or OS hyprcross
/// knows. (`igl test --cross parse` filters tests by `parse` and builds for the default
/// target.)
fn looks_like_target(s: &str) -> bool {
    use hyprcross::targets::{TargetArch, TargetInfo, TargetOs};
    if s.starts_with('-') || !s.contains('-') {
        return false;
    }
    let info = TargetInfo::parse(s);
    !matches!(info.arch, TargetArch::Other(_)) || !matches!(info.os, TargetOs::Other(_))
}

/// `igl cross <args>`: hyprcross' command line, with Igel's `--config` for the Igel that builds.
fn cross_command(args: &[OsString], globals: &Globals) -> Result<i32> {
    let mut hosted: Vec<OsString> = vec![COMMAND.into()];
    hosted.extend(globals.color.iter().cloned());
    hosted.extend(args.iter().cloned());
    let builds = args
        .iter()
        .find(|a| !a.to_string_lossy().starts_with('-'))
        .and_then(|a| a.to_str())
        .is_some_and(|a| COMMANDS.iter().any(|(n, _)| *n == a));
    if builds && !globals.config.is_empty() {
        // After hyprcross' `--`, arguments are the builder's.
        match hosted.iter().position(|a| a == "--") {
            Some(dash) => {
                let at = dash + 1;
                hosted.splice(at..at, globals.config.iter().cloned());
            }
            None => {
                hosted.push("--".into());
                hosted.extend(globals.config.iter().cloned());
            }
        }
    }
    run_hosted(hosted)
}

/// `igl <command> --cross …`: the same as `igl cross <command> -t <target> …`.
fn cross_build(sub: &str, flags: CrossFlags, globals: &Globals) -> Result<i32> {
    let mut hosted: Vec<OsString> = vec![COMMAND.into(), sub.into()];
    if let Some(t) = &flags.target {
        hosted.extend(["-t".into(), t.into()]);
    }
    if let Some(e) = &flags.engine {
        hosted.extend(["--engine".into(), e.into()]);
    }
    // Quiet and verbose apply to hyprcross' own lines too; the builder gets them as well.
    for flag in ["-q", "--quiet", "-v", "--verbose"] {
        if flags
            .rest
            .iter()
            .take_while(|a| *a != "--")
            .any(|a| a == flag)
        {
            hosted.push(flag.into());
        }
    }
    hosted.extend(globals.color.iter().cloned());
    hosted.push("--".into());
    hosted.extend(globals.config.iter().cloned());
    hosted.extend(flags.rest);
    run_hosted(hosted)
}

fn run_hosted(args: Vec<OsString>) -> Result<i32> {
    let host = host()?;
    match hyprcross::run_hosted(args, host) {
        Ok(code) => Ok(code),
        // Printed hyprcross' way: some of its errors carry a hint.
        Err(e) => {
            hyprcross::ui::fatal(&e);
            Ok(1)
        }
    }
}

/// hyprcross inside Igel: this Igel builds and links (with its own hyprlnk), and Igel's
/// settings go over hyprcross' files.
fn host() -> Result<Host> {
    let root = crate::workspace::find_project_root(None);
    let settings = Settings::load(root.as_deref())?;
    let cross = settings.cross();
    let linker = settings.linker();
    let targets = target_tables(root.as_deref())?;
    let exe = std::env::current_exe().context("cannot find Igel's own executable")?;
    Ok(Host {
        command: COMMAND,
        default_target_setting: "`cross.default-target` (`igl config set cross.default-target \
                                 <triple>`)",
        target_settings: "Igel.toml",
        builder: Some(Builder {
            kind: BuilderKind::Igel,
            path: exe,
        }),
        links_with_hyprlnk: true,
        configure: Some(Box::new(move |cfg| {
            apply(cfg, &cross, &linker, &targets);
        })),
        init: Some(init),
    })
}

/// Igel's `cross.*` settings over hyprcross' configuration, where set.
fn apply(cfg: &mut Config, s: &CrossSettings, linker: &str, targets: &[(String, TargetConfig)]) {
    let set = |field: &mut String, value: &Option<String>| {
        if let Some(v) = value {
            field.clone_from(v);
        }
    };
    if s.default_target.is_some() {
        cfg.build.default_target.clone_from(&s.default_target);
    }
    set(&mut cfg.build.engine, &s.engine);
    set(&mut cfg.build.container_engine, &s.container_engine);
    set(&mut cfg.build.image_repository, &s.image_repository);
    set(&mut cfg.build.image_tag, &s.image_tag);
    if let Some(d) = s.daemon {
        cfg.build.daemon = d;
    }
    if let Some(m) = s.idle_timeout_mins {
        cfg.build.idle_timeout_mins = m;
    }
    // `build.linker = "default"` reaches the Igel that builds through its own settings; this
    // only keeps hyprcross from announcing hyprlnk.
    if linker == "default" {
        cfg.build.linker = "default".into();
    }
    for (triple, t) in targets {
        cfg.merge_target(triple, t.clone());
    }
}

/// `[cross.target.<triple>]` tables: from `~/.igel/config.toml`, then the project's `Igel.toml`.
fn target_tables(root: Option<&Path>) -> Result<Vec<(String, TargetConfig)>> {
    let mut files = vec![settings::global_config_path()];
    if let Some(root) = root {
        files.push(root.join(settings::PROJECT_FILE));
    }
    let mut out = Vec::new();
    for file in files {
        out.extend(target_tables_in(&file)?);
    }
    Ok(out)
}

fn target_tables_in(file: &Path) -> Result<Vec<(String, TargetConfig)>> {
    let Ok(text) = std::fs::read_to_string(file) else {
        return Ok(Vec::new());
    };
    // A file that doesn't parse is reported by the settings, which read it first.
    let Ok(doc) = text.parse::<toml::Table>() else {
        return Ok(Vec::new());
    };
    let Some(tables) = doc
        .get("cross")
        .and_then(|c| c.get("target"))
        .and_then(|t| t.as_table())
    else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for (triple, table) in tables {
        let target: TargetConfig = table
            .clone()
            .try_into()
            .with_context(|| format!("invalid [cross.target.{triple}] in {}", file.display()))?;
        out.push((triple.clone(), target));
    }
    Ok(out)
}

/// `igl cross init [-t <triple>]`: the default target in the project's `Igel.toml`.
fn init(target: Option<&str>) -> Result<()> {
    let Some(target) = target else {
        bail!(
            "which target? `igl cross init -t <triple>` (`igl cross targets` lists those with images)"
        );
    };
    let root = crate::workspace::find_project_root(None)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let file = root.join(settings::PROJECT_FILE);
    settings::set_in_file(&file, "cross.default-target", target)?;
    hyprcross::ui::success(format!(
        "cross.default-target = \"{target}\" in {}",
        hyprcross::ui::tilde(&file)
    ));
    hyprcross::ui::hint(format!(
        "`igl cross build` and `igl build --cross` build for it now; per-target settings (an \
         image of your own, packages to install) go in [cross.target.{target}]"
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn flags(args: &[&str]) -> Option<CrossFlags> {
        CrossFlags::extract(&os(args)).map(|r| r.unwrap())
    }

    #[test]
    fn cross_flags_and_what_stays_for_the_builder() {
        assert_eq!(flags(&["--release", "-p", "app"]), None, "no --cross");
        assert_eq!(
            flags(&[
                "--release",
                "--cross",
                "aarch64-unknown-linux-gnu",
                "-p",
                "app"
            ]),
            Some(CrossFlags {
                target: Some("aarch64-unknown-linux-gnu".into()),
                engine: None,
                rest: os(&["--release", "-p", "app"]),
            })
        );
        assert_eq!(
            flags(&[
                "--target",
                "x86_64-pc-windows-gnu",
                "--cross",
                "--cross-engine=container"
            ]),
            Some(CrossFlags {
                target: Some("x86_64-pc-windows-gnu".into()),
                engine: Some("container".into()),
                rest: vec![],
            })
        );
        // A test filter isn't a target; what follows `--` is the test binaries'.
        assert_eq!(
            flags(&["--cross", "parse", "--", "--cross", "--nocapture"]),
            Some(CrossFlags {
                target: None,
                engine: None,
                rest: os(&["parse", "--", "--cross", "--nocapture"]),
            })
        );
        assert_eq!(
            flags(&["--", "--cross"]),
            None,
            "--cross after -- is the program's"
        );
        let clash = CrossFlags::extract(&os(&[
            "--cross=aarch64-unknown-linux-gnu",
            "--target",
            "x86_64-unknown-linux-gnu",
        ]))
        .unwrap();
        assert!(clash.is_err());
    }

    #[test]
    fn targets_are_told_from_test_names() {
        for t in [
            "aarch64-unknown-linux-gnu",
            "x86_64-pc-windows-gnu",
            "wasm32-wasip1",
            "mips-unknown-linux-gnu",
            "thumbv7em-none-eabihf",
        ] {
            assert!(looks_like_target(t), "{t}");
        }
        for t in ["parse", "parse-a-b", "--release", "my-test"] {
            assert!(!looks_like_target(t), "{t}");
        }
    }

    #[test]
    fn the_command_is_found_after_igel_s_global_options() {
        let args = os(&[
            "igl",
            "--color",
            "never",
            "--config=x=1",
            "cross",
            "targets",
        ]);
        assert_eq!(command_position(&args), Some(4));
        let g = Globals::split(&args[1..4]);
        assert_eq!(g.color, os(&["--color", "never"]));
        assert_eq!(g.config, os(&["--config=x=1"]));
        assert_eq!(command_position(&os(&["igl", "-q", "build"])), None);
    }

    #[test]
    fn target_tables_come_from_igel_toml() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Igel.toml");
        std::fs::write(
            &file,
            "[package]\nname = \"a\"\n\n[cross]\nengine = \"container\"\n\n\
             [cross.target.aarch64-unknown-linux-gnu]\nimage = \"mine:1\"\n\
             pre-build = [\"apt-get install -y libssl-dev:arm64\"]\nenv = { A = \"1\" }\n",
        )
        .unwrap();
        let tables = target_tables_in(&file).unwrap();
        assert_eq!(tables.len(), 1);
        let (triple, t) = &tables[0];
        assert_eq!(triple, "aarch64-unknown-linux-gnu");
        assert_eq!(t.image.as_deref(), Some("mine:1"));
        assert_eq!(t.pre_build.len(), 1);
        assert_eq!(t.env.get("A").map(String::as_str), Some("1"));

        let mut cfg = Config::default();
        let s = CrossSettings {
            engine: Some("container".into()),
            idle_timeout_mins: Some(5),
            ..CrossSettings::default()
        };
        apply(&mut cfg, &s, "hyprlnk", &tables);
        assert_eq!(cfg.build.engine, "container");
        assert_eq!(cfg.build.idle_timeout_mins, 5);
        assert_eq!(
            cfg.build.image_tag, "latest",
            "unset keeps hyprcross' default"
        );
        assert_eq!(
            cfg.get_target("aarch64-unknown-linux-gnu")
                .and_then(|t| t.image.as_deref()),
            Some("mine:1")
        );
    }
}
