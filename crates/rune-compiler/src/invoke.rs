use crate::output::{ArtifactEvent, CompilationResult, parse_rustc_output};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[derive(thiserror::Error, Debug)]
pub enum InvokeError {
    #[error("I/O error during invocation: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, InvokeError>;

/// Engine for executing `rustc`.
pub struct RustcInvoker {
    rustc_path: PathBuf,
}

impl Default for RustcInvoker {
    fn default() -> Self {
        Self::new()
    }
}

impl RustcInvoker {
    /// Discovers rustc via the `RUSTC` env var or defaults to "rustc".
    pub fn new() -> Self {
        let rustc_path = std::env::var_os("RUSTC")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("rustc"));
        Self { rustc_path }
    }

    /// Returns the path to the rustc binary.
    pub fn rustc_path(&self) -> &PathBuf {
        &self.rustc_path
    }

    /// Invoke `rustc` synchronously using a pre-built `Command`.
    ///
    /// The caller should have created the `Command` via
    /// `RustcCommandBuilder::build()` which already sets the
    /// program to `rustc`. This method just runs it and parses
    /// the output.
    pub fn invoke(&self, mut cmd: Command) -> Result<CompilationResult> {
        let output = cmd.output()?;
        let stderr_raw = String::from_utf8_lossy(&output.stderr).into_owned();
        let (artifacts, diagnostics) = parse_rustc_output(&stderr_raw);

        Ok(CompilationResult {
            success: output.status.success(),
            exit_code: output.status.code().unwrap_or(1),
            artifacts,
            diagnostics,
            stderr_raw,
        })
    }

    /// Invoke `rustc` and stream stderr, calling `on_metadata`
    /// as soon as the `.rmeta` is ready.
    ///
    /// This is the key pipelining optimization: downstream crates
    /// can begin compiling as soon as metadata is available, without
    /// waiting for the full codegen/linking phase.
    pub fn invoke_pipelined(
        &self,
        cmd: Command,
        on_metadata: impl FnOnce(PathBuf),
    ) -> Result<CompilationResult> {
        let mut on_metadata = Some(on_metadata);
        self.invoke_streaming(cmd, &mut |artifact| {
            if artifact.emit == "metadata" {
                if let Some(cb) = on_metadata.take() {
                    cb(artifact.artifact.clone());
                }
            }
        })
    }

    /// Invoke `rustc`, calling `on_artifact` for every artifact it announces
    /// (`dep-info`, `metadata`, `link`) the moment it does. rustc announces
    /// dep-info — which lists the environment variables read by `env!` — before
    /// it announces metadata, so a caller can react to it before dependents
    /// start.
    pub fn invoke_streaming(
        &self,
        mut cmd: Command,
        on_artifact: &mut dyn FnMut(&ArtifactEvent),
    ) -> Result<CompilationResult> {
        cmd.stderr(Stdio::piped());
        cmd.stdout(Stdio::null());

        let mut child = cmd.spawn()?;
        let stderr = child.stderr.take().unwrap();

        let mut stderr_raw = String::new();
        let mut artifacts = Vec::new();
        let mut diagnostics = Vec::new();
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();

        while reader.read_line(&mut line)? > 0 {
            stderr_raw.push_str(&line);

            if line.starts_with('{') {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
                    if value.get("$message_type").and_then(|v| v.as_str()) == Some("artifact") {
                        if let Ok(artifact) = serde_json::from_value::<ArtifactEvent>(value.clone())
                        {
                            on_artifact(&artifact);
                            artifacts.push(artifact);
                        }
                    } else if value.get("message").is_some() && value.get("level").is_some() {
                        if let Ok(diag) =
                            serde_json::from_value::<crate::output::RawDiagnostic>(value)
                        {
                            diagnostics.push(crate::output::Diagnostic {
                                level: diag.level,
                                message: diag.message,
                                rendered: diag.rendered,
                            });
                        }
                    }
                }
            }
            line.clear();
        }

        let status = child.wait()?;

        Ok(CompilationResult {
            success: status.success(),
            exit_code: status.code().unwrap_or(1),
            artifacts,
            diagnostics,
            stderr_raw,
        })
    }
}
