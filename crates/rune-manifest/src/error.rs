use std::io;
use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("Failed to read manifest at {path}: {source}")]
    Io { path: PathBuf, source: io::Error },

    #[error("invalid TOML in {path}:\n{source}")]
    TomlParse {
        path: PathBuf,
        source: toml::de::Error,
    },

    #[error("Missing required field '{field}' in {path}")]
    MissingField { path: PathBuf, field: String },

    #[error("Invalid value for '{field}' in {path}: {message}")]
    InvalidValue {
        path: PathBuf,
        field: String,
        message: String,
    },

    #[error("Parse error: {0}")]
    Parse(String),

    #[error("Manifest file not found at {0}")]
    NotFound(PathBuf),

    #[error("Glob pattern error: {0}")]
    Glob(#[from] glob::PatternError),
}

impl ManifestError {
    pub fn names_path(&self) -> bool {
        matches!(
            self,
            Self::Io { .. }
                | Self::TomlParse { .. }
                | Self::MissingField { .. }
                | Self::InvalidValue { .. }
                | Self::NotFound(_)
        )
    }
}
