//! Deterministic compile-fix pass: harvest and apply rustc's own suggestions.
//!
//! `cargo check --message-format=json` attaches structured fix suggestions to
//! many diagnostics — each with exact byte spans, a `suggested_replacement`,
//! and an `applicability`:
//!   * `MachineApplicable` — rustc is certain; apply and trust (this is what
//!     `cargo fix` uses).
//!   * `MaybeIncorrect` — a good guess (e.g. "consider borrowing", a similar
//!     method name); apply it, re-run `cargo check`, and **revert if it didn't
//!     reduce the error count**. The compiler is the oracle, so a wrong guess
//!     can never make the crate worse.
//!   * `HasPlaceholders` / `Unspecified` — left for the LLM.
//!
//! All edits are confined to `<workspace>/src`, so a dependency's cached source
//! is never touched. No LLM, no tokens.

use std::collections::BTreeMap;
use std::ops::Range;
use std::path::{Path, PathBuf};

use rustyfi_core::state::CargoOutput;
use tracing::{debug, info};

#[derive(Debug, Clone)]
pub struct Edit {
    pub file: PathBuf,
    pub start: usize,
    pub end: usize,
    pub replacement: String,
}

/// A suggestion is applied all-or-nothing (a multi-span fix is only correct if
/// every part lands).
#[derive(Debug, Clone)]
pub struct Suggestion {
    pub edits: Vec<Edit>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct FixResult {
    pub applied: usize,
    pub passes: usize,
}

/// Apply rustc's suggestions to `<workspace>/src` until none help or
/// `max_passes` is reached. Trusted (`MachineApplicable`) edits are applied
/// directly; guessed (`MaybeIncorrect`) edits are applied only if they reduce
/// the error count, otherwise reverted.
pub fn apply_machine_suggestions(workspace: &Path, max_passes: usize) -> FixResult {
    let mut result = FixResult::default();
    for _ in 0..max_passes {
        let Some(output) = check(workspace) else {
            break;
        };
        if output.exit_code == Some(0) {
            break;
        }
        result.passes += 1;
        let mut applied = 0;

        // ── Tier 1: trusted machine-applicable edits ────────────────────
        let trusted = parse_suggestions(&output, workspace, &["MachineApplicable"]);
        applied += apply_suggestions(&trusted);

        let after_trusted = check(workspace).unwrap_or(output);
        if after_trusted.exit_code == Some(0) {
            result.applied += applied;
            break;
        }

        // ── Tier 2: guessed edits, verified against the compiler ────────
        let before = error_count(&after_trusted);
        let guesses = parse_suggestions(&after_trusted, workspace, &["MaybeIncorrect"]);
        if !guesses.is_empty() {
            let snapshot = snapshot_files(&guesses);
            let n = apply_suggestions(&guesses);
            if n > 0 {
                let verified = check(workspace);
                let after = verified.as_ref().map(error_count).unwrap_or(usize::MAX);
                if after < before {
                    applied += n; // the guess helped — keep it
                } else {
                    restore(&snapshot); // it didn't — undo, leave for the LLM
                }
            }
        }

        result.applied += applied;
        if applied == 0 {
            break; // fixpoint
        }
    }
    if result.applied > 0 {
        info!(
            "rustfix applied {} compiler edit(s) over {} pass(es)",
            result.applied, result.passes
        );
    }
    result
}

fn check(workspace: &Path) -> Option<CargoOutput> {
    rustyfi_core::compiler::run_cargo_check(workspace).ok()
}

/// Count error-level compiler messages in a cargo JSON output.
fn error_count(output: &CargoOutput) -> usize {
    output
        .stdout_lines
        .iter()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
        .filter(|v| v.get("reason").and_then(|r| r.as_str()) == Some("compiler-message"))
        .filter(|v| {
            v.get("message")
                .and_then(|m| m.get("level"))
                .and_then(|l| l.as_str())
                == Some("error")
        })
        .count()
}

/// Parse suggestions whose applicability is in `accepted`, restricted to files
/// under `<workspace>/src`. Replacements containing literal placeholders are
/// skipped (they won't compile).
pub fn parse_suggestions(
    output: &CargoOutput,
    workspace: &Path,
    accepted: &[&str],
) -> Vec<Suggestion> {
    let src_dir = workspace.join("src");
    let mut out = Vec::new();
    for line in &output.stdout_lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        if v.get("reason").and_then(|r| r.as_str()) != Some("compiler-message") {
            continue;
        }
        let Some(msg) = v.get("message") else {
            continue;
        };
        if let Some(s) = suggestion_from_spans(msg.get("spans"), workspace, &src_dir, accepted) {
            out.push(s);
        }
        if let Some(children) = msg.get("children").and_then(|c| c.as_array()) {
            for child in children {
                if let Some(s) =
                    suggestion_from_spans(child.get("spans"), workspace, &src_dir, accepted)
                {
                    out.push(s);
                }
            }
        }
    }
    out
}

