//! The Rune command-line interface.

mod affected;
mod args;
mod cmd_build;
mod cmd_deps;
mod cmd_misc;
mod cmd_test;
mod cmd_watch;
mod doctor;
mod graph;
mod messages;
mod script;
mod session;
mod test_cache;
mod ui;
mod workspace;

use clap::Parser;

use args::{Cli, Commands};
use graph::Mode;

fn main() {
    ui::setup_logging();

    let cli = Cli::parse();
    match cli.color.as_str() {
        "always" => colored::control::set_override(true),
        "never" => colored::control::set_override(false),
        _ => {}
    }

    let result = match cli.command {
        Commands::Build(a) => cmd_build::build(a, Mode::Build),
        Commands::Check(a) => cmd_build::build(a, Mode::Check),
        Commands::Test(a) => cmd_test::run(a, Mode::Test),
        Commands::Bench(a) => cmd_test::run(a, Mode::Bench),
        Commands::Run(a) => cmd_build::run(a),
        Commands::Fmt(a) => cmd_misc::fmt(a),
        Commands::Clippy(a) => cmd_build::clippy(a),
        Commands::Doc(a) => cmd_build::doc(a),
        Commands::New(a) => cmd_misc::new(a),
        Commands::Init(a) => cmd_misc::init(a),
        Commands::Add(a) => cmd_deps::add(a),
        Commands::Remove(a) => cmd_deps::remove(a),
        Commands::Update(a) => cmd_deps::update(a),
        Commands::Tree(a) => cmd_deps::tree(a),
        Commands::Why(a) => cmd_deps::why(a),
        Commands::Fetch(a) => cmd_deps::fetch(a),
        Commands::GenerateLockfile(a) => cmd_deps::generate_lockfile(a),
        Commands::LocateProject(a) => cmd_deps::locate_project(a),
        Commands::Metadata(a) => cmd_deps::metadata(a),
        Commands::Install(a) => cmd_build::install(a),
        Commands::Clean(a) => cmd_misc::clean(a),
        Commands::Config(c) => cmd_misc::config(c),
        Commands::Cas(c) => cmd_misc::cas(c),
        Commands::Doctor => cmd_misc::doctor(),
        Commands::Watch(a) => cmd_watch::watch(a),
        Commands::Affected(a) => cmd_misc::affected(a),
        Commands::Script(a) => cmd_misc::script(a),
        Commands::Lockfile(c) => cmd_misc::lockfile(c),
    };

    if let Err(err) = result {
        ui::error(format!("{err:#}"));
        std::process::exit(1);
    }
}
