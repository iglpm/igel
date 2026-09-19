use std::path::PathBuf;

/// Result of a rustc compilation.
#[derive(Debug, Clone)]
pub struct CompilationResult {
    pub success: bool,
    pub exit_code: i32,
    pub artifacts: Vec<ArtifactEvent>,
    pub diagnostics: Vec<Diagnostic>,
    pub stderr_raw: String,
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
        if line.starts_with('{') {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
                if value.get("$message_type").and_then(|v| v.as_str()) == Some("artifact") {
                    if let Ok(artifact) = serde_json::from_value::<ArtifactEvent>(value.clone()) {
                        artifacts.push(artifact);
                    }
                } else if value.get("message").is_some() && value.get("level").is_some() {
                    if let Ok(diag) = serde_json::from_value::<RawDiagnostic>(value) {
                        diagnostics.push(Diagnostic {
                            level: diag.level,
                            message: diag.message,
                            rendered: diag.rendered,
                        });
                    }
                }
            }
        }
    }

    (artifacts, diagnostics)
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
}
