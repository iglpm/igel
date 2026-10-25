//! Output styling. Every line goes through [`emit`], which suspends the progress display while writing.

use colored::*;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::fmt::Display;
use std::io::{self, IsTerminal, Write};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tracing_subscriber::{EnvFilter, fmt};

static QUIET: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_quiet(on: bool) {
    QUIET.store(on, std::sync::atomic::Ordering::Relaxed);
}

fn quiet() -> bool {
    QUIET.load(std::sync::atomic::Ordering::Relaxed)
}

static MULTI_PROGRESS: OnceLock<MultiProgress> = OnceLock::new();

pub fn multi_progress() -> MultiProgress {
    MULTI_PROGRESS.get_or_init(MultiProgress::new).clone()
}

static OUTPUT: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Out,
    Err,
}

/// Whether the last text printed ended with a blank line (see `blank_line`).
static ENDS_BLANK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn emit(stream: Stream, text: &str) {
    let _guard = OUTPUT.lock().unwrap_or_else(|e| e.into_inner());
    if !text.is_empty() {
        ENDS_BLANK.store(
            text == "\n" || text.ends_with("\n\n"),
            std::sync::atomic::Ordering::Relaxed,
        );
    }
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

pub fn out(line: impl Display) {
    emit(Stream::Out, &format!("{line}\n"));
}

pub fn blank_line() {
    if !ENDS_BLANK.load(std::sync::atomic::Ordering::Relaxed) {
        err("");
    }
}

pub fn err(line: impl Display) {
    emit(Stream::Err, &format!("{line}\n"));
}

pub fn print_above_progress(text: &str) {
    if text.ends_with('\n') {
        emit(Stream::Err, text);
    } else {
        emit(Stream::Err, &format!("{text}\n"));
    }
}

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

pub fn setup_logging() {
    match EnvFilter::try_from_default_env() {
        Ok(filter) => fmt()
            .with_env_filter(filter)
            .with_target(false)
            .with_writer(ProgressAwareWriter)
            .init(),
        Err(_) => fmt()
            .with_env_filter(EnvFilter::new("warn"))
            .with_writer(ProgressAwareWriter)
            .event_format(PlainWarnings)
            .init(),
    }
    igel_common::progress::set_sink(Box::new(NetProgress::default()));
}

struct PlainWarnings;

impl<S, N> tracing_subscriber::fmt::FormatEvent<S, N> for PlainWarnings
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    N: for<'a> tracing_subscriber::fmt::FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &tracing_subscriber::fmt::FmtContext<'_, S, N>,
        mut writer: tracing_subscriber::fmt::format::Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        let mut message = String::new();
        ctx.field_format().format_fields(
            tracing_subscriber::fmt::format::Writer::new(&mut message),
            event,
        )?;
        let error = *event.metadata().level() == tracing::Level::ERROR;
        let mut lines = message.lines();
        let first = lines.next().unwrap_or_default();
        if error {
            writeln!(writer, "{} {first}", "✗".red().bold())?;
        } else {
            writeln!(writer, "{} {}", "!".yellow().bold(), first.yellow())?;
        }
        for line in lines {
            writeln!(writer, "  {line}")?;
        }
        Ok(())
    }
}

/// Without 24-bit colour, the logo's warm quill colour uses its closest 256-colour neighbour.
fn truecolor() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("COLORTERM").is_ok_and(|v| v.contains("truecolor") || v.contains("24bit"))
    })
}

pub fn colors() -> bool {
    colored::control::SHOULD_COLORIZE.should_colorize()
}

pub fn brand(s: impl Display) -> String {
    paint(s, true)
}

fn paint(s: impl Display, bold: bool) -> String {
    if !colors() {
        return s.to_string();
    }
    let weight = if bold { "1;" } else { "" };
    if truecolor() {
        format!("\x1b[{weight}38;2;215;154;98m{s}\x1b[0m")
    } else {
        format!("\x1b[{weight}38;5;179m{s}\x1b[0m")
    }
}

fn marker() -> String {
    brand("▸")
}

