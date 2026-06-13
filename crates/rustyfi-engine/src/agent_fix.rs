//! Doctor session core: a guarded, budget-capped tool-execution loop over a
//! workspace. This is the model-free layer — the LLM transport lives in Task 2.
//!
//! The session exposes a small set of tools that a driver can call one at a
//! time.  Every call is checked against confinement rules (no path escapes,
//! payload caps) and the per-session budget (max calls, max wall-clock seconds).
//! When the budget is exhausted the session returns a terminal `ToolOutcome`
//! regardless of which tool was requested.

use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::fix_context::ItemIndex;
use crate::llm::AssistantTurn;
use crate::EngineError;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// How many tool calls and wall-clock seconds the session may consume.
#[derive(Debug, Clone)]
pub struct DoctorBudget {
    /// The session allows calls 1..=max_tool_calls; call max_tool_calls+1
    /// returns a terminal "budget exhausted" outcome.
    pub max_tool_calls: usize,
    pub max_wall_secs: u64,
}

impl Default for DoctorBudget {
    fn default() -> Self {
        Self {
            max_tool_calls: 40,
            max_wall_secs: 1200,
        }
    }
}

/// Every tool a driver may invoke.
#[derive(Debug, Clone)]
pub enum ToolCall {
    /// List `.rs` files under `<ws>/src` plus `Cargo.toml` and `NEXT_STEPS.md`
    /// (if present).
    ListFiles,
    /// Read one file confined to the workspace.  Payload capped at 24 000 bytes.
    ReadFile { path: String },
    /// Search the item index for definitions / impls mentioning `symbol`.
    Search { symbol: String },
    /// Run `cargo check` and return the first 8 000 bytes of error diagnostics.
    CargoCheck,
    /// Build + run the target against the behavioral corpus; report per-case diffs.
    RunBehaviorChecks,
    /// Return a cached `rustc --explain` excerpt for an error code.
    Explain { code: String },
    /// Write a file confined to `<ws>/src`; rebuilds the item index afterwards.
    WriteFile { path: String, content: String },
    /// Signal end of session with a human-readable summary.
    Done { summary: String },
}

/// Result of executing one tool call.
#[derive(Debug, Clone)]
pub struct ToolOutcome {
    /// Human-readable payload to feed back to the driver / model.
    pub payload: String,
    /// `true` when the session should stop (Done, budget exhausted, …).
    pub is_terminal: bool,
}

// ---------------------------------------------------------------------------
// DoctorSession
// ---------------------------------------------------------------------------

/// An active doctor session over a workspace.
pub struct DoctorSession {
    workspace: PathBuf,
    budget: DoctorBudget,
    calls_used: usize,
    started: Instant,
    item_index: ItemIndex,
    /// Optional behavioral corpus + scratch dir; enables the RunBehaviorChecks tool.
    behavior: Option<(crate::behavior::BehaviorSpec, std::path::PathBuf)>,
    /// Error count from the most recent CargoCheck call (–1 = never run).
    last_error_count: i64,
}

impl DoctorSession {
    /// Create a new session.  Builds the initial item index immediately.
    pub fn new(workspace: &Path, budget: DoctorBudget) -> Self {
        let item_index = ItemIndex::build(workspace);
        DoctorSession {
            workspace: workspace.to_path_buf(),
            budget,
            calls_used: 0,
            started: Instant::now(),
            item_index,
            behavior: None,
            last_error_count: -1,
        }
    }

    /// Count an invalid tool invocation toward the session budget.
    ///
    /// Invalid tool invocations (unknown tool name or missing/bad arguments) consume
    /// budget too — a misbehaving model must not loop for free. Call this method
    /// in error paths before continuing the loop.
    pub(crate) fn count_invalid_call(&mut self) {
        self.calls_used += 1;
    }

    /// Attach a behavioral corpus so the session can run `RunBehaviorChecks`.
    pub fn with_behavior(
        mut self,
        spec: crate::behavior::BehaviorSpec,
        work: std::path::PathBuf,
    ) -> Self {
        self.behavior = Some((spec, work));
        self
    }

    /// True when a behavioral corpus is attached.
    pub fn has_behavior(&self) -> bool {
        self.behavior.is_some()
    }

    /// Execute one tool call.  Always increments `calls_used` first; returns a
    /// terminal outcome if the budget is already exhausted before dispatch.
    pub fn execute(&mut self, call: ToolCall) -> ToolOutcome {
        // Increment unconditionally — every call costs one unit of budget.
        self.calls_used += 1;

        // Check budget AFTER incrementing (the call that tips us over is still
        // a terminal outcome).
        if self.budget_exhausted() {
            return ToolOutcome {
                payload: "budget exhausted".to_string(),
                is_terminal: true,
            };
        }

        match call {
            ToolCall::Done { summary } => ToolOutcome {
                payload: summary,
                is_terminal: true,
            },
            ToolCall::ListFiles => self.list_files(),
            ToolCall::ReadFile { path } => self.read_file(&path),
            ToolCall::Search { symbol } => self.search(&symbol),
            ToolCall::CargoCheck => self.cargo_check(),
            ToolCall::RunBehaviorChecks => self.run_behavior_checks(),
            ToolCall::Explain { code } => self.explain(&code),
            ToolCall::WriteFile { path, content } => self.write_file(&path, &content),
        }
    }

    /// Returns `true` when either cap (calls or wall clock) is exceeded.
    pub fn budget_exhausted(&self) -> bool {
        self.calls_used > self.budget.max_tool_calls
            || self.started.elapsed().as_secs() > self.budget.max_wall_secs
    }

    /// How many calls have been made (including the current one if inside
    /// `execute`).
    pub fn calls_used(&self) -> usize {
        self.calls_used
    }

    /// Error count from the most recent `CargoCheck` call, or `None` if no
    /// check has been run yet.
    pub fn last_error_count(&self) -> Option<usize> {
        if self.last_error_count < 0 {
            None
        } else {
            Some(self.last_error_count as usize)
        }
    }

    // -----------------------------------------------------------------------
    // Tool implementations
    // -----------------------------------------------------------------------

