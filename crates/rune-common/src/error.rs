//! Shared error types for the Rune build system.

use std::path::PathBuf;

/// Top-level error type for Rune operations.
#[derive(Debug, thiserror::Error)]
pub enum RuneError {
    // -- Manifest errors --
    #[error("failed to read manifest at {path}: {source}")]
    ManifestRead {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to parse manifest at {path}: {message}")]
    ManifestParse { path: PathBuf, message: String },

    #[error("missing required field '{field}' in {path}")]
    ManifestMissingField { path: PathBuf, field: String },

    // -- Graph errors --
    #[error("dependency cycle detected: {cycle}")]
    DependencyCycle { cycle: String },

    #[error("unresolved dependency: {name} required by {required_by}")]
    UnresolvedDependency { name: String, required_by: String },

    // -- Build errors --
    #[error("rustc invocation failed for {crate_name}: {message}")]
    CompilationFailed { crate_name: String, message: String },

    #[error("build script failed for {crate_name}: exit code {exit_code}")]
    BuildScriptFailed { crate_name: String, exit_code: i32 },

    #[error("build script for {crate_name} produced invalid output: {message}")]
    BuildScriptInvalidOutput { crate_name: String, message: String },

    // -- Fingerprint errors --
    #[error("failed to compute fingerprint for {path}: {source}")]
    FingerprintError {
        path: PathBuf,
        source: std::io::Error,
    },

    // -- CAS errors --
    #[error("CAS store error: {message}")]
    CasError { message: String },

    // -- General IO --
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    // -- Catch-all --
    #[error("{0}")]
    Other(String),
}

/// A Result type alias using [`RuneError`].
pub type RuneResult<T> = Result<T, RuneError>;
