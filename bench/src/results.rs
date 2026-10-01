//! What a benchmark run found, as written to `results.json` and read back
//! by `report` and `compare`.

use serde::{Deserialize, Serialize};

use crate::measure::Sample;
use crate::stats::Summary;

pub const FORMAT: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Results {
    pub format: u32,
    pub meta: Meta,
    pub measurements: Vec<Measurement>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    /// Seconds since the Unix epoch when the run started.
    pub started: u64,
    pub suite: String,
    pub tools: Vec<crate::tool::Tool>,
    pub rustc: String,
    pub host: String,
    pub cpus: usize,
    pub memory_bytes: Option<u64>,
    pub os: String,
    /// The Rune checkout's commit, when run from one.
    pub commit: Option<String>,
    pub runs_heavy: usize,
    pub runs_light: usize,
    pub workloads: Vec<WorkloadInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkloadInfo {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Measurement {
    pub workload: String,
    pub scenario: String,
    pub category: String,
    pub title: String,
    pub about: String,
    pub tool: String,
    /// `s` (wall seconds) or `bytes`.
    pub unit: String,
    /// The measured value of each run.
    pub values: Vec<f64>,
    pub summary: Summary,
    /// CPU seconds (user + system) per run, for time measurements.
    pub cpu: Summary,
    /// Peak memory of the largest process over all runs, KiB.
    pub peak_rss_kib: u64,
    pub samples: Vec<Sample>,
    /// Runs that failed (not counted in `values`).
    pub failures: usize,
    /// Why it could not be measured, or what a failed run printed last.
    pub note: Option<String>,
    /// Disk: bytes in the project alone (the rest is Rune's store).
    #[serde(default)]
    pub project_bytes: Option<u64>,
}

impl Results {
    /// The measurement of `tool` for a workload and scenario.
    pub fn find(&self, workload: &str, scenario: &str, tool: &str) -> Option<&Measurement> {
        self.measurements
            .iter()
            .find(|m| m.workload == workload && m.scenario == scenario && m.tool == tool)
    }

    /// Workload names in the order they ran.
    pub fn workloads(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for m in &self.measurements {
            if !out.contains(&m.workload) {
                out.push(m.workload.clone());
            }
        }
        out
    }

    /// Scenario ids measured for `workload`, in order.
    pub fn scenarios(&self, workload: &str) -> Vec<&Measurement> {
        let mut out: Vec<&Measurement> = Vec::new();
        for m in self.measurements.iter().filter(|m| m.workload == workload) {
            if !out.iter().any(|o| o.scenario == m.scenario) {
                out.push(m);
            }
        }
        out
    }
}