    fn list_files(&self) -> ToolOutcome {
        let src_root = self.workspace.join("src");
        let mut paths: Vec<String> = Vec::new();

        if src_root.is_dir() {
            let mut rs_files: Vec<String> = walkdir::WalkDir::new(&src_root)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.path().extension().and_then(|s| s.to_str()) == Some("rs")
                        && e.file_type().is_file()
                })
                .filter_map(|e| {
                    e.path()
                        .strip_prefix(&self.workspace)
                        .ok()
                        .map(|p| p.to_string_lossy().replace('\\', "/"))
                })
                .collect();
            rs_files.sort();
            paths.extend(rs_files);
        }

        // Always include Cargo.toml if present.
        if self.workspace.join("Cargo.toml").exists() {
            paths.push("Cargo.toml".to_string());
        }

        // Include NEXT_STEPS.md only if present.
        if self.workspace.join("NEXT_STEPS.md").exists() {
            paths.push("NEXT_STEPS.md".to_string());
        }

        ToolOutcome {
            payload: paths.join("\n"),
            is_terminal: false,
        }
    }

    fn read_file(&self, rel_path: &str) -> ToolOutcome {
        match self.confined_read(rel_path) {
            Ok(content) => ToolOutcome {
                payload: tail_truncate(content, READ_CAP),
                is_terminal: false,
            },
            Err(e) => ToolOutcome {
                payload: format!("error: {e}"),
                is_terminal: false,
            },
        }
    }

    fn search(&self, symbol: &str) -> ToolOutcome {
        let mut blocks: Vec<String> = Vec::new();

        // Definitions.
        if let Some(defs) = self.item_index.items.get(symbol) {
            for def in defs {
                blocks.push(format!(
                    "// definition of {symbol} (from {})\n{}",
                    def.rel_path, def.source_text
                ));
            }
        }

        // Impls mentioning the symbol (by trait name or self type).
        for imp in &self.item_index.impls {
            let matches = imp
                .trait_name
                .as_deref()
                .map(|t| t == symbol)
                .unwrap_or(false)
                || imp.type_name == symbol;
            if matches {
                blocks.push(format!(
                    "// impl involving {symbol} (from {})\n{}",
                    imp.rel_path, imp.source_text
                ));
            }
        }

        let payload = if blocks.is_empty() {
            format!("no matches for '{symbol}'")
        } else {
            blocks.join("\n\n")
        };

        ToolOutcome {
            payload,
            is_terminal: false,
        }
    }

    fn cargo_check(&mut self) -> ToolOutcome {
        use rustyfi_core::compiler::{parse_cargo_diagnostics, run_cargo_check};

        let output = match run_cargo_check(&self.workspace) {
            Ok(o) => o,
            Err(e) => {
                return ToolOutcome {
                    payload: format!("cargo check failed to run: {e}"),
                    is_terminal: false,
                };
            }
        };

        // Count error-level diagnostics.
        let diags = parse_cargo_diagnostics(&output).unwrap_or_default();
        // `== Error` here (the count the model sees) vs the pipeline's `>= Error`
        // keep/revert criterion: ICEs are excluded from the model-facing count on
        // purpose — the model can't fix a compiler crash, but the pipeline still
        // counts it when judging whether to keep the doctor's changes.
        let error_count = diags
            .iter()
            .filter(|d| d.level == rustyfi_core::state::DiagnosticLevel::Error)
            .count();
        self.last_error_count = error_count as i64;

        // Collect rendered strings from error-level diagnostics, capped.
        let mut payload = String::new();
        for diag in &diags {
            if diag.level != rustyfi_core::state::DiagnosticLevel::Error {
                continue;
            }
            let text = diag.rendered.as_deref().unwrap_or(diag.message.as_str());
            if payload.len() + text.len() + 1 > CARGO_CHECK_CAP {
                break;
            }
            if !payload.is_empty() {
                payload.push('\n');
            }
            payload.push_str(text);
        }

        payload.push_str(&format!("\nerror count: {error_count}"));

        ToolOutcome {
            payload,
            is_terminal: false,
        }
    }

    /// Build + run the target against the corpus and render per-case diffs.
    fn run_behavior_checks(&mut self) -> ToolOutcome {
        let Some((spec, work)) = &self.behavior else {
            return ToolOutcome {
                payload: "no behavioral corpus loaded for this session".to_string(),
                is_terminal: false,
            };
        };
        match crate::behavior::verify(spec, &self.workspace, work) {
            Ok(report) => {
                let mut p = format!(
                    "behavior: {}/{} cases matched ({} quarantined)\n",
                    report.matched, report.total, report.quarantined
                );
                for c in report.cases.iter().filter(|c| !c.matched) {
                    p.push_str(&format!("MISMATCH {}:\n", c.name));
                    for d in &c.diffs {
                        p.push_str(&format!("  {d}\n"));
                    }
                }
                ToolOutcome {
                    payload: tail_truncate(p, 8_000),
                    is_terminal: false,
                }
            }
            Err(e) => ToolOutcome {
                payload: format!("behavior check failed to run: {e}"),
                is_terminal: false,
            },
        }
    }

    fn explain(&self, code: &str) -> ToolOutcome {
        let payload = crate::fix_context::explain_excerpt(code)
            .unwrap_or_else(|| "no explanation available".to_string());
        ToolOutcome {
            payload,
            is_terminal: false,
        }
    }

    fn write_file(&mut self, rel_path: &str, content: &str) -> ToolOutcome {
        match self.confined_write(rel_path, content) {
            Ok(bytes) => {
                // Rebuild the item index after every successful write.
                self.item_index = ItemIndex::build(&self.workspace);
                ToolOutcome {
                    payload: format!("wrote {rel_path} ({bytes} bytes)"),
                    is_terminal: false,
                }
            }
            Err(e) => ToolOutcome {
                payload: format!("error: {e}"),
                is_terminal: false,
            },
        }
    }

    // -----------------------------------------------------------------------
    // Confinement helpers
    // -----------------------------------------------------------------------

    /// Check that `rel_path` is a safe relative path and resolve it to an
    /// absolute path that lies under the workspace.  Returns the resolved path
    /// on success or an error message on failure.
    fn resolve_confined(&self, rel_path: &str) -> Result<PathBuf, String> {
        // Reject absolute paths.
        let p = Path::new(rel_path);
        if p.is_absolute() {
            return Err(format!("absolute paths are not allowed: {rel_path}"));
        }

        // Reject obvious traversal components.
        for component in p.components() {
            use std::path::Component;
            if component == Component::ParentDir {
                return Err(format!("path traversal is not allowed: {rel_path}"));
            }
        }

        let full = self.workspace.join(rel_path);

        // Canonicalize the workspace root.
        let canon_ws = match self.workspace.canonicalize() {
            Ok(c) => c,
            Err(e) => return Err(format!("cannot canonicalize workspace: {e}")),
        };

        // Canonicalize the parent directory of the target (the file itself may
        // not exist yet for writes).
        let parent = full.parent().unwrap_or(&full);
        let canon_parent = match parent.canonicalize() {
            Ok(c) => c,
            // Parent doesn't exist yet — fall back to a textual prefix check.
            // This is weaker than canonicalization (symlinks are not resolved)
            // but is layered with the `..`-component and absolute-path
            // rejections above, which together prevent the common traversal
            // vectors.  We still verify that the resolved path starts with the
            // workspace root so that any future change to the join/relative
            // logic cannot silently produce an out-of-workspace path.
            Err(_) => {
                if !full.starts_with(&self.workspace) {
                    return Err(format!(
                        "cannot verify path is within workspace: {rel_path}"
                    ));
                }
                return Ok(full);
            }
        };

        // Verify the parent actually lives inside the workspace.
        if !canon_parent.starts_with(&canon_ws) {
            return Err(format!("path escapes the workspace: {rel_path}"));
        }

        Ok(full)
    }

    /// Read a file that must be one of: `src/**`, `Cargo.toml`, `NEXT_STEPS.md`.
    fn confined_read(&self, rel_path: &str) -> Result<String, String> {
        let full = self.resolve_confined(rel_path)?;

        // Must be one of the allowed locations.
        let norm = rel_path.replace('\\', "/");
        let allowed = norm.starts_with("src/") || norm == "Cargo.toml" || norm == "NEXT_STEPS.md";
        if !allowed {
            return Err(format!(
                "read not allowed outside src/, Cargo.toml, NEXT_STEPS.md: {rel_path}"
            ));
        }

        std::fs::read_to_string(&full).map_err(|e| format!("cannot read {rel_path}: {e}"))
    }

    /// Write a file that must be under `src/` only.
    fn confined_write(&self, rel_path: &str, content: &str) -> Result<usize, String> {
        let full = self.resolve_confined(rel_path)?;

        let norm = rel_path.replace('\\', "/");
        if !norm.starts_with("src/") {
            return Err(format!("writes are only allowed under src/: {rel_path}"));
        }

        // Create parent directories if needed.
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create directories for {rel_path}: {e}"))?;
        }

        let bytes = content.len();
        std::fs::write(&full, content).map_err(|e| format!("cannot write {rel_path}: {e}"))?;
        Ok(bytes)
    }
}

// ---------------------------------------------------------------------------
// Module-level public helpers
// ---------------------------------------------------------------------------

/// Payload cap for `ReadFile` (bytes).
const READ_CAP: usize = 24_000;

/// Payload cap for `CargoCheck` (bytes, before appending error count line).
const CARGO_CHECK_CAP: usize = 8_000;

/// Truncate `s` to at most `cap` bytes, appending `"\n…[truncated]"` when
/// truncation occurs.  Snaps to a valid UTF-8 boundary.
fn tail_truncate(s: String, cap: usize) -> String {
    if s.len() <= cap {
        return s;
    }
    // Find the largest valid char boundary ≤ cap.
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…[truncated]", &s[..end])
}

// ---------------------------------------------------------------------------
// 2.2  JSON-action fallback parser
// ---------------------------------------------------------------------------

