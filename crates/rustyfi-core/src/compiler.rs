use std::path::Path;
use std::process::{Command, Output};

use crate::errors::CompilerError;
use crate::state::{
    CargoOutput, CompilerDiagnostic, DiagnosticFamily, DiagnosticLevel, DiagnosticSpan,
};

// ---------------------------------------------------------------------------
// Raw subprocess execution
// ---------------------------------------------------------------------------

/// Invokes `cargo check --message-format=json` in the given workspace
/// directory and returns the raw captured output.
///
/// # Design notes
///
/// * No panics — all error paths are returned as [`CompilerError`].
/// * stdout and stderr are captured separately and preserved verbatim.
/// * Callers should drive retry orchestration; this function is a single-shot
///   harness with no retry logic of its own.
/// * Full JSON parsing is deferred to [`parse_cargo_diagnostics`]; this
///   function only captures bytes.
pub fn run_cargo_check(workspace_path: &Path) -> Result<CargoOutput, CompilerError> {
    let output: Output = Command::new("cargo")
        .args(["check", "--message-format=json"])
        .current_dir(workspace_path)
        .output()
        .map_err(|e| CompilerError::SpawnFailure {
            reason: e.to_string(),
        })?;

    let stdout_raw =
        String::from_utf8(output.stdout).map_err(|e| CompilerError::OutputEncoding {
            reason: e.to_string(),
        })?;

    let stderr_raw =
        String::from_utf8(output.stderr).map_err(|e| CompilerError::OutputEncoding {
            reason: e.to_string(),
        })?;

    let exit_code = output.status.code();

    Ok(CargoOutput {
        stdout_lines: stdout_raw.lines().map(str::to_owned).collect(),
        stderr_lines: stderr_raw.lines().map(str::to_owned).collect(),
        exit_code,
    })
}

// ---------------------------------------------------------------------------
// Diagnostic JSON parsing
// ---------------------------------------------------------------------------

/// Parses the `stdout_lines` of a [`CargoOutput`] into a structured
/// [`Vec<CompilerDiagnostic>`].
///
/// `cargo check --message-format=json` emits one JSON object per line.
/// Only lines whose top-level `"reason"` field equals `"compiler-message"`
/// are relevant; all others are silently skipped (they convey build-graph
/// metadata, not diagnostics).
///
/// # Errors
///
/// This parser is deliberately lenient: malformed lines and diagnostics with
/// unrecognised levels are skipped rather than aborting the whole parse.
/// Losing one line must never cost us the rest of the diagnostics — the fix
/// loop depends on them.  Lines with unknown `reason` values are passed
/// over without error.
pub fn parse_cargo_diagnostics(
    output: &CargoOutput,
) -> Result<Vec<CompilerDiagnostic>, CompilerError> {
    let mut diagnostics = Vec::new();

    for line in output.stdout_lines.iter() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Parse into an untyped value first so we can inspect `reason`.
        // Non-JSON lines (cargo banners, panics, etc.) are skipped.
        let raw: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let reason = raw.get("reason").and_then(|v| v.as_str()).unwrap_or("");
        if reason != "compiler-message" {
            continue;
        }

        let msg_value = match raw.get("message") {
            Some(v) => v,
            None => continue,
        };

        if let Some(diagnostic) = deserialize_diagnostic(msg_value) {
            diagnostics.push(diagnostic);
        }
    }

    Ok(diagnostics)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Deserializes a single `message` object from a `compiler-message` envelope.
/// Returns `None` only if the object is too malformed to be useful.
fn deserialize_diagnostic(msg: &serde_json::Value) -> Option<CompilerDiagnostic> {
    let level_str = msg.get("level").and_then(|v| v.as_str()).unwrap_or("note");

    // Unknown levels (future rustc versions) degrade to Note — never abort.
    let level = parse_diagnostic_level(level_str).unwrap_or(DiagnosticLevel::Note);

    let message = msg
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();

    let code = msg
        .get("code")
        .and_then(|c| c.get("code"))
        .and_then(|v| v.as_str())
        .map(str::to_owned);

    let rendered = msg
        .get("rendered")
        .and_then(|v| v.as_str())
        .map(str::to_owned);

    let spans = msg
        .get("spans")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(deserialize_span).collect::<Vec<_>>())
        .unwrap_or_default();

    Some(CompilerDiagnostic {
        level,
        message,
        code,
        spans,
        rendered,
    })
}

