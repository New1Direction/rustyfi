use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::context::{LanguageMetadata, SourceTarget};

// ---------------------------------------------------------------------------
// Per-state context structs
// ---------------------------------------------------------------------------

/// Context carried while the machine is parsing an incoming
/// [`ContextManifest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsingContext {
    /// The ordered list of files scheduled for translation.
    pub source_targets: Vec<SourceTarget>,
    /// Primary language of the workspace.
    pub language_metadata: LanguageMetadata,
    /// Parser-specific metadata (e.g. AST statistics, parse duration in ms).
    pub parser_metadata: std::collections::HashMap<String, String>,
}

/// Context carried while the machine is setting up the Cargo workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScaffoldingContext {
    /// Absolute path to the generated Cargo workspace root.
    pub workspace_path: PathBuf,
    /// Mapping from source package name → Cargo crate name.
    pub dependency_manifest: std::collections::HashMap<String, String>,
    /// Planned Rust module hierarchy (path segments).
    pub module_layout_plan: Vec<String>,
}

/// Context carried during active file-by-file translation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranslatingContext {
    /// The source file currently being translated.
    pub current_source_file: PathBuf,
    /// 0-based index of the current chunk within the current file.
    pub chunk_index: u32,
    /// Total number of chunks for the current file.
    pub total_chunks: u32,
    /// How many generation attempts have been made for the current chunk.
    pub generation_attempt: u32,
    /// Maximum number of generation attempts before the machine transitions
    /// to `Failed`.
    pub retry_ceiling: u32,
}

/// Context carried during Cargo verification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyingContext {
    /// Raw output captured from `cargo check --message-format=json`.
    pub cargo_output: CargoOutput,
    /// Structured diagnostics parsed from the JSON message stream.
    pub diagnostics: Vec<CompilerDiagnostic>,
    /// How many verification attempts have been made for the current state.
    pub verification_attempt: u32,
    /// Maximum number of verification attempts before the machine transitions
    /// to `Failed`.
    pub retry_ceiling: u32,
}

/// Context carried during the optimization phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptimizingContext {
    /// Optimization configuration (e.g. `opt-level`, `lto`, `codegen-units`).
    pub release_config: ReleaseConfig,
    /// Paths to binary artifacts produced so far.
    pub produced_artifacts: Vec<PathBuf>,
    /// Human-readable labels for optimization passes completed.
    pub completed_passes: Vec<String>,
}

/// Context carried in the terminal `Completed` state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletedContext {
    /// Paths to all final binary/library artifacts.
    pub artifact_locations: Vec<PathBuf>,
    /// Build metadata (e.g. rustc version, build timestamp, feature flags).
    pub build_metadata: std::collections::HashMap<String, String>,
}

/// Context carried in the terminal `Failed` state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailedContext {
    /// A structured description of why the machine failed.
    pub reason: FailureReason,
    /// The name of the state in which the failure occurred (static str
    /// converted to `String` to satisfy `Deserialize` lifetime bounds).
    pub originating_state: String,
    /// Whether the failure is considered recoverable (i.e., a Reset + retry
    /// may succeed without human intervention).
    pub recoverable: bool,
}

// ---------------------------------------------------------------------------
// Supporting value types for state contexts
// ---------------------------------------------------------------------------

/// Structured description of a terminal failure.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum FailureReason {
    /// Translation generation exceeded the retry ceiling.
    TranslationRetryCeilingExceeded {
        file: PathBuf,
        chunk_index: u32,
        attempts: u32,
    },
    /// Cargo verification exceeded the retry ceiling.
    VerificationRetryCeilingExceeded {
        attempts: u32,
        last_diagnostics: Vec<CompilerDiagnostic>,
    },
    /// An unrecoverable compiler error was returned.
    CompilerError { message: String },
    /// The incoming manifest failed validation.
    ManifestValidationFailed { detail: String },
    /// A required file was not found on the filesystem.
    FileNotFound { path: PathBuf },
    /// An internal invariant was violated.
    InternalInvariant { detail: String },
}


/// Cargo release-profile optimization settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseConfig {
    /// LLVM optimization level (0–3, or `"s"` / `"z"`).
    pub opt_level: String,
    /// Link-time optimization mode.
    pub lto: LtoMode,
    /// Number of parallel code-generation units.
    pub codegen_units: u32,
    /// Whether debug symbols are stripped from the final binary.
    pub strip_debug: bool,
}

