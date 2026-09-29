//! `rune help`: a short, grouped overview of the everyday commands, the
//! complete list with `--all`, and one command's options with
//! `rune help <command>` (the same as `rune <command> --help`). Long help
//! opens in the pager on a terminal.

use crate::args::Cli;
use crate::pager::Page;
use crate::ui;
use clap::{CommandFactory, Parser};
use colored::Colorize;

/// The commands `rune help` shows, by section.
const EVERYDAY: &[(&str, &[&str])] = &[
    ("Build", &["build", "check", "run", "test", "watch"]),
    (
        "Dependencies",
        &["add", "remove", "update", "outdated", "tree"],
    ),
    ("Project", &["new", "fmt", "clippy", "doc", "clean"]),
    ("Registry", &["search", "info", "install", "publish"]),
    ("Rune", &["doctor", "cas", "config"]),
];

/// Every command, by section (`rune help --all`). A command missing here
/// still shows, under "Other".
const ALL: &[(&str, &[&str])] = &[
    (
        "Build and run",
        &[
            "build", "check", "run", "test", "bench", "watch", "clean", "explain",
        ],
    ),
    ("Code", &["fmt", "clippy", "fix", "doc"]),
    (
        "Dependencies",
        &[
            "add", "remove", "update", "outdated", "upgrade", "unused", "tree", "why", "features",
            "graph", "fetch", "vendor",
        ],
    ),
    (
        "Packages",
        &[
            "new", "init", "package", "publish", "yank", "owner", "login", "logout",
        ],
    ),
    ("Registry", &["search", "info", "install", "uninstall"]),
    (
        "Rune",
        &[
            "doctor",
            "cas",
            "config",
            "affected",
            "script",
            "version",
            "completions",
        ],
    ),
    (
        "Tools and Cargo compatibility",
        &[
            "rustc",
            "rustdoc",
            "metadata",
            "sbom",
            "generate-lockfile",
            "lockfile",
            "locate-project",
            "pkgid",
            "verify-project",
        ],
    ),
];

/// One-line descriptions for the overview (a command's own `about` is
/// often longer than fits).
fn short(name: &str) -> Option<&'static str> {
    Some(match name {
        "build" => "Compile the package and its dependencies",
        "check" => "Type-check without producing binaries",
        "run" => "Build and run a binary or example",
        "test" => "Build and run the tests",
        "bench" => "Build and run the benchmarks",
        "watch" => "Re-run a command whenever files change",
        "clean" => "Remove build output",
        "explain" => "Show what a build would compile, and why",
        "fmt" => "Format the code with rustfmt",
        "clippy" => "Lint the code with clippy",
        "fix" => "Apply the compiler's suggested fixes",
        "doc" => "Build the documentation",
        "add" => "Add a dependency",
        "remove" => "Remove a dependency",
        "update" => "Update locked versions",
        "outdated" => "List dependencies with newer releases",
        "upgrade" => "Raise requirements to newer releases",
        "unused" => "Find dependencies nothing uses",
        "tree" => "Show the dependency tree",
        "why" => "Explain why a package is in the graph",
        "features" => "Which features are on, and who turned them on",
        "graph" => "Draw the package graph (DOT, Mermaid, JSON)",
        "fetch" => "Download every dependency",
        "vendor" => "Copy the dependencies into the project",
        "new" => "Create a package",
        "init" => "Create a package in this directory",
        "package" => "Build the .crate file to publish",
        "publish" => "Publish the package",
        "yank" => "Yank (or restore) a published version",
        "owner" => "Manage a crate's owners",
        "login" => "Save a registry token",
        "logout" => "Remove a registry token",
        "search" => "Search crates.io",
        "info" => "Show a crate's details and features",
        "install" => "Install a program from crates.io",
        "uninstall" => "Remove an installed program",
        "doctor" => "Check what slows your builds down",
        "cas" => "Inspect or clean the build cache",
        "config" => "View and change settings",
        "affected" => "Packages a change can affect (for CI)",
        "script" => "Run a single-file Rust script",
        "version" => "Show version information",
        "completions" => "Print a shell completion script",
        "rustc" => "Build, passing extra flags to rustc",
        "rustdoc" => "Document, passing extra flags to rustdoc",
        "metadata" => "Workspace and dependency metadata as JSON",
        "sbom" => "Write a software bill of materials",
        "generate-lockfile" => "Resolve and write the lockfile",
        "lockfile" => "Export or migrate lockfiles",
        "locate-project" => "Print the manifest's path as JSON",
        "pkgid" => "Print a package's full specification",
        "verify-project" => "Check that the manifest is well-formed",
        _ => return None,
    })
}

