use crate::PipelineEvent;
use igel_graph::NodeId;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::time::Duration;

/// Single-line build progress, cut to the terminal width, with active crates oldest first.
pub struct BuildProgress {
    multi: MultiProgress,
    bar: ProgressBar,
    active: Vec<(NodeId, String)>,
}

impl BuildProgress {
    pub fn new(multi: MultiProgress, total_units: u64) -> Self {
        Self::with_verb(multi, total_units, "building")
    }

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
                self.stop(*node_id);
            }
            PipelineEvent::AllComplete { .. } => self.finish(),
            _ => {}
        }
    }

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

fn term_width() -> usize {
    console::Term::stderr()
        .size_checked()
        .map(|(_, w)| w as usize)
        .unwrap_or(80)
}

pub fn bar_width() -> usize {
    (term_width().saturating_sub(40) / 2).clamp(8, 24)
}

pub fn bar_style(width: usize) -> ProgressStyle {
    ProgressStyle::default_bar()
        .template(&format!(
            "{{spinner:.179}} {{prefix:.179.bold}} {{bar:{width}.179/238}} {{pos}}/{{len}} {{wide_msg:.dim}}"
        ))
        .expect("valid template")
        .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ")
        .progress_chars("━╾─")
}

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
