//! # rune-scheduler
//!
//! Parallel build task scheduler with DAG-based execution,
//! critical-path-first scheduling, and rmeta pipelining.

pub mod executor;
pub mod hints;
pub mod inputs;
pub mod pipeline;
pub mod progress;
pub mod schedule;

pub use executor::BuildExecutor;
pub use pipeline::PipelineEvent;
pub use progress::BuildProgress;