/// Parse a model reply that encodes its action as a JSON object.
///
/// Accepts replies with surrounding prose (the first balanced `{…}` that is
/// valid JSON and has a `"tool"` field is used).  Also accepts ` ```json ` or
/// bare ` ``` ` fences around the object.
///
/// Tool name → ToolCall mapping:
///
/// | `"tool"` value  | `ToolCall` variant                                |
/// |-----------------|---------------------------------------------------|
/// | `"list_files"`  | `ListFiles`                                       |
/// | `"read_file"`   | `ReadFile { path }`                               |
/// | `"search"`      | `Search { symbol }`                               |
/// | `"cargo_check"` | `CargoCheck`                                      |
/// | `"explain"`     | `Explain { code }`                                |
/// | `"write_file"`  | `WriteFile { path, content }`                     |
/// | `"done"`        | `Done { summary }`                                |
pub fn parse_action_reply(reply: &str) -> Result<ToolCall, String> {
    // 1. Try to extract a fenced JSON block first (```json … ``` or ``` … ```).
    let candidate = extract_json_fence(reply).unwrap_or_else(|| reply.to_string());

    // 2. Find the first balanced top-level `{…}` in the candidate text.
    let json_str = extract_first_object(&candidate)
        .ok_or_else(|| "no JSON object found in reply".to_string())?;

    let val: serde_json::Value =
        serde_json::from_str(&json_str).map_err(|e| format!("JSON parse error: {e}"))?;

    let tool = val["tool"]
        .as_str()
        .ok_or_else(|| "missing \"tool\" field".to_string())?;

    let args = &val["args"];

    match tool {
        "list_files" => Ok(ToolCall::ListFiles),
        "cargo_check" => Ok(ToolCall::CargoCheck),
        "run_behavior_checks" => Ok(ToolCall::RunBehaviorChecks),
        "read_file" => {
            let path = args["path"]
                .as_str()
                .ok_or_else(|| "read_file requires args.path".to_string())?
                .to_string();
            Ok(ToolCall::ReadFile { path })
        }
        "search" => {
            let symbol = args["symbol"]
                .as_str()
                .ok_or_else(|| "search requires args.symbol".to_string())?
                .to_string();
            Ok(ToolCall::Search { symbol })
        }
        "explain" => {
            let code = args["code"]
                .as_str()
                .ok_or_else(|| "explain requires args.code".to_string())?
                .to_string();
            Ok(ToolCall::Explain { code })
        }
        "write_file" => {
            let path = args["path"]
                .as_str()
                .ok_or_else(|| "write_file requires args.path".to_string())?
                .to_string();
            let content = args["content"]
                .as_str()
                .ok_or_else(|| "write_file requires args.content".to_string())?
                .to_string();
            Ok(ToolCall::WriteFile { path, content })
        }
        "done" => {
            let summary = args["summary"]
                .as_str()
                .ok_or_else(|| "done requires args.summary".to_string())?
                .to_string();
            Ok(ToolCall::Done { summary })
        }
        other => Err(format!("unknown tool: \"{other}\"")),
    }
}

/// Extract the body of a fenced block (` ```json … ``` ` or ` ``` … ``` `).
/// Returns `None` if the reply has no fence.
fn extract_json_fence(text: &str) -> Option<String> {
    let open_json = text.find("```json");
    let open_bare = text.find("```");
    let start_fence = match (open_json, open_bare) {
        (Some(a), _) => a,
        (None, Some(b)) => b,
        (None, None) => return None,
    };
    let after_fence = &text[start_fence..];
    let body_start = after_fence.find('\n').map(|i| i + 1)?;
    let body = &after_fence[body_start..];
    // Take everything up to the closing fence.
    let end = body.find("```").unwrap_or(body.len());
    Some(body[..end].to_string())
}

/// Find the first balanced top-level `{…}` in `text`.
/// Returns the matched substring (including braces) or `None`.
fn extract_first_object(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut start: Option<usize> = None;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape_next = false;
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];

        if escape_next {
            escape_next = false;
            i += 1;
            continue;
        }

        if in_string {
            match b {
                b'\\' => escape_next = true,
                b'"' => in_string = false,
                _ => {}
            }
            i += 1;
            continue;
        }

        match b {
            b'"' => in_string = true,
            b'{' => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    if let Some(s) = start {
                        return Some(text[s..=i].to_string());
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

// ---------------------------------------------------------------------------
// 2.3  Doctor report, transport trait, driver
// ---------------------------------------------------------------------------

/// Summary of a completed doctor session.
#[derive(Debug, Clone)]
pub struct DoctorReport {
    /// Number of cargo errors at the start of the session.
    pub start_errors: usize,
    /// Number of cargo errors at the end of the session (final check after the
    /// loop terminates, run outside the budget).
    pub end_errors: usize,
    /// Total number of tool calls counted against the budget.
    pub tool_calls_used: usize,
    /// Elapsed wall-clock seconds for the session.
    pub wall_secs: u64,
    /// Human-readable summary string (from the model's Done call, or a
    /// budget-exhaustion notice).
    pub summary: String,
}

/// The LLM transport abstraction.  Separates the network layer from the driver
/// so that tests can inject a scripted sequence of turns without hitting the
/// network.
///
/// **Contract**: `turn()` MUST leave the assistant's own message appended to
/// `conversation` before returning.  This invariant is what allows the driver
/// to extract `tool_call_id` values from the last message and build proper
/// `{role:"tool"}` result messages.  The driver never pushes assistant messages
/// itself — that responsibility belongs entirely to the transport.
///
/// For `LlmTransport` the raw API message is verbatim-appended so that
/// `tool_call_id` values round-trip correctly.  For `ScriptedTransport` a
/// synthetic assistant message is synthesised from the scripted turn.
pub trait DoctorTransport {
    fn turn(
        &mut self,
        conversation: &mut Vec<serde_json::Value>,
        tools: &serde_json::Value,
    ) -> Result<AssistantTurn, EngineError>;
}

/// Live transport backed by an `LlmClient`.
pub struct LlmTransport<'a>(pub &'a crate::llm::LlmClient);

impl<'a> DoctorTransport for LlmTransport<'a> {
    fn turn(
        &mut self,
        conversation: &mut Vec<serde_json::Value>,
        tools: &serde_json::Value,
    ) -> Result<AssistantTurn, EngineError> {
        // System prompt is empty here — the driver seeds the conversation with
        // the system message content via SYSTEM_DOCTOR inside the user message;
        // the actual system role is passed to complete_with_tools.
        let (turn, raw_msg) = self
            .0
            .complete_with_tools(SYSTEM_DOCTOR, conversation, tools)?;
        conversation.push(raw_msg);
        Ok(turn)
    }
}

/// OpenAI tool schema for the doctor tools.
///
/// Pass `include_behavior = true` to append the `run_behavior_checks` tool
/// when a behavioral corpus is attached to the session.
pub fn tools_schema(include_behavior: bool) -> serde_json::Value {
    let mut tools = serde_json::json!([
        {
            "type": "function",
            "function": {
                "name": "list_files",
                "description": "List .rs files under src/ plus Cargo.toml and NEXT_STEPS.md (if present).",
                "parameters": { "type": "object", "properties": {}, "required": [] }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read one file from the workspace (src/, Cargo.toml, NEXT_STEPS.md). Payload capped at 24 000 bytes.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Workspace-relative path, e.g. src/lib.rs" }
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "search",
                "description": "Search the item index for definitions/impls mentioning a symbol.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "symbol": { "type": "string", "description": "Symbol name to search for." }
                    },
                    "required": ["symbol"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "cargo_check",
                "description": "Run cargo check and return compiler errors (capped at 8 000 bytes) plus error count.",
                "parameters": { "type": "object", "properties": {}, "required": [] }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "explain",
                "description": "Return a rustc --explain excerpt for a given error code.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "code": { "type": "string", "description": "Rust error code, e.g. E0308." }
                    },
                    "required": ["code"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Write (create or overwrite) a file under src/. Rebuilds the item index afterwards.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Workspace-relative path, must start with src/." },
                        "content": { "type": "string", "description": "Complete new file content." }
                    },
                    "required": ["path", "content"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "done",
                "description": "Signal end of session. Call when the crate is clean or when you are stuck and cannot make further progress.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "summary": { "type": "string", "description": "Human-readable summary of what was done." }
                    },
                    "required": ["summary"]
                }
            }
        }
    ]);
    if include_behavior {
        if let Some(arr) = tools.as_array_mut() {
            arr.push(serde_json::json!({
                "type": "function",
                "function": {
                    "name": "run_behavior_checks",
                    "description": "Build and run the translated crate against the behavioral corpus; returns per-case stdout/stderr/exit diffs vs the original. Use after cargo check is clean to find and fix behavioral divergences.",
                    "parameters": { "type": "object", "properties": {}, "required": [] }
                }
            }));
        }
    }
    tools
}

