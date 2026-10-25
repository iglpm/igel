//! A small built-in pager for long output. `IGEL_PAGER=never` turns paging off.

use console::{Key, Term};
use std::io::Write;

#[derive(Default)]
pub struct Page {
    lines: Vec<String>,
    title: String,
    keep_on_exit: bool,
}

impl Page {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            ..Self::default()
        }
    }

    pub fn keep_on_exit(mut self) -> Self {
        self.keep_on_exit = true;
        self
    }

    pub fn line(&mut self, text: impl AsRef<str>) {
        let text = text.as_ref();
        if text.is_empty() {
            self.lines.push(String::new());
        } else {
            self.lines.extend(text.lines().map(str::to_string));
        }
    }

    pub fn blank(&mut self) {
        self.lines.push(String::new());
    }

    pub fn show(self) {
        let term = Term::stdout();
        let enabled = std::env::var("IGEL_PAGER").map_or(true, |v| v != "never" && v != "0");
        let fits = term
            .size_checked()
            .is_none_or(|(rows, _)| self.lines.len() < rows as usize);
        if !enabled
            || !term.is_term()
            || fits
            || !std::io::IsTerminal::is_terminal(&std::io::stdin())
        {
            let mut out = std::io::stdout().lock();
            for l in &self.lines {
                let _ = writeln!(out, "{l}");
            }
            return;
        }
        Pager::new(self, term).run();
    }
}

struct Pager {
    page: Page,
    term: Term,
    top: usize,
    query: Option<String>,
    hits: Vec<usize>,
    notice: Option<String>,
}

impl Pager {
    fn new(page: Page, term: Term) -> Self {
        Self {
            page,
            term,
            top: 0,
            query: None,
            hits: Vec::new(),
            notice: None,
        }
    }

    fn size(&self) -> (usize, usize) {
        let (rows, cols) = self.term.size();
        ((rows as usize).max(3), (cols as usize).max(20))
    }

    fn body_rows(&self) -> usize {
        self.size().0 - 1
    }

    fn max_top(&self) -> usize {
        self.page.lines.len().saturating_sub(self.body_rows())
    }

    fn run(mut self) {
        let _ = self.term.write_str("\x1b[?1049h\x1b[?25l\x1b[?1007h");
        loop {
            self.draw();
            let Ok(key) = self.term.read_key() else { break };
            self.notice = None;
            let page = self.body_rows();
            match key {
                Key::Char('q') | Key::Escape | Key::CtrlC => break,
                Key::ArrowDown | Key::Char('j') | Key::Enter => self.scroll(1),
                Key::ArrowUp | Key::Char('k') => self.scroll(-1),
                Key::PageDown | Key::Char(' ') | Key::Char('f') => self.scroll(page as isize),
                Key::PageUp | Key::Char('b') => self.scroll(-(page as isize)),
                Key::Char('d') => self.scroll((page / 2) as isize),
                Key::Char('u') => self.scroll(-((page / 2) as isize)),
                Key::Home | Key::Char('g') => self.top = 0,
                Key::End | Key::Char('G') => self.top = self.max_top(),
                Key::Char('/') => self.prompt_search(),
                Key::Char('n') => self.next_hit(true),
                Key::Char('N') => self.next_hit(false),
                _ => {}
            }
        }
        let _ = self.term.write_str("\x1b[?1007l\x1b[?25h\x1b[?1049l");
        if self.page.keep_on_exit {
            let rows = self.body_rows();
            let mut out = std::io::stdout().lock();
            for l in self.page.lines.iter().skip(self.top).take(rows) {
                let _ = writeln!(out, "{l}");
            }
        }
    }

    fn scroll(&mut self, by: isize) {
        let top = self.top as isize + by;
        self.top = top.clamp(0, self.max_top() as isize) as usize;
    }

    fn draw(&self) {
        let (rows, cols) = self.size();
        let body = rows - 1;
        let mut frame = String::from("\x1b[H");
        for i in 0..body {
            frame.push_str("\x1b[2K");
            if let Some(line) = self.page.lines.get(self.top + i) {
                frame.push_str(&self.render_line(line, cols));
            }
            frame.push_str("\r\n");
        }
        frame.push_str("\x1b[2K");
        frame.push_str(&self.status_line(cols));
        let _ = self.term.write_str(&frame);
        let _ = self.term.flush();
    }

    fn render_line(&self, line: &str, cols: usize) -> String {
        match &self.query {
            Some(q) if !q.is_empty() && matches(line, q) => {
                let plain = console::strip_ansi_codes(line).to_string();
                let plain = console::truncate_str(&plain, cols, "…").to_string();
                highlight(&plain, q)
            }
            _ => console::truncate_str(line, cols, "…").to_string(),
        }
    }