fn suggestion_from_spans(
    spans: Option<&serde_json::Value>,
    workspace: &Path,
    src_dir: &Path,
    accepted: &[&str],
) -> Option<Suggestion> {
    let arr = spans?.as_array()?;
    let mut edits = Vec::new();
    for span in arr {
        let appl = span
            .get("suggestion_applicability")
            .and_then(|a| a.as_str());
        if !appl.map(|a| accepted.contains(&a)).unwrap_or(false) {
            continue;
        }
        let Some(replacement) = span.get("suggested_replacement").and_then(|r| r.as_str()) else {
            continue;
        };
        // A replacement with a literal placeholder won't compile.
        if replacement.contains("/*") || replacement.contains("...") {
            continue;
        }
        let (Some(start), Some(end)) = (
            span.get("byte_start").and_then(|b| b.as_u64()),
            span.get("byte_end").and_then(|b| b.as_u64()),
        ) else {
            continue;
        };
        let Some(file_name) = span.get("file_name").and_then(|f| f.as_str()) else {
            continue;
        };
        let file = workspace.join(file_name);
        // SAFETY: never edit anything outside <workspace>/src.
        if !file.starts_with(src_dir) {
            continue;
        }
        edits.push(Edit {
            file,
            start: start as usize,
            end: end as usize,
            replacement: replacement.to_string(),
        });
    }
    (!edits.is_empty()).then_some(Suggestion { edits })
}

/// Greedily apply a non-overlapping set of suggestions to disk; return the
/// number of edits applied.
pub fn apply_suggestions(suggestions: &[Suggestion]) -> usize {
    let mut used: BTreeMap<PathBuf, Vec<Range<usize>>> = BTreeMap::new();
    let mut chosen: Vec<&Edit> = Vec::new();
    for s in suggestions {
        let conflict = s.edits.iter().any(|e| {
            used.get(&e.file)
                .map(|rs| rs.iter().any(|r| overlaps(r, e.start, e.end)))
                .unwrap_or(false)
        });
        if conflict {
            continue;
        }
        for e in &s.edits {
            used.entry(e.file.clone()).or_default().push(e.start..e.end);
            chosen.push(e);
        }
    }

    let mut by_file: BTreeMap<PathBuf, Vec<&Edit>> = BTreeMap::new();
    for e in chosen {
        by_file.entry(e.file.clone()).or_default().push(e);
    }
    let mut applied = 0;
    for (file, mut edits) in by_file {
        let Ok(content) = std::fs::read_to_string(&file) else {
            continue;
        };
        edits.sort_by_key(|e| std::cmp::Reverse(e.start));
        let new = apply_edits_to_content(&content, &edits);
        if new != content && std::fs::write(&file, &new).is_ok() {
            applied += edits.len();
            debug!("rustfix: {} edit(s) → {}", edits.len(), file.display());
        }
    }
    applied
}

/// Apply edits (sorted descending by start, non-overlapping) to a string.
pub fn apply_edits_to_content(content: &str, edits: &[&Edit]) -> String {
    let mut out = content.to_string();
    for e in edits {
        if e.start <= e.end
            && e.end <= out.len()
            && out.is_char_boundary(e.start)
            && out.is_char_boundary(e.end)
        {
            out.replace_range(e.start..e.end, &e.replacement);
        }
    }
    out
}

fn snapshot_files(suggestions: &[Suggestion]) -> Vec<(PathBuf, String)> {
    let mut files: Vec<PathBuf> = suggestions
        .iter()
        .flat_map(|s| s.edits.iter().map(|e| e.file.clone()))
        .collect();
    files.sort();
    files.dedup();
    files
        .into_iter()
        .filter_map(|f| std::fs::read_to_string(&f).ok().map(|c| (f, c)))
        .collect()
}

fn restore(snapshot: &[(PathBuf, String)]) {
    for (file, content) in snapshot {
        let _ = std::fs::write(file, content);
    }
}

