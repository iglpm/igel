//! Terminal summary, Markdown, HTML and run-to-run comparison.

use std::fmt::Write as _;

use crate::results::{Measurement, Results};
use crate::stats::{self, Summary};

fn value(m: &Measurement) -> String {
    if m.summary.n == 0 {
        return if m.failures > 0 {
            "failed".into()
        } else {
            "—".into()
        };
    }
    match m.unit.as_str() {
        "bytes" => stats::bytes(m.summary.median),
        _ => stats::seconds(m.summary.median),
    }
}

fn spread(m: &Measurement) -> String {
    if m.summary.n < 2 || m.unit == "bytes" {
        return String::new();
    }
    format!("±{:.0} %", m.summary.cv() * 100.0)
}

fn verdict(igl: &Measurement, cargo: &Measurement) -> Option<(f64, String)> {
    let c = stats::compare(&igl.summary, &cargo.summary)?;
    let text = if c.within_noise && igl.unit != "bytes" {
        "about the same (within noise)".to_string()
    } else if igl.unit == "bytes" {
        if c.speedup >= 1.0 {
            format!("{:.1}× less disk", c.speedup)
        } else {
            format!("{:.1}× more disk", 1.0 / c.speedup)
        }
    } else {
        stats::describe(c.speedup)
    };
    Some((c.speedup, text))
}

fn geomean(pairs: &[(f64, &Measurement)]) -> Option<f64> {
    let logs: Vec<f64> = pairs
        .iter()
        .filter(|(_, m)| m.unit == "s")
        .map(|(s, _)| s.ln())
        .collect();
    (!logs.is_empty()).then(|| (logs.iter().sum::<f64>() / logs.len() as f64).exp())
}

fn pairs(results: &Results) -> Vec<(&Measurement, &Measurement, f64, String)> {
    let mut out = Vec::new();
    for w in results.workloads() {
        for m in results.scenarios(&w) {
            if let (Some(r), Some(c)) = (
                results.find(&w, &m.scenario, "igl"),
                results.find(&w, &m.scenario, "cargo"),
            ) && let Some((s, text)) = verdict(r, c)
            {
                out.push((r, c, s, text));
            }
        }
    }
    out
}

pub fn terminal(results: &Results) -> String {
    let mut out = String::new();
    let label = |m: &Measurement| format!("{} · {}", m.category, m.title);
    let width = results
        .measurements
        .iter()
        .map(|m| label(m).chars().count())
        .max()
        .unwrap_or(10)
        .max(10);
    for w in results.workloads() {
        let _ = writeln!(out, "\n{w}");
        let _ = writeln!(
            out,
            "  {:<width$} {:>12} {:>12}   igl vs cargo",
            "scenario", "igl", "cargo"
        );
        for m in results.scenarios(&w) {
            let r = results.find(&w, &m.scenario, "igl");
            let c = results.find(&w, &m.scenario, "cargo");
            let v = match (r, c) {
                (Some(r), Some(c)) => verdict(r, c).map(|(_, t)| t).unwrap_or_default(),
                _ => String::new(),
            };
            let _ = writeln!(
                out,
                "  {:<width$} {:>12} {:>12}   {v}",
                label(m),
                r.map(value).unwrap_or_default(),
                c.map(value).unwrap_or_default(),
            );
        }
    }
    let all = pairs(results);
    let speedups: Vec<(f64, &Measurement)> = all.iter().map(|(r, _, s, _)| (*s, *r)).collect();
    if let Some(g) = geomean(&speedups) {
        let _ = writeln!(
            out,
            "\nOverall (geometric mean of the time measurements): Igel is {}.",
            stats::describe(g)
        );
    }
    out
}