/// Handles the ways of asking for Rune's own help — `rune`, `rune -h`,
/// `rune --help`, `rune help [--all | COMMAND…]`, `rune --list` — and
/// returns `true` when it did. Anything else is left to the parser.
pub fn intercept(args: &[std::ffi::OsString]) -> bool {
    let words: Vec<String> = args
        .iter()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    // Global options may come first.
    let mut rest: &[String] = &words;
    loop {
        match rest.first().map(String::as_str) {
            Some("--color") | Some("--config") if rest.len() >= 2 => rest = &rest[2..],
            Some(a) if a.starts_with("--color=") || a.starts_with("--config=") => rest = &rest[1..],
            _ => break,
        }
    }
    let rest: Vec<&str> = rest.iter().map(String::as_str).collect();
    match rest.as_slice() {
        [] | ["-h"] | ["--help"] | ["help"] => overview(false),
        ["help", "--all"] | ["help", "-a"] | ["--help", "--all"] | ["--list"] => overview(true),
        ["help", command @ ..] => {
            command_help(command);
        }
        _ => return false,
    }
    true
}

/// Shows a command's help, as clap renders it for `rune <command> --help`.
fn command_help(path: &[&str]) {
    let mut argv = vec!["rune"];
    argv.extend_from_slice(path);
    argv.push("--help");
    match Cli::try_parse_from(argv) {
        Err(e) if e.kind() == clap::error::ErrorKind::DisplayHelp => show_clap(&e),
        _ => {
            ui::error(format!(
                "no such command: `{}` (see `rune help --all`)",
                path.join(" ")
            ));
            std::process::exit(1);
        }
    }
}

/// Shows a help message clap produced (for `--help`), in the pager.
pub fn show_clap(e: &clap::Error) {
    let text = if ui::colors() {
        e.render().ansi().to_string()
    } else {
        e.render().to_string()
    };
    let mut page = Page::new("rune help");
    page.line(text.trim_end());
    page.show();
}

