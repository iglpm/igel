//! `igl help`: a grouped overview, `--all`, and `igl help <command>`.

use crate::args::Cli;
use crate::pager::Page;
use crate::ui;
use clap::{CommandFactory, Parser};
use colored::Colorize;

mod logo;

const EVERYDAY: &[(&str, &[&str])] = &[
    (
        "Build",
        &["build", "check", "run", "test", "cross", "watch"],
    ),
    (
        "Dependencies",
        &["add", "remove", "update", "outdated", "tree", "audit"],
    ),
    (
        "Project",
        &["new", "fmt", "clippy", "doc", "coverage", "clean"],
    ),
    ("Registry", &["search", "info", "install", "publish"]),
    ("Igel", &["doctor", "cas", "config"]),
];

const ALL: &[(&str, &[&str])] = &[
    (
        "Build and run",
        &[
            "build", "check", "run", "test", "bench", "cross", "watch", "clean", "explain",
        ],
    ),
    ("Code", &["fmt", "clippy", "fix", "doc", "coverage"]),
    (
        "Dependencies",
        &[
            "add", "remove", "update", "outdated", "upgrade", "unused", "tree", "why", "features",
            "graph", "fetch", "vendor", "audit",
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
        "Igel",
        &[
            "doctor",
            "cas",
            "config",
            "affected",
            "script",
            "version",
            "completions",
            "self",
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

fn short(name: &str) -> Option<&'static str> {
    Some(match name {
        "build" => "Compile the package and its dependencies",
        "check" => "Type-check without producing binaries",
        "run" => "Build and run a binary or example",
        "test" => "Build and run the tests",
        "bench" => "Build and run the benchmarks",
        "cross" => "Build, test and run for other targets",
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
        "self" => "Manage Igel itself (self-update)",
        "rustc" => "Build, passing extra flags to rustc",
        "rustdoc" => "Document, passing extra flags to rustdoc",
        "metadata" => "Workspace and dependency metadata as JSON",
        "sbom" => "Write a software bill of materials",
        "audit" => "Check dependencies for security advisories",
        "coverage" => "Measure which code the tests run",
        "generate-lockfile" => "Resolve and write the lockfile",
        "lockfile" => "Export or migrate lockfiles",
        "locate-project" => "Print the manifest's path as JSON",
        "pkgid" => "Print a package's full specification",
        "verify-project" => "Check that the manifest is well-formed",
        _ => return None,
    })
}

pub fn intercept(args: &[std::ffi::OsString]) -> bool {
    let words: Vec<String> = args
        .iter()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
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

fn command_help(path: &[&str]) {
    let mut argv = vec!["igl"];
    argv.extend_from_slice(path);
    argv.push("--help");
    match Cli::try_parse_from(argv) {
        Err(e) if e.kind() == clap::error::ErrorKind::DisplayHelp => show_clap(&e),
        _ => {
            ui::error(format!(
                "no such command: `{}` (see `igl help --all`)",
                path.join(" ")
            ));
            std::process::exit(1);
        }
    }
}

pub fn show_clap(e: &clap::Error) {
    let text = if ui::colors() {
        e.render().ansi().to_string()
    } else {
        e.render().to_string()
    };
    let mut page = Page::new("igl help");
    page.line(text.trim_end());
    page.show();
}

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

    let mut page = Page::new(if all { "igl help --all" } else { "igl help" });
    let heading = format!(
        "{} {}  {}",
        ui::brand("igl"),
        env!("CARGO_PKG_VERSION").dimmed(),
        "fast Rust builds, one shared build cache".dimmed()
    );
    let usage = format!(
        "{}  {} {}",
        "Usage".bold(),
        "igl".bold(),
        "<command> [options]".dimmed()
    );
    add_heading(&mut page, &heading, &usage);
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
        ("-h, --help", "Help for igl, or for a command"),
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
        "igl help <command>".bold(),
        "options and examples for one command".dimmed()
    ));
    if !all {
        page.line(format!(
            "{}      {}",
            "igl help --all".bold(),
            format!("all {total} commands").dimmed()
        ));
    }
    page.show();
}

fn add_heading(page: &mut Page, heading: &str, usage: &str) {
    // Help goes to stdout, as does the pager. Leave room for its right edge.
    let width = console::Term::stdout()
        .size_checked()
        .map(|(_, width)| usize::from(width))
        .unwrap_or_else(ui::term_width);
    for line in heading_lines(heading, usage, width) {
        page.line(line);
    }
}

fn heading_lines(heading: &str, usage: &str, width: usize) -> Vec<String> {
    const MARGIN: usize = 4;
    const GAP: usize = 6;
    let width = width.min(120);
    let left_width = ui::width_of(heading).max(ui::width_of(usage));
    let mut lines = vec![String::new()];
    if left_width + GAP + logo::WIDTH + MARGIN <= width {
        let logo_start = width - MARGIN - logo::WIDTH;
        for (row, logo) in logo::lines().into_iter().enumerate() {
            let left = match row {
                2 => heading,
                4 => usage,
                _ => "",
            };
            let gap = logo_start - ui::width_of(left);
            lines.push(format!("{left}{:gap$}{logo}", ""));
        }
    } else {
        lines.extend([heading.to_owned(), String::new(), usage.to_owned()]);
        if logo::WIDTH + 2 * MARGIN <= width {
            lines.push(String::new());
            let indent = (width - logo::WIDTH) / 2;
            for logo in logo::lines() {
                lines.push(format!("{:indent$}{logo}", ""));
            }
        }
    }
    lines.push(String::new());
    lines
}

pub fn styles() -> clap::builder::Styles {
    use clap::builder::styling::{Ansi256Color, AnsiColor, Color, Style};
    let quill = Some(Color::Ansi256(Ansi256Color(179)));
    clap::builder::Styles::styled()
        .header(Style::new().bold().fg_color(quill))
        .usage(Style::new().bold().fg_color(quill))
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

pub fn examples(rows: &[(&str, &str)]) -> String {
    use clap::builder::styling::{Ansi256Color, Color, Style};
    let header = Style::new()
        .bold()
        .fg_color(Some(Color::Ansi256(Ansi256Color(179))));
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
    fn heading_keeps_logo_inside_terminal_with_padding() {
        let heading = "igl 2027.0.0  fast Rust builds, one shared build cache";
        let usage = "Usage  igl <command> [options]";
        for width in [36, 40, 68, 80, 92, 100, 120, 160] {
            let lines = heading_lines(heading, usage, width);
            assert!(lines.first().unwrap().is_empty());
            assert!(lines.last().unwrap().is_empty());
            for line in lines
                .iter()
                .filter(|line| !line.is_empty() && !line.contains(heading) && !line.contains(usage))
            {
                assert!(line.starts_with("    "), "left padding at {width}");
                assert!(
                    ui::width_of(line) <= width.min(120) - 4,
                    "logo clips at width {width}: {line:?}"
                );
            }
        }
        let lines = heading_lines(heading, usage, 32);
        assert_eq!(lines.len(), 5, "omit logo when margins cannot fit");
    }

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
        let listed: Vec<&str> = ALL.iter().flat_map(|(_, n)| n.iter().copied()).collect();
        for c in cmd.get_subcommands().filter(|c| !c.is_hide_set()) {
            assert!(listed.contains(&c.get_name()), "{}", c.get_name());
        }
    }
}