/// System prompt for the doctor session.
pub const SYSTEM_DOCTOR: &str = concat!(
    "You are a senior Rust engineer fixing a crate that was machine-translated from another language. ",
    "`cargo check` is the ground truth for correctness. Your job is to reduce the error count to zero.\n\n",
    "Rules:\n",
    "1. Fix root causes — do not delete functionality or silence errors with casts / #[allow].\n",
    "2. Prefer minimal, surgical edits; change only what is broken.\n",
    "3. After every WriteFile call, run CargoCheck to confirm the error count improved.\n",
    "4. Call Done when the crate is clean OR when you are stuck and cannot make further progress.\n",
    "5. Never skip a CargoCheck after a write — confirm improvement before moving on.\n",
    "6. If native tool calling is unavailable, reply with exactly one JSON object: ",
    r#"{"tool": "...", "args": {...}} "#,
    "— valid tool names: list_files, read_file, search, cargo_check, run_behavior_checks, explain, write_file, done.\n",
    "7. After cargo check is clean, if behavioral mismatches remain, call run_behavior_checks to see which cases diverge from the original, ",
    "fix the root cause (output formatting, stream routing, off-by-one, logic), and call run_behavior_checks again to confirm. ",
    "NEVER edit behavior.yaml or change expected values — the original's behavior is ground truth.",
);

/// Convert a `ToolCall` name (as returned by the model's tool_calls field) and
/// its arguments JSON string into a `ToolCall` variant.
///
/// Used by the driver to dispatch native tool-call responses.  Returns
/// `Err(message)` on unknown tool name or missing required argument.
fn tool_call_from_native(name: &str, arguments_json: &str) -> Result<ToolCall, String> {
    let args: serde_json::Value = serde_json::from_str(arguments_json)
        .unwrap_or(serde_json::Value::Object(Default::default()));

    match name {
        "list_files" => Ok(ToolCall::ListFiles),
        "cargo_check" => Ok(ToolCall::CargoCheck),
        "run_behavior_checks" => Ok(ToolCall::RunBehaviorChecks),
        "read_file" => {
            let path = args["path"]
                .as_str()
                .ok_or("read_file requires args.path")?
                .to_string();
            Ok(ToolCall::ReadFile { path })
        }
        "search" => {
            let symbol = args["symbol"]
                .as_str()
                .ok_or("search requires args.symbol")?
                .to_string();
            Ok(ToolCall::Search { symbol })
        }
        "explain" => {
            let code = args["code"]
                .as_str()
                .ok_or("explain requires args.code")?
                .to_string();
            Ok(ToolCall::Explain { code })
        }
        "write_file" => {
            let path = args["path"]
                .as_str()
                .ok_or("write_file requires args.path")?
                .to_string();
            let content = args["content"]
                .as_str()
                .ok_or("write_file requires args.content")?
                .to_string();
            Ok(ToolCall::WriteFile { path, content })
        }
        "done" => {
            let summary = args["summary"]
                .as_str()
                .ok_or("done requires args.summary")?
                .to_string();
            Ok(ToolCall::Done { summary })
        }
        other => Err(format!("unknown tool: \"{other}\"")),
    }
}

