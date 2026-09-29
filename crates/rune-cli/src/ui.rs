//! Shared output styling for the Rune CLI.
//!
//! Deliberately does not mirror Cargo's layout (a right-aligned, bold-green,
//! Capitalized verb column). Rune has its own identity: a violet brand
//! accent, a left-aligned glyph marker instead of column padding, and
//! lowercase verbs. No emoji — every marker below is a plain text symbol
//! that renders as a single monochrome glyph in any terminal font.
//!
//! Every line goes through [`emit`]: it suspends the live progress display
//! while it writes, so status lines printed from any thread (test results
//! arriving while crates still compile, warnings, log lines) land above the
//! progress line instead of tearing it.

use colored::*;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::fmt::Display;
use std::io::{self, IsTerminal, Write};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tracing_subscriber::{EnvFilter, fmt};

/// `--quiet`: no action or summary lines at all.
static QUIET: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_quiet(on: bool) {
    QUIET.store(on, std::sync::atomic::Ordering::Relaxed);
}

fn quiet() -> bool {
    QUIET.load(std::sync::atomic::Ordering::Relaxed)
}

/// The `MultiProgress` shared between the active progress display and every
/// line printed, so output never tears the progress line.
static MULTI_PROGRESS: OnceLock<MultiProgress> = OnceLock::new();

/// Returns the process-wide `MultiProgress`, creating it on first use.
pub fn multi_progress() -> MultiProgress {
    MULTI_PROGRESS.get_or_init(MultiProgress::new).clone()
}

/// Serializes whole lines between threads, so two lines never interleave.
static OUTPUT: Mutex<()> = Mutex::new(());

/// Which stream a line goes to.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Out,
    Err,
}

/// Writes `text` (one or more complete lines, each ending in `\n`) with the
/// progress display suspended.
pub fn emit(stream: Stream, text: &str) {
    let _guard = OUTPUT.lock().unwrap_or_else(|e| e.into_inner());
    multi_progress().suspend(|| {
        let _ = match stream {
            Stream::Out => {
                let mut out = io::stdout().lock();
                out.write_all(text.as_bytes()).and_then(|_| out.flush())
            }
            Stream::Err => {
                let mut err = io::stderr().lock();
                err.write_all(text.as_bytes()).and_then(|_| err.flush())
            }
        };
    });
}

/// A line on stdout (what a command produces), above any progress display.
pub fn out(line: impl Display) {
    emit(Stream::Out, &format!("{line}\n"));
}

/// A line on stderr (status), above any progress display.
pub fn err(line: impl Display) {
    emit(Stream::Err, &format!("{line}\n"));
}

/// Prints text to stderr above any active progress display without tearing it.
pub fn print_above_progress(text: &str) {
    if text.ends_with('\n') {
        emit(Stream::Err, text);
    } else {
        emit(Stream::Err, &format!("{text}\n"));
    }
}

/// A `tracing` writer that goes through [`emit`], so log lines are printed
/// cleanly above the progress display instead of corrupting its redraw.
#[derive(Clone)]
struct ProgressAwareWriter;

impl Write for ProgressAwareWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        emit(Stream::Err, &String::from_utf8_lossy(buf));
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

/// Installs the global tracing subscriber (`RUST_LOG` controls verbosity)
/// and the network progress display.
pub fn setup_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(ProgressAwareWriter)
        .init();
    rune_common::progress::set_sink(Box::new(NetProgress::default()));
}

// ---------------------------------------------------------------------------
// Colour
// ---------------------------------------------------------------------------

/// Whether the terminal renders 24-bit colour (`COLORTERM`). Without it the
/// brand violet is sent as its 256-colour neighbour rather than let
/// `colored` round it to the nearest of the 16 basic colours (blue).
fn truecolor() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("COLORTERM").is_ok_and(|v| v.contains("truecolor") || v.contains("24bit"))
    })
}

