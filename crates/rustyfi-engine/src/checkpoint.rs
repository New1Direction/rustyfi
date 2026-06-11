/// Resumable per-phase checkpoint store for the Rustyfi pipeline.
///
/// Each pipeline phase writes a typed JSON checkpoint to disk when it
/// completes successfully.  On the next run with the same `run_dir`, the
/// pipeline reads existing checkpoints and skips completed phases rather
/// than re-running them.
///
/// This gives:
/// - **Resumable stages** — crash mid-translation? resume from the last
///   successfully-translated file.
/// - **Persisted intermediate artifacts** — each phase has an isolated
///   artifact sub-directory.
/// - **Stage-local retries** — retry counters live inside each phase's
///   checkpoint so they survive process restarts.
/// - **Deterministic replay** — re-run with `force = true` to replay from
///   any chosen stage without touching later checkpoints.
use std::fs;
use std::path::{Path, PathBuf};

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tracing::{debug, warn};

use crate::graph::EdgeRecord;
use crate::slicer::OwnershipGraph;
use crate::EngineError;

// ---------------------------------------------------------------------------
// CheckpointStore
// ---------------------------------------------------------------------------

/// Manages the per-run checkpoint directory tree.
///
/// ```text
/// <run_dir>/
///   checkpoints/
///     analysis.json
///     scaffold.json
///     translation.json    ← updated incrementally per file
///     verification.json
///     packaging.json
///   artifacts/
///     analysis/
///     scaffold/
///     translation/
///     verification/
///     packaging/
/// ```
pub struct CheckpointStore {
    checkpoints_dir: PathBuf,
    pub artifacts_dir: PathBuf,
}

impl CheckpointStore {
    /// Create (or reopen) a checkpoint store rooted at `run_dir`.
    pub fn new(run_dir: &Path) -> Result<Self, EngineError> {
        let checkpoints_dir = run_dir.join("checkpoints");
        let artifacts_dir = run_dir.join("artifacts");
        fs::create_dir_all(&checkpoints_dir).map_err(|e| EngineError::Io(e.to_string()))?;
        fs::create_dir_all(&artifacts_dir).map_err(|e| EngineError::Io(e.to_string()))?;
        Ok(Self {
            checkpoints_dir,
            artifacts_dir,
        })
    }

    /// Write a typed checkpoint for `phase`.
    pub fn write<T: Serialize>(&self, phase: &str, data: &T) -> Result<(), EngineError> {
        let path = self.checkpoint_path(phase);
        let json = serde_json::to_string_pretty(data)
            .map_err(|e| EngineError::Io(format!("checkpoint serialize: {e}")))?;
        fs::write(&path, json)
            .map_err(|e| EngineError::Io(format!("checkpoint write {phase}: {e}")))?;
        debug!("Checkpoint written: {}", path.display());
        Ok(())
    }

    /// Read and deserialize an existing checkpoint for `phase`.
    /// Returns `None` if the checkpoint does not exist or cannot be parsed.
    pub fn read<T: DeserializeOwned>(&self, phase: &str) -> Option<T> {
        let path = self.checkpoint_path(phase);
        let json = fs::read_to_string(&path).ok()?;
        match serde_json::from_str(&json) {
            Ok(v) => Some(v),
            Err(e) => {
                warn!("Failed to deserialize checkpoint `{phase}`: {e} — will re-run phase");
                None
            }
        }
    }

    /// Returns `true` if a valid checkpoint exists for `phase`.
    pub fn is_done(&self, phase: &str) -> bool {
        self.checkpoint_path(phase).exists()
    }

    /// Delete the checkpoint for `phase` (forces re-run on next invocation).
    pub fn invalidate(&self, phase: &str) {
        let path = self.checkpoint_path(phase);
        if path.exists() {
            let _ = fs::remove_file(&path);
            debug!("Checkpoint invalidated: {}", path.display());
        }
    }

    /// Delete all checkpoints at and after `from_phase` (invalidates the
    /// tail of the pipeline, useful after a manual source edit).
    pub fn invalidate_from(&self, from_phase: &str) {
        let order = phase_order();
        let start = order.iter().position(|&p| p == from_phase).unwrap_or(0);
        for phase in &order[start..] {
            self.invalidate(phase);
        }
    }

