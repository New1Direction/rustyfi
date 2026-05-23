use thiserror::Error;

use crate::state::RustyfiState;

/// Errors produced by illegal or malformed state transitions.
#[derive(Debug, Error)]
pub enum TransitionError {
    /// The requested event is not valid from the current state.
    #[error("illegal transition: event `{event}` is not valid from state `{from}`")]
    IllegalTransition {
        from: &'static str,
        event: &'static str,
    },

    /// The machine is in `Failed` and must be explicitly reset before any
    /// other transition is accepted.
    #[error("machine is in Failed state (origin: {origin_state}, recoverable: {recoverable}); call Reset before continuing")]
    MustResetBeforeContinuing {
        origin_state: String,
        recoverable: bool,
    },

    /// An event carried a payload that is structurally valid but semantically
    /// rejected (e.g., a retry count that already exceeds the ceiling).
    #[error("invalid event payload for `{event}`: {reason}")]
    InvalidPayload {
        event: &'static str,
        reason: &'static str,
    },

    /// Produced when the machine receives a `Retry` event but the retry
    /// ceiling has already been reached.
    #[error("retry ceiling reached: attempt {attempt} exceeds maximum {ceiling}")]
    RetryCeilingExceeded { attempt: u32, ceiling: u32 },

    /// Internal invariant violated — indicates a logic bug in the
    /// orchestrator, not in user input.
    #[error("internal orchestrator invariant violated: {detail}")]
    InternalInvariant { detail: &'static str },
}

/// Errors produced by the compiler feedback layer.
#[derive(Debug, Error)]
pub enum CompilerError {
    /// The subprocess could not be spawned.
    #[error("failed to spawn cargo subprocess: {reason}")]
    SpawnFailure { reason: String },

    /// The subprocess exited with a non-zero code.
    #[error("cargo exited with code {code}")]
    NonZeroExit { code: i32 },

    /// The subprocess was terminated by a signal (Unix only).
    #[error("cargo process was terminated by a signal")]
    SignalTermination,

    /// Raw stdout/stderr could not be captured as UTF-8.
    #[error("subprocess output is not valid UTF-8: {reason}")]
    OutputEncoding { reason: String },

    /// A line in the JSON message stream could not be deserialized.
    #[error("failed to parse compiler diagnostic JSON on line {line}: {reason}")]
    DiagnosticParse { line: usize, reason: String },
}

/// Errors produced when deserializing a [`ContextManifest`].
///
/// [`ContextManifest`]: crate::context::ContextManifest
#[derive(Debug, Error)]
pub enum ManifestError {
    /// The raw bytes are not valid JSON.
    #[error("manifest JSON is malformed: {reason}")]
    MalformedJson { reason: String },

    /// The JSON is valid but does not conform to the expected schema.
    #[error("manifest schema mismatch: {reason}")]
    SchemaMismatch { reason: String },

    /// A required field is absent from the manifest.
    #[error("manifest is missing required field `{field}`")]
    MissingField { field: &'static str },
}

/// Converts a [`RustyfiState`] reference to its static string discriminant,
/// used in error messages without allocating.
pub(crate) fn state_name(s: &RustyfiState) -> &'static str {
    match s {
        RustyfiState::Idle => "Idle",
        RustyfiState::Parsing(_) => "Parsing",
        RustyfiState::Scaffolding(_) => "Scaffolding",
        RustyfiState::Translating(_) => "Translating",
        RustyfiState::Verifying(_) => "Verifying",
        RustyfiState::Optimizing(_) => "Optimizing",
        RustyfiState::Completed(_) => "Completed",
        RustyfiState::Failed(_) => "Failed",
    }
}
