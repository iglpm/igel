//! Phase timing, enabled with `RUNE_TIMINGS=1` (or `--timings`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

static ENABLED: AtomicBool = AtomicBool::new(false);

pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
        || std::env::var("RUNE_TIMINGS").is_ok_and(|v| !v.is_empty() && v != "0")
}

pub struct Phase {
    name: &'static str,
    start: Option<Instant>,
}

pub fn phase(name: &'static str) -> Phase {
    Phase {
        name,
        start: enabled().then(Instant::now),
    }
}

impl Drop for Phase {
    fn drop(&mut self) {
        if let Some(start) = self.start {
            eprintln!(
                "  [timing] {:<24} {:>8.1} ms",
                self.name,
                start.elapsed().as_secs_f64() * 1000.0
            );
        }
    }
}