/// Maps a rustc level string to the typed [`DiagnosticLevel`] enum.
///
/// `failure-note` is the trailer rustc appends to every failing build
/// ("aborting due to N previous errors") — it must parse as a Note, not
/// abort the whole diagnostic stream.
fn parse_diagnostic_level(s: &str) -> Option<DiagnosticLevel> {
    match s {
        "help" => Some(DiagnosticLevel::Help),
        "note" | "failure-note" => Some(DiagnosticLevel::Note),
        "warning" => Some(DiagnosticLevel::Warning),
        "error" => Some(DiagnosticLevel::Error),
        "error: internal compiler error" => Some(DiagnosticLevel::Ice),
        _ => None,
    }
}

/// Best-effort deserialization of a `span` object.  Returns `None` if
/// required fields are absent (the span is silently dropped).
fn deserialize_span(s: &serde_json::Value) -> Option<DiagnosticSpan> {
    Some(DiagnosticSpan {
        file_name: s.get("file_name")?.as_str()?.to_owned(),
        line_start: s.get("line_start")?.as_u64()? as u32,
        line_end: s.get("line_end")?.as_u64()? as u32,
        column_start: s.get("column_start")?.as_u64()? as u32,
        column_end: s.get("column_end")?.as_u64()? as u32,
        is_primary: s.get("is_primary")?.as_bool()?,
        label: s.get("label").and_then(|v| v.as_str()).map(str::to_owned),
    })
}

// ---------------------------------------------------------------------------
// Diagnostic classification
// ---------------------------------------------------------------------------