pub fn markdown(results: &Results) -> String {
    let mut out = String::from("# Igel vs Cargo\n\n");
    out.push_str(&environment_lines(results).join("\n"));
    out.push_str("\n\n");
    let all = pairs(results);
    let speedups: Vec<(f64, &Measurement)> = all.iter().map(|(r, _, s, _)| (*s, *r)).collect();
    if let Some(g) = geomean(&speedups) {
        let _ = writeln!(
            out,
            "**Overall:** across the {} time measurements, Igel is **{}** (geometric mean).\n",
            speedups.iter().filter(|(_, m)| m.unit == "s").count(),
            stats::describe(g)
        );
    }
    for w in results.workloads() {
        let about = results
            .meta
            .workloads
            .iter()
            .find(|i| i.name == w)
            .map(|i| i.description.clone())
            .unwrap_or_default();
        let _ = writeln!(out, "## {w}\n\n{about}\n");
        out.push_str(
            "| Scenario | Igel | Cargo | Igel vs Cargo | CPU time (Igel / Cargo) | Peak memory (Igel / Cargo) |\n",
        );
        out.push_str("| --- | ---: | ---: | --- | ---: | ---: |\n");
        for m in results.scenarios(&w) {
            let r = results.find(&w, &m.scenario, "igl");
            let c = results.find(&w, &m.scenario, "cargo");
            let v = match (r, c) {
                (Some(r), Some(c)) => verdict(r, c).map(|(_, t)| t).unwrap_or_default(),
                _ => String::new(),
            };
            let cell = |m: Option<&Measurement>| {
                m.map(|m| format!("{} {}", value(m), spread(m)).trim().to_string())
                    .unwrap_or_default()
            };
            let cpu = |m: Option<&Measurement>| {
                m.filter(|m| m.unit == "s" && m.cpu.n > 0)
                    .map(|m| stats::seconds(m.cpu.median))
                    .unwrap_or_else(|| "—".into())
            };
            let mem = |m: Option<&Measurement>| {
                m.filter(|m| m.peak_rss_kib > 0)
                    .map(|m| stats::bytes(m.peak_rss_kib as f64 * 1024.0))
                    .unwrap_or_else(|| "—".into())
            };
            let _ = writeln!(
                out,
                "| {} · {} | {} | {} | {v} | {} / {} | {} / {} |",
                m.category,
                m.title,
                cell(r),
                cell(c),
                cpu(r),
                cpu(c),
                mem(r),
                mem(c)
            );
        }
        out.push('\n');
    }
    out.push_str("## Scenarios\n\n");
    let mut seen: Vec<&str> = Vec::new();
    for m in &results.measurements {
        if !seen.contains(&m.scenario.as_str()) {
            seen.push(&m.scenario);
            let _ = writeln!(
                out,
                "- **{} · {}** (`{}`): {}",
                m.category, m.title, m.scenario, m.about
            );
        }
    }
    out.push_str(
        "\nTimes are medians of the runs (± the coefficient of variation); CPU time adds up every \
         process a command waited for; peak memory is the largest single process. Each tool \
         builds its own copy of a workload, and runs alternate between the tools.\n",
    );
    out
}

