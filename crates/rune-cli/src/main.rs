//! The Rune command-line interface.

mod affected;
mod args;
mod cmd_audit;
mod cmd_build;
mod cmd_coverage;
mod cmd_deps;
mod cmd_explain;
mod cmd_graph;
mod cmd_install;
mod cmd_misc;
mod cmd_publish;
mod cmd_sbom;
mod cmd_self;
mod cmd_surface;
mod cmd_test;
mod cmd_unused;
mod cmd_upgrade;
mod cmd_vendor;
mod cmd_watch;
mod doctor;
mod external;
mod fix;
mod graph;
mod help;
mod messages;
mod pager;
mod script;
mod session;
mod test_cache;
mod test_progress;
mod test_report;
mod ui;
mod workspace;

use clap::Parser;

/// Most of what Rune does between compilers is small allocations (index
/// records, keys, paths) on many threads, where glibc's allocator spent
/// about 40 % of an offline resolution.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
use std::ffi::OsString;

use args::{Cli, Commands};
use graph::Mode;

fn main() {
    // `rune tree | head`: when the reader goes away, stop quietly, as Cargo
    // and every Unix tool do, instead of panicking on the next write.
    #[cfg(unix)]
    // SAFETY: called first thing, before any other thread exists.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    // The sandbox helper (`build.sandbox`): confine, then run the program.
    if std::env::args_os().nth(1).as_deref()
        == Some(std::ffi::OsStr::new(rune_common::sandbox::HELPER_ARG))
    {
        let rest: Vec<OsString> = std::env::args_os().skip(2).collect();
        rune_common::sandbox::helper_main(&rest);
    }
    // llvm-cov's demangler (`rune coverage --html`): mangled names in, one
    // per line; readable names out.
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("__demangle")) {
        cmd_coverage::demangle_stdin();
        return;
    }
    ui::setup_logging();
    install_panic_hook();

    let args = external::expand(std::env::args_os().collect());
    apply_color_flag(&args);
    if help::intercept(&args) {
        return;
    }
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(e) if e.kind() == clap::error::ErrorKind::DisplayHelp => {
            help::show_clap(&e);
            return;
        }
        Err(e) => e.exit(),
    };
    rune_common::cargo_config::set_cli_overrides(cli.config_overrides.clone());

    let result = match cli.command {
        Commands::Build(a) => cmd_build::build(a, Mode::Build),
        Commands::Check(a) => cmd_build::build(a, Mode::Check),
        Commands::Test(a) => cmd_test::run(a, Mode::Test),
        Commands::Bench(a) => cmd_test::run(a, Mode::Bench),
        Commands::Run(a) => cmd_build::run(a),
        Commands::Fmt(a) => cmd_misc::fmt(a),
        Commands::Clippy(a) => cmd_build::clippy(a),
        Commands::Doc(a) => cmd_build::doc(a),
        Commands::Rustc(a) => cmd_build::rustc(a),
        Commands::Rustdoc(a) => cmd_build::rustdoc(a),
        Commands::Fix(a) => cmd_build::fix(a),
        Commands::Explain(a) => cmd_explain::explain(a),
        Commands::New(a) => cmd_misc::new(a),
        Commands::Init(a) => cmd_misc::init(a),
        Commands::Add(a) => cmd_deps::add(a),
        Commands::Remove(a) => cmd_deps::remove(a),
        Commands::Update(a) => cmd_deps::update(a),
        Commands::Outdated(a) => cmd_upgrade::outdated(a),
        Commands::Upgrade(a) => cmd_upgrade::upgrade(a),
        Commands::Unused(a) => cmd_unused::unused(a),
        Commands::Sbom(a) => cmd_sbom::sbom(a),
        Commands::Audit(a) => cmd_audit::audit(a),
        Commands::Coverage(a) => cmd_coverage::coverage(*a),
        Commands::Tree(a) => cmd_deps::tree(a),
        Commands::Graph(a) => cmd_graph::graph(a),
        Commands::Why(a) => cmd_deps::why(a),
        Commands::Features(a) => cmd_deps::features(a),
        Commands::Fetch(a) => cmd_deps::fetch(a),
        Commands::GenerateLockfile(a) => cmd_deps::generate_lockfile(a),
        Commands::LocateProject(a) => cmd_deps::locate_project(a),
        Commands::Metadata(a) => cmd_deps::metadata(a),
        Commands::Install(a) => cmd_install::install(a),
        Commands::Uninstall(a) => cmd_install::uninstall(a),
        Commands::Package(a) => cmd_publish::package(a),
        Commands::Publish(a) => cmd_publish::publish(a),
        Commands::Login(a) => cmd_publish::login(a),
        Commands::Logout(a) => cmd_publish::logout(a),
        Commands::Yank(a) => cmd_publish::yank(a),
        Commands::Owner(a) => cmd_publish::owner(a),
        Commands::Search(a) => cmd_publish::search(a),
        Commands::Info(a) => cmd_publish::info(a),
        Commands::Vendor(a) => cmd_vendor::vendor(a),
        Commands::Pkgid(a) => cmd_deps::pkgid(a),
        Commands::VerifyProject(a) => cmd_deps::verify_project(a),
        Commands::Clean(a) => cmd_misc::clean(a),
        Commands::Config(c) => cmd_misc::config(c),
        Commands::Cas(args::CasCommands::Export(a)) => cmd_build::cas_export(*a),
        Commands::Cas(args::CasCommands::Pin(a)) => cmd_build::cas_pin(*a),
        Commands::Cas(c) => cmd_misc::cas(c),
        Commands::Version { verbose } => cmd_misc::version(verbose),
        Commands::Completions { shell } => cmd_misc::completions(shell),
        Commands::Doctor => cmd_misc::doctor(),
        Commands::Watch(a) => cmd_watch::watch(a),
        Commands::Affected(a) => cmd_misc::affected(a),
        Commands::Script(a) => cmd_misc::script(a),
        Commands::Lockfile(c) => cmd_misc::lockfile(c),
        Commands::Self_(args::SelfCommands::Update(a)) => cmd_self::update(a),
        Commands::Self_(args::SelfCommands::Uninstall(a)) => cmd_self::uninstall(a),
        Commands::SelfUpdate(a) => cmd_self::update(a),
    };

    if let Err(err) = result {
        ui::error(render_error(&err));
        if let Some(hint) = hint_for(&err) {
            ui::hint(hint);
        }
        std::process::exit(1);
    }
}

