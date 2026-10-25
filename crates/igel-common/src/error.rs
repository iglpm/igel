use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum IgelError {
    #[error("failed to read manifest at {path}: {source}")]
    ManifestRead {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to parse manifest at {path}: {message}")]
    ManifestParse { path: PathBuf, message: String },

    #[error("missing required field '{field}' in {path}")]
    ManifestMissingField { path: PathBuf, field: String },

    #[error("dependency cycle detected: {cycle}")]
    DependencyCycle { cycle: String },

    #[error("unresolved dependency: {name} required by {required_by}")]
    UnresolvedDependency { name: String, required_by: String },

    #[error("rustc invocation failed for {crate_name}: {message}")]
    CompilationFailed { crate_name: String, message: String },

    #[error("build script failed for {crate_name}: exit code {exit_code}")]
    BuildScriptFailed { crate_name: String, exit_code: i32 },

    #[error("build script for {crate_name} produced invalid output: {message}")]
    BuildScriptInvalidOutput { crate_name: String, message: String },

    #[error("failed to compute fingerprint for {path}: {source}")]
    FingerprintError {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("CAS store error: {message}")]
    CasError { message: String },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(String),
}

pub type IgelResult<T> = Result<T, IgelError>;
