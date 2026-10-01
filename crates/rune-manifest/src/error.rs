use std::io;
use std::path::PathBuf;
use thiserror::Error;

/// Errors that can occur when parsing a manifest.
#[derive(Debug, Error)]
pub enum ManifestError {
    /// IO error reading the manifest.
    #[error("Failed to read manifest at {path}: {source}")]
    Io {
        /// Path to the manifest file.
        path: PathBuf,
        /// Underlying IO error.
        source: io::Error,
    },

    /// Parse error for TOML.
    #[error("invalid TOML in {path}:\n{source}")]
    TomlParse {
        /// Path to the manifest file.
        path: PathBuf,
        /// Underlying TOML parsing error.
        source: toml::de::Error,
    },

    /// Missing required field.
    #[error("Missing required field '{field}' in {path}")]
    MissingField {
        /// Path to the manifest file.
        path: PathBuf,
        /// Missing field name.
        field: String,
    },

    /// Invalid value for a field.
    #[error("Invalid value for '{field}' in {path}: {message}")]
    InvalidValue {
        /// Path to the manifest file.
        path: PathBuf,
        /// Field name.
        field: String,
        /// Error message.
        message: String,
    },

    /// General parse error.
    #[error("Parse error: {0}")]
    Parse(String),

    /// Missing manifest file.
    #[error("Manifest file not found at {0}")]
    NotFound(PathBuf),

    /// Glob error.
    #[error("Glob pattern error: {0}")]
    Glob(#[from] glob::PatternError),
}

impl ManifestError {
    /// Whether the message already names the manifest it is about, so a
    /// caller need not add "while reading <path>" around it.
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
