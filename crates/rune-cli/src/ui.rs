//! Shared output styling for the Rune CLI.
//!
//! Deliberately does not mirror Cargo's layout (a right-aligned, bold-green,
//! Capitalized verb column). Rune has its own identity: a violet brand
//! accent, a left-aligned glyph marker instead of column padding, and
//! lowercase verbs. No emoji — every marker below is a plain text symbol
//! that renders as a single monochrome glyph in any terminal font.

use colored::*;
use indicatif::MultiProgress;
use std::fmt::Display;
use std::io::{self, Write};
use std::sync::OnceLock;
use tracing_subscriber::{EnvFilter, fmt};

/// When set, stdout carries machine-readable output only (`--message-format
/// json`), so the human-readable action and summary lines go to stderr.
static MACHINE_OUTPUT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Reserves stdout for machine-readable output.
pub fn set_machine_output(on: bool) {
    MACHINE_OUTPUT.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// `--quiet`: no action or summary lines at all.
static QUIET: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_quiet(on: bool) {
    QUIET.store(on, std::sync::atomic::Ordering::Relaxed);
}

fn quiet() -> bool {
    QUIET.load(std::sync::atomic::Ordering::Relaxed)
}

fn machine_output() -> bool {
    MACHINE_OUTPUT.load(std::sync::atomic::Ordering::Relaxed)
}

/// The `MultiProgress` shared between the active build's progress bars and
/// the tracing writer below, so log lines never tear up the bars.
static MULTI_PROGRESS: OnceLock<MultiProgress> = OnceLock::new();

/// Returns the process-wide `MultiProgress`, creating it on first use.
pub fn multi_progress() -> MultiProgress {
    MULTI_PROGRESS.get_or_init(MultiProgress::new).clone()
}

/// Prints text to stderr above any active progress bars without tearing them.
pub fn print_above_progress(text: &str) {
    multi_progress().suspend(|| eprint!("{text}"));
}

/// A `tracing` writer that suspends the shared `MultiProgress` for the
/// duration of each write, so log lines are printed cleanly above any active
/// progress bars instead of corrupting their redraw.
#[derive(Clone)]
struct ProgressAwareWriter(MultiProgress);

impl Write for ProgressAwareWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.suspend(|| io::stderr().write_all(buf))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stderr().flush()
    }
}

impl<'a> fmt::MakeWriter<'a> for ProgressAwareWriter {
    type Writer = ProgressAwareWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Installs the global tracing subscriber (`RUST_LOG` controls verbosity).
pub fn setup_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(ProgressAwareWriter(multi_progress()))
        .init();
}

/// Rune's brand accent (a soft violet), used for in-progress and completed
/// actions in place of Cargo's green.
fn brand(s: &str) -> ColoredString {
    s.truecolor(167, 139, 250).bold()
}

/// The marker printed at the start of every action line.
fn marker() -> ColoredString {
    "▸".truecolor(167, 139, 250).bold()
}

/// A notable action, in progress or completed (e.g. `resolving`, `finished`,
/// `added`). Lowercase by convention — set the verb lowercase at the call
/// site.
pub fn action(verb: &str, message: impl Display) {
    if quiet() {
        return;
    }
    if machine_output() {
        eprintln!("{} {} {}", marker(), brand(verb), message);
    } else {
        println!("{} {} {}", marker(), brand(verb), message);
    }
}

/// The final summary line for a completed build.
pub fn summary(message: impl Display) {
    if quiet() {
        return;
    }
    if machine_output() {
        eprintln!("{} {}", "✓".green().bold(), message);
    } else {
        println!("{} {}", "✓".green().bold(), message);
    }
}

/// A non-fatal warning.
pub fn warn(message: impl Display) {
    eprintln!("{} {}", "!".yellow().bold(), message.to_string().yellow());
}

/// A fatal error, printed the way the process is about to exit.
pub fn error(message: impl Display) {
    eprintln!("{} {}", "✗".red().bold(), message);
}

/// A dimmed, secondary detail printed under an action or summary line.
pub fn detail(message: impl Display) {
    println!("  {}", message.to_string().dimmed());
}

/// A section heading for multi-line output (e.g. CAS stats).
pub fn heading(title: &str) {
    println!("{}", title.truecolor(167, 139, 250).bold());
}