    /// List all phases that have a completed checkpoint.
    pub fn completed_phases(&self) -> Vec<String> {
        phase_order()
            .iter()
            .filter(|&&p| self.is_done(p))
            .map(|p| p.to_string())
            .collect()
    }

    /// Return the isolated artifact sub-directory for a phase.
    /// Created on demand.
    pub fn artifact_dir(&self, phase: &str) -> PathBuf {
        let d = self.artifacts_dir.join(phase);
        let _ = fs::create_dir_all(&d);
        d
    }

    fn checkpoint_path(&self, phase: &str) -> PathBuf {
        self.checkpoints_dir.join(format!("{phase}.json"))
    }
}

/// Canonical phase ordering (used by `invalidate_from`).
fn phase_order() -> &'static [&'static str] {
    &[
        "analysis",
        "scaffold",
        "contract",
        "translation",
        "verification",
        "packaging",
    ]
}

// ---------------------------------------------------------------------------
// Per-phase data types
// ---------------------------------------------------------------------------

/// Serializable output of the Analysis phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisCheckpoint {
    pub source_dir: PathBuf,
    pub crate_name: String,
    pub language: String,
    pub target_paths: Vec<PathBuf>,
    pub inferred_entrypoints: Vec<PathBuf>,
    /// Serialised dependency edges (from → to).
    pub edges: Vec<EdgeRecord>,
    pub warning_count: usize,
    pub produced_at: String,
}

/// Serializable output of the Scaffold phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScaffoldCheckpoint {
    pub workspace_path: PathBuf,
    pub crate_name: String,
    pub module_plan: Vec<String>,
}

/// The canonical Rust API surface for one source package.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PackageContract {
    pub root_segment: String,
    pub package: String,
    pub is_entrypoint: bool,
    /// Authoritative `pub struct`/`enum`/`trait`/`type` definitions (full
    /// bodies) — written once into the package's `mod.rs`.
    pub data_surface: String,
    /// `pub fn`/method signature lines — injected into body prompts as context.
    pub signatures: String,
}

/// Serializable output of the Contract phase: one entry per package.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContractCheckpoint {
    pub contracts: Vec<PackageContract>,
}

/// The translation result for a single source file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileTranslation {
    /// Absolute path to the source file.
    pub source_path: PathBuf,
    /// Absolute path to the generated `.rs` file.
    pub rust_path: PathBuf,
    /// Module name (stem of `rust_path`).
    pub module_name: String,
    /// How many LLM attempts were needed.
    pub attempt_count: u32,
    /// `true` if the LLM succeeded; `false` if a placeholder was written.
    pub succeeded: bool,
}

/// Serializable output of the Translation phase.
///
/// Written incrementally: after every successfully-translated file, the
/// checkpoint is updated.  This allows resuming mid-translation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranslationCheckpoint {
    pub completed: Vec<FileTranslation>,
    /// Index of the next pending file (into the original target list).
    pub next_index: usize,
    pub module_names: Vec<String>,
    /// Accumulated Rust signatures keyed by source file path.
    /// Persisted so that resume has full ownership context.
    #[serde(default)]
    pub ownership: OwnershipGraph,
    /// Total number of semantic chunks processed across all files.
    pub total_chunks_processed: usize,
}

/// Classification summary for one fix cycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixCycleSummary {
    pub attempt: u32,
    pub error_count: usize,
    /// The dominant `DiagnosticFamily` names seen in this cycle.
    pub dominant_families: Vec<String>,
    pub resolved: bool,
}

/// Serializable output of the Verification phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationCheckpoint {
    pub exit_clean: bool,
    pub fix_cycles: Vec<FixCycleSummary>,
    pub final_error_count: usize,
}

/// Serializable output of the Packaging phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackagingCheckpoint {
    pub zip_path: PathBuf,
    pub zip_bytes: usize,
    pub crate_name: String,
}
