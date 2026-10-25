use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Summary {
    pub n: usize,
    pub median: f64,
    pub mean: f64,
    pub min: f64,
    pub max: f64,
    pub stddev: f64,
}

impl Summary {
    pub fn of(values: &[f64]) -> Self {
        let n = values.len();
        if n == 0 {
            return Self::default();
        }
        let mut sorted = values.to_vec();
        sorted.sort_by(|a, b| a.total_cmp(b));
        let median = if n % 2 == 1 {
            sorted[n / 2]
        } else {
            (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
        };
        let mean = sorted.iter().sum::<f64>() / n as f64;
        let stddev = if n > 1 {
            (sorted.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1) as f64).sqrt()
        } else {
            0.0
        };
        Self {
            n,
            median,
            mean,
            min: sorted[0],
            max: sorted[n - 1],
            stddev,
        }
    }

    pub fn cv(&self) -> f64 {
        if self.mean > 0.0 {
            self.stddev / self.mean
        } else {
            0.0
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Comparison {
    pub speedup: f64,
    pub decisive: bool,
    pub within_noise: bool,
}

pub fn compare(igl: &Summary, cargo: &Summary) -> Option<Comparison> {
    if igl.n == 0 || cargo.n == 0 || igl.median <= 0.0 {
        return None;
    }
    let speedup = cargo.median / igl.median;
    let decisive = igl.max < cargo.min || cargo.max < igl.min;
    let spread = igl.cv().max(cargo.cv());
    Some(Comparison {
        speedup,
        decisive,
        within_noise: !decisive && igl.n > 1 && cargo.n > 1 && (speedup.ln().abs() <= spread),
    })
}

pub fn describe(speedup: f64) -> String {
    if (0.95..=1.05).contains(&speedup) {
        "about the same".to_string()
    } else if speedup > 1.0 {
        format!("{speedup:.1}× faster")
    } else {
        format!("{:.1}× slower", 1.0 / speedup)
    }
}

pub fn seconds(s: f64) -> String {
    if s < 1.0 {
        format!("{:.0} ms", s * 1000.0)
    } else if s < 100.0 {
        format!("{s:.2} s")
    } else {
        format!("{} min {:02.0} s", (s / 60.0) as u64, s % 60.0)
    }
}

pub fn bytes(n: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{v:.0} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_mean_and_spread() {
        let s = Summary::of(&[3.0, 1.0, 2.0, 10.0]);
        assert_eq!(s.n, 4);
        assert_eq!(s.median, 2.5);
        assert_eq!(s.mean, 4.0);
        assert_eq!((s.min, s.max), (1.0, 10.0));
        assert!((s.stddev - 4.0826).abs() < 1e-3);
        assert_eq!(Summary::of(&[]).n, 0);
    }

    #[test]
    fn comparisons_read_naturally() {
        let igl = Summary::of(&[1.0, 1.1]);
        let cargo = Summary::of(&[3.0, 3.3]);
        let c = compare(&igl, &cargo).unwrap();
        assert!(c.decisive && !c.within_noise);
        assert_eq!(describe(c.speedup), "3.0× faster");
        let a = Summary::of(&[1.0, 1.4, 0.8]);
        let b = Summary::of(&[1.1, 0.9, 1.3]);
        assert!(compare(&a, &b).unwrap().within_noise);
        assert_eq!(describe(0.5), "2.0× slower");
        assert_eq!(describe(1.01), "about the same");
        assert_eq!(seconds(0.0123), "12 ms");
        assert_eq!(seconds(125.0), "2 min 05 s");
        assert_eq!(bytes(1536.0 * 1024.0), "1.5 MiB");
    }
}
