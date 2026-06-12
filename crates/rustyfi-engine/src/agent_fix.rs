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

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// How many tool calls and wall-clock seconds the session may consume.
#[derive(Debug, Clone)]
pub struct DoctorBudget {
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
            last_error_count: -1,
        }
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
            // Parent doesn't exist yet — still allow if no traversal components.
            Err(_) => {
                // If we got here the path had no `..` and isn't absolute, so
                // accept it; `write_file` will create dirs as needed.
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
            lines.iter().any(|l| *l == "src/lib.rs"),
            "src/lib.rs missing from: {:?}",
            lines
        );
        assert!(
            lines.iter().any(|l| *l == "src/util.rs"),
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
}
