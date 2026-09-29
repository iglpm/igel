use std::path::PathBuf;

/// Result of a rustc compilation.
#[derive(Debug, Clone)]
pub struct CompilationResult {
    pub success: bool,
    pub exit_code: i32,
    pub artifacts: Vec<ArtifactEvent>,
    pub diagnostics: Vec<Diagnostic>,
    pub stderr_raw: String,
    /// The compiler's peak memory (resident set) in bytes, where the
    /// platform reports it.
    pub peak_memory: Option<u64>,
}

/// An artifact emitted by rustc (from --json=artifacts).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ArtifactEvent {
    pub artifact: PathBuf,
    pub emit: String,
}

/// A compiler diagnostic.
#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub level: String,
    pub message: String,
    pub rendered: Option<String>,
}

#[derive(serde::Deserialize)]
pub(crate) struct RawDiagnostic {
    pub(crate) level: String,
    pub(crate) message: String,
    pub(crate) rendered: Option<String>,
}

/// Parses line-by-line JSON from rustc stderr.
/// Lines that aren't valid JSON are raw compiler output and are ignored here.
pub fn parse_rustc_output(stderr: &str) -> (Vec<ArtifactEvent>, Vec<Diagnostic>) {
    let mut artifacts = Vec::new();
    let mut diagnostics = Vec::new();

    for line in stderr.lines() {
        if line.starts_with('{')
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(line)
        {
            if value.get("$message_type").and_then(|v| v.as_str()) == Some("artifact") {
                if let Ok(artifact) = serde_json::from_value::<ArtifactEvent>(value.clone()) {
                    artifacts.push(artifact);
                }
            } else if value.get("message").is_some()
                && value.get("level").is_some()
                && let Ok(diag) = serde_json::from_value::<RawDiagnostic>(value)
            {
                diagnostics.push(Diagnostic {
                    level: diag.level,
                    message: diag.message,
                    rendered: diag.rendered,
                });
            }
        }
    }

    (artifacts, diagnostics)
}

/// The raw JSON of every diagnostic rustc printed, one string per message,
/// exactly as rustc wrote it (so tools such as rust-analyzer see the same
/// structure — spans, codes, suggestions — they would from Cargo).
pub fn diagnostic_lines(stderr: &str) -> Vec<String> {
    stderr
        .lines()
        .filter(|line| line.starts_with('{'))
        .filter(|line| {
            serde_json::from_str::<serde_json::Value>(line).is_ok_and(|v| {
                v.get("$message_type").and_then(|t| t.as_str()) == Some("diagnostic")
                    || (v.get("message").is_some() && v.get("level").is_some())
            })
        })
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_rustc_output() {
        let output = r#"{"$message_type":"artifact","artifact":"/tmp/foo.rmeta","emit":"metadata"}
{"message":"unused variable: `x`","code":{"code":"unused_variables","explanation":null},"level":"warning","spans":[],"children":[],"rendered":"warning: unused variable: `x`\n"}
normal line
"#;
        let (artifacts, diagnostics) = parse_rustc_output(output);
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].emit, "metadata");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].level, "warning");
    }

    #[test]
    fn diagnostic_lines_keep_only_diagnostics_verbatim() {
        let output = r#"{"$message_type":"artifact","artifact":"/tmp/foo.rmeta","emit":"metadata"}
{"$message_type":"diagnostic","message":"unused variable: `x`","level":"warning","spans":[],"children":[],"rendered":"w\n"}
normal line
{"message":"old style","level":"error","spans":[],"children":[],"rendered":null}
"#;
        let lines = diagnostic_lines(output);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("unused variable"));
        assert!(lines[1].contains("old style"));
    }
}