pub fn width_of(s: &str) -> usize {
    console::measure_text_width(s)
}

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

pub fn action(verb: &str, message: impl Display) {
    if quiet() {
        return;
    }
    err(format!("{} {} {}", marker(), brand(verb), message));
}

pub fn summary(message: impl Display) {
    if quiet() {
        return;
    }
    err(format!("{} {}", "✓".green().bold(), message));
}

pub fn result(message: impl Display) {
    err(format!("{} {}", "✓".green().bold(), message));
}

pub fn hint(message: impl Display) {
    if quiet() {
        return;
    }
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

pub fn hint_line(message: impl Display) {
    if quiet() {
        return;
    }
    err(format!("    {}", message.to_string().dimmed()));
}

pub fn warn(message: impl Display) {
    err(format!(
        "{} {}",
        "!".yellow().bold(),
        message.to_string().yellow()
    ));
}

pub fn error(message: impl Display) {
    err(format!("{} {}", "✗".red().bold(), message));
}

pub fn detail(message: impl Display) {
    out(format!("  {}", message.to_string().dimmed()));
}

pub fn heading(title: &str) {
    out(brand(title));
}

pub fn kv_lines(rows: &[(&str, String)], indent: usize) -> Vec<String> {
    let key_width = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    rows.iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| format!("{:indent$}{}  {v}", "", format!("{k:<key_width$}").dimmed()))
        .collect()
}

fn spinner_style() -> ProgressStyle {
    ProgressStyle::default_spinner()
        .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ")
        .template("{spinner:.179} {prefix:.179.bold} {wide_msg:.dim}")
        .expect("valid template")
}

pub struct Activity {
    bar: Option<ProgressBar>,
    started: Instant,
}

static CURRENT: Mutex<Option<ProgressBar>> = Mutex::new(None);
static FETCHED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

impl Activity {
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

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    pub fn fetched(&self) -> usize {
        FETCHED.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn clear(mut self) {
        self.stop();
    }

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

pub fn rel(path: &std::path::Path) -> String {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| path.strip_prefix(cwd).ok().map(|p| p.display().to_string()))
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| path.display().to_string())
}

pub fn secs(d: Duration) -> String {
    if d < Duration::from_secs(1) {
        format!("{}ms", d.as_millis())
    } else {
        format!("{:.2}s", d.as_secs_f64())
    }
}

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
    active: Vec<String>,
    failed: usize,
}

impl igel_common::progress::Sink for NetProgress {
    fn event(&self, event: igel_common::progress::Event<'_>) {
        use igel_common::progress::Event;
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
                    bar.set_style(igel_scheduler::progress::bar_style(
                        igel_scheduler::progress::bar_width(),
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
            Event::DownloadFinished { name, version, ok } => {
                let mut d = self.downloads.lock().unwrap_or_else(|e| e.into_inner());
                let label = format!("{name} {version}");
                d.active.retain(|a| *a != label);
                d.done += 1;
                if !ok {
                    d.failed += 1;
                }
                if let Some(bar) = &d.bar {
                    bar.set_position(d.done as u64);
                }
                d.redraw();
                if d.done >= d.total && d.total > 0 {
                    let took = d.started.map(|s| s.elapsed()).unwrap_or_default();
                    let (n, size, failed) = (d.done, d.bytes, d.failed);
                    if let Some(bar) = d.bar.take() {
                        bar.finish_and_clear();
                        multi_progress().remove(&bar);
                    }
                    *d = Downloads::default();
                    drop(d);
                    if failed > 0 {
                        return;
                    }
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
            + igel_scheduler::progress::bar_width()
            + 1
            + format!("{}/{}", self.done, self.total).len()
            + 1
            + size.len()
            + 3;
        let names: Vec<&str> = self.active.iter().map(String::as_str).collect();
        let list =
            igel_scheduler::progress::fit_list(&names, term_width().saturating_sub(used + 1));
        bar.set_message(if list.is_empty() {
            size
        } else {
            format!("{size} · {list}")
        });
    }
}

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

pub fn stdout_is_term() -> bool {
    io::stdout().is_terminal()
}