/// Whether styled output is on (`--color`, `NO_COLOR`, a terminal).
pub fn colors() -> bool {
    colored::control::SHOULD_COLORIZE.should_colorize()
}

/// `s` in Rune's brand accent (a soft violet), bold.
pub fn brand(s: impl Display) -> String {
    paint(s, true)
}

fn paint(s: impl Display, bold: bool) -> String {
    if !colors() {
        return s.to_string();
    }
    let weight = if bold { "1;" } else { "" };
    if truecolor() {
        format!("\x1b[{weight}38;2;167;139;250m{s}\x1b[0m")
    } else {
        format!("\x1b[{weight}38;5;141m{s}\x1b[0m")
    }
}

/// The marker printed at the start of every action line.
fn marker() -> String {
    brand("▸")
}

/// The display width of `s`, ignoring colour codes.
pub fn width_of(s: &str) -> usize {
    console::measure_text_width(s)
}

/// The width of the terminal on stderr (80 when it is not a terminal).
pub fn term_width() -> usize {
    console::Term::stderr()
        .size_checked()
        .map(|(_, w)| w as usize)
        .or_else(|| {
            console::Term::stdout()
                .size_checked()
                .map(|(_, w)| w as usize)
        })
        .unwrap_or(80)
        .max(20)
}

/// `text` wrapped to `width` columns (at spaces), as lines.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(10);
    let mut lines = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        for word in para.split(' ') {
            if !line.is_empty() && width_of(&line) + 1 + width_of(word) > width {
                lines.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        lines.push(line);
    }
    lines
}

// ---------------------------------------------------------------------------
// Status lines
// ---------------------------------------------------------------------------

/// A notable action, in progress or completed (e.g. `resolving`, `finished`,
/// `added`). Lowercase by convention — set the verb lowercase at the call
/// site. Status goes to stderr, as Cargo's does: stdout is for what a command
/// produces (`metadata`, `vendor`'s configuration, the program `run` runs),
/// so that it can be piped and redirected.
pub fn action(verb: &str, message: impl Display) {
    if quiet() {
        return;
    }
    err(format!("{} {} {}", marker(), brand(verb), message));
}

/// The final summary line for a completed command (on stderr, see [`action`]).
pub fn summary(message: impl Display) {
    if quiet() {
        return;
    }
    err(format!("{} {}", "✓".green().bold(), message));
}

/// Advice after a summary: something the user may want to know or change,
/// not a problem (on stderr, like every status line).
pub fn hint(message: impl Display) {
    if quiet() {
        return;
    }
    // Wrapped at word boundaries, continuation lines under the text.
    let text = message.to_string();
    let lines = if io::stderr().is_terminal() {
        wrap(&text, term_width().saturating_sub(9).max(30))
    } else {
        vec![text]
    };
    for (i, l) in lines.iter().enumerate() {
        if i == 0 {
            err(format!("  {} {}", "note:".dimmed(), l.dimmed()));
        } else {
            err(format!("        {}", l.dimmed()));
        }
    }
}

/// An indented, dimmed line under a status line (on stderr).
pub fn hint_line(message: impl Display) {
    if quiet() {
        return;
    }
    err(format!("    {}", message.to_string().dimmed()));
}

/// A non-fatal warning.
pub fn warn(message: impl Display) {
    err(format!(
        "{} {}",
        "!".yellow().bold(),
        message.to_string().yellow()
    ));
}

/// A fatal error, printed the way the process is about to exit.
pub fn error(message: impl Display) {
    err(format!("{} {}", "✗".red().bold(), message));
}

/// A dimmed, secondary detail printed under an action or summary line.
pub fn detail(message: impl Display) {
    out(format!("  {}", message.to_string().dimmed()));
}

/// A section heading for multi-line output (e.g. CAS stats).
pub fn heading(title: &str) {
    out(brand(title));
}