/// `rune help` (`all`: every command).
fn overview(all: bool) {
    let cmd = Cli::command();
    let about = |name: &str| -> String {
        short(name).map(str::to_string).unwrap_or_else(|| {
            cmd.find_subcommand(name)
                .and_then(|c| c.get_about())
                .map(|a| a.to_string())
                .unwrap_or_default()
        })
    };
    let label = |name: &str| -> (String, usize) {
        let aliases: Vec<&str> = cmd
            .find_subcommand(name)
            .map(|c| c.get_visible_aliases().collect())
            .unwrap_or_default();
        let plain = std::iter::once(name)
            .chain(aliases.iter().copied())
            .collect::<Vec<_>>()
            .join(", ");
        let styled = std::iter::once(ui::brand(name))
            .chain(aliases.iter().map(|a| a.dimmed().to_string()))
            .collect::<Vec<_>>()
            .join(&", ".dimmed().to_string());
        (styled, plain.chars().count())
    };

    let mut sections: Vec<(&str, Vec<&str>)> = if all { ALL } else { EVERYDAY }
        .iter()
        .map(|(title, names)| (*title, names.to_vec()))
        .collect();
    if all {
        let listed: Vec<&str> = ALL.iter().flat_map(|(_, n)| n.iter().copied()).collect();
        let other: Vec<&str> = cmd
            .get_subcommands()
            .filter(|c| !c.is_hide_set())
            .map(|c| c.get_name())
            .filter(|n| !listed.contains(n))
            .collect();
        if !other.is_empty() {
            sections.push(("Other", other));
        }
    }
    let total = cmd.get_subcommands().filter(|c| !c.is_hide_set()).count();
    let width = sections
        .iter()
        .flat_map(|(_, names)| names.iter())
        .map(|n| label(n).1)
        .max()
        .unwrap_or(10)
        + 3;

    let mut page = Page::new(if all { "rune help --all" } else { "rune help" });
    page.line(format!(
        "{} {}  {}",
        ui::brand("rune"),
        env!("CARGO_PKG_VERSION").dimmed(),
        "fast Rust builds, one shared build cache".dimmed()
    ));
    page.blank();
    page.line(format!(
        "{}  {} {}",
        "Usage".bold(),
        "rune".bold(),
        "<command> [options]".dimmed()
    ));
    for (title, names) in &sections {
        page.blank();
        page.line(title.bold().to_string());
        for name in names {
            if cmd.find_subcommand(name).is_none() {
                continue;
            }
            let (styled, w) = label(name);
            page.line(format!(
                "  {styled}{:pad$}{}",
                "",
                about(name),
                pad = width.saturating_sub(w)
            ));
        }
    }
    page.blank();
    page.line("Options".bold().to_string());
    for (flag, what) in [
        ("-h, --help", "Help for rune, or for a command"),
        ("-V, --version", "Print the version"),
        ("--color <when>", "auto, always or never"),
        ("--config <key=value>", "Change a setting for this run"),
    ] {
        page.line(format!(
            "  {}{:pad$}{what}",
            flag,
            "",
            pad = 24 - flag.len()
        ));
    }
    page.blank();
    page.line(format!(
        "{}  {}",
        "rune help <command>".bold(),
        "options and examples for one command".dimmed()
    ));
    if !all {
        page.line(format!(
            "{}      {}",
            "rune help --all".bold(),
            format!("all {total} commands").dimmed()
        ));
    }
    page.show();
}

/// Rune's colours for clap's help and error messages.
pub fn styles() -> clap::builder::Styles {
    use clap::builder::styling::{Ansi256Color, AnsiColor, Color, Style};
    let violet = Some(Color::Ansi256(Ansi256Color(141)));
    clap::builder::Styles::styled()
        .header(Style::new().bold().fg_color(violet))
        .usage(Style::new().bold().fg_color(violet))
        .literal(Style::new().bold())
        .placeholder(Style::new().dimmed())
        .valid(Style::new().fg_color(Some(Color::Ansi(AnsiColor::Green))))
        .invalid(Style::new().fg_color(Some(Color::Ansi(AnsiColor::Yellow))))
        .error(
            Style::new()
                .bold()
                .fg_color(Some(Color::Ansi(AnsiColor::Red))),
        )
}

/// An "Examples" section for a command's help: `(command line, what it does)`.
pub fn examples(rows: &[(&str, &str)]) -> String {
    use clap::builder::styling::{Ansi256Color, Color, Style};
    let header = Style::new()
        .bold()
        .fg_color(Some(Color::Ansi256(Ansi256Color(141))));
    let literal = Style::new().bold();
    let width = rows.iter().map(|(c, _)| c.len()).max().unwrap_or(0) + 3;
    let mut out = format!("{}Examples:{}\n", header.render(), header.render_reset());
    for (cmd, what) in rows {
        out.push_str(&format!(
            "  {}{cmd}{}{:pad$}{what}\n",
            literal.render(),
            literal.render_reset(),
            "",
            pad = width - cmd.len()
        ));
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_command_exists_and_everyday_ones_have_short_descriptions() {
        let cmd = Cli::command();
        for (_, names) in ALL.iter().chain(EVERYDAY) {
            for name in *names {
                assert!(cmd.find_subcommand(name).is_some(), "{name}");
            }
        }
        for (_, names) in EVERYDAY {
            for name in *names {
                assert!(short(name).is_some(), "{name}");
            }
        }
        // `--all` lists everything (anything unlisted goes to "Other", but
        // nothing should need to).
        let listed: Vec<&str> = ALL.iter().flat_map(|(_, n)| n.iter().copied()).collect();
        for c in cmd.get_subcommands().filter(|c| !c.is_hide_set()) {
            assert!(listed.contains(&c.get_name()), "{}", c.get_name());
        }
    }
}