/// LTO mode for release builds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LtoMode {
    Off,
    Thin,
    Full,
}

// ---------------------------------------------------------------------------
// Compiler feedback value types (shared with compiler.rs)
// ---------------------------------------------------------------------------

/// Raw captured output from a `cargo` subprocess invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CargoOutput {
    /// Lines written to stdout (the JSON message stream).
    pub stdout_lines: Vec<String>,
    /// Lines written to stderr (human-readable progress / errors).
    pub stderr_lines: Vec<String>,
    /// The process exit code, if the process terminated normally.
    pub exit_code: Option<i32>,
}

/// High-level semantic classification of a compiler diagnostic.
///
/// Used to select targeted repair prompts, route retry policies, and enable
/// deterministic (non-LLM) repair rules for well-understood error classes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticFamily {
    /// Missing or invalid lifetime annotations.
    /// Triggered by: E0106, E0597, E0621–E0623, E0700, E0495.
    MissingLifetime,

    /// Trait bound not satisfied.
    /// Triggered by: E0277 (non-async), E0283, E0284, E0369, E0391.
    TraitBoundFailure,

    /// Value used after move or partial move.
    /// Triggered by: E0382, E0505–E0509.
    OwnershipMove,

    /// Conflicting or illegal borrows.
    /// Triggered by: E0499–E0504, E0515, E0521.
    BorrowConflict,

    /// Type mismatch or type inference failure.
    /// Triggered by: E0308, E0309, E0310, E0365, E0606–E0607.
    TypeMismatch,

    /// Missing, incorrect, or private import path.
    /// Triggered by: E0412, E0422, E0423, E0425, E0432, E0433, E0603.
    MissingImport,

    /// async/await misuse, Future not satisfied, or Send/Sync violation.
    /// Triggered by: E0277 (async context), E0728, E0752, E0753.
    AsyncMismatch,

    /// Macro expansion or invocation error.
    MacroError,

    /// Non-exhaustive pattern match.
    /// Triggered by: E0004, E0005.
    PatternExhaustiveness,

    /// Integer overflow or numeric coercion error.
    IntegerOverflow,

    /// Unused variable, unused import, or dead code (warnings only).
    UnusedCode,

    /// Internal compiler error (ICE).
    InternalCompilerError,

    /// Any diagnostic not matched by the above patterns.
    /// Contains the raw error code or `"unknown"`.
    Other(String),
}

