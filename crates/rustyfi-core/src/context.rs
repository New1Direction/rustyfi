use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Language identification
// ---------------------------------------------------------------------------

/// Identifies the source language of a translation target.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceLanguage {
    Python,
    TypeScript,
    JavaScript,
    Go,
    Cpp,
    C,
    Java,
    CSharp,
    Ruby,
    /// An explicitly named language not in the closed set above.
    Other(String),
}

// ---------------------------------------------------------------------------
// Source targets
// ---------------------------------------------------------------------------

/// A single file or module that has been selected for translation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceTarget {
    /// Absolute path to the source file on the worker's filesystem.
    pub path: PathBuf,
    /// Language detected for this specific file (may differ from the
    /// workspace-level language in polyglot codebases).
    pub language: SourceLanguage,
    /// Size in bytes, captured at analysis time.
    pub size_bytes: u64,
    /// SHA-256 digest of the file contents at analysis time (hex-encoded).
    pub content_hash: String,
    /// Whether this file was identified as containing a program entry point.
    pub is_entrypoint: bool,
}

// ---------------------------------------------------------------------------
// Dependency graph
// ---------------------------------------------------------------------------

/// A directed edge in the inter-module dependency graph.
///
/// `from` imports/requires `to`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyEdge {
    pub from: PathBuf,
    pub to: PathBuf,
    /// The raw import symbol as it appears in the source (e.g. `"os"`,
    /// `"../utils"`).
    pub import_symbol: String,
    /// Whether `to` resolves to a path within the workspace (`true`) or an
    /// external package registry (`false`).
    pub is_internal: bool,
}

// ---------------------------------------------------------------------------
// Filesystem and I/O boundaries
// ---------------------------------------------------------------------------

/// A filesystem path that is accessed by the source codebase but lies outside
/// the primary workspace root (e.g. `/etc/config`, `/var/data`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilesystemBoundary {
    pub path: PathBuf,
    pub access_mode: AccessMode,
    /// Human-readable hint about why this boundary exists.
    pub description: String,
}

/// The direction of a filesystem or I/O access.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessMode {
    ReadOnly,
    WriteOnly,
    ReadWrite,
}

/// An external I/O boundary detected in the source (network sockets,
/// subprocesses, inter-process communication, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalIoBoundary {
    pub kind: IoBoundaryKind,
    /// A best-effort human-readable description of the endpoint or resource.
    pub description: String,
    /// Source files where this boundary was detected.
    pub detected_in: Vec<PathBuf>,
}

/// The class of external I/O detected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IoBoundaryKind {
    TcpSocket,
    UdpSocket,
    UnixSocket,
    HttpClient,
    Subprocess,
    SharedMemory,
    Ipc,
    Other(String),
}

// ---------------------------------------------------------------------------
// Parser warnings
// ---------------------------------------------------------------------------

/// A non-fatal warning emitted by the language analysis worker during parsing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParserWarning {
    /// The source file in which the warning was generated.
    pub file: PathBuf,
    /// 1-based line number, if applicable.
    pub line: Option<u32>,
    pub message: String,
    pub severity: WarningSeverity,
}

/// Severity classification for parser warnings.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WarningSeverity {
    /// Informational only; will not affect translation quality.
    Info,
    /// May produce suboptimal Rust output but is not blocking.
    Low,
    /// Likely to degrade translation quality; human review recommended.
    Medium,
    /// Translation may fail or produce incorrect output.
    High,
}

// ---------------------------------------------------------------------------
// Language-level metadata
// ---------------------------------------------------------------------------

/// Workspace-level metadata produced by the language analysis worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LanguageMetadata {
    /// Primary language of the workspace.
    pub primary_language: SourceLanguage,
    /// Runtime version string as reported by the source toolchain
    /// (e.g. `"3.11.4"`, `"18.17.0"`).
    pub runtime_version: Option<String>,
    /// Name and version of the primary package manager
    /// (e.g. `"pip 23.1"`, `"npm 9.6.7"`).
    pub package_manager: Option<String>,
    /// Whether the source codebase uses dynamic typing exclusively (`true`)
    /// or has partial/full static typing (`false`).
    pub is_dynamically_typed: bool,
    /// Additional key/value pairs specific to the source language.
    #[serde(default)]
    pub extra: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// Context manifest (ingestion contract)
// ---------------------------------------------------------------------------

/// The deterministic ingestion contract between external language-analysis
/// workers and the Rust orchestration runtime.
///
/// Workers serialize this struct and hand it off to the orchestrator.  The
/// orchestrator deserializes it, validates it, and uses it to seed the
/// [`Parsing`] state context before driving further transitions.
///
/// [`Parsing`]: crate::state::ParsingContext
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextManifest {
    /// A globally unique identifier for this analysis run (UUID v4 or similar).
    pub run_id: String,

    /// Absolute path to the workspace root on the analysis worker's
    /// filesystem.
    pub workspace_root: PathBuf,

    /// All files selected for translation, in dependency-resolved order where
    /// possible.
    pub source_targets: Vec<SourceTarget>,

    /// Import/require statements detected across the entire workspace, formed
    /// into a directed edge list.
    pub dependency_edges: Vec<DependencyEdge>,

    /// External package names (from a registry such as PyPI, npm, crates.io)
    /// that the workspace depends on.
    pub external_packages: Vec<String>,

    /// Filesystem accesses that cross the workspace boundary.
    pub filesystem_boundaries: Vec<FilesystemBoundary>,

    /// Detected external I/O boundaries (sockets, subprocesses, etc.).
    pub external_io_boundaries: Vec<ExternalIoBoundary>,

    /// Files or symbols that the worker identified as program entry points.
    pub inferred_entrypoints: Vec<PathBuf>,

    /// Non-fatal warnings emitted during parsing.
    pub parser_warnings: Vec<ParserWarning>,

    /// Workspace-level language metadata.
    pub language_metadata: LanguageMetadata,

    /// ISO 8601 timestamp at which this manifest was produced.
    pub produced_at: String,
}
