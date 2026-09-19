//! Lightweight phase timing, enabled with `RUNE_TIMINGS=1` (or `--timings`).
//!
//! Zero cost when disabled: [`phase`] returns a guard that does nothing.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Turns timing output on for the rest of the process.
pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

/// Whether timing output is on. Also true if `RUNE_TIMINGS` is set to
/// anything other than `0`/empty.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
        || std::env::var("RUNE_TIMINGS").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// Prints `name: elapsed` to stderr when dropped, if timings are enabled.
pub struct Phase {
    name: &'static str,
    start: Option<Instant>,
}

/// Starts timing a phase.
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