    fn status_line(&self, cols: usize) -> String {
        let total = self.page.lines.len();
        let last = (self.top + self.body_rows()).min(total);
        let pct = (last * 100).checked_div(total).unwrap_or(100);
        let mut left = format!(
            "{}  {}–{} of {total} ({pct}%)",
            self.page.title,
            self.top + 1,
            last
        );
        if let Some(q) = &self.query {
            let shown = self
                .hits
                .iter()
                .filter(|&&h| h >= self.top && h < last)
                .count();
            left.push_str(&format!(
                "  ·  /{q}: {} ({shown} on screen)",
                crate::ui::plural(self.hits.len(), "match")
            ));
        }
        if let Some(n) = &self.notice {
            left.push_str(&format!("  ·  {n}"));
        }
        let keys = "↑↓ scroll  / search  q quit";
        let used = console::measure_text_width(&left) + 2;
        let keys_width = console::measure_text_width(keys);
        let text = if used + keys_width + 2 <= cols {
            let pad = cols - used - keys_width;
            format!(" {left}{:pad$}{keys} ", "")
        } else {
            format!(" {left:<w$}", w = cols.saturating_sub(1))
        };
        let text = console::truncate_str(&text, cols, "");
        format!("\x1b[7m{text}\x1b[0m")
    }

    fn prompt_search(&mut self) {
        let (rows, cols) = self.size();
        let mut query = String::new();
        loop {
            let prompt = format!("/{query}");
            let _ = self.term.write_str(&format!(
                "\x1b[{rows};1H\x1b[2K\x1b[7m {}\x1b[0m\x1b[?25h",
                console::truncate_str(&format!("{prompt:<w$}", w = cols - 2), cols - 1, "")
            ));
            let Ok(key) = self.term.read_key() else { break };
            match key {
                Key::Enter => break,
                Key::Escape | Key::CtrlC => {
                    let _ = self.term.write_str("\x1b[?25l");
                    return;
                }
                Key::Backspace => {
                    if query.pop().is_none() {
                        let _ = self.term.write_str("\x1b[?25l");
                        return;
                    }
                }
                Key::Char(c) if !c.is_control() => query.push(c),
                _ => {}
            }
        }
        let _ = self.term.write_str("\x1b[?25l");
        if query.is_empty() {
            self.next_hit(true);
            return;
        }
        self.hits = self
            .page
            .lines
            .iter()
            .enumerate()
            .filter(|(_, l)| matches(l, &query))
            .map(|(i, _)| i)
            .collect();
        self.query = Some(query);
        if self.hits.is_empty() {
            self.notice = Some("no match".into());
        } else {
            let at = self.hits.iter().copied().find(|&h| h >= self.top + 2);
            self.jump_to(at.unwrap_or(self.hits[0]));
        }
    }

    fn next_hit(&mut self, forward: bool) {
        if self.hits.is_empty() {
            self.notice = Some(if self.query.is_some() {
                "no match".into()
            } else {
                "no search yet: press /".into()
            });
            return;
        }
        let current = self.top + 2;
        let target = if forward {
            self.hits.iter().copied().find(|&h| h > current)
        } else {
            self.hits.iter().rev().copied().find(|&h| h < current)
        };
        match target {
            Some(t) => self.jump_to(t),
            None => {
                self.notice = Some(if forward {
                    "last match".into()
                } else {
                    "first match".into()
                })
            }
        }
    }

    fn jump_to(&mut self, line: usize) {
        self.top = line.saturating_sub(2).min(self.max_top());
    }
}

fn matches(line: &str, query: &str) -> bool {
    let plain = console::strip_ansi_codes(line);
    if query.chars().any(char::is_uppercase) {
        plain.contains(query)
    } else {
        plain.to_lowercase().contains(&query.to_lowercase())
    }
}

fn highlight(plain: &str, query: &str) -> String {
    let sensitive = query.chars().any(char::is_uppercase);
    let (hay, needle) = if sensitive {
        (plain.to_string(), query.to_string())
    } else {
        (plain.to_lowercase(), query.to_lowercase())
    };
    // Lowercasing can change byte lengths; skip highlighting rather than slice wrongly.
    if hay.len() != plain.len() || needle.is_empty() {
        return plain.to_string();
    }
    let mut out = String::new();
    let mut last = 0;
    for (at, _) in hay.match_indices(&needle) {
        out.push_str(&plain[last..at]);
        out.push_str("\x1b[7m");
        out.push_str(&plain[at..at + needle.len()]);
        out.push_str("\x1b[27m");
        last = at + needle.len();
    }
    out.push_str(&plain[last..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_is_smart_case_and_ignores_colours() {
        assert!(matches("\x1b[1mserde\x1b[0m = 1.0", "serde"));
        assert!(matches("Serde", "serde"));
        assert!(!matches("serde", "Serde"));
        assert_eq!(highlight("a serde b", "serde"), "a \x1b[7mserde\x1b[27m b");
        assert_eq!(highlight("SERDE", "serde"), "\x1b[7mSERDE\x1b[27m");
    }
}
