//! Direct rustc invocation and build script runner (no Cargo).

pub mod build_script;
pub mod command;
pub mod invoke;
pub mod output;

pub use build_script::BuildScriptRunner;
pub use command::RustcCommandBuilder;
pub use invoke::RustcInvoker;
pub use output::{CompilationResult, diagnostic_lines};
