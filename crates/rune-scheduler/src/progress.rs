use crate::PipelineEvent;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use rune_graph::NodeId;
use std::time::Duration;

/// Live progress display for builds: a single line,
///
/// ```text
/// ⠹ building ━━━━━━━━━━━╾──────────── 45/183 · serde_derive, syn, tokio +2
/// ```
///
/// with the crates being compiled in the order they started (the oldest
/// first: those are what the build is waiting on). One line that is cut to
/// the terminal's width can never wrap or outgrow the screen, which a stack
/// of one spinner per crate did on narrow or short terminals.
pub struct BuildProgress {
    multi: MultiProgress,
    bar: ProgressBar,
    /// Units compiling or running a build script, oldest first.
    active: Vec<(NodeId, String)>,
}

impl BuildProgress {
    /// Create a new progress display on top of an existing `MultiProgress`.
    ///
    /// The `MultiProgress` is shared with every line the CLI prints, which
    /// suspends it while writing, so output never tears the progress line.
    pub fn new(multi: MultiProgress, total_units: u64) -> Self {
        Self::with_verb(multi, total_units, "building")
    }

    /// Like [`Self::new`], with another verb (`checking`, `testing`).
    pub fn with_verb(multi: MultiProgress, total_units: u64, verb: &str) -> Self {
        let bar = multi.add(ProgressBar::new(total_units));
        bar.set_style(bar_style(bar_width()));
        bar.set_prefix(verb.to_string());
        bar.enable_steady_tick(Duration::from_millis(80));
        Self {
            multi,
            bar,
            active: Vec::new(),
        }
    }

    fn start(&mut self, node: NodeId, label: String) {
        self.active.retain(|(n, _)| *n != node);
        self.active.push((node, label));
        self.redraw();
    }

    fn stop(&mut self, node: NodeId) {
        self.active.retain(|(n, _)| *n != node);
        self.redraw();
    }

    fn redraw(&self) {
        // What is left of the line after the spinner, verb, bar and count.
        let used = 2
            + self.bar.prefix().chars().count()
            + 1
            + bar_width()
            + 1
            + format!("{}/{}", self.bar.position(), self.bar.length().unwrap_or(0)).len()
            + 1;
        let room = term_width().saturating_sub(used + 1);
        let names: Vec<&str> = self.active.iter().map(|(_, l)| l.as_str()).collect();
        self.bar.set_message(fit_list(&names, room));
    }

    /// Process a pipeline event.
    pub fn on_event(&mut self, event: &PipelineEvent) {
        match event {
            PipelineEvent::BuildStarted { node_id, unit } => {
                self.start(*node_id, unit.crate_id.name.clone());
            }
            PipelineEvent::BuildScriptStarted {
                node_id,
                crate_name,
            } => {
                self.start(*node_id, format!("{crate_name} (build script)"));
            }
            PipelineEvent::BuildScriptFinished { node_id, .. } => self.stop(*node_id),
            PipelineEvent::CacheHit { .. } => self.bar.inc(1),
            PipelineEvent::BuildFinished { node_id, .. } => {
                self.stop(*node_id);
                self.bar.inc(1);
            }
            PipelineEvent::BuildFailed { node_id, .. } => {
                // The full rendered diagnostic is printed separately by the
                // caller once the progress display has finished.
                self.stop(*node_id);
            }
            PipelineEvent::AllComplete { .. } => self.finish(),
            _ => {}
        }
    }

    /// Finish the progress display.
    pub fn finish(&self) {
        self.bar.finish_and_clear();
        self.multi.remove(&self.bar);
    }
}

impl Drop for BuildProgress {
    fn drop(&mut self) {
        if !self.bar.is_finished() {
            self.finish();
        }
    }
}

/// The width of the terminal the progress line is drawn on.
fn term_width() -> usize {
    console::Term::stderr()
        .size_checked()
        .map(|(_, w)| w as usize)
        .unwrap_or(80)
}

/// A bar that leaves room for the count and some names: 24 cells on an
/// ordinary terminal, fewer on a narrow one.
pub fn bar_width() -> usize {
    (term_width().saturating_sub(40) / 2).clamp(8, 24)
}

/// The style of Rune's progress bars, `width` cells wide.
pub fn bar_style(width: usize) -> ProgressStyle {
    ProgressStyle::default_bar()
        .template(&format!(
            "{{spinner:.141}} {{prefix:.141.bold}} {{bar:{width}.141/238}} {{pos}}/{{len}} {{wide_msg:.dim}}"
        ))
        .expect("valid template")
        .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ")
        .progress_chars("━╾─")
}

/// As many of `names` as fit in `room` columns, joined with commas, and
/// `+N` for the rest: `serde_derive, syn, tokio +3`. Never cuts a name.
pub fn fit_list(names: &[&str], room: usize) -> String {
    let mut out = String::new();
    for (i, name) in names.iter().enumerate() {
        let rest = names.len() - i - 1;
        let tail = if rest > 0 {
            format!(" +{rest}")
        } else {
            String::new()
        };
        let sep = if out.is_empty() { "" } else { ", " };
        let candidate = out.chars().count() + sep.len() + name.chars().count() + tail.len();
        if candidate > room {
            let shown = i;
            if shown == 0 {
                return String::new();
            }
            return format!("{out} +{}", names.len() - shown);
        }
        out.push_str(sep);
        out.push_str(name);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::fit_list;

    #[test]
    fn names_are_listed_whole_and_the_rest_counted() {
        let names = ["serde_derive", "syn", "tokio", "regex"];
        assert_eq!(fit_list(&names, 80), "serde_derive, syn, tokio, regex");
        assert_eq!(fit_list(&names, 26), "serde_derive, syn +2");
        assert_eq!(fit_list(&names, 5), "");
        assert_eq!(fit_list(&[], 10), "");
    }
}
