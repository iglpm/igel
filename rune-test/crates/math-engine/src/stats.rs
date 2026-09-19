/// Summary statistics for a numeric dataset.
#[derive(Debug, Clone, PartialEq)]
pub struct DatasetStats {
    pub count: usize,
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub variance: f64,
    pub std_dev: f64,
}

/// Computes statistical metrics on a slice of f64 numbers.
pub fn compute_stats(data: &[f64]) -> Option<DatasetStats> {
    if data.is_empty() {
        return None;
    }
    let count = data.len();
    let mut min = f64::MAX;
    let mut max = f64::MIN;
    let mut sum = 0.0;

    for &x in data {
        if x < min { min = x; }
        if x > max { max = x; }
        sum += x;
    }
    let mean = sum / count as f64;

    let variance = data.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / count as f64;
    let std_dev = variance.sqrt();

    Some(DatasetStats {
        count,
        min,
        max,
        mean,
        variance,
        std_dev,
    })
}
