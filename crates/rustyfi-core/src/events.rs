use std::path::PathBuf;

use crate::context::ContextManifest;
use crate::state::{CargoOutput, FailureReason, ReleaseConfig};

/// All events that can drive state transitions in the Rustyfi orchestrator.
///
/// Each variant carries exactly the payload required by the transition it
/// triggers.  Consumers construct events and hand them to
/// [`crate::transitions::Orchestrator::transition`]; the orchestrator is
/// responsible for deciding whether the transition is legal.
#[derive(Debug, Clone)]
pub enum StateEvent {
    // ------------------------------------------------------------------
    // Idle → Parsing
    // ------------------------------------------------------------------
    /// A language-analysis worker has finished and produced a validated
    /// [`ContextManifest`].  Triggers ingestion and moves to `Parsing`.
    StartParsing { manifest: Box<ContextManifest> },

    // ------------------------------------------------------------------
    // Parsing → Scaffolding
    // ------------------------------------------------------------------
    /// Parsing has completed successfully.  The payload provides the
    /// workspace skeleton that was determined from the parsed manifest.
    ParseComplete {
        workspace_path: PathBuf,
        /// Mapping from source package name → Cargo crate name.
        dependency_manifest: std::collections::HashMap<String, String>,
        module_layout_plan: Vec<String>,
    },

    // ------------------------------------------------------------------
    // Scaffolding → Translating
    // ------------------------------------------------------------------
    /// The Cargo workspace skeleton has been written to disk and the
    /// orchestrator is ready to begin chunk-by-chunk translation.
    ScaffoldComplete {
        first_file: PathBuf,
        total_chunks: u32,
        retry_ceiling: u32,
    },

    // ------------------------------------------------------------------
    // Translating → Translating  (same-state advancement)
    // ------------------------------------------------------------------
    /// A chunk was successfully generated.  Advance the chunk cursor.
    /// If `next_chunk_index == total_chunks`, the caller should emit
    /// `TranslationComplete` instead.
    ChunkAccepted { next_chunk_index: u32 },

    /// A chunk generation failed.  Increment the attempt counter; the
    /// orchestrator will return [`TransitionError::RetryCeilingExceeded`]
    /// if the ceiling has been reached rather than silently accepting.
    ///
    /// [`TransitionError::RetryCeilingExceeded`]: crate::errors::TransitionError::RetryCeilingExceeded
    ChunkRetry { reason: String },

    /// All chunks for all files have been translated successfully.
    TranslationComplete {
        cargo_output: CargoOutput,
        retry_ceiling: u32,
    },

    // ------------------------------------------------------------------
    // Verifying → Optimizing  (happy path)
    // ------------------------------------------------------------------
    /// `cargo check` returned zero diagnostics at error level.  The
    /// workspace is clean; proceed to optimization.
    VerifyPassed { release_config: ReleaseConfig },

    // ------------------------------------------------------------------
    // Verifying → Translating  (retry path)
    // ------------------------------------------------------------------
    /// `cargo check` returned errors that can be addressed by re-running
    /// translation.  The orchestrator validates that the retry ceiling has
    /// not been exceeded before accepting this event.
    VerifyRetry {
        /// The file that needs to be re-translated.
        target_file: PathBuf,
        /// Chunk to restart from.
        chunk_index: u32,
        total_chunks: u32,
        retry_ceiling: u32,
    },

    // ------------------------------------------------------------------
    // Optimizing → Completed
    // ------------------------------------------------------------------
    /// All optimization passes have completed and final binaries are
    /// available.
    OptimizationComplete {
        artifact_locations: Vec<PathBuf>,
        build_metadata: std::collections::HashMap<String, String>,
    },

    // ------------------------------------------------------------------
    // * → Failed
    // ------------------------------------------------------------------
    /// Any state may transition to `Failed` when an unrecoverable condition
    /// is detected.
    Fail {
        reason: FailureReason,
        recoverable: bool,
    },

    // ------------------------------------------------------------------
    // Failed → Idle
    // ------------------------------------------------------------------
    /// Resets a `Failed` machine to `Idle` so a new run can be started.
    /// This is the only event accepted in `Failed`.
    Reset,
}

impl StateEvent {
    /// Returns a static string discriminant for use in error messages,
    /// without allocating.
    pub fn name(&self) -> &'static str {
        match self {
            StateEvent::StartParsing { .. } => "StartParsing",
            StateEvent::ParseComplete { .. } => "ParseComplete",
            StateEvent::ScaffoldComplete { .. } => "ScaffoldComplete",
            StateEvent::ChunkAccepted { .. } => "ChunkAccepted",
            StateEvent::ChunkRetry { .. } => "ChunkRetry",
            StateEvent::TranslationComplete { .. } => "TranslationComplete",
            StateEvent::VerifyPassed { .. } => "VerifyPassed",
            StateEvent::VerifyRetry { .. } => "VerifyRetry",
            StateEvent::OptimizationComplete { .. } => "OptimizationComplete",
            StateEvent::Fail { .. } => "Fail",
            StateEvent::Reset => "Reset",
        }
    }
}