fn overlaps(r: &Range<usize>, start: usize, end: usize) -> bool {
    if start == end {
        return r.start <= start && start < r.end; // insertion inside a replaced range
    }
    start < r.end && r.start < end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(lines: &[&str]) -> CargoOutput {
        CargoOutput {
            stdout_lines: lines.iter().map(|s| s.to_string()).collect(),
            stderr_lines: vec![],
            exit_code: Some(101),
        }
    }

    #[test]
    fn parses_by_applicability_tier() {
        let line = r#"{"reason":"compiler-message","message":{"level":"error","message":"mismatched types","code":{"code":"E0308"},"spans":[],"children":[{"level":"help","message":"consider borrowing","spans":[{"file_name":"src/main.rs","byte_start":10,"byte_end":13,"suggested_replacement":"&s","suggestion_applicability":"MachineApplicable"}]}]}}"#;
        let ws = PathBuf::from("/ws");
        assert_eq!(
            parse_suggestions(&out(&[line]), &ws, &["MachineApplicable"]).len(),
            1
        );
        // not in the requested tier
        assert!(parse_suggestions(&out(&[line]), &ws, &["MaybeIncorrect"]).is_empty());
    }

    #[test]
    fn skips_placeholder_replacements() {
        let line = r#"{"reason":"compiler-message","message":{"level":"error","message":"x","spans":[],"children":[{"level":"help","message":"h","spans":[{"file_name":"src/a.rs","byte_start":1,"byte_end":2,"suggested_replacement":"/* value */","suggestion_applicability":"MaybeIncorrect"}]}]}}"#;
        let ws = PathBuf::from("/ws");
        assert!(parse_suggestions(&out(&[line]), &ws, &["MaybeIncorrect"]).is_empty());
    }

    #[test]
    fn never_targets_dependency_sources() {
        let dep = r#"{"reason":"compiler-message","message":{"level":"error","message":"x","spans":[],"children":[{"level":"help","message":"h","spans":[{"file_name":"/home/u/.cargo/registry/src/foo/lib.rs","byte_start":1,"byte_end":2,"suggested_replacement":"y","suggestion_applicability":"MachineApplicable"}]}]}}"#;
        assert!(
            parse_suggestions(&out(&[dep]), &PathBuf::from("/ws"), &["MachineApplicable"])
                .is_empty()
        );
    }

    #[test]
    fn counts_only_error_messages() {
        let err = r#"{"reason":"compiler-message","message":{"level":"error","message":"e","spans":[],"children":[]}}"#;
        let warn = r#"{"reason":"compiler-message","message":{"level":"warning","message":"w","spans":[],"children":[]}}"#;
        let other = r#"{"reason":"build-finished","success":false}"#;
        assert_eq!(error_count(&out(&[err, warn, other, err])), 2);
    }

    #[test]
    fn applies_edit_by_byte_range() {
        let content = "let x = need_ref(s);\n";
        let at = content.find('s').unwrap();
        let edit = Edit {
            file: "x".into(),
            start: at,
            end: at,
            replacement: "&".into(),
        };
        assert_eq!(
            apply_edits_to_content(content, &[&edit]),
            "let x = need_ref(&s);\n"
        );
    }

    #[test]
    fn applies_multiple_descending() {
        let content = "aaa bbb ccc";
        let e1 = Edit {
            file: "x".into(),
            start: 8,
            end: 11,
            replacement: "ZZZ".into(),
        };
        let e2 = Edit {
            file: "x".into(),
            start: 4,
            end: 7,
            replacement: "YY".into(),
        };
        assert_eq!(apply_edits_to_content(content, &[&e1, &e2]), "aaa YY ZZZ");
    }

    /// Real end-to-end: a crate with a borrow error → the pass makes it compile.
    /// Runs cargo, so it's `#[ignore]`d from the fast suite (`--ignored` to run).
    #[test]
    #[ignore]
    fn end_to_end_fixes_borrow_error_via_cargo() {
        let dir = std::env::temp_dir().join(format!("rustfix_e2e_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"e2e\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/main.rs"),
            "fn need_ref(x: &str) -> usize { x.len() }\n\
             fn main() { let s = String::from(\"hi\"); let _ = need_ref(s); }\n",
        )
        .unwrap();
        let res = apply_machine_suggestions(&dir, 4);
        let final_exit = check(&dir).and_then(|o| o.exit_code);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(res.applied >= 1, "no edits applied: {res:?}");
        assert_eq!(
            final_exit,
            Some(0),
            "crate still does not compile after rustfix"
        );
    }
}