fn environment_lines(results: &Results) -> Vec<String> {
    let m = &results.meta;
    let mut lines = Vec::new();
    for t in &m.tools {
        lines.push(format!("- {}: `{}`", t.name, t.version));
    }
    lines.push(format!("- rustc: `{}`", m.rustc));
    lines.push(format!(
        "- machine: {} CPUs{}, {} ({})",
        m.cpus,
        m.memory_bytes
            .map(|b| format!(", {} memory", stats::bytes(b as f64)))
            .unwrap_or_default(),
        m.os,
        m.host
    ));
    lines.push(format!(
        "- suite `{}`: {} runs of each expensive scenario, {} of each cheap one{}",
        m.suite,
        m.runs_heavy,
        m.runs_light,
        m.commit
            .as_ref()
            .map(|c| format!(", Igel at `{c}`"))
            .unwrap_or_default()
    ));
    lines
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn bars(r: Option<&Measurement>, c: Option<&Measurement>) -> String {
    let rv = r.map(|m| m.summary.median).unwrap_or(0.0);
    let cv = c.map(|m| m.summary.median).unwrap_or(0.0);
    let max = rv.max(cv);
    if max <= 0.0 {
        return String::new();
    }
    let width = 360.0;
    let bar = |v: f64, y: u32, class: &str, label: String| {
        let w = (v / max * width).max(1.0);
        format!(
            "<rect class=\"{class}\" x=\"0\" y=\"{y}\" width=\"{w:.1}\" height=\"14\" rx=\"3\"/>\
             <text x=\"{tx:.1}\" y=\"{ty}\" class=\"lbl\">{label}</text>",
            tx = w + 6.0,
            ty = y + 11
        )
    };
    let label = |m: Option<&Measurement>| m.map(value).unwrap_or_default();
    format!(
        "<svg viewBox=\"0 0 470 36\" width=\"470\" height=\"36\" role=\"img\">{}{}</svg>",
        bar(rv, 2, "igl", format!("igl {}", label(r))),
        bar(cv, 20, "cargo", format!("cargo {}", label(c)))
    )
}

pub fn html(results: &Results) -> String {
    let mut body = String::new();
    let all = pairs(results);
    let speedups: Vec<(f64, &Measurement)> = all.iter().map(|(r, _, s, _)| (*s, *r)).collect();
    if let Some(g) = geomean(&speedups) {
        let _ = write!(
            body,
            "<p class=\"headline\">Across {} time measurements, Igel is <strong>{}</strong> than Cargo \
             (geometric mean).</p>",
            speedups.iter().filter(|(_, m)| m.unit == "s").count(),
            esc(&stats::describe(g))
        );
    }
    let mut cats: Vec<(String, Vec<(f64, &Measurement)>)> = Vec::new();
    for (r, _, s, _) in &all {
        match cats.iter_mut().find(|(c, _)| *c == r.category) {
            Some((_, v)) => v.push((*s, *r)),
            None => cats.push((r.category.clone(), vec![(*s, *r)])),
        }
    }
    body.push_str("<div class=\"tiles\">");
    for (cat, v) in &cats {
        let label = if v.iter().all(|(_, m)| m.unit == "bytes") {
            let mean = v.iter().map(|(s, _)| s.ln()).sum::<f64>() / v.len() as f64;
            let s = mean.exp();
            if s >= 1.0 {
                format!("{s:.1}× less disk")
            } else {
                format!("{:.1}× more disk", 1.0 / s)
            }
        } else {
            geomean(v).map(stats::describe).unwrap_or_default()
        };
        let _ = write!(
            body,
            "<div class=\"tile\"><div class=\"cat\">{}</div><div class=\"big\">{}</div></div>",
            esc(cat),
            esc(&label)
        );
    }
    body.push_str("</div>");

    for w in results.workloads() {
        let about = results
            .meta
            .workloads
            .iter()
            .find(|i| i.name == w)
            .map(|i| i.description.clone())
            .unwrap_or_default();
        let _ = write!(
            body,
            "<section><h2>{}</h2><p class=\"muted\">{}</p><table><thead><tr><th>Scenario</th>\
             <th>Median time (igl vs cargo)</th><th>Igel vs Cargo</th><th>CPU</th><th>Peak memory</th>\
             </tr></thead><tbody>",
            esc(&w),
            esc(&about)
        );
        for m in results.scenarios(&w) {
            let r = results.find(&w, &m.scenario, "igl");
            let c = results.find(&w, &m.scenario, "cargo");
            let (class, v) = match (r, c) {
                (Some(r), Some(c)) => match verdict(r, c) {
                    Some((s, t)) if s >= 1.05 => ("good", t),
                    Some((s, t)) if s <= 0.95 => ("bad", t),
                    Some((_, t)) => ("even", t),
                    None => ("even", String::new()),
                },
                _ => ("even", String::new()),
            };
            let cpu = |m: Option<&Measurement>| {
                m.filter(|m| m.unit == "s" && m.cpu.n > 0)
                    .map(|m| stats::seconds(m.cpu.median))
                    .unwrap_or_else(|| "—".into())
            };
            let mem = |m: Option<&Measurement>| {
                m.filter(|m| m.peak_rss_kib > 0)
                    .map(|m| stats::bytes(m.peak_rss_kib as f64 * 1024.0))
                    .unwrap_or_else(|| "—".into())
            };
            let _ = write!(
                body,
                "<tr><td><div class=\"sc\">{}</div><div class=\"muted small\">{} · {}</div></td>\
                 <td>{}</td><td class=\"{class}\">{}</td><td class=\"small\">{} / {}</td>\
                 <td class=\"small\">{} / {}</td></tr>",
                esc(&m.title),
                esc(&m.category),
                esc(&m.about),
                bars(r, c),
                esc(&v),
                cpu(r),
                cpu(c),
                mem(r),
                mem(c)
            );
        }
        body.push_str("</tbody></table></section>");
    }
    let env: String = environment_lines(results)
        .iter()
        .map(|l| {
            format!(
                "<li>{}</li>",
                esc(l.trim_start_matches("- ")).replace('`', "")
            )
        })
        .collect();
    format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Igel vs Cargo</title>
<style>
:root {{ --bg:#fbfaff; --fg:#1c1830; --muted:#6b6585; --line:#e4e0f0; --igl:#7c4dff; --cargo:#d9822b; --good:#16794a; --bad:#b4232f; --tile:#f1edff; }}
@media (prefers-color-scheme: dark) {{ :root {{ --bg:#14121c; --fg:#ece9f6; --muted:#9a94b4; --line:#2c2840; --igl:#a68bff; --cargo:#f0a35a; --good:#4cc38a; --bad:#ff6b76; --tile:#1f1b2e; }} }}
body {{ margin:0; background:var(--bg); color:var(--fg); font:15px/1.5 system-ui, -apple-system, Segoe UI, sans-serif; }}
main {{ max-width:1100px; margin:0 auto; padding:24px 16px 64px; }}
h1 {{ font-size:28px; margin:0 0 4px; }} h2 {{ margin:36px 0 2px; }}
.muted {{ color:var(--muted); }} .small {{ font-size:13px; }}
.headline {{ font-size:18px; }}
.tiles {{ display:grid; grid-template-columns:repeat(auto-fill,minmax(180px,1fr)); gap:10px; margin:18px 0; }}
.tile {{ background:var(--tile); border-radius:10px; padding:12px 14px; }}
.tile .cat {{ color:var(--muted); font-size:13px; }} .tile .big {{ font-size:19px; font-weight:600; }}
table {{ width:100%; border-collapse:collapse; margin-top:10px; }}
th, td {{ text-align:left; padding:8px 6px; border-bottom:1px solid var(--line); vertical-align:middle; }}
th {{ font-size:13px; color:var(--muted); font-weight:500; }}
.sc {{ font-weight:600; }}
.good {{ color:var(--good); font-weight:600; }} .bad {{ color:var(--bad); font-weight:600; }}
svg {{ max-width:100%; height:auto; }} svg .igl {{ fill:var(--igl); }} svg .cargo {{ fill:var(--cargo); }}
svg .lbl {{ fill:var(--fg); font-size:11px; }}
.table-wrap {{ overflow-x:auto; }}
ul.env {{ padding-left:18px; color:var(--muted); font-size:13px; }}
</style></head><body><main>
<h1>Igel vs Cargo</h1>
<ul class="env">{env}</ul>
{body}
<p class="muted small">Times are medians; each tool builds its own copy of a workload, runs alternate between the tools, CPU time adds up every process a command waited for, peak memory is the largest single process. Generated by <code>igel-bench</code>.</p>
</main></body></html>
"#
    )
}

pub fn compare(old: &Results, new: &Results, tool: &str, threshold: f64) -> (String, usize) {
    let mut out = String::new();
    let mut regressions = 0;
    let control_tool = if tool == "cargo" { "igl" } else { "cargo" };
    let _ = writeln!(
        out,
        "  {:<12} {:<18} {:>12} {:>12} {:>9} {:>10}",
        "workload",
        "scenario",
        "before",
        "after",
        "change",
        format!("vs {control_tool}")
    );
    for n in new.measurements.iter().filter(|m| m.tool == tool) {
        let Some(o) = old.find(&n.workload, &n.scenario, tool) else {
            continue;
        };
        if o.summary.n == 0 || n.summary.n == 0 || o.summary.median <= 0.0 {
            continue;
        }
        let ratio = n.summary.median / o.summary.median;
        // The other tool, measured in the same runs, says how much the machine itself changed.
        let control = old
            .find(&n.workload, &n.scenario, control_tool)
            .zip(new.find(&n.workload, &n.scenario, control_tool))
            .filter(|(co, cn)| co.summary.median > 0.0 && cn.summary.n > 0)
            .map(|(co, cn)| cn.summary.median / co.summary.median);
        let change = ratio / control.unwrap_or(1.0) - 1.0;
        let noise = noise(&o.summary).max(noise(&n.summary));
        // Every run slower than every earlier one: with three runs, a median moves this much by chance.
        let apart = |a: &Summary, b: &Summary| a.max < b.min;
        let flag = if change > threshold.max(noise) && apart(&o.summary, &n.summary) {
            regressions += 1;
            "  ✗ slower"
        } else if change < -threshold.max(noise) && apart(&n.summary, &o.summary) {
            "  ✓ faster"
        } else {
            ""
        };
        let versus = match control {
            Some(_) => format!("{:>+9.1} %", change * 100.0),
            None => format!("{:>11}", "-"),
        };
        let _ = writeln!(
            out,
            "  {:<12} {:<18} {:>12} {:>12} {:>+8.1} % {versus}{flag}",
            n.workload,
            n.scenario,
            value(o),
            value(n),
            (ratio - 1.0) * 100.0
        );
    }
    (out, regressions)
}

fn noise(s: &Summary) -> f64 {
    if s.n < 3 { 0.0 } else { 2.0 * s.cv() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::results::{Meta, WorkloadInfo};
    use crate::stats::Summary;

    fn m(workload: &str, scenario: &str, tool: &str, values: &[f64]) -> Measurement {
        Measurement {
            workload: workload.into(),
            scenario: scenario.into(),
            category: "Cat".into(),
            title: scenario.into(),
            about: "about".into(),
            tool: tool.into(),
            unit: "s".into(),
            values: values.to_vec(),
            summary: Summary::of(values),
            cpu: Summary::of(values),
            peak_rss_kib: 1024,
            samples: Vec::new(),
            failures: 0,
            note: None,
            project_bytes: None,
        }
    }

    fn results(ms: Vec<Measurement>) -> Results {
        Results {
            format: crate::results::FORMAT,
            meta: Meta {
                started: 0,
                suite: "quick".into(),
                tools: Vec::new(),
                rustc: "rustc 1.0".into(),
                host: "x".into(),
                cpus: 4,
                memory_bytes: None,
                os: "linux".into(),
                commit: None,
                runs_heavy: 3,
                runs_light: 5,
                workloads: vec![WorkloadInfo {
                    name: "w".into(),
                    description: "d".into(),
                }],
            },
            measurements: ms,
        }
    }

    #[test]
    fn reports_name_the_faster_tool() {
        let r = results(vec![
            m("w", "noop", "igl", &[0.05, 0.05]),
            m("w", "noop", "cargo", &[0.2, 0.2]),
        ]);
        let t = terminal(&r);
        assert!(t.contains("4.0× faster"), "{t}");
        let md = markdown(&r);
        assert!(md.contains("| Cat · noop | 50 ms"), "{md}");
        let h = html(&r);
        assert!(h.contains("<svg") && h.contains("4.0× faster"));
    }

    #[test]
    fn a_clear_slowdown_is_a_regression_and_noise_is_not() {
        let old = results(vec![m("w", "noop", "igl", &[1.0, 1.0, 1.0])]);
        let slower = results(vec![m("w", "noop", "igl", &[1.3, 1.3, 1.3])]);
        let (_, n) = compare(&old, &slower, "igl", 0.10);
        assert_eq!(n, 1);
        let noisy_old = results(vec![m("w", "noop", "igl", &[0.7, 1.0, 1.3])]);
        let (_, n) = compare(&noisy_old, &slower, "igl", 0.10);
        assert_eq!(n, 0, "within the old run's own spread");
        let overlapping = results(vec![m("w", "noop", "igl", &[1.05, 1.3, 1.31])]);
        let (_, n) = compare(&old, &overlapping, "igl", 0.10);
        assert_eq!(n, 1, "{:?}", Summary::of(&[1.05, 1.3, 1.31]));
        let old_spread = results(vec![m("w", "noop", "igl", &[0.9, 1.0, 1.1])]);
        let (_, n) = compare(&old_spread, &overlapping, "igl", 0.10);
        assert_eq!(n, 0, "the runs overlap");
    }

    #[test]
    fn a_machine_that_got_slower_for_both_tools_is_not_a_regression() {
        let old = results(vec![
            m("w", "build", "igl", &[1.0, 1.0, 1.0]),
            m("w", "build", "cargo", &[2.0, 2.0, 2.0]),
        ]);
        let busier = results(vec![
            m("w", "build", "igl", &[1.3, 1.3, 1.3]),
            m("w", "build", "cargo", &[2.6, 2.6, 2.6]),
        ]);
        let (text, n) = compare(&old, &busier, "igl", 0.10);
        assert_eq!(n, 0, "{text}");
        assert!(
            text.contains("+30.0 %") && text.contains("+0.0 %"),
            "{text}"
        );
    }
}