impl DiagnosticFamily {
    /// A targeted repair instruction injected into the LLM fix prompt.
    pub fn repair_hint(&self) -> &'static str {
        match self {
            Self::MissingLifetime =>
                "Add explicit lifetime annotations. Prefer owned types (`String`, `Vec<T>`, `Box<T>`) \
                 over references to avoid lifetime complexity. Use `'_` for elided lifetimes where allowed.",
            Self::TraitBoundFailure =>
                "Add the missing trait bound to the generic parameter or `impl` block. \
                 Check if a `#[derive(Debug, Clone, PartialEq)]` or similar is missing.",
            Self::OwnershipMove =>
                "Clone the value before moving, or restructure to use references (`&T`). \
                 Consider `Arc<T>` for shared ownership across scopes.",
            Self::BorrowConflict =>
                "Restructure borrows so mutable and shared borrows do not overlap. \
                 Introduce a `{}` block to limit borrow scope, or use index-based access instead of references.",
            Self::TypeMismatch =>
                "Check expected vs found types. Add explicit type annotations or conversions: \
                 `.into()`, `.to_string()`, `as u64`, `From::from()`, `TryInto::try_into()?`.",
            Self::MissingImport =>
                "Add the missing `use` statement at the top of the file. \
                 Verify the correct module path (e.g. `use std::collections::HashMap;`).",
            Self::AsyncMismatch =>
                "Ensure all types used across `.await` points implement `Send + Sync`. \
                 Wrap in `Arc<Mutex<T>>` or `Arc<RwLock<T>>`. \
                 Do not hold non-Send types across await points.",
            Self::MacroError =>
                "Check macro invocation syntax. Ensure all arguments match the macro pattern. \
                 If using procedural macros, verify the derive/attribute is in scope.",
            Self::PatternExhaustiveness =>
                "Add the missing match arms. Use `_` as a catch-all for intentionally ignored variants. \
                 If adding a new enum variant, handle it in every match expression.",
            Self::IntegerOverflow =>
                "Use explicit casts (`as u64`, `u32::try_from(x)?`) or checked arithmetic \
                 (`.checked_add()`, `.saturating_mul()`). Avoid silent truncation.",
            Self::UnusedCode =>
                "Remove the unused item, prefix the binding with `_`, or add `#[allow(unused)]` / \
                 `#[allow(dead_code)]` if intentionally kept.",
            Self::InternalCompilerError =>
                "This is a rustc compiler bug. Simplify the code around the ICE. \
                 Try splitting the expression into smaller let-bindings.",
            Self::Other(_) =>
                "Read the compiler error message carefully and apply the suggested fix. \
                 Add any missing imports and ensure all types are correct.",
        }
    }

    /// Heuristic priority for retry scheduling (higher = easier/more useful to fix first).
    pub fn retry_priority(&self) -> u8 {
        match self {
            Self::MissingImport          => 10,
            Self::UnusedCode             => 9,
            Self::TypeMismatch           => 8,
            Self::IntegerOverflow        => 8,
            Self::PatternExhaustiveness  => 7,
            Self::TraitBoundFailure      => 6,
            Self::MacroError             => 5,
            Self::AsyncMismatch          => 5,
            Self::BorrowConflict         => 3,
            Self::OwnershipMove          => 3,
            Self::MissingLifetime        => 2,
            Self::InternalCompilerError  => 0,
            Self::Other(_)               => 4,
        }
    }

    /// Whether this family is considered automatically fixable without human review.
    pub fn is_auto_fixable(&self) -> bool {
        matches!(
            self,
            Self::MissingImport
                | Self::UnusedCode
                | Self::TypeMismatch
                | Self::PatternExhaustiveness
                | Self::IntegerOverflow
        )
    }
}

/// A single structured compiler diagnostic parsed from `cargo check
/// --message-format=json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompilerDiagnostic {
    pub level: DiagnosticLevel,
    pub message: String,
    pub code: Option<String>,
    pub spans: Vec<DiagnosticSpan>,
    /// The rendered (human-readable) form of the diagnostic, if present.
    pub rendered: Option<String>,
}

impl CompilerDiagnostic {
    /// Classify this diagnostic into a [`DiagnosticFamily`].
    ///
    /// Classification is pure (no I/O) and uses a two-pass strategy:
    /// 1. Exact match on the rustc error code (most precise).
    /// 2. Heuristic pattern matching on the message text (fallback).
    pub fn family(&self) -> DiagnosticFamily {
        crate::compiler::classify_diagnostic(self)
    }
}


/// Severity level of a compiler diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticLevel {
    Help,
    Note,
    Warning,
    Error,
    Ice,
}

/// A source span referenced by a compiler diagnostic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticSpan {
    pub file_name: String,
    pub line_start: u32,
    pub line_end: u32,
    pub column_start: u32,
    pub column_end: u32,
    pub is_primary: bool,
    pub label: Option<String>,
}

// ---------------------------------------------------------------------------
// State machine enum
// ---------------------------------------------------------------------------

/// The closed set of states the Rustyfi orchestrator can occupy.
///
/// Each variant holds exactly the context required for that phase; no more,
/// no less.  Structural pattern-matching on this enum is the only way to
/// read or mutate state context, which means the type system statically
/// prevents accessing e.g. `TranslatingContext` while in `Idle`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RustyfiState {
    /// No active translation job.
    Idle,

    /// The incoming [`ContextManifest`] is being validated and ingested.
    ///
    /// [`ContextManifest`]: crate::context::ContextManifest
    Parsing(ParsingContext),

    /// The Cargo workspace skeleton is being generated.
    Scaffolding(ScaffoldingContext),

    /// Source files are being translated chunk-by-chunk.
    Translating(TranslatingContext),

    /// The generated Rust code is being verified with `cargo check`.
    Verifying(VerifyingContext),

    /// The verified code is being compiled in release mode and optimized.
    Optimizing(OptimizingContext),

    /// Translation, verification, and optimization completed successfully.
    Completed(CompletedContext),

    /// An unrecoverable (or ceiling-exceeded) failure occurred.
    Failed(FailedContext),
}
