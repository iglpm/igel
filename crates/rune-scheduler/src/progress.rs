use crate::PipelineEvent;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use rune_graph::NodeId;
use std::collections::HashMap;

/// Live progress display for builds.
pub struct BuildProgress {
    multi: MultiProgress,
    main_bar: ProgressBar,
    active_bars: HashMap<NodeId, ProgressBar>,
}

impl BuildProgress {
    /// Create a new progress display on top of an existing `MultiProgress`.
    ///
    /// The `MultiProgress` is shared with the global logger (see
    /// `rune-cli::setup_logging`) so that log lines emitted while a build is
    /// running are suspended-and-redrawn through the same draw target
    /// instead of being written directly to stderr, which would tear up the
    /// bars.
    pub fn new(multi: MultiProgress, total_units: u64) -> Self {
        let main_style = ProgressStyle::default_bar()
            .template("{bar:32.magenta/dim} {pos}/{len} {msg}")
            .unwrap()
            .progress_chars("━╾─");
        let main_bar = multi.add(ProgressBar::new(total_units));
        main_bar.set_style(main_style);
        main_bar.set_message("compiling");

        Self {
            multi,
            main_bar,
            active_bars: HashMap::new(),
        }
    }

    /// Process a pipeline event.
    pub fn on_event(&mut self, event: &PipelineEvent) {
        let spinner_style = ProgressStyle::default_spinner()
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏✓")
            .template("{spinner:.magenta} {msg}")
            .unwrap();

        match event {
            PipelineEvent::BuildStarted { node_id, unit } => {
                let pb = self.multi.add(ProgressBar::new_spinner());
                pb.set_style(spinner_style);
                pb.set_message(format!(
                    "compiling {} v{}",
                    unit.crate_id.name, unit.crate_id.version
                ));
                pb.enable_steady_tick(std::time::Duration::from_millis(100));
                self.active_bars.insert(*node_id, pb);
            }
            PipelineEvent::BuildScriptStarted {
                node_id,
                crate_name,
            } => {
                let pb = self.multi.add(ProgressBar::new_spinner());
                pb.set_style(spinner_style);
                pb.set_message(format!("running build script · {}", crate_name));
                pb.enable_steady_tick(std::time::Duration::from_millis(100));
                self.active_bars.insert(*node_id, pb);
            }
            PipelineEvent::BuildScriptFinished { node_id, .. } => {
                if let Some(pb) = self.active_bars.remove(node_id) {
                    pb.finish_and_clear();
                }
            }
            PipelineEvent::CacheHit { .. } => {
                self.main_bar.inc(1);
            }
            PipelineEvent::BuildFinished { node_id, .. } => {
                if let Some(pb) = self.active_bars.remove(node_id) {
                    pb.finish_and_clear();
                }
                self.main_bar.inc(1);
            }
            PipelineEvent::BuildFailed { node_id, .. } => {
                // The full rendered diagnostic is printed separately by the
                // caller once the progress display has finished, rather than
                // stuffed into a single spinner line here.
                if let Some(pb) = self.active_bars.remove(node_id) {
                    pb.finish_and_clear();
                }
            }
            PipelineEvent::AllComplete { .. } => {
                self.finish();
            }
            _ => {}
        }
    }

    /// Finish the progress display.
    pub fn finish(&self) {
        self.main_bar.finish_and_clear();
        for pb in self.active_bars.values() {
            pb.finish_and_clear();
        }
    }
}