/// Classify a [`CompilerDiagnostic`] into a [`DiagnosticFamily`].
///
/// Uses a two-pass strategy:
///
/// 1. **Code pass** — exact match on the rustc `E####` error code.  This is
///    the most reliable signal and is attempted first.
/// 2. **Text pass** — heuristic keyword scan of the message string.  Used
///    when no code is present or when the code was not recognised (e.g. future
///    rustc versions).
///
/// The function is pure: no I/O, no allocation beyond the `DiagnosticFamily`
/// return value.
pub fn classify_diagnostic(diag: &CompilerDiagnostic) -> DiagnosticFamily {
    // ── Pass 1: error code ─────────────────────────────────────────────────
    if let Some(ref code) = diag.code {
        match code.as_str() {
            // Lifetime
            "E0106" | "E0495" | "E0597" | "E0621" | "E0622" | "E0623" | "E0700" => {
                return DiagnosticFamily::MissingLifetime;
            }

            // Trait bounds — check for async context before committing.
            "E0277" | "E0283" | "E0284" | "E0369" | "E0391" | "E0275" => {
                let m = diag.message.to_lowercase();
                if m.contains("send")
                    || m.contains("sync")
                    || m.contains("future")
                    || m.contains("async")
                    || m.contains("unpin")
                {
                    return DiagnosticFamily::AsyncMismatch;
                }
                return DiagnosticFamily::TraitBoundFailure;
            }

            // Ownership / move
            "E0382" | "E0505" | "E0506" | "E0507" | "E0508" | "E0509" => {
                return DiagnosticFamily::OwnershipMove;
            }

            // Borrow conflicts
            "E0499" | "E0500" | "E0501" | "E0502" | "E0503" | "E0504" | "E0515" | "E0521" => {
                return DiagnosticFamily::BorrowConflict;
            }

            // Type mismatch / inference
            "E0308" | "E0309" | "E0310" | "E0365" | "E0606" | "E0607" => {
                let m = diag.message.to_lowercase();
                if m.contains("integer") || m.contains("overflow") || m.contains("truncat") {
                    return DiagnosticFamily::IntegerOverflow;
                }
                return DiagnosticFamily::TypeMismatch;
            }

            // Missing import / unresolved name
            "E0412" | "E0422" | "E0423" | "E0425" | "E0432" | "E0433" | "E0603" | "E0614" => {
                return DiagnosticFamily::MissingImport;
            }

            // Async / futures
            "E0728" | "E0752" | "E0753" | "E0654" | "E0746" => {
                return DiagnosticFamily::AsyncMismatch;
            }

            // Pattern exhaustiveness
            "E0004" | "E0005" => {
                return DiagnosticFamily::PatternExhaustiveness;
            }

            // Internal compiler error
            "E0021" => {
                return DiagnosticFamily::InternalCompilerError;
            }

            _ => { /* fall through to text pass */ }
        }
    }

    // ── Pass 2: message text heuristics ────────────────────────────────────
    let m = diag.message.to_lowercase();

    if m.contains("lifetime")
        || m.contains("does not live long enough")
        || m.contains("borrowed value does not live")
    {
        return DiagnosticFamily::MissingLifetime;
    }

    if m.contains("cannot move out")
        || m.contains("use of moved value")
        || m.contains("moved out of")
    {
        return DiagnosticFamily::OwnershipMove;
    }

    if m.contains("cannot borrow")
        || m.contains("borrow of moved value")
        || m.contains("two mutable borrows")
    {
        return DiagnosticFamily::BorrowConflict;
    }

    // Async check before generic trait-bound check.
    if m.contains("send") && m.contains("not implemented")
        || m.contains("future") && m.contains("not implemented")
        || m.contains("async")
        || m.contains("await")
    {
        return DiagnosticFamily::AsyncMismatch;
    }

    if m.contains("the trait") && m.contains("is not implemented") || m.contains("trait bound") {
        return DiagnosticFamily::TraitBoundFailure;
    }

    if m.contains("mismatched types") || (m.contains("expected") && m.contains("found")) {
        return DiagnosticFamily::TypeMismatch;
    }

    if m.contains("unresolved import")
        || m.contains("not found in this scope")
        || m.contains("no module named")
        || m.contains("cannot find")
    {
        return DiagnosticFamily::MissingImport;
    }

    if m.contains("overflow") || m.contains("truncat") {
        return DiagnosticFamily::IntegerOverflow;
    }

    if m.contains("macro") {
        return DiagnosticFamily::MacroError;
    }

    if m.contains("non-exhaustive") || m.contains("pattern") && m.contains("not covered") {
        return DiagnosticFamily::PatternExhaustiveness;
    }

    if m.contains("unused") || m.contains("dead code") {
        return DiagnosticFamily::UnusedCode;
    }

    if m.contains("internal compiler error") {
        return DiagnosticFamily::InternalCompilerError;
    }

    DiagnosticFamily::Other(diag.code.clone().unwrap_or_else(|| "unknown".to_string()))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::CargoOutput;

    fn make_output(lines: Vec<&str>) -> CargoOutput {
        CargoOutput {
            stdout_lines: lines.into_iter().map(str::to_owned).collect(),
            stderr_lines: vec![],
            exit_code: Some(0),
        }
    }

    #[test]
    fn skips_non_compiler_message_lines() {
        let output = make_output(vec![
            r#"{"reason":"build-script-executed","package_id":"foo"}"#,
            r#"{"reason":"build-finished","success":true}"#,
        ]);
        let diags = parse_cargo_diagnostics(&output).unwrap();
        assert!(diags.is_empty());
    }

    #[test]
    fn parses_error_diagnostic() {
        let line = r#"{
            "reason":"compiler-message",
            "message":{
                "level":"error",
                "message":"cannot find value `x` in this scope",
                "code":{"code":"E0425","explanation":null},
                "rendered":"error[E0425]: cannot find value `x`",
                "spans":[{
                    "file_name":"src/main.rs",
                    "line_start":5,"line_end":5,
                    "column_start":9,"column_end":10,
                    "is_primary":true,
                    "label":"not found in this scope"
                }],
                "children":[]
            }
        }"#;
        let output = make_output(vec![line]);
        let diags = parse_cargo_diagnostics(&output).unwrap();
        assert_eq!(diags.len(), 1);
        let d = &diags[0];
        assert_eq!(d.level, DiagnosticLevel::Error);
        assert_eq!(d.code.as_deref(), Some("E0425"));
        assert_eq!(d.spans.len(), 1);
        assert!(d.spans[0].is_primary);
    }

    #[test]
    fn empty_stdout_produces_no_diagnostics() {
        let output = make_output(vec![]);
        let diags = parse_cargo_diagnostics(&output).unwrap();
        assert!(diags.is_empty());
    }

    // ── DiagnosticFamily classification tests ──────────────────────────────

    fn make_diag(code: Option<&str>, message: &str) -> CompilerDiagnostic {
        CompilerDiagnostic {
            level: DiagnosticLevel::Error,
            message: message.to_string(),
            code: code.map(str::to_owned),
            spans: vec![],
            rendered: None,
        }
    }

    #[test]
    fn classify_missing_import_by_code() {
        let d = make_diag(Some("E0425"), "cannot find value `x` in this scope");
        assert_eq!(d.family(), DiagnosticFamily::MissingImport);
    }

    #[test]
    fn classify_missing_import_by_code_e0432() {
        let d = make_diag(Some("E0432"), "unresolved import `serde`");
        assert_eq!(d.family(), DiagnosticFamily::MissingImport);
    }

    #[test]
    fn classify_ownership_move_by_code() {
        let d = make_diag(Some("E0382"), "use of moved value: `x`");
        assert_eq!(d.family(), DiagnosticFamily::OwnershipMove);
    }

    #[test]
    fn classify_borrow_conflict_by_code() {
        let d = make_diag(
            Some("E0502"),
            "cannot borrow `x` as mutable because it is also borrowed as immutable",
        );
        assert_eq!(d.family(), DiagnosticFamily::BorrowConflict);
    }

    #[test]
    fn classify_lifetime_by_code() {
        let d = make_diag(Some("E0597"), "borrowed value does not live long enough");
        assert_eq!(d.family(), DiagnosticFamily::MissingLifetime);
    }

    #[test]
    fn classify_type_mismatch_by_code() {
        let d = make_diag(
            Some("E0308"),
            "mismatched types: expected `i32`, found `u64`",
        );
        assert_eq!(d.family(), DiagnosticFamily::TypeMismatch);
    }

    #[test]
    fn classify_async_mismatch_e0277_send() {
        // E0277 with "Send" in message → AsyncMismatch, not TraitBoundFailure
        let d = make_diag(
            Some("E0277"),
            "`MyType` cannot be sent between threads safely: the trait `Send` is not implemented",
        );
        assert_eq!(d.family(), DiagnosticFamily::AsyncMismatch);
    }

    #[test]
    fn classify_trait_bound_e0277_no_async() {
        let d = make_diag(
            Some("E0277"),
            "the trait `Display` is not implemented for `MyType`",
        );
        assert_eq!(d.family(), DiagnosticFamily::TraitBoundFailure);
    }

    #[test]
    fn classify_pattern_exhaustiveness_by_code() {
        let d = make_diag(Some("E0004"), "non-exhaustive patterns: `None` not covered");
        assert_eq!(d.family(), DiagnosticFamily::PatternExhaustiveness);
    }

    #[test]
    fn classify_via_text_when_no_code_lifetime() {
        let d = make_diag(None, "borrowed value does not live long enough");
        assert_eq!(d.family(), DiagnosticFamily::MissingLifetime);
    }

    #[test]
    fn classify_via_text_when_no_code_ownership() {
        let d = make_diag(None, "use of moved value: `buf`");
        assert_eq!(d.family(), DiagnosticFamily::OwnershipMove);
    }

    #[test]
    fn classify_via_text_when_no_code_import() {
        let d = make_diag(None, "cannot find type `HashMap` in this scope");
        assert_eq!(d.family(), DiagnosticFamily::MissingImport);
    }

    #[test]
    fn repair_hint_is_non_empty_for_all_families() {
        let families: &[DiagnosticFamily] = &[
            DiagnosticFamily::MissingLifetime,
            DiagnosticFamily::TraitBoundFailure,
            DiagnosticFamily::OwnershipMove,
            DiagnosticFamily::BorrowConflict,
            DiagnosticFamily::TypeMismatch,
            DiagnosticFamily::MissingImport,
            DiagnosticFamily::AsyncMismatch,
            DiagnosticFamily::MacroError,
            DiagnosticFamily::PatternExhaustiveness,
            DiagnosticFamily::IntegerOverflow,
            DiagnosticFamily::UnusedCode,
            DiagnosticFamily::InternalCompilerError,
            DiagnosticFamily::Other("E9999".to_string()),
        ];
        for f in families {
            assert!(!f.repair_hint().is_empty(), "Empty hint for {f:?}");
        }
    }

    #[test]
    fn retry_priority_ice_is_zero() {
        assert_eq!(DiagnosticFamily::InternalCompilerError.retry_priority(), 0);
    }

    #[test]
    fn missing_import_is_auto_fixable() {
        assert!(DiagnosticFamily::MissingImport.is_auto_fixable());
        assert!(!DiagnosticFamily::MissingLifetime.is_auto_fixable());
    }
}