/// An error and its causes, each once: errors that already include their
/// cause in their own message (a TOML error naming the file and the parse
/// error) would otherwise print it twice. A cause spanning several lines
/// (a compiler's or parser's report) starts on a line of its own.
fn render_error(err: &anyhow::Error) -> String {
    let mut parts: Vec<String> = Vec::new();
    for cause in err.chain() {
        let text = cause.to_string();
        let text = text.trim();
        if text.is_empty() || parts.iter().any(|p| p.contains(text)) {
            continue;
        }
        parts.push(text.to_string());
    }
    let mut out = String::new();
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            out.push_str(if part.contains('\n') { ":\n" } else { ": " });
        }
        out.push_str(part);
    }
    out
}

/// What to do about an error the machine caused, not the project.
fn hint_for(err: &anyhow::Error) -> Option<String> {
    for cause in err.chain() {
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            if io.kind() == std::io::ErrorKind::StorageFull || io.raw_os_error() == Some(28) {
                return Some(disk_full_hint());
            }
            if io.kind() == std::io::ErrorKind::PermissionDenied {
                return Some(
                    "a file or directory Rune needs is not writable: check the ownership of \
                     the project's `.rune/` and of `~/.rune` (or `RUNE_HOME`)"
                        .to_string(),
                );
            }
        }
    }
    let text = format!("{err:#}");
    (text.contains("No space left on device") || text.contains("os error 28")).then(disk_full_hint)
}

fn disk_full_hint() -> String {
    "the disk is full: `rune cas gc --max-size 5GB` shrinks the shared store, `rune clean` \
     removes this project's build output"
        .to_string()
}

/// A panic is a bug in Rune: say so plainly, with what to report, instead of
/// a bare panic message under a half-drawn progress line.
fn install_panic_hook() {
    use colored::Colorize;
    static REPORTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::env::var_os("RUST_BACKTRACE").is_some() {
            default(info);
        }
        // A worker's panic resurfaces where its scope ends: report it once.
        if REPORTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let _ = ui::multi_progress().clear();
        let what = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".to_string());
        let place = info
            .location()
            .map(|l| format!(" at {}:{}", l.file(), l.line()))
            .unwrap_or_default();
        let args: Vec<String> = std::env::args().skip(1).collect();
        eprintln!(
            "{} rune {} crashed: {what}{place}\n  \
             This is a bug in Rune, not in your project. Please report it at\n  \
             https://github.com/runepm/rune/issues with the command (`rune {}`) and, if you \
             can,\n  the output of the same command with RUST_BACKTRACE=1.",
            "✗".red().bold(),
            env!("CARGO_PKG_VERSION"),
            args.join(" ")
        );
    }));
}

/// `--color` takes effect before anything is printed, help included.
fn apply_color_flag(args: &[OsString]) {
    let mut words = args.iter().filter_map(|a| a.to_str());
    let mut choice = None;
    while let Some(w) = words.next() {
        if w == "--" {
            break;
        }
        if let Some(v) = w.strip_prefix("--color=") {
            choice = Some(v.to_string());
        } else if w == "--color" {
            choice = words.next().map(str::to_string);
        }
    }
    match choice.as_deref() {
        Some("always") => {
            colored::control::set_override(true);
            console::set_colors_enabled(true);
            console::set_colors_enabled_stderr(true);
        }
        Some("never") => {
            colored::control::set_override(false);
            console::set_colors_enabled(false);
            console::set_colors_enabled_stderr(false);
        }
        _ => {}
    }
}
