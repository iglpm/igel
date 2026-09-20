use rune_common::CompilationUnit;
use rune_graph::NodeId;
use std::path::PathBuf;
use std::time::Duration;

/// Events emitted during the build pipeline.
#[derive(Debug, Clone)]
pub enum PipelineEvent {
    /// A compilation unit has started.
    BuildStarted {
        node_id: NodeId,
        unit: CompilationUnit,
    },
    /// A crate's metadata (.rmeta) is ready — dependents can start.
    MetadataReady {
        node_id: NodeId,
        rmeta_path: PathBuf,
    },
    /// A compilation unit finished successfully.
    BuildFinished {
        node_id: NodeId,
        artifacts: Vec<PathBuf>,
        duration: Duration,
        /// Rendered compiler warnings (empty if none), for local crates.
        warnings: String,
        /// The compiler's diagnostics as raw JSON, for local crates.
        diagnostics: Vec<String>,
    },
    /// A CAS cache hit — artifacts linked without compilation.
    CacheHit {
        node_id: NodeId,
        artifacts: Vec<PathBuf>,
        /// Warnings replayed from the build that populated the cache entry.
        warnings: String,
        /// Diagnostics (raw JSON) replayed from that build.
        diagnostics: Vec<String>,
    },
    /// A compilation unit failed.
    BuildFailed {
        node_id: NodeId,
        error: String,
        /// The compiler's diagnostics as raw JSON (errors included).
        diagnostics: Vec<String>,
    },
    /// Build script started.
    BuildScriptStarted { node_id: NodeId, crate_name: String },
    /// Build script finished.
    BuildScriptFinished {
        node_id: NodeId,
        crate_name: String,
        duration: Duration,
    },
    /// All units complete.
    AllComplete {
        total_duration: Duration,
        cached: usize,
        compiled: usize,
        failed: usize,
    },
}
