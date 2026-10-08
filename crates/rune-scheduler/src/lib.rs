//! Parallel build scheduling: critical-path-first with `.rmeta` pipelining.

pub mod executor;
pub mod hints;
pub mod inputs;
pub mod pipeline;
pub mod progress;
pub mod schedule;
pub mod tokens;

pub use executor::BuildExecutor;
pub use pipeline::PipelineEvent;
pub use progress::BuildProgress;
