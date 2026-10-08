use rune_common::BuildScriptOutput;
use rune_common::CompilationUnit;
use rune_graph::NodeId;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum PipelineEvent {
    BuildStarted {
        node_id: NodeId,
        unit: CompilationUnit,
    },
    MetadataReady {
        node_id: NodeId,
        rmeta_path: PathBuf,
    },
    BuildFinished {
        node_id: NodeId,
        artifacts: Vec<PathBuf>,
        duration: Duration,
        warnings: String,
        diagnostics: Vec<String>,
    },
    CacheHit {
        node_id: NodeId,
        artifacts: Vec<PathBuf>,
        warnings: String,
        diagnostics: Vec<String>,
    },
    BuildFailed {
        node_id: NodeId,
        error: String,
        diagnostics: Vec<String>,
    },
    ScriptOutput {
        node_id: NodeId,
        output: BuildScriptOutput,
    },
    ScriptWarnings {
        node_id: NodeId,
        warnings: String,
    },
    BuildScriptStarted {
        node_id: NodeId,
        crate_name: String,
    },
    BuildScriptFinished {
        node_id: NodeId,
        crate_name: String,
        duration: Duration,
    },
    AllComplete {
        total_duration: Duration,
        cached: usize,
        compiled: usize,
        failed: usize,
    },
}