/// Run a doctor session over `ws` using the given transport.
///
/// `behavior` optionally attaches a behavioral corpus to the session.  When
/// present the `run_behavior_checks` tool is included in the schema and the
/// seed message is extended with an initial behavior diff.
///
/// `progress_cb` is called with a human-readable status line before each tool
/// execution.
///
/// The final `cargo check` that determines `end_errors` is run directly (not
/// through the session) so it does NOT count against the budget.
pub fn run_doctor(
    ws: &Path,
    transport: &mut dyn DoctorTransport,
    budget: DoctorBudget,
    behavior: Option<(crate::behavior::BehaviorSpec, std::path::PathBuf)>,
    progress_cb: &mut dyn FnMut(String),
) -> DoctorReport {
    use rustyfi_core::compiler::{parse_cargo_diagnostics, run_cargo_check};

    let started = Instant::now();
    let tools = tools_schema(behavior.is_some());

    // Build the initial session (manages tool execution + budget tracking).
    let mut session = DoctorSession::new(ws, budget);
    if let Some((spec, work)) = behavior {
        session = session.with_behavior(spec, work);
    }

    // --- Seed the conversation with the initial cargo check output. ----------
    // Run the first CargoCheck through the session (counts as tool call 1).
    progress_cb("doctor: seeding — running initial cargo check".to_string());
    let seed_outcome = session.execute(ToolCall::CargoCheck);
    let start_errors = session.last_error_count().unwrap_or(0);

    // Also include NEXT_STEPS.md if present.
    let next_steps_content = {
        let ns_path = ws.join("NEXT_STEPS.md");
        if ns_path.exists() {
            std::fs::read_to_string(&ns_path)
                .map(|s| format!("\n\n--- NEXT_STEPS.md ---\n{s}"))
                .unwrap_or_default()
        } else {
            String::new()
        }
    };

    let seed_user_msg = format!(
        "Current cargo check output:\n```\n{}\n```{}",
        seed_outcome.payload, next_steps_content
    );

    // When a behavioral corpus is attached, run an initial behavior check and
    // append the results to the seed message so the model starts with full
    // context (both compile errors and behavioral divergences).
    let seed_user_msg = if session.has_behavior() {
        let bc = session.execute(ToolCall::RunBehaviorChecks);
        format!(
            "{seed_user_msg}\n\nBehavioral check (the original is ground truth):\n```\n{}\n```",
            bc.payload
        )
    } else {
        seed_user_msg
    };

    let mut conversation: Vec<serde_json::Value> = vec![serde_json::json!({
        "role": "user",
        "content": seed_user_msg
    })];

    let mut summary = "budget exhausted".to_string();

    // --- Main ReAct loop. ---------------------------------------------------
    loop {
        if session.budget_exhausted() {
            break;
        }

        let turn_result = transport.turn(&mut conversation, &tools);
        let turn = match turn_result {
            Ok(t) => t,
            Err(e) => {
                // Transport error — stop the session.
                summary = format!("transport error: {e}");
                break;
            }
        };

        match turn {
            AssistantTurn::ToolInvocation {
                name,
                arguments_json,
            } => {
                // Parse the tool call from the native format.
                let tool_call = match tool_call_from_native(&name, &arguments_json) {
                    Ok(tc) => tc,
                    Err(e) => {
                        // Feed the parse error back to the model as a tool result.
                        let error_content = format!("error: {e}");
                        progress_cb(format!("doctor: bad tool call ({e})"));
                        // Invalid tool invocations consume budget too.
                        session.count_invalid_call();
                        // We need to append a tool result even without a valid
                        // tool_call_id; use a plain user message as a fallback
                        // (documented simplification for the scripted test path).
                        conversation.push(serde_json::json!({
                            "role": "user",
                            "content": error_content
                        }));
                        continue;
                    }
                };

                // Extract the tool_call_id from the last assistant message so
                // we can send a proper {role:"tool"} result.
                let tool_call_id = conversation
                    .last()
                    .and_then(|m| m.get("tool_calls"))
                    .and_then(|tc| tc.get(0))
                    .and_then(|tc| tc.get("id"))
                    .and_then(|id| id.as_str())
                    .map(|s| s.to_string());

                progress_cb(format!("doctor: {name} …"));

                // Done is terminal — capture summary and break before executing.
                if let ToolCall::Done {
                    summary: ref done_summary,
                } = tool_call
                {
                    summary = done_summary.clone();
                    // Still execute so is_terminal is set, but we break regardless.
                    session.execute(tool_call);
                    break;
                }

                let outcome = session.execute(tool_call);

                // Append the tool result to the conversation.
                let result_msg = if let Some(id) = tool_call_id {
                    serde_json::json!({
                        "role": "tool",
                        "tool_call_id": id,
                        "content": outcome.payload
                    })
                } else {
                    // Scripted path fallback: no tool_call_id available.
                    serde_json::json!({
                        "role": "user",
                        "content": outcome.payload
                    })
                };

                conversation.push(result_msg);

                if outcome.is_terminal {
                    break;
                }
            }
            AssistantTurn::Text(text) => {
                // The transport has already appended the assistant message to
                // `conversation` (see DoctorTransport contract).  The driver
                // must NOT push a second assistant message here.

                // Attempt to parse a JSON-encoded action from the text before
                // falling back to a nudge.  Some models (or endpoints without
                // native tool-calling support) embed the action as JSON prose.
                //
                // Note: this path has no tool_call_id machinery — the tool
                // result is injected as a plain {role:"user"} message rather
                // than a {role:"tool"} message.  This is the documented
                // simplification for the JSON-fallback path.
                match parse_action_reply(&text) {
                    Ok(tool_call) => {
                        let name = match &tool_call {
                            ToolCall::ListFiles => "list_files",
                            ToolCall::ReadFile { .. } => "read_file",
                            ToolCall::Search { .. } => "search",
                            ToolCall::CargoCheck => "cargo_check",
                            ToolCall::RunBehaviorChecks => "run_behavior_checks",
                            ToolCall::Explain { .. } => "explain",
                            ToolCall::WriteFile { .. } => "write_file",
                            ToolCall::Done { .. } => "done",
                        };

                        // Done is terminal — capture summary and break.
                        if let ToolCall::Done {
                            summary: ref done_summary,
                        } = tool_call
                        {
                            summary = done_summary.clone();
                            progress_cb(format!("doctor: {name} … (json-fallback)"));
                            session.execute(tool_call);
                            break;
                        }

                        progress_cb(format!("doctor: {name} … (json-fallback)"));
                        let outcome = session.execute(tool_call);

                        // JSON-fallback has no tool_call_id — use a plain user
                        // message as the tool result (documented simplification).
                        conversation.push(serde_json::json!({
                            "role": "user",
                            "content": format!("tool result:\n{}", outcome.payload)
                        }));

                        if outcome.is_terminal {
                            break;
                        }
                    }
                    Err(_) => {
                        // Plain text with no parseable action — nudge the model
                        // and teach it the expected JSON format.
                        conversation.push(serde_json::json!({
                            "role": "user",
                            "content": concat!(
                                "Please use a tool to continue, or call the done tool if you are finished. ",
                                r#"Reply with exactly one tool action as JSON: {"tool": "<name>", "args": {…}} "#,
                                "— or use native tool calling."
                            )
                        }));
                    }
                }
            }
        }
    }

    // --- Final cargo check (outside budget). --------------------------------
    let end_errors = match run_cargo_check(ws) {
        Ok(output) => {
            let diags = parse_cargo_diagnostics(&output).unwrap_or_default();
            diags
                .iter()
                .filter(|d| d.level == rustyfi_core::state::DiagnosticLevel::Error)
                .count()
        }
        Err(_) => start_errors, // If check fails to run, report no change.
    };

    DoctorReport {
        start_errors,
        end_errors,
        // The first CargoCheck already consumed 1 call; session.calls_used() is
        // the total including it.
        tool_calls_used: session.calls_used(),
        wall_secs: started.elapsed().as_secs(),
        summary,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // ── Fixture helpers ──────────────────────────────────────────────────────

    /// Build a minimal 2-file crate in a tempdir.  Returns the `TempDir` (to
    /// keep it alive) and the workspace `PathBuf`.
    fn mini_crate() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::TempDir::new().unwrap();
        let ws = tmp.path().to_path_buf();
        let src = ws.join("src");
        fs::create_dir_all(&src).unwrap();

        fs::write(
            ws.join("Cargo.toml"),
            "[package]\nname = \"mini\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\n",
        )
        .unwrap();

        fs::write(src.join("lib.rs"), "pub struct Foo;\npub fn bar() {}\n").unwrap();

        fs::write(src.join("util.rs"), "pub struct Helper;\n").unwrap();

        (tmp, ws)
    }

    fn session(ws: &Path) -> DoctorSession {
        DoctorSession::new(ws, DoctorBudget::default())
    }

    // ── T1: ListFiles shape ──────────────────────────────────────────────────

    #[test]
    fn list_files_includes_rs_and_cargo_toml() {
        let (_tmp, ws) = mini_crate();
        let mut s = session(&ws);
        let out = s.execute(ToolCall::ListFiles);
        assert!(!out.is_terminal);
        let lines: Vec<&str> = out.payload.lines().collect();
        // Must contain src/lib.rs and src/util.rs
        assert!(
            lines.contains(&"src/lib.rs"),
            "src/lib.rs missing from: {:?}",
            lines
        );
        assert!(
            lines.contains(&"src/util.rs"),
            "src/util.rs missing from: {:?}",
            lines
        );
        // Must contain Cargo.toml
        assert!(
            lines.contains(&"Cargo.toml"),
            "Cargo.toml missing from: {:?}",
            lines
        );
        // NEXT_STEPS.md not present → must not appear
        assert!(
            !lines.contains(&"NEXT_STEPS.md"),
            "NEXT_STEPS.md should not appear when absent"
        );
    }

    #[test]
    fn list_files_includes_next_steps_when_present() {
        let (_tmp, ws) = mini_crate();
        fs::write(ws.join("NEXT_STEPS.md"), "fix stuff\n").unwrap();
        let mut s = session(&ws);
        let out = s.execute(ToolCall::ListFiles);
        let lines: Vec<&str> = out.payload.lines().collect();
        assert!(
            lines.contains(&"NEXT_STEPS.md"),
            "NEXT_STEPS.md should appear when present"
        );
    }

    // ── T2: ReadFile confinement ─────────────────────────────────────────────

    #[test]
    fn read_src_file_succeeds() {
        let (_tmp, ws) = mini_crate();
        let mut s = session(&ws);
        let out = s.execute(ToolCall::ReadFile {
            path: "src/lib.rs".into(),
        });
        assert!(!out.is_terminal);
        assert!(
            !out.payload.starts_with("error:"),
            "unexpected error: {}",
            out.payload
        );
        assert!(out.payload.contains("pub struct Foo"));
    }

    #[test]
    fn read_cargo_toml_succeeds() {
        let (_tmp, ws) = mini_crate();
        let mut s = session(&ws);
        let out = s.execute(ToolCall::ReadFile {
            path: "Cargo.toml".into(),
        });
        assert!(!out.is_terminal);
        assert!(!out.payload.starts_with("error:"), "{}", out.payload);
        assert!(out.payload.contains("[package]"));
    }

    #[test]
    fn read_dotdot_is_rejected() {
        let (_tmp, ws) = mini_crate();
        let mut s = session(&ws);
        let out = s.execute(ToolCall::ReadFile {
            path: "../secret.txt".into(),
        });
        assert!(
            out.payload.starts_with("error:"),
            "expected error for .., got: {}",
            out.payload
        );
        assert!(
            out.payload.contains("path traversal is not allowed"),
            "expected guard-specific phrase 'path traversal is not allowed', got: {}",
            out.payload
        );
    }

    #[test]
    fn read_absolute_path_is_rejected() {
        let (_tmp, ws) = mini_crate();
        let mut s = session(&ws);
        let out = s.execute(ToolCall::ReadFile {
            path: "/etc/passwd".into(),
        });
        assert!(
            out.payload.starts_with("error:"),
            "expected error for absolute path"
        );
        assert!(
            out.payload.contains("absolute paths are not allowed"),
            "expected guard-specific phrase 'absolute paths are not allowed', got: {}",
            out.payload
        );
    }

    #[test]
    fn read_outside_allowed_files_is_rejected() {
        let (_tmp, ws) = mini_crate();
        // Create a file that exists but is not under src/ or one of the allowed files.
        fs::write(ws.join("secrets.txt"), "top secret\n").unwrap();
        let mut s = session(&ws);
        let out = s.execute(ToolCall::ReadFile {
            path: "secrets.txt".into(),
        });
        assert!(
            out.payload.starts_with("error:"),
            "expected error for file outside allowed locations: {}",
            out.payload
        );
        assert!(
            out.payload.contains("read not allowed"),
            "expected guard-specific phrase 'read not allowed', got: {}",
            out.payload
        );
    }

    // ── T3: ReadFile truncation ──────────────────────────────────────────────

    #[test]
    fn read_large_file_is_truncated() {
        let (_tmp, ws) = mini_crate();
        // Write a file larger than READ_CAP.
        let big_content: String = "x".repeat(READ_CAP + 1_000);
        fs::write(ws.join("src/big.rs"), &big_content).unwrap();

        let mut s = session(&ws);
        let out = s.execute(ToolCall::ReadFile {
            path: "src/big.rs".into(),
        });
        assert!(
            out.payload.contains("…[truncated]"),
            "expected truncation marker"
        );
        assert!(
            out.payload.len() <= READ_CAP + 20, // +20 for the marker itself
            "payload too large: {} bytes",
            out.payload.len()
        );
    }

    // ── T4: Search hit and miss ──────────────────────────────────────────────

    #[test]
    fn search_hit_returns_definition() {
        let (_tmp, ws) = mini_crate();
        let mut s = session(&ws);
        let out = s.execute(ToolCall::Search {
            symbol: "Foo".into(),
        });
        assert!(!out.is_terminal);
        assert!(
            out.payload.contains("Foo"),
            "expected Foo in search result: {}",
            out.payload
        );
        assert!(
            !out.payload.starts_with("no matches"),
            "unexpected no-match: {}",
            out.payload
        );
    }

    #[test]
    fn search_miss_returns_no_matches() {
        let (_tmp, ws) = mini_crate();
        let mut s = session(&ws);
        let out = s.execute(ToolCall::Search {
            symbol: "DoesNotExist".into(),
        });
        assert!(
            out.payload.contains("no matches"),
            "expected no-matches for unknown symbol: {}",
            out.payload
        );
    }

    // ── T5: WriteFile rebuilds index ─────────────────────────────────────────

    #[test]
    fn write_rebuilds_index_and_search_finds_new_item() {
        let (_tmp, ws) = mini_crate();
        let mut s = session(&ws);

        // Before write: NewThing should not be found.
        let before = s.execute(ToolCall::Search {
            symbol: "NewThing".into(),
        });
        assert!(
            before.payload.contains("no matches"),
            "NewThing found before write: {}",
            before.payload
        );

        // Write a file defining NewThing.
        let write_out = s.execute(ToolCall::WriteFile {
            path: "src/new_thing.rs".into(),
            content: "pub struct NewThing;\n".into(),
        });
        assert!(
            !write_out.payload.starts_with("error:"),
            "write failed: {}",
            write_out.payload
        );
        assert!(
            write_out.payload.starts_with("wrote"),
            "unexpected write payload: {}",
            write_out.payload
        );

        // After write: NewThing must be found.
        let after = s.execute(ToolCall::Search {
            symbol: "NewThing".into(),
        });
        assert!(
            after.payload.contains("NewThing"),
            "NewThing not found after write: {}",
            after.payload
        );
    }

    // ── T6: WriteFile confinement ────────────────────────────────────────────

    #[test]
    fn write_outside_src_is_rejected() {
        let (_tmp, ws) = mini_crate();
        let mut s = session(&ws);
        let out = s.execute(ToolCall::WriteFile {
            path: "Cargo.toml".into(),
            content: "evil".into(),
        });
        assert!(
            out.payload.starts_with("error:"),
            "expected error for write outside src/: {}",
            out.payload
        );
        assert!(
            out.payload.contains("writes are only allowed under src/"),
            "expected guard-specific phrase 'writes are only allowed under src/', got: {}",
            out.payload
        );
    }

    #[test]
    fn write_dotdot_is_rejected() {
        let (_tmp, ws) = mini_crate();
        let mut s = session(&ws);
        let out = s.execute(ToolCall::WriteFile {
            path: "../escape.rs".into(),
            content: "evil".into(),
        });
        assert!(
            out.payload.starts_with("error:"),
            "expected error for ..: {}",
            out.payload
        );
        assert!(
            out.payload.contains("path traversal is not allowed"),
            "expected guard-specific phrase 'path traversal is not allowed', got: {}",
            out.payload
        );
    }

    #[test]
    fn write_absolute_is_rejected() {
        let (_tmp, ws) = mini_crate();
        let mut s = session(&ws);
        let out = s.execute(ToolCall::WriteFile {
            path: "/tmp/evil.rs".into(),
            content: "evil".into(),
        });
        assert!(
            out.payload.starts_with("error:"),
            "expected error for absolute path"
        );
        assert!(
            out.payload.contains("absolute paths are not allowed"),
            "expected guard-specific phrase 'absolute paths are not allowed', got: {}",
            out.payload
        );
    }

    // ── T7: Done is terminal ─────────────────────────────────────────────────

    #[test]
    fn done_returns_terminal_outcome() {
        let (_tmp, ws) = mini_crate();
        let mut s = session(&ws);
        let out = s.execute(ToolCall::Done {
            summary: "all fixed".into(),
        });
        assert!(out.is_terminal, "Done should be terminal");
        assert_eq!(out.payload, "all fixed");
    }

    // ── T8: Budget exhaustion ────────────────────────────────────────────────

    #[test]
    fn budget_terminal_on_third_call_when_max_is_two() {
        let (_tmp, ws) = mini_crate();
        let budget = DoctorBudget {
            max_tool_calls: 2,
            max_wall_secs: 1200,
        };
        let mut s = DoctorSession::new(&ws, budget);

        let out1 = s.execute(ToolCall::ListFiles);
        assert!(!out1.is_terminal, "call 1 should not be terminal");

        let out2 = s.execute(ToolCall::ListFiles);
        assert!(!out2.is_terminal, "call 2 should not be terminal");

        // Third call: calls_used becomes 3 > max_tool_calls (2) → budget exhausted.
        let out3 = s.execute(ToolCall::ListFiles);
        assert!(out3.is_terminal, "call 3 should be terminal (budget)");
        assert_eq!(out3.payload, "budget exhausted");
    }

    #[test]
    fn calls_used_tracks_count() {
        let (_tmp, ws) = mini_crate();
        let mut s = session(&ws);
        assert_eq!(s.calls_used(), 0);
        s.execute(ToolCall::ListFiles);
        assert_eq!(s.calls_used(), 1);
        s.execute(ToolCall::ListFiles);
        assert_eq!(s.calls_used(), 2);
    }

    #[test]
    fn bad_tool_calls_consume_budget() {
        let (_tmp, ws) = mini_crate();
        let budget = DoctorBudget {
            max_tool_calls: 2,
            max_wall_secs: 1200,
        };
        let mut s = DoctorSession::new(&ws, budget);

        // Simulate invalid tool calls by directly invoking count_invalid_call.
        assert_eq!(s.calls_used(), 0);
        assert!(!s.budget_exhausted());

        // First invalid call.
        s.count_invalid_call();
        assert_eq!(s.calls_used(), 1);
        assert!(!s.budget_exhausted());

        // Second invalid call.
        s.count_invalid_call();
        assert_eq!(s.calls_used(), 2);
        assert!(!s.budget_exhausted());

        // Third invalid call consumes the last unit of budget.
        s.count_invalid_call();
        assert_eq!(s.calls_used(), 3);
        assert!(
            s.budget_exhausted(),
            "budget should be exhausted after 3 calls with max_tool_calls=2"
        );
    }

    // ── T9: tail_truncate helper ─────────────────────────────────────────────

    #[test]
    fn tail_truncate_passthrough_when_short() {
        let s = "hello".to_string();
        assert_eq!(tail_truncate(s.clone(), 100), s);
    }

    #[test]
    fn tail_truncate_appends_marker_when_long() {
        let s = "a".repeat(50);
        let out = tail_truncate(s, 10);
        assert!(out.contains("…[truncated]"));
        // Output must be a valid UTF-8 string (no panic).
        let _ = out.len();
    }

    // ── T10: CargoCheck on a real valid crate (ignored — runs cargo) ─────────

    #[test]
    #[ignore]
    fn cargo_check_on_valid_crate_returns_zero_errors() {
        let (_tmp, ws) = mini_crate();
        let mut s = session(&ws);
        let out = s.execute(ToolCall::CargoCheck);
        assert!(!out.is_terminal);
        // A clean crate should report 0 errors.
        assert!(
            out.payload.contains("error count: 0"),
            "expected zero errors on clean crate: {}",
            out.payload
        );
        assert_eq!(s.last_error_count(), Some(0));
    }

    // ── ScriptedTransport ────────────────────────────────────────────────────

    /// A test-only transport that replays a pre-scripted sequence of turns.
    /// Each call to `turn` pops the next `AssistantTurn` from the front of the
    /// queue (panics if the queue is empty, which catches test scripts that are
    /// too short).
    ///
    /// Upholds the `DoctorTransport` contract: `turn()` appends a synthesised
    /// assistant message to `conversation` before returning so the driver never
    /// needs to push one.
    ///
    /// For `ToolInvocation` turns a minimal `{role:"assistant","tool_calls":[…]}`
    /// stub is appended.  For `Text` turns a `{role:"assistant","content":"…"}`
    /// message is appended.  The tool_call_id round-trip path in `run_doctor`
    /// falls back to a `{role:"user"}` result message when the id is absent —
    /// this is the documented simplification for the scripted test path.
    struct ScriptedTransport(std::collections::VecDeque<AssistantTurn>);

    impl ScriptedTransport {
        fn from(turns: Vec<AssistantTurn>) -> Self {
            ScriptedTransport(turns.into_iter().collect())
        }
    }

    impl DoctorTransport for ScriptedTransport {
        fn turn(
            &mut self,
            conversation: &mut Vec<serde_json::Value>,
            _tools: &serde_json::Value,
        ) -> Result<AssistantTurn, EngineError> {
            let turn = self
                .0
                .pop_front()
                .ok_or_else(|| EngineError::Llm("ScriptedTransport queue exhausted".into()))?;

            // Uphold the transport contract: append the assistant message BEFORE
            // returning so the driver sees it as the last message.
            match &turn {
                AssistantTurn::ToolInvocation {
                    name,
                    arguments_json,
                } => {
                    conversation.push(serde_json::json!({
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": "scripted_call",
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": arguments_json
                            }
                        }]
                    }));
                }
                AssistantTurn::Text(text) => {
                    conversation.push(serde_json::json!({
                        "role": "assistant",
                        "content": text
                    }));
                }
            }

            Ok(turn)
        }
    }

    // ── T11b: single assistant message per Text turn ─────────────────────────

    /// After a Text turn via ScriptedTransport the conversation must contain
    /// exactly ONE assistant message for that turn — no duplicates.
    #[test]
    fn scripted_text_turn_produces_exactly_one_assistant_message() {
        // Build a minimal session / transport setup that we drive manually
        // rather than through run_doctor, so we can inspect the conversation
        // slice directly.
        let (_tmp, _ws) = mini_crate();
        let tools = tools_schema(false);
        let mut conversation: Vec<serde_json::Value> = vec![serde_json::json!({
            "role": "user",
            "content": "initial seed"
        })];

        let mut transport = ScriptedTransport::from(vec![AssistantTurn::Text(
            "I will look at the files.".to_string(),
        )]);

        let turn = transport
            .turn(&mut conversation, &tools)
            .expect("turn should succeed");
        assert!(matches!(turn, AssistantTurn::Text(_)), "expected Text turn");

        // Count assistant messages in the conversation.
        let assistant_count = conversation
            .iter()
            .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("assistant"))
            .count();
        assert_eq!(
            assistant_count, 1,
            "expected exactly 1 assistant message after Text turn, got {assistant_count}; conversation: {conversation:?}"
        );
    }

    // ── T11: parse_action_reply ──────────────────────────────────────────────

    #[test]
    fn parse_action_list_files() {
        let reply = r#"{"tool":"list_files","args":{}}"#;
        let tc = parse_action_reply(reply).unwrap();
        assert!(matches!(tc, ToolCall::ListFiles));
    }

    #[test]
    fn parse_action_cargo_check() {
        let reply = r#"{"tool":"cargo_check","args":{}}"#;
        let tc = parse_action_reply(reply).unwrap();
        assert!(matches!(tc, ToolCall::CargoCheck));
    }

    #[test]
    fn parse_action_read_file() {
        let reply = r#"{"tool":"read_file","args":{"path":"src/main.rs"}}"#;
        let tc = parse_action_reply(reply).unwrap();
        match tc {
            ToolCall::ReadFile { path } => assert_eq!(path, "src/main.rs"),
            other => panic!("expected ReadFile, got {other:?}"),
        }
    }

    #[test]
    fn parse_action_search() {
        let reply = r#"{"tool":"search","args":{"symbol":"Foo"}}"#;
        let tc = parse_action_reply(reply).unwrap();
        match tc {
            ToolCall::Search { symbol } => assert_eq!(symbol, "Foo"),
            other => panic!("expected Search, got {other:?}"),
        }
    }

    #[test]
    fn parse_action_explain() {
        let reply = r#"{"tool":"explain","args":{"code":"E0308"}}"#;
        let tc = parse_action_reply(reply).unwrap();
        match tc {
            ToolCall::Explain { code } => assert_eq!(code, "E0308"),
            other => panic!("expected Explain, got {other:?}"),
        }
    }

    #[test]
    fn parse_action_write_file() {
        let reply =
            r#"{"tool":"write_file","args":{"path":"src/lib.rs","content":"pub fn foo() {}"}}"#;
        let tc = parse_action_reply(reply).unwrap();
        match tc {
            ToolCall::WriteFile { path, content } => {
                assert_eq!(path, "src/lib.rs");
                assert_eq!(content, "pub fn foo() {}");
            }
            other => panic!("expected WriteFile, got {other:?}"),
        }
    }

    #[test]
    fn parse_action_done() {
        let reply = r#"{"tool":"done","args":{"summary":"all clean"}}"#;
        let tc = parse_action_reply(reply).unwrap();
        match tc {
            ToolCall::Done { summary } => assert_eq!(summary, "all clean"),
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn parse_action_fenced_json() {
        let reply = "I'll do this:\n```json\n{\"tool\":\"cargo_check\",\"args\":{}}\n```\n";
        let tc = parse_action_reply(reply).unwrap();
        assert!(matches!(tc, ToolCall::CargoCheck));
    }

    #[test]
    fn parse_action_prose_with_embedded_json() {
        let reply =
            "Let me list the files first. {\"tool\":\"list_files\",\"args\":{}} That should help.";
        let tc = parse_action_reply(reply).unwrap();
        assert!(matches!(tc, ToolCall::ListFiles));
    }

    #[test]
    fn parse_action_malformed_json_is_err() {
        let reply = r#"{"tool":"list_files","args": oops}"#;
        let result = parse_action_reply(reply);
        assert!(result.is_err(), "expected Err for malformed JSON");
    }

    #[test]
    fn parse_action_unknown_tool_is_err() {
        let reply = r#"{"tool":"fly_to_moon","args":{}}"#;
        let result = parse_action_reply(reply);
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(
            msg.contains("unknown tool"),
            "expected 'unknown tool' in error: {msg}"
        );
    }

    #[test]
    fn parse_action_missing_required_arg_is_err() {
        // read_file without path
        let reply = r#"{"tool":"read_file","args":{}}"#;
        let result = parse_action_reply(reply);
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(
            msg.contains("path"),
            "expected 'path' mentioned in error: {msg}"
        );
    }

    #[test]
    fn parse_action_no_json_object_is_err() {
        let result = parse_action_reply("just some text with no json");
        assert!(result.is_err());
    }

    // ── T12: run_doctor integration (scripted, #[ignore] — runs cargo) ───────

    #[test]
    #[ignore]
    fn run_doctor_scripted_fixes_compile_error() {
        // Build a crate with a deliberate compile error.
        let tmp = tempfile::TempDir::new().unwrap();
        let ws = tmp.path().to_path_buf();
        let src = ws.join("src");
        fs::create_dir_all(&src).unwrap();

        fs::write(
            ws.join("Cargo.toml"),
            "[package]\nname = \"broken\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\n",
        )
        .unwrap();

        // Deliberately broken: calls an undefined function.
        fs::write(src.join("main.rs"), "fn main() { does_not_exist(); }\n").unwrap();

        // Fixed version that compiles.
        let fixed_content = "fn main() {}\n".to_string();

        // Script: CargoCheck (seeded by driver) → ReadFile → WriteFile(fixed) → Done
        // Note: the driver itself executes the first CargoCheck as part of seeding.
        // The scripted turns correspond to model responses AFTER the seed message.
        let turns = vec![
            AssistantTurn::ToolInvocation {
                name: "read_file".to_string(),
                arguments_json: r#"{"path":"src/main.rs"}"#.to_string(),
            },
            AssistantTurn::ToolInvocation {
                name: "write_file".to_string(),
                arguments_json: format!(
                    r#"{{"path":"src/main.rs","content":{}}}"#,
                    serde_json::to_string(&fixed_content).unwrap()
                ),
            },
            AssistantTurn::ToolInvocation {
                name: "done".to_string(),
                arguments_json: r#"{"summary":"fixed the undefined function"}"#.to_string(),
            },
        ];

        let mut transport = ScriptedTransport::from(turns);
        let budget = DoctorBudget {
            max_tool_calls: 20,
            max_wall_secs: 300,
        };

        let mut log = Vec::new();
        let report = run_doctor(&ws, &mut transport, budget, None, &mut |msg| log.push(msg));

        // The driver runs the first CargoCheck (1 call) + 3 scripted tool calls = 4 total.
        assert_eq!(
            report.tool_calls_used, 4,
            "expected 4 tool calls (1 seed + 3 scripted), got {}",
            report.tool_calls_used
        );
        assert!(
            report.start_errors > 0,
            "start_errors should be > 0 for a broken crate, got {}",
            report.start_errors
        );
        assert_eq!(
            report.end_errors, 0,
            "end_errors should be 0 after fix, got {}",
            report.end_errors
        );
        assert!(
            report.end_errors < report.start_errors,
            "end_errors ({}) should be < start_errors ({})",
            report.end_errors,
            report.start_errors
        );
    }

    // ── T13: text turn with embedded JSON action executes the tool ───────────

    /// A Text turn that contains a JSON-encoded action should execute the tool
    /// and count it in tool_calls_used, not just nudge the model.  The session
    /// ends when a Text turn encodes `done` as JSON.
    ///
    /// Marked #[ignore] because the driver seeds the session with a real
    /// `cargo check` call, consistent with the suite's convention for tests
    /// that invoke cargo.
    #[test]
    #[ignore]
    fn text_turn_with_json_action_executes_tool() {
        let (_tmp, ws) = mini_crate();

        // Script two Text turns, each carrying an embedded JSON action:
        //   turn 1 → list_files (as JSON prose)
        //   turn 2 → done      (as JSON prose)
        // Neither uses native ToolInvocation — the JSON-fallback path should
        // parse and execute both.
        let turns = vec![
            AssistantTurn::Text(
                r#"Let me start by listing the files. {"tool":"list_files","args":{}}"#.to_string(),
            ),
            AssistantTurn::Text(
                r#"All done. {"tool":"done","args":{"summary":"no errors found"}}"#.to_string(),
            ),
        ];

        let mut transport = ScriptedTransport::from(turns);
        let budget = DoctorBudget {
            max_tool_calls: 20,
            max_wall_secs: 300,
        };

        let mut log: Vec<String> = Vec::new();
        let report = run_doctor(&ws, &mut transport, budget, None, &mut |msg| log.push(msg));

        // The driver seeds with CargoCheck (1 call) + list_files (1) + done (1) = 3 total.
        assert_eq!(
            report.tool_calls_used, 3,
            "expected 3 tool calls (1 seed + list_files + done), got {}",
            report.tool_calls_used
        );

        // The summary should come from the done action's args.
        assert_eq!(
            report.summary, "no errors found",
            "expected summary from done JSON action, got: {}",
            report.summary
        );

        // Both JSON-fallback tool calls should be reflected in the progress log.
        assert!(
            log.iter()
                .any(|m| m.contains("list_files") && m.contains("json-fallback")),
            "expected a log entry for list_files via json-fallback; log: {log:?}"
        );
    }

    // ── T14: RunBehaviorChecks tool ──────────────────────────────────────────

    #[test]
    fn run_behavior_checks_reports_mismatch() {
        use crate::behavior::{BehaviorSpec, Case, CompareSpec, Expect, Provenance, Side};
        let tmp = tempfile::tempdir().unwrap();
        let spec = BehaviorSpec {
            name: "t".into(),
            source: Side {
                lang: "sh".into(),
                dir: ".".into(),
                build: vec![],
                run: vec![],
            },
            target: Side {
                lang: "sh".into(),
                dir: ".".into(),
                build: vec![],
                run: vec!["sh".into(), "-c".into(), "printf WRONG".into(), "sh".into()],
            },
            compare: CompareSpec::default(),
            normalize: vec![],
            cases: vec![Case {
                name: "c".into(),
                provenance: Provenance::Manual,
                args: vec![],
                stdin: None,
                env: Default::default(),
                expect: Some(Expect {
                    stdout: "OK".into(),
                    stderr: String::new(),
                    exit_code: 0,
                }),
                nondeterministic: false,
                compare: None,
            }],
        };
        let work = tempfile::tempdir().unwrap();
        let mut session = DoctorSession::new(tmp.path(), DoctorBudget::default())
            .with_behavior(spec, work.path().to_path_buf());
        let out = session.execute(ToolCall::RunBehaviorChecks);
        assert!(!out.is_terminal);
        assert!(out.payload.contains("c"));
        assert!(
            out.payload.to_lowercase().contains("mismatch") || out.payload.contains("0/1"),
            "expected mismatch indicator in: {}",
            out.payload
        );
    }

    #[test]
    fn run_behavior_checks_without_corpus_is_a_noop_error() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = DoctorSession::new(tmp.path(), DoctorBudget::default());
        let out = session.execute(ToolCall::RunBehaviorChecks);
        assert!(!out.is_terminal);
        assert!(
            out.payload.contains("no behavioral corpus"),
            "expected 'no behavioral corpus' in: {}",
            out.payload
        );
    }

    #[test]
    fn parses_run_behavior_checks_action() {
        let t = parse_action_reply(r#"{"tool":"run_behavior_checks","args":{}}"#).unwrap();
        assert!(matches!(t, ToolCall::RunBehaviorChecks));
    }

    // ── T15: run_doctor scripted behavioral repair (ignored — runs cargo+sh) ──

    /// End-to-end test: a crate that compiles but prints the wrong output is
    /// repaired by the doctor using the run_behavior_checks tool.
    ///
    /// Script: run_behavior_checks (sees mismatch) → write_file (fix main.rs)
    ///         → run_behavior_checks (now matches) → done
    ///
    /// The driver seeds with CargoCheck (1) + initial RunBehaviorChecks (1)
    /// (because behavior is attached), then 4 scripted turns = 6 total.
    #[test]
    #[ignore]
    fn run_doctor_scripted_behavior_repair() {
        use crate::behavior::{BehaviorSpec, Case, CompareSpec, Expect, Provenance, Side};

        // ---- Build a crate that compiles but prints "WRONG". ----------------
        let crate_tmp = tempfile::TempDir::new().unwrap();
        let crate_dir = crate_tmp.path().to_path_buf();
        let src = crate_dir.join("src");
        fs::create_dir_all(&src).unwrap();

        fs::write(
            crate_dir.join("Cargo.toml"),
            "[package]\nname = \"btest\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\n",
        )
        .unwrap();
        fs::write(src.join("main.rs"), "fn main() { print!(\"WRONG\"); }\n").unwrap();

        // ---- Build a BehaviorSpec expecting "OK". ---------------------------
        let spec = BehaviorSpec {
            name: "btest".into(),
            source: Side {
                lang: "rust".into(),
                dir: ".".into(),
                build: vec![],
                run: vec![],
            },
            target: Side {
                lang: "rust".into(),
                dir: ".".into(),
                build: vec!["cargo".into(), "build".into(), "--quiet".into()],
                run: vec!["target/debug/btest".into()],
            },
            compare: CompareSpec::default(),
            normalize: vec![],
            cases: vec![Case {
                name: "prints_ok".into(),
                provenance: Provenance::Manual,
                args: vec![],
                stdin: None,
                env: Default::default(),
                expect: Some(Expect {
                    stdout: "OK".into(),
                    stderr: String::new(),
                    exit_code: 0,
                }),
                nondeterministic: false,
                compare: None,
            }],
        };

        let work = tempfile::TempDir::new().unwrap();

        // ---- Script the doctor turns. --------------------------------------
        // The driver seeds with: CargoCheck (1) + RunBehaviorChecks (1).
        // Scripted model turns:
        //   1. run_behavior_checks  — see the mismatch
        //   2. write_file           — fix src/main.rs to print "OK"
        //   3. run_behavior_checks  — confirm it now matches
        //   4. done
        let fixed_main = "fn main() { print!(\"OK\"); }\n".to_string();
        let turns = vec![
            AssistantTurn::ToolInvocation {
                name: "run_behavior_checks".to_string(),
                arguments_json: "{}".to_string(),
            },
            AssistantTurn::ToolInvocation {
                name: "write_file".to_string(),
                arguments_json: format!(
                    r#"{{"path":"src/main.rs","content":{}}}"#,
                    serde_json::to_string(&fixed_main).unwrap()
                ),
            },
            AssistantTurn::ToolInvocation {
                name: "run_behavior_checks".to_string(),
                arguments_json: "{}".to_string(),
            },
            AssistantTurn::ToolInvocation {
                name: "done".to_string(),
                arguments_json: r#"{"summary":"fixed behavioral mismatch"}"#.to_string(),
            },
        ];

        let mut transport = ScriptedTransport::from(turns);
        let budget = DoctorBudget {
            max_tool_calls: 20,
            max_wall_secs: 300,
        };

        let mut log: Vec<String> = Vec::new();
        let _report = run_doctor(
            &crate_dir,
            &mut transport,
            budget,
            Some((spec.clone(), work.path().to_path_buf())),
            &mut |msg| log.push(msg),
        );

        // ---- Assert the repair actually worked. ----------------------------
        let result = crate::behavior::verify(&spec, &crate_dir, work.path())
            .expect("verify should succeed after repair");
        assert!(
            result.total >= 1,
            "expected at least 1 case in the spec, got {}",
            result.total
        );
        assert_eq!(
            result.matched, result.total,
            "expected all {} cases to match after repair, got {} matched; log: {log:?}",
            result.total, result.matched
        );
    }
}