/// `key  value` rows with the keys aligned, for a card of facts about one
/// thing (`rune info`, `rune cas stats`). Keys are dimmed.
pub fn kv_lines(rows: &[(&str, String)], indent: usize) -> Vec<String> {
    let key_width = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    rows.iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| format!("{:indent$}{}  {v}", "", format!("{k:<key_width$}").dimmed()))
        .collect()
}

// ---------------------------------------------------------------------------
// Activity: one live status line for a phase
// ---------------------------------------------------------------------------

fn spinner_style() -> ProgressStyle {
    ProgressStyle::default_spinner()
        .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ")
        .template("{spinner:.141} {prefix:.141.bold} {wide_msg:.dim}")
        .expect("valid template")
}

/// A phase in progress (resolving, fetching an index), shown as one spinner
/// line on a terminal and nothing otherwise. Network progress reported while
/// it is active is shown on its line.
pub struct Activity {
    bar: Option<ProgressBar>,
    started: Instant,
}

/// The activity whose line index fetches are reported on.
static CURRENT: Mutex<Option<ProgressBar>> = Mutex::new(None);
/// Index fetches since the current activity started.
static FETCHED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

impl Activity {
    /// Starts showing `verb message` with a spinner.
    pub fn start(verb: &str, message: impl Display) -> Self {
        FETCHED.store(0, std::sync::atomic::Ordering::Relaxed);
        let bar = (!quiet()).then(|| {
            let bar = multi_progress().add(ProgressBar::new_spinner());
            bar.set_style(spinner_style());
            bar.set_prefix(verb.to_string());
            bar.set_message(message.to_string());
            bar.enable_steady_tick(Duration::from_millis(80));
            bar
        });
        if let Some(b) = &bar {
            *CURRENT.lock().unwrap_or_else(|e| e.into_inner()) = Some(b.clone());
        }
        Self {
            bar,
            started: Instant::now(),
        }
    }

    /// How long the activity has run.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Index files fetched over the network while the activity ran.
    pub fn fetched(&self) -> usize {
        FETCHED.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Removes the line, leaving nothing behind.
    pub fn clear(mut self) {
        self.stop();
    }

    /// Removes the line and prints `verb message` as an action line.
    pub fn finish(mut self, verb: &str, message: impl Display) {
        self.stop();
        action(verb, message);
    }

    fn stop(&mut self) {
        if let Some(b) = self.bar.take() {
            b.finish_and_clear();
            multi_progress().remove(&b);
            let mut cur = CURRENT.lock().unwrap_or_else(|e| e.into_inner());
            if cur
                .as_ref()
                .is_some_and(|c| c.is_hidden() || c.is_finished())
            {
                *cur = None;
            }
        }
    }
}

impl Drop for Activity {
    fn drop(&mut self) {
        self.stop();
    }
}

/// `path` relative to the working directory when it is below it
/// (`.rune/target/debug/app`), as it was given otherwise.
pub fn rel(path: &std::path::Path) -> String {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| path.strip_prefix(cwd).ok().map(|p| p.display().to_string()))
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| path.display().to_string())
}

/// `1.24s` / `380ms`.
pub fn secs(d: Duration) -> String {
    if d < Duration::from_secs(1) {
        format!("{}ms", d.as_millis())
    } else {
        format!("{:.2}s", d.as_secs_f64())
    }
}

/// `4.2 MiB`.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

// ---------------------------------------------------------------------------
// Network progress (index fetches, downloads)
// ---------------------------------------------------------------------------

/// Draws [`rune_common::progress`] events: index fetches on the current
/// [`Activity`]'s line, downloads on a bar of their own that turns into a
/// summary line when the batch is done.
#[derive(Default)]
struct NetProgress {
    downloads: Mutex<Downloads>,
}

#[derive(Default)]
struct Downloads {
    bar: Option<ProgressBar>,
    total: usize,
    done: usize,
    bytes: u64,
    started: Option<Instant>,
    /// In flight, in start order (shown while they download).
    active: Vec<String>,
}

