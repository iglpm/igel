//! # rune-compiler
//!
//! Direct rustc invocation and build script runner for the Rune build system.
//! This crate does NOT wrap Cargo — it constructs and executes rustc commands
//! directly for maximum control and speed.

pub mod build_script;
pub mod command;
pub mod invoke;
pub mod output;

pub use build_script::BuildScriptRunner;
pub use command::RustcCommandBuilder;
pub use invoke::RustcInvoker;
pub use output::{CompilationResult, diagnostic_lines};
