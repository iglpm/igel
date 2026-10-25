//! The recorded schedule of a build and the chain of steps that determined its length.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StepId {
    pub node: usize,
    pub script: bool,
    /// The link of an executable compiled before its dependencies were (rustc-runified).
    pub link: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    End(StepId),
    Metadata(StepId),
}

#[derive(Debug, Clone)]
pub struct Step {
    pub label: String,
    pub kind: &'static str,
    pub ready: f64,
    pub start: f64,
    pub metadata: Option<f64>,
    pub end: f64,
    pub gates: Vec<Gate>,
}

impl Step {
    fn time_of(&self, gate: &Gate) -> f64 {
        match gate {
            Gate::End(_) => self.end,
            Gate::Metadata(_) => self.metadata.unwrap_or(self.end),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    pub label: String,
    pub kind: &'static str,
    pub secs: f64,
    pub waited: f64,
}

#[derive(Debug, Clone)]
pub struct ScheduleReport {
    pub wall: f64,
    pub busy: f64,
    pub cpus: usize,
    pub jobs: usize,
    pub steps: usize,
    pub chain: Vec<Link>,
}

impl ScheduleReport {
    pub fn analyze(steps: &HashMap<StepId, Step>, cpus: usize, jobs: usize) -> Self {
        let first = steps
            .values()
            .map(|s| s.start)
            .fold(f64::INFINITY, f64::min);
        let last = steps.values().map(|s| s.end).fold(0.0, f64::max);
        let wall = if steps.is_empty() { 0.0 } else { last - first };
        let busy = steps.values().map(|s| s.end - s.start).sum();

        let mut chain = Vec::new();
        let mut cur = steps
            .iter()
            .max_by(|a, b| a.1.end.total_cmp(&b.1.end))
            .map(|(id, _)| *id);
        let mut until = last;
        let mut seen = std::collections::HashSet::new();
        while let Some(id) = cur {
            if !seen.insert(id) {
                break;
            }
            let step = &steps[&id];
            chain.push(Link {
                label: step.label.clone(),
                kind: step.kind,
                secs: (until - step.start).max(0.0),
                waited: (step.start - step.ready).max(0.0),
            });
            let prev = step
                .gates
                .iter()
                .filter_map(|g| {
                    let (Gate::End(p) | Gate::Metadata(p)) = g;
                    steps.get(p).map(|s| (*p, s.time_of(g)))
                })
                .max_by(|a, b| a.1.total_cmp(&b.1));
            match prev {
                Some((p, t)) => {
                    until = t;
                    cur = Some(p);
                }
                None => cur = None,
            }
        }
        chain.reverse();
        Self {
            wall,
            busy,
            cpus,
            jobs,
            steps: steps.len(),
            chain,
        }
    }

    pub fn parallelism(&self) -> f64 {
        if self.wall > 0.0 {
            self.busy / self.wall
        } else {
            0.0
        }
    }

    pub fn chain_secs(&self) -> f64 {
        self.chain.iter().map(|l| l.secs).sum()
    }

    pub fn hint(&self) -> Option<String> {
        let cores = self.cpus.min(self.jobs).max(1) as f64;
        if self.wall < 10.0
            || self.parallelism() >= 0.6 * cores
            || self.chain.len() < 2
            || self.chain_secs() < 0.6 * self.wall
        {
            return None;
        }
        let slowest = self.chain.iter().max_by(|a, b| a.secs.total_cmp(&b.secs))?;
        Some(format!(
            "{:.1} of {} cores were busy on average: {:.0} of the {:.0} s went to {} crates that \
             had to be built one after another ({} → … → {}); the longest was {} ({:.1} s). \
             `--timings` shows the whole chain",
            self.parallelism(),
            self.cpus,
            self.chain_secs(),
            self.wall,
            self.chain.len(),
            self.chain[0].label,
            self.chain[self.chain.len() - 1].label,
            slowest.label,
            slowest.secs
        ))
    }

    pub fn describe_chain(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "critical chain: {} steps, {:.1} s of {:.1} s ({:.1} steps running on average)",
            self.chain.len(),
            self.chain_secs(),
            self.wall,
            self.parallelism()
        );
        for link in &self.chain {
            let _ = write!(out, "    {:>6.2}s  {}", link.secs, link.label);
            if link.waited >= 0.05 {
                let _ = write!(out, "  (waited {:.2}s for a job slot)", link.waited);
            }
            out.push('\n');
        }
        out
    }
}

fn lanes(steps: &HashMap<StepId, Step>) -> Vec<(StepId, usize)> {
    let mut order: Vec<(&StepId, &Step)> = steps.iter().collect();
    order.sort_by(|a, b| a.1.start.total_cmp(&b.1.start));
    let mut free_at: Vec<f64> = Vec::new();
    let mut out = Vec::with_capacity(order.len());
    for (id, step) in order {
        let lane = match free_at.iter().position(|&t| t <= step.start + 1e-6) {
            Some(l) => l,
            None => {
                free_at.push(0.0);
                free_at.len() - 1
            }
        };
        free_at[lane] = step.end;
        out.push((*id, lane));
    }
    out
}

pub fn write_chrome_trace(path: &Path, steps: &HashMap<StepId, Step>) {
    let events: Vec<serde_json::Value> = lanes(steps)
        .into_iter()
        .map(|(id, lane)| {
            let s = &steps[&id];
            let after: Vec<&str> = s
                .gates
                .iter()
                .filter_map(|g| {
                    let (Gate::End(p) | Gate::Metadata(p)) = g;
                    steps.get(p).map(|d| d.label.as_str())
                })
                .collect();
            serde_json::json!({
                "name": s.label,
                "cat": s.kind,
                "ph": "X",
                "ts": (s.start * 1e6) as u64,
                "dur": ((s.end - s.start) * 1e6) as u64,
                "pid": 1,
                "tid": lane,
                "args": { "after": after, "ready": s.ready, "metadata": s.metadata },
            })
        })
        .collect();
    let doc = serde_json::json!({ "traceEvents": events });
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, doc.to_string());
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub fn write_html(path: &Path, steps: &HashMap<StepId, Step>, report: &ScheduleReport) {
    let placed = lanes(steps);
    let lane_count = placed.iter().map(|(_, l)| l + 1).max().unwrap_or(1);
    let origin = steps
        .values()
        .map(|s| s.start)
        .fold(f64::INFINITY, f64::min);
    let origin = if origin.is_finite() { origin } else { 0.0 };
    let span = report.wall.max(0.001);
    let (width, row, left) = (1100.0_f64, 16.0_f64, 8.0_f64);
    let x = |t: f64| left + (t - origin) / span * (width - 2.0 * left);
    let on_chain: std::collections::HashSet<&str> =
        report.chain.iter().map(|l| l.label.as_str()).collect();

    let mut bars = String::new();
    for (id, lane) in &placed {
        let s = &steps[id];
        let y = 4.0 + *lane as f64 * row;
        let w = (x(s.end) - x(s.start)).max(1.0);
        let class = format!(
            "k-{}{}",
            s.kind.replace(' ', "-"),
            if on_chain.contains(s.label.as_str()) {
                " chain"
            } else {
                ""
            }
        );
        let _ = write!(
            bars,
            r#"<rect class="{class}" x="{:.1}" y="{y:.1}" width="{w:.1}" height="{:.1}" rx="2"><title>{} — {:.2} s (ready at {:.2} s, started {:.2} s)</title></rect>"#,
            x(s.start),
            row - 3.0,
            escape(&s.label),
            s.end - s.start,
            s.ready - origin,
            s.start - origin,
        );
        if w > 70.0 {
            let _ = write!(
                bars,
                r#"<text x="{:.1}" y="{:.1}">{}</text>"#,
                x(s.start) + 3.0,
                y + row - 7.0,
                escape(&s.label)
            );
        }
    }

    let samples = 400;
    let mut concurrency = String::new();
    let graph_h = 90.0;
    let peak = report.jobs.max(1) as f64;
    for i in 0..=samples {
        let t = origin + span * i as f64 / samples as f64;
        let n = steps.values().filter(|s| s.start <= t && t < s.end).count() as f64;
        let _ = write!(
            concurrency,
            "{:.1},{:.1} ",
            x(t),
            graph_h - (n / peak).min(1.0) * (graph_h - 6.0)
        );
    }
    let cpu_line = graph_h - (report.cpus as f64 / peak).min(1.0) * (graph_h - 6.0);

    let mut chain_rows = String::new();
    for link in &report.chain {
        let _ = write!(
            chain_rows,
            "<tr><td>{}</td><td>{}</td><td class=num>{:.2}</td><td class=num>{}</td></tr>",
            escape(&link.label),
            link.kind,
            link.secs,
            if link.waited >= 0.05 {
                format!("{:.2}", link.waited)
            } else {
                String::new()
            }
        );
    }

    let hint = report
        .hint()
        .map(|h| format!("<p class=hint>{}</p>", escape(&h)))
        .unwrap_or_default();
    let html = format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Igel build timings</title>
<style>
:root {{ --bg:#fbfaf7; --fg:#1d1b18; --muted:#6b665e; --line:#e3ded5; --lib:#6b8fd6; --bin:#d98b3a; --test:#5aa36b; --pm:#a07ad6; --bs:#c9b458; --chain:#d0342c; }}
@media (prefers-color-scheme: dark) {{ :root {{ --bg:#16150f; --fg:#ece8df; --muted:#a39e94; --line:#34312a; }} }}
body {{ background:var(--bg); color:var(--fg); font:14px/1.45 system-ui,-apple-system,sans-serif; margin:24px; }}
h1 {{ font-size:20px; margin:0 0 4px; }} h2 {{ font-size:15px; margin:24px 0 8px; }}
.meta {{ color:var(--muted); }} .hint {{ border-left:3px solid var(--chain); padding:6px 10px; }}
.scroll {{ overflow-x:auto; border:1px solid var(--line); border-radius:6px; }}
svg text {{ font-size:10px; fill:var(--fg); pointer-events:none; }}
rect.k-lib {{ fill:var(--lib); }} rect.k-bin, rect.k-example {{ fill:var(--bin); }} rect.k-test, rect.k-bench {{ fill:var(--test); }}
rect.k-proc-macro {{ fill:var(--pm); }} rect.k-build-script {{ fill:var(--bs); }} rect.chain {{ stroke:var(--chain); stroke-width:2; }}
table {{ border-collapse:collapse; }} td, th {{ padding:3px 10px; border-bottom:1px solid var(--line); text-align:left; }} .num {{ text-align:right; font-variant-numeric:tabular-nums; }}
.legend span {{ display:inline-block; margin-right:14px; }} .legend i {{ display:inline-block; width:10px; height:10px; margin-right:4px; border-radius:2px; }}
</style></head><body>
<h1>Build timings</h1>
<p class=meta>{steps} steps in {wall:.1} s · {par:.1} running on average · {cpus} cores · {jobs} jobs · critical chain {chain:.1} s</p>
{hint}
<h2>Timeline</h2>
<p class=legend><span><i style="background:var(--lib)"></i>library</span><span><i style="background:var(--bin)"></i>binary</span><span><i style="background:var(--test)"></i>test</span><span><i style="background:var(--pm)"></i>proc-macro</span><span><i style="background:var(--bs)"></i>build script</span><span><i style="border:2px solid var(--chain)"></i>critical chain</span></p>
<div class=scroll><svg width="{width}" height="{h}" role="img" aria-label="Build timeline">{bars}</svg></div>
<h2>Steps running at once</h2>
<div class=scroll><svg width="{width}" height="{graph_h}" role="img" aria-label="Concurrency over time">
<line x1="0" x2="{width}" y1="{cpu_line:.1}" y2="{cpu_line:.1}" stroke="var(--muted)" stroke-dasharray="4 3"><title>{cpus} cores</title></line>
<polyline fill="none" stroke="var(--lib)" stroke-width="1.5" points="{concurrency}"/></svg></div>
<h2>Critical chain</h2>
<table><tr><th>Step</th><th>Kind</th><th class=num>Seconds</th><th class=num>Waited for a slot</th></tr>{chain_rows}</table>
</body></html>
"#,
        steps = report.steps,
        wall = report.wall,
        par = report.parallelism(),
        cpus = report.cpus,
        jobs = report.jobs,
        chain = report.chain_secs(),
        h = 8.0 + lane_count as f64 * row,
    );
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, html);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(
        label: &str,
        ready: f64,
        start: f64,
        meta: Option<f64>,
        end: f64,
        gates: Vec<Gate>,
    ) -> Step {
        Step {
            label: label.into(),
            kind: "lib",
            ready,
            start,
            metadata: meta,
            end,
            gates,
        }
    }

    fn id(n: usize) -> StepId {
        StepId {
            node: n,
            script: false,
            link: false,
        }
    }

    #[test]
    fn the_chain_follows_whatever_was_ready_last() {
        // a (0-4, rmeta at 2) ─pipelined→ c (2-9)
        // b (0-1)          ─────────────↗
        // c links against nothing else; d (9-12) needs c fully.
        let mut steps = HashMap::new();
        steps.insert(id(0), step("a", 0.0, 0.0, Some(2.0), 4.0, vec![]));
        steps.insert(id(1), step("b", 0.0, 0.0, Some(0.5), 1.0, vec![]));
        steps.insert(
            id(2),
            step(
                "c",
                2.0,
                2.0,
                Some(6.0),
                9.0,
                vec![Gate::Metadata(id(0)), Gate::Metadata(id(1))],
            ),
        );
        steps.insert(
            id(3),
            step("d", 9.0, 9.5, None, 12.0, vec![Gate::End(id(2))]),
        );
        let r = ScheduleReport::analyze(&steps, 8, 8);
        let labels: Vec<&str> = r.chain.iter().map(|l| l.label.as_str()).collect();
        assert_eq!(labels, ["a", "c", "d"]);
        assert!((r.chain[0].secs - 2.0).abs() < 1e-9);
        assert!((r.chain[1].secs - 7.0).abs() < 1e-9);
        assert!((r.chain[2].waited - 0.5).abs() < 1e-9);
        assert!((r.wall - 12.0).abs() < 1e-9);
    }

    #[test]
    fn a_long_mostly_serial_build_gets_a_hint_and_a_busy_one_does_not() {
        let mut serial = HashMap::new();
        let mut prev: Option<StepId> = None;
        for i in 0..6 {
            let t = i as f64 * 3.0;
            serial.insert(
                id(i),
                step(
                    &format!("s{i}"),
                    t,
                    t,
                    None,
                    t + 3.0,
                    prev.map(Gate::End).into_iter().collect(),
                ),
            );
            prev = Some(id(i));
        }
        let r = ScheduleReport::analyze(&serial, 10, 13);
        let hint = r
            .hint()
            .expect("an 18 s one-at-a-time build deserves a hint");
        assert!(hint.contains("s0 → … → s5"), "{hint}");

        let mut busy = HashMap::new();
        for i in 0..40 {
            busy.insert(id(i), step(&format!("p{i}"), 0.0, 0.0, None, 20.0, vec![]));
        }
        assert!(ScheduleReport::analyze(&busy, 10, 13).hint().is_none());
    }

    #[test]
    fn the_html_report_names_every_step() {
        let dir = tempfile::tempdir().unwrap();
        let mut steps = HashMap::new();
        steps.insert(id(0), step("alpha <lib>", 0.0, 0.0, None, 1.0, vec![]));
        steps.insert(
            id(1),
            step("beta", 1.0, 1.0, None, 2.0, vec![Gate::End(id(0))]),
        );
        let r = ScheduleReport::analyze(&steps, 4, 4);
        let path = dir.path().join("t.html");
        write_html(&path, &steps, &r);
        let html = std::fs::read_to_string(&path).unwrap();
        assert!(html.contains("alpha &lt;lib&gt;") && html.contains("beta"));
        assert!(html.contains("Critical chain"));
    }
}