impl rune_common::progress::Sink for NetProgress {
    fn event(&self, event: rune_common::progress::Event<'_>) {
        use rune_common::progress::Event;
        match event {
            Event::IndexStarted { name } => {
                if let Some(bar) = CURRENT.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                    let n = FETCHED.load(std::sync::atomic::Ordering::Relaxed);
                    bar.set_message(format!("{} fetched · {name}", plural(n, "index file")));
                }
            }
            Event::IndexFinished { .. } => {
                FETCHED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            Event::DownloadsQueued { count } => {
                if quiet() {
                    return;
                }
                let mut d = self.downloads.lock().unwrap_or_else(|e| e.into_inner());
                d.total += count;
                if d.bar.is_none() {
                    d.started = Some(Instant::now());
                    let bar = multi_progress().add(ProgressBar::new(d.total as u64));
                    bar.set_style(rune_scheduler::progress::bar_style(
                        rune_scheduler::progress::bar_width(),
                    ));
                    bar.set_prefix("downloading");
                    bar.enable_steady_tick(Duration::from_millis(80));
                    d.bar = Some(bar);
                } else if let Some(bar) = &d.bar {
                    bar.set_length(d.total as u64);
                }
            }
            Event::DownloadStarted { name, version } => {
                let mut d = self.downloads.lock().unwrap_or_else(|e| e.into_inner());
                d.active.push(format!("{name} {version}"));
                d.redraw();
            }
            Event::DownloadBytes { bytes } => {
                let mut d = self.downloads.lock().unwrap_or_else(|e| e.into_inner());
                d.bytes += bytes;
                d.redraw();
            }
            Event::DownloadFinished { name, version } => {
                let mut d = self.downloads.lock().unwrap_or_else(|e| e.into_inner());
                let label = format!("{name} {version}");
                d.active.retain(|a| *a != label);
                d.done += 1;
                if let Some(bar) = &d.bar {
                    bar.set_position(d.done as u64);
                }
                d.redraw();
                if d.done >= d.total && d.total > 0 {
                    let took = d.started.map(|s| s.elapsed()).unwrap_or_default();
                    let (n, size) = (d.done, d.bytes);
                    if let Some(bar) = d.bar.take() {
                        bar.finish_and_clear();
                        multi_progress().remove(&bar);
                    }
                    *d = Downloads::default();
                    drop(d);
                    action(
                        "downloaded",
                        format!(
                            "{} {}",
                            plural(n, "crate"),
                            format!("({}, {})", bytes(size), secs(took)).dimmed()
                        ),
                    );
                }
            }
        }
    }
}

impl Downloads {
    fn redraw(&self) {
        let Some(bar) = &self.bar else { return };
        let size = bytes(self.bytes);
        let used = 2
            + "downloading".len()
            + 1
            + rune_scheduler::progress::bar_width()
            + 1
            + format!("{}/{}", self.done, self.total).len()
            + 1
            + size.len()
            + 3;
        let names: Vec<&str> = self.active.iter().map(String::as_str).collect();
        let list =
            rune_scheduler::progress::fit_list(&names, term_width().saturating_sub(used + 1));
        bar.set_message(if list.is_empty() {
            size
        } else {
            format!("{size} · {list}")
        });
    }
}

/// `1 crate` / `3 crates`.
pub fn plural(n: usize, what: &str) -> String {
    if n == 1 {
        format!("1 {what}")
    } else if let Some(stem) = what.strip_suffix('y') {
        format!("{n} {stem}ies")
    } else if what.ends_with('s') || what.ends_with("sh") || what.ends_with("ch") {
        format!("{n} {what}es")
    } else {
        format!("{n} {what}s")
    }
}

/// Whether stdout is a terminal.
pub fn stdout_is_term() -> bool {
    io::stdout().is_terminal()
}
